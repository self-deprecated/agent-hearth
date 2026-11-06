//! Seeds a worktree's own build directory from the golden that matches it best.
//!
//! ```text
//! <state>/targets/<worktree-id>/        the worktree's build directory, a reflink clone of a golden's
//! <state>/worktrees/<worktree-id>.json  which worktree it belongs to and which golden seeded it
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{self, Command};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::check;
use crate::golden::{self, Golden, Inputs};
use crate::locate::{Location, Vcs};

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    worktree: PathBuf,
    /// `None` for a build directory that started cold, before there was a golden.
    #[serde(default)]
    golden: Option<String>,
}

#[derive(Debug)]
pub struct Adopted {
    pub golden: Golden,
    pub files: u64,
    pub bytes: u64,
    /// Source files identical to the golden's, which now carry its mtimes.
    pub unchanged: u64,
    /// Source files in the worktree.
    pub sources: u64,
}

#[derive(Debug)]
pub enum Outcome {
    Adopted(Adopted),
    /// The worktree already has a build directory.
    Already,
    /// There is no golden to seed from.
    NoGolden,
    /// The state directory's filesystem cannot clone files (ext4, tmpfs, NTFS), and a full copy
    /// of a golden would cost more than it saves: the reason.
    NoReflinks(String),
}

/// Whether files in `dir` can be cloned: clones a scratch file there.
pub fn reflinks_work(dir: &Path) -> std::io::Result<()> {
    let scratch = dir.join(format!("reflink-probe-{}", process::id()));
    let (from, to) = (scratch.with_extension("a"), scratch.with_extension("b"));
    let result = fs::write(&from, b"agent-hearth").and_then(|()| reflink_copy::reflink(&from, &to));
    for path in [&from, &to] {
        let _ = fs::remove_file(path);
    }
    result
}

/// The worktree's own build directory.
pub fn target_dir(location: &Location) -> PathBuf {
    location
        .state_dir
        .join("targets")
        .join(worktree_id(location))
}

/// The golden that seeded the worktree's build directory, if it was adopted.
pub fn seeded_from(location: &Location) -> Result<Option<String>> {
    let path = record_path(location);
    match fs::read(&path) {
        Ok(json) => {
            let record: Record = serde_json::from_slice(&json)
                .with_context(|| format!("reading {}", path.display()))?;
            Ok(record.golden)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Every worktree adopted in the repository, whether or not it still exists.
pub fn recorded_worktrees(location: &Location) -> Result<Vec<PathBuf>> {
    Ok(records(location)?
        .into_iter()
        .map(|(_, path)| path)
        .collect())
}

/// Every recorded worktree's id (which names its build directory) and path.
pub fn records(location: &Location) -> Result<Vec<(String, PathBuf)>> {
    let dir = location.state_dir.join("worktrees");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", dir.display())),
    };
    let mut worktrees = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let record: Record = serde_json::from_slice(&fs::read(&path)?)
                .with_context(|| format!("reading {}", path.display()))?;
            let id = path.file_stem().unwrap_or_default().to_string_lossy();
            worktrees.push((id.into_owned(), record.worktree));
        }
    }
    worktrees.sort();
    Ok(worktrees)
}

fn worktree_id(location: &Location) -> String {
    let id = blake3::hash(location.worktree.as_os_str().as_encoded_bytes()).to_hex();
    id[..16].to_owned()
}

fn record_path(location: &Location) -> PathBuf {
    let name = format!("{}.json", worktree_id(location));
    location.state_dir.join("worktrees").join(name)
}

/// Seeds the worktree's build directory from the best golden and gives source files identical to
/// the golden's its mtimes, so cargo rebuilds only what differs.
pub fn adopt(location: &Location) -> Result<Outcome> {
    let target = target_dir(location);
    if target.is_dir() {
        return Ok(Outcome::Already);
    }
    // Shared: `gc` deletes goldens only while no adopt is reading one.
    let gc_lock = golden::gc_lock(location)?;
    gc_lock.lock_shared().context("waiting for gc")?;
    let Some(golden) = choose(location)? else {
        return Ok(Outcome::NoGolden);
    };
    let golden_dir = golden::dir(location, &golden.id);
    let targets = target.parent().context("target directory has no parent")?;
    fs::create_dir_all(targets).with_context(|| format!("creating {}", targets.display()))?;
    if let Err(err) = reflinks_work(targets) {
        return Ok(Outcome::NoReflinks(format!(
            "the state directory's filesystem can't clone files ({err}): seeding needs btrfs, \
             XFS, APFS or ReFS"
        )));
    }
    // Before the build directory appears, so no build in it sees the worktree's mtimes unsynced.
    let (unchanged, sources) = sync_mtimes(
        &golden_dir.join("src"),
        &location.worktree,
        check::source_files(location)?,
    )?;

    let staging = target.with_extension(format!("tmp-{}", process::id()));
    let cloned = clone_tree(&golden_dir.join("target"), &staging);
    let (files, bytes) = cloned.inspect_err(|_| drop(fs::remove_dir_all(&staging)))?;
    if let Err(err) = fs::rename(&staging, &target) {
        let raced = target.is_dir();
        fs::remove_dir_all(&staging)?;
        if raced {
            // A concurrent adopt of the same worktree finished first.
            return Ok(Outcome::Already);
        }
        return Err(err).with_context(|| format!("moving {}", target.display()));
    }

    drop(gc_lock);
    write_record(location, Some(&golden.id))?;
    Ok(Outcome::Adopted(Adopted {
        golden,
        files,
        bytes,
        unchanged,
        sources,
    }))
}

/// [`adopt`], or an empty build directory when there is no golden to seed it from, or no way to:
/// the build then starts cold, as plain cargo would, rather than fail.
pub fn adopt_or_start_cold(location: &Location) -> Result<Outcome> {
    let outcome = adopt(location)?;
    if let Outcome::NoGolden | Outcome::NoReflinks(_) = outcome {
        start_cold(location)?;
    }
    Ok(outcome)
}

/// Deletes the worktree's build directory and seeds it again, for `cargo clean`: cargo would
/// leave it empty (pinned, it then fails on the mount point), and the next builds would be cold.
pub fn reseed(location: &Location) -> Result<Outcome> {
    let target = target_dir(location);
    match fs::remove_dir_all(&target) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            return Err(err).with_context(|| format!("removing {}", target.display()));
        }
        _ => {}
    }
    adopt_or_start_cold(location)
}

/// Gives the worktree an empty build directory, for when there is no golden to seed it from.
fn start_cold(location: &Location) -> Result<()> {
    let target = target_dir(location);
    fs::create_dir_all(&target).with_context(|| format!("creating {}", target.display()))?;
    write_record(location, None)
}

fn write_record(location: &Location, golden: Option<&str>) -> Result<()> {
    let record = record_path(location);
    fs::create_dir_all(record.parent().context("record has no parent")?)?;
    let json = serde_json::to_vec_pretty(&Record {
        worktree: location.worktree.clone(),
        golden: golden.map(str::to_owned),
    })?;
    fs::write(&record, json).with_context(|| format!("writing {}", record.display()))
}

/// Prefers a golden with the worktree's inputs, then the nearest ancestor of its revision.
fn choose(location: &Location) -> Result<Option<Golden>> {
    let inputs = Inputs::of(&location.worktree, location.toolchain.as_ref())?;
    let head = golden::resolve(
        location,
        match location.vcs {
            Vcs::Jj => "@",
            Vcs::Git => "HEAD",
        },
    )?;
    let mut best: Option<((bool, u64), Golden)> = None;
    for candidate in golden::finished(location)? {
        let distance = ancestor_distance(&location.git_dir, &candidate.commit, &head)?;
        let key = (candidate.inputs != inputs, distance.unwrap_or(u64::MAX));
        if best.as_ref().is_none_or(|(best_key, _)| key < *best_key) {
            best = Some((key, candidate));
        }
    }
    Ok(best.map(|(_, golden)| golden))
}

/// How many commits `head` is ahead of `ancestor`, or `None` when it is not a descendant.
fn ancestor_distance(git_dir: &Path, ancestor: &str, head: &str) -> Result<Option<u64>> {
    let git = || {
        let mut git = Command::new("git");
        git.arg("--git-dir").arg(git_dir);
        git
    };
    let is_ancestor = git()
        .args(["merge-base", "--is-ancestor", ancestor, head])
        .status()
        .context("running git merge-base")?;
    match is_ancestor.code() {
        Some(0) => {}
        Some(1) => return Ok(None),
        _ => bail!("git merge-base --is-ancestor {ancestor} {head} failed"),
    }
    let output = git()
        .args(["rev-list", "--count", &format!("{ancestor}..{head}")])
        .output()
        .context("running git rev-list")?;
    let count = String::from_utf8_lossy(&output.stdout);
    count
        .trim()
        .parse()
        .map(Some)
        .with_context(|| format!("git rev-list --count printed {count:?}"))
}

/// Reflink-clones every file under `from` into a new directory `to`, keeping mtimes and
/// permissions. Returns the number of files and their total size.
pub fn clone_tree(from: &Path, to: &Path) -> Result<(u64, u64)> {
    fs::create_dir(to).with_context(|| format!("creating {}", to.display()))?;
    let (mut files, mut bytes) = (0, 0);
    for entry in fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            let (f, b) = clone_tree(&src, &dst)?;
            (files, bytes) = (files + f, bytes + b);
        } else if kind.is_symlink() {
            copy_symlink(&src, &dst)?;
        } else {
            reflink_copy::reflink(&src, &dst).with_context(|| {
                format!(
                    "cloning {}: seeding needs a filesystem with reflinks (btrfs, XFS, APFS, \
                     ReFS), and Agent Hearth never falls back to a full copy",
                    src.display()
                )
            })?;
            let metadata = entry.metadata()?;
            set_mtime(&dst, metadata.modified()?)?;
            (files, bytes) = (files + 1, bytes + metadata.len());
        }
    }
    Ok((files, bytes))
}

#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> Result<()> {
    std::os::unix::fs::symlink(fs::read_link(src)?, dst)
        .with_context(|| format!("copying the symlink {}", src.display()))
}

#[cfg(not(unix))]
fn copy_symlink(src: &Path, _dst: &Path) -> Result<()> {
    bail!(
        "{} is a symlink, which seeding does not support here",
        src.display()
    )
}

/// Gives each of `paths` under `root` (the files the build could read, those git does not ignore)
/// the golden's mtime when its content is identical to the golden's copy, and the time now
/// otherwise. A differing file left older than the golden's outputs would make cargo take them,
/// compiled from the golden's copy, for fresh. Never touches content. Returns (identical files,
/// files).
///
/// Directories too: cargo counts a directory's own mtime (and its subdirectories') when a build
/// script names it in `rerun-if-changed`, and a new worktree's directories are all newer than the
/// golden's build: atuin's `migrations` reran four build scripts and 50 artifacts in every new
/// worktree. A directory gets the golden's mtime when it holds the same entries as the golden's
/// copy, and the time now otherwise: deleting a file changes only its directory's mtime.
pub fn sync_mtimes(
    golden_src: &Path,
    root: &Path,
    paths: impl IntoIterator<Item = String>,
) -> Result<(u64, u64)> {
    let now = std::time::SystemTime::now();
    let (mut unchanged, mut sources) = (0, 0);
    let paths: Vec<String> = paths.into_iter().collect();
    for path in &paths {
        let ours = root.join(path);
        let metadata = match fs::symlink_metadata(&ours) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err).with_context(|| format!("reading {}", ours.display())),
        };
        // Not following symlinks: only a regular file of the same size can be identical.
        if !metadata.is_file() {
            continue;
        }
        sources += 1;
        let golden = golden_src.join(path);
        let same_size = fs::symlink_metadata(&golden)
            .ok()
            .filter(|golden| golden.is_file() && golden.len() == metadata.len());
        match same_size {
            Some(golden_meta) if fs::read(&golden)? == fs::read(&ours)? => {
                set_mtime(&ours, golden_meta.modified()?)?;
                unchanged += 1;
            }
            _ => set_mtime(&ours, now)?,
        }
    }
    // Each directory holding sources, but not the root, which holds `.git` and builds' output, and
    // its entries named by `paths`.
    let mut dirs: BTreeMap<&Path, BTreeSet<&std::ffi::OsStr>> = BTreeMap::new();
    for path in &paths {
        let mut child = Path::new(path.as_str());
        while let Some(dir) = child.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            if let Some(name) = child.file_name() {
                dirs.entry(dir).or_default().insert(name);
            }
            child = dir;
        }
    }
    for (dir, ours) in dirs {
        let golden = golden_src.join(dir);
        let theirs: Option<BTreeSet<std::ffi::OsString>> =
            fs::read_dir(&golden).ok().map(|entries| {
                entries
                    .filter_map(|entry| Some(entry.ok()?.file_name()))
                    .collect()
            });
        let same = theirs.is_some_and(|theirs| {
            theirs.len() == ours.len() && ours.iter().all(|name| theirs.contains(*name))
        });
        let mtime = match fs::metadata(&golden) {
            Ok(metadata) if same => metadata.modified()?,
            _ => now,
        };
        let ours = root.join(dir);
        if ours.is_dir() {
            set_mtime(&ours, mtime)?;
        }
    }
    Ok((unchanged, sources))
}

/// Sets `path`'s mtime without opening it for writing, so read-only files (build scripts often
/// copy them from a read-only Nix store) work too.
fn set_mtime(path: &Path, mtime: std::time::SystemTime) -> Result<()> {
    let mut options = OpenOptions::new();
    // The owner may set a file's times through any descriptor.
    #[cfg(unix)]
    options.read(true);
    // Windows needs only the right to write attributes, which read-only files grant, and opens a
    // directory only with backup semantics.
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        options
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    }
    options
        .open(path)
        .and_then(|file| file.set_modified(mtime))
        .with_context(|| format!("setting the mtime of {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::cargo;
    use crate::testing::Sandbox;

    fn mtime(path: &Path) -> SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    fn age(path: &Path, seconds: u64) {
        set_mtime(path, SystemTime::now() - Duration::from_secs(seconds)).unwrap();
    }

    #[test]
    fn reseed_replaces_the_build_directory_with_the_golden_s() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let location = s.location("repo");
        golden::refresh(&location, "main").unwrap();
        adopt(&location).unwrap();
        let target = target_dir(&location);
        fs::write(target.join("leftover"), "").unwrap();
        fs::remove_dir_all(target.join("debug")).unwrap();

        assert!(matches!(reseed(&location).unwrap(), Outcome::Adopted(_)));
        assert!(!target.join("leftover").exists());
        assert!(target.join("debug").is_dir());
    }

    #[test]
    fn clone_tree_keeps_content_mtimes_and_permissions() {
        let s = Sandbox::new();
        s.write("from/a", "a");
        s.write("from/deep/b", "bb");
        age(&s.path("from/deep/b"), 3600);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(s.path("from/a"), fs::Permissions::from_mode(0o755)).unwrap();
            // Read-only, as build scripts copying from the Nix store leave files.
            fs::set_permissions(s.path("from/deep/b"), fs::Permissions::from_mode(0o444)).unwrap();
            std::os::unix::fs::symlink("a", s.path("from/link")).unwrap();
        }

        assert_eq!(clone_tree(&s.path("from"), &s.path("to")).unwrap(), (2, 3));
        assert_eq!(fs::read_to_string(s.path("to/deep/b")).unwrap(), "bb");
        assert_eq!(mtime(&s.path("to/deep/b")), mtime(&s.path("from/deep/b")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(s.path("to/a")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
            assert_eq!(fs::read_link(s.path("to/link")).unwrap(), Path::new("a"));
        }
    }

    #[test]
    fn sync_mtimes_gives_identical_files_the_golden_s_mtime_and_others_now() {
        let s = Sandbox::new();
        s.crate_repo("ours");
        s.write("ours/same", "x");
        s.write("ours/dir/changed", "y");
        s.write("ours/new", "z");
        s.write("ours/target/ignored", "x");
        for name in ["same", "dir/changed"] {
            s.write(&format!("golden/{name}"), "x");
            age(&s.path(&format!("golden/{name}")), 3600);
        }
        s.write("golden/missing", "x");
        for name in ["same", "dir/changed", "new", "target/ignored"] {
            age(&s.path(&format!("ours/{name}")), 7200);
        }
        let ignored_before = mtime(&s.path("ours/target/ignored"));
        let before = SystemTime::now();

        let ours = s.location("ours");
        let paths = check::source_files(&ours).unwrap();
        let (unchanged, sources) = sync_mtimes(&s.path("golden"), &ours.worktree, paths).unwrap();
        assert_eq!(unchanged, 1);
        assert!(sources >= 3);
        assert_eq!(mtime(&s.path("ours/same")), mtime(&s.path("golden/same")));
        assert!(mtime(&s.path("ours/dir/changed")) >= before);
        assert!(mtime(&s.path("ours/new")) >= before);
        assert_eq!(mtime(&s.path("ours/target/ignored")), ignored_before);
        assert_eq!(fs::read_to_string(s.path("ours/dir/changed")).unwrap(), "y");
    }

    #[test]
    fn a_directory_gets_the_golden_s_mtime_only_with_the_same_entries() {
        let s = Sandbox::new();
        s.crate_repo("ours");
        for name in ["same/a", "fewer/a"] {
            s.write(&format!("ours/{name}"), "x");
        }
        for name in ["same/a", "fewer/a", "fewer/deleted"] {
            s.write(&format!("golden/{name}"), "x");
        }
        for dir in ["same", "fewer"] {
            age(&s.path(&format!("golden/{dir}")), 3600);
            age(&s.path(&format!("ours/{dir}")), 7200);
        }
        let before = SystemTime::now();
        let ours = s.location("ours");
        let paths = check::source_files(&ours).unwrap();
        sync_mtimes(&s.path("golden"), &ours.worktree, paths).unwrap();
        assert_eq!(mtime(&s.path("ours/same")), mtime(&s.path("golden/same")));
        // A file deleted since the golden shows only in its directory's mtime.
        assert!(mtime(&s.path("ours/fewer")) >= before);
    }

    #[test]
    fn a_build_script_watching_a_directory_does_not_rerun_in_a_new_worktree() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        s.write(
            "repo/build.rs",
            "fn main() {\n    println!(\"cargo:rerun-if-changed=migrations\");\n}\n",
        );
        s.write("repo/migrations/one/up.sql", "create table t (x int);\n");
        s.run("repo", "git", &["add", "."]);
        s.run("repo", "git", &["commit", "-q", "-m", "migrations"]);
        golden::refresh(&s.location("repo"), "main").unwrap();
        s.run("repo", "git", &["worktree", "add", "-q", "../wt"]);
        let wt = s.location("wt");
        adopt(&wt).unwrap();
        let args = ["build", "--message-format=json"].map(String::from);
        let mut out = Vec::new();
        let code = cargo::run_to(
            &wt,
            &target_dir(&wt),
            &wt.worktree,
            &args,
            &[],
            &mut out,
            std::io::sink(),
        );
        assert_eq!(code.unwrap(), 0);
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("\"fresh\":false"), "compiled:\n{out}");
    }

    #[test]
    fn an_edit_older_than_the_golden_is_built_not_taken_from_the_golden() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        s.run("repo", "git", &["worktree", "add", "-q", "../wt"]);
        // The worktree's edit predates the golden, as in any worktree with work adopted later.
        s.write("wt/src/lib.rs", "pub fn one() -> u32 {\n    2\n}\n");
        age(&s.path("wt/src/lib.rs"), 3600);
        golden::refresh(&s.location("repo"), "main").unwrap();
        let wt = s.location("wt");
        let before = SystemTime::now();

        assert!(matches!(adopt(&wt).unwrap(), Outcome::Adopted(_)));
        assert!(mtime(&s.path("wt/src/lib.rs")) >= before);
        assert_eq!(rebuilt(&wt), BTreeSet::from(["tiny".to_owned()]));
    }

    /// Builds the worktree at the pin into its own build directory and returns the names of the
    /// workspace crates cargo had to compile.
    fn rebuilt(location: &Location) -> BTreeSet<String> {
        let mut cargo = cargo::command(
            &location.worktree,
            &target_dir(location),
            Path::new(""),
            location.pin.as_deref(),
            &location
                .pinned()
                .map(|(_, mounts)| mounts)
                .unwrap_or_default(),
            location.toolchain.as_ref(),
        );
        let output = cargo
            .args([
                "clippy",
                "--workspace",
                "--all-targets",
                "--message-format=json",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|message| message["reason"] == "compiler-artifact" && message["fresh"] == false)
            .map(|message| message["target"]["name"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn an_adopted_worktree_starts_warm() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        golden::refresh(&s.location("repo"), "main").unwrap();
        s.run("repo", "git", &["worktree", "add", "-q", "../wt"]);
        let wt = s.location("wt");

        let Outcome::Adopted(adopted) = adopt(&wt).unwrap() else {
            panic!("adopts a new worktree");
        };
        assert!(adopted.files > 0);
        assert_eq!(adopted.unchanged, adopted.sources);
        assert!(matches!(adopt(&wt).unwrap(), Outcome::Already));
        let record: Record = serde_json::from_slice(&fs::read(record_path(&wt)).unwrap()).unwrap();
        assert_eq!(record.worktree, wt.worktree);
        assert_eq!(seeded_from(&wt).unwrap(), Some(adopted.golden.id));

        if wt.pin.is_some() {
            assert_eq!(
                rebuilt(&wt),
                BTreeSet::new(),
                "seeded and pinned: nothing to build"
            );
        }
        s.write("wt/src/lib.rs", "pub fn one() -> u32 {\n    2\n}\n");
        assert_eq!(rebuilt(&wt), BTreeSet::from(["tiny".to_owned()]));
    }

    #[test]
    fn a_golden_refresh_during_adopts_breaks_nothing() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let repo = s.location("repo");
        let (first, _) = golden::refresh(&repo, "main").unwrap();
        for n in 0..6 {
            s.run(
                "repo",
                "git",
                &["worktree", "add", "-q", "--detach", &format!("../wt{n}")],
            );
        }
        s.write("repo/src/lib.rs", "pub fn two() -> u32 {\n    2\n}\n");
        s.run("repo", "git", &["commit", "-q", "-am", "two"]);
        let worktrees: Vec<Location> = (0..6).map(|n| s.location(&format!("wt{n}"))).collect();
        let second = std::thread::scope(|scope| {
            let refresh = scope.spawn(|| golden::refresh(&repo, "main").unwrap().0);
            for wt in &worktrees {
                let Outcome::Adopted(adopted) = adopt(wt).unwrap() else {
                    panic!("adopts {}", wt.worktree.display());
                };
                assert!(adopted.files > 0);
            }
            refresh.join().unwrap()
        });
        assert_ne!(second.id, first.id);
        for wt in &worktrees {
            let seeded = seeded_from(wt).unwrap().unwrap();
            assert!(seeded == first.id || seeded == second.id);
            if wt.pin.is_some() {
                assert_eq!(
                    rebuilt(wt),
                    BTreeSet::new(),
                    "{} starts warm",
                    wt.worktree.display()
                );
            }
        }
    }

    #[test]
    fn adopting_and_building_leave_the_worktree_as_git_sees_it() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        golden::refresh(&s.location("repo"), "main").unwrap();
        s.run("repo", "git", &["worktree", "add", "-q", "../wt"]);
        let wt = s.location("wt");
        adopt(&wt).unwrap();
        rebuilt(&wt);
        let status = s.run("wt", "git", &["status", "--porcelain", "--ignored"]);
        assert_eq!(status, "", "no new files, ignored or not");
    }

    #[test]
    fn choose_prefers_the_nearest_ancestor() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let repo = s.location("repo");
        let (first, _) = golden::refresh(&repo, "main").unwrap();
        s.write("repo/src/lib.rs", "pub fn two() -> u32 {\n    2\n}\n");
        s.run("repo", "git", &["commit", "-q", "-am", "two"]);
        let (second, _) = golden::refresh(&repo, "main").unwrap();

        s.run(
            "repo",
            "git",
            &["worktree", "add", "-q", "--detach", "../old", "main~1"],
        );
        assert_eq!(choose(&s.location("old")).unwrap().unwrap().id, first.id);
        s.write("repo/README", "later\n");
        s.run("repo", "git", &["add", "README"]);
        s.run("repo", "git", &["commit", "-q", "-m", "later"]);
        assert_eq!(choose(&repo).unwrap().unwrap().id, second.id);
    }

    #[test]
    fn adopt_without_a_golden_leaves_the_worktree_alone() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let repo = s.location("repo");
        assert!(matches!(adopt(&repo).unwrap(), Outcome::NoGolden));
        assert!(!target_dir(&repo).exists());
    }
}
