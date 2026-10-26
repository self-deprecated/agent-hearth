//! Where every worktree of the repository stands: verdicts for its current code, runs in flight,
//! and whether the golden that seeded it is still the current one.

use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

use crate::adopt;
use crate::check::{self, Status};
use crate::golden;
use crate::land;
use crate::locate::Location;
use crate::settle;
use crate::slot;

#[derive(Debug, Serialize)]
pub struct Worktree {
    pub path: PathBuf,
    /// False for a recorded worktree that has since been removed.
    pub exists: bool,
    pub check: Option<Status>,
    pub settle: Option<Status>,
    pub settling: bool,
    pub seeded_from: Option<String>,
    /// Seeded from a golden other than the current one.
    pub stale: bool,
}

/// This worktree first, then every other worktree adopted in its repository.
pub fn status(location: &Location) -> Result<Vec<Worktree>> {
    let current = golden::current(location)?.map(|golden| golden.id);
    let mut paths = vec![location.worktree.clone()];
    for path in adopt::recorded_worktrees(location)? {
        // Neither is an agent's worktree, and the gate is missing whenever no land runs.
        let mine = !land::is_gate(location, &path) && !slot::is_free(location, &path);
        if !paths.contains(&path) && mine {
            paths.push(path);
        }
    }
    let mut worktrees = Vec::new();
    for path in paths {
        let Some(found) = location
            .find_alike(&path)
            .ok()
            .filter(|found| found.worktree == path)
        else {
            worktrees.push(Worktree {
                path,
                exists: false,
                check: None,
                settle: None,
                settling: false,
                seeded_from: None,
                stale: false,
            });
            continue;
        };
        let seeded_from = adopt::seeded_from(&found)?;
        worktrees.push(Worktree {
            check: check::current_verdict(&found)?,
            settle: settle::verdict(&found)?.map(|verdict| verdict.status),
            settling: settle::running(&found)?,
            stale: seeded_from.is_some() && seeded_from != current,
            seeded_from,
            exists: true,
            path,
        });
    }
    Ok(worktrees)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn;
    use crate::testing::Sandbox;

    #[test]
    fn reports_verdicts_staleness_and_removed_worktrees() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let repo = s.location("repo");
        golden::refresh(&repo, "main").unwrap();
        let (wt, _) = spawn::spawn(&repo, "wt", "main").unwrap();
        let (gone, _) = spawn::spawn(&repo, "gone", "main").unwrap();
        check::check(&wt, &adopt::target_dir(&wt)).unwrap();
        std::fs::remove_dir_all(&gone.worktree).unwrap();
        s.write("repo/src/lib.rs", "pub fn two() -> u32 {\n    2\n}\n");
        s.run("repo", "git", &["commit", "-q", "-am", "two"]);
        golden::refresh(&repo, "main").unwrap();

        let worktrees = status(&wt).unwrap();
        let summary: Vec<_> = worktrees
            .iter()
            .map(|w| (w.path.as_path(), w.exists, w.check, w.stale))
            .collect();
        // Paths as spawn placed them: beside the repository, or in slots without the pin.
        assert_eq!(
            summary,
            [
                (wt.worktree.as_path(), true, Some(Status::Pass), true),
                (gone.worktree.as_path(), false, None, false)
            ]
        );
    }
}
