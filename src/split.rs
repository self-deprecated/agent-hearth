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
//! the source that calls it (insta, expect-test, snapbox, through any dependency), files whose
//! `tests.rs` would be in another directory. Then verified, or undone: the release build's
//! programs and C-ABI libraries are byte-identical, the test inventory (`cargo test --workspace --
//! --list`) is the same, no unit test that passed before fails after (each failure confirmed by a
//! second run), and `cargo fmt --check` passes if it passed before.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// Snapshot tools that find snapshot files from `file!()`, beside the source that calls them.
const SNAPSHOT_TOOLS: [&str; 3] = ["insta", "expect-test", "snapbox"];
/// In a test module, the macros that read its own position or paths relative to its file.
const POSITIONAL: [&str; 6] = [
    "file",
    "line",
    "column",
    "include",
    "include_str",
    "include_bytes",
];
/// In a build script, what walks directories: it may read or hash the crate's sources.
const WALKING: [&str; 7] = [
    "read_dir",
    "walkdir",
    "WalkDir",
    "WalkBuilder",
    "Walk::new",
    "glob",
    "jwalk",
];

/// One test module to move.
#[derive(Debug, Clone)]
pub struct Split {
    /// The source file, relative to the worktree, and its content before and after.
    pub file: PathBuf,
    original: String,
    replaced: String,
    /// The new `tests.rs`, relative to the worktree, and its content, formatted.
    pub tests: PathBuf,
    tests_source: String,
}

/// A crate or file left as it is, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub what: String,
    pub why: String,
}

/// The test modules of the workspace's members that can move, and what was refused.
pub fn plan(worktree: &Path) -> Result<(Vec<Split>, Vec<Refusal>)> {
    let metadata = crate::cargo::metadata(worktree, false)?;
    // Through any dependency: insta's macros, called from a helper crate's, still find
    // snapshots from the file that calls them.
    let snapshot_users = crate::cargo::reaching(&metadata, &SNAPSHOT_TOOLS);
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
        let snapshots = snapshot_users
            .get(package["id"].as_str().unwrap_or_default())
            .map(String::as_str);
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
                            Some((_, tests, _)) => reads
                                .allows(file, tests, dir)
                                .and_then(|()| not_a_target(tests, dir))
                                .map(|()| split),
                            None => Ok(None),
                        },
                    );
                match split {
                    Ok(Some((replaced, tests, tests_source))) => splits.push(Split {
                        file: relative,
                        original: text.to_owned(),
                        replaced,
                        tests: tests.strip_prefix(worktree).unwrap_or(&tests).to_path_buf(),
                        tests_source: rustfmt(&tests_source, &edition, file),
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
    // Two crate roots in one directory (`lib.rs` and `main.rs`) would both write its `tests.rs`:
    // neither moves.
    let mut writers: BTreeMap<PathBuf, usize> = BTreeMap::new();
    for split in &splits {
        *writers.entry(split.tests.clone()).or_default() += 1;
    }
    let (splits, shared): (Vec<Split>, Vec<Split>) = splits
        .into_iter()
        .partition(|split| writers[&split.tests] == 1);
    refusals.extend(shared.into_iter().map(|split| Refusal {
        what: split.file.display().to_string(),
        why: format!(
            "another crate root in its directory would also move its tests to {}",
            split.tests.display()
        ),
    }));
    Ok((splits, refusals))
}

/// Refuses a `tests.rs` where cargo finds targets of its own (`src/bin/tests.rs` would be a
/// program `tests`), in the package at `dir`.
fn not_a_target(tests: &Path, dir: &Path) -> std::result::Result<(), String> {
    let found = ["src/bin", "tests", "benches", "examples"]
        .iter()
        .any(|targets| tests.parent() == Some(dir.join(targets).as_path()));
    if found {
        return Err(format!(
            "cargo would build {} as a target of its own",
            tests.display()
        ));
    }
    Ok(())
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
            // Its root and every module file it declares.
            let text = build_script_sources(path).concat();
            if let Some(token) = WALKING.iter().find(|token| text.contains(**token)) {
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

/// The text of a build script's root `file` and of every module file it declares, `#[path]`
/// ones included.
fn build_script_sources(file: &Path) -> Vec<String> {
    let mut texts = Vec::new();
    let mut seen = BTreeSet::new();
    let mut next = vec![(file.to_path_buf(), true)];
    while let Some((file, mod_rs)) = next.pop() {
        if !seen.insert(file.clone()) {
            continue;
        }
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        if let (Ok(parsed), Some(dir)) = (syn::parse_file(&text), file.parent()) {
            let children = if mod_rs || file.ends_with("mod.rs") {
                dir.to_path_buf()
            } else {
                file.with_extension("")
            };
            module_files(&parsed.items, dir, &children, &mut next);
        }
        texts.push(text);
    }
    texts
}

/// The files of the modules `items` declare, with whether their own live beside them: `dir` is
/// the declaring file's directory, `children` where its modules live.
fn module_files(items: &[syn::Item], dir: &Path, children: &Path, next: &mut Vec<(PathBuf, bool)>) {
    for item in items {
        let syn::Item::Mod(module) = item else {
            continue;
        };
        let path = module.attrs.iter().find_map(|attr| {
            let value = &attr.meta.require_name_value().ok()?.value;
            match value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(path),
                    ..
                }) if attr.path().is_ident("path") => Some(path.value()),
                _ => None,
            }
        });
        let name = module.ident.to_string();
        match (path, &module.content) {
            (Some(path), _) => next.push((dir.join(path), true)),
            (None, Some((_, inner))) => module_files(inner, dir, &children.join(&name), next),
            (None, None) => {
                next.push((children.join(format!("{name}.rs")), false));
                next.push((children.join(&name).join("mod.rs"), true));
            }
        }
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
        if module.attrs.iter().any(sets_path) {
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
    let tokens: proc_macro2::TokenStream = body
        .parse()
        .map_err(|_| "its tests module does not read as Rust tokens".to_owned())?;
    if let Some(why) = depends_on_place(tokens, false) {
        return Err(why);
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

/// Whether an attribute sets a module's file: `#[path = …]`, or `#[cfg_attr(…, path = …)]`.
fn sets_path(attr: &syn::Attribute) -> bool {
    attr.path().is_ident("path")
        || attr.path().is_ident("cfg_attr")
            && attr.meta.require_list().is_ok_and(|list| {
                depends_on_place(list.tokens.clone(), true).is_some_and(|why| why.contains("path"))
            })
}

/// What in `tokens` depends on where their file is, however spaced: a macro in [`POSITIONAL`]
/// (`file!`, `include_str! (…)`), a `path = …` in an attribute (`#[path]`, `# [cfg_attr(…, path
/// = …)]`; `attribute`: `tokens` are in one), or a module in another file (`pub(crate) mod x;`).
fn depends_on_place(tokens: proc_macro2::TokenStream, attribute: bool) -> Option<String> {
    use proc_macro2::{Delimiter, TokenTree};
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    let punct = |at: usize, c: char| matches!(tokens.get(at), Some(TokenTree::Punct(punct)) if punct.as_char() == c);
    for (at, token) in tokens.iter().enumerate() {
        match token {
            TokenTree::Ident(ident) => {
                let name = ident.to_string();
                // `line!(…)`, not `line != …`.
                if POSITIONAL.contains(&name.as_str()) && punct(at + 1, '!') && !punct(at + 2, '=')
                {
                    return Some(format!(
                        "its tests module uses `{name}!`, which depends on where it is"
                    ));
                }
                if attribute && name == "path" && punct(at + 1, '=') {
                    return Some(
                        "its tests module uses `#[path]`, which depends on where it is".to_owned(),
                    );
                }
                if name == "mod"
                    && matches!(tokens.get(at + 1), Some(TokenTree::Ident(_)))
                    && punct(at + 2, ';')
                {
                    return Some("its tests module declares modules in other files".to_owned());
                }
            }
            TokenTree::Group(group) => {
                let inside = attribute
                    || (group.delimiter() == Delimiter::Bracket && punct_before(&tokens, at));
                if let Some(why) = depends_on_place(group.stream(), inside) {
                    return Some(why);
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether the token before `at` is `#` (or `#!`): the group at `at` is an attribute.
fn punct_before(tokens: &[proc_macro2::TokenTree], at: usize) -> bool {
    use proc_macro2::TokenTree;
    let is = |at: usize, c: char| matches!(tokens.get(at), Some(TokenTree::Punct(punct)) if punct.as_char() == c);
    at > 0 && (is(at - 1, '#') || is(at - 1, '!') && at > 1 && is(at - 2, '#'))
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

/// `source` formatted by `rustfmt` for `edition`, with the configuration that applies beside
/// `file`; as it is where `rustfmt` does not run.
fn rustfmt(source: &str, edition: &str, file: &Path) -> String {
    let formatted = || -> Option<String> {
        let mut child = Command::new("rustfmt")
            .args(["--edition", edition])
            .current_dir(file.parent()?)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        child.stdin.take()?.write_all(source.as_bytes()).ok()?;
        let output = child.wait_with_output().ok()?;
        output.status.success().then_some(())?;
        String::from_utf8(output.stdout).ok()
    };
    formatted().unwrap_or_else(|| source.to_owned())
}

/// A file's text, `None` when it does not exist.
fn text(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if matches!(err.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            Ok(None)
        }
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes the splits, if every file is still as planned and no `tests.rs` exists; else changes
/// nothing. A write that fails part way is undone.
pub fn apply(worktree: &Path, splits: &[Split]) -> Result<()> {
    let mut changed = Vec::new();
    for split in splits {
        let file = worktree.join(&split.file);
        if text(&file)?.as_deref() != Some(split.original.as_str()) {
            changed.push(format!("{} changed", split.file.display()));
        }
        let tests = worktree.join(&split.tests);
        if tests.exists() || tests.with_extension("").join("mod.rs").exists() {
            changed.push(format!("{} exists", split.tests.display()));
        }
    }
    if !changed.is_empty() {
        bail!(
            "nothing moved, as the tree changed since the plan: {}",
            changed.join(", ")
        );
    }
    let write = |split: &Split| -> Result<()> {
        let tests = worktree.join(&split.tests);
        if let Some(dir) = tests.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tests)
            .and_then(|mut file| file.write_all(split.tests_source.as_bytes()))
            .with_context(|| format!("writing {}", tests.display()))?;
        let file = worktree.join(&split.file);
        fs::write(&file, &split.replaced).with_context(|| format!("writing {}", file.display()))
    };
    for (at, split) in splits.iter().enumerate() {
        if let Err(err) = write(split) {
            return Err(match undo(worktree, &splits[..=at]) {
                Ok(()) => err.context("moving the test modules, now undone"),
                Err(undo) => err.context(format!("moving the test modules, and undoing: {undo:#}")),
            });
        }
    }
    Ok(())
}

/// Whether every split is in place as [`apply`] wrote it.
pub fn intact(worktree: &Path, splits: &[Split]) -> Result<Vec<String>> {
    let mut changed = Vec::new();
    for split in splits {
        if text(&worktree.join(&split.file))?.as_deref() != Some(split.replaced.as_str()) {
            changed.push(split.file.display().to_string());
        }
        if text(&worktree.join(&split.tests))?.as_deref() != Some(split.tests_source.as_str()) {
            changed.push(split.tests.display().to_string());
        }
    }
    Ok(changed)
}

/// Undoes [`apply`], all of it or part: each file must be as `apply` wrote it, or as it was
/// before; if any is neither, it was edited since, and nothing changes.
pub fn undo(worktree: &Path, splits: &[Split]) -> Result<()> {
    let mut edited = Vec::new();
    for split in splits {
        let file = text(&worktree.join(&split.file))?;
        let as_left = |text: &str| file.as_deref() == Some(text);
        if !as_left(&split.replaced) && !as_left(&split.original) {
            edited.push(split.file.display().to_string());
        }
        let tests = text(&worktree.join(&split.tests))?;
        if tests.is_some_and(|tests| tests != split.tests_source) {
            edited.push(split.tests.display().to_string());
        }
    }
    if !edited.is_empty() {
        bail!(
            "not undone, as these changed after the move: {}; undo it by hand",
            edited.join(", ")
        );
    }
    for split in splits {
        let file = worktree.join(&split.file);
        if text(&file)?.as_deref() == Some(split.replaced.as_str()) {
            fs::write(&file, &split.original)
                .with_context(|| format!("writing {}", file.display()))?;
        }
        let tests = worktree.join(&split.tests);
        if text(&tests)?.is_some() {
            fs::remove_file(&tests).with_context(|| format!("removing {}", tests.display()))?;
        }
        // A directory made for it, if it holds nothing else.
        if let Some(dir) = tests.parent() {
            let _ = fs::remove_dir(dir);
        }
    }
    Ok(())
}

/// Set by SIGINT or SIGTERM once [`catch_interrupts`] ran: cargo stops, and the split is undone.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// From now on, SIGINT and SIGTERM end the split's cargo and undo the split rather than end the
/// process with the split half verified. Cargo runs in a process group of its own, so Ctrl-C
/// reaches it only through this.
pub fn catch_interrupts() {
    #[cfg(unix)]
    {
        extern "C" fn on_signal(_: libc::c_int) {
            INTERRUPTED.store(true, Ordering::SeqCst);
        }
        let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // SAFETY: the handler only stores to an atomic.
        unsafe {
            libc::signal(libc::SIGINT, handler);
            libc::signal(libc::SIGTERM, handler);
        }
    }
}

fn interrupted() -> Result<()> {
    if INTERRUPTED.load(Ordering::SeqCst) {
        bail!("interrupted");
    }
    Ok(())
}

/// Runs `command` to its end, or until interrupted ([`catch_interrupts`]): then it and its
/// children are stopped. Whether it succeeded, its stdout and its stderr; with `merged`, both in
/// one, in the order written (cargo's `Running` lines on stderr name the test binary whose
/// results follow on stdout), and stderr empty.
fn run(command: &mut Command, merged: bool) -> Result<(bool, String, String)> {
    interrupted()?;
    command.stdin(Stdio::null());
    let pipe = if merged {
        let (reader, writer) = std::io::pipe()?;
        command.stdout(writer.try_clone()?).stderr(writer);
        Some(reader)
    } else {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        None
    };
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    let spawned = command.spawn();
    // The command's ends of the pipe, so that it ends when the child exits.
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = spawned?;
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            String::from_utf8_lossy(&bytes).into_owned()
        })
    };
    let stdout: Option<Box<dyn Read + Send>> = match pipe {
        Some(reader) => Some(Box::new(reader)),
        None => child.stdout.take().map(|pipe| Box::new(pipe) as _),
    };
    let stdout = read(stdout);
    let stderr = read(child.stderr.take().map(|pipe| Box::new(pipe) as _));
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if INTERRUPTED.load(Ordering::SeqCst) {
            #[cfg(unix)]
            if let Ok(group) = libc::pid_t::try_from(child.id()) {
                // SAFETY: signalling the process group cargo leads.
                unsafe { libc::kill(-group, libc::SIGTERM) };
            }
            let _ = child.kill();
            let _ = child.wait();
            bail!("interrupted");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    Ok((status.success(), stdout, stderr))
}

/// How a split ended.
#[derive(Debug)]
pub enum Outcome {
    /// Moved and verified: what the release build and the tests were.
    Kept(Evidence),
    /// Moved, found to change something, and undone: why.
    Undone(String),
}

/// Records `evidence`, moves `splits`, records it again, and keeps the move only if nothing it
/// must leave alone changed. An error, or an interrupt, after the move undoes it.
pub fn transact(
    worktree: &Path,
    splits: &[Split],
    evidence: &mut dyn FnMut() -> Result<Evidence>,
) -> Result<Outcome> {
    let before = evidence().context("before moving anything")?;
    interrupted()?;
    apply(worktree, splits)?;
    let undone = |why: anyhow::Error| match undo(worktree, splits) {
        Ok(()) => why.context("after moving the test modules, now undone"),
        Err(undo) => why.context(format!(
            "after moving the test modules, and undoing: {undo:#}"
        )),
    };
    let after = match interrupted().and_then(|()| evidence()) {
        Ok(after) => after,
        Err(err) => return Err(undone(err)),
    };
    if let Some(why) = differs(&before, &after) {
        undo(worktree, splits).with_context(|| format!("undoing, as {why}"))?;
        return Ok(Outcome::Undone(why));
    }
    if let Err(err) = interrupted() {
        return Err(undone(err));
    }
    let changed = intact(worktree, splits)?;
    if !changed.is_empty() {
        bail!(
            "moved, but not verified: these changed while it was being verified: {}",
            changed.join(", ")
        );
    }
    Ok(Outcome::Kept(after))
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
    /// The unit tests (`--lib --bins`) that failed when run and again when run alone, each as its
    /// test binary's file name and its name; `<binary> did not finish` for one that crashed.
    pub failed: BTreeSet<String>,
}

/// Builds the release and the tests of `worktree` with plain cargo, in the build directory
/// `target` (not the one the environment or configuration names, which may be anywhere), never
/// rewriting `Cargo.lock` (`--locked`), and records what the split must not change. Calls
/// `progress` with each step.
pub fn evidence(
    worktree: &Path,
    target: &Path,
    progress: &mut dyn FnMut(&str),
) -> Result<Evidence> {
    // `cargo <args>`, `--locked` and the build directory before any `--`.
    let command = |args: &[&str]| {
        let at = args
            .iter()
            .position(|arg| *arg == "--")
            .unwrap_or(args.len());
        let mut command = Command::new("cargo");
        command
            .args(&args[..at])
            .arg("--locked")
            .arg("--target-dir")
            .arg(target)
            .args(&args[at..])
            .current_dir(worktree);
        command
    };
    let cargo = |args: &[&str]| -> Result<(bool, String, String)> {
        run(&mut command(args), false).with_context(|| format!("running cargo {}", args.join(" ")))
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
    // Run, since a moved test module can behave differently in its new file (a snapshot found
    // from `file!()` in a macro of another crate's): the unit tests, where the moved modules are.
    progress("cargo test --workspace --lib --bins");
    let unit_tests = |names: &[String]| -> Result<String> {
        let mut command = command(&[
            "test",
            "--workspace",
            "--lib",
            "--bins",
            "--no-fail-fast",
            "--",
        ]);
        command
            .args(names)
            // Snapshot tools fail rather than write or accept snapshots.
            .env("INSTA_UPDATE", "no")
            .env_remove("INSTA_FORCE_PASS")
            .env_remove("UPDATE_EXPECT")
            .env_remove("SNAPSHOTS");
        Ok(run(&mut command, true).context("running the unit tests")?.1)
    };
    let mut failed = failed_tests(&unit_tests(&[])?);
    if !failed.is_empty() {
        // Again, the failed ones alone: only those that fail twice count, as under load some
        // tests fail now and then.
        let mut names: Vec<String> = failed
            .iter()
            .filter_map(|test| test.split_once(' ').map(|(_, name)| name.to_owned()))
            .filter(|name| name != "did not finish")
            .collect();
        names.sort();
        names.dedup();
        names.insert(0, "--exact".to_owned());
        let again = failed_tests(&unit_tests(&names)?);
        failed.retain(|test| test.ends_with(" did not finish") || again.contains(test));
    }
    progress("cargo fmt --all --check");
    let mut fmt = Command::new("cargo");
    fmt.args(["fmt", "--all", "--check"]).current_dir(worktree);
    let formatted = run(&mut fmt, false).context("running cargo fmt")?.0;
    Ok(Evidence {
        release,
        inventory,
        formatted,
        failed,
    })
}

/// The tests that failed in `cargo test`'s output, stdout and stderr merged, each as its test
/// binary's file name and its name, and `<binary> did not finish` for each test binary that ran
/// without reaching its `test result` line.
fn failed_tests(output: &str) -> BTreeSet<String> {
    let mut failed = BTreeSet::new();
    let mut binary: Option<String> = None;
    let mut lines = output.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(path) = line
            .strip_prefix("     Running ")
            .and_then(|rest| rest.strip_suffix(')'))
            .and_then(|rest| rest.rsplit_once(" (").map(|(_, path)| path))
        {
            if let Some(unfinished) = binary.take() {
                failed.insert(format!("{unfinished} did not finish"));
            }
            let name = Path::new(path).file_name().unwrap_or_default();
            binary = Some(name.to_string_lossy().into_owned());
        } else if line.starts_with("test result: ") {
            binary = None;
        } else if let Some(name) = line
            .strip_prefix("test ")
            .and_then(|rest| rest.strip_suffix(" ... FAILED"))
        {
            failed.insert(format!("{} {name}", binary.as_deref().unwrap_or("?")));
        } else if line == "failures:"
            && lines
                .peek()
                .is_some_and(|next| next.starts_with("    ") && !next.trim().is_empty())
        {
            // The harness's closing list, which a test's own output cannot split.
            while let Some(next) = lines.next_if(|next| next.starts_with("    ")) {
                let name = next.trim();
                failed.insert(format!("{} {name}", binary.as_deref().unwrap_or("?")));
            }
        }
    }
    if let Some(unfinished) = binary {
        failed.insert(format!("{unfinished} did not finish"));
    }
    failed
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
    let broken: Vec<&str> = after
        .failed
        .difference(&before.failed)
        .map(String::as_str)
        .collect();
    if !broken.is_empty() {
        return Some(format!(
            "unit tests that passed before fail after: {}",
            broken.join(", ")
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

    /// A repository whose `src/lib.rs` declares `mod util;`, both with a tests module, planned.
    fn util_repo(s: &Sandbox) -> (PathBuf, Vec<Split>) {
        s.crate_repo("repo");
        s.write(
            "repo/src/lib.rs",
            &LIB.replacen("//! A library.\n", "//! A library.\n\nmod util;\n", 1),
        );
        s.write("repo/src/util.rs", LIB);
        let worktree = s.path("repo");
        let (splits, _) = plan(&worktree).unwrap();
        assert_eq!(splits.len(), 2);
        (worktree, splits)
    }

    #[test]
    fn apply_refuses_a_file_edited_since_the_plan_and_changes_nothing() {
        let s = Sandbox::new();
        let (worktree, splits) = util_repo(&s);
        let edited = format!("{LIB}// the user's edit\n");
        s.write("repo/src/util.rs", &edited);
        assert!(apply(&worktree, &splits).is_err());
        assert_eq!(
            fs::read_to_string(s.path("repo/src/util.rs")).unwrap(),
            edited
        );
        assert!(!s.path("repo/src/tests.rs").exists(), "nothing moved");
    }

    #[test]
    fn undo_refuses_a_file_edited_since_the_move() {
        let s = Sandbox::new();
        let (worktree, splits) = util_repo(&s);
        apply(&worktree, &splits).unwrap();
        let tests = s.path("repo/src/util/tests.rs");
        let edited = format!(
            "{}// the user's edit\n",
            fs::read_to_string(&tests).unwrap()
        );
        fs::write(&tests, &edited).unwrap();
        assert!(undo(&worktree, &splits).is_err());
        assert_eq!(fs::read_to_string(&tests).unwrap(), edited);
    }

    #[test]
    fn a_move_that_fails_part_way_is_undone() {
        let s = Sandbox::new();
        let (worktree, splits) = util_repo(&s);
        // `src/util/` cannot be made: the second move fails after the first.
        s.write("repo/src/util", "");
        assert!(apply(&worktree, &splits).is_err());
        assert!(!s.path("repo/src/tests.rs").exists());
        assert!(
            fs::read_to_string(s.path("repo/src/lib.rs"))
                .unwrap()
                .ends_with("    }\n}\n")
        );
    }

    fn same() -> Evidence {
        Evidence {
            release: BTreeMap::new(),
            inventory: Vec::new(),
            formatted: true,
            failed: BTreeSet::new(),
        }
    }

    #[test]
    fn an_error_after_the_move_undoes_it() {
        let s = Sandbox::new();
        let (worktree, splits) = util_repo(&s);
        let mut runs = 0;
        let mut evidence = || {
            runs += 1;
            if runs == 1 {
                Ok(same())
            } else {
                bail!("the build was killed")
            }
        };
        let err = transact(&worktree, &splits, &mut evidence).unwrap_err();
        assert!(format!("{err:#}").contains("now undone"), "{err:#}");
        assert_eq!(fs::read_to_string(s.path("repo/src/util.rs")).unwrap(), LIB);
        assert!(!s.path("repo/src/tests.rs").exists());
    }

    #[test]
    fn an_edit_during_verification_is_kept_and_the_move_not_called_verified() {
        let s = Sandbox::new();
        let (worktree, splits) = util_repo(&s);
        let tests = s.path("repo/src/util/tests.rs");
        let mut runs = 0;
        let mut evidence = || {
            runs += 1;
            if runs == 2 {
                let edited = format!("{}// the user's edit\n", fs::read_to_string(&tests)?);
                fs::write(&tests, edited)?;
            }
            Ok(same())
        };
        let err = transact(&worktree, &splits, &mut evidence).unwrap_err();
        assert!(format!("{err:#}").contains("not verified"), "{err:#}");
        assert!(
            fs::read_to_string(&tests)
                .unwrap()
                .ends_with("// the user's edit\n")
        );
    }

    /// Two crate roots in one directory would both write its `tests.rs`, and one in `src/bin`
    /// would add a program `tests`.
    #[test]
    fn refuses_two_moves_to_one_file_and_a_new_program() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        s.write("repo/src/lib.rs", LIB);
        s.write(
            "repo/src/main.rs",
            &LIB.replacen("//! A library.\n", "fn main() {}\n", 1),
        );
        s.write(
            "repo/src/bin/extra.rs",
            &LIB.replacen("//! A library.\n", "fn main() {}\n", 1),
        );
        let (splits, refusals) = plan(&s.path("repo")).unwrap();
        assert!(splits.is_empty(), "{splits:?}");
        assert_eq!(refusals.len(), 3, "{refusals:?}");
    }

    /// A snapshot tool reached through another crate counts, as does a test whose outcome
    /// depends on where its file is: the tests run before and after.
    #[test]
    fn snapshot_tools_count_through_other_crates() {
        let s = Sandbox::new();
        s.write(
            "repo/Cargo.toml",
            "[workspace]\nmembers = [\"app\", \"helper\", \"insta\"]\nresolver = \"3\"\n",
        );
        let package = |name: &str, deps: &str| {
            s.write(
                &format!("repo/{name}/Cargo.toml"),
                &format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                     [dev-dependencies]\n{deps}\n"
                ),
            );
        };
        package("insta", "");
        s.write("repo/insta/src/lib.rs", "");
        s.write("repo/helper/Cargo.toml", "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ninsta = { path = \"../insta\" }\n");
        s.write("repo/helper/src/lib.rs", "");
        package("app", "helper = { path = \"../helper\" }");
        s.write(
            "repo/app/src/lib.rs",
            &LIB.replacen("//! A library.\n", "//! A library.\n\nmod util;\n", 1),
        );
        s.write("repo/app/src/util.rs", LIB);
        s.run(
            "repo",
            "cargo",
            &["generate-lockfile", "--quiet", "--offline"],
        );
        let (splits, refusals) = plan(&s.path("repo")).unwrap();
        assert_eq!(splits.len(), 1, "{splits:?}");
        assert!(refusals[0].why.starts_with("insta"), "{refusals:?}");
    }

    #[test]
    fn a_move_that_changes_what_a_test_does_is_undone() {
        let s = Sandbox::new();
        s.write(
            "repo/Cargo.toml",
            "[workspace]\nmembers = [\"app\", \"here\"]\nresolver = \"3\"\n",
        );
        s.write(
            "repo/here/Cargo.toml",
            "[package]\nname = \"here\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        // Where the test that calls it is, as insta's macros find it.
        s.write(
            "repo/here/src/lib.rs",
            "#[macro_export]\nmacro_rules! here {\n    () => {\n        file!()\n    };\n}\n",
        );
        s.write(
            "repo/app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
             [dev-dependencies]\nhere = { path = \"../here\" }\n",
        );
        s.write("repo/app/src/lib.rs", "mod util;\n");
        s.write(
            "repo/app/src/util.rs",
            "pub fn one() -> u32 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    \
             fn beside_util() {\n        assert!(here::here!().ends_with(\"util.rs\"));\n    }\n}\n",
        );
        s.write("repo/.gitignore", "/target/\n");
        s.run(
            "repo",
            "cargo",
            &["generate-lockfile", "--quiet", "--offline"],
        );
        let worktree = s.path("repo");
        let code = crate::split_tests(&worktree, &s.path("split-target")).unwrap();
        assert_eq!(code, std::process::ExitCode::from(1), "undone");
        assert!(!s.path("repo/app/src/util/tests.rs").exists());
    }

    #[test]
    fn a_build_script_s_modules_and_other_walkers_count() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        s.write("repo/src/lib.rs", LIB);
        s.write(
            "repo/build.rs",
            "mod helper;\n\nfn main() {\n    println!(\"cargo::rerun-if-changed=build.rs\");\n    helper::run();\n}\n",
        );
        s.write(
            "repo/helper.rs",
            "pub fn run() {\n    for _ in std::fs::read_dir(\"src\").unwrap() {}\n}\n",
        );
        let (splits, refusals) = plan(&s.path("repo")).unwrap();
        assert!(splits.is_empty(), "{refusals:?}");
        s.write(
            "repo/helper.rs",
            "pub fn run() {\n    let _ = ignore::WalkBuilder::new(\"src\");\n}\n",
        );
        let (splits, refusals) = plan(&s.path("repo")).unwrap();
        assert!(splits.is_empty(), "{refusals:?}");
    }

    #[test]
    fn paths_and_position_are_found_however_they_are_written() {
        for body in [
            "#[cfg_attr(unix, path = \"x.rs\")] mod x;",
            "# [path = \"x.rs\"]\n    mod x;",
            "pub(crate) mod x;",
            "const F: &str = file !();",
            "const S: &str = include_str! (\"x.txt\");",
        ] {
            let text = LIB.replace("use super::*;", &format!("use super::*;\n    {body}"));
            let why = split_file(
                Path::new("/w/src/lib.rs"),
                true,
                &text,
                &parsed(&text),
                None,
            );
            assert!(why.is_err(), "{body}: {why:?}");
        }
    }

    #[test]
    fn a_comparison_is_not_a_positional_macro() {
        let text = LIB.replace(
            "use super::*;",
            "use super::*;\n    const L: bool = 1 != 2;\n    fn f(line: u32) -> bool {\n        line != 3\n    }",
        );
        let split = split_file(
            Path::new("/w/src/lib.rs"),
            true,
            &text,
            &parsed(&text),
            None,
        );
        assert!(split.unwrap().is_some());
    }

    #[test]
    fn evidence_is_locked_and_builds_apart() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let worktree = s.path("repo");
        let target = s.path("split-target");
        let evidence_of = || evidence(&worktree, &target, &mut |_| {});
        evidence_of().unwrap();
        assert!(target.join("release").is_dir());
        assert!(
            !worktree.join("target").exists(),
            "built in its own directory"
        );
        fs::remove_file(worktree.join("Cargo.lock")).unwrap();
        assert!(evidence_of().is_err(), "no lock file");
        assert!(!worktree.join("Cargo.lock").exists(), "none written");
    }
}
