//! Creates a new worktree and adopts it: beside the current one where builds run at the pin,
//! else in a slot of the slot pool.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::adopt::{self, Outcome};
use crate::locate::{Location, Vcs};
use crate::slot;

/// Creates the worktree `name` at `rev` with `from`'s tool, then adopts it. Returns the new
/// worktree's location and how adopting went.
pub fn spawn(from: &Location, name: &str, rev: &str) -> Result<(Location, Outcome)> {
    if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
        bail!("{name:?} is not a worktree name: use a plain name such as fix-login");
    }
    match from.pin {
        Some(_) => beside(from, name, rev),
        None => slot::claim(from, name, rev),
    }
}

/// The worktree `name`, beside `from`'s.
fn beside(from: &Location, name: &str, rev: &str) -> Result<(Location, Outcome)> {
    let parent = from
        .worktree
        .parent()
        .context("the worktree has no parent directory")?;
    let dest = parent.join(name);
    if dest.exists() {
        bail!("{} already exists", dest.display());
    }
    create(from, name, rev, &dest)?;
    let location = from.find_alike(&dest)?;
    location.create_dirs()?;
    let outcome = adopt::adopt(&location)?;
    Ok((location, outcome))
}

/// Makes the jj workspace or git worktree (on a new branch) `name` at `dest`.
pub fn create(from: &Location, name: &str, rev: &str, dest: &Path) -> Result<()> {
    let mut command = match from.vcs {
        Vcs::Jj => {
            let mut jj = crate::locate::jj_command();
            jj.args(["workspace", "add", "--name", name, "--revision", rev])
                .arg(dest);
            jj
        }
        Vcs::Git => {
            let mut git = Command::new("git");
            git.args(["worktree", "add", "-b", name]).arg(dest).arg(rev);
            git
        }
    };
    let program = from.vcs.name();
    let output = command
        .current_dir(&from.worktree)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{program} could not create {}: {}",
            dest.display(),
            stderr.trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden;
    use crate::testing::Sandbox;

    #[test]
    fn spawns_and_adopts_a_jj_workspace_and_a_git_worktree() {
        let s = Sandbox::new();
        s.crate_repo("git");
        let git = s.location("git");
        golden::refresh(&git, "main").unwrap();
        let (wt, outcome) = beside(&git, "feature", "main").unwrap();
        assert_eq!(
            wt.worktree,
            s.root().canonicalize().unwrap().join("feature")
        );
        assert_eq!((wt.vcs, &wt.repo_id), (Vcs::Git, &git.repo_id));
        assert!(matches!(outcome, Outcome::Adopted(_)));
        let branch = s.run("feature", "git", &["branch", "--show-current"]);
        assert_eq!(branch.trim(), "feature");

        s.run("git", "jj", &["git", "init", "--colocate"]);
        let jj = s.location("git");
        let (ws, outcome) = beside(&jj, "jj-feature", "main").unwrap();
        assert_eq!((ws.vcs, &ws.repo_id), (Vcs::Jj, &git.repo_id));
        assert!(matches!(outcome, Outcome::Adopted(_)), "{outcome:?}");
        assert!(ws.worktree.join("src/lib.rs").is_file());
    }

    #[test]
    fn refuses_names_that_are_paths_or_taken() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let repo = s.location("repo");
        for name in ["", "a/b", "..", ".hidden"] {
            assert!(spawn(&repo, name, "main").is_err(), "{name:?}");
        }
        err_contains(beside(&repo, "repo", "main"), "already exists");
    }

    fn err_contains(result: Result<(Location, Outcome)>, text: &str) {
        let err = result.expect_err("an error");
        assert!(err.to_string().contains(text), "{err}");
    }
}
