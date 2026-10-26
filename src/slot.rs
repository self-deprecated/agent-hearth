//! The slot pool: worktrees at fixed paths that are reused, so each build directory only ever
//! builds at one path. Where builds cannot run at the pin (macOS, Windows, Linux without user
//! namespaces), `spawn` claims a free slot and moves it to the revision asked for, keeping its
//! build directory; removing the worktree frees its slot instead of deleting it.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::adopt::{self, Outcome};
use crate::golden;
use crate::hook::run;
use crate::locate::{Location, Vcs};

/// `slots/<n>.json`, beside the slot's worktree `slots/<n>/`.
#[derive(Default, Serialize, Deserialize)]
struct Slot {
    /// The worktree's name while an agent holds the slot; `None` while it is free.
    name: Option<String>,
}

fn dir(location: &Location) -> PathBuf {
    location.state_dir.join("slots")
}

/// Claims a free slot, or makes a new one, as the worktree `name` at `rev` (a revision of
/// `from`), then adopts it. A reused slot keeps its build directory.
pub fn claim(from: &Location, name: &str, rev: &str) -> Result<(Location, Outcome)> {
    let commit = golden::resolve(from, rev)?;
    let slots = dir(from);
    fs::create_dir_all(&slots).with_context(|| format!("creating {}", slots.display()))?;
    let _lock = lock(&slots)?;
    let n = match free(&slots, &[])? {
        Some((n, _)) => {
            reset(from.vcs, &slots.join(n.to_string()), name, &commit)?;
            n
        }
        None => {
            let n = (1..)
                .find(|n| !slots.join(n.to_string()).exists() && !record(&slots, *n).exists())
                .expect("a number is free");
            crate::spawn::create(from, name, &commit, &slots.join(n.to_string()))?;
            n
        }
    };
    let slot = Slot {
        name: Some(name.to_owned()),
    };
    golden::write_atomically(&record(&slots, n), &serde_json::to_vec(&slot)?)?;
    from.link_outside_dependencies(&slots.join(n.to_string()))?;
    let location = from.find_alike(&slots.join(n.to_string()))?;
    location.create_dirs()?;
    let outcome = adopt::adopt(&location)?;
    Ok((location, outcome))
}

/// Frees the slot `location` sits in. Returns false when it is not a slot. Its files and build
/// directory stay; a git slot with uncommitted changes is refused.
pub fn release(location: &Location) -> Result<bool> {
    let slots = dir(location);
    let Ok(canonical) = fs::canonicalize(&slots) else {
        return Ok(false);
    };
    let Some(n) = location
        .worktree
        .strip_prefix(&canonical)
        .ok()
        .and_then(|rest| rest.to_str()?.parse::<u32>().ok())
    else {
        return Ok(false);
    };
    let _lock = lock(&slots)?;
    let worktree = &location.worktree;
    match location.vcs {
        // The agent's changes stay in their commit; only the workspace's name goes.
        Vcs::Jj => {
            run(crate::locate::jj_command()
                .args(["workspace", "rename", &format!("slot-{n}")])
                .current_dir(worktree))?;
        }
        Vcs::Git => {
            refuse_changes(worktree)?;
            run(Command::new("git")
                .args(["switch", "--detach", "--quiet"])
                .current_dir(worktree))?;
        }
    }
    golden::write_atomically(&record(&slots, n), &serde_json::to_vec(&Slot::default())?)?;
    Ok(true)
}

/// Moves each free slot to `commit` (the new golden's) and builds it there as the golden was built,
/// so the next agent in it starts warm instead of building what changed since its last agent.
/// Returns the slots warmed. A slot being warmed holds `slots/<n>.lock`, and `claim` passes it by.
/// A slot that fails to warm (a full disk, say) is reported and skipped:
/// the golden is current either way.
pub fn warm(from: &Location, commit: &str) -> Result<Vec<u32>> {
    let slots = dir(from);
    let (mut tried, mut warmed) = (Vec::new(), Vec::new());
    if !slots.is_dir() {
        return Ok(warmed);
    }
    loop {
        let (n, _warming) = {
            let _lock = lock(&slots)?;
            match free(&slots, &tried)? {
                Some(found) => found,
                None => return Ok(warmed),
            }
        };
        tried.push(n);
        if let Err(err) = move_and_build(from, &slots.join(n.to_string()), commit) {
            eprintln!("agent-hearth: slot {n} not warmed: {err:#}");
            continue;
        }
        warmed.push(n);
    }
}

/// Moves the free slot at `path` to `commit` and runs the golden's builds there.
fn move_and_build(from: &Location, path: &Path, commit: &str) -> Result<()> {
    match from.vcs {
        Vcs::Jj => {
            for args in [&["workspace", "update-stale"][..], &["new", commit]] {
                run(crate::locate::jj_command().args(args).current_dir(path))?;
            }
        }
        Vcs::Git => {
            refuse_changes(path)?;
            run(Command::new("git")
                .args(["switch", "--detach", "--quiet", commit])
                .current_dir(path))?;
        }
    }
    let location = from.find_alike(path)?;
    let alone = crate::builds::packages_built_alone(&location);
    // What fails here failed in the golden too, which allowed it: main's own errors.
    golden::build_at(
        &location.worktree,
        &adopt::target_dir(&location),
        None,
        &[],
        &alone,
    )
    .map(drop)
}

/// Moves the free slot at `path` to `commit` as the worktree `name`. The checkout rewrites only
/// the files that differ, so cargo rebuilds only what changed, at the same path.
fn reset(vcs: Vcs, path: &Path, name: &str, commit: &str) -> Result<()> {
    match vcs {
        Vcs::Jj => {
            for args in [
                &["workspace", "update-stale"][..],
                &["workspace", "rename", name],
                &["new", commit],
            ] {
                run(crate::locate::jj_command().args(args).current_dir(path))?;
            }
        }
        Vcs::Git => {
            refuse_changes(path)?;
            run(Command::new("git")
                .args(["switch", "--quiet", "--create", name, commit])
                .current_dir(path))?;
        }
    }
    Ok(())
}

fn refuse_changes(worktree: &Path) -> Result<()> {
    let status = run(Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(worktree))?;
    if !status.trim().is_empty() {
        bail!(
            "{} has uncommitted changes: commit or discard them first",
            worktree.display()
        );
    }
    Ok(())
}

/// The lowest-numbered free slot, not in `except`, whose worktree still exists and that nobody is
/// warming, with its warming lock taken. Called with the slots lock held.
fn free(slots: &Path, except: &[u32]) -> Result<Option<(u32, File)>> {
    let mut free = Vec::new();
    for entry in fs::read_dir(slots).with_context(|| format!("reading {}", slots.display()))? {
        let path = entry?.path();
        let Some(n) = path
            .file_name()
            .and_then(|name| name.to_str()?.strip_suffix(".json")?.parse::<u32>().ok())
        else {
            continue;
        };
        let slot: Slot = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("reading {}", path.display()))?;
        if slot.name.is_none() && slots.join(n.to_string()).is_dir() && !except.contains(&n) {
            free.push(n);
        }
    }
    free.sort();
    for n in free {
        let warming = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(slots.join(format!("{n}.lock")))
            .context("opening a slot's warming lock")?;
        match warming.try_lock() {
            Ok(()) => return Ok(Some((n, warming))),
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(err)) => return Err(err).context("locking a slot"),
        }
    }
    Ok(None)
}

/// Whether `path` is a slot no agent holds: no one's worktree.
pub fn is_free(location: &Location, path: &Path) -> bool {
    let slots = dir(location);
    // Worktree paths are canonical (macOS's /var is /private/var).
    let canonical = fs::canonicalize(&slots).unwrap_or_else(|_| slots.clone());
    let Some(n) = path
        .strip_prefix(&canonical)
        .ok()
        .and_then(|rest| rest.to_str()?.parse::<u32>().ok())
    else {
        return false;
    };
    fs::read(record(&slots, n))
        .ok()
        .and_then(|json| serde_json::from_slice::<Slot>(&json).ok())
        .is_some_and(|slot| slot.name.is_none())
}

fn record(slots: &Path, n: u32) -> PathBuf {
    slots.join(format!("{n}.json"))
}

fn lock(slots: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(slots.join("lock"))
        .context("opening the slots lock")?;
    file.lock().context("waiting for the slots lock")?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Sandbox;

    #[test]
    fn a_released_jj_slot_is_reused_with_its_build_directory() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        s.run("repo", "jj", &["git", "init", "--colocate"]);
        let repo = s.location("repo");
        golden::refresh(&repo, "main").unwrap();

        let (one, outcome) = claim(&repo, "one", "main").unwrap();
        assert!(matches!(outcome, Outcome::Adopted(_)), "{outcome:?}");
        let (two, _) = claim(&repo, "two", "main").unwrap();
        assert_ne!(one.worktree, two.worktree);
        let marker = adopt::target_dir(&one).join("marker");
        fs::write(&marker, "built here").unwrap();
        fs::write(one.worktree.join("src/lib.rs"), "pub fn changed() {}\n").unwrap();

        assert!(release(&one).unwrap());
        assert!(!release(&repo).unwrap());
        let listed: Vec<_> = crate::status::status(&repo)
            .unwrap()
            .into_iter()
            .map(|worktree| worktree.path)
            .collect();
        assert_eq!(
            listed,
            [repo.worktree.clone(), two.worktree.clone()],
            "a free slot is no one's"
        );
        let (three, outcome) = claim(&repo, "three", "main").unwrap();
        assert_eq!(three.worktree, one.worktree);
        assert!(matches!(outcome, Outcome::Already), "{outcome:?}");
        assert!(marker.is_file());
        let names = s.run(
            "repo",
            "jj",
            &["workspace", "list", "-T", "name ++ \"\\n\""],
        );
        assert!(names.contains("three") && !names.contains("one"), "{names}");
        // Back at main, while the first agent's change stays in its own commit.
        let lib = fs::read_to_string(three.worktree.join("src/lib.rs")).unwrap();
        assert!(!lib.contains("changed"), "{lib}");
        let all = s.run(
            "repo",
            "jj",
            &["log", "-r", "all()", "--no-graph", "-p", "-T", "''"],
        );
        assert!(all.contains("pub fn changed"), "{all}");
    }

    #[test]
    fn free_slots_are_warmed_at_a_new_golden_and_claim_passes_a_warming_slot_by() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        s.run("repo", "jj", &["git", "init", "--colocate"]);
        let repo = s.location("repo");
        golden::refresh(&repo, "main").unwrap();
        let (one, _) = claim(&repo, "one", "main").unwrap();
        let (_two, _) = claim(&repo, "two", "main").unwrap();
        assert!(release(&one).unwrap());

        s.write("repo/src/lib.rs", "pub fn two() -> u32 {\n    2\n}\n");
        s.run("repo", "jj", &["commit", "-m", "two"]);
        s.run("repo", "jj", &["bookmark", "set", "main", "-r", "@-"]);
        let (golden, _) = golden::refresh(&repo, "main").unwrap();
        assert_eq!(
            warm(&repo, &golden.commit).unwrap(),
            [1],
            "only the free slot"
        );
        let lib = fs::read_to_string(one.worktree.join("src/lib.rs")).unwrap();
        assert!(lib.contains("fn two"), "{lib}");
        // Built there: the next build in the slot has nothing to do.
        let output = crate::cargo::command(
            &one.worktree,
            &adopt::target_dir(&one),
            Path::new(""),
            None,
            &[],
        )
        .args(["test", "--no-run", "--message-format=json", "--quiet"])
        .output()
        .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("\"fresh\":true") && !stdout.contains("\"fresh\":false"),
            "{stdout}"
        );

        // While slot 1 is being warmed, a new agent gets a new slot.
        let slots = dir(&repo);
        let (_, warming) = {
            let _lock = lock(&slots).unwrap();
            free(&slots, &[]).unwrap().expect("slot 1 is free")
        };
        let (three, _) = claim(&repo, "three", "main").unwrap();
        assert!(
            three.worktree.ends_with("slots/3"),
            "{}",
            three.worktree.display()
        );
        drop(warming);
        let (four, _) = claim(&repo, "four", "main").unwrap();
        assert_eq!(four.worktree, one.worktree);
    }

    #[test]
    fn a_git_slot_with_uncommitted_changes_stays_claimed() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let repo = s.location("repo");
        let (one, _) = claim(&repo, "one", "main").unwrap();
        fs::write(one.worktree.join("new.txt"), "work").unwrap();
        let err = release(&one).expect_err("refused");
        assert!(err.to_string().contains("uncommitted"), "{err}");

        fs::remove_file(one.worktree.join("new.txt")).unwrap();
        assert!(release(&one).unwrap());
        let (two, _) = claim(&repo, "two", "main").unwrap();
        assert_eq!(two.worktree, one.worktree);
        let branch = run(Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(&two.worktree))
        .unwrap();
        assert_eq!(branch.trim(), "two");
    }
}
