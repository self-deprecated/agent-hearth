//! A settle reuses main's results for test binaries the change cannot have reached: the golden's
//! settle ran every test binary of main, and a worktree's build directory is a clone of the
//! golden's, so a test binary cargo did not rebuild is byte for byte the one main's results came
//! from.
//!
//! The rule: when every file that differs from the golden's tree is a Rust source named in some
//! unit's dep-info (not a build script's), each test binary whose bytes, and the bytes of the
//! programs and shared libraries it may run, equal the golden's keeps the golden's result,
//! main's failures included. Anything else that changed (fixtures, snapshots, trybuild cases,
//! `Cargo.toml`, configuration) runs the full suite. Doctests run again, all of them, when a
//! package's library or a path dev-dependency's differs, or its sources changed. Binaries come
//! from cargo's JSON messages, never from paths inside the build directory.
//!
//! What runs is still `cargo test --workspace`, so every unit is built as the golden built it:
//! selecting packages with `-p` resolves their features apart and rebuilt half of ruff. A runner
//! (`target.'cfg(all())'.runner`) skips the binaries whose results are main's.
//!
//! ```text
//! <state>/goldens/<id>/reuse.json   per test binary and package: its identity and main's result
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// In a golden's directory: what its settle found, for settles of worktrees seeded from it.
pub const MAIN: &str = "reuse.json";

/// Set to `off` to run the full suite every time, as before reuse (the oracle in
/// `bench/settle-reuse.sh` compares the two).
pub const SWITCH: &str = "AGENT_HEARTH_REUSE";

pub fn enabled() -> bool {
    std::env::var(SWITCH).map_or(true, |value| value != "off")
}

/// What one test binary, or one package's doctests, did in a run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub tests: u64,
    /// Tests that failed: each one's name and its line in a verdict.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Recorded {
    identity: String,
    /// For doctests: the library's crate name, as `Doc-tests` lines give it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    name: String,
    #[serde(flatten)]
    outcome: Outcome,
}

/// What the golden's settle learned about main.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Main {
    /// Every file of main's tree and a hash of its content, as `check::Snapshot` hashes them.
    files: BTreeMap<String, String>,
    /// Rust sources named in the dep-info of path packages' units, relative to the tree's root.
    sources: BTreeSet<String>,
    /// Those of build scripts: a change there runs the full suite.
    build_scripts: BTreeSet<String>,
    /// By test binary key ([`Unit::key`]).
    binaries: BTreeMap<String, Recorded>,
    /// By package key (its manifest relative to the root).
    doctests: BTreeMap<String, Recorded>,
    /// The artifacts identities were made of, by path relative to the build directory.
    artifacts: BTreeMap<String, Artifact>,
}

/// A build artifact as the golden had it. A worktree's build directory is a clone of the golden's,
/// mtimes included, and cargo rewrites only what it rebuilds: a file of the same size and mtime is
/// the golden's, and one of another size is not, both without reading it (atuin's test binaries
/// are gigabytes, and hashing them all took 251s on sd-tay). Only a rebuilt file of the same size
/// is hashed, as it may have come out the same.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Artifact {
    size: u64,
    /// Nanoseconds since the epoch.
    modified: u64,
    hash: String,
}

/// Where a build ran and how its paths read.
pub struct Paths<'a> {
    /// The root of the source tree as cargo's messages name it.
    pub root: &'a Path,
    /// The directory rustc ran in, which relative paths in dep-info start from, and which
    /// absolute ones there start with.
    pub dep_info_root: &'a Path,
    /// Path prefixes in messages and their real paths, where they differ (a golden's build at
    /// the pin).
    pub map: &'a [(PathBuf, PathBuf)],
    /// The build directory, real path.
    pub target: &'a Path,
}

impl Paths<'_> {
    fn real(&self, path: &Path) -> PathBuf {
        for (from, to) in self.map {
            if let Ok(rest) = path.strip_prefix(from) {
                return to.join(rest);
            }
        }
        path.to_path_buf()
    }

    fn relative(&self, path: &Path) -> String {
        match path.strip_prefix(self.root) {
            Ok(rest) => rest.to_string_lossy().into_owned(),
            Err(_) => path.to_string_lossy().into_owned(),
        }
    }
}

/// A unit of a path package that cargo built, from its `compiler-artifact` message.
#[derive(Debug, Clone)]
struct Unit {
    /// The package's manifest, relative to the root.
    package: String,
    kind: Vec<String>,
    name: String,
    test: bool,
    executable: Option<PathBuf>,
    /// Real paths.
    filenames: Vec<PathBuf>,
}

impl Unit {
    /// Names a test binary across checkouts: its package and target.
    fn key(&self) -> String {
        format!("{} {} {}", self.package, self.kind.join(","), self.name)
    }

    fn is(&self, kind: &str) -> bool {
        self.kind.iter().any(|k| k == kind)
    }

    /// Integration tests, benches and examples: they run the package's programs, and nushell's
    /// run other packages' too.
    fn integration(&self) -> bool {
        ["test", "bench", "example"]
            .iter()
            .any(|kind| self.is(kind))
    }
}

/// A build's units, from `cargo … --message-format=json`.
pub struct Build {
    units: Vec<Unit>,
}

impl Build {
    pub fn parse(stdout: &str, paths: &Paths) -> Self {
        let mut units = Vec::new();
        for line in stdout.lines() {
            let Ok(message) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if message["reason"] != "compiler-artifact" {
                continue;
            }
            let package_id = message["package_id"].as_str().unwrap_or_default();
            if !package_id.contains("path+file://") {
                continue;
            }
            let strings = |value: &Value| -> Vec<String> {
                value
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            };
            let manifest = Path::new(message["manifest_path"].as_str().unwrap_or_default());
            units.push(Unit {
                package: paths.relative(manifest),
                kind: strings(&message["target"]["kind"]),
                name: message["target"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                test: message["profile"]["test"] == true,
                executable: message["executable"]
                    .as_str()
                    .map(|path| paths.real(Path::new(path))),
                filenames: strings(&message["filenames"])
                    .iter()
                    .map(|path| paths.real(Path::new(path)))
                    .collect(),
            });
        }
        Self { units }
    }

    /// Test binaries cargo runs: units built as tests, with an executable.
    fn tests(&self) -> impl Iterator<Item = &Unit> {
        self.units
            .iter()
            .filter(|unit| unit.test && unit.executable.is_some())
    }

    /// The executable file names of test binaries, which cargo's `Running` lines end in, and
    /// their keys.
    pub fn test_files(&self) -> BTreeMap<String, String> {
        self.tests()
            .filter_map(|unit| {
                let name = unit.executable.as_ref()?.file_name()?.to_string_lossy();
                Some((name.into_owned(), unit.key()))
            })
            .collect()
    }

    /// Library crate names (as `Doc-tests` lines give them) and their packages.
    pub fn crates(&self) -> BTreeMap<String, String> {
        self.units
            .iter()
            .filter(|unit| !unit.test && is_library(unit))
            .map(|unit| (unit.name.replace('-', "_"), unit.package.clone()))
            .collect()
    }
}

fn is_library(unit: &Unit) -> bool {
    ["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"]
        .iter()
        .any(|kind| unit.is(kind))
}

/// What a build's artifacts are, hashed: each test binary's identity, each package's doctests'
/// identity, and the sources rustc read.
struct Identities {
    /// By test binary key: its identity.
    binaries: BTreeMap<String, String>,
    /// By package key: doctests' identity, when its library was built.
    doctests: BTreeMap<String, String>,
    /// By package key, its library's sources.
    lib_sources: BTreeMap<String, BTreeSet<String>>,
    sources: BTreeSet<String>,
    build_scripts: BTreeSet<String>,
    /// What each artifact was, to record for the golden.
    artifacts: BTreeMap<String, Artifact>,
}

impl Identities {
    /// `known`: the golden's artifacts, which a worktree's are compared with; empty for the
    /// golden's own.
    fn of(
        build: &Build,
        paths: &Paths,
        metadata: &Metadata,
        known: &BTreeMap<String, Artifact>,
    ) -> Result<Self> {
        // Every artifact whose bytes count, hashed once.
        let mut files: BTreeSet<PathBuf> = BTreeSet::new();
        // Programs and shared libraries a test may run, by package.
        let mut runnables: BTreeMap<&str, Vec<&Path>> = BTreeMap::new();
        let mut libraries: BTreeMap<&str, Vec<&Path>> = BTreeMap::new();
        for unit in &build.units {
            if unit.is("custom-build") {
                continue;
            }
            if unit.test {
                if let Some(exe) = &unit.executable {
                    files.insert(exe.clone());
                }
                continue;
            }
            if is_library(unit) {
                // The rlib, or for a proc macro or shared library, the shared object.
                let artifact = unit
                    .filenames
                    .iter()
                    .find(|path| path.extension().is_some_and(|ext| ext == "rlib"))
                    .or_else(|| unit.filenames.iter().find(|path| shared_library(path)));
                if let Some(artifact) = artifact {
                    files.insert(artifact.clone());
                    libraries.entry(&unit.package).or_default().push(artifact);
                }
                if !unit.is("proc-macro") {
                    for shared in unit.filenames.iter().filter(|path| shared_library(path)) {
                        files.insert(shared.clone());
                        runnables.entry(&unit.package).or_default().push(shared);
                    }
                }
            }
            if let Some(exe) = &unit.executable {
                files.insert(exe.clone());
                runnables.entry(&unit.package).or_default().push(exe);
            }
        }
        let files: Vec<PathBuf> = files.into_iter().collect();
        let (found, artifacts) = examine(&files, paths.target, known)?;
        let hashes: BTreeMap<&Path, String> =
            files.iter().map(PathBuf::as_path).zip(found).collect();
        let hashes_of = |paths: &[&Path]| -> Vec<String> {
            let mut all: Vec<String> = paths.iter().map(|path| hashes[path].clone()).collect();
            all.sort_unstable();
            all
        };
        let all_runnables: Vec<&Path> = runnables.values().flatten().copied().collect();
        let mut identities = Identities {
            binaries: BTreeMap::new(),
            doctests: BTreeMap::new(),
            lib_sources: BTreeMap::new(),
            sources: BTreeSet::new(),
            build_scripts: BTreeSet::new(),
            artifacts,
        };
        for unit in build.tests() {
            let exe = unit
                .executable
                .as_deref()
                .context("a test without an executable")?;
            let own = runnables
                .get(unit.package.as_str())
                .map_or(&[][..], Vec::as_slice);
            let runs = if unit.integration() {
                &all_runnables[..]
            } else {
                own
            };
            let identity = identity(&[hashes[exe].clone()], &hashes_of(runs));
            identities.binaries.insert(unit.key(), identity);
        }
        for (package, artifacts) in &libraries {
            let mut deps = Vec::new();
            let mut known = true;
            for dependency in metadata.of(package) {
                match libraries.get(dependency.as_str()) {
                    Some(theirs) => deps.extend(hashes_of(theirs)),
                    // A dev-dependency whose library is not in this build: unknown.
                    None => known = false,
                }
            }
            if !known {
                continue;
            }
            deps.sort_unstable();
            let identity = identity(&hashes_of(artifacts), &deps);
            identities.doctests.insert((*package).to_owned(), identity);
        }
        for unit in &build.units {
            // Without dep-info (cargo writes none beside an uplifted program), its sources count
            // as not compiled: a change to one that no other unit names runs the whole suite.
            let read = dep_info_sources(unit, paths, &metadata.workspace).unwrap_or_default();
            if unit.is("custom-build") {
                identities.build_scripts.extend(read);
                continue;
            }
            if !unit.test && is_library(unit) {
                identities
                    .lib_sources
                    .entry(unit.package.clone())
                    .or_default()
                    .extend(read.iter().cloned());
            }
            identities.sources.extend(read);
        }
        Ok(identities)
    }
}

fn shared_library(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext == "so" || ext == "dylib" || ext == "dll")
}

fn identity(own: &[String], runs: &[String]) -> String {
    let mut hasher = blake3::Hasher::new();
    for hash in own {
        hasher.update(hash.as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(b"runs\n");
    for hash in runs {
        hasher.update(hash.as_bytes());
        hasher.update(b"\n");
    }
    hasher.finalize().to_hex()[..32].to_owned()
}

/// What each of `files` is, for an identity: the golden's hash when it is the golden's file
/// (same size and mtime, `known`), something no hash equals when its size differs, and otherwise
/// a blake3 of its content, on every core. Also what each file was, by path relative to `target`.
#[allow(clippy::type_complexity)]
fn examine(
    files: &[PathBuf],
    target: &Path,
    known: &BTreeMap<String, Artifact>,
) -> Result<(Vec<String>, BTreeMap<String, Artifact>)> {
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut found: Vec<Option<(String, Artifact)>> = vec![None; files.len()];
    let look = |path: &Path| -> Result<(String, Artifact)> {
        let metadata = fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
        let modified = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
            });
        let key = path
            .strip_prefix(target)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        let size = metadata.len();
        let hash = match known.get(&key) {
            Some(theirs) if theirs.size == size && theirs.modified == modified => {
                theirs.hash.clone()
            }
            Some(theirs) if theirs.size != size => format!("size {size}"),
            None if !known.is_empty() => format!("new, size {size}"),
            _ => {
                let mut hasher = blake3::Hasher::new();
                let mut file =
                    File::open(path).with_context(|| format!("reading {}", path.display()))?;
                std::io::copy(&mut file, &mut hasher)
                    .with_context(|| format!("reading {}", path.display()))?;
                hasher.finalize().to_hex().to_string()
            }
        };
        let artifact = Artifact {
            size,
            modified,
            hash: hash.clone(),
        };
        Ok((key, artifact))
    };
    let results: Vec<Result<Vec<(usize, (String, Artifact))>>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads.min(files.len().max(1)))
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let at = next.fetch_add(1, Ordering::Relaxed);
                        let Some(path) = files.get(at) else {
                            return Ok(done);
                        };
                        done.push((at, look(path)?));
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().expect("examining artifacts panicked"))
            .collect()
    });
    for result in results {
        for (at, examined) in result? {
            found[at] = Some(examined);
        }
    }
    let mut hashes = Vec::with_capacity(files.len());
    let mut artifacts = BTreeMap::new();
    for examined in found {
        let (key, artifact) = examined.context("an artifact was not examined")?;
        hashes.push(artifact.hash.clone());
        artifacts.insert(key, artifact);
    }
    Ok((hashes, artifacts))
}

/// The files under the root that rustc read for `unit`, from the dep-info it wrote beside the
/// unit's artifacts (`<dir>/<crate>-<hash>.d` for `<dir>/lib<crate>-<hash>.rlib` and the like;
/// cargo's own beside an uplifted program; for a build script, which cargo names
/// `build-script-build`, rustc's `build_script_build-<hash>.d` beside it).
fn dep_info_sources(unit: &Unit, paths: &Paths, workspace: &Path) -> Option<BTreeSet<String>> {
    let mut found = false;
    let mut sources = BTreeSet::new();
    for artifact in &unit.filenames {
        let Some(name) = artifact.file_name().map(|name| name.to_string_lossy()) else {
            continue;
        };
        let stem = match artifact.extension() {
            Some(ext) if name.len() > ext.len() + 1 => &name[..name.len() - ext.len() - 1],
            _ => &name[..],
        };
        let named = [Some(stem), stem.strip_prefix("lib")]
            .into_iter()
            .flatten()
            .map(|stem| artifact.with_file_name(format!("{stem}.d")));
        // rustc's own, `<crate name>-<hash>.d`, in the same directory.
        let crate_name = format!("{}-", stem.replace('-', "_"));
        let beside = artifact.parent().and_then(|dir| fs::read_dir(dir).ok());
        let rustc_s = beside.into_iter().flatten().filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_string_lossy();
            (name.starts_with(&crate_name) && name.ends_with(".d")).then_some(path)
        });
        for path in named.chain(rustc_s) {
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            found = true;
            for dependency in dep_info_paths(&text) {
                let dependency = Path::new(&dependency);
                let relative = if dependency.is_absolute() {
                    match dependency.strip_prefix(paths.dep_info_root) {
                        Ok(rest) => rest.to_path_buf(),
                        Err(_) => continue,
                    }
                } else {
                    workspace.join(dependency)
                };
                sources.insert(relative.to_string_lossy().into_owned());
            }
            break;
        }
    }
    found.then_some(sources)
}

/// The prerequisites in a Makefile-style dep-info file: rustc's (one rule per output, then an
/// empty rule per source) or cargo's (one rule).
fn dep_info_paths(text: &str) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        // The rule's target ends at the first unescaped `: ` (or `:` at the end of the line).
        let bytes = line.as_bytes();
        let mut split = None;
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 1,
                b':' if bytes.get(i + 1).is_none_or(|next| *next == b' ') => {
                    split = Some(i);
                    break;
                }
                _ => {}
            }
            i += 1;
        }
        let Some(split) = split else { continue };
        let mut word = String::new();
        let mut chars = line[split + 1..].chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(next) = chars.next() {
                        word.push(next);
                    }
                }
                ' ' => {
                    if !word.is_empty() {
                        paths.insert(std::mem::take(&mut word));
                    }
                }
                _ => word.push(c),
            }
        }
        if !word.is_empty() {
            paths.insert(word);
        }
        // An empty rule names a source as its target.
        if line[split + 1..].trim().is_empty() {
            paths.insert(unescape(&line[..split]));
        }
    }
    paths
}

fn unescape(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(next) = chars.next()
        {
            out.push(next);
        } else {
            out.push(c);
        }
    }
    out
}

/// From `cargo metadata`: each package's path dev-dependencies, as package keys (their libraries
/// are linked into its doctests), and the workspace's root relative to the tree's (rustc runs
/// there, and its dep-info names workspace members' sources relative to it).
#[derive(Default)]
pub struct Metadata {
    dev: BTreeMap<String, Vec<String>>,
    workspace: PathBuf,
}

impl Metadata {
    /// From `cargo metadata --no-deps` in `root`.
    pub fn of_tree(root: &Path) -> Result<Self> {
        let output = Command::new("cargo")
            .args([
                "metadata",
                "--no-deps",
                "--format-version",
                "1",
                "--offline",
            ])
            .current_dir(root)
            .output()
            .context("running cargo metadata")?;
        if !output.status.success() {
            bail!(
                "cargo metadata failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let metadata: Value =
            serde_json::from_slice(&output.stdout).context("parsing cargo metadata")?;
        let key = |manifest: &Path| match manifest.strip_prefix(root) {
            Ok(rest) => rest.to_string_lossy().into_owned(),
            Err(_) => manifest.to_string_lossy().into_owned(),
        };
        let mut map = BTreeMap::new();
        for package in metadata["packages"].as_array().into_iter().flatten() {
            let manifest = Path::new(package["manifest_path"].as_str().unwrap_or_default());
            let dev: Vec<String> = package["dependencies"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|dep| dep["kind"] == "dev")
                .filter_map(|dep| dep["path"].as_str())
                .map(|dir| key(&Path::new(dir).join("Cargo.toml")))
                .collect();
            map.insert(key(manifest), dev);
        }
        let workspace = Path::new(metadata["workspace_root"].as_str().unwrap_or_default());
        Ok(Self {
            dev: map,
            workspace: workspace
                .strip_prefix(root)
                .unwrap_or(Path::new(""))
                .to_path_buf(),
        })
    }

    fn of(&self, package: &str) -> &[String] {
        self.dev.get(package).map_or(&[], Vec::as_slice)
    }
}

/// Records what the golden's settle found: the run's output (`cargo test --workspace
/// --message-format=json`, stdout and stderr merged) parsed into `units`, and its tree's files.
pub fn record(
    build: &Build,
    paths: &Paths,
    units: &BTreeMap<String, Outcome>,
    files: BTreeMap<String, String>,
    metadata: &Metadata,
) -> Result<Main> {
    let identities = Identities::of(build, paths, metadata, &BTreeMap::new())?;
    let mut main = Main {
        files,
        sources: identities.sources,
        build_scripts: identities.build_scripts,
        binaries: BTreeMap::new(),
        doctests: BTreeMap::new(),
        artifacts: identities.artifacts,
    };
    let tests = build.test_files();
    let crates = build.crates();
    for (unit, outcome) in units {
        if let Some(file) = unit.strip_prefix("bin ") {
            let Some(key) = tests.get(file) else { continue };
            let Some(identity) = identities.binaries.get(key) else {
                continue;
            };
            main.binaries.insert(
                key.clone(),
                Recorded {
                    identity: identity.clone(),
                    name: String::new(),
                    outcome: outcome.clone(),
                },
            );
        } else if let Some(name) = unit.strip_prefix("doc ") {
            let Some(package) = crates.get(name) else {
                continue;
            };
            let Some(identity) = identities.doctests.get(package) else {
                continue;
            };
            main.doctests.insert(
                package.clone(),
                Recorded {
                    identity: identity.clone(),
                    name: name.to_owned(),
                    outcome: outcome.clone(),
                },
            );
        }
    }
    Ok(main)
}

pub fn load(dir: &Path) -> Option<Main> {
    serde_json::from_slice(&fs::read(dir.join(MAIN)).ok()?).ok()
}

/// What a settle runs and what it takes from main.
#[derive(Debug)]
pub struct Plan {
    /// The test binaries (file names) whose results are main's, which the run skips.
    pub skip: Vec<String>,
    /// Whether any test binary must run.
    pub run: bool,
    /// Whether some library's doctests must run: then they all do, as `cargo test` picks them.
    pub doctests: bool,
    /// Main's results, by test binary file name (`bin <name>`) or library crate name
    /// (`doc <name>`), as a run's output names them, for those that do not run; any that runs
    /// after all counts from its own run.
    pub reused: BTreeMap<String, Outcome>,
}

/// Why a settle runs the full suite rather than reusing main's results.
#[derive(Debug, PartialEq, Eq)]
pub enum Full {
    /// The golden's settle recorded nothing to reuse (it ran before this existed, or not yet).
    NoRecord,
    /// A changed file that is not a compiled Rust source (or is a build script's): the path.
    Changed(String),
    /// The worktree's test build or its artifacts could not be read.
    Build(String),
}

/// Plans a settle of the worktree's tree `files` (as `Snapshot` hashes them) from `build`, the
/// worktree's test build, against `main`, what the golden's settle recorded.
pub fn plan(
    main: Option<&Main>,
    files: &BTreeMap<String, String>,
    build: &Build,
    paths: &Paths,
    metadata: &Metadata,
) -> std::result::Result<Plan, Full> {
    let main = main.ok_or(Full::NoRecord)?;
    let identities = Identities::of(build, paths, metadata, &main.artifacts)
        .map_err(|err| Full::Build(format!("{err:#}")))?;
    let changed: BTreeSet<&String> = files
        .iter()
        .filter(|(path, hash)| main.files.get(*path) != Some(hash))
        .map(|(path, _)| path)
        .chain(main.files.keys().filter(|path| !files.contains_key(*path)))
        .collect();
    for path in &changed {
        let compiled = main.sources.contains(*path) || identities.sources.contains(*path);
        let build_script =
            main.build_scripts.contains(*path) || identities.build_scripts.contains(*path);
        if !path.ends_with(".rs") || !compiled || build_script {
            return Err(Full::Changed((*path).clone()));
        }
    }
    let mut plan = Plan {
        skip: Vec::new(),
        run: false,
        doctests: false,
        reused: BTreeMap::new(),
    };
    let files_by_key: BTreeMap<String, String> = build
        .test_files()
        .into_iter()
        .map(|(file, key)| (key, file))
        .collect();
    for (key, identity) in &identities.binaries {
        let file = &files_by_key[key];
        match main.binaries.get(key) {
            Some(recorded) if recorded.identity == *identity => {
                plan.skip.push(file.clone());
                plan.reused
                    .insert(format!("bin {file}"), recorded.outcome.clone());
            }
            _ => plan.run = true,
        }
    }
    // Doctests: main ran those of every library that has them. A package whose library or a
    // path dev-dependency's differs, or whose library's sources changed, runs them again.
    for (package, recorded) in &main.doctests {
        let owns_change = identities
            .lib_sources
            .get(package)
            .is_some_and(|sources| changed.iter().any(|path| sources.contains(*path)));
        if !owns_change && identities.doctests.get(package) == Some(&recorded.identity) {
            plan.reused
                .insert(format!("doc {}", recorded.name), recorded.outcome.clone());
        } else {
            plan.doctests = true;
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_rustc_s_and_cargo_s_dep_info() {
        let rustc = "\
/t/debug/deps/foo-1.d: src/lib.rs src/a\\ b.rs

/t/debug/deps/libfoo-1.rlib: src/lib.rs src/a\\ b.rs

src/lib.rs:
src/a\\ b.rs:

# env-dep:CARGO_PKG_NAME=foo
";
        assert_eq!(
            dep_info_paths(rustc),
            BTreeSet::from(["src/lib.rs".to_owned(), "src/a b.rs".to_owned()])
        );
        let cargo = "/t/debug/foo: /w/src/main.rs /w/src/cli.rs\n";
        assert_eq!(
            dep_info_paths(cargo),
            BTreeSet::from(["/w/src/main.rs".to_owned(), "/w/src/cli.rs".to_owned()])
        );
    }

    #[test]
    fn integration_tests_benches_and_examples_may_run_any_program() {
        let unit = |kind: &str| Unit {
            package: "a/Cargo.toml".to_owned(),
            kind: vec![kind.to_owned()],
            name: "x".to_owned(),
            test: true,
            executable: None,
            filenames: Vec::new(),
        };
        assert!(unit("test").integration() && unit("bench").integration());
        assert!(!unit("bin").integration() && !unit("lib").integration());
    }
}
