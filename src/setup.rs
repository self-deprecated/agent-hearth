//! `agent-hearth setup`: Agent Hearth's hooks in the repository's Claude Code and Codex settings,
//! merged with what is there, and a golden. An agent in its sandbox can do neither (Codex's keeps
//! even the repository's `.codex/` read-only), so this is one command for its user to run.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::golden;
use crate::locate::Location;
use crate::settle;
use crate::toolchain::{self, Toolchain};

/// A hook: its event, the tools it matches (`None`: all), the `agent-hearth hook` it runs, and
/// its timeout in seconds.
type Hook = (
    &'static str,
    Option<&'static str>,
    &'static str,
    Option<u64>,
);

const CLAUDE: [Hook; 6] = [
    ("PreToolUse", Some("Bash"), "claude-pre-tool-use", None),
    (
        "PostToolUse",
        Some("Edit|Write|MultiEdit|NotebookEdit|Bash"),
        "claude-post-tool-use",
        None,
    ),
    // Long enough for a settle the Stop hook waits for.
    ("Stop", None, "claude-stop", Some(600)),
    ("UserPromptSubmit", None, "claude-user-prompt-submit", None),
    ("WorktreeCreate", None, "claude-worktree-create", None),
    ("WorktreeRemove", None, "claude-worktree-remove", None),
];

/// Codex edits with `apply_patch`, which Claude Code's matcher would miss: its PostToolUse
/// matches every tool, and the hook ignores those that edit nothing.
const CODEX: [Hook; 4] = [
    ("PreToolUse", Some("Bash"), "codex-pre-tool-use", None),
    ("PostToolUse", None, "codex-post-tool-use", None),
    ("Stop", None, "codex-stop", Some(600)),
    ("UserPromptSubmit", None, "codex-user-prompt-submit", None),
];

pub struct Report {
    /// The settings files written, and the ones that already had every hook.
    pub written: Vec<PathBuf>,
    pub unchanged: Vec<PathBuf>,
    /// The golden, and whether it had to be built; `None` when not asked for.
    pub golden: Option<(golden::Golden, bool)>,
}

/// Merges the hooks into the repository's `.claude/settings.json` (`claude`) and
/// `.codex/hooks.json` (`codex`), with `agent_hearth` as the binary, then refreshes the golden of
/// main when `refresh`.
pub fn setup(
    location: &Location,
    agent_hearth: &Path,
    claude: bool,
    codex: bool,
    refresh: bool,
) -> Result<Report> {
    let exe = crate::shell_word(&agent_hearth.to_string_lossy());
    // Before any file is written, so a repository without one is left as it was.
    let main = if refresh {
        Some(golden::main_rev(location)?)
    } else {
        None
    };
    let mut report = Report {
        written: Vec::new(),
        unchanged: Vec::new(),
        golden: None,
    };
    let files = [
        (claude, ".claude/settings.json", &CLAUDE[..]),
        (codex, ".codex/hooks.json", &CODEX[..]),
    ];
    for (wanted, file, hooks) in files {
        if !wanted {
            continue;
        }
        let path = location.worktree.join(file);
        let mut settings = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("{} is not valid JSON; fix it first", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => json!({}),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        if merge(&mut settings, &exe, hooks)? {
            fs::create_dir_all(path.parent().context("settings have no parent")?)?;
            let mut text = serde_json::to_string_pretty(&settings)?;
            text.push('\n');
            fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
            report.written.push(path);
        } else {
            report.unchanged.push(path);
        }
    }
    if let Some(main) = main {
        location.create_dirs()?;
        report.golden = Some(golden::refresh(location, main)?);
    }
    Ok(report)
}

/// How many more runs a test that fails only on the nightly must fail before setup believes it.
const FLAKY_RUNS: usize = 3;

/// What `setup --nightly` found.
pub struct Nightly {
    pub toolchain: Toolchain,
    /// Failures main has on the nightly but not on the repository's own toolchain; it is turned
    /// on only when there are none.
    pub only_nightly: Vec<String>,
    /// Failures main has on both.
    pub both: usize,
}

/// Tries `candidate` on main before every build uses it: a golden of main on each toolchain,
/// the repository's own and the candidate, and main's suite on each. When main fails nothing on
/// the candidate that it passes on its own toolchain, every build uses the candidate from then
/// on; otherwise the repository's own golden is made current again.
pub fn try_nightly(location: &Location, candidate: Toolchain) -> Result<Nightly> {
    location.create_dirs()?;
    let main_rev = golden::main_rev(location)?;
    let own = location.with_toolchain(None);
    let (before, _) = golden::refresh(&own, main_rev)?;
    settle::settle_golden(&own, &before.id)?;
    let nightly = location.with_toolchain(Some(candidate.clone()));
    let (after, _) = golden::refresh(&nightly, main_rev)?;
    settle::settle_golden(&nightly, &after.id)?;
    let main = |location: &Location, id: &str| {
        settle::main_failures(location, id).context("main's failures were not recorded")
    };
    let (main_before, main_after) = (main(&own, &before.id)?, main(&nightly, &after.id)?);
    // A test that failed twice on the nightly but passes once in three more runs is flaky, not
    // the nightly's: atuin's terminal tests are.
    let (tests, mut only_nightly): (Vec<String>, Vec<String>) = main_after
        .difference(&main_before)
        .cloned()
        .partition(|failure| failure.starts_with("test "));
    let names: Vec<String> = tests
        .iter()
        .map(|test| test["test ".len()..].to_owned())
        .collect();
    only_nightly.extend(
        settle::failing_every_time(&nightly, &after.id, &names, FLAKY_RUNS)?
            .into_iter()
            .map(|name| format!("test {name}")),
    );
    let (before, after) = (main_before, main_after);
    if only_nightly.is_empty() {
        toolchain::store(&location.state_dir, Some(&candidate))?;
    } else {
        golden::refresh(&own, main_rev)?;
    }
    Ok(Nightly {
        toolchain: candidate,
        only_nightly,
        both: after.intersection(&before).count(),
    })
}

/// The dev-profile settings that made atuin's builds after an edit 34-40% faster and its unit
/// tests twice as fast (`bench/settings-atuin-linux1.txt`): line tables instead of full debuginfo
/// for the workspace, and dependencies optimised without debuginfo, which only the golden builds.
const BUILD_SETTINGS: &str = "
# Faster builds after an edit and faster tests (`agent-hearth setup --build-settings`): backtraces
# keep files and lines; dependencies are built once, optimised.
[profile.dev]
debug = \"line-tables-only\"

[profile.dev.package.\"*\"]
debug = false
opt-level = 1
";

/// Appends [`BUILD_SETTINGS`] to the workspace's `Cargo.toml`. Returns false, changing nothing,
/// when it has dev-profile settings of its own, which another `[profile.dev]` would clash with.
pub fn apply_build_settings(worktree: &Path) -> Result<bool> {
    let path = worktree.join("Cargo.toml");
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let manifest: toml::Table = text
        .parse()
        .with_context(|| format!("{} is not valid TOML", path.display()))?;
    if manifest
        .get("profile")
        .and_then(|profile| profile.get("dev"))
        .is_some()
    {
        return Ok(false);
    }
    let separator = if text.ends_with('\n') { "" } else { "\n" };
    fs::write(&path, format!("{text}{separator}{BUILD_SETTINGS}"))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// Adds each of `hooks` to `settings` that is not there yet, and points those that are at `exe`.
/// Other hooks and settings stay. Returns whether anything changed.
fn merge(settings: &mut Value, exe: &str, hooks: &[Hook]) -> Result<bool> {
    let Some(root) = settings.as_object_mut() else {
        bail!("the settings are not a JSON object");
    };
    let all = root.entry("hooks").or_insert_with(|| json!({}));
    let Some(all) = all.as_object_mut() else {
        bail!("`hooks` in the settings is not a JSON object");
    };
    let mut changed = false;
    for &(event, matcher, name, timeout) in hooks {
        let command = format!("{exe} hook {name}");
        let entries = all.entry(event).or_insert_with(|| json!([]));
        let Some(entries) = entries.as_array_mut() else {
            bail!("`hooks.{event}` in the settings is not a JSON array");
        };
        // An earlier setup's hook, perhaps from another install of the binary.
        let mut found = false;
        for hook in entries
            .iter_mut()
            .filter_map(|entry| entry.get_mut("hooks")?.as_array_mut())
            .flatten()
        {
            let Some(existing) = hook.get("command").and_then(Value::as_str) else {
                continue;
            };
            if existing.ends_with(&format!(" hook {name}")) {
                found = true;
                if existing != command {
                    hook["command"] = json!(command);
                    changed = true;
                }
            }
        }
        if found {
            continue;
        }
        let mut hook = json!({"type": "command", "command": command});
        if let Some(timeout) = timeout {
            hook["timeout"] = json!(timeout);
        }
        let mut entry = json!({"hooks": [hook]});
        if let Some(matcher) = matcher {
            entry["matcher"] = json!(matcher);
        }
        entries.push(entry);
        changed = true;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_into_existing_settings_once_and_follows_the_binary() {
        let mut settings = json!({
            "permissions": {"allow": ["Bash(ls:*)"]},
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "notify-me"}]}]}
        });
        assert!(merge(&mut settings, "/bin/ah", &CLAUDE).unwrap());
        assert_eq!(settings["permissions"]["allow"][0], "Bash(ls:*)");
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "{stop:?}");
        assert_eq!(stop[0]["hooks"][0]["command"], "notify-me");
        assert_eq!(stop[1]["hooks"][0]["command"], "/bin/ah hook claude-stop");
        assert_eq!(stop[1]["hooks"][0]["timeout"], 600);
        assert_eq!(settings["hooks"]["PreToolUse"][0]["matcher"], "Bash");

        let once = settings.clone();
        assert!(!merge(&mut settings, "/bin/ah", &CLAUDE).unwrap());
        assert_eq!(settings, once);

        // Installed elsewhere since: the same hooks, pointed at the new binary.
        assert!(merge(&mut settings, "/opt/ah", &CLAUDE).unwrap());
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[1]["hooks"][0]["command"], "/opt/ah hook claude-stop");
    }

    #[test]
    fn without_a_main_branch_setup_writes_nothing() {
        let s = crate::testing::Sandbox::new();
        s.crate_repo("repo");
        s.run("repo", "git", &["branch", "-m", "main", "develop"]);
        let err = setup(&s.location("repo"), Path::new("ah"), true, true, true)
            .err()
            .unwrap();
        assert!(err.to_string().contains("main or master"), "{err}");
        assert!(!s.path("repo/.claude").exists());
        assert!(!s.path("repo/.codex").exists());
    }

    #[test]
    fn codex_s_post_tool_use_matches_every_tool() {
        let mut settings = json!({});
        merge(&mut settings, "ah", &CODEX).unwrap();
        assert!(settings["hooks"]["PostToolUse"][0].get("matcher").is_none());
        assert_eq!(
            settings["hooks"]["Stop"][0]["hooks"][0]["command"],
            "ah hook codex-stop"
        );
    }

    #[test]
    fn build_settings_are_added_once_and_never_over_a_dev_profile() {
        let s = crate::testing::Sandbox::new();
        s.crate_repo("repo");
        let repo = s.path("repo");
        assert!(apply_build_settings(&repo).unwrap());
        let manifest = fs::read_to_string(repo.join("Cargo.toml")).unwrap();
        let parsed: toml::Table = manifest.parse().unwrap();
        assert_eq!(
            parsed["profile"]["dev"]["debug"].as_str(),
            Some("line-tables-only")
        );
        assert!(
            crate::doctor::profile_advice(&repo).is_empty(),
            "{manifest}"
        );
        assert!(!apply_build_settings(&repo).unwrap(), "not twice");
        assert_eq!(
            fs::read_to_string(repo.join("Cargo.toml")).unwrap(),
            manifest
        );
    }

    #[test]
    fn writes_both_files_then_changes_nothing() {
        let s = crate::testing::Sandbox::new();
        s.crate_repo("repo");
        let repo = s.location("repo");
        let report = setup(&repo, Path::new("/bin/ah"), true, true, false).unwrap();
        assert_eq!(report.written.len(), 2);
        let claude = fs::read_to_string(s.path("repo/.claude/settings.json")).unwrap();
        assert!(
            claude.contains("/bin/ah hook claude-worktree-remove"),
            "{claude}"
        );
        let again = setup(&repo, Path::new("/bin/ah"), true, true, false).unwrap();
        assert!(again.written.is_empty());
        assert_eq!(again.unchanged.len(), 2);
    }

    /// A stand-in for another toolchain: the real cargo, rustc, clippy and rustfmt behind shims,
    /// where rustc reports another version and compiles with `--cfg shim`. Every run is logged.
    #[cfg(unix)]
    fn shim_toolchain(s: &crate::testing::Sandbox) -> Toolchain {
        use std::os::unix::fs::PermissionsExt;
        let bin = s.path("shim/bin");
        fs::create_dir_all(&bin).unwrap();
        let log = s.path("shim/log");
        for program in ["cargo", "rustc", "rustdoc", "cargo-clippy", "rustfmt"] {
            let real = s.run(".", "sh", &["-c", &format!("command -v {program}")]);
            let real = real.trim();
            let body = match program {
                "rustc" => format!(
                    "if [ \"$1\" = -vV ]; then {real} -vV; echo 'shim: yes'; \
                     else exec {real} \"$@\" --cfg shim; fi"
                ),
                _ => format!("exec {real} \"$@\""),
            };
            let path = bin.join(program);
            fs::write(
                &path,
                format!("#!/bin/sh\necho {program} >> {}\n{body}\n", log.display()),
            )
            .unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Toolchain {
            name: "shim".to_owned(),
            bin,
            threads: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_toolchain_main_fails_the_same_on_is_turned_on_and_builds_its_own_golden() {
        let s = crate::testing::Sandbox::new();
        s.crate_repo("repo");
        let location = s.location("repo");
        let shim = shim_toolchain(&s);
        let tried = try_nightly(&location, shim.clone()).unwrap();
        assert!(tried.only_nightly.is_empty(), "{:?}", tried.only_nightly);
        assert_eq!(
            toolchain::load(&location.state_dir).unwrap(),
            Some(shim.clone())
        );

        // The golden every build now seeds from is the shim's own, built with it.
        let location = s.location("repo");
        let current = golden::current(&location).unwrap().unwrap();
        assert_eq!(current.built_with, Some(shim));
        assert!(current.inputs.toolchain.contains("shim: yes"));
        let log = fs::read_to_string(s.path("shim/log")).unwrap();
        for program in ["cargo", "rustc", "cargo-clippy"] {
            assert!(log.lines().any(|line| line == program), "{log}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_toolchain_main_fails_more_on_is_left_off() {
        let s = crate::testing::Sandbox::new();
        s.crate_repo("repo");
        s.write(
            "repo/src/lib.rs",
            "#![allow(unexpected_cfgs)]\n\npub fn one() -> u32 {\n    1\n}\n\n#[cfg(shim)]\n#[test]\n\
             fn only_on_the_shim() {\n    assert_eq!(one(), 2);\n}\n",
        );
        s.run(
            "repo",
            "git",
            &["commit", "-q", "-am", "a test only the shim fails"],
        );
        let location = s.location("repo");
        let tried = try_nightly(&location, shim_toolchain(&s)).unwrap();
        assert_eq!(tried.only_nightly.len(), 1, "{:?}", tried.only_nightly);
        assert!(tried.only_nightly[0].contains("only_on_the_shim"));
        assert_eq!(toolchain::load(&location.state_dir).unwrap(), None);
        let current = golden::current(&location).unwrap().unwrap();
        assert_eq!(current.built_with, None);
    }

    #[cfg(unix)]
    #[test]
    fn a_test_that_fails_twice_on_a_toolchain_then_passes_does_not_keep_it_off() {
        let s = crate::testing::Sandbox::new();
        s.crate_repo("repo");
        let runs = s.path("runs");
        s.write(
            "repo/src/lib.rs",
            &format!(
                "#![allow(unexpected_cfgs)]\n\npub fn one() -> u32 {{\n    1\n}}\n\n#[cfg(shim)]\n\
                 #[test]\nfn flaky_on_the_shim() {{\n    let runs = std::fs::read_to_string({runs:?})\
                 .map_or(0, |s| s.len());\n    std::fs::write({runs:?}, \"x\".repeat(runs + 1))\
                 .unwrap();\n    assert!(runs >= 2, \"run {{runs}}\");\n}}\n"
            ),
        );
        s.run(
            "repo",
            "git",
            &["commit", "-q", "-am", "a test flaky on the shim"],
        );
        let location = s.location("repo");
        let tried = try_nightly(&location, shim_toolchain(&s)).unwrap();
        assert!(tried.only_nightly.is_empty(), "{:?}", tried.only_nightly);
        assert_eq!(
            fs::read_to_string(&runs).unwrap().len(),
            3,
            "failed twice, passed once"
        );
        assert!(toolchain::load(&location.state_dir).unwrap().is_some());
    }
}
