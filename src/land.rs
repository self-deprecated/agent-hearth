//! `land`: finished jj workspaces onto `main`, gated at the golden. A batch's commits are copied
//! onto `main` (`jj duplicate`), so gating never touches the workspaces or other agents' work.
//! Green, `main` moves to the copies, each workspace is rebased onto it (its landed commits empty
//! out and go) and the golden is refreshed. Red, the copies are abandoned and the batch is split
//! in halves until each failing workspace stands alone.
//!
//! ```text
//! <state>/land.lock    held for the whole of a land
//! <state>/land/gate    the jj workspace the gate runs in; its build directory stays warm
//! ```

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::adopt;
use crate::check;
use crate::check::Status;
use crate::golden;
use crate::locate::{Location, Vcs};
use crate::settle;

const GATE_WORKSPACE: &str = "agent-hearth-land-gate";

#[derive(Debug, Default, Serialize)]
pub struct Report {
    /// Workspaces whose work is now on `main`, in landing order.
    pub landed: Vec<String>,
    /// `main` afterwards.
    pub main: String,
    /// Workspaces left out, with why: gate problems, a conflict, or nothing to land.
    pub refused: Vec<(String, Vec<String>)>,
    /// Things that went wrong after `main` moved, such as a workspace that could not be rebased.
    pub warnings: Vec<String>,
    /// The log of the golden refresh running in the background, if one was started.
    pub refreshing: Option<PathBuf>,
}

/// Lands the workspaces `names`, in order. `accept_fewer` lets the gate's settle run fewer tests
/// than the last one on main, for a stack that deletes tests on purpose.
/// When anything landed, the golden is refreshed from the new `main`: in the background by
/// `agent_hearth` when given (it is a full build, most of a land's time), else before
/// returning.
pub fn land(
    location: &Location,
    names: &[String],
    accept_fewer: bool,
    agent_hearth: Option<&Path>,
) -> Result<Report> {
    if location.vcs != Vcs::Jj {
        bail!("land needs a jj repository");
    }
    fs::create_dir_all(&location.state_dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(location.state_dir.join("land.lock"))
        .context("opening the land lock")?;
    lock.lock().context("waiting for another land")?;

    let mut report = Report::default();
    let mut ready = Vec::new();
    for name in names {
        match landable(location, name)? {
            Ok(()) => ready.push(name.clone()),
            Err(reason) => report.refused.push((name.clone(), vec![reason])),
        }
    }
    land_batch(location, &ready, accept_fewer, &mut report)?;
    if !report.landed.is_empty() {
        // Main has moved: a failed refresh is reported, not fatal.
        match agent_hearth {
            Some(agent_hearth) => match golden::refresh_in_background(location, agent_hearth) {
                Ok(log) => report.refreshing = Some(log),
                Err(err) => report
                    .warnings
                    .push(format!("the golden refresh did not start: {err:#}")),
            },
            None => {
                if let Err(err) = golden::refresh(location, "main") {
                    report
                        .warnings
                        .push(format!("the golden was not refreshed: {err:#}"));
                }
            }
        }
    }
    report.main = jj(
        location,
        &["log", "--no-graph", "-r", "main", "-T", "commit_id"],
    )?;
    Ok(report)
}

/// Whether `name` has a linear stack of described commits that `main` lacks. Snapshots the
/// workspace first, so its latest edits count.
fn landable(location: &Location, name: &str) -> Result<Result<(), String>> {
    let root = match jj(location, &["workspace", "root", "--name", name]) {
        Ok(root) => PathBuf::from(root.trim()),
        Err(_) => return Ok(Err(format!("no jj workspace named {name}"))),
    };
    let status = crate::locate::jj_command()
        .args(["status", "--no-pager"])
        .current_dir(&root)
        .output()
        .context("running jj status")?;
    if !status.status.success() {
        bail!(
            "jj status in {}: {}",
            root.display(),
            String::from_utf8_lossy(&status.stderr).trim()
        );
    }
    let stack = stack(name);
    let count = |revset: &str| -> Result<usize> {
        let ids = jj(
            location,
            &[
                "log",
                "--no-graph",
                "-r",
                revset,
                "-T",
                "commit_id ++ \"\\n\"",
            ],
        )?;
        Ok(ids.lines().count())
    };
    Ok(if count(&stack)? == 0 {
        Err("nothing to land: no change that main lacks".to_owned())
    } else if count(&format!("heads({stack})"))? != 1 {
        Err("its changes are not one linear stack".to_owned())
    } else if count(&format!("({stack}) & description(exact:\"\")"))? > 0 {
        Err("a change has no description: describe it with `jj describe`".to_owned())
    } else {
        Ok(())
    })
}

/// The non-empty changes of workspace `name` that `main` lacks.
fn stack(name: &str) -> String {
    format!("(main..{name}@) ~ empty()")
}

fn land_batch(
    location: &Location,
    batch: &[String],
    accept_fewer: bool,
    report: &mut Report,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let mut copies = Vec::new();
    let mut tip = "main".to_owned();
    let mut conflict = None;
    for name in batch {
        let new = duplicate(location, &stack(name), &tip)?;
        let conflicted = !jj(
            location,
            &[
                "log",
                "--no-graph",
                "-r",
                &format!("({}) & conflicts()", new.join(" | ")),
                "-T",
                "commit_id",
            ],
        )?
        .is_empty();
        tip = new.last().cloned().context("jj duplicated nothing")?;
        copies.extend(new);
        if conflicted {
            conflict = Some(name.clone());
            break;
        }
    }
    let problems = match &conflict {
        Some(name) => vec![format!("{name}'s changes conflict with main")],
        None => gate(location, &tip, accept_fewer)?,
    };
    if problems.is_empty() {
        jj(location, &["bookmark", "set", "main", "-r", &tip])?;
        for name in batch {
            // Landed either way; a workspace that cannot follow main is reported, not fatal.
            if let Err(err) = follow_main(location, name) {
                report
                    .warnings
                    .push(format!("{name} landed, but stays on its old base: {err:#}"));
            }
            report.landed.push(name.clone());
        }
        return Ok(());
    }
    jj(location, &["abandon", &copies.join(" | ")])?;
    if let [name] = batch {
        report.refused.push((name.clone(), problems));
        return Ok(());
    }
    let (first, second) = batch.split_at(batch.len() / 2);
    land_batch(location, first, accept_fewer, report)?;
    land_batch(location, second, accept_fewer, report)
}

/// Rebases workspace `name` onto `main`, where its landed changes empty out and go, and brings its
/// files up to date. Fails on changes the user's jj settings make immutable, and leaves them.
fn follow_main(location: &Location, name: &str) -> Result<()> {
    let workspace = format!("{name}@");
    jj(
        location,
        &["rebase", "-b", &workspace, "-o", "main", "--skip-emptied"],
    )?;
    let root = jj(location, &["workspace", "root", "--name", name])?;
    let updated = crate::locate::jj_command()
        .args(["workspace", "update-stale"])
        .current_dir(root.trim())
        .output()
        .context("running jj workspace update-stale")?;
    if !updated.status.success() {
        bail!(
            "jj workspace update-stale in {root}: {}",
            String::from_utf8_lossy(&updated.stderr).trim()
        );
    }
    Ok(())
}

/// Copies `revset` onto `onto` and returns the copies' commit ids, oldest first.
fn duplicate(location: &Location, revset: &str, onto: &str) -> Result<Vec<String>> {
    let output = jj_output(location, &["duplicate", revset, "-o", onto])?;
    // "Duplicated <old commit> as <new change> <new commit> <description>"
    let mut copies = Vec::new();
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if let ["Duplicated", _, "as", _, commit, ..] = words[..] {
            copies.push(jj(
                location,
                &["log", "--no-graph", "-r", commit, "-T", "commit_id"],
            )?);
        }
    }
    Ok(copies)
}

/// Runs `check` and `settle` on `tip` in the gate workspace and returns what fails. A tip that
/// `check` would format or fix fails too: the fixes belong in the workspace, not in `main`.
fn gate(location: &Location, tip: &str, accept_fewer: bool) -> Result<Vec<String>> {
    let dir = gate_dir(location);
    fs::create_dir_all(dir.parent().context("gate has no parent")?)?;
    remove_gate(location, &dir)?;
    let added = crate::locate::jj_command()
        .args(["workspace", "add", "--name", GATE_WORKSPACE, "-r", tip])
        .arg(&dir)
        .current_dir(&location.worktree)
        .output()
        .context("running jj workspace add")?;
    if !added.status.success() {
        bail!(
            "jj workspace add {}: {}",
            dir.display(),
            String::from_utf8_lossy(&added.stderr).trim()
        );
    }
    let result = gate_in(location, &dir, accept_fewer);
    remove_gate(location, &dir)?;
    result
}

/// Where the gate workspace is checked out. It exists only during a gate, but its recorded build
/// directory is meant to outlive it.
pub fn gate_dir(location: &Location) -> PathBuf {
    location.state_dir.join("land").join("gate")
}

/// Whether `worktree`, a canonical path as adopt records it, is the gate workspace.
pub fn is_gate(location: &Location, worktree: &Path) -> bool {
    let land = location.state_dir.join("land");
    fs::canonicalize(land).is_ok_and(|land| worktree == land.join("gate"))
}

fn gate_in(location: &Location, dir: &Path, accept_fewer: bool) -> Result<Vec<String>> {
    let gate = location.find_alike(dir)?;
    gate.create_dirs()?;
    adopt::adopt_or_start_cold(&gate)?;
    let target = adopt::target_dir(&gate);
    let report = check::check(&gate, &target)?;
    if !report.fixed.is_empty() {
        let files: Vec<String> = report
            .fixed
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        return Ok(vec![format!(
            "`agent-hearth check` would change {}: run it in the workspace",
            files.join(", ")
        )]);
    }
    match report.status {
        Status::Pass => {}
        Status::Fail => return Ok(report.diagnostics.iter().map(|d| d.summary()).collect()),
        Status::Superseded => return Ok(vec![SUPERSEDED.to_owned()]),
    }
    let settled = settle::settle(&gate, &target, accept_fewer)?;
    Ok(match settled.verdict.status {
        Status::Pass => Vec::new(),
        Status::Fail => settled.verdict.failures,
        // Its failures may be empty: only a pass may move main.
        Status::Superseded => vec![SUPERSEDED.to_owned()],
    })
}

/// The gate's files changed while it ran, so its verdict says nothing about the tip.
const SUPERSEDED: &str = "the gate's files changed while it ran (a build script or test writing \
                          into the source tree?)";

/// Forgets the gate workspace and deletes its files. Its build directory stays, warm for the
/// next gate at the same path.
fn remove_gate(location: &Location, dir: &Path) -> Result<()> {
    let names = jj(location, &["workspace", "list", "-T", "name ++ \"\\n\""])?;
    if names.lines().any(|name| name == GATE_WORKSPACE) {
        jj(location, &["abandon", &format!("{GATE_WORKSPACE}@")])?;
        jj(location, &["workspace", "forget", GATE_WORKSPACE])?;
    }
    if dir.exists() {
        fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    Ok(())
}

/// Runs jj without snapshotting this worktree and returns its stdout, trimmed of the last newline.
fn jj(location: &Location, args: &[&str]) -> Result<String> {
    let output = jj_output(location, args)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.strip_suffix('\n').unwrap_or(&stdout).to_owned())
}

fn jj_output(location: &Location, args: &[&str]) -> Result<std::process::Output> {
    let output = crate::locate::jj_command()
        .args(["--ignore-working-copy", "--color=never"])
        .args(args)
        .current_dir(&location.worktree)
        .output()
        .context("running jj")?;
    if !output.status.success() {
        bail!(
            "jj {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn;
    use crate::testing::Sandbox;

    /// A colocated jj repo with the tiny crate, a golden, and one workspace per `(name, file,
    /// content)` holding one described change.
    fn repo_with(s: &Sandbox, changes: &[(&str, &str, &str)]) -> Location {
        s.crate_repo("repo");
        s.run("repo", "jj", &["git", "init", "--colocate"]);
        let repo = s.location("repo");
        golden::refresh(&repo, "main").unwrap();
        for (name, file, content) in changes {
            // Beside the repository, or in a slot where there is no pin.
            let (workspace, _) = spawn::spawn(&repo, name, "main").unwrap();
            let path = workspace.worktree.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
            s.run_at(
                &workspace.worktree,
                "jj",
                &["commit", "-m", &format!("{name}: change {file}")],
            );
        }
        repo
    }

    fn main_files(s: &Sandbox) -> String {
        s.run("repo", "jj", &["file", "list", "-r", "main"])
    }

    #[test]
    fn lands_a_green_batch_and_refreshes_the_golden() {
        let s = Sandbox::new();
        let repo = repo_with(
            &s,
            &[
                ("one", "src/one.rs", "pub fn one() {}\n"),
                ("two", "README.md", "Two.\n"),
            ],
        );
        let before = golden::current(&repo).unwrap().unwrap();
        let report = land(&repo, &["one".to_owned(), "two".to_owned()], false, None).unwrap();
        assert_eq!(report.landed, ["one", "two"], "{:?}", report.refused);
        let files = main_files(&s);
        assert!(
            files.contains("src/one.rs") && files.contains("README.md"),
            "{files}"
        );
        let after = golden::current(&repo).unwrap().unwrap();
        assert_eq!(after.commit, report.main);
        assert_ne!(after.id, before.id);
        // The workspaces now sit on main, with nothing left to land.
        assert!(
            matches!(landable(&repo, "one").unwrap(), Err(reason) if reason.starts_with("nothing to land"))
        );
        assert!(
            !s.root()
                .join("state")
                .join(&repo.repo_id)
                .join("land/gate")
                .exists()
        );
    }

    #[test]
    fn a_failing_or_conflicting_workspace_stays_out_and_the_rest_lands() {
        let s = Sandbox::new();
        let failing = "pub fn one() -> u32 {\n    1\n}\n\n#[test]\nfn one_is_two() {\n    assert_eq!(one(), 2);\n}\n";
        let repo = repo_with(
            &s,
            &[
                ("good", "README.md", "Good.\n"),
                ("bad", "src/lib.rs", failing),
                ("clash", "README.md", "Clash.\n"),
            ],
        );
        let names = ["good", "bad", "clash"].map(String::from);
        let report = land(&repo, &names, false, None).unwrap();
        assert_eq!(report.landed, ["good"]);
        let refused: Vec<&str> = report
            .refused
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(refused, ["bad", "clash"]);
        assert!(
            report.refused[0].1[0].starts_with("FAILED one_is_two"),
            "{:?}",
            report.refused
        );
        assert_eq!(report.refused[1].1, ["clash's changes conflict with main"]);
        let leftovers = s.run(
            "repo",
            "jj",
            &["log", "--no-graph", "-r", "conflicts()", "-T", "commit_id"],
        );
        assert_eq!(leftovers, "", "the conflicted copies were abandoned");
    }
}
