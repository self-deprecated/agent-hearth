//! The turn-end gate: format, apply clippy's fixes, report what is left as one line per
//! diagnostic, and remember the verdict for this exact code. Measured against main: when main
//! itself is not formatted, or has fixable or other diagnostics, under this toolchain (atuin
//! formats with nightly rustfmt, nushell and ruff have clippy warnings on 1.98), the gate does not
//! reformat or fix the whole workspace, and reports only diagnostics main does not have.
//!
//! ```text
//! <state>/verdicts/<fingerprint>.json   the verdict for code with this fingerprint, any worktree
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adopt;
use crate::cargo;
use crate::golden::{self, Inputs};
use crate::locate::Location;
use crate::toolchain::{self, Toolchain};

/// Part of every fingerprint: change it whenever the gate's steps change, so old verdicts lapse.
const GATE: &str = "check 3: cargo fmt --all; clippy --fix if fixable; \
                    clippy --workspace --all-targets --keep-going with warnings denied; \
                    main's baseline";
/// `--keep-going`: an error in one target (a bench that needs nightly) leaves the others'
/// diagnostics still to come.
const CLIPPY: [&str; 5] = [
    "clippy",
    "--workspace",
    "--all-targets",
    "--keep-going",
    "--message-format=json",
];
const FIX: [&str; 7] = [
    "clippy",
    "--workspace",
    "--all-targets",
    "--fix",
    "--allow-dirty",
    "--allow-staged",
    "--allow-no-vcs",
];
/// Denies warnings without touching rustc's flags, which would rebuild everything.

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Relative to the worktree root when inside it.
    pub file: Option<String>,
    pub line: Option<u64>,
    pub column: Option<u64>,
    pub level: String,
    pub code: Option<String>,
    pub message: String,
    pub rendered: Option<String>,
}

impl Diagnostic {
    /// The diagnostic without its line and column, which edits elsewhere in the file move: what
    /// matches it against main's.
    pub fn key(&self) -> String {
        let code = self.code.as_deref().unwrap_or(&self.level);
        format!(
            "{} {code} {}",
            self.file.as_deref().unwrap_or(""),
            self.message
        )
    }

    /// `file:line:col CODE message`, the form agents get by default.
    pub fn summary(&self) -> String {
        let code = self.code.as_deref().unwrap_or(&self.level);
        match (&self.file, self.line, self.column) {
            (Some(file), Some(line), Some(column)) => {
                format!("{file}:{line}:{column} {code} {}", self.message)
            }
            (Some(file), _, _) => format!("{file} {code} {}", self.message),
            _ => format!("{code} {}", self.message),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Fail,
    /// The code changed while it was being checked; nothing was stored.
    Superseded,
}

impl Status {
    pub fn exit_code(self) -> u8 {
        match self {
            Status::Pass => 0,
            Status::Fail => 1,
            Status::Superseded => 3,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Verdict {
    status: Status,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub status: Status,
    pub fingerprint: String,
    /// The verdict was stored by an earlier check of the same code.
    pub cached: bool,
    /// Files that `cargo fmt` or `clippy --fix` changed.
    pub fixed: Vec<PathBuf>,
    pub diagnostics: Vec<Diagnostic>,
    /// Diagnostics main has too, not reported.
    #[serde(default)]
    pub existing: usize,
}

/// What main's code is like under this toolchain, as the golden built it: the gate leaves alone
/// what was already so on main.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Baseline {
    /// `cargo fmt --all --check` passed.
    pub fmt_clean: bool,
    /// Clippy had machine-applicable suggestions.
    pub fixable: bool,
    /// Its diagnostics, by [`Diagnostic::key`].
    pub diagnostics: BTreeSet<String>,
    /// How many of them are errors in main's code.
    #[serde(default)]
    pub errors: usize,
}

/// The gate for the worktree: the steps, and the baseline of the golden that seeded it.
fn gate(location: &Location) -> Result<(String, Option<Baseline>)> {
    let baseline = match adopt::seeded_from(location)? {
        Some(id) => golden::baseline(location, &id),
        None => None,
    };
    let key = serde_json::to_string(&baseline)?;
    let toolchain = toolchain::gate_key(location.toolchain.as_ref());
    Ok((format!("{GATE}: {key}{toolchain}"), baseline))
}

/// Runs the gate on the worktree, building into `target`.
pub fn check(location: &Location, target: &Path) -> Result<Report> {
    let (gate, baseline) = gate(location)?;
    let baseline = baseline.unwrap_or(Baseline {
        fmt_clean: true,
        ..Baseline::default()
    });
    let before = Snapshot::take(location, &gate)?;
    if let Some(verdict) = load_verdict::<Verdict>(location, &before.fingerprint)? {
        return Ok(Report {
            status: verdict.status,
            fingerprint: before.fingerprint,
            cached: true,
            fixed: Vec::new(),
            diagnostics: verdict.diagnostics,
            existing: 0,
        });
    }

    // Formatting or fixing a workspace that was not clean on main rewrites files the agent never
    // touched: atuin's nightly formatting gave each agent's worktree 90 changed files.
    let formatted = if baseline.fmt_clean {
        fmt(location)?
    } else {
        Ok(())
    };
    let mut checked = Snapshot::take(location, &gate)?;
    let mut clippy = clippy(location, target, &CLIPPY)?;
    if clippy.fixable && !baseline.fixable {
        clippy_fix(location, target)?;
        checked = Snapshot::take(location, &gate)?;
        clippy = self::clippy(location, target, &CLIPPY)?;
    }
    let after = Snapshot::take(location, &gate)?;
    let fixed = before.changed(&checked);

    let found = clippy.diagnostics.len();
    let mut diagnostics: Vec<Diagnostic> = clippy
        .diagnostics
        .into_iter()
        .filter(|diagnostic| !baseline.diagnostics.contains(&diagnostic.key()))
        .collect();
    let existing = found - diagnostics.len();
    // Clippy fails on main's warnings too; with only those, the code passes.
    clippy.success |= diagnostics.is_empty() && existing > 0 && !clippy.unexplained;
    // A failure of the tools rather than the code is not stored: it would outlive its fix.
    let mut tool_failed = clippy.unexplained;
    if let Err(fmt_error) = formatted
        && diagnostics.is_empty()
    {
        // With no compile errors, a failed `cargo fmt` is a missing or broken rustfmt.
        diagnostics.push(Diagnostic::error(format!("cargo fmt failed: {fmt_error}")));
        tool_failed = true;
    }
    let status = if after.fingerprint != checked.fingerprint {
        Status::Superseded
    } else if clippy.success && diagnostics.is_empty() {
        Status::Pass
    } else {
        Status::Fail
    };
    if status != Status::Superseded && !tool_failed {
        let verdict = Verdict {
            status,
            diagnostics: diagnostics.clone(),
        };
        store_verdict(location, &checked.fingerprint, &verdict)?;
    }
    Ok(Report {
        status,
        fingerprint: checked.fingerprint,
        cached: false,
        fixed,
        diagnostics,
        existing,
    })
}

/// Records what main's code is like in the golden at `src`, built into `target` at `pin`.
pub fn baseline_of(
    src: &Path,
    target: &Path,
    pin: Option<&Path>,
    mounts: &[(PathBuf, PathBuf)],
    toolchain: Option<&Toolchain>,
) -> Result<Baseline> {
    let fmt_clean = toolchain::command("cargo", toolchain)
        .args(["fmt", "--all", "--check"])
        .current_dir(src)
        .output()
        .context("running cargo fmt --check")?
        .status
        .success();
    let output = cargo::command(src, target, Path::new(""), pin, mounts, toolchain)
        .args(CLIPPY)
        .envs(cargo::DENY_WARNINGS)
        .output()
        .context("running clippy on the golden")?;
    // Diagnostics name files under the path the golden built at.
    let root = pin.map_or_else(|| src.to_path_buf(), |pin| pin.join("src"));
    let clippy = parse(
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
        output.status.success(),
        &root,
    );
    Ok(Baseline {
        fmt_clean,
        fixable: clippy.fixable,
        diagnostics: clippy.diagnostics.iter().map(Diagnostic::key).collect(),
        errors: clippy
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.level == "error" && diagnostic.file.is_some())
            .count(),
    })
}

/// The stored check verdict for the worktree's current code, if there is one.
pub fn current_verdict(location: &Location) -> Result<Option<Status>> {
    let snapshot = Snapshot::take(location, &gate(location)?.0)?;
    let verdict: Option<Verdict> = load_verdict(location, &snapshot.fingerprint)?;
    Ok(verdict.map(|verdict| verdict.status))
}

/// The stored verdict for `fingerprint`, from this gate or another.
pub fn load_verdict<T: DeserializeOwned>(
    location: &Location,
    fingerprint: &str,
) -> Result<Option<T>> {
    let path = verdict_path(location, fingerprint);
    match fs::read(&path) {
        Ok(json) => Ok(Some(
            serde_json::from_slice(&json).with_context(|| format!("reading {}", path.display()))?,
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn store_verdict(
    location: &Location,
    fingerprint: &str,
    verdict: &impl Serialize,
) -> Result<()> {
    let path = verdict_path(location, fingerprint);
    let dir = path.parent().context("verdict has no parent")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    golden::write_atomically(&path, &serde_json::to_vec_pretty(verdict)?)
}

fn verdict_path(location: &Location, fingerprint: &str) -> PathBuf {
    location
        .state_dir
        .join("verdicts")
        .join(format!("{fingerprint}.json"))
}

/// Runs `cargo fmt --all`. Its failure is returned, not raised: a syntax error also fails it, and
/// clippy reports those better.
fn fmt(location: &Location) -> Result<Result<(), String>> {
    let output = toolchain::command("cargo", location.toolchain.as_ref())
        .args(["fmt", "--all"])
        .current_dir(&location.worktree)
        .output()
        .context("running cargo fmt")?;
    if output.status.success() {
        return Ok(Ok(()));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let first = stderr.lines().find(|line| !line.trim().is_empty());
    Ok(Err(first.unwrap_or("no output").trim().to_owned()))
}

fn clippy_fix(location: &Location, target: &Path) -> Result<()> {
    let args = FIX.map(String::from);
    // The final clippy run decides; this one only edits files.
    cargo::run_to(
        location,
        target,
        &location.worktree,
        &args,
        &[],
        std::io::sink(),
        std::io::sink(),
    )?;
    Ok(())
}

struct Clippy {
    success: bool,
    diagnostics: Vec<Diagnostic>,
    fixable: bool,
    /// Cargo failed without saying why, as when it or bwrap is killed or misconfigured.
    unexplained: bool,
}

fn clippy(location: &Location, target: &Path, args: &[&str]) -> Result<Clippy> {
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let code = cargo::run_to(
        location,
        target,
        &location.worktree,
        &args,
        &cargo::DENY_WARNINGS,
        &mut stdout,
        &mut stderr,
    )?;
    Ok(parse(
        &String::from_utf8_lossy(&stdout),
        &String::from_utf8_lossy(&stderr),
        code == 0,
        &location.worktree,
    ))
}

/// Turns cargo's JSON messages into deduplicated diagnostics. When cargo failed without a
/// compiler message, its own `error:` lines become the diagnostics.
fn parse(stdout: &str, stderr: &str, success: bool, worktree: &Path) -> Clippy {
    let mut seen = BTreeSet::new();
    let mut diagnostics = Vec::new();
    let mut fixable = false;
    let mut unexplained = false;
    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if message["reason"] != "compiler-message" {
            continue;
        }
        fixable |= line.contains(r#""suggestion_applicability":"MachineApplicable""#);
        if let Some(diagnostic) = Diagnostic::from_rustc(&message["message"], worktree)
            && seen.insert(diagnostic.summary())
        {
            diagnostics.push(diagnostic);
        }
    }
    if !success && diagnostics.is_empty() {
        diagnostics = stderr
            .lines()
            .filter_map(|line| line.strip_prefix("error: "))
            .filter(|line| !line.starts_with("could not compile"))
            .map(|line| Diagnostic::error(line.to_owned()))
            .collect();
        if diagnostics.is_empty() {
            let last = stderr.lines().rev().find(|line| !line.trim().is_empty());
            diagnostics.push(Diagnostic::error(format!(
                "cargo failed: {}",
                last.unwrap_or("no output")
            )));
            unexplained = true;
        }
    }
    Clippy {
        success,
        diagnostics,
        fixable,
        unexplained,
    }
}

impl Diagnostic {
    pub fn error(message: String) -> Self {
        Self {
            file: None,
            line: None,
            column: None,
            level: "error".to_owned(),
            code: None,
            message,
            rendered: None,
        }
    }

    /// Errors and warnings only; rustc's closing summaries ("aborting due to…", "1 warning
    /// emitted") are left out.
    pub fn from_rustc(message: &Value, worktree: &Path) -> Option<Self> {
        let level = message["level"].as_str()?;
        let text = message["message"].as_str()?;
        let spans = message["spans"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let summary = text.starts_with("aborting due to")
            || text.ends_with("warning emitted")
            || text.ends_with("warnings emitted");
        if !matches!(level, "error" | "warning") || (spans.is_empty() && summary) {
            return None;
        }
        let primary = spans
            .iter()
            .find(|span| span["is_primary"] == true)
            .or(spans.first());
        let label = primary.and_then(|span| span["label"].as_str());
        let file = primary
            .and_then(|span| span["file_name"].as_str())
            .map(|file| {
                Path::new(file).strip_prefix(worktree).map_or_else(
                    |_| file.to_owned(),
                    |rel| rel.to_string_lossy().into_owned(),
                )
            });
        Some(Self {
            file,
            line: primary.and_then(|span| span["line_start"].as_u64()),
            column: primary.and_then(|span| span["column_start"].as_u64()),
            level: level.to_owned(),
            code: message["code"]["code"].as_str().map(str::to_owned),
            message: match label {
                Some(label) if !label.is_empty() => format!("{text}: {label}"),
                _ => text.to_owned(),
            },
            rendered: message["rendered"].as_str().map(str::to_owned),
        })
    }
}

/// The content of every file the build could read, and the fingerprint over it and a gate spec.
pub struct Snapshot {
    pub fingerprint: String,
    /// Path relative to the worktree root, and a hash of its content.
    pub files: BTreeMap<String, String>,
}

impl Snapshot {
    pub fn take(location: &Location, gate: &str) -> Result<Self> {
        let mut files = BTreeMap::new();
        for path in source_files(location)? {
            let full = location.worktree.join(&path);
            let metadata = match fs::symlink_metadata(&full) {
                Ok(metadata) => metadata,
                // Deleted since it was listed: the end-of-run snapshot will differ.
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err).with_context(|| format!("reading {}", full.display())),
            };
            let hash = if metadata.is_symlink() {
                let link = fs::read_link(&full)?;
                format!("symlink {}", link.display())
            } else if metadata.is_file() {
                let content =
                    fs::read(&full).with_context(|| format!("reading {}", full.display()))?;
                blake3::hash(&content).to_hex().to_string()
            } else {
                continue;
            };
            files.insert(path, hash);
        }
        let inputs = Inputs::of(&location.worktree, location.toolchain.as_ref())?;
        let hashed = serde_json::to_vec(&(gate, &inputs, &files))?;
        Ok(Self {
            fingerprint: blake3::hash(&hashed).to_hex()[..32].to_owned(),
            files,
        })
    }

    /// Files whose content differs in `later`, including ones it added.
    pub fn changed(&self, later: &Snapshot) -> Vec<PathBuf> {
        later
            .files
            .iter()
            .filter(|(path, hash)| self.files.get(*path) != Some(hash))
            .map(|(path, _)| PathBuf::from(path))
            .collect()
    }
}

/// Every file in the worktree that git does not ignore, tracked or not. An index that does not
/// exist makes git list them all, the same way in jj workspaces and git worktrees, and reads only.
pub fn source_files(location: &Location) -> Result<Vec<String>> {
    let no_index = location.state_dir.join("no-index");
    if no_index.exists() {
        bail!("{} must not exist", no_index.display());
    }
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(&location.git_dir)
        .arg("--work-tree")
        .arg(&location.worktree)
        .args(["ls-files", "-z", "--others", "--exclude-standard"])
        .current_dir(&location.worktree)
        .env("GIT_INDEX_FILE", &no_index)
        .output()
        .context("running git ls-files")?;
    if !output.status.success() {
        bail!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let listed = String::from_utf8(output.stdout).context("git listed a non-UTF-8 path")?;
    Ok(listed
        .split('\0')
        .filter(|path| !path.is_empty() && !path.starts_with(".jj/") && *path != ".git")
        .map(str::to_owned)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adopt;
    use crate::testing::Sandbox;

    fn message(level: &str, text: &str, code: Option<&str>, spans: Value) -> String {
        serde_json::json!({
            "reason": "compiler-message",
            "message": {"level": level, "message": text, "code": code.map(|c| serde_json::json!({"code": c})),
                        "spans": spans, "rendered": format!("{level}: {text}")},
        })
        .to_string()
    }

    fn span(file: &str, line: u64, column: u64, label: Option<&str>) -> Value {
        serde_json::json!([{"file_name": file, "line_start": line, "column_start": column,
                            "is_primary": true, "label": label}])
    }

    #[test]
    fn parse_keeps_one_line_per_diagnostic() {
        let mismatch = message(
            "error",
            "mismatched types",
            Some("E0308"),
            span("src/lib.rs", 1, 26, Some("expected `u32`, found `&str`")),
        );
        let stdout = [
            mismatch.clone(),
            // The same error from the test target.
            mismatch,
            message(
                "warning",
                "unused variable: `x`",
                Some("unused_variables"),
                span("/w/src/main.rs", 3, 9, None),
            ),
            message(
                "error",
                "aborting due to 1 previous error",
                None,
                serde_json::json!([]),
            ),
            message(
                "failure-note",
                "For more information…",
                None,
                serde_json::json!([]),
            ),
            r#"{"reason":"compiler-artifact","fresh":true}"#.to_owned(),
            "not json".to_owned(),
        ]
        .join("\n");
        let parsed = parse(&stdout, "", false, Path::new("/w"));
        let lines: Vec<_> = parsed.diagnostics.iter().map(Diagnostic::summary).collect();
        assert_eq!(
            lines,
            [
                "src/lib.rs:1:26 E0308 mismatched types: expected `u32`, found `&str`",
                "src/main.rs:3:9 unused_variables unused variable: `x`",
            ]
        );
        assert!(!parsed.fixable);
    }

    #[test]
    fn a_cargo_failure_without_a_reason_is_the_tools_not_the_codes() {
        let killed = parse("", "warning: something\n", false, Path::new("/w"));
        assert!(killed.unexplained);
        assert_eq!(
            killed.diagnostics[0].message,
            "cargo failed: warning: something"
        );
        let manifest = parse(
            "",
            "error: failed to parse manifest\n",
            false,
            Path::new("/w"),
        );
        assert!(
            !manifest.unexplained,
            "a broken Cargo.toml is the code's problem"
        );
    }

    #[test]
    fn parse_falls_back_to_cargo_errors() {
        let stderr = "error: failed to parse manifest at `/w/Cargo.toml`\n\nCaused by:\n  x\n";
        let parsed = parse("", stderr, false, Path::new("/w"));
        let lines: Vec<_> = parsed.diagnostics.iter().map(Diagnostic::summary).collect();
        assert_eq!(lines, ["error failed to parse manifest at `/w/Cargo.toml`"]);
    }

    fn adopted(s: &Sandbox) -> (Location, PathBuf) {
        s.crate_repo("repo");
        let repo = s.location("repo");
        golden::refresh(&repo, "main").unwrap();
        adopt::adopt(&repo).unwrap();
        let target = adopt::target_dir(&repo);
        (repo, target)
    }

    #[test]
    fn passes_then_answers_from_the_stored_verdict() {
        let s = Sandbox::new();
        let (repo, target) = adopted(&s);
        let first = check(&repo, &target).unwrap();
        assert_eq!(
            (first.status, first.cached, first.diagnostics.len()),
            (Status::Pass, false, 0)
        );
        let again = check(&repo, &target).unwrap();
        assert_eq!((again.status, again.cached), (Status::Pass, true));
        assert_eq!(again.fingerprint, first.fingerprint);
    }

    #[test]
    fn a_type_error_is_one_short_line() {
        let s = Sandbox::new();
        let (repo, target) = adopted(&s);
        s.write("repo/src/lib.rs", "pub fn one() -> u32 {\n    \"no\"\n}\n");
        let report = check(&repo, &target).unwrap();
        assert_eq!(report.status, Status::Fail);
        let lines: Vec<_> = report.diagnostics.iter().map(Diagnostic::summary).collect();
        assert_eq!(
            lines,
            ["src/lib.rs:2:5 E0308 mismatched types: expected `u32`, found `&str`"]
        );
        assert!(lines[0].len() <= 100);
        assert!(check(&repo, &target).unwrap().cached);
    }

    #[test]
    fn formats_and_applies_clippy_fixes() {
        let s = Sandbox::new();
        let (repo, target) = adopted(&s);
        s.write("repo/src/lib.rs", "pub fn one() -> u32 {   return 1; }\n");
        let report = check(&repo, &target).unwrap();
        assert_eq!(report.status, Status::Pass, "{:?}", report.diagnostics);
        assert_eq!(report.fixed, [PathBuf::from("src/lib.rs")]);
        let fixed = fs::read_to_string(s.path("repo/src/lib.rs")).unwrap();
        assert_eq!(fixed, "pub fn one() -> u32 {\n    1\n}\n");
    }

    #[test]
    fn leaves_alone_what_main_already_has_and_reports_only_new_diagnostics() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        // Main is not formatted and has a fixable warning of its own.
        let main = "pub fn one() -> u32 {   return 1; }\n";
        s.write("repo/src/lib.rs", main);
        s.run(
            "repo",
            "git",
            &["commit", "-q", "-am", "unformatted, with a warning"],
        );
        let repo = s.location("repo");
        golden::refresh(&repo, "main").unwrap();
        adopt::adopt(&repo).unwrap();
        let target = adopt::target_dir(&repo);

        // The agent adds a warning of its own in another file.
        s.write("repo/src/lib.rs", &format!("{main}pub mod other;\n"));
        s.write(
            "repo/src/other.rs",
            "pub fn two() -> u32 {\n    let unused = 2;\n    2\n}\n",
        );
        let report = check(&repo, &target).unwrap();
        assert_eq!(report.status, Status::Fail);
        assert!(report.fixed.is_empty(), "{:?}", report.fixed);
        assert_eq!(report.existing, 1, "{:?}", report.diagnostics);
        assert_eq!(report.diagnostics.len(), 1, "{:?}", report.diagnostics);
        assert!(
            report.diagnostics[0]
                .summary()
                .starts_with("src/other.rs:2:9")
        );
        let lib = fs::read_to_string(s.path("repo/src/lib.rs")).unwrap();
        assert!(lib.starts_with(main), "main's code was rewritten:\n{lib}");

        // Its own fixed, the code passes with main's warning still there.
        s.write("repo/src/other.rs", "pub fn two() -> u32 {\n    2\n}\n");
        let report = check(&repo, &target).unwrap();
        assert_eq!(report.status, Status::Pass, "{:?}", report.diagnostics);
        assert_eq!(report.existing, 1);
    }

    #[test]
    fn a_main_with_a_target_that_does_not_build_gets_a_golden_and_a_gate() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        // A bench that does not build, as benches that need nightly do not on stable.
        s.write(
            "repo/benches/broken.rs",
            "#[test]\nfn broken() {\n    let _: u32 = \"no\";\n}\n",
        );
        s.run("repo", "git", &["add", "."]);
        s.run("repo", "git", &["commit", "-q", "-m", "a broken bench"]);
        let repo = s.location("repo");
        golden::refresh(&repo, "main").unwrap();
        adopt::adopt(&repo).unwrap();
        let target = adopt::target_dir(&repo);
        let report = check(&repo, &target).unwrap();
        assert_eq!(report.status, Status::Pass, "{:?}", report.diagnostics);
        assert_eq!(report.existing, 1);

        s.write("repo/src/lib.rs", "pub fn one() -> u32 {\n    \"no\"\n}\n");
        let report = check(&repo, &target).unwrap();
        assert_eq!(report.status, Status::Fail);
        let lines: Vec<_> = report.diagnostics.iter().map(Diagnostic::summary).collect();
        assert_eq!(
            lines,
            ["src/lib.rs:2:5 E0308 mismatched types: expected `u32`, found `&str`"]
        );
    }

    #[test]
    fn fingerprints_untracked_files_but_not_ignored_ones() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let repo = s.location("repo");
        let base = Snapshot::take(&repo, GATE).unwrap();
        assert!(base.files.contains_key("src/lib.rs"));
        s.write("repo/target/debug/junk", "ignored");
        assert_eq!(
            Snapshot::take(&repo, GATE).unwrap().fingerprint,
            base.fingerprint
        );
        s.write("repo/src/new.rs", "// untracked\n");
        let with_new = Snapshot::take(&repo, GATE).unwrap();
        assert_ne!(with_new.fingerprint, base.fingerprint);
        assert_eq!(base.changed(&with_new), [PathBuf::from("src/new.rs")]);
    }
}
