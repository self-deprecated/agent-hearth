//! `agent-hearth setup --split-tests`: moves each inline `#[cfg(test)] mod tests { … }` at the end
//! of a source file to its own `tests.rs` (`mod tests;`), once, in the repository. The library's
//! build no longer reads its tests, so a test-only edit rebuilds the crate's tests, not the crate
//! and everything that depends on it. On Ruff such test-module isolation made a test edit's
//! `cargo test --no-run` 0.085× as long, with byte-identical production binaries (the lab).
//!
//! Only a module that is the file's last item moves, so nothing below it changes line, and the
//! production build reads exactly what it did. Left where they are, with the reason: modules that
//! use their position (`file!`, `line!`, `include*!`, `#[path]`); files a build script names or
//! the crate's code `include_str!`s; every file of a crate whose build script walks directories
//! or reruns after any change; and, in crates using snapshot tooling that finds its files beside
//! the source that calls it (insta, expect-test, snapbox), files whose `tests.rs` would be in
//! another directory. Then verified, or undone: the release build's programs and C-ABI libraries
//! are byte-identical, the test inventory (`cargo test --workspace -- --list`) is the same, and
//! `cargo fmt --check` passes if it passed before.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// Snapshot tools that find snapshot files from `file!()`, beside the source that calls them.
const SNAPSHOT_TOOLS: [&str; 3] = ["insta", "expect-test", "snapbox"];
/// In a test module, what reads its own position or paths relative to its file.
const POSITIONAL: [&str; 7] = [
    "file!(",
    "line!(",
    "column!(",
    "include!(",
    "include_str!(",
    "include_bytes!(",
    "#[path",
];

/// One test module to move.
#[derive(Debug, Clone)]
pub struct Split {
    /// The source file, relative to the worktree, and its content before and after.
    pub file: PathBuf,
    original: String,
    replaced: String,
    /// The new `tests.rs`, relative to the worktree, and its content.
    pub tests: PathBuf,
    tests_source: String,
    edition: String,
}

/// A crate or file left as it is, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub what: String,
    pub why: String,
}

/// The test modules of the workspace's members that can move, and what was refused.
pub fn plan(worktree: &Path) -> Result<(Vec<Split>, Vec<Refusal>)> {
    let metadata = metadata(worktree)?;
    let members: BTreeSet<&str> = metadata["workspace_members"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let (mut splits, mut refusals) = (Vec::new(), Vec::new());
    for package in metadata["packages"].as_array().into_iter().flatten() {
        if !members.contains(package["id"].as_str().unwrap_or_default()) {
            continue;
        }
        let name = package["name"].as_str().unwrap_or_default();
        let manifest = Path::new(package["manifest_path"].as_str().unwrap_or_default());
        let dir = manifest
            .parent()
            .context("a manifest without a directory")?;
        let edition = package["edition"].as_str().unwrap_or("2015").to_owned();
        let targets: Vec<&Value> = package["targets"]
            .as_array()
            .into_iter()
            .flatten()
            .collect();
        let reads = Reads::of(&targets, dir)?;
        if let Some(why) = &reads.walks {
            refusals.push(Refusal {
                what: format!("crate {name}"),
                why: why.clone(),
            });
            continue;
        }
        let snapshots = package["dependencies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|dep| dep["name"].as_str())
            .find(|dep| SNAPSHOT_TOOLS.contains(dep));
        // Crate roots of libraries and programs: integration tests, benches and examples have
        // no inline test modules worth moving.
        let roots: Vec<PathBuf> = targets
            .iter()
            .filter(|target| {
                target["kind"].as_array().into_iter().flatten().any(|kind| {
                    !matches!(
                        kind.as_str(),
                        Some("test" | "bench" | "example" | "custom-build")
                    )
                })
            })
            .filter_map(|target| target["src_path"].as_str().map(PathBuf::from))
            .collect();
        let mut seen = BTreeSet::new();
        for root in &roots {
            walk(root, true, &mut seen, &mut |file, mod_rs, text, items| {
                let relative = file.strip_prefix(worktree).unwrap_or(file).to_path_buf();
                let split =
                    split_file(file, mod_rs, text, items, snapshots).and_then(
                        |split| match &split {
                            Some((_, tests, _)) => reads.allows(file, tests, dir).map(|()| split),
                            None => Ok(None),
                        },
                    );
                match split {
                    Ok(Some((replaced, tests, tests_source))) => splits.push(Split {
                        file: relative,
                        original: text.to_owned(),
                        replaced,
                        tests: tests.strip_prefix(worktree).unwrap_or(&tests).to_path_buf(),
                        tests_source,
                        edition: edition.clone(),
                    }),
                    Ok(None) => {}
                    Err(why) => refusals.push(Refusal {
                        what: relative.display().to_string(),
                        why,
                    }),
                }
            });
        }
    }
    Ok((splits, refusals))
}

fn metadata(worktree: &Path) -> Result<Value> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(worktree)
        .output()
        .context("running cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout).context("parsing cargo metadata")
}

/// What in a crate reads its source files as files, which moving a test module could change.
struct Reads {
    /// Why none of its files may move: its build script walks directories, or reruns on any
    /// change.
    walks: Option<String>,
    /// Paths its build script names (`rerun-if-changed`, `#[path]`, any string), relative to the
    /// package.
    named: Vec<PathBuf>,
    /// Names of the Rust sources its code `include_str!`s or `include_bytes!`, and the files
    /// that include themselves (`file!()`).
    included: BTreeSet<String>,
    including_themselves: BTreeSet<PathBuf>,
}

impl Reads {
    fn of(targets: &[&Value], dir: &Path) -> Result<Self> {
        let mut reads = Reads {
            walks: None,
            named: Vec::new(),
            included: BTreeSet::new(),
            including_themselves: BTreeSet::new(),
        };
        for target in targets {
            let build_script = target["kind"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|kind| kind == "custom-build");
            if !build_script {
                continue;
            }
            let path = Path::new(target["src_path"].as_str().unwrap_or_default());
            let text = fs::read_to_string(path).unwrap_or_default();
            let walking = ["read_dir", "walkdir", "WalkDir", "glob"];
            if let Some(token) = walking.iter().find(|token| text.contains(**token)) {
                reads.walks = Some(format!(
                    "its build script may read or hash its sources: it walks directories \
                     (`{token}`)"
                ));
            } else if !text.contains("rerun-if-") {
                // Cargo reruns it after any change in the package, and what it records (the
                // time, whether the tree is clean: nushell's shadow-rs) would change the release
                // build.
                reads.walks = Some(
                    "its build script declares no `rerun-if-…`, so it reruns after any change \
                     in the crate, and what it records could change the release build"
                        .to_owned(),
                );
            }
            // Every string in it, and what follows `rerun-if-changed=`, as a path.
            let strings = text.split('"').skip(1).step_by(2);
            let watched = text
                .split("rerun-if-changed=")
                .skip(1)
                .filter_map(|rest| rest.split(['"', ')', '\n']).next());
            for named in strings.chain(watched) {
                let named = named.trim().trim_start_matches("./");
                if !named.is_empty() && !named.contains(['{', '}', ' ']) {
                    reads.named.push(PathBuf::from(named));
                }
            }
        }
        for file in rust_files(dir)? {
            let text = fs::read_to_string(&file).unwrap_or_default();
            for include in ["include_str!(", "include_bytes!("] {
                for (at, _) in text.match_indices(include) {
                    let argument = text[at..].split(')').next().unwrap_or_default();
                    if argument.contains("file!") {
                        reads.including_themselves.insert(file.clone());
                    }
                    let names = argument.split(['"', '/', '\\']);
                    reads.included.extend(
                        names
                            .filter(|name| name.ends_with(".rs"))
                            .map(str::to_owned),
                    );
                }
            }
        }
        Ok(reads)
    }

    /// Whether `file`'s test module may move to `tests`, in the package at `dir`: neither is a
    /// path the build script names or is under one, and no code includes `file`.
    fn allows(&self, file: &Path, tests: &Path, dir: &Path) -> std::result::Result<(), String> {
        for path in [file, tests] {
            let relative = path.strip_prefix(dir).unwrap_or(path);
            if let Some(named) = self.named.iter().find(|named| relative.starts_with(named)) {
                return Err(format!(
                    "the build script names `{}`, which holds {}",
                    named.display(),
                    relative.display()
                ));
            }
        }
        let name = file.file_name().unwrap_or_default().to_string_lossy();
        if self.included.contains(name.as_ref()) || self.including_themselves.contains(file) {
            return Err(format!(
                "the crate's code `include_str!`s or `include_bytes!` a file named {name}"
            ));
        }
        Ok(())
    }
}

/// Every `.rs` file under `dir`, not under a `target` directory or another package's.
fn rust_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(next) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&next) else {
            continue;
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                let name = entry.file_name();
                let nested = path != dir && path.join("Cargo.toml").is_file();
                if name != "target" && name != ".git" && name != ".jj" && !nested {
                    dirs.push(path);
                }
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    Ok(files)
}

/// What [`walk`] calls for each module file: its path, whether it is a "mod.rs" file, its text
/// and its items.
type Visit<'a> = dyn FnMut(&Path, bool, &str, &[syn::Item]) + 'a;

/// Visits `file` (a crate root when `root`) and every module file it declares (`mod name;`,
/// without `#[path]`), with whether it is a "mod.rs" file (a crate root or `mod.rs`: its modules
/// live beside it), its text and its items. A file that does not parse is skipped.
fn walk(file: &Path, root: bool, seen: &mut BTreeSet<PathBuf>, visit: &mut Visit) {
    if !seen.insert(file.to_path_buf()) {
        return;
    }
    let Ok(text) = fs::read_to_string(file) else {
        return;
    };
    let Ok(parsed) = syn::parse_file(&text) else {
        return;
    };
    // A crate root's and a `mod.rs`'s modules live beside it; any other file's in a directory
    // named after it.
    let mod_rs = root || file.ends_with("mod.rs");
    let children = match file.parent() {
        Some(dir) if mod_rs => dir.to_path_buf(),
        _ => file.with_extension(""),
    };
    visit(file, mod_rs, &text, &parsed.items);
    declared(&parsed.items, &children, seen, visit);
}

fn declared(items: &[syn::Item], children: &Path, seen: &mut BTreeSet<PathBuf>, visit: &mut Visit) {
    for item in items {
        let syn::Item::Mod(module) = item else {
            continue;
        };
        if module.attrs.iter().any(|attr| attr.path().is_ident("path")) {
            continue;
        }
        let name = module.ident.to_string();
        match &module.content {
            // Inline: its own `mod x;` live in a directory named after it.
            Some((_, inner)) => declared(inner, &children.join(&name), seen, visit),
            None => {
                let flat = children.join(format!("{name}.rs"));
                let nested = children.join(&name).join("mod.rs");
                let child = if flat.is_file() { flat } else { nested };
                if child.is_file() {
                    walk(&child, false, seen, visit);
                }
            }
        }
    }
}

/// The file's new text, the new `tests.rs` and its text, when its last item is an inline
/// `#[cfg(test)] mod tests` that can move; `Ok(None)` when it has none; why not, otherwise.
fn split_file(
    file: &Path,
    mod_rs: bool,
    text: &str,
    items: &[syn::Item],
    snapshots: Option<&str>,
) -> std::result::Result<Option<(String, PathBuf, String)>, String> {
    let is_tests = |item: &syn::Item| match item {
        syn::Item::Mod(module) => {
            module.ident == "tests" && module.content.is_some() && cfg_test(&module.attrs)
        }
        _ => false,
    };
    let Some(at) = items.iter().position(is_tests) else {
        return Ok(None);
    };
    if at + 1 != items.len() {
        return Err("code follows its tests module, which would move up".to_owned());
    }
    let syn::Item::Mod(module) = &items[at] else {
        return Ok(None);
    };
    let Some((brace, _)) = &module.content else {
        return Ok(None);
    };
    let (open, close) = (brace.span.open().start(), brace.span.close().start());
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let line = |n: usize| lines.get(n - 1).copied().unwrap_or_default();
    let open_line = line(open.line);
    let close_line = line(close.line);
    if open_line
        .chars()
        .skip(open.column + 1)
        .any(|c| !c.is_whitespace())
        || close_line.trim() != "}"
        || close.column != 0
    {
        return Err("its tests module is laid out unusually".to_owned());
    }
    let body: String = lines[open.line..close.line - 1].concat();
    if let Some(token) = POSITIONAL.iter().find(|token| body.contains(**token)) {
        return Err(format!(
            "its tests module uses `{}`, which depends on where it is",
            token.trim_end_matches('(')
        ));
    }
    if body.lines().any(|line| {
        let line = line.trim();
        line.starts_with("mod ") && line.ends_with(';')
            || line.starts_with("pub mod ") && line.ends_with(';')
    }) {
        return Err("its tests module declares modules in other files".to_owned());
    }
    let dir = if mod_rs {
        file.parent().map(Path::to_path_buf).unwrap_or_default()
    } else {
        file.with_extension("")
    };
    if let Some(tool) = snapshots
        && !mod_rs
    {
        return Err(format!(
            "{tool} finds snapshots beside the file that calls it, and `tests.rs` would be in {}",
            dir.display()
        ));
    }
    let tests = dir.join("tests.rs");
    if tests.exists() || dir.join("tests").join("mod.rs").exists() {
        return Err(format!("{} exists", tests.display()));
    }
    // Everything before the `{` on its line, then `;`, then whatever follows the `}`.
    let start: usize = lines[..open.line - 1].iter().map(|line| line.len()).sum();
    let brace_at = start
        + open_line
            .char_indices()
            .nth(open.column)
            .map_or(open_line.len(), |(at, _)| at);
    let end: usize = lines[..close.line].iter().map(|line| line.len()).sum();
    let replaced = format!("{};\n{}", text[..brace_at].trim_end(), &text[end..]);
    Ok(Some((replaced, tests, dedent(&body, text, open.line))))
}

fn cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .meta
                .require_list()
                .is_ok_and(|list| list.tokens.to_string() == "test")
    })
}

/// `body` (the file's lines after `first`, 1-based) one level less indented, except the lines
/// that continue a multi-line string literal, whose leading spaces are the string's.
fn dedent(body: &str, file: &str, first: usize) -> String {
    let mut inside = BTreeSet::new();
    if let Ok(tokens) = file.parse::<proc_macro2::TokenStream>() {
        literal_lines(tokens, &mut inside);
    }
    let lines: Vec<&str> = body.split_inclusive('\n').collect();
    let line_no = |i: usize| first + 1 + i;
    let indent = lines
        .iter()
        .enumerate()
        .filter(|(i, line)| !line.trim().is_empty() && !inside.contains(&line_no(*i)))
        .map(|(_, line)| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if inside.contains(&line_no(i)) {
            out.push_str(line);
        } else if line.trim().is_empty() {
            out.push('\n');
        } else {
            out.push_str(&line[indent.min(line.len() - line.trim_start().len())..]);
        }
    }
    let trimmed = out.trim_matches('\n');
    format!("{trimmed}\n")
}

/// The lines (1-based) that continue a literal spanning several lines.
fn literal_lines(tokens: proc_macro2::TokenStream, inside: &mut BTreeSet<usize>) {
    for token in tokens {
        match token {
            proc_macro2::TokenTree::Group(group) => literal_lines(group.stream(), inside),
            proc_macro2::TokenTree::Literal(literal) => {
                let span = literal.span();
                let (start, end) = (span.start().line, span.end().line);
                inside.extend(start + 1..=end);
            }
            _ => {}
        }
    }
}

/// Writes the splits, the new files formatted with `rustfmt` where it runs.
pub fn apply(worktree: &Path, splits: &[Split]) -> Result<()> {
    for split in splits {
        let tests = worktree.join(&split.tests);
        if let Some(dir) = tests.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        fs::write(&tests, &split.tests_source)
            .with_context(|| format!("writing {}", tests.display()))?;
        let _ = Command::new("rustfmt")
            .args(["--edition", &split.edition])
            .arg(&tests)
            .current_dir(worktree)
            .output();
        let file = worktree.join(&split.file);
        fs::write(&file, &split.replaced).with_context(|| format!("writing {}", file.display()))?;
    }
    Ok(())
}

/// Undoes [`apply`].
pub fn undo(worktree: &Path, splits: &[Split]) -> Result<()> {
    for split in splits {
        let file = worktree.join(&split.file);
        fs::write(&file, &split.original).with_context(|| format!("writing {}", file.display()))?;
        let tests = worktree.join(&split.tests);
        match fs::remove_file(&tests) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                return Err(err).with_context(|| format!("removing {}", tests.display()));
            }
            _ => {}
        }
        // A directory made for it, if it holds nothing else.
        if let Some(dir) = tests.parent() {
            let _ = fs::remove_dir(dir);
        }
    }
    Ok(())
}

/// What the split must leave as it was.
#[derive(Debug, PartialEq, Eq)]
pub struct Evidence {
    /// The release build's programs and C-ABI libraries, by target, and a hash of each.
    pub release: BTreeMap<String, String>,
    /// `cargo test --workspace -- --list`'s lines, sorted.
    pub inventory: Vec<String>,
    /// Whether `cargo fmt --all --check` passed.
    pub formatted: bool,
}

/// Builds the release and the tests of `worktree` with plain cargo, in its own build directory,
/// and records what the split must not change. `cargo` runs with `progress` on stderr's lines.
pub fn evidence(worktree: &Path, progress: &mut dyn FnMut(&str)) -> Result<Evidence> {
    let cargo = |args: &[&str]| -> Result<(bool, String, String)> {
        let output = Command::new("cargo")
            .args(args)
            .current_dir(worktree)
            .output()
            .with_context(|| format!("running cargo {}", args.join(" ")))?;
        Ok((
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    };
    let last = |stderr: &str| {
        stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("no output")
            .to_owned()
    };
    progress("cargo build --release --workspace");
    let (ok, stdout, stderr) =
        cargo(&["build", "--release", "--workspace", "--message-format=json"])?;
    if !ok {
        bail!("the release build failed: {}", last(&stderr));
    }
    let mut release = BTreeMap::new();
    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if message["reason"] != "compiler-artifact"
            || !message["package_id"]
                .as_str()
                .is_some_and(|id| id.contains("path+file://"))
        {
            continue;
        }
        let target = format!(
            "{} {}",
            message["target"]["kind"],
            message["target"]["name"].as_str().unwrap_or_default()
        );
        // C-ABI libraries ship; proc macros and Rust dylibs carry their crate's metadata, which
        // records its sources' hashes (ruff's `ruff_macros` differed), and what they generate
        // shows in the programs.
        let kinds = &message["target"]["kind"];
        let shipped = ["cdylib", "staticlib"].iter().any(|kind| {
            kinds
                .as_array()
                .is_some_and(|all| all.iter().any(|k| k == kind))
        });
        let artifacts = message["filenames"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(PathBuf::from)
            .filter(|path| {
                Some(path.as_path()) == message["executable"].as_str().map(Path::new)
                    || shipped
                        && path.extension().is_some_and(|ext| {
                            ["so", "dylib", "dll", "a", "lib"].iter().any(|e| ext == *e)
                        })
            });
        for artifact in artifacts {
            let bytes =
                fs::read(&artifact).with_context(|| format!("reading {}", artifact.display()))?;
            let name = artifact.file_name().unwrap_or_default().to_string_lossy();
            release.insert(
                format!("{target} {name}"),
                blake3::hash(&bytes).to_hex().to_string(),
            );
        }
    }
    progress("cargo test --workspace -- --list");
    let (ok, stdout, stderr) = cargo(&["test", "--workspace", "--", "--list"])?;
    if !ok {
        bail!("listing the tests failed: {}", last(&stderr));
    }
    let mut inventory: Vec<String> = stdout
        .lines()
        .filter(|line| line.ends_with(": test") || line.ends_with(": benchmark"))
        .map(str::to_owned)
        .collect();
    inventory.sort();
    progress("cargo fmt --all --check");
    let formatted = cargo(&["fmt", "--all", "--check"]).is_ok_and(|(ok, ..)| ok);
    Ok(Evidence {
        release,
        inventory,
        formatted,
    })
}

/// Why `after` does not match `before`, if it does not.
pub fn differs(before: &Evidence, after: &Evidence) -> Option<String> {
    if before.release != after.release {
        let changed: Vec<&String> = before
            .release
            .iter()
            .filter(|(target, hash)| after.release.get(*target) != Some(hash))
            .map(|(target, _)| target)
            .chain(
                after
                    .release
                    .keys()
                    .filter(|t| !before.release.contains_key(*t)),
            )
            .collect();
        return Some(format!(
            "the release build changed: {}",
            changed
                .iter()
                .map(|target| target.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if before.inventory != after.inventory {
        let gone = before
            .inventory
            .iter()
            .find(|test| !after.inventory.contains(test));
        let new = after
            .inventory
            .iter()
            .find(|test| !before.inventory.contains(test));
        return Some(format!(
            "the test inventory changed ({} tests before, {} after; first missing: {}, first new: {})",
            before.inventory.len(),
            after.inventory.len(),
            gone.map_or("none", String::as_str),
            new.map_or("none", String::as_str)
        ));
    }
    if before.formatted && !after.formatted {
        return Some("`cargo fmt --all --check` passed before and fails after".to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Sandbox;

    const LIB: &str = "\
//! A library.

pub fn one() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_is_one() {
        let text = \"a
    indented line\";
        assert_eq!(one(), 1, \"{text}\");
    }
}
";

    fn parsed(text: &str) -> Vec<syn::Item> {
        syn::parse_file(text).unwrap().items
    }

    #[test]
    fn moves_a_trailing_test_module_and_keeps_string_contents() {
        let items = parsed(LIB);
        let (replaced, tests, source) =
            split_file(Path::new("/w/src/lib.rs"), true, LIB, &items, None)
                .unwrap()
                .unwrap();
        assert_eq!(
            replaced,
            "//! A library.\n\npub fn one() -> u32 {\n    1\n}\n\n#[cfg(test)]\nmod tests;\n"
        );
        assert_eq!(tests, Path::new("/w/src/tests.rs"));
        assert_eq!(
            source,
            "use super::*;\n\n#[test]\nfn one_is_one() {\n    let text = \"a\n    indented line\";\n    assert_eq!(one(), 1, \"{text}\");\n}\n"
        );
        // A file that is not a crate root or `mod.rs` keeps its tests in a directory of its own.
        let (_, tests, _) = split_file(Path::new("/w/src/util.rs"), false, LIB, &items, None)
            .unwrap()
            .unwrap();
        assert_eq!(tests, Path::new("/w/src/util/tests.rs"));
    }

    #[test]
    fn leaves_what_cannot_move_and_says_why() {
        let after = format!("{LIB}\npub fn two() -> u32 {{\n    2\n}}\n");
        let why = split_file(
            Path::new("/w/src/lib.rs"),
            true,
            &after,
            &parsed(&after),
            None,
        );
        assert!(why.unwrap_err().contains("code follows"));

        let positional = LIB.replace(
            "use super::*;",
            "use super::*;\n    const L: u32 = line!();",
        );
        let why = split_file(
            Path::new("/w/src/lib.rs"),
            true,
            &positional,
            &parsed(&positional),
            None,
        );
        assert!(why.unwrap_err().contains("`line!`"));

        let why = split_file(
            Path::new("/w/src/util.rs"),
            false,
            LIB,
            &parsed(LIB),
            Some("insta"),
        );
        assert!(why.unwrap_err().starts_with("insta finds snapshots"));
        // Beside a crate root, insta finds its snapshots where it did.
        let same_dir = split_file(
            Path::new("/w/src/lib.rs"),
            true,
            LIB,
            &parsed(LIB),
            Some("insta"),
        );
        assert!(same_dir.unwrap().is_some());

        let plain = "pub fn one() -> u32 {\n    1\n}\n";
        assert!(
            split_file(
                Path::new("/w/src/lib.rs"),
                true,
                plain,
                &parsed(plain),
                None
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn plans_the_crate_s_module_files_and_refuses_a_build_script_reading_sources() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        s.write(
            "repo/src/lib.rs",
            &LIB.replacen("//! A library.\n", "//! A library.\n\nmod util;\n", 1),
        );
        s.write("repo/src/util.rs", LIB);
        let worktree = s.path("repo");
        let (splits, refusals) = plan(&worktree).unwrap();
        assert!(refusals.is_empty(), "{refusals:?}");
        let moved: Vec<_> = splits
            .iter()
            .map(|split| (split.file.clone(), split.tests.clone()))
            .collect();
        assert_eq!(
            moved,
            [
                (PathBuf::from("src/lib.rs"), PathBuf::from("src/tests.rs")),
                (
                    PathBuf::from("src/util.rs"),
                    PathBuf::from("src/util/tests.rs")
                ),
            ]
        );
        apply(&worktree, &splits).unwrap();
        assert!(s.path("repo/src/util/tests.rs").is_file());
        s.run("repo", "cargo", &["test", "--quiet"]);
        undo(&worktree, &splits).unwrap();
        assert!(!s.path("repo/src/util").exists());
        assert_eq!(fs::read_to_string(s.path("repo/src/util.rs")).unwrap(), LIB);

        // A build script that watches `src/util.rs`'s directory: it stays, `src/lib.rs` moves.
        s.write(
            "repo/build.rs",
            "fn main() {\n    println!(\"cargo::rerun-if-changed=src/util\");\n}\n",
        );
        let (splits, refusals) = plan(&worktree).unwrap();
        assert_eq!(splits.len(), 1);
        assert_eq!(refusals.len(), 1, "{refusals:?}");
        assert_eq!(refusals[0].what, "src/util.rs");
        assert!(refusals[0].why.contains("names `src/util`"), "{refusals:?}");
        // One that reruns after any change, or walks directories: nothing in the crate moves.
        s.write("repo/build.rs", "fn main() {}\n");
        let (splits, refusals) = plan(&worktree).unwrap();
        assert!(splits.is_empty());
        assert!(refusals[0].why.contains("no `rerun-if-…`"), "{refusals:?}");
        s.write(
            "repo/build.rs",
            "fn main() {\n    for _ in std::fs::read_dir(\"src\").unwrap() {}\n}\n",
        );
        let (splits, refusals) = plan(&worktree).unwrap();
        assert!(splits.is_empty());
        assert_eq!(refusals.len(), 1, "{refusals:?}");
        assert!(
            refusals[0].why.contains("walks directories"),
            "{refusals:?}"
        );
    }
}
