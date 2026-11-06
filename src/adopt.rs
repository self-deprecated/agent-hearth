//! Seeds a worktree's own build directory from the golden that matches it best.
//!
//! ```text
//! <state>/targets/<worktree-id>/        the worktree's build directory, a reflink clone of a golden's
//! <state>/worktrees/<worktree-id>.json  which worktree it belongs to and which golden seeded it
//! <state>/goldens/<id>/linked.json      what of a golden's build directory may be hardlinked, as built
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io;
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
    /// Without reflinks: of `bytes`, those copied rather than hardlinked (see [`link_tree`]).
    pub copied: Option<u64>,
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
    /// The state directory's filesystem can neither clone files nor, on Unix, share them by
    /// hardlinks between the goldens and the build directories, and a full copy of a golden would
    /// cost more than it saves: the reason. The build starts cold.
    Cold(String),
    /// The golden that would have seeded the worktree was changed in place since it was built,
    /// or cannot be checked: the reason. It is marked so, seeds nothing until a refresh rebuilds
    /// it, and the build starts cold.
    Damaged(String),
}

/// Whether files in `dir` can be cloned: clones a scratch file there.
pub fn reflinks_work(dir: &Path) -> io::Result<()> {
    let scratch = dir.join(format!("reflink-probe-{}", process::id()));
    let (from, to) = (scratch.with_extension("a"), scratch.with_extension("b"));
    let result = fs::write(&from, b"agent-hearth").and_then(|()| reflink_copy::reflink(&from, &to));
    for path in [&from, &to] {
        let _ = fs::remove_file(path);
    }
    result
}

/// Whether a file in `from` can be hardlinked into `to`: links a scratch file. Not on exFAT,
/// vfat and some network filesystems, nor across filesystems.
pub fn hardlinks_work(from: &Path, to: &Path) -> io::Result<()> {
    probe(from, to, "hardlink", |src, dst| fs::hard_link(src, dst))
}

/// Whether a file in `from` can be cloned into `to`: clones a scratch file. Not across
/// filesystems.
fn clones_work(from: &Path, to: &Path) -> io::Result<()> {
    probe(from, to, "reflink", |src, dst| {
        reflink_copy::reflink(src, dst)
    })
}

fn probe(
    from: &Path,
    to: &Path,
    what: &str,
    share: impl Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let name = format!("{what}-probe-{}", process::id());
    let (src, dst) = (from.join(&name), to.join(&name));
    let result = fs::write(&src, b"agent-hearth").and_then(|()| share(&src, &dst));
    for path in [&src, &dst] {
        let _ = fs::remove_file(path);
    }
    result
}

/// How build directories are seeded from goldens here.
#[derive(Debug, PartialEq, Eq)]
pub enum Seeding {
    /// Every file cloned (btrfs, XFS, APFS, ReFS).
    Clones,
    /// What cargo and rustc only replace hardlinked, the rest copied (ext4; Unix only).
    Hardlinks,
    /// Not at all, and why: worktrees build cold.
    Not(String),
}

/// How a build directory in `targets` would be seeded from a golden in `goldens`, found by trying
/// each way on scratch files.
pub fn seeding(goldens: &Path, targets: &Path) -> Seeding {
    let no_clones = match clones_work(goldens, targets) {
        Ok(()) => return Seeding::Clones,
        Err(err) => err,
    };
    if !cfg!(unix) {
        return Seeding::Not(format!(
            "the state directory's filesystem can't clone files ({no_clones}): seeding needs \
             ReFS or a Dev Drive"
        ));
    }
    match hardlinks_work(goldens, targets) {
        Ok(()) => Seeding::Hardlinks,
        Err(no_links) => Seeding::Not(format!(
            "the state directory can neither clone files ({no_clones}) nor hardlink them from \
             the goldens into the build directories ({no_links})"
        )),
    }
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
        // None left, the current one found changed in place: say so rather than that none
        // was ever built.
        if let Some(current) = golden::current(location)?
            && let Some(why) = golden::damaged(location, &current.id)
        {
            return Ok(Outcome::Damaged(damaged_golden(&current.id, &why)));
        }
        return Ok(Outcome::NoGolden);
    };
    let golden_dir = golden::dir(location, &golden.id);
    let targets = target.parent().context("target directory has no parent")?;
    fs::create_dir_all(targets).with_context(|| format!("creating {}", targets.display()))?;
    let links = match seeding(&golden_dir, targets) {
        Seeding::Clones => false,
        Seeding::Hardlinks => true,
        Seeding::Not(why) => return Ok(Outcome::Cold(why)),
    };
    // A file written in place through a hardlink changed the golden for every worktree sharing
    // it, and cargo, seeing its fingerprint fresh, never rebuilds it.
    if let Some(why) = damage(&golden_dir, links)? {
        golden::mark_damaged(location, &golden.id, &why)?;
        return Ok(Outcome::Damaged(damaged_golden(&golden.id, &why)));
    }
    // Before the build directory appears, so no build in it sees the worktree's mtimes unsynced.
    let (unchanged, sources) = sync_mtimes(
        &golden_dir.join("src"),
        &location.worktree,
        check::source_files(location)?,
    )?;

    let staging = target.with_extension(format!("tmp-{}", process::id()));
    let from = golden_dir.join("target");
    let seeded = if links {
        let manifest = load_manifest(&golden_dir).unwrap_or_default();
        link_tree(&from, &staging, &manifest)
            .map(|linked| (linked.files, linked.bytes, Some(linked.copied)))
    } else {
        clone_tree(&from, &staging).map(|(files, bytes)| (files, bytes, None))
    };
    let (files, bytes, copied) = seeded.inspect_err(|_| drop(fs::remove_dir_all(&staging)))?;
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
        copied,
        unchanged,
        sources,
    }))
}

/// What to say of the golden `id`, found changed in place (`why`).
fn damaged_golden(id: &str, why: &str) -> String {
    format!(
        "golden {id}: {why}. It seeds nothing until it is rebuilt (`agent-hearth golden \
         refresh`); worktrees seeded from it share those files, and `agent-hearth cargo clean` \
         in one seeds it again"
    )
}

/// [`adopt`], or an empty build directory when there is no golden to seed it from, or no way to:
/// the build then starts cold, as plain cargo would, rather than fail.
pub fn adopt_or_start_cold(location: &Location) -> Result<Outcome> {
    let outcome = adopt(location)?;
    if let Outcome::NoGolden | Outcome::Cold(_) | Outcome::Damaged(_) = outcome {
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
        } else if !kind.is_file() {
            // A FIFO, socket or device a test left: opening a FIFO waits for a writer forever.
            continue;
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

/// What [`link_tree`] did: every file, their bytes, and of those the bytes it copied.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Linked {
    pub files: u64,
    pub bytes: u64,
    pub copied: u64,
}

/// Where the filesystem cannot clone files (ext4): seeds `to` from the golden's build directory
/// `from` by hardlinking the files in `manifest` (see [`linkable`]) that are as the golden made
/// them, and copying the rest, keeping mtimes and permissions; FIFOs, sockets and devices are
/// left out. A hardlinked file is the golden's own: what keeps the golden as built is that cargo
/// and rustc delete or rename over these files before writing them, traced with strace
/// (`bench/ext4-writes-ruff-tay1.txt`). They cannot be made read-only to catch a write in place:
/// rustc refuses to replace a read-only output ("is not writeable -- check its permissions").
pub fn link_tree(from: &Path, to: &Path, manifest: &Manifest) -> Result<Linked> {
    let mut linked = Linked::default();
    link_dir(from, to, Path::new(""), manifest, &mut linked)?;
    Ok(linked)
}

fn link_dir(
    from: &Path,
    to: &Path,
    rel: &Path,
    manifest: &Manifest,
    linked: &mut Linked,
) -> Result<()> {
    fs::create_dir(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        let rel = rel.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            link_dir(&src, &dst, &rel, manifest, linked)?;
        } else if kind.is_symlink() {
            copy_symlink(&src, &dst)?;
        } else if kind.is_file() {
            let metadata = entry.metadata()?;
            let as_built = manifest
                .get(rel.to_string_lossy().as_ref())
                .is_some_and(|stamp| Stamp::of(&metadata).is_ok_and(|now| now == *stamp));
            if as_built {
                fs::hard_link(&src, &dst)
                    .with_context(|| format!("hardlinking {}", src.display()))?;
            } else {
                fs::copy(&src, &dst).with_context(|| format!("copying {}", src.display()))?;
                set_mtime(&dst, metadata.modified()?)?;
                linked.copied += metadata.len();
            }
            linked.files += 1;
            linked.bytes += metadata.len();
        }
    }
    Ok(())
}

/// In a golden's directory: the files of its build directory that worktrees may share by
/// hardlinks where nothing clones, each as it was when the golden was made.
pub const MANIFEST: &str = "linked.json";

/// What a golden's build directory may share by hardlinks, by path relative to it.
pub type Manifest = BTreeMap<String, Stamp>;

/// How a file was: its inode, size and mtime (nanoseconds since the epoch). A file written in
/// place keeps its inode and gets another size or mtime; one replaced (renamed over, or deleted
/// and written anew) gets another inode. Not ctime: making each hardlink changes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    ino: u64,
    size: u64,
    mtime: u64,
}

impl Stamp {
    fn of(metadata: &fs::Metadata) -> Result<Self> {
        let mtime = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
            });
        Ok(Self {
            ino: inode(metadata),
            size: metadata.len(),
            mtime,
        })
    }
}

#[cfg(unix)]
fn inode(metadata: &fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::ino(metadata)
}

#[cfg(not(unix))]
fn inode(_metadata: &fs::Metadata) -> u64 {
    0
}

/// The files of the build directory `target` that cargo and rustc only ever replace or delete,
/// never write in place, by their own records: in a profile directory (one with cargo's
/// `.fingerprint`), the outputs rustc's dep-info files in `deps/` and in a build script's
/// directory name (libraries, programs, test binaries, build script programs; a program or shared
/// library only if LLD linked it, see [`replaced_by_its_linker`]), cargo's hardlinks
/// of those (a program or library uplifted into the profile directory, `build-script-build`), and
/// incremental session files. Never metadata (`.rmeta`: rustc gave some a new mtime through the
/// link, `bench/ext4-ruff-tay1.txt`) or dep-info (`.d`, written in place, as cargo's fingerprints
/// are), nor what a build script or test wrote (`out/`, `tmp/`, a
/// library a build script copied beside the programs), nor anything unknown.
pub fn linkable(target: &Path) -> Result<Manifest> {
    let mut files = Vec::new();
    regular_files(target, Path::new(""), &mut files)?;
    let mut profiles = Vec::new();
    for entry in fs::read_dir(target).with_context(|| format!("reading {}", target.display()))? {
        let dir = entry?.path();
        if !dir.is_dir() {
            continue;
        }
        // `debug`, or a target triple's `<triple>/debug`.
        let children = fs::read_dir(&dir).into_iter().flatten().flatten();
        for candidate in std::iter::once(dir.clone()).chain(children.map(|entry| entry.path())) {
            if candidate.join(".fingerprint").is_dir() {
                profiles.push(candidate.strip_prefix(target)?.to_path_buf());
            }
        }
    }
    // rustc's outputs, as its dep-info names them, each beside its `.d` file.
    let mut outputs = BTreeSet::new();
    for profile in &profiles {
        let mut dirs = vec![profile.join("deps")];
        if let Ok(builds) = fs::read_dir(target.join(profile).join("build")) {
            dirs.extend(
                builds
                    .flatten()
                    .map(|entry| profile.join("build").join(entry.file_name())),
            );
        }
        for dir in dirs {
            let Ok(entries) = fs::read_dir(target.join(&dir)) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "d")
                    && let Ok(text) = fs::read_to_string(&path)
                {
                    for name in dep_info_outputs(&text) {
                        outputs.insert(dir.join(name));
                    }
                }
            }
        }
    }
    // Metadata, and dep-info (rustc names its `.d` file among its outputs, and writes it in
    // place).
    let never = |rel: &Path| {
        rel.extension()
            .is_some_and(|ext| ext == "rmeta" || ext == "d")
    };
    // A linker's output (a program or shared library), only from a linker that replaces it.
    let replaced = |rel: &Path| {
        let linked = rel
            .extension()
            .is_none_or(|ext| ext == "so" || ext == "dylib");
        !linked || replaced_by_its_linker(&target.join(rel))
    };
    let outputs: BTreeSet<PathBuf> = outputs
        .into_iter()
        .filter(|rel| !never(rel) && replaced(rel))
        .collect();
    let inodes: BTreeSet<u64> = files
        .iter()
        .filter(|(rel, _)| outputs.contains(rel))
        .map(|(_, metadata)| inode(metadata))
        .collect();
    let mut manifest = Manifest::new();
    for (rel, metadata) in &files {
        let Some(profile) = profiles.iter().find(|profile| rel.starts_with(profile)) else {
            continue;
        };
        let name = rel.file_name().unwrap_or_default().to_string_lossy();
        if never(rel) || name.starts_with('.') {
            continue;
        }
        let within: Vec<_> = rel.strip_prefix(profile)?.iter().collect();
        // <profile>/incremental/<crate>/<session>/<file>
        let session = within.len() == 4 && within[0] == "incremental";
        if outputs.contains(rel) || session || inodes.contains(&inode(metadata)) {
            manifest.insert(rel.to_string_lossy().into_owned(), Stamp::of(metadata)?);
        }
    }
    Ok(manifest)
}

/// Whether the linker that wrote the program or shared library `path` is one traced to delete or
/// rename over its output rather than write it in place: LLD (rustc's own on x86_64 Linux, and
/// `ld.lld`), which names itself in the `.comment` section (`Linker: LLD 21.1.8`). mold renames an
/// existing output aside and writes into it through any hardlink
/// (`bench/ext4-writes-ruff-mold-tay2.txt`), so what it linked is copied, as is what no
/// `.comment` names a linker for.
fn replaced_by_its_linker(path: &Path) -> bool {
    elf_comment(path).is_some_and(|comment| comment.contains("Linker: LLD"))
}

/// The `.comment` section of the ELF file `path` (64-bit, little-endian), if it has one.
fn elf_comment(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(path).ok()?;
    let mut header = [0u8; 64];
    file.read_exact(&mut header).ok()?;
    if &header[..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 {
        return None;
    }
    let u16_at = |b: &[u8], at: usize| u16::from_le_bytes([b[at], b[at + 1]]);
    let u32_at = |b: &[u8], at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    let u64_at = |b: &[u8], at: usize| {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&b[at..at + 8]);
        u64::from_le_bytes(bytes)
    };
    let (offset, size, count, names_at) = (
        u64_at(&header, 0x28),
        usize::from(u16_at(&header, 0x3a)),
        usize::from(u16_at(&header, 0x3c)),
        usize::from(u16_at(&header, 0x3e)),
    );
    if size < 64 || count == 0 || names_at >= count {
        return None;
    }
    let mut sections = vec![0u8; size * count];
    file.seek(SeekFrom::Start(offset)).ok()?;
    file.read_exact(&mut sections).ok()?;
    // (name's offset among the section names, file offset, size)
    let section = |n: usize| {
        let header = &sections[n * size..];
        (
            u32_at(header, 0) as usize,
            u64_at(header, 0x18),
            u64_at(header, 0x20),
        )
    };
    let mut read = |offset: u64, size: u64| {
        let mut bytes = vec![0u8; usize::try_from(size).ok().filter(|size| *size <= 1 << 20)?];
        file.seek(SeekFrom::Start(offset)).ok()?;
        file.read_exact(&mut bytes).ok()?;
        Some(bytes)
    };
    let (_, at, len) = section(names_at);
    let names = read(at, len)?;
    for n in 0..count {
        let (name, at, len) = section(n);
        let Some(rest) = names.get(name..) else {
            continue;
        };
        if rest.split(|byte| *byte == 0).next() == Some(b".comment") {
            let comment = read(at, len)?;
            return Some(String::from_utf8_lossy(&comment).replace('\0', " "));
        }
    }
    None
}

/// The file names of the outputs a rustc dep-info file names (`/t/debug/deps/liba-1f.rlib:
/// src/lib.rs …`), its sources' own empty rules (`src/lib.rs:`) among them, which name no file
/// beside it.
fn dep_info_outputs(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        // The target ends at the first `:` followed by a space or the line's end; spaces in it
        // are escaped.
        let bytes = line.as_bytes();
        let end = (0..bytes.len())
            .find(|&i| bytes[i] == b':' && bytes.get(i + 1).is_none_or(|next| *next == b' '));
        if let Some(end) = end {
            let target = line[..end].replace("\\ ", " ");
            if let Some(name) = Path::new(&target).file_name() {
                names.push(name.to_string_lossy().into_owned());
            }
        }
    }
    names
}

fn regular_files(dir: &Path, rel: &Path, files: &mut Vec<(PathBuf, fs::Metadata)>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            regular_files(&entry.path(), &rel.join(entry.file_name()), files)?;
        } else if kind.is_file() {
            files.push((rel.join(entry.file_name()), entry.metadata()?));
        }
    }
    Ok(())
}

/// Records what the golden in `golden_dir` may share by hardlinks ([`linkable`]), as built.
pub fn record_manifest(golden_dir: &Path) -> Result<()> {
    let manifest = linkable(&golden_dir.join("target"))?;
    golden::write_atomically(&golden_dir.join(MANIFEST), &serde_json::to_vec(&manifest)?)
}

fn load_manifest(golden_dir: &Path) -> Option<Manifest> {
    serde_json::from_slice(&fs::read(golden_dir.join(MANIFEST)).ok()?).ok()
}

/// Why the golden in `golden_dir` must not seed a build directory, if it must not: a file it may
/// share by hardlinks was written in place since it was built (the same inode with another size
/// or mtime), as `cp`, `strip`, `objcopy` or a linker that rewrites its output would do through
/// any worktree's link. With `required`, where seeding would hardlink, also when it has no
/// manifest to check (built before Agent Hearth kept one). A file written in place with its size
/// and mtime kept is not seen.
pub fn damage(golden_dir: &Path, required: bool) -> Result<Option<String>> {
    let Some(manifest) = load_manifest(golden_dir) else {
        return Ok(required.then(|| "it has no record of its files to check them by".to_owned()));
    };
    let target = golden_dir.join("target");
    let mut changed = Vec::new();
    for (rel, stamp) in &manifest {
        match fs::symlink_metadata(target.join(rel)) {
            Ok(metadata) => {
                let now = Stamp::of(&metadata)?;
                if now.ino == stamp.ino && now != *stamp {
                    changed.push(rel.as_str());
                }
            }
            // Replaced or deleted by a build in the golden itself, as cargo does.
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("reading {rel}")),
        }
    }
    if changed.is_empty() {
        return Ok(None);
    }
    let more = if changed.len() > 3 { ", …" } else { "" };
    Ok(Some(format!(
        "{} of its files were written in place since it was built ({}{more})",
        changed.len(),
        changed[..changed.len().min(3)].join(", ")
    )))
}

/// How many files the golden in `golden_dir` may share by hardlinks, if it recorded them.
pub fn manifest_len(golden_dir: &Path) -> Option<usize> {
    load_manifest(golden_dir).map(|manifest| manifest.len())
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
        if crate::testing::no_reflinks(&s) {
            return;
        }
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

    #[cfg(unix)]
    fn inode_of(path: &Path) -> u64 {
        std::os::unix::fs::MetadataExt::ino(&fs::metadata(path).unwrap())
    }

    /// A minimal ELF file (64-bit, little-endian) whose `.comment` section is `comment`.
    fn elf(comment: &str) -> Vec<u8> {
        let names = b"\0.shstrtab\0.comment\0";
        let mut bytes = vec![0u8; 64];
        bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        bytes.extend_from_slice(names);
        let comment_at = bytes.len();
        bytes.extend_from_slice(comment.as_bytes());
        bytes.push(0);
        let sections_at = bytes.len();
        // Null, .shstrtab, .comment: name, type, flags, address, offset, size, and the rest.
        for (name, kind, at, len) in [
            (0u32, 0u32, 0usize, 0usize),
            (1, 3, 64, names.len()),
            (11, 1, comment_at, comment.len() + 1),
        ] {
            bytes.extend_from_slice(&name.to_le_bytes());
            bytes.extend_from_slice(&kind.to_le_bytes());
            bytes.extend_from_slice(&[0u8; 16]);
            bytes.extend_from_slice(&(at as u64).to_le_bytes());
            bytes.extend_from_slice(&(len as u64).to_le_bytes());
            bytes.extend_from_slice(&[0u8; 24]);
        }
        bytes[0x28..0x30].copy_from_slice(&(sections_at as u64).to_le_bytes());
        bytes[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
        bytes[0x3c..0x3e].copy_from_slice(&3u16.to_le_bytes());
        bytes[0x3e..0x40].copy_from_slice(&1u16.to_le_bytes());
        bytes
    }

    #[test]
    fn a_linker_output_is_hardlinked_only_from_lld() {
        let s = Sandbox::new();
        for (name, comment, replaced) in [
            (
                "lld",
                "GCC: (GNU) 14.3.0\0Linker: LLD 21.1.8\0rustc version 1.98.0",
                true,
            ),
            (
                "mold",
                "GCC: (GNU) 14.3.0\0mold 2.41.0 (compatible with GNU ld)",
                false,
            ),
            (
                "wild",
                "Wild 0.9.0 non-git-build (compatible with GNU linkers)",
                false,
            ),
            ("gcc", "GCC: (GNU) 14.3.0", false),
        ] {
            fs::write(s.path(name), elf(comment)).unwrap();
            assert_eq!(replaced_by_its_linker(&s.path(name)), replaced, "{name}");
        }
        s.write("text", "not an ELF file");
        assert!(!replaced_by_its_linker(&s.path("text")));
        assert_eq!(elf_comment(&s.path("text")), None);
    }

    #[cfg(unix)]
    #[test]
    fn only_rustc_s_outputs_cargo_s_links_to_them_and_sessions_are_hardlinked() {
        let s = Sandbox::new();
        let t = |rel: &str| s.path(&format!("t/{rel}"));
        for profile in ["debug", "x86_64-unknown-linux-gnu/debug"] {
            fs::create_dir_all(t(&format!("{profile}/.fingerprint/tiny-0123"))).unwrap();
        }
        for rel in [
            "debug/deps/tiny-0123",
            "debug/deps/libtiny-0123.rlib",
            "debug/deps/libtiny-0123.rmeta",
            "debug/deps/libbundled.so",
            "debug/build/tiny-0123/build_script_build-0123",
            "debug/build/tiny-0456/out/libnative.a",
            "debug/build/tiny-0456/output",
            "debug/incremental/tiny-0123/s-abc-def/dep-graph.bin",
            "debug/incremental/tiny-0123/s-abc-def/metadata.rmeta",
            "debug/incremental/tiny-0123/s-abc-def.lock",
            "debug/libvendored.so",
            "debug/.cargo-lock",
            "debug/.fingerprint/tiny-0123/lib-tiny",
            "x86_64-unknown-linux-gnu/debug/deps/libtiny-0123.rlib",
            "tmp/db",
            "tmp/deps/libfoo.so",
            "other/thing",
            "CACHEDIR.TAG",
        ] {
            s.write(&format!("t/{rel}"), "x");
        }
        // Linker outputs: by LLD, which replaces them, and by mold, which writes them in place.
        let lld = elf("Linker: LLD 21.1.8");
        for rel in [
            "debug/deps/tiny-0123",
            "debug/build/tiny-0123/build_script_build-0123",
        ] {
            fs::write(t(rel), &lld).unwrap();
        }
        fs::write(t("debug/deps/molded-0456"), elf("mold 2.41.0")).unwrap();
        s.write(
            "t/debug/deps/molded-0456.d",
            "/pin/target/debug/deps/molded-0456: src/main.rs\n",
        );
        fs::hard_link(t("debug/deps/molded-0456"), t("debug/molded")).unwrap();
        // rustc's dep-info names its outputs, at the path the golden built at.
        s.write(
            "t/debug/deps/tiny-0123.d",
            "/pin/target/debug/deps/tiny-0123: src/lib.rs\n\n/pin/target/debug/deps/tiny-0123.d: src/lib.rs\n\nsrc/lib.rs:\n",
        );
        s.write(
            "t/debug/deps/libtiny-0123.d",
            "/pin/target/debug/deps/libtiny-0123.rlib: src/lib.rs\n/pin/target/debug/deps/libtiny-0123.rmeta: src/lib.rs\n\n# env-dep:CARGO_PKG_NAME=tiny\n",
        );
        s.write(
            "t/debug/build/tiny-0123/build_script_build-0123.d",
            "/pin/target/debug/build/tiny-0123/build_script_build-0123: build.rs\n",
        );
        s.write(
            "t/x86_64-unknown-linux-gnu/debug/deps/libtiny-0123.d",
            "/pin/target/x86_64-unknown-linux-gnu/debug/deps/libtiny-0123.rlib: src/lib.rs\n",
        );
        s.write(
            "t/tmp/deps/libfoo.d",
            "/pin/target/tmp/deps/libfoo.so: x.rs\n",
        );
        // cargo's own links: a program uplifted, the build script under its plain name.
        fs::hard_link(t("debug/deps/tiny-0123"), t("debug/tiny")).unwrap();
        fs::hard_link(
            t("debug/build/tiny-0123/build_script_build-0123"),
            t("debug/build/tiny-0123/build-script-build"),
        )
        .unwrap();
        let manifest = linkable(&s.path("t")).unwrap();
        let linked = link_tree(&s.path("t"), &s.path("w"), &manifest).unwrap();
        assert!(linked.copied < linked.bytes, "{linked:?}");
        for (rel, expected) in [
            ("debug/deps/tiny-0123", true),
            ("debug/tiny", true),
            ("debug/deps/molded-0456", false),
            ("debug/molded", false),
            ("debug/deps/libtiny-0123.rlib", true),
            (
                "x86_64-unknown-linux-gnu/debug/deps/libtiny-0123.rlib",
                true,
            ),
            ("debug/build/tiny-0123/build_script_build-0123", true),
            ("debug/build/tiny-0123/build-script-build", true),
            ("debug/incremental/tiny-0123/s-abc-def/dep-graph.bin", true),
            // Metadata, dep-info, cargo's own records, locks.
            ("debug/deps/libtiny-0123.rmeta", false),
            (
                "debug/incremental/tiny-0123/s-abc-def/metadata.rmeta",
                false,
            ),
            ("debug/incremental/tiny-0123/s-abc-def.lock", false),
            ("debug/deps/tiny-0123.d", false),
            ("debug/.cargo-lock", false),
            ("debug/.fingerprint/tiny-0123/lib-tiny", false),
            // What build scripts and tests write: a library a build script put beside rustc's
            // outputs or the programs, a build script's output, a test's temp directory.
            ("debug/deps/libbundled.so", false),
            ("debug/libvendored.so", false),
            ("debug/build/tiny-0456/out/libnative.a", false),
            ("debug/build/tiny-0456/output", false),
            ("tmp/db", false),
            ("tmp/deps/libfoo.so", false),
            ("other/thing", false),
            ("CACHEDIR.TAG", false),
        ] {
            let same = inode_of(&t(rel)) == inode_of(&s.path(&format!("w/{rel}")));
            assert_eq!(same, expected, "{rel}");
        }
        // A file written in place since the manifest was made is copied.
        fs::write(t("debug/deps/libtiny-0123.rlib"), "rewritten").unwrap();
        link_tree(&s.path("t"), &s.path("w2"), &manifest).unwrap();
        assert_ne!(
            inode_of(&t("debug/deps/libtiny-0123.rlib")),
            inode_of(&s.path("w2/debug/deps/libtiny-0123.rlib"))
        );
    }

    #[test]
    fn dep_info_names_its_outputs() {
        assert_eq!(
            dep_info_outputs(
                "/t/debug/deps/liba-1f.rlib: /w/src/lib.rs /w/src/x.rs\n/w/src/lib.rs:\n# env-dep:X=1\n/t/debug/deps/my\\ prog-2e: a.rs\n"
            ),
            ["liba-1f.rlib", "lib.rs", "my prog-2e"]
        );
    }

    /// Seeds `wt`'s build directory from `repo`'s current golden by hardlinks, whatever this
    /// filesystem can do, as `adopt` does where nothing clones.
    #[cfg(unix)]
    fn adopt_by_hardlinks(wt: &Location) -> Result<Outcome> {
        let golden = golden::current(wt)?.unwrap();
        let golden_dir = golden::dir(wt, &golden.id);
        if let Some(why) = damage(&golden_dir, true)? {
            golden::mark_damaged(wt, &golden.id, &why)?;
            return Ok(Outcome::Damaged(why));
        }
        sync_mtimes(
            &golden_dir.join("src"),
            &wt.worktree,
            check::source_files(wt)?,
        )?;
        let target = target_dir(wt);
        fs::create_dir_all(target.parent().unwrap())?;
        let manifest = load_manifest(&golden_dir).unwrap_or_default();
        link_tree(&golden_dir.join("target"), &target, &manifest)?;
        write_record(wt, Some(&golden.id))?;
        Ok(Outcome::Already)
    }

    #[cfg(unix)]
    #[test]
    fn a_golden_written_in_place_seeds_nothing_until_it_is_rebuilt() {
        use std::io::Write;
        let s = Sandbox::new();
        s.crate_repo("repo");
        let repo = s.location("repo");
        let (golden, _) = golden::refresh(&repo, "main").unwrap();
        let golden_dir = golden::dir(&repo, &golden.id);
        assert_eq!(damage(&golden_dir, true).unwrap(), None);
        s.run(
            "repo",
            "git",
            &["worktree", "add", "-q", "--detach", "../wt1"],
        );
        let wt1 = s.location("wt1");
        adopt_by_hardlinks(&wt1).unwrap();
        // The library, hardlinked: rewritten in place through the worktree's link, as `cp`, GNU
        // `strip` and `objcopy` write a file with more than one link.
        let deps = target_dir(&wt1).join("debug/deps");
        let binary = fs::read_dir(&deps)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "rlib"))
            .unwrap();
        let golden_binary = golden_dir
            .join("target")
            .join(binary.strip_prefix(target_dir(&wt1)).unwrap());
        assert_eq!(inode_of(&binary), inode_of(&golden_binary));
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&binary)
            .unwrap()
            .write_all(b"rewritten in place")
            .unwrap();

        let why = damage(&golden_dir, false).unwrap().expect("damage seen");
        assert!(
            why.starts_with("1 of its files were written in place"),
            "{why}"
        );
        s.run(
            "repo",
            "git",
            &["worktree", "add", "-q", "--detach", "../wt2"],
        );
        let wt2 = s.location("wt2");
        // Either way of seeding refuses it, and the build starts cold.
        let outcome = adopt_or_start_cold(&wt2).unwrap();
        assert!(matches!(outcome, Outcome::Damaged(_)), "{outcome:?}");
        assert!(target_dir(&wt2).is_dir());
        assert_eq!(seeded_from(&wt2).unwrap(), None);
        assert!(
            golden::finished(&repo).unwrap().is_empty(),
            "no longer chosen"
        );
        let report = crate::doctor::doctor(&repo).unwrap();
        let check = report.checks.iter().find(|c| c.name == "golden").unwrap();
        assert!(
            !check.ok && check.detail.contains("written in place"),
            "{check:?}"
        );

        let (again, built) = golden::refresh(&repo, "main").unwrap();
        assert!(built, "rebuilt");
        assert_eq!(again.id, golden.id);
        assert_eq!(damage(&golden_dir, true).unwrap(), None);
        assert_eq!(golden::finished(&repo).unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_build_directory_on_a_filesystem_that_cannot_hardlink_starts_cold() {
        use std::os::unix::fs::MetadataExt;
        let Ok(shm) = tempfile::tempdir_in("/dev/shm") else {
            eprintln!("skipped: no /dev/shm to put build directories on another filesystem");
            return;
        };
        let s = Sandbox::new();
        if fs::metadata(shm.path()).unwrap().dev() == fs::metadata(s.root()).unwrap().dev() {
            eprintln!("skipped: /dev/shm is on TMPDIR's filesystem");
            return;
        }
        s.crate_repo("repo");
        let repo = s.location("repo");
        golden::refresh(&repo, "main").unwrap();
        // The build directories on another filesystem than the goldens: neither clones nor
        // hardlinks reach them.
        std::os::unix::fs::symlink(shm.path(), repo.state_dir.join("targets")).unwrap();
        s.run("repo", "git", &["worktree", "add", "-q", "../wt"]);
        let wt = s.location("wt");
        let outcome = adopt_or_start_cold(&wt).unwrap();
        assert!(matches!(outcome, Outcome::Cold(_)), "{outcome:?}");
        assert!(target_dir(&wt).is_dir());
        let report = crate::doctor::doctor(&wt).unwrap();
        assert!(report.mode.starts_with("gate only"), "{}", report.mode);
        let check = report.checks.iter().find(|c| c.name == "seeding").unwrap();
        assert!(!check.ok, "{check:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_in_a_golden_is_left_out_not_waited_on() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let repo = s.location("repo");
        let (golden, _) = golden::refresh(&repo, "main").unwrap();
        let golden_target = golden::dir(&repo, &golden.id).join("target");
        let fifo = golden_target.join("debug/a-fifo");
        s.run("repo", "mkfifo", &[fifo.to_str().unwrap()]);
        let from = golden_target.clone();
        let to = s.path("linked");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let linked = link_tree(&from, &to, &Manifest::new()).map_err(|err| format!("{err:#}"));
            let cloned = reflinks_work(&from)
                .is_ok()
                .then(|| clone_tree(&from, &to.with_extension("clone")).map(drop))
                .map(|cloned| cloned.map_err(|err| format!("{err:#}")));
            let _ = tx.send((linked.map(drop), cloned));
        });
        let (linked, cloned) = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("seeding returns");
        assert_eq!(linked, Ok(()));
        assert!(cloned.as_ref().is_none_or(Result::is_ok), "{cloned:?}");
        assert!(!s.path("linked/debug/a-fifo").exists());
        assert!(s.path("linked/debug/deps").is_dir());
    }

    /// Path, content and mtime of every file under `dir`.
    #[cfg(unix)]
    fn manifest(dir: &Path) -> BTreeMap<PathBuf, (blake3::Hash, SystemTime)> {
        let mut files = BTreeMap::new();
        let mut dirs = vec![dir.to_path_buf()];
        while let Some(next) = dirs.pop() {
            for entry in fs::read_dir(&next).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    dirs.push(entry.path());
                } else if entry.file_type().unwrap().is_file() {
                    let path = entry.path();
                    let hash = blake3::hash(&fs::read(&path).unwrap());
                    files.insert(
                        path.strip_prefix(dir).unwrap().to_owned(),
                        (hash, mtime(&path)),
                    );
                }
            }
        }
        files
    }

    #[cfg(unix)]
    #[test]
    fn a_hardlinked_build_directory_leaves_the_golden_as_it_was() {
        use std::os::unix::fs::MetadataExt;
        let s = Sandbox::new();
        s.crate_repo("repo");
        s.write(
            "repo/build.rs",
            "fn main() {\n    println!(\"cargo::rerun-if-changed=build.rs\");\n}\n",
        );
        s.run("repo", "git", &["add", "."]);
        s.run("repo", "git", &["commit", "-q", "-m", "build script"]);
        let repo = s.location("repo");
        let (golden, _) = golden::refresh(&repo, "main").unwrap();
        let golden_dir = golden::dir(&repo, &golden.id);
        let golden_target = golden_dir.join("target");
        let before = manifest(&golden_target);
        s.run("repo", "git", &["worktree", "add", "-q", "../wt"]);
        let wt = s.location("wt");
        // `adopt` without reflinks, whatever this filesystem can do.
        sync_mtimes(
            &golden_dir.join("src"),
            &wt.worktree,
            check::source_files(&wt).unwrap(),
        )
        .unwrap();
        let target = target_dir(&wt);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        let linkable = load_manifest(&golden_dir).expect("a golden records its manifest");
        let linked = link_tree(&golden_target, &target, &linkable).unwrap();
        write_record(&wt, Some(&golden.id)).unwrap();
        assert!(linked.copied < linked.bytes, "{linked:?}");
        let rlib = fs::read_dir(target.join("debug/deps"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "rlib"))
            .unwrap();
        let golden_rlib = golden_target.join(rlib.strip_prefix(&target).unwrap());
        assert_eq!(
            fs::metadata(&rlib).unwrap().ino(),
            fs::metadata(&golden_rlib).unwrap().ino()
        );

        let build = |flags: &str| {
            for args in [&["clippy", "--all-targets"][..], &["test", "--no-run"]] {
                let mounts = wt.pinned().map(|(_, mounts)| mounts).unwrap_or_default();
                let mut cargo = cargo::command(
                    &wt.worktree,
                    &target,
                    Path::new(""),
                    wt.pin.as_deref(),
                    &mounts,
                    None,
                );
                if !flags.is_empty() {
                    cargo.env("RUSTFLAGS", flags);
                }
                let output = cargo.args(args).output().unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        };
        build("");
        s.write("wt/src/lib.rs", "pub fn one() -> u32 {\n    2\n}\n");
        build("");
        set_mtime(&s.path("wt/build.rs"), SystemTime::now()).unwrap();
        build("");
        build("--cfg agent_hearth_probe");
        // Content only: some files get a new mtime through the link (see `link_tree`).
        let content = |files: BTreeMap<PathBuf, (blake3::Hash, SystemTime)>| {
            files
                .into_iter()
                .map(|(path, (hash, _))| (path, hash))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            content(manifest(&golden_target)),
            content(before),
            "the golden's files are as built"
        );
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
