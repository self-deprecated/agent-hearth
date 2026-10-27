//! A log of the agent's own cargo runs: the golden builds the packages they built alone.
//!
//! ```text
//! <state>/builds.jsonl   one line per `agent-hearth cargo` run
//! ```

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::adopt;
use crate::locate::Location;

#[derive(Debug, Serialize, Deserialize)]
pub struct Build {
    /// Seconds since the Unix epoch at its start.
    pub at: u64,
    /// The worktree's build directory's name, which identifies the worktree.
    pub worktree: String,
    pub args: Vec<String>,
    pub code: u8,
}

/// Appends a run, started at `started`, to the log. Failing to log never fails the run.
pub fn record(location: &Location, args: &[String], code: u8, started: SystemTime) {
    let target = adopt::target_dir(location);
    let build = Build {
        at: started
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs()),
        worktree: target
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        args: args.to_vec(),
        code,
    };
    let append = || -> Result<()> {
        let path = location.state_dir.join("builds.jsonl");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut line = serde_json::to_vec(&build)?;
        line.push(b'\n');
        file.write_all(&line)?;
        Ok(())
    };
    if let Err(err) = append() {
        eprintln!("agent-hearth: not logged: {err:#}");
    }
}

fn read(location: &Location) -> Vec<Build> {
    let Ok(log) = fs::read_to_string(location.state_dir.join("builds.jsonl")) else {
        return Vec::new();
    };
    log.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The packages agents built alone (`-p <name>`, `--package <name>`) in any of the repository's
/// worktrees: the golden builds those members' variants.
pub fn packages_built_alone(location: &Location) -> BTreeSet<String> {
    let mut packages = BTreeSet::new();
    for build in read(location) {
        let mut args = build.args.iter().take_while(|arg| *arg != "--");
        while let Some(arg) = args.next() {
            let value = match arg.as_str() {
                "-p" | "--package" => args.next().cloned(),
                _ => arg
                    .strip_prefix("--package=")
                    .or_else(|| arg.strip_prefix("-p"))
                    .map(|name| name.trim_start_matches('=').to_owned()),
            };
            packages.extend(value.filter(|name| !name.is_empty()));
        }
    }
    packages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_packages_agents_built_alone() {
        let s = crate::testing::Sandbox::new();
        s.crate_repo("repo");
        let repo = s.location("repo");
        repo.create_dirs().unwrap();
        for args in [
            "test -p a",
            "clippy --package b -- -p notapackage",
            "build -pc",
            "check --package=d",
        ] {
            let args: Vec<String> = args.split(' ').map(String::from).collect();
            record(&repo, &args, 0, SystemTime::now());
        }
        let packages: Vec<String> = packages_built_alone(&repo).into_iter().collect();
        assert_eq!(packages, ["a", "b", "c", "d"]);
    }
}
