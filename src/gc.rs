//! Removes what no worktree needs any more, and reports the real disk use of what is left:
//! reflink clones share extents, so apparent sizes overstate it.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::adopt;
use crate::golden;
use crate::locate::Location;

/// Verdicts unused for this long are dropped; code that old rarely comes back.
const VERDICT_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
/// A staging directory this old belongs to an adopt that died.
const STAGING_AGE: Duration = Duration::from_secs(3600);

#[derive(Debug, Default, Serialize)]
pub struct Report {
    /// Build directories of worktrees that no longer exist.
    pub worktrees: u64,
    pub goldens: u64,
    pub verdicts: u64,
    /// Apparent bytes removed.
    pub freed: u64,
    /// Apparent bytes left in the state directory.
    pub remaining: u64,
    /// Real disk use of the state directory, where the filesystem can say.
    pub disk: Option<Disk>,
}

/// `btrfs filesystem du` for the state directory.
#[derive(Debug, Serialize)]
pub struct Disk {
    /// Every extent once per file that references it: the size without reflinks.
    pub referenced: u64,
    /// Extents no other file references.
    pub exclusive: u64,
    /// Shared extents, each counted once.
    pub shared: u64,
}

impl Disk {
    /// What the state directory really occupies.
    pub fn used(&self) -> u64 {
        self.exclusive + self.shared
    }
}

/// Removes the build directories of worktrees that are gone, every golden but the current one
/// and the newest others up to `keep` in all, and verdicts older than a month.
pub fn gc(location: &Location, keep: usize) -> Result<Report> {
    let mut report = Report::default();
    let state = &location.state_dir;
    for (id, path) in adopt::records(location)? {
        let alive = location
            .find_alike(&path)
            .is_ok_and(|found| found.worktree == path);
        if alive {
            continue;
        }
        report.freed += remove(&state.join("targets").join(&id))?;
        for leftover in [
            state.join("worktrees").join(format!("{id}.json")),
            state.join("settles").join(format!("{id}.json")),
            state.join("settles").join(format!("{id}.log")),
            state.join("settles").join(format!("{id}.lock")),
        ] {
            report.freed += remove(&leftover)?;
        }
        report.worktrees += 1;
    }
    for entry in entries(&state.join("targets"))? {
        let stale = entry.file_name().to_string_lossy().contains(".tmp-")
            && older_than(&entry.path(), STAGING_AGE);
        if stale {
            report.freed += remove(&entry.path())?;
        }
    }

    let refresh = golden::refresh_lock(location)?;
    refresh.lock().context("waiting for a golden refresh")?;
    let adopts = golden::gc_lock(location)?;
    adopts
        .lock()
        .context("waiting for adopts reading goldens")?;
    let current = golden::current(location)?.map(|golden| golden.id);
    let mut others: Vec<(SystemTime, String)> = golden::finished(location)?
        .into_iter()
        .filter(|golden| Some(&golden.id) != current.as_ref())
        .map(|golden| {
            let built = fs::metadata(golden::dir(location, &golden.id).join("golden.json"))
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (built, golden.id)
        })
        .collect();
    others.sort_by(|a, b| b.cmp(a));
    let keep_others = keep.saturating_sub(usize::from(current.is_some()));
    let kept: Vec<String> = current
        .into_iter()
        .chain(others.into_iter().take(keep_others).map(|(_, id)| id))
        .collect();
    for entry in entries(&state.join("goldens"))? {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Unfinished builds and staging go too: the refresh lock says nothing is writing them.
        if entry.file_type()?.is_dir() && !kept.contains(&name) {
            report.freed += remove(&entry.path())?;
            report.goldens += 1;
        }
    }
    drop((adopts, refresh));

    for entry in entries(&state.join("verdicts"))? {
        if older_than(&entry.path(), VERDICT_AGE) {
            report.freed += remove(&entry.path())?;
            report.verdicts += 1;
        }
    }
    report.remaining = size(state);
    report.disk = btrfs_du(state);
    Ok(report)
}

fn entries(dir: &Path) -> Result<Vec<fs::DirEntry>> {
    match fs::read_dir(dir) {
        Ok(entries) => entries
            .collect::<Result<_, _>>()
            .with_context(|| format!("reading {}", dir.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err).with_context(|| format!("reading {}", dir.display())),
    }
}

fn older_than(path: &Path, age: Duration) -> bool {
    fs::symlink_metadata(path)
        .and_then(|metadata| metadata.modified())
        .is_ok_and(|modified| modified.elapsed().is_ok_and(|elapsed| elapsed > age))
}

/// Removes a file or directory tree and returns its apparent size; nothing there is not an error.
fn remove(path: &Path) -> Result<u64> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(0);
    };
    let bytes = size(path);
    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .with_context(|| format!("removing {}", path.display()))?;
    Ok(bytes)
}

/// Apparent size of everything under `path`, without following symlinks.
fn size(path: &Path) -> u64 {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !metadata.is_dir() {
        return metadata.len();
    }
    fs::read_dir(path)
        .map(|entries| entries.flatten().map(|entry| size(&entry.path())).sum())
        .unwrap_or(0)
}

/// `btrfs filesystem du`, which works unprivileged and counts shared extents once.
fn btrfs_du(path: &Path) -> Option<Disk> {
    let output = Command::new("btrfs")
        .args(["filesystem", "du", "-s", "--raw"])
        .arg(path)
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut numbers = stdout
        .lines()
        .last()?
        .split_whitespace()
        .map(str::parse::<u64>);
    Some(Disk {
        referenced: numbers.next()?.ok()?,
        exclusive: numbers.next()?.ok()?,
        shared: numbers.next()?.ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn;
    use crate::testing::Sandbox;

    #[test]
    fn removes_gone_worktrees_and_old_goldens_only() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let repo = s.location("repo");
        let mut goldens = Vec::new();
        for n in 2..=4 {
            s.write(
                "repo/src/lib.rs",
                &format!("pub fn n() -> u32 {{\n    {n}\n}}\n"),
            );
            s.run("repo", "git", &["commit", "-q", "-am", &format!("{n}")]);
            goldens.push(golden::refresh(&repo, "main").unwrap().0.id);
        }
        let (alive, _) = spawn::spawn(&repo, "alive", "main").unwrap();
        let (gone, _) = spawn::spawn(&repo, "gone", "main").unwrap();
        fs::remove_dir_all(&gone.worktree).unwrap();
        let staging = repo.state_dir.join("targets/crashed.tmp-1");
        fs::create_dir_all(&staging).unwrap();
        s.run(
            "repo",
            "touch",
            &["-d", "2 hours ago", staging.to_str().unwrap()],
        );

        let report = gc(&repo, 2).unwrap();
        assert_eq!((report.worktrees, report.goldens), (1, 1));
        assert!(report.freed > 0 && report.remaining > 0);
        assert!(adopt::target_dir(&alive).is_dir());
        assert!(!adopt::target_dir(&gone).exists() && !staging.exists());
        let left: Vec<bool> = goldens
            .iter()
            .map(|id| golden::dir(&repo, id).exists())
            .collect();
        assert_eq!(left, [false, true, true], "the oldest golden goes");
        assert_eq!(adopt::recorded_worktrees(&repo).unwrap(), [alive.worktree]);
    }
}
