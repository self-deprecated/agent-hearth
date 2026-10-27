//! Goldens: warm builds of a revision, made at the fixed path, that new worktrees are seeded from.
//!
//! ```text
//! <state>/goldens/current        id of the golden new worktrees seed from
//! <state>/goldens/refresh.lock   held for the whole of a refresh
//! <state>/goldens/refresh.log    the last background refresh's output
//! <state>/goldens/<id>/src       the revision's tree, never edited
//! <state>/goldens/<id>/target    its build directory
//! <state>/goldens/<id>/golden.json   written last: a golden without it is unfinished
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::{env, io};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::adopt;
use crate::builds;
use crate::cargo;
use crate::check;
use crate::locate::{Location, Vcs};

/// Environment variables that change rustc's flags, and with them every fingerprint.
const FLAG_VARS: [&str; 3] = [
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_BUILD_RUSTFLAGS",
];

/// The builds that make a golden warm for an agent's turn.
/// `--keep-going` (`--no-fail-fast` for `test`): a main with errors in some targets under this
/// toolchain (benches that need nightly) still gets the rest built.
const BUILDS: [&[&str]; 2] = [
    &[
        "clippy",
        "--workspace",
        "--all-targets",
        "--keep-going",
        "--quiet",
    ],
    &[
        "test",
        "--workspace",
        "--no-run",
        "--no-fail-fast",
        "--quiet",
    ],
];

const METADATA: &str = "golden.json";
/// What main's code is like under this toolchain, for the gate (`check::Baseline`).
const BASELINE: &str = "baseline.json";
/// A golden seeded from the current one adds its stale artifacts (old versions of changed
/// dependencies) to the build directory; after this many in a row, the next is built fresh.
const GENERATIONS: u32 = 10;

/// What a golden was built from. The id hashes all of it, so a golden is reused only when every
/// input matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Golden {
    pub id: String,
    pub commit: String,
    #[serde(flatten)]
    pub inputs: Inputs,
    /// How many goldens, each seeded from the one before, it descends from: 0 when built fresh.
    #[serde(default)]
    pub generation: u32,
}

/// Everything besides the source that a build directory's contents depend on. A worktree seeded
/// from a golden with other inputs rebuilds more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inputs {
    /// blake3 of `Cargo.lock`, or `None` when there is none.
    pub cargo_lock: Option<String>,
    /// `rustc -vV`, run in the source tree so toolchain files apply.
    pub toolchain: String,
    pub flags: BTreeMap<String, String>,
}

impl Inputs {
    /// The inputs a build of `src` would have in this environment.
    pub fn of(src: &Path) -> Result<Self> {
        let cargo_lock = match fs::read(src.join("Cargo.lock")) {
            Ok(lock) => Some(blake3::hash(&lock).to_hex().to_string()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return Err(err).context("reading Cargo.lock"),
        };
        Ok(Self {
            cargo_lock,
            toolchain: capture(Command::new("rustc").arg("-vV").current_dir(src))?,
            flags: FLAG_VARS
                .iter()
                .filter_map(|name| Some((name.to_string(), env::var(name).ok()?)))
                .collect(),
        })
    }
}

/// Held for the whole of a refresh, and by `gc` while it deletes goldens.
pub fn refresh_lock(location: &Location) -> Result<std::fs::File> {
    let goldens = location.state_dir.join("goldens");
    fs::create_dir_all(&goldens).with_context(|| format!("creating {}", goldens.display()))?;
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(goldens.join("refresh.lock"))
        .context("opening the refresh lock")
}

/// Held shared by every adopt while it reads a golden, and exclusively by `gc` while it deletes
/// goldens.
pub fn gc_lock(location: &Location) -> Result<std::fs::File> {
    let goldens = location.state_dir.join("goldens");
    fs::create_dir_all(&goldens).with_context(|| format!("creating {}", goldens.display()))?;
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(goldens.join("gc.lock"))
        .context("opening the gc lock")
}

/// The directory of the golden `id`.
pub fn dir(location: &Location, id: &str) -> PathBuf {
    location.state_dir.join("goldens").join(id)
}

/// The baseline of the golden `id`, if it recorded one (goldens built before it did not).
pub fn baseline(location: &Location, id: &str) -> Option<check::Baseline> {
    let json = fs::read(dir(location, id).join(BASELINE)).ok()?;
    serde_json::from_slice(&json).ok()
}

/// The golden new worktrees seed from, if one has been built.
pub fn current(location: &Location) -> Result<Option<Golden>> {
    let pointer = location.state_dir.join("goldens").join("current");
    let id = match fs::read_to_string(&pointer) {
        Ok(id) => id,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("reading {}", pointer.display())),
    };
    let metadata = dir(location, id.trim()).join(METADATA);
    let json = fs::read(&metadata).with_context(|| format!("reading {}", metadata.display()))?;
    Ok(Some(serde_json::from_slice(&json).with_context(|| {
        format!("reading {}", metadata.display())
    })?))
}

/// Every golden whose build finished.
pub fn finished(location: &Location) -> Result<Vec<Golden>> {
    let goldens = location.state_dir.join("goldens");
    let entries = match fs::read_dir(&goldens) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", goldens.display())),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let metadata = entry.path().join(METADATA);
        match fs::read(&metadata) {
            Ok(json) => found.push(
                serde_json::from_slice(&json)
                    .with_context(|| format!("reading {}", metadata.display()))?,
            ),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("reading {}", metadata.display())),
        }
    }
    Ok(found)
}

/// Starts a refresh of the golden in the background when main has moved since the current one was
/// built and no refresh runs: seeded from the current golden, it rebuilds only what changed (37s
/// after an ordinary atuin commit, `bench/golden-atuin-linux2.txt`), so worktrees spawned after
/// it start from the new main. Never the first golden, which `setup` builds. Returns the
/// refresh's log when it started one.
pub fn refresh_if_moved(location: &Location, agent_hearth: &Path) -> Result<Option<PathBuf>> {
    let Some(golden) = current(location)? else {
        return Ok(None);
    };
    // No main to follow, or main where the golden is: nothing to do.
    if resolve(location, "main").map_or(true, |main| main == golden.commit) {
        return Ok(None);
    }
    let lock = refresh_lock(location)?;
    match lock.try_lock() {
        Ok(()) => drop(lock),
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(std::fs::TryLockError::Error(err)) => {
            return Err(err).context("checking the refresh lock");
        }
    }
    refresh_in_background(location, agent_hearth).map(Some)
}

/// Starts `agent_hearth golden refresh` detached, at low CPU priority, logging to
/// `<state>/goldens/refresh.log`, and returns the log's path. Another refresh in progress makes
/// it wait for the refresh lock.
pub fn refresh_in_background(location: &Location, agent_hearth: &Path) -> Result<PathBuf> {
    let goldens = location.state_dir.join("goldens");
    fs::create_dir_all(&goldens).with_context(|| format!("creating {}", goldens.display()))?;
    let log_path = goldens.join("refresh.log");
    let log =
        fs::File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
    let base = location.base();
    #[cfg(unix)]
    let mut command = {
        let mut nice = Command::new("nice");
        nice.args(["-n", "10"]).arg(agent_hearth);
        nice
    };
    #[cfg(not(unix))]
    let mut command = Command::new(agent_hearth);
    command
        .args(["golden", "refresh", "--rev", "main"])
        .current_dir(&location.worktree)
        .env("AGENT_HEARTH_STATE_DIR", &base.state)
        .env("AGENT_HEARTH_CACHE_DIR", &base.cache)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    // Not waited for: it outlives this process.
    command
        .spawn()
        .context("starting a background golden refresh")?;
    Ok(log_path)
}

/// Builds `rev` in a new golden unless an identical one exists, then makes it current. The new
/// golden is seeded from the current one, as `adopt` seeds a worktree, when their toolchain and
/// flags match: every golden builds at the pin, so cargo rebuilds only what changed. Returns the
/// golden and whether it had to be built.
pub fn refresh(location: &Location, rev: &str) -> Result<(Golden, bool)> {
    let goldens = location.state_dir.join("goldens");
    fs::create_dir_all(&goldens).with_context(|| format!("creating {}", goldens.display()))?;
    let lock = refresh_lock(location)?;
    lock.lock().context("waiting for another golden refresh")?;

    let commit = resolve(location, rev)?;
    let staging = goldens.join("staging");
    remove_dir_if_exists(&staging)?;
    materialise(&location.git_dir, &commit, &staging)?;
    let inputs = Inputs::of(&staging.join("src"))?;
    let mut golden = Golden {
        id: golden_id(&commit, &inputs)?,
        commit,
        inputs,
        generation: 0,
    };

    let dir = goldens.join(&golden.id);
    let built = !dir.join(METADATA).is_file();
    if built {
        remove_dir_if_exists(&dir)?;
        fs::rename(&staging, &dir).with_context(|| format!("moving {}", dir.display()))?;
        let seed = current(location)?.filter(|old| {
            old.inputs.toolchain == golden.inputs.toolchain
                && old.inputs.flags == golden.inputs.flags
                && old.generation + 1 < GENERATIONS
        });
        if let Some(old) = seed {
            let old_dir = goldens.join(&old.id);
            // Without reflinks, the golden is built fresh.
            match adopt::clone_tree(&old_dir.join("target"), &dir.join("target")) {
                Ok(_) => {
                    let src = dir.join("src");
                    adopt::sync_mtimes(&old_dir.join("src"), &src, files_under(&src)?)?;
                    golden.generation = old.generation + 1;
                }
                Err(err) => {
                    eprintln!("agent-hearth: building the golden fresh: {err:#}");
                    remove_dir_if_exists(&dir.join("target"))?;
                }
            }
        }
        let failed = build(location, &dir)?;
        let pinned = location.pinned();
        let (pin, mounts) = match &pinned {
            Some((pin, mounts)) => (Some(*pin), mounts.as_slice()),
            None => (None, &[][..]),
        };
        let baseline = check::baseline_of(&dir.join("src"), &dir.join("target"), pin, mounts)?;
        // Errors in main's own code are main's, which `check` leaves out, and the golden holds
        // everything else. A failure that names no file (a build script, a missing library) is
        // the machine's, and worktrees would fail the same way.
        if let Some(failed) = failed.first() {
            if baseline.errors == 0 {
                bail!("golden build failed: {failed}");
            }
            eprintln!(
                "agent-hearth: main does not build cleanly with this toolchain ({failed}): {} \
                 errors in its code, which `check` leaves out; the golden holds the rest",
                baseline.errors
            );
        }
        write_atomically(&dir.join(BASELINE), &serde_json::to_vec_pretty(&baseline)?)?;
        write_atomically(&dir.join(METADATA), &serde_json::to_vec_pretty(&golden)?)?;
    } else {
        fs::remove_dir_all(&staging)?;
    }
    write_atomically(&goldens.join("current"), golden.id.as_bytes())?;
    Ok((golden, built))
}

/// Resolves `rev` to one commit id with the tool that found the worktree.
pub fn resolve(location: &Location, rev: &str) -> Result<String> {
    let mut command = match location.vcs {
        Vcs::Jj => {
            let mut jj = crate::locate::jj_command();
            jj.args([
                "--ignore-working-copy",
                "--color=never",
                "log",
                "--no-graph",
            ])
            .arg(format!("--revisions={rev}"))
            .args(["--template", "commit_id ++ \"\\n\""]);
            jj
        }
        Vcs::Git => {
            let mut git = Command::new("git");
            git.args(["rev-parse", "--verify", "--end-of-options"])
                .arg(format!("{rev}^{{commit}}"));
            git
        }
    };
    let stdout = capture(command.current_dir(&location.worktree))
        .with_context(|| format!("resolving revision {rev}"))?;
    match stdout.lines().collect::<Vec<_>>()[..] {
        [commit] => Ok(commit.to_owned()),
        ref commits => bail!("revision {rev} is {} commits, not one", commits.len()),
    }
}

/// Writes `commit`'s tree to `staging/src` through a temporary index, so the repository itself
/// is only read.
fn materialise(git_dir: &Path, commit: &str, staging: &Path) -> Result<()> {
    let src = staging.join("src");
    fs::create_dir_all(&src).with_context(|| format!("creating {}", src.display()))?;
    let index = staging.join("index");
    let git = |args: &[&str]| {
        let mut git = Command::new("git");
        git.arg("--git-dir")
            .arg(git_dir)
            .arg("--work-tree")
            .arg(&src)
            .args(args)
            .env("GIT_INDEX_FILE", &index);
        capture(&mut git).map(drop)
    };
    git(&["read-tree", commit])?;
    git(&["checkout-index", "--all"])?;
    fs::remove_file(&index).context("removing the temporary index")
}

/// Every file under `dir`, as paths relative to it: a golden's source tree holds only what its
/// commit tracks.
fn files_under(dir: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(next) = dirs.pop() {
        for entry in fs::read_dir(&next).with_context(|| format!("reading {}", next.display()))? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                dirs.push(entry.path());
            } else if let Ok(relative) = entry.path().strip_prefix(dir) {
                files.push(relative.to_string_lossy().into_owned());
            }
        }
    }
    Ok(files)
}

fn golden_id(commit: &str, inputs: &Inputs) -> Result<String> {
    let hashed = serde_json::to_vec(&(commit, inputs))?;
    Ok(blake3::hash(&hashed).to_hex()[..16].to_owned())
}

fn build(location: &Location, dir: &Path) -> Result<Vec<String>> {
    let (src, target) = (dir.join("src"), dir.join("target"));
    fs::create_dir_all(&target).with_context(|| format!("creating {}", target.display()))?;
    let alone = builds::packages_built_alone(location);
    let pinned = location.pinned();
    let (pin, mounts) = match &pinned {
        Some((pin, mounts)) => (Some(*pin), mounts.as_slice()),
        None => {
            location.link_outside_dependencies(&src)?;
            (None, &[][..])
        }
    };
    build_at(&src, &target, pin, mounts, &alone)
}

/// Runs the golden's builds of the tree `src` into `target`, at `pin` when given: the workspace's,
/// then, for each member in `alone`, its own. Returns the workspace builds that failed.
pub fn build_at(
    src: &Path,
    target: &Path,
    pin: Option<&Path>,
    mounts: &[(PathBuf, PathBuf)],
    alone: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let run = |args: &[&str]| {
        cargo::status(cargo::command(src, target, Path::new(""), pin, mounts).args(args))
    };
    let mut failed = Vec::new();
    for args in BUILDS {
        let status = run(args)?;
        if !status.success() {
            failed.push(format!("cargo {} exited with {status}", args.join(" ")));
        }
    }
    // Agents may build one package at a time (`cargo test -p <member>`), which resolves
    // dependency features apart from the workspace, so a fresh worktree's first such build was not
    // warm. Build the variants of the members agents built alone (from the build log). Every
    // member's took the golden from 168s to 867s on atuin (`bench/public-atuin-linux1.txt`). A
    // member need not build alone (nushell's plugins lean on features other members turn on),
    // and plain cargo would fail there too, so a failing variant is skipped rather than failing
    // the golden.
    let members = members(src)?;
    if members.len() > 1 {
        for name in members.iter().filter(|name| alone.contains(*name)) {
            let name = name.as_str();
            for args in [
                ["clippy", "-p", name, "--all-targets", "--quiet"],
                ["test", "-p", name, "--no-run", "--quiet"],
            ] {
                let status = run(&args)?;
                if !status.success() {
                    eprintln!(
                        "agent-hearth: skipped: cargo {} exited with {status}",
                        args.join(" ")
                    );
                    break;
                }
            }
        }
    }
    Ok(failed)
}

/// The names of the workspace's member packages.
fn members(src: &Path) -> Result<Vec<String>> {
    let metadata = capture(
        Command::new("cargo")
            .args(["metadata", "--no-deps", "--format-version", "1"])
            .current_dir(src),
    )?;
    let metadata: serde_json::Value =
        serde_json::from_str(&metadata).context("parsing cargo metadata")?;
    Ok(metadata["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|package| package["name"].as_str().map(String::from))
        .collect())
}

/// Runs `command` and returns its stdout, failing with its stderr.
fn capture(command: &mut Command) -> Result<String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("{program} failed: {}", stderr.trim()));
    }
    String::from_utf8(output.stdout).with_context(|| format!("{program} printed non-UTF-8 output"))
}

/// Replaces `path` by renaming a finished file over it, so readers see the old or the new content.
pub fn write_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", process::id()));
    fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

fn remove_dir_if_exists(dir: &Path) -> Result<()> {
    match fs::remove_dir_all(dir) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => {
            Err(err).with_context(|| format!("removing {}", dir.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Sandbox;

    fn current(location: &Location) -> String {
        fs::read_to_string(location.state_dir.join("goldens/current")).unwrap()
    }

    #[test]
    fn golden_id_covers_every_input() {
        let inputs = Inputs {
            cargo_lock: Some("l".to_owned()),
            toolchain: "t".to_owned(),
            flags: BTreeMap::from([("RUSTFLAGS".to_owned(), "-Copt-level=1".to_owned())]),
        };
        let id = |commit, change: fn(&mut Inputs)| {
            let mut changed = inputs.clone();
            change(&mut changed);
            golden_id(commit, &changed).unwrap()
        };
        let base = id("c", |_| {});
        assert_eq!(base, id("c", |_| {}));
        assert_ne!(base, id("d", |_| {}));
        assert_ne!(base, id("c", |i| i.cargo_lock = None));
        assert_ne!(base, id("c", |i| i.toolchain = "u".to_owned()));
        assert_ne!(base, id("c", |i| i.flags.clear()));
    }

    #[test]
    fn resolves_git_and_jj_revisions() {
        let s = Sandbox::new();
        s.run("git", "git", &["init", "-q", "-b", "main"]);
        s.run(
            "git",
            "git",
            &["commit", "-q", "--allow-empty", "-m", "one"],
        );
        let head = s.run("git", "git", &["rev-parse", "HEAD"]);
        assert_eq!(resolve(&s.location("git"), "main").unwrap(), head.trim());

        s.run("jj", "jj", &["git", "init", "--colocate"]);
        s.run("jj", "jj", &["commit", "-m", "one"]);
        let parent = s.run(
            "jj",
            "jj",
            &["log", "--no-graph", "-r", "@-", "-T", "commit_id"],
        );
        assert_eq!(resolve(&s.location("jj"), "@-").unwrap(), parent);

        let err = resolve(&s.location("jj"), "all()").unwrap_err();
        assert!(err.to_string().ends_with("commits, not one"), "{err}");
    }

    #[test]
    fn materialise_writes_the_tree_and_leaves_the_repository_alone() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        s.write("repo/src/lib.rs", "uncommitted edit\n");
        let index_before = fs::read(s.path("repo/.git/index")).unwrap();
        let commit = s.run("repo", "git", &["rev-parse", "HEAD"]);

        materialise(&s.path("repo/.git"), commit.trim(), &s.path("staging")).unwrap();

        let lib = fs::read_to_string(s.path("staging/src/src/lib.rs")).unwrap();
        assert!(lib.starts_with("pub fn one()"), "{lib}");
        assert!(s.path("staging/src/Cargo.lock").is_file());
        assert!(!s.path("staging/index").exists());
        assert_eq!(fs::read(s.path("repo/.git/index")).unwrap(), index_before);
    }

    #[test]
    fn refresh_builds_reuses_and_replaces_goldens() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let location = s.location("repo");

        let (first, built) = refresh(&location, "main").unwrap();
        assert!(built);
        assert_eq!(current(&location), first.id);
        let dir = location.state_dir.join("goldens").join(&first.id);
        assert!(dir.join("target/debug").is_dir());
        let stored: Golden =
            serde_json::from_slice(&fs::read(dir.join(METADATA)).unwrap()).unwrap();
        assert_eq!(stored, first);

        let (again, built) = refresh(&location, "main").unwrap();
        assert!(!built);
        assert_eq!(again, first);

        s.write("repo/src/lib.rs", "pub fn two() -> u32 {\n    2\n}\n");
        s.run("repo", "git", &["commit", "-q", "-am", "two"]);
        let (second, built) = refresh(&location, "main").unwrap();
        assert!(built);
        assert_ne!(second.id, first.id);
        assert_eq!(current(&location), second.id);
        assert!(dir.join(METADATA).is_file(), "the old golden stays for gc");
    }

    #[test]
    fn refreshes_in_the_background_only_when_main_has_moved() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let location = s.location("repo");
        // `true` stands in for agent-hearth: what matters is whether a refresh starts.
        let exe = Path::new("true");
        assert!(
            refresh_if_moved(&location, exe).unwrap().is_none(),
            "no golden yet"
        );
        refresh(&location, "main").unwrap();
        assert!(
            refresh_if_moved(&location, exe).unwrap().is_none(),
            "main has not moved"
        );

        s.write("repo/src/lib.rs", "pub fn two() -> u32 {\n    2\n}\n");
        s.run("repo", "git", &["commit", "-q", "-am", "two"]);
        let running = refresh_lock(&location).unwrap();
        running.lock().unwrap();
        assert!(
            refresh_if_moved(&location, exe).unwrap().is_none(),
            "one is running"
        );
        drop(running);
        assert!(refresh_if_moved(&location, exe).unwrap().is_some());
    }

    #[test]
    fn a_failed_build_leaves_current_alone() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let location = s.location("repo");
        let (good, _) = refresh(&location, "main").unwrap();

        // A failure that is the machine's, not main's code (that would be main's, in its
        // baseline; see check's tests).
        s.write(
            "repo/build.rs",
            "fn main() {\n    panic!(\"no library here\");\n}\n",
        );
        s.run("repo", "git", &["add", "."]);
        s.run(
            "repo",
            "git",
            &["commit", "-q", "-m", "a build script that fails"],
        );
        let err = refresh(&location, "main").unwrap_err();
        assert!(err.to_string().starts_with("golden build failed"), "{err}");
        assert_eq!(current(&location), good.id);
    }

    /// A workspace where `b` turns on a feature of `shared` that `a` alone does not, so `-p a`
    /// resolves `shared` differently from the workspace. `a_src` is `a`'s lib.rs.
    fn feature_workspace(s: &Sandbox, a_src: &str) {
        s.write(
            "repo/Cargo.toml",
            "[workspace]\nmembers = [\"a\", \"b\", \"shared\"]\nresolver = \"3\"\n",
        );
        let package = |name: &str, deps: &str, extra: &str, src: &str| {
            s.write(
                &format!("repo/{name}/Cargo.toml"),
                &format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                     [dependencies]\n{deps}\n{extra}"
                ),
            );
            s.write(&format!("repo/{name}/src/lib.rs"), src);
        };
        package(
            "shared",
            "",
            "[features]\nextra = []\n",
            "pub fn f() {}\n#[cfg(feature = \"extra\")]\npub fn extra() {}\n",
        );
        package("a", "shared = { path = \"../shared\" }", "", a_src);
        package(
            "b",
            "shared = { path = \"../shared\", features = [\"extra\"] }",
            "",
            "pub fn f() {}\n",
        );
        s.write("repo/.gitignore", "/target/\n");
        s.run("repo", "git", &["init", "-q", "-b", "main"]);
        s.run("repo", "cargo", &["generate-lockfile", "--quiet"]);
        s.run("repo", "git", &["add", "."]);
        s.run("repo", "git", &["commit", "-q", "-m", "workspace"]);
    }

    #[test]
    fn a_member_that_does_not_build_alone_does_not_fail_the_golden() {
        let s = Sandbox::new();
        // `a` calls `shared::extra`, which only `b`'s feature turns on.
        feature_workspace(&s, "pub fn f() { shared::extra() }\n");
        refresh(&s.location("repo"), "main").unwrap();
    }

    /// Whether a new worktree seeded from the repository's golden compiles anything for
    /// `-p a`'s test build and clippy.
    fn one_package_compiles(s: &Sandbox, name: &str) -> bool {
        s.run(
            "repo",
            "git",
            &["worktree", "add", "-q", &format!("../{name}")],
        );
        let wt = s.location(name);
        crate::adopt::adopt(&wt).unwrap();
        let target = crate::adopt::target_dir(&wt);
        [
            &["test", "-p", "a", "--no-run"][..],
            &["clippy", "-p", "a", "--all-targets"],
        ]
        .iter()
        .any(|args| {
            let mut args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
            args.insert(1, "--message-format=json".to_owned());
            let mut out = Vec::new();
            let code = cargo::run_to(&wt, &target, &wt.worktree, &args, &[], &mut out, io::sink());
            assert_eq!(code.unwrap(), 0, "{args:?}");
            String::from_utf8(out).unwrap().contains("\"fresh\":false")
        })
    }

    #[test]
    fn a_seeded_worktree_builds_a_package_agents_built_alone_without_compiling() {
        let s = Sandbox::new();
        feature_workspace(&s, "pub fn f() {}\n");
        let repo = s.location("repo");
        // Without a build of `-p a` in the log, the golden has no variant for it.
        refresh(&repo, "main").unwrap();
        assert!(one_package_compiles(&s, "wt1"));

        let args: Vec<String> = ["test", "-p", "a"].map(String::from).to_vec();
        builds::record(&repo, &args, 0, std::time::SystemTime::now());
        s.write("repo/a/src/lib.rs", "pub fn f() {}\npub fn g() {}\n");
        s.run("repo", "git", &["commit", "-q", "-am", "next"]);
        refresh(&repo, "main").unwrap();
        assert!(!one_package_compiles(&s, "wt2"));
    }

    /// The mtimes of the `libshared` rlibs under a golden's target.
    fn shared_rlibs(dir: &Path) -> Vec<std::time::SystemTime> {
        let deps = dir.join("target/debug/deps");
        let mut mtimes: Vec<_> = fs::read_dir(deps)
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.starts_with("libshared-") && name.ends_with(".rlib")
            })
            .map(|entry| entry.metadata().unwrap().modified().unwrap())
            .collect();
        mtimes.sort();
        mtimes
    }

    #[test]
    fn a_golden_is_seeded_from_the_current_one_and_rebuilds_only_what_changed() {
        let s = Sandbox::new();
        feature_workspace(&s, "pub fn f() {}\n");
        let repo = s.location("repo");
        let (first, _) = refresh(&repo, "main").unwrap();
        assert_eq!(first.generation, 0);

        s.write("repo/a/src/lib.rs", "pub fn f() {}\npub fn g() {}\n");
        s.run("repo", "git", &["commit", "-q", "-am", "a changes"]);
        let (second, built) = refresh(&repo, "main").unwrap();
        assert!(built);
        assert_eq!(second.generation, 1);
        let (old, new) = (dir(&repo, &first.id), dir(&repo, &second.id));
        assert!(!shared_rlibs(&old).is_empty());
        assert_eq!(shared_rlibs(&new), shared_rlibs(&old), "shared was rebuilt");

        // The new golden holds main's `a`: a worktree seeded from it compiles nothing.
        s.run("repo", "git", &["worktree", "add", "-q", "../wt"]);
        let wt = s.location("wt");
        crate::adopt::adopt(&wt).unwrap();
        let target = crate::adopt::target_dir(&wt);
        let args: Vec<String> = ["test", "--message-format=json", "--workspace", "--no-run"]
            .map(String::from)
            .to_vec();
        let mut out = Vec::new();
        let code = cargo::run_to(&wt, &target, &wt.worktree, &args, &[], &mut out, io::sink());
        assert_eq!(code.unwrap(), 0);
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("\"fresh\":false"), "compiled:\n{out}");
    }
}
