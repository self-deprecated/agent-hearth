//! A log of the agent's own cargo runs, to see where its waiting goes: building or running tests,
//! waiting for a warm, and runs of code that has not changed since the same command last ran.
//!
//! ```text
//! <state>/builds.jsonl   one line per `agent-hearth cargo` or `agent-hearth test` run
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::adopt;
use crate::cargo::Timing;
use crate::locate::Location;

#[derive(Debug, Serialize, Deserialize)]
pub struct Build {
    /// Seconds since the Unix epoch at its start.
    pub at: u64,
    /// The worktree's build directory's name, which identifies the worktree.
    pub worktree: String,
    pub args: Vec<String>,
    pub code: u8,
    /// From the start to its end, the wait for a warm included.
    pub seconds: f64,
    /// Waiting for a warm to finish or stop.
    pub warm_wait: f64,
    /// Building: up to cargo's `Finished` line. Unknown when cargo printed none (`--quiet`).
    pub build: Option<f64>,
    pub compiled: u32,
    /// The fingerprint of the code it ran on.
    pub fingerprint: String,
}

/// Appends a run to the log. Failing to log never fails the run.
#[allow(clippy::too_many_arguments)]
pub fn record(
    location: &Location,
    args: &[String],
    code: u8,
    started: SystemTime,
    seconds: Duration,
    warm_wait: Duration,
    timing: &Timing,
    fingerprint: String,
) {
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
        seconds: seconds.as_secs_f64(),
        warm_wait: warm_wait.as_secs_f64(),
        build: timing.build.map(|build| build.as_secs_f64()),
        compiled: timing.compiled,
        fingerprint,
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

pub fn read(location: &Location) -> Vec<Build> {
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

/// One subcommand's runs, summed.
#[derive(Debug, Default, Serialize)]
pub struct Summary {
    pub runs: u32,
    pub seconds: f64,
    /// Of `seconds`: building, where cargo said when it finished.
    pub build: f64,
    /// Of `seconds`: after building (running tests or the program).
    pub after_build: f64,
    pub warm_wait: f64,
    /// Runs, and their seconds, of the same command on the same worktree's unchanged code as the
    /// run before it: what a cache of results could answer at once.
    pub repeats: u32,
    pub repeat_seconds: f64,
    pub median: f64,
}

/// The log summed per subcommand (`test`, `clippy`, ...), and over all of them as `all`.
pub fn summarize(builds: &[Build]) -> BTreeMap<String, Summary> {
    let mut summaries: BTreeMap<String, Summary> = BTreeMap::new();
    let mut durations: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut last: BTreeMap<(&str, &[String]), &str> = BTreeMap::new();
    for build in builds {
        let subcommand = build.args.first().cloned().unwrap_or_default();
        let repeat = last.insert((&build.worktree, &build.args), &build.fingerprint)
            == Some(&build.fingerprint);
        for key in [subcommand, "all".to_owned()] {
            let summary = summaries.entry(key.clone()).or_default();
            summary.runs += 1;
            summary.seconds += build.seconds;
            summary.warm_wait += build.warm_wait;
            if let Some(seconds) = build.build {
                summary.build += seconds;
                summary.after_build += (build.seconds - build.warm_wait - seconds).max(0.0);
            }
            if repeat {
                summary.repeats += 1;
                summary.repeat_seconds += build.seconds;
            }
            durations.entry(key).or_default().push(build.seconds);
        }
    }
    for (key, mut seconds) in durations {
        seconds.sort_by(f64::total_cmp);
        summaries.get_mut(&key).expect("summed above").median = seconds[seconds.len() / 2];
    }
    summaries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(args: &str, fingerprint: &str, seconds: f64, build: Option<f64>) -> Build {
        Build {
            at: 0,
            worktree: "w".to_owned(),
            args: args.split(' ').map(String::from).collect(),
            code: 0,
            seconds,
            warm_wait: 0.0,
            build,
            compiled: 0,
            fingerprint: fingerprint.to_owned(),
        }
    }

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
            let started = SystemTime::now();
            record(
                &repo,
                &args,
                0,
                started,
                Duration::ZERO,
                Duration::ZERO,
                &Timing::default(),
                String::new(),
            );
        }
        let packages: Vec<String> = packages_built_alone(&repo).into_iter().collect();
        assert_eq!(packages, ["a", "b", "c", "d"]);
    }

    #[test]
    fn sums_per_subcommand_and_counts_repeats_of_unchanged_code() {
        let builds = [
            build("test", "a", 10.0, Some(6.0)),
            build("clippy", "a", 4.0, Some(4.0)),
            // The same command on the same code: a repeat. Another command between them is not.
            build("test", "a", 5.0, Some(1.0)),
            build("test", "b", 8.0, Some(5.0)),
        ];
        let summaries = summarize(&builds);
        let test = &summaries["test"];
        assert_eq!((test.runs, test.repeats), (3, 1));
        assert_eq!((test.seconds, test.repeat_seconds), (23.0, 5.0));
        assert_eq!((test.build, test.after_build), (12.0, 11.0));
        assert_eq!(test.median, 8.0);
        assert_eq!(summaries["all"].runs, 4);
        assert_eq!(summaries["clippy"].repeats, 0);
    }
}
