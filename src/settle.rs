//! Settling: the full test suite, doctests included, for exactly the current code, in the
//! foreground or the background. The verdict is stored where the next Stop hook finds it.
//!
//! ```text
//! <state>/settles/<worktree-id>.json   the last passing settle's test count: the guard's baseline
//! <state>/settles/<worktree-id>.log    output of the last background settle
//! <state>/settles/<worktree-id>.lock   held while a settle of the worktree runs
//! <state>/settles/<worktree-id>.failed.json  the tests that failed in its recent settles
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adopt;
use crate::cargo;
use crate::check::{self, Diagnostic, Snapshot, Status};
use crate::golden;
use crate::locate::Location;
use crate::memory;
use crate::reuse::{self, Outcome};

/// Windows' `BELOW_NORMAL_PRIORITY_CLASS` process creation flag.
#[cfg(windows)]
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

/// Part of every settle fingerprint, as `check`'s gate spec is of its.
const SETTLE: &str =
    "settle 3: cargo test --workspace --no-fail-fast; main's failures and what it leaves out";
/// Set in a settle started detached, in a process group of its own.
const DETACHED: &str = "AGENT_HEARTH_DETACHED";

/// Ends a detached settle: stops what its tests left running in its process group (atuin's
/// tests leave `atuin daemon` processes behind), all but itself. A settle in the foreground shares
/// its caller's group, so it leaves that alone.
pub fn end_detached() {
    #[cfg(unix)]
    if std::env::var_os(DETACHED).is_some() {
        // SAFETY: ignoring SIGTERM for this process, then sending it to its own group.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
            libc::kill(0, libc::SIGTERM);
        }
    }
}

/// In a golden's directory: the failures main's own code has, from its run of the suite.
const MAIN_FAILURES: &str = "tests.json";
/// In a golden's directory: what of the suite does not compile on main, which its run and every
/// settle of a worktree seeded from it leave out ([`LeftOut`]); written before `MAIN_FAILURES`.
const LEFT_OUT: &str = "left-out.json";
/// How long a retry of failed tests waits for other settles to finish; tests do not wait on the
/// machine they run on.
const RETRY_WAIT: std::time::Duration = if cfg!(test) {
    std::time::Duration::ZERO
} else {
    std::time::Duration::from_secs(180)
};
/// A settle's `cargo test`, before what it selects (`--workspace`, or packages and targets).
const TEST: [&str; 3] = ["test", "--no-fail-fast", "--message-format=json"];
const WORKSPACE: &str = "--workspace";
/// How many of a worktree's recent settles decide which tests run first.
const HISTORY: usize = 5;
/// At most this many tests run first: past that, the first pass is much of the suite.
const FIRST: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub status: Status,
    /// Tests run or ignored, doctests included.
    pub tests: u64,
    /// One line per failed test or compile error.
    pub failures: Vec<String>,
    /// Tests that failed, then passed when run again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flaky: Vec<String>,
    /// Only the tests that failed in recent settles ran, and some failed again: the rest of the
    /// suite runs once they pass.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
    /// Failures main has too, not reported.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub existing: usize,
    /// Of `tests`, those whose results were main's, their test binaries unchanged (`reuse`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reused: u64,
    /// Why the whole suite ran rather than only what the change can have reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full: Option<String>,
    /// Test targets left out (`b --test broken`): they do not compile on main, and the rest of
    /// their packages' tests ran without them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
    /// Packages of which no test ran: their library or build script does not compile on main,
    /// or they depend on one that does not. Not a full pass, whatever the status.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unbuilt: Vec<String>,
}

/// What of the suite does not compile on main, found by the golden's run (`settle_golden`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct LeftOut {
    /// Test targets that do not compile, each left out alone.
    targets: BTreeSet<Target>,
    /// Packages whose library or build script does not compile: nothing of theirs, or of what
    /// depends on them, can run.
    unbuilt: BTreeSet<String>,
}

impl LeftOut {
    fn is_empty(&self) -> bool {
        self.targets.is_empty() && self.unbuilt.is_empty()
    }
}

/// A package's target as cargo selects it: `kind` is `lib` (any library), `bin`, `test`,
/// `bench`, `example`, or `build-script`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct Target {
    package: String,
    kind: String,
    name: String,
}

impl Target {
    /// From a cargo JSON message's `target` and its package.
    fn of(package: String, target: &Value) -> Option<Self> {
        let kind = match target["kind"].as_array()?.first()?.as_str()? {
            "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro" => "lib",
            "custom-build" => "build-script",
            kind => kind,
        };
        Some(Self {
            package,
            kind: kind.to_owned(),
            name: target["name"].as_str()?.to_owned(),
        })
    }

    /// The arguments that select it for `cargo test`, past `-p <package>`.
    fn flags(&self) -> Vec<String> {
        match self.kind.as_str() {
            "lib" => vec!["--lib".to_owned()],
            kind => vec![format!("--{kind}"), self.name.clone()],
        }
    }

    /// `b --test broken`.
    fn describe(&self) -> String {
        format!("{} {}", self.package, self.flags().join(" "))
    }

    /// The library or build script, without which nothing of the package builds.
    fn whole(&self) -> bool {
        self.kind == "lib" || self.kind == "build-script"
    }
}

/// What one part of a settle selects for `cargo test`: packages (`--workspace`, with
/// `--exclude`s, or `-p <package>`), and their test targets when not cargo's own choice.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Selection {
    packages: Vec<String>,
    /// The test targets' arguments (`--lib`, `--test x`), or `None` for every test target and the
    /// doctests, as `cargo test` picks them.
    targets: Option<Vec<String>>,
    /// With `targets`: the doctests too, in a run of their own (cargo does not mix `--doc` with
    /// other targets).
    doc: bool,
}

impl Selection {
    fn workspace() -> Self {
        Self {
            packages: vec![WORKSPACE.to_owned()],
            targets: None,
            doc: true,
        }
    }

    /// The package this selects alone, if it does.
    fn alone(&self) -> Option<&str> {
        match &self.packages[..] {
            [p, package] if p == "-p" => Some(package.as_str()),
            _ => None,
        }
    }

    /// The cargo runs of all of it.
    fn runs(&self) -> Vec<Vec<String>> {
        let Some(targets) = &self.targets else {
            return vec![self.packages.clone()];
        };
        let mut runs = Vec::new();
        if !targets.is_empty() {
            runs.push([self.packages.clone(), targets.clone()].concat());
        }
        if self.doc {
            runs.push([self.packages.clone(), vec!["--doc".to_owned()]].concat());
        }
        runs
    }

    /// The cargo runs of the tests `names` (each a test's full name) in it: one for the doctests
    /// among them (`src/lib.rs - item (line 3)`) and one for the others. Each names its targets:
    /// without that, running a few tests by name still built and ran every crate's doctests, 73s
    /// for one failing unit test on atuin on an M2 (`bench/feedback-atuin-mac1.txt`). Doctests
    /// are matched as substrings: rustdoc's names never match with `--exact` (`cargo test --doc
    /// -- --exact "a/src/lib.rs - one (line 3)"` runs nothing, `bench/doctest-exact-vm1.txt`), so
    /// a failing doctest's retry ran nothing and it passed as flaky.
    fn named(&self, names: &[String]) -> Vec<Vec<String>> {
        // Older rustdoc adds ` - compile fail` and the like after the line.
        let doctest = |name: &&String| name.contains(" - ") && name.contains(" (line ");
        let (doctests, others): (Vec<&String>, Vec<&String>) = names.iter().partition(doctest);
        let mut runs = Vec::new();
        if !doctests.is_empty() {
            let mut run = self.packages.clone();
            run.extend(["--doc".to_owned(), "--".to_owned()]);
            run.extend(doctests.into_iter().cloned());
            runs.push(run);
        }
        if !others.is_empty() {
            let mut run = self.packages.clone();
            match &self.targets {
                Some(targets) if !targets.is_empty() => run.extend(targets.iter().cloned()),
                _ => run.push("--tests".to_owned()),
            }
            run.extend(["--".to_owned(), "--exact".to_owned()]);
            run.extend(others.into_iter().cloned());
            runs.push(run);
        }
        runs
    }
}

/// How a settle runs the suite when main leaves some of it out (`left`), from the tree's
/// `metadata`: the workspace without the packages concerned; each package with a target left
/// out, alone (`-p`), with its other test targets and its doctests; nothing of a package whose
/// library does not build, or of what depends on it. Returns the parts and the packages of which
/// nothing runs. Alone, a package resolves its features without the rest of the workspace, which
/// may rebuild dependencies; main's run selects the same, so their results compare.
fn selections(metadata: &Value, left: &LeftOut) -> (Vec<Selection>, BTreeSet<String>) {
    if left.is_empty() {
        return (vec![Selection::workspace()], BTreeSet::new());
    }
    let members: BTreeSet<&str> = metadata["workspace_members"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let packages: Vec<&Value> = metadata["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|package| {
            package["id"]
                .as_str()
                .is_some_and(|id| members.contains(id))
        })
        .collect();
    let name_of = |id: &str| {
        packages
            .iter()
            .find(|package| package["id"] == id)
            .and_then(|package| package["name"].as_str())
            .map(str::to_owned)
    };
    let roots: Vec<&str> = left.unbuilt.iter().map(String::as_str).collect();
    let mut unbuilt = left.unbuilt.clone();
    unbuilt.extend(
        cargo::reaching(metadata, &roots)
            .keys()
            .filter_map(|id| name_of(id)),
    );
    let partial: BTreeSet<&str> = left
        .targets
        .iter()
        .map(|target| target.package.as_str())
        .filter(|package| !unbuilt.contains(*package))
        .collect();
    let mut selections = Vec::new();
    let names: Vec<&str> = packages
        .iter()
        .filter_map(|package| package["name"].as_str())
        .collect();
    let rest: Vec<&&str> = names
        .iter()
        .filter(|name| !unbuilt.contains(**name) && !partial.contains(**name))
        .collect();
    // Without metadata (`Value::Null`), the workspace without those packages, as far as known.
    if !rest.is_empty() || packages.is_empty() {
        let mut excluded: Vec<String> = unbuilt.iter().cloned().collect();
        excluded.extend(partial.iter().map(|name| name.to_string()));
        selections.push(Selection {
            packages: excluding(&excluded),
            targets: None,
            doc: true,
        });
    }
    for package in packages.iter().filter(|package| {
        package["name"]
            .as_str()
            .is_some_and(|name| partial.contains(name))
    }) {
        let name = package["name"].as_str().unwrap_or_default().to_owned();
        let mut targets = Vec::new();
        let mut doc = false;
        for target in package["targets"].as_array().into_iter().flatten() {
            let Some(target_id) = Target::of(name.clone(), target) else {
                continue;
            };
            if target_id.kind == "lib" && target["doctest"] == true {
                doc = true;
            }
            // What `cargo test` runs: targets with `test = true` (examples and benches are only
            // built, unless they say so).
            if target["test"] == true
                && target_id.kind != "build-script"
                && !left.targets.contains(&target_id)
            {
                targets.extend(target_id.flags());
            }
        }
        if !targets.is_empty() || doc {
            selections.push(Selection {
                packages: vec!["-p".to_owned(), name],
                targets: Some(targets),
                doc,
            });
        }
    }
    (selections, unbuilt)
}

/// What main leaves out of the suite in the golden `id`, once its run has found it.
fn left_out(location: &Location, id: &str) -> LeftOut {
    fs::read(golden::dir(location, id).join(LEFT_OUT))
        .ok()
        .and_then(|json| serde_json::from_slice(&json).ok())
        .unwrap_or_default()
}

fn is_zero<T: Default + PartialEq>(n: &T) -> bool {
    *n == T::default()
}

/// The settle gate for the worktree, and the failures of the main its golden was built from:
/// tests that fail there too (they need a display, a service, the network the machine lacks) are
/// not the agent's to fix. Until the golden's own run has found them, they are unknown (`None`),
/// and the gate differs, so a verdict reached without them is not reused once they are known.
fn gate(location: &Location, reuse: bool) -> Result<(String, Option<BTreeSet<String>>)> {
    let golden = adopt::seeded_from(location)?;
    let main = golden.as_deref().and_then(|id| main_failures(location, id));
    let key = match (&main, &golden) {
        (Some(failures), Some(id)) => {
            let left = left_out(location, id);
            let mut key = serde_json::to_string(failures)?;
            if !left.is_empty() {
                key.push_str(&serde_json::to_string(&left)?);
            }
            key
        }
        _ => "unknown".to_owned(),
    };
    // Reuse turned off is another gate, so the two can be compared on the same code.
    let reuse = if reuse { "" } else { " without reuse" };
    let toolchain = crate::toolchain::gate_key(location.toolchain.as_ref());
    Ok((format!("{SETTLE}{reuse}: {key}{toolchain}"), main))
}

/// The failures main has in the golden `id`, once its run of the suite has found them.
pub fn main_failures(location: &Location, id: &str) -> Option<BTreeSet<String>> {
    let json = fs::read(golden::dir(location, id).join(MAIN_FAILURES)).ok()?;
    serde_json::from_slice(&json).ok()
}

/// `run`'s failures that main does not have, and how many it has.
fn new_failures(run: &Run, main: &BTreeSet<String>) -> (Vec<String>, usize) {
    let mut new = Vec::new();
    for (failure, key) in run.failures.iter().zip(&run.keys) {
        if !main.contains(key) {
            new.push(failure.clone());
        }
    }
    let existing = run.failures.len() - new.len();
    (new, existing)
}

/// The arguments of a settle's `cargo test`.
fn test_args() -> Vec<String> {
    // An eighth of the cores for the build, at least 4: on 32 cores a settle linked 30 test
    // binaries at once, most of its peak memory; with 4 jobs it peaked at a third of that and
    // took a little longer, so more can run at once. Fewer than 4 slowed it more than
    // it saved on an 8-core Mac.
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut args = TEST.map(String::from).to_vec();
    args.extend(["--jobs".to_owned(), (cores / 8).max(4).to_string()]);
    args
}

/// Runs the suite on the golden `id` of `location`'s repository, admitted as a settle is, and
/// records the failures main has: tests that fail twice, and compile errors.
pub fn settle_golden(location: &Location, id: &str) -> Result<()> {
    let dir = golden::dir(location, id);
    let path = dir.join(MAIN_FAILURES);
    if path.is_file() {
        return Ok(());
    }
    let _place = queue(location)?;
    let built_with = golden::get(location, id)?.and_then(|golden| golden.built_with);
    let (src, target) = (dir.join("src"), dir.join("target"));
    let pinned = location.pinned();
    let (pin, mounts) = match &pinned {
        Some((pin, mounts)) => (Some(*pin), mounts.as_slice()),
        None => (None, &[][..]),
    };
    // Diagnostics name files under the path the golden builds at.
    let root = pin.map_or_else(|| src.clone(), |pin| pin.join("src"));
    let args = test_args();
    // Before the run, which may write files into the tree.
    let files = check::tree_files(location, &src);
    let suite = |args: &[String]| {
        let mut command = cargo::command(
            &src,
            &target,
            Path::new(""),
            pin,
            mounts,
            built_with.as_ref(),
        );
        command.args(args);
        let (code, output) = cargo::merged_output(command).context("running the golden's tests")?;
        anyhow::Ok((code, String::from_utf8_lossy(&output).into_owned()))
    };
    // A target that does not compile stops `cargo test` before any test runs: run what does,
    // without it (`selections`), and find what else stops that, until nothing does. Compile
    // errors in a library or build script leave out its package and what depends on it.
    let metadata = cargo::metadata(&src, true).unwrap_or(Value::Null);
    let mut left = LeftOut::default();
    let mut diagnostics = BTreeSet::new();
    let (mut first, mut code, mut output, mut parts);
    loop {
        (parts, _) = selections(&metadata, &left);
        (first, code, output) = (Run::default(), 0, String::new());
        let mut found = false;
        for selection in &parts {
            for scope in selection.runs() {
                let (part_code, part_output) = suite(&[args.clone(), scope].concat())?;
                let run = parse(&part_output, &root);
                for broken in &run.broken {
                    let alone = selection.alone() == Some(broken.package.as_str());
                    found |= if broken.whole() && (alone || left.targets.contains(broken)) {
                        left.unbuilt.insert(broken.package.clone())
                    } else {
                        left.targets.insert(broken.clone())
                    };
                }
                diagnostics.extend(
                    run.keys
                        .iter()
                        .filter(|key| key.starts_with("diagnostic "))
                        .cloned(),
                );
                (code, output) = (code.max(part_code), output + &part_output);
                first.add(run);
            }
        }
        if !found {
            break;
        }
    }
    // A test binary that crashed or was cut off (no `test result`), or failed naming no test,
    // is not main's result to reuse; nor is anything when cargo failed unexplained.
    let units = first.sound_units(code);
    let map = match pin {
        Some(pin) => vec![
            (pin.join("target"), target.clone()),
            (pin.join("src"), src.clone()),
        ],
        None => Vec::new(),
    };
    let paths = reuse::Paths {
        root: &root,
        dep_info_root: &root,
        map: &map,
        target: &target,
    };
    let recorded = files.and_then(|files| {
        let build = reuse::Build::parse(&output, &paths);
        let metadata = reuse::Metadata::of_tree(&src)?;
        let main = reuse::record(&build, &paths, &units, files, &metadata)?;
        golden::write_atomically(&dir.join(reuse::MAIN), &serde_json::to_vec(&main)?)
    });
    if let Err(err) = recorded {
        eprintln!("agent-hearth: main's results are not kept for settles to reuse: {err:#}");
    }
    let mut failures = diagnostics;
    // Those that pass when run again are flaky, which a settle retries anyway.
    if !first.failed.is_empty() {
        let again = run_golden(location, id, &parts, &first.failed, false)?;
        failures.extend(again.failed.iter().map(|name| format!("test {name}")));
    }
    // Before main's failures, which make a worktree's settle read both.
    golden::write_atomically(&dir.join(LEFT_OUT), &serde_json::to_vec_pretty(&left)?)?;
    golden::write_atomically(&path, &serde_json::to_vec_pretty(&failures)?)
}

/// Starts `agent_hearth golden settle` for the golden `id`, detached and niced, unless its
/// failures are recorded already.
pub fn settle_golden_in_background(
    location: &Location,
    agent_hearth: &Path,
    id: &str,
) -> Result<()> {
    let dir = golden::dir(location, id);
    if dir.join(MAIN_FAILURES).is_file() {
        return Ok(());
    }
    let log_path = dir.join("settle.log");
    let log =
        File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
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
        .args(["golden", "settle", id])
        .current_dir(&location.worktree)
        .env("AGENT_HEARTH_STATE_DIR", &base.state)
        .env("AGENT_HEARTH_CACHE_DIR", &base.cache)
        .env(DETACHED, "1")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command.spawn().context("starting the golden's settle")?;
    Ok(())
}

/// The tests among `names` that fail on the golden `id` in each of `times` runs, one test at a
/// time: those that are not flaky.
pub fn failing_every_time(
    location: &Location,
    id: &str,
    names: &[String],
    times: usize,
) -> Result<Vec<String>> {
    let _place = queue(location)?;
    let src = golden::dir(location, id).join("src");
    let metadata = cargo::metadata(&src, true).unwrap_or(Value::Null);
    let (parts, _) = selections(&metadata, &left_out(location, id));
    let mut failing = names.to_vec();
    for _ in 0..times {
        if failing.is_empty() {
            break;
        }
        let run = run_golden(location, id, &parts, &failing, true)?;
        failing.retain(|name| run.failed.contains(name));
    }
    Ok(failing)
}

/// Runs the tests `names` of `parts` ([`Selection::named`]) on the golden `id`, with the toolchain
/// it was built with, whichever builds use now; with `one_at_a_time`, one test at a time.
fn run_golden(
    location: &Location,
    id: &str,
    parts: &[Selection],
    names: &[String],
    one_at_a_time: bool,
) -> Result<Run> {
    let dir = golden::dir(location, id);
    let built_with = golden::get(location, id)?.and_then(|golden| golden.built_with);
    let (src, target) = (dir.join("src"), dir.join("target"));
    let pinned = location.pinned();
    let (pin, mounts) = match &pinned {
        Some((pin, mounts)) => (Some(*pin), mounts.as_slice()),
        None => (None, &[][..]),
    };
    // Diagnostics name files under the path the golden builds at.
    let root = pin.map_or_else(|| src.clone(), |pin| pin.join("src"));
    let mut run = Run::default();
    for selection in parts.iter().flat_map(|part| part.named(names)) {
        let mut command = cargo::command(
            &src,
            &target,
            Path::new(""),
            pin,
            mounts,
            built_with.as_ref(),
        );
        command.args(test_args()).args(selection);
        if one_at_a_time {
            command.arg("--test-threads=1");
        }
        let output = command
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .context("running the golden's tests")?;
        run.add(parse(&String::from_utf8_lossy(&output.stdout), &root));
    }
    Ok(run)
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub fingerprint: String,
    pub cached: bool,
    #[serde(flatten)]
    pub verdict: Verdict,
}

#[derive(Debug, Serialize, Deserialize)]
struct Baseline {
    tests: u64,
}

/// The worktree's last settle that ran to a verdict, kept even after its code changed: a settle
/// run in the background after the agent stopped has to reach it some other way.
#[derive(Debug, Serialize, Deserialize)]
pub struct Last {
    pub fingerprint: String,
    pub verdict: Verdict,
    /// From its start to its verdict, its wait for memory or a turn left out.
    pub seconds: f64,
    /// Whether the agent has been told of its failures.
    #[serde(default)]
    pub reported: bool,
}

/// The fingerprint a settle of the worktree's current code would have.
pub fn fingerprint(location: &Location) -> Result<String> {
    Ok(Snapshot::take(location, &gate(location, reuse::enabled())?.0)?.fingerprint)
}

/// The worktree's last settle, if it failed and the agent has not been told; marks it told.
pub fn take_unreported_failure(location: &Location) -> Result<Option<Last>> {
    let path = file(location, "last.json");
    let Some(mut last) = fs::read(&path)
        .ok()
        .and_then(|json| serde_json::from_slice::<Last>(&json).ok())
    else {
        return Ok(None);
    };
    if last.verdict.status != Status::Fail || last.reported {
        return Ok(None);
    }
    last.reported = true;
    fs::write(&path, serde_json::to_vec(&last)?)
        .with_context(|| format!("writing {}", path.display()))?;
    last.reported = false;
    Ok(Some(last))
}

/// The longest of the repository's five most recent settles, in seconds, if any has run.
pub fn recent_seconds(location: &Location) -> Result<Option<f64>> {
    let dir = location.state_dir.join("settles");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(None);
    };
    let mut lasts = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().ends_with(".last.json") {
            continue;
        }
        let modified = entry.metadata()?.modified()?;
        // A partial settle ran a few tests, which says nothing about how long the suite takes.
        if let Ok(last) = serde_json::from_slice::<Last>(&fs::read(entry.path())?)
            && !last.verdict.partial
        {
            lasts.push((modified, last.seconds));
        }
    }
    lasts.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    Ok(lasts
        .iter()
        .take(5)
        .map(|(_, seconds)| *seconds)
        .reduce(f64::max))
}

/// The stored settle verdict for the worktree's current code, if there is one.
pub fn verdict(location: &Location) -> Result<Option<Verdict>> {
    let snapshot = Snapshot::take(location, &gate(location, reuse::enabled())?.0)?;
    check::load_verdict(location, &snapshot.fingerprint)
}

/// Runs the full suite into `target` and stores its verdict. Fails when fewer tests ran than in
/// the worktree's last passing settle, unless `accept_fewer`.
pub fn settle(location: &Location, target: &Path, accept_fewer: bool) -> Result<Report> {
    settle_with(location, target, accept_fewer, reuse::enabled())
}

/// [`settle`], reusing main's results for unchanged test binaries when `reuse`.
fn settle_with(
    location: &Location,
    target: &Path,
    accept_fewer: bool,
    reuse: bool,
) -> Result<Report> {
    let lock = lock_file(location)?;
    lock.lock()
        .context("waiting for another settle of this worktree")?;
    let (gate, known) = gate(location, reuse)?;
    let main = known.clone().unwrap_or_default();
    // Main's failures and what it leaves out come from the same run of the golden's.
    let left = match (&known, adopt::seeded_from(location)?) {
        (Some(_), Some(id)) => left_out(location, &id),
        _ => LeftOut::default(),
    };
    let before = Snapshot::take(location, &gate)?;
    if !accept_fewer && let Some(verdict) = check::load_verdict(location, &before.fingerprint)? {
        return Ok(Report {
            fingerprint: before.fingerprint,
            cached: true,
            verdict,
        });
    }
    let _place = queue(location)?;
    // The code may have changed while this settle waited its turn.
    let before = Snapshot::take(location, &gate)?;
    let start = std::time::Instant::now();
    let args = test_args();
    // The tests that failed recently first: an agent that has not fixed them hears so in the
    // time they take, not the suite's. The full suite then runs in its own order, so tests that
    // depend on it behave as before.
    let mut first = recent_failures(location);
    first.retain(|name| !main.contains(&format!("test {name}")));
    if !first.is_empty() {
        let workspace = Selection::workspace();
        let tried = run_tests(
            location,
            target,
            &args,
            &workspace.packages,
            &workspace,
            &first,
            &main,
        )?;
        let (failures, existing) = new_failures(&tried.run, &main);
        if tried.code != 0 && !failures.is_empty() {
            let after = Snapshot::take(location, &gate)?;
            let status = if after.fingerprint == before.fingerprint {
                Status::Fail
            } else {
                Status::Superseded
            };
            let verdict = Verdict {
                status,
                tests: tried.run.tests,
                failures,
                flaky: tried.flaky,
                partial: true,
                existing,
                reused: 0,
                full: None,
                skipped: Vec::new(),
                unbuilt: Vec::new(),
            };
            return finish(location, before, verdict, start, &tried.failed, true);
        }
    }
    let (tried, reused, full, unbuilt) = suite(
        location,
        target,
        &args,
        &before,
        known.as_ref(),
        reuse,
        &left,
    )?;
    let skipped: Vec<String> = left
        .targets
        .iter()
        .filter(|target| !unbuilt.contains(&target.package))
        .map(Target::describe)
        .collect();
    let unbuilt: Vec<String> = unbuilt.into_iter().collect();
    let (code, mut run, mut flaky, mut stderr) = (tried.code, tried.run, tried.flaky, tried.stderr);
    // A test target that failed naming no failed test: a binary killed by a signal (a UI test
    // crashed in libvulkan under load, with real agents on a private workspace), or one without
    // the harness (`harness = false`) that exited unsuccessfully. Run it once more, like a failed
    // test: (its rerun arguments, what it did, its failure).
    let mut crashed: Vec<(String, String, String)> = crashes(&String::from_utf8_lossy(&stderr))
        .into_iter()
        .map(|(rerun, signal)| {
            let failure = format!("CRASHED `{rerun}`: signal {signal}");
            (rerun, format!("crashed with signal {signal}"), failure)
        })
        .collect();
    for rerun in &run.silent {
        if !crashed.iter().any(|(seen, ..)| seen == rerun) {
            let failure = format!("FAILED `{rerun}`: it failed naming no failed test");
            crashed.push((rerun.clone(), "failed naming no test".to_owned(), failure));
        }
    }
    let mut crash_failures = Vec::new();
    for (rerun, what, failure) in &crashed {
        let mut retry = args.clone();
        retry.extend(rerun.split_whitespace().map(String::from));
        let mut again = Vec::new();
        let code = cargo::run_to(
            location,
            target,
            &location.worktree,
            &retry,
            &[],
            &mut again,
            &mut stderr,
        )?;
        let again = parse(&String::from_utf8_lossy(&again), &location.worktree);
        if code == 0 {
            run.tests += again.tests;
            flaky.push(format!("`{rerun}` ({what})"));
        } else {
            crash_failures.push(failure.clone());
        }
    }
    let after = Snapshot::take(location, &gate)?;
    let stderr = String::from_utf8_lossy(&stderr);
    let (mut failures, existing) = new_failures(&run, &main);
    failures.extend(crash_failures);
    // Cargo failed without a failed test or compile error: a tool failure (a killed build, say)
    // that would outlive its fix if stored. Main's failures explain it only when cargo said a
    // target failed, or main's code does not compile.
    let explained_by_main = existing > 0 && (run.failed_targets > 0 || run.compile_errors);
    let unexplained = code != 0 && failures.is_empty() && flaky.is_empty() && !explained_by_main;
    if unexplained {
        let last = stderr.lines().rev().find(|line| !line.trim().is_empty());
        failures.push(format!(
            "cargo test failed: {}",
            last.unwrap_or("no output")
        ));
    }
    let baseline_path = baseline_path(location);
    let baseline: Option<Baseline> = fs::read(&baseline_path)
        .ok()
        .and_then(|json| serde_json::from_slice(&json).ok());
    // A crashed binary's tests are not counted: the crash is the failure to report. Nor are the
    // tests main leaves out, which may have run in the last passing settle.
    if let Some(baseline) = baseline
        && run.tests < baseline.tests
        && !accept_fewer
        && crashed.is_empty()
        && skipped.is_empty()
        && unbuilt.is_empty()
    {
        failures.push(format!(
            "test count dropped from {} to {} since the last passing settle: if that was \
             deliberate, run `agent-hearth settle --accept-fewer`",
            baseline.tests, run.tests
        ));
    }
    let status = if after.fingerprint != before.fingerprint {
        Status::Superseded
    } else if failures.is_empty() {
        Status::Pass
    } else {
        Status::Fail
    };
    let verdict = Verdict {
        status,
        tests: run.tests,
        failures,
        flaky,
        partial: false,
        existing,
        reused,
        full,
        skipped,
        unbuilt,
    };
    finish(
        location,
        before,
        verdict,
        start,
        &tried.failed,
        !unexplained,
    )
}

/// Stores a settle's verdict where the Stop hook finds it, with the tests that `failed`, and
/// returns its report. Not stored: a verdict on code that changed during the run, one that failed
/// for reasons outside the code (`keep` false), and one from a sandbox.
fn finish(
    location: &Location,
    before: Snapshot,
    mut verdict: Verdict,
    start: std::time::Instant,
    failed: &[String],
    keep: bool,
) -> Result<Report> {
    // In an agent's sandbox, tests that open sockets fail: an agent's own settle in Codex's
    // stored such failures and the Stop hook, outside the sandbox, reused them
    // (`bench/sandbox-settle-linux1.txt`). Its failures are shown but not kept.
    let sandboxed = verdict.status == Status::Fail && !sockets_work();
    if sandboxed {
        verdict.failures.push(
            "this sandbox denies sockets, so tests may fail here that pass outside it: \
             this verdict is not kept, and the Stop hook settles outside the sandbox"
                .to_owned(),
        );
    }
    if verdict.status != Status::Superseded && keep && !sandboxed {
        check::store_verdict(location, &before.fingerprint, &verdict)?;
        let last = Last {
            fingerprint: before.fingerprint.clone(),
            verdict: verdict.clone(),
            seconds: start.elapsed().as_secs_f64(),
            reported: false,
        };
        let path = file(location, "last.json");
        fs::write(&path, serde_json::to_vec(&last)?)
            .with_context(|| format!("writing {}", path.display()))?;
        record_failures(location, failed)?;
    }
    if verdict.status == Status::Pass {
        let path = baseline_path(location);
        let json = serde_json::to_vec(&Baseline {
            tests: verdict.tests,
        })?;
        fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(Report {
        fingerprint: before.fingerprint,
        cached: false,
        verdict,
    })
}

/// One run of the tests `scope` selects (`--workspace`, or packages and their targets), part of
/// `within`, or only those of `within` named in `only` when it is not empty. Failed tests run
/// once more, by name within `within`: timing-sensitive tests fail under load (UI tests on a
/// private workspace, while other agents built), so those that pass then are flaky rather than
/// failed.
#[allow(clippy::too_many_arguments)]
fn run_tests(
    location: &Location,
    target: &Path,
    args: &[String],
    scope: &[String],
    within: &Selection,
    only: &[String],
    main: &BTreeSet<String>,
) -> Result<Tried> {
    let cwd = &location.worktree;
    // The tests `names` in `within`, a cargo run per kind of test (`Selection::named`), with
    // `extra` arguments for the test harness; the highest exit code.
    let named = |names: &[String], extra: &[&str], stdout: &mut Vec<u8>, stderr: &mut Vec<u8>| {
        let mut code = 0;
        for selection in within.named(names) {
            let mut run = args.to_vec();
            run.extend(selection);
            run.extend(extra.iter().map(|arg| arg.to_string()));
            code = code.max(cargo::run_to(
                location,
                target,
                cwd,
                &run,
                &[],
                &mut *stdout,
                &mut *stderr,
            )?);
        }
        anyhow::Ok(code)
    };
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    // The whole run in one stream, so each test binary's results are told apart (`Run::units`).
    let code = if only.is_empty() {
        let mut all = args.to_vec();
        all.extend(scope.iter().cloned());
        let code = cargo::run_merged(location, target, cwd, &all, &mut stdout)?;
        stderr.clone_from(&stdout);
        code
    } else {
        named(only, &[], &mut stdout, &mut stderr)?
    };
    let mut run = parse(&String::from_utf8_lossy(&stdout), cwd);
    // Main's failures fail again: no retry, and no place among the recent failures.
    let ours: Vec<String> = run
        .failed
        .iter()
        .filter(|name| !main.contains(&format!("test {name}")))
        .cloned()
        .collect();
    let (mut failed, mut flaky) = (ours.clone(), Vec::new());
    if code != 0 && !ours.is_empty() && !run.compile_errors {
        let mut again = Vec::new();
        // One at a time, and once no other settle runs: under six settles at once, atuin's
        // terminal tests failed, and again when retried at once, together or one at a time
        // (`bench/agents-atuin-codex-hearth-2.txt`, `agents-atuin-claude-hearth-1.txt`). Waiting
        // on the machine's load instead held a failed-first retry back while the settle's own
        // build had just loaded it (`bench/feedback-atuin-mac3.txt`).
        let quiet = std::time::Instant::now() + RETRY_WAIT;
        while settles_running(&places(location)) > 1 && std::time::Instant::now() < quiet {
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
        let code = named(&ours, &["--test-threads=1"], &mut again, &mut stderr)?;
        let again = parse(&String::from_utf8_lossy(&again), cwd);
        (failed, flaky) = ours
            .iter()
            .cloned()
            .partition(|name| again.failed.contains(name));
        // A retry that failed without naming a failed test explains nothing: keep the first run,
        // less what passed when run again.
        if code == 0 || !again.failed.is_empty() {
            let keep = |key: &String| {
                let name = key.strip_prefix("test ").unwrap_or_default();
                !flaky.iter().any(|passed| passed == name)
            };
            let kept: Vec<(String, String)> = run
                .failures
                .drain(..)
                .zip(run.keys.drain(..))
                .filter(|(_, key)| keep(key))
                .collect();
            (run.failures, run.keys) = kept.into_iter().unzip();
        }
    }
    Ok(Tried {
        code,
        run,
        failed,
        flaky,
        stderr,
    })
}

#[derive(Default)]
struct Tried {
    code: u8,
    run: Run,
    /// Tests that failed twice.
    failed: Vec<String>,
    /// Tests that failed, then passed when run again.
    flaky: Vec<String>,
    stderr: Vec<u8>,
}

impl Tried {
    fn add(&mut self, other: Tried) {
        self.code = self.code.max(other.code);
        self.run.add(other.run);
        self.failed.extend(other.failed);
        self.flaky.extend(other.flaky);
        self.stderr.extend(other.stderr);
    }
}

/// `--workspace` without `packages`.
fn excluding(packages: &[String]) -> Vec<String> {
    let mut scope = vec![WORKSPACE.to_owned()];
    for package in packages {
        scope.extend(["--exclude".to_owned(), package.clone()]);
    }
    scope
}

/// Runs the suite, or of it what the change can have reached with main's results for the rest:
/// the run, how many tests' results were main's, why the whole suite ran if it did, and the
/// packages of which nothing ran. `known`: main's failures, as the gate has them; while they are
/// unknown (the golden's run has recorded its results but not yet which failures are main's),
/// nothing is reused, as a reused failure could not be told from main's. `left`: what main
/// leaves out of the suite, which this run leaves out too (`selections`): a worktree is never
/// held to tests main never ran, and runs the rest of their packages' tests. Nothing is reused
/// then.
#[allow(clippy::type_complexity)]
fn suite(
    location: &Location,
    target: &Path,
    args: &[String],
    before: &Snapshot,
    known: Option<&BTreeSet<String>>,
    reuse: bool,
    left: &LeftOut,
) -> Result<(Tried, u64, Option<String>, BTreeSet<String>)> {
    let empty = BTreeSet::new();
    let main = known.unwrap_or(&empty);
    if !left.is_empty() {
        // Without metadata (a lockfile to update, say), the whole workspace, which main's
        // compile errors may stop: then that is said below.
        let (parts, mut unbuilt) = match cargo::metadata(&location.worktree, true) {
            Ok(metadata) => selections(&metadata, left),
            Err(_) => (vec![Selection::workspace()], BTreeSet::new()),
        };
        let mut tried = Tried::default();
        for part in &parts {
            for scope in part.runs() {
                let ran = run_tests(location, target, args, &scope, part, &[], main)?;
                // Stopped by main's own compile errors, which the worktree may now reach from
                // somewhere new: nothing of the packages they are in ran.
                let (new, _) = new_failures(&ran.run, main);
                if new.is_empty() && !ran.run.broken.is_empty() {
                    unbuilt.insert(
                        part.alone()
                            .map_or("the rest of the workspace", |package| package)
                            .to_owned(),
                    );
                }
                tried.add(ran);
            }
        }
        let why = "main leaves out what does not compile there".to_owned();
        return Ok((tried, 0, Some(why), unbuilt));
    }
    let planned = if !reuse {
        Err(format!("{}=off", reuse::SWITCH))
    } else if known.is_none() {
        Err("main's failures are not recorded yet".to_owned())
    } else {
        plan(location, target, args, before)
    };
    let workspace = Selection::workspace();
    let plan = match planned {
        Ok(plan) => plan,
        Err(why) => {
            let tried = run_tests(
                location,
                target,
                args,
                &workspace.packages,
                &workspace,
                &[],
                main,
            )?;
            return Ok((tried, 0, Some(why), BTreeSet::new()));
        }
    };
    let mut tried = Tried::default();
    if plan.run || plan.doctests {
        // The whole workspace, built as the golden built it; the runner skips main's binaries.
        // Doctests run as plain `cargo test` picks them: `--doc` picked 33 more of ruff's
        // crates, and 6 tests more than the whole suite runs.
        let mut scope = vec![
            WORKSPACE.to_owned(),
            "--config".to_owned(),
            runner(location, &plan.skip)?,
        ];
        if !plan.doctests {
            scope.push("--tests".to_owned());
        }
        tried.add(run_tests(
            location,
            target,
            args,
            &scope,
            &workspace,
            &[],
            main,
        )?);
    }
    let mut reused = 0;
    // A compile error stopped the run before any test ran: main's results stand in for no run.
    let stopped = tried.run.compile_errors;
    for (unit, outcome) in plan.reused.iter().filter(|_| !stopped) {
        // Selected along with what had to run.
        if tried.run.units.contains_key(unit) {
            continue;
        }
        reused += outcome.tests;
        tried.run.tests += outcome.tests;
        // Main's failures fail again; those that passed when main ran them again were flaky.
        for (name, failure) in &outcome.failed {
            let key = format!("test {name}");
            if main.contains(&key) && !tried.run.failures.contains(failure) {
                tried.run.failures.push(failure.clone());
                tried.run.keys.push(key);
            }
        }
    }
    Ok((tried, reused, None, BTreeSet::new()))
}

/// What the runner prints for a test binary it skips.
const REUSED: &str = "agent-hearth: main's results reused";

/// Writes the list of test binaries to skip, `skip`, and returns the `--config` setting that makes
/// cargo run every test binary through a runner that skips them. The runner is a script in the
/// setting itself, and the list the worktree's own, which only its settle (holding its lock)
/// writes: a shared script, rewritten by every settle, could be read cut short by another
/// worktree's, which then ran nothing and passed.
fn runner(location: &Location, skip: &[String]) -> Result<String> {
    let list = file(location, "skip");
    fs::create_dir_all(list.parent().context("the skip list has no directory")?)?;
    let mut names = skip.join("\n");
    names.push('\n');
    golden::write_atomically(&list, names.as_bytes())?;
    // `sh -c <script> <$0> <list> <binary> <arguments…>`, on one line: cargo quotes the command
    // in the line that says how a test binary ended (`(signal: 6, …)`, which `crashes` reads).
    let script = format!(
        "skip=$1; shift; name=${{1##*/}}; \
         while IFS= read -r reused; do \
         if [ \"$reused\" = \"$name\" ]; then echo \"{REUSED}\"; exit 0; fi; \
         done < \"$skip\"; exec \"$@\""
    );
    let toml = |text: &str| serde_json::to_string(text).unwrap_or_default();
    Ok(format!(
        "target.'cfg(all())'.runner = [\"sh\", \"-c\", {}, \"agent-hearth-runner\", {}]",
        toml(&script),
        toml(&list.to_string_lossy())
    ))
}

/// Why reuse cannot skip test binaries here, if it cannot: the runner needs a Unix shell, and a
/// runner the repository configures (in `.cargo/config.toml` here or above, or the environment)
/// would clash with it.
fn runner_problem(location: &Location) -> Option<String> {
    if !cfg!(unix) {
        return Some("skipping test binaries needs a Unix shell".to_owned());
    }
    if let Some((name, _)) = std::env::vars()
        .find(|(name, _)| name.starts_with("CARGO_TARGET_") && name.ends_with("_RUNNER"))
    {
        return Some(format!("{name} sets a runner"));
    }
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".cargo")));
    let dirs = location
        .worktree
        .ancestors()
        .map(|dir| dir.join(".cargo"))
        .chain(home);
    for dir in dirs {
        for name in ["config", "config.toml"] {
            let path = dir.join(name);
            if fs::read_to_string(&path).is_ok_and(|text| text.contains("runner")) {
                return Some(format!("{} sets a runner", path.display()));
            }
        }
    }
    None
}

/// What a settle of `before` must run, from the worktree's test build and what the golden's settle
/// recorded; otherwise why it runs the whole suite.
fn plan(
    location: &Location,
    target: &Path,
    args: &[String],
    before: &Snapshot,
) -> std::result::Result<reuse::Plan, String> {
    if let Some(problem) = runner_problem(location) {
        return Err(problem);
    }
    let golden = adopt::seeded_from(location)
        .ok()
        .flatten()
        .ok_or("no golden seeded this worktree")?;
    let main = reuse::load(&golden::dir(location, &golden));
    let mut build = args.to_vec();
    build.extend([WORKSPACE.to_owned(), "--no-run".to_owned()]);
    let mut out = Vec::new();
    let code = cargo::run_to(
        location,
        target,
        &location.worktree,
        &build,
        &[],
        &mut out,
        std::io::sink(),
    )
    .map_err(|err| format!("{err:#}"))?;
    let pinned = location.pinned().map(|(pin, _)| pin.join("src"));
    let paths = reuse::Paths {
        root: &location.worktree,
        dep_info_root: pinned.as_deref().unwrap_or(&location.worktree),
        map: &[],
        target,
    };
    let build = reuse::Build::parse(&String::from_utf8_lossy(&out), &paths);
    // A test target that does not compile, even on main, stops `cargo test` before any test
    // runs: the whole suite does the same.
    if code != 0 {
        return Err(format!("the test build failed (exit code {code})"));
    }
    let metadata =
        reuse::Metadata::of_tree(&location.worktree).map_err(|err| format!("{err:#}"))?;
    let plan = reuse::plan(main.as_ref(), &before.files, &build, &paths, &metadata);
    let plan = plan.map_err(|full| match full {
        reuse::Full::NoRecord => "main's results are not recorded".to_owned(),
        reuse::Full::Changed(path) => format!("{path} is not a compiled Rust source"),
        reuse::Full::Build(why) => why,
    })?;
    Ok(plan)
}

/// The tests that failed in the worktree's recent settles, those that failed most often first,
/// then the most recent; at most `FIRST`.
fn recent_failures(location: &Location) -> Vec<String> {
    let mut seen: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let history = failure_history(location);
    for (age, failed) in history.iter().enumerate() {
        for name in failed {
            seen.entry(name).or_insert((0, age)).0 += 1;
        }
    }
    let mut names: Vec<_> = seen.into_iter().collect();
    names.sort_by_key(|(_, (count, age))| (std::cmp::Reverse(*count), *age));
    names
        .into_iter()
        .take(FIRST)
        .map(|(name, _)| name.to_owned())
        .collect()
}

/// Adds a settle's failed tests (none when it passed) to the worktree's last `HISTORY`.
fn record_failures(location: &Location, failed: &[String]) -> Result<()> {
    let mut history = failure_history(location);
    history.insert(0, failed.to_vec());
    history.truncate(HISTORY);
    let path = file(location, "failed.json");
    fs::create_dir_all(path.parent().context("settle history has no parent")?)?;
    fs::write(&path, serde_json::to_vec(&history)?)
        .with_context(|| format!("writing {}", path.display()))
}

/// The tests that failed in each of the worktree's recent settles, the newest first.
fn failure_history(location: &Location) -> Vec<Vec<String>> {
    fs::read(file(location, "failed.json"))
        .ok()
        .and_then(|json| serde_json::from_slice(&json).ok())
        .unwrap_or_default()
}

pub enum Background {
    Started(PathBuf),
    AlreadyRunning,
}

/// Starts `<agent_hearth> settle` for the worktree as a detached process logging to a file,
/// unless a settle of it is already running.
pub fn settle_in_background(location: &Location, agent_hearth: &Path) -> Result<Background> {
    let lock = lock_file(location)?;
    match lock.try_lock() {
        Ok(()) => drop(lock),
        Err(TryLockError::WouldBlock) => return Ok(Background::AlreadyRunning),
        Err(TryLockError::Error(err)) => return Err(err).context("checking the settle lock"),
    }
    let log_path = file(location, "log");
    let log =
        File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
    let base = location.base();
    // At low CPU priority, so the builds an agent waits on come first.
    #[cfg(unix)]
    let mut command = {
        let mut nice = Command::new("nice");
        nice.args(["-n", "10"]).arg(agent_hearth);
        nice
    };
    #[cfg(not(unix))]
    let mut command = Command::new(agent_hearth);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(
        &mut command,
        BELOW_NORMAL_PRIORITY_CLASS,
    );
    command
        .arg("settle")
        .current_dir(&location.worktree)
        .env("AGENT_HEARTH_STATE_DIR", &base.state)
        .env("AGENT_HEARTH_CACHE_DIR", &base.cache)
        .env(DETACHED, "1")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    // Not waited for: it outlives this process.
    command.spawn().context("starting a background settle")?;
    Ok(Background::Started(log_path))
}

/// Whether a settle of the worktree is running now.
pub fn running(location: &Location) -> Result<bool> {
    match lock_file(location)?.try_lock() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(err)) => Err(err).context("checking the settle lock"),
    }
}

/// Test targets whose binary was killed by a signal, from cargo's stderr: the arguments cargo
/// gives to rerun it (`-p app --test ui`) and the signal (`6, SIGABRT: process abort signal`).
fn crashes(stderr: &str) -> Vec<(String, String)> {
    let mut crashes = Vec::new();
    let mut rerun = None;
    for line in stderr.lines() {
        if let Some(args) = line
            .strip_prefix("error: test failed, to rerun pass `")
            .and_then(|rest| rest.strip_suffix('`'))
        {
            rerun = Some(args.to_owned());
        } else if line.contains("process didn't exit successfully")
            && let Some(signal) = line.split("(signal: ").nth(1)
            && let Some(rerun) = rerun.take()
        {
            crashes.push((rerun, signal.trim_end_matches(')').to_owned()));
        }
    }
    crashes
}

/// Whether this process may listen on loopback TCP and on a Unix socket: Codex's sandbox denies
/// both, Claude Code's only the second (`bench/sandbox-commands-*.txt`). Both leave the temp
/// directory writable, and its path is short enough for a socket's.
fn sockets_work() -> bool {
    #[cfg(unix)]
    {
        // Unique per call: two settles in one process (tests) raced for one path, and the loser
        // looked sandboxed.
        static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("agent-hearth-{}-{call}.sock", std::process::id());
        let path = std::env::temp_dir().join(name);
        let unix = std::os::unix::net::UnixListener::bind(&path).is_ok();
        let _ = fs::remove_file(&path);
        if !unix {
            return false;
        }
    }
    TcpListener::bind(("127.0.0.1", 0)).is_ok()
}

/// The most settles at once. Memory decides below it: three at once on an 8-core M2 took 694s,
/// against about 1720s one at a time (`bench/feedback-atuin-mac2.txt`).
const PLACES: usize = 16;

/// Waits for a place among the machine's settles, whatever their repository, and holds it while
/// the settle runs. One settle may always run; another only while available memory covers a
/// settle, taken as a sixteenth of the machine, plus an eighth left to agents' builds. atuin's
/// settles peaked at 2-4 GB with ten at once on 62 GB (`bench/agents-atuin-codex-hearth-n10.txt`)
/// and under 1 GB on a 16 GB M2, where 4 GB plus a quarter never fitted and three settles ran one
/// at a time (`bench/feedback-atuin-mac3.txt`). Ten agents' settles one at a time reached the
/// last agent many minutes after it stopped; ten at once with no regard to memory filled 62 GB and
/// the swap.
fn queue(location: &Location) -> Result<File> {
    let room = || {
        memory::memory()
            .is_ok_and(|memory| memory.available >= memory.total / 16 + memory.total / 8)
    };
    take_place(&places(location), PLACES, room, "settles")
}

fn places(location: &Location) -> PathBuf {
    location.base().state.join("settle-places")
}

/// How many places in `dir` are held: settles running, this one's included.
fn settles_running(dir: &Path) -> usize {
    (0..PLACES)
        .filter(|n| {
            OpenOptions::new()
                .write(true)
                .open(dir.join(format!("{n}.lock")))
                .is_ok_and(|file| matches!(file.try_lock(), Err(TryLockError::WouldBlock)))
        })
        .count()
}

/// Takes the first free place of `places` in `dir`, the first always and the others while `room`
/// says so, waiting until one is, and saying once that it waits for `what`. Waiters go in the
/// order they came: each holds a ticket in `dir/queue` and tries for a place only while no older
/// ticket's waiter lives (without that, one could wait without bound while others passed it).
pub(crate) fn take_place(
    dir: &Path,
    places: usize,
    room: impl Fn() -> bool,
    what: &str,
) -> Result<File> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let ticket = Ticket::take(&dir.join("queue"))?;
    let mut said = false;
    loop {
        if ticket.first()? {
            for n in 0..places {
                if n > 0 && !room() {
                    break;
                }
                let path = dir.join(format!("{n}.lock"));
                let file = OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(&path)
                    .with_context(|| format!("opening {}", path.display()))?;
                match file.try_lock() {
                    Ok(()) => return Ok(file),
                    Err(TryLockError::WouldBlock) => {}
                    Err(TryLockError::Error(err)) => {
                        return Err(err).with_context(|| format!("taking a place among {what}"));
                    }
                }
            }
        }
        if !said {
            eprintln!("agent-hearth: waiting for other {what} to finish");
            said = true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// A waiter's place in line: a file named by when it came, locked while it waits (a crash frees
/// it), removed once it has a place.
struct Ticket {
    path: PathBuf,
    _lock: File,
}

impl Ticket {
    fn take(queue: &Path) -> Result<Self> {
        static TAKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        fs::create_dir_all(queue).with_context(|| format!("creating {}", queue.display()))?;
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let taken = TAKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!(
            "{:020}-{:010}-{taken:06}",
            since.as_nanos(),
            std::process::id()
        );
        // Locked before it appears under its name, so a visible ticket's lock says whether its
        // waiter lives.
        let staging = queue.join(format!(".{name}"));
        let lock =
            File::create(&staging).with_context(|| format!("creating {}", staging.display()))?;
        lock.lock().context("locking a ticket")?;
        let path = queue.join(&name);
        fs::rename(&staging, &path).with_context(|| format!("creating {}", path.display()))?;
        Ok(Self { path, _lock: lock })
    }

    /// Whether no waiter that came earlier still waits. Tickets of waiters that died are removed.
    fn first(&self) -> Result<bool> {
        let queue = self.path.parent().context("a ticket has no queue")?;
        let mine = self.path.file_name().unwrap_or_default();
        for entry in fs::read_dir(queue).with_context(|| format!("reading {}", queue.display()))? {
            let entry = entry?;
            let name = entry.file_name();
            if name.as_encoded_bytes().starts_with(b".") || name.as_os_str() >= mine {
                continue;
            }
            let Ok(file) = File::open(entry.path()) else {
                continue;
            };
            match file.try_lock() {
                Err(TryLockError::WouldBlock) => return Ok(false),
                Ok(()) => {
                    let _ = fs::remove_file(entry.path());
                }
                Err(TryLockError::Error(err)) => return Err(err).context("checking a ticket"),
            }
        }
        Ok(true)
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_file(location: &Location) -> Result<File> {
    let path = file(location, "lock");
    fs::create_dir_all(path.parent().context("settle lock has no parent")?)?;
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))
}

fn baseline_path(location: &Location) -> PathBuf {
    file(location, "json")
}

fn file(location: &Location, extension: &str) -> PathBuf {
    let target = adopt::target_dir(location);
    let id = target.file_name().unwrap_or_default();
    location
        .state_dir
        .join("settles")
        .join(Path::new(id).with_extension(extension))
}

#[derive(Default)]
struct Run {
    tests: u64,
    failures: Vec<String>,
    /// For each of `failures`, what matches it against main's: `test <name>`, or
    /// `diagnostic <Diagnostic::key>` for a compile error.
    keys: Vec<String>,
    /// Names of the failed tests, for a retry.
    failed: Vec<String>,
    /// Whether any failure is a compile error, which a retry cannot fix.
    compile_errors: bool,
    /// The targets with compile errors.
    broken: BTreeSet<Target>,
    /// Each test binary's (`bin <file name>`) and library's doctests' (`doc <crate>`) results,
    /// when cargo's own lines are in the output too ([`cargo::run_merged`]).
    units: BTreeMap<String, Outcome>,
    /// Of `units`, those that ran to the harness's `test result` line: one without it crashed,
    /// was cut off, or has no harness.
    finished: BTreeSet<String>,
    /// How many test targets cargo said failed (`error: test failed, to rerun pass …`).
    failed_targets: usize,
    /// Those of them that named no failed test (a crash, `harness = false`), by the arguments
    /// that rerun them, and their units.
    silent: Vec<String>,
    silent_units: BTreeSet<String>,
}

impl Run {
    fn add(&mut self, other: Run) {
        self.tests += other.tests;
        for (failure, key) in other.failures.into_iter().zip(other.keys) {
            if !self.failures.contains(&failure) {
                self.failures.push(failure);
                self.keys.push(key);
            }
        }
        self.failed.extend(other.failed);
        self.compile_errors |= other.compile_errors;
        self.broken.extend(other.broken);
        self.units.extend(other.units);
        self.finished.extend(other.finished);
        self.failed_targets += other.failed_targets;
        self.silent.extend(other.silent);
        self.silent_units.extend(other.silent_units);
    }

    /// The results a run of every test binary may leave to settles: those of units that finished
    /// and failed only by naming their failed tests. None when cargo failed (`code`) with no
    /// failed target or compile error to explain it: it was killed, say, and what it left is
    /// not main's whole result.
    fn sound_units(&self, code: u8) -> BTreeMap<String, Outcome> {
        if code != 0 && self.failed_targets == 0 && !self.compile_errors {
            return BTreeMap::new();
        }
        self.units
            .iter()
            .filter(|(unit, _)| self.finished.contains(*unit) && !self.silent_units.contains(*unit))
            .map(|(unit, outcome)| (unit.clone(), outcome.clone()))
            .collect()
    }
}

/// Reads `cargo test --message-format=json` output: compiler errors as JSON lines, and the test
/// harness's own lines for counts, failed tests and their panic messages. With cargo's stderr
/// merged in, its `Running` and `Doc-tests` lines tell which test binary each result is from.
fn parse(stdout: &str, worktree: &Path) -> Run {
    let mut run = Run::default();
    let mut failed: Vec<(String, Option<String>)> = Vec::new();
    let mut unit: Option<String> = None;
    let mut panics = BTreeMap::new();
    let mut lines = stdout.lines().peekable();
    while let Some(line) = lines.next() {
        if let Ok(message) = serde_json::from_str::<Value>(line) {
            if message["reason"] == "compiler-message"
                && message["message"]["level"] == "error"
                && let Some(diagnostic) = Diagnostic::from_rustc(&message["message"], worktree)
            {
                run.compile_errors = true;
                if let Some(target) = message["package_id"]
                    .as_str()
                    .and_then(package_name)
                    .and_then(|package| Target::of(package, &message["target"]))
                {
                    run.broken.insert(target);
                }
                let summary = diagnostic.summary();
                if !run.failures.contains(&summary) {
                    run.failures.push(summary);
                    run.keys.push(format!("diagnostic {}", diagnostic.key()));
                }
            }
            continue;
        }
        // Cargo's status lines, right-aligned: `     Running unittests src/lib.rs
        // (/t/debug/deps/app-1a2b)` and `   Doc-tests app`.
        if let Some(binary) = line
            .strip_prefix("     Running ")
            .and_then(|rest| rest.strip_suffix(')'))
            .and_then(|rest| rest.rsplit_once(" (").map(|(_, path)| path))
            .and_then(|path| Path::new(path).file_name())
        {
            unit = Some(format!("bin {}", binary.to_string_lossy()));
            run.units
                .entry(unit.clone().unwrap_or_default())
                .or_default();
        } else if line == REUSED {
            // Skipped by the runner: not run after all.
            if let Some(unit) = unit.take() {
                run.units.remove(&unit);
            }
        } else if let Some(name) = line.strip_prefix("   Doc-tests ") {
            unit = Some(format!("doc {}", name.trim()));
            run.units
                .entry(unit.clone().unwrap_or_default())
                .or_default();
        } else if let Some(result) = line.strip_prefix("test result: ") {
            run.tests += count(result);
            if let Some(unit) = &unit {
                run.units.entry(unit.clone()).or_default().tests += count(result);
                run.finished.insert(unit.clone());
            }
        } else if let Some(rerun) = line
            .strip_prefix("error: ")
            .and_then(|rest| rest.split_once(" failed, to rerun pass `"))
            .and_then(|(_, rest)| rest.strip_suffix('`'))
        {
            // Cargo's line after a test target that failed (`test` or `doctest`): it names a
            // failed test, or failed some other way.
            run.failed_targets += 1;
            let named = failed.iter().any(|(_, of)| of.is_some() && *of == unit);
            if !named {
                run.silent.push(rerun.to_owned());
                run.silent_units.extend(unit.clone());
            }
        } else if let Some(name) = line
            .strip_prefix("test ")
            .and_then(|rest| rest.strip_suffix(" ... FAILED"))
        {
            if !failed.iter().any(|(seen, _)| seen == name) {
                failed.push((name.to_owned(), unit.clone()));
            }
        } else if let Some(name) = line
            .strip_prefix("---- ")
            .and_then(|rest| rest.strip_suffix(" stdout ----"))
        {
            // The block's first line after "panicked at <place>:" is the message.
            while let Some(next) = lines.next_if(|next| !next.starts_with("---- ")) {
                if next.contains("panicked at")
                    && let Some(message) = lines.next_if(|m| !m.starts_with("---- "))
                {
                    panics.insert(name.to_owned(), message.trim().to_owned());
                    break;
                }
            }
        } else if line == "failures:"
            && lines
                .peek()
                .is_some_and(|next| next.starts_with("    ") && !next.trim().is_empty())
        {
            // The harness's closing list of failed tests, which a test's own output on the same
            // stream cannot split as it can a `test … FAILED` line.
            while let Some(next) = lines.next_if(|next| next.starts_with("    ")) {
                let name = next.trim();
                if !failed.iter().any(|(seen, _)| seen == name) {
                    failed.push((name.to_owned(), unit.clone()));
                }
            }
        }
    }
    for (name, unit) in failed {
        let failure = match panics.get(&name) {
            Some(message) => format!("FAILED {name}: {message}"),
            None => format!("FAILED {name}"),
        };
        if let Some(unit) = unit {
            let outcome = run.units.entry(unit).or_default();
            outcome.failed.push((name.clone(), failure.clone()));
        }
        if !run.failures.contains(&failure) {
            run.failures.push(failure);
            run.keys.push(format!("test {name}"));
        }
        run.failed.push(name);
    }
    run
}

/// The package name in a cargo package id: `path+file:///w/a#0.1.0`, `registry+…#serde@1.0.0`,
/// or before Cargo 1.77 `a 0.1.0 (path+file:///w/a)`.
fn package_name(id: &str) -> Option<String> {
    let name = match id.split_once('#') {
        Some((_, fragment)) if fragment.contains('@') => fragment.split('@').next()?,
        Some((url, _)) => url.rsplit('/').next()?,
        None => id.split_whitespace().next()?,
    };
    (!name.is_empty()).then(|| name.to_owned())
}

/// Passed, failed and ignored tests in a harness summary such as
/// `ok. 22 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.01s`.
fn count(summary: &str) -> u64 {
    summary
        .split(';')
        .filter_map(|part| {
            let mut words = part.split_whitespace().rev();
            let kind = words.next()?;
            let number = words.next()?.parse::<u64>().ok()?;
            ["passed", "failed", "ignored"]
                .contains(&kind)
                .then_some(number)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden;
    use crate::testing::Sandbox;

    #[test]
    fn parse_counts_tests_and_reports_failures_with_their_panics() {
        let stdout = "\
running 3 tests
test a::passes ... ok
test a::fails ... FAILED
test a::skipped ... ignored

failures:

---- a::fails stdout ----

thread 'a::fails' panicked at src/lib.rs:9:5:
assertion `left == right` failed
  left: 1
 right: 2
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    a::fails

test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s

running 1 test
test src/lib.rs - one (line 3) ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.20s
";
        let run = parse(stdout, Path::new("/w"));
        assert_eq!(run.tests, 4);
        assert_eq!(
            run.failures,
            ["FAILED a::fails: assertion `left == right` failed"]
        );
    }

    #[test]
    fn parse_tells_test_binaries_apart_in_merged_output() {
        let merged = "     Running unittests src/lib.rs (/t/debug/deps/a-1a2b)

running 2 tests
test one ... ok
test two ... 2026 a stray log line
FAILED

failures:

---- two stdout ----

thread 'two' panicked at src/lib.rs:9:5:
not two
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    two

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/cli.rs (/t/debug/deps/cli-3c4d)

running 1 test
test cli ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

   Doc-tests a

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
";
        let run = parse(merged, Path::new("/w"));
        assert_eq!(run.tests, 3);
        // The `FAILED` its line lost to the log line is in the closing list.
        assert_eq!(run.failures, ["FAILED two: not two"]);
        let unit = |name: &str| run.units.get(name).cloned().unwrap_or_default();
        assert_eq!(unit("bin a-1a2b").tests, 2);
        assert_eq!(
            unit("bin a-1a2b").failed,
            [("two".to_owned(), "FAILED two: not two".to_owned())]
        );
        assert_eq!(unit("bin cli-3c4d").tests, 1);
        assert!(run.units.contains_key("doc a"));
    }

    /// A doctest and a unit test in `a`, which `b` uses; `c` stands alone, and fails one test on
    /// main.
    const A: &str = "/// One.\n///\n/// ```\n/// assert_eq!(a::one(), 1);\n/// ```\npub fn one() -> u32 {\n    1\n}\n\n#[test]\nfn one_is_one() {\n    assert_eq!(one(), 1);\n}\n";

    /// The workspace of [`A`] on main, its golden settled, the checkout adopted.
    fn workspace(s: &Sandbox) -> (Location, PathBuf) {
        s.write(
            "repo/Cargo.toml",
            "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"3\"\n",
        );
        let package = |name: &str, deps: &str, src: &str| {
            s.write(
                &format!("repo/{name}/Cargo.toml"),
                &format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                     [dependencies]\n{deps}\n"
                ),
            );
            s.write(&format!("repo/{name}/src/lib.rs"), src);
        };
        package("a", "", A);
        // A build script, whose dep-info cargo does not name after its artifact.
        s.write(
            "repo/a/build.rs",
            "fn main() {\n    println!(\"cargo::rerun-if-changed=build.rs\");\n}\n",
        );
        package(
            "b",
            "a = { path = \"../a\" }",
            "pub fn two() -> u32 {\n    a::one() + 1\n}\n\n#[test]\nfn two_is_two() {\n    assert_eq!(two(), 2);\n}\n",
        );
        package(
            "c",
            "",
            "#[test]\nfn three() {\n    assert_eq!(3, 3);\n}\n\n#[test]\nfn needs_a_display() {\n    panic!(\"no display\");\n}\n",
        );
        s.write("repo/.gitignore", "/target/\n");
        s.run("repo", "git", &["init", "-q", "-b", "main"]);
        s.run("repo", "cargo", &["generate-lockfile", "--quiet"]);
        s.run("repo", "git", &["add", "."]);
        s.run("repo", "git", &["commit", "-q", "-m", "workspace"]);
        let repo = s.location("repo");
        let (golden, _) = golden::refresh(&repo, "main").unwrap();
        settle_golden(&repo, &golden.id).unwrap();
        assert!(golden::dir(&repo, &golden.id).join(reuse::MAIN).is_file());
        adopt::adopt(&repo).unwrap();
        let target = adopt::target_dir(&repo);
        (repo, target)
    }

    #[test]
    fn a_settle_that_reuses_main_s_results_agrees_with_the_whole_suite() {
        let s = Sandbox::new();
        let (repo, target) = workspace(&s);
        // The verdict with reuse, which must be the whole suite's, and how many results were
        // main's and whether the whole suite ran anyway.
        let settled = |label: &str| {
            let mut verdicts = [true, false].map(|reuse| {
                // A failure from the run before would run first, alone.
                let _ = fs::remove_file(file(&repo, "failed.json"));
                let report = settle_with(&repo, &target, true, reuse).unwrap();
                eprintln!("{label}, reuse {reuse}: {:?}", report.verdict);
                report.verdict
            });
            let reused = (verdicts[0].reused, verdicts[0].full.is_some());
            assert_eq!((verdicts[1].reused, verdicts[1].full.is_some()), (0, true));
            for verdict in &mut verdicts {
                (verdict.reused, verdict.full) = (0, None);
            }
            let [with, without] = verdicts;
            assert_eq!(format!("{with:?}"), format!("{without:?}"), "{label}");
            (
                with.status,
                with.tests,
                with.failures,
                with.existing,
                reused,
            )
        };
        // Nothing changed: every result is main's, its failure among them.
        assert_eq!(
            settled("no-op"),
            (Status::Pass, 5, vec![], 1, (5, false)),
            "a: unit and doctest, b: unit, c: two units"
        );

        // `a`'s code that `b` runs changed: `c`'s results are main's.
        let edited = A.replace("    1\n}", "    std::hint::black_box(1)\n}");
        s.write("repo/a/src/lib.rs", &edited);
        assert_eq!(
            settled("library edit"),
            (Status::Pass, 5, vec![], 1, (2, false))
        );

        // A test of the agent's own that fails is reported.
        let failing = "\n#[test]\nfn seeded() {\n    panic!(\"seeded\");\n}\n";
        s.write("repo/a/src/lib.rs", &format!("{edited}{failing}"));
        let (status, tests, failures, existing, (reused, full)) = settled("seeded failure");
        assert_eq!((status, tests, existing), (Status::Fail, 6, 1));
        assert_eq!(failures, ["FAILED seeded: seeded"]);
        assert!(reused >= 2 && !full, "{reused}");

        // A changed file that is not compiled Rust: the whole suite.
        s.write("repo/a/src/lib.rs", A);
        s.write("repo/a/fixture.txt", "data\n");
        assert_eq!(settled("fixture"), (Status::Pass, 5, vec![], 1, (0, true)));
        fs::remove_file(s.path("repo/a/fixture.txt")).unwrap();

        // A doctest edited: `a`'s doctests run again.
        s.write(
            "repo/a/src/lib.rs",
            &A.replace("a::one(), 1", "a::one() + 1, 2"),
        );
        let (status, tests, _, _, (reused, full)) = settled("doctest edit");
        assert_eq!((status, tests), (Status::Pass, 5));
        assert!((2..5).contains(&reused) && !full, "{reused}");

        // A doctest that fails: the same verdict either way.
        s.write(
            "repo/a/src/lib.rs",
            &A.replace("a::one(), 1", "a::one(), 2"),
        );
        let (_, tests, _, _, (reused, full)) = settled("failing doctest");
        assert_eq!(tests, 5);
        assert!(reused > 0 && !full, "{reused}");

        // A test that does not compile stops `cargo test` before any test runs: the whole
        // suite, which does the same.
        s.write("repo/a/src/lib.rs", A);
        s.write(
            "repo/a/tests/broken.rs",
            "#[test]\nfn broken() {\n    let _: u32 = \"no\";\n}\n",
        );
        let (status, _, failures, _, (reused, full)) = settled("compile error");
        assert_eq!(
            (status, failures.len(), reused, full),
            (Status::Fail, 1, 0, true)
        );
    }

    #[test]
    fn a_test_target_that_does_not_compile_on_main_leaves_out_that_target_only() {
        let s = Sandbox::new();
        s.write(
            "repo/b/tests/broken.rs",
            "#[test]\nfn broken() {\n    let _: u32 = \"no\";\n}\n",
        );
        let (repo, target) = workspace(&s);
        let golden = golden::current(&repo).unwrap().unwrap();
        let main = main_failures(&repo, &golden.id).unwrap();
        assert!(
            main.contains("test needs_a_display"),
            "the rest ran: {main:?}"
        );
        let left = left_out(&repo, &golden.id);
        assert_eq!(
            left.targets
                .iter()
                .map(Target::describe)
                .collect::<Vec<_>>(),
            ["b --test broken"]
        );
        assert!(left.unbuilt.is_empty());

        // Exactly what compiles runs, once each, with reuse or without: a's test and doctest,
        // b's unit test, c's two.
        for reuse in [true, false] {
            let _ = fs::remove_file(file(&repo, "failed.json"));
            let report = settle_with(&repo, &target, true, reuse).unwrap();
            let verdict = &report.verdict;
            assert_eq!(verdict.status, Status::Pass, "{verdict:?}");
            assert_eq!(verdict.tests, 5, "reuse {reuse}: {verdict:?}");
            assert_eq!(verdict.reused, 0, "{verdict:?}");
            assert_eq!(verdict.existing, 1, "{verdict:?}");
            assert_eq!(verdict.skipped, ["b --test broken"]);
            assert!(verdict.unbuilt.is_empty(), "{verdict:?}");
        }

        // The rest of b's tests run: breaking its unit test fails.
        s.write(
            "repo/b/src/lib.rs",
            "pub fn two() -> u32 {\n    a::one() + 2\n}\n\n#[test]\nfn two_is_two() {\n    assert_eq!(two(), 2);\n}\n",
        );
        let report = settle_with(&repo, &target, false, true).unwrap();
        assert_eq!(report.verdict.status, Status::Fail, "{:?}", report.verdict);
        assert!(
            report.verdict.failures[0].starts_with("FAILED two_is_two"),
            "{:?}",
            report.verdict
        );
        // And a new compile error elsewhere is the worktree's.
        s.write(
            "repo/c/src/lib.rs",
            "#[test]\nfn three() {\n    let _: u32 = \"three\";\n}\n",
        );
        let report = settle_with(&repo, &target, false, true).unwrap();
        assert_eq!(report.verdict.status, Status::Fail, "{:?}", report.verdict);
    }

    #[test]
    fn a_library_that_does_not_compile_on_main_runs_nothing_of_its_package_or_dependents() {
        let s = Sandbox::new();
        let (repo, target) = workspace(&s);
        // Main moves on: b's library stops compiling, and c comes to depend on it.
        s.write(
            "repo/b/src/lib.rs",
            "pub fn two() -> u32 {\n    \"two\"\n}\n\n#[test]\nfn two_is_two() {\n    assert_eq!(two(), 2);\n}\n",
        );
        s.write(
            "repo/c/Cargo.toml",
            "[package]\nname = \"c\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nb = { path = \"../b\" }\n",
        );
        s.run(
            "repo",
            "cargo",
            &["generate-lockfile", "--quiet", "--offline"],
        );
        s.run("repo", "git", &["commit", "-q", "-am", "b breaks"]);
        let (golden, _) = golden::refresh(&repo, "main").unwrap();
        settle_golden(&repo, &golden.id).unwrap();
        let left = left_out(&repo, &golden.id);
        assert_eq!(left.unbuilt, BTreeSet::from(["b".to_owned()]), "{left:?}");
        adopt::reseed(&repo).unwrap();

        let report = settle_with(&repo, &target, true, true).unwrap();
        let verdict = &report.verdict;
        assert_eq!(verdict.status, Status::Pass, "{verdict:?}");
        assert_eq!(verdict.tests, 2, "a's test and doctest: {verdict:?}");
        assert_eq!(verdict.unbuilt, ["b", "c"], "{verdict:?}");
        assert!(verdict.skipped.is_empty(), "{verdict:?}");
    }

    #[test]
    fn a_worktree_that_fixes_main_s_broken_target_is_not_held_to_its_tests() {
        let s = Sandbox::new();
        let broken = "#[test]\nfn broken() {\n    let _: u32 = \"no\";\n}\n\n#[test]\nfn fails_on_main_too() {\n    panic!(\"main never ran me\");\n}\n";
        s.write("repo/b/tests/broken.rs", broken);
        let (repo, target) = workspace(&s);
        s.write("repo/b/tests/broken.rs", &broken.replace("\"no\"", "1"));
        let report = settle_with(&repo, &target, false, true).unwrap();
        assert_eq!(report.verdict.status, Status::Pass, "{:?}", report.verdict);
        assert_eq!(report.verdict.skipped, ["b --test broken"]);
    }

    #[test]
    fn package_names_come_from_both_id_formats() {
        for (id, name) in [
            ("path+file:///w/repo/a#0.1.0", "a"),
            ("path+file:///w/repo/b#bee@0.1.0", "bee"),
            (
                "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0",
                "serde",
            ),
            ("a 0.1.0 (path+file:///w/repo/a)", "a"),
        ] {
            assert_eq!(package_name(id).as_deref(), Some(name), "{id}");
        }
    }

    fn settled(s: &Sandbox) -> (Location, PathBuf) {
        s.crate_repo("repo");
        let repo = s.location("repo");
        golden::refresh(&repo, "main").unwrap();
        adopt::adopt(&repo).unwrap();
        let target = adopt::target_dir(&repo);
        (repo, target)
    }

    #[test]
    fn settles_caches_and_guards_the_test_count() {
        let s = Sandbox::new();
        let (repo, target) = settled(&s);
        let first = settle(&repo, &target, false).unwrap();
        assert_eq!(first.verdict.status, Status::Pass, "{:?}", first.verdict);
        assert_eq!((first.cached, first.verdict.tests), (false, 1));
        assert!(settle(&repo, &target, false).unwrap().cached);
        assert_eq!(verdict(&repo).unwrap().unwrap().status, Status::Pass);

        s.write("repo/src/lib.rs", "pub fn one() -> u32 {\n    1\n}\n");
        let fewer = settle(&repo, &target, false).unwrap();
        assert_eq!(fewer.verdict.status, Status::Fail);
        assert!(fewer.verdict.failures[0].starts_with("test count dropped from 1 to 0"));
        let accepted = settle(&repo, &target, true).unwrap();
        assert_eq!(accepted.verdict.status, Status::Pass);
    }

    #[test]
    fn a_failing_test_is_one_line_with_its_panic() {
        let s = Sandbox::new();
        let (repo, target) = settled(&s);
        s.write(
            "repo/src/lib.rs",
            "pub fn one() -> u32 {\n    1\n}\n\n#[test]\nfn one_is_two() {\n    assert_eq!(one(), 2, \"one is not two\");\n}\n",
        );
        let report = settle(&repo, &target, false).unwrap();
        assert_eq!(report.verdict.status, Status::Fail);
        assert_eq!(
            report.verdict.failures.len(),
            1,
            "{:?}",
            report.verdict.failures
        );
        assert!(
            report.verdict.failures[0]
                .starts_with("FAILED one_is_two: assertion `left == right` failed: one is not two"),
            "{:?}",
            report.verdict.failures
        );
    }

    #[test]
    fn a_test_that_passes_when_run_again_is_flaky_not_failed() {
        let s = Sandbox::new();
        let (repo, target) = settled(&s);
        // Fails the first time it runs in this sandbox, passes after: a marker outside the
        // worktree, so the code itself does not change.
        let marker = s.path("flaky-marker");
        s.write(
            "repo/src/lib.rs",
            &format!(
                "pub fn one() -> u32 {{\n    1\n}}\n\n#[test]\nfn flaky_once() {{\n    \
                 let marker = std::path::Path::new({marker:?});\n    \
                 if !marker.exists() {{\n        std::fs::write(marker, \"\").unwrap();\n        \
                 panic!(\"first run\");\n    }}\n}}\n"
            ),
        );
        let report = settle(&repo, &target, false).unwrap();
        assert_eq!(report.verdict.status, Status::Pass, "{:?}", report.verdict);
        assert_eq!(report.verdict.flaky, ["flaky_once"]);
        assert!(report.verdict.failures.is_empty());
    }

    #[test]
    fn tests_that_failed_recently_run_first_and_alone_decide_a_failure() {
        let s = Sandbox::new();
        let (repo, target) = settled(&s);
        let lib = |tests: &str| format!("pub fn one() -> u32 {{\n    1\n}}\n{tests}");
        let broken = "\n#[test]\nfn one_is_two() {\n    assert_eq!(one(), 2);\n}\n";
        s.write("repo/src/lib.rs", &lib(broken));
        let full = settle(&repo, &target, false).unwrap();
        assert_eq!(full.verdict.status, Status::Fail);
        assert!(!full.verdict.partial);
        assert_eq!(recent_failures(&repo), ["one_is_two"]);

        // Still broken, and a new failure besides: only the recent one runs, and that decides.
        let also = "\n#[test]\nfn also_broken() {\n    assert_eq!(one(), 3);\n}\n";
        s.write("repo/src/lib.rs", &lib(&format!("{broken}{also}")));
        let first = settle(&repo, &target, false).unwrap();
        assert_eq!(first.verdict.status, Status::Fail);
        assert!(first.verdict.partial);
        assert_eq!(first.verdict.failures.len(), 1, "{:?}", first.verdict);
        assert!(first.verdict.failures[0].starts_with("FAILED one_is_two"));

        // Fixed: the recent failure passes, so the full suite runs and finds the other.
        let fixed = "\n#[test]\nfn one_is_one() {\n    assert_eq!(one(), 1);\n}\n";
        s.write("repo/src/lib.rs", &lib(&format!("{fixed}{also}")));
        let rest = settle(&repo, &target, false).unwrap();
        assert_eq!(rest.verdict.status, Status::Fail);
        assert!(!rest.verdict.partial);
        assert_eq!(rest.verdict.tests, 2);
        assert!(rest.verdict.failures[0].starts_with("FAILED also_broken"));
        // `one_is_two` failed in two of the three settles.
        assert_eq!(recent_failures(&repo), ["one_is_two", "also_broken"]);
    }

    #[test]
    fn a_test_that_fails_on_main_too_is_left_out_once_the_golden_s_run_has_found_it() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        // Main has a test that fails, as one needing a display would here.
        let lib = |tests: &str| format!("pub fn one() -> u32 {{\n    1\n}}\n{tests}");
        let main = "\n#[test]\nfn needs_a_display() {\n    panic!(\"no display\");\n}\n";
        s.write("repo/src/lib.rs", &lib(main));
        s.run(
            "repo",
            "git",
            &["commit", "-q", "-am", "a test that fails here"],
        );
        let repo = s.location("repo");
        let (golden, _) = golden::refresh(&repo, "main").unwrap();
        adopt::adopt(&repo).unwrap();
        let target = adopt::target_dir(&repo);

        // Before the golden's run: the failure is reported.
        let before = settle(&repo, &target, false).unwrap();
        assert_eq!(before.verdict.status, Status::Fail);

        settle_golden(&repo, &golden.id).unwrap();
        let found = main_failures(&repo, &golden.id).unwrap();
        assert_eq!(found, BTreeSet::from(["test needs_a_display".to_owned()]));
        let after = settle(&repo, &target, false).unwrap();
        assert!(
            !after.cached,
            "the verdict without main's failures was reused"
        );
        assert_eq!(after.verdict.status, Status::Pass, "{:?}", after.verdict);
        assert_eq!(after.verdict.existing, 1);

        // A failure of the agent's own is still one.
        let ours = "\n#[test]\nfn one_is_two() {\n    assert_eq!(one(), 2);\n}\n";
        s.write("repo/src/lib.rs", &lib(&format!("{main}{ours}")));
        let new = settle(&repo, &target, false).unwrap();
        assert_eq!(new.verdict.status, Status::Fail);
        assert_eq!(new.verdict.failures.len(), 1, "{:?}", new.verdict);
        assert!(new.verdict.failures[0].starts_with("FAILED one_is_two"));
        assert_eq!(new.verdict.existing, 1);
    }

    #[test]
    fn tests_run_by_name_skip_doctests_unless_they_are_doctests() {
        let names = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };
        let unit = "utils::tests::unquote";
        let doc = "src/lib.rs - one (line 3)";
        let old_doc = "src/lib.rs - one (line 3) - compile fail";
        let workspace = Selection::workspace();
        assert_eq!(
            workspace.named(&names(&[unit])),
            [names(&["--workspace", "--tests", "--", "--exact", unit])]
        );
        assert_eq!(
            workspace.named(&names(&[doc, old_doc])),
            [names(&["--workspace", "--doc", "--", doc, old_doc])]
        );
        assert_eq!(
            workspace.named(&names(&[unit, doc])),
            [
                names(&["--workspace", "--doc", "--", doc]),
                names(&["--workspace", "--tests", "--", "--exact", unit])
            ]
        );
        // A package alone, with the targets it selects: never one left out.
        let alone = Selection {
            packages: names(&["-p", "b"]),
            targets: Some(names(&["--lib", "--test", "ok"])),
            doc: true,
        };
        assert_eq!(
            alone.runs(),
            [
                names(&["-p", "b", "--lib", "--test", "ok"]),
                names(&["-p", "b", "--doc"])
            ]
        );
        assert_eq!(
            alone.named(&names(&[unit, doc])),
            [
                names(&["-p", "b", "--doc", "--", doc]),
                names(&["-p", "b", "--lib", "--test", "ok", "--", "--exact", unit])
            ]
        );
        // Only its doctests: those run, and run again by name.
        let doctests = Selection {
            targets: Some(Vec::new()),
            ..alone
        };
        assert_eq!(doctests.runs(), [names(&["-p", "b", "--doc"])]);
        assert_eq!(
            doctests.named(&names(&[doc])),
            [names(&["-p", "b", "--doc", "--", doc])]
        );
    }

    #[test]
    fn a_failing_doctest_fails_and_is_not_flaky() {
        let s = Sandbox::new();
        let (repo, target) = workspace(&s);
        s.write(
            "repo/a/src/lib.rs",
            &A.replace("a::one(), 1", "a::one(), 2"),
        );
        let report = settle_with(&repo, &target, false, false).unwrap();
        assert_eq!(report.verdict.status, Status::Fail, "{:?}", report.verdict);
        assert!(report.verdict.flaky.is_empty(), "{:?}", report.verdict);
        assert!(
            report
                .verdict
                .failures
                .iter()
                .any(|f| f.contains("a/src/lib.rs - one (line")),
            "{:?}",
            report.verdict
        );
    }

    #[test]
    fn recent_failures_put_the_most_frequent_then_the_newest_first() {
        let s = Sandbox::new();
        let (repo, _) = settled(&s);
        // Newest first: `b` failed twice, `a` twice but longer ago, `c` once.
        for failed in [&["c", "a"][..], &["a", "b"], &["b"], &[]] {
            let failed: Vec<String> = failed.iter().map(|name| name.to_string()).collect();
            record_failures(&repo, &failed).unwrap();
        }
        assert_eq!(recent_failures(&repo), ["b", "a", "c"]);
        for _ in 0..HISTORY {
            record_failures(&repo, &[]).unwrap();
        }
        assert!(
            recent_failures(&repo).is_empty(),
            "passing settles age failures out"
        );
    }

    #[test]
    fn crashes_are_signal_deaths_with_their_rerun_arguments() {
        let stderr = "\
error: test failed, to rerun pass `-p app --test ui`

Caused by:
  process didn't exit successfully: `/t/debug/deps/ui-1` (signal: 11, SIGSEGV: invalid memory reference)
error: test failed, to rerun pass `-p app --lib`

Caused by:
  process didn't exit successfully: `/t/debug/deps/app-2` (exit status: 101)
";
        assert_eq!(
            crashes(stderr),
            [(
                "-p app --test ui".to_owned(),
                "11, SIGSEGV: invalid memory reference".to_owned()
            )]
        );
    }

    #[test]
    fn a_crashing_test_binary_is_a_failure_or_flaky_when_it_passes_again() {
        let s = Sandbox::new();
        let (repo, target) = settled(&s);
        // Aborts the first time it runs in this sandbox, passes after.
        let marker = s.path("crash-marker");
        s.write(
            "repo/tests/crash.rs",
            &format!(
                "#[test]\nfn crash_once() {{\n    let marker = std::path::Path::new({marker:?});\n    \
                 if !marker.exists() {{\n        std::fs::write(marker, \"\").unwrap();\n        \
                 std::process::abort();\n    }}\n}}\n"
            ),
        );
        let report = settle(&repo, &target, false).unwrap();
        assert_eq!(report.verdict.status, Status::Pass, "{:?}", report.verdict);
        assert_eq!(report.verdict.flaky.len(), 1, "{:?}", report.verdict);
        assert!(report.verdict.flaky[0].starts_with("`--test crash` (crashed with signal 6"));

        s.write(
            "repo/tests/crash.rs",
            "#[test]\nfn crash_always() {\n    std::process::abort();\n}\n",
        );
        let report = settle(&repo, &target, false).unwrap();
        assert_eq!(report.verdict.status, Status::Fail);
        assert_eq!(report.verdict.failures.len(), 1, "{:?}", report.verdict);
        assert!(report.verdict.failures[0].starts_with("CRASHED `--test crash`: signal 6"));
    }

    #[test]
    fn a_settle_waits_for_a_place_and_takes_another_only_with_room() {
        let s = Sandbox::new();
        let dir = s.path("places");
        let waits = |places: usize, room: bool| {
            let held = take_place(&dir, places, || room, "settles").unwrap();
            std::thread::scope(|scope| {
                let waiter = scope.spawn(|| {
                    let start = std::time::Instant::now();
                    take_place(&dir, places, || room, "settles").unwrap();
                    start.elapsed()
                });
                std::thread::sleep(std::time::Duration::from_millis(700));
                drop(held);
                waiter.join().unwrap() >= std::time::Duration::from_millis(600)
            })
        };
        let held = take_place(&dir, 2, || true, "settles").unwrap();
        assert_eq!(settles_running(&dir), 1);
        drop(held);
        assert!(crate::testing::soon(|| settles_running(&dir) == 0));
        assert!(waits(1, true), "one place");
        assert!(waits(2, false), "no memory for a second");
        assert!(!waits(2, true), "a second place with room");
    }

    #[test]
    fn waiters_take_places_in_the_order_they_came() {
        let s = Sandbox::new();
        let dir = s.path("places");
        let held = take_place(&dir, 1, || true, "builds").unwrap();
        let order = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for n in 0..5 {
                let (dir, order) = (&dir, &order);
                scope.spawn(move || {
                    let place = take_place(dir, 1, || true, "builds").unwrap();
                    order.lock().unwrap().push(n);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    drop(place);
                });
                // Each in line before the next comes.
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            drop(held);
        });
        assert_eq!(*order.lock().unwrap(), [0, 1, 2, 3, 4]);
        // A waiter that died left a ticket nobody holds: it is passed over.
        fs::write(dir.join("queue/00000000000000000001-dead"), "").unwrap();
        drop(take_place(&dir, 1, || true, "builds").unwrap());
    }

    #[test]
    fn starts_one_background_settle_at_a_time() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let repo = s.location("repo");
        // `true` stands in for agent-hearth: the real binary is not built for unit tests.
        let started = settle_in_background(&repo, Path::new("true")).unwrap();
        assert!(matches!(started, Background::Started(log) if log.is_file()));
        let held = lock_file(&repo).unwrap();
        held.lock().unwrap();
        let busy = settle_in_background(&repo, Path::new("true")).unwrap();
        assert!(matches!(busy, Background::AlreadyRunning));
    }

    /// Settles the worktree with reuse and with the whole suite, asserts the two verdicts agree
    /// (but for how many results were main's and why the whole suite ran), and returns the one
    /// with reuse.
    fn agree(repo: &Location, target: &Path, label: &str) -> Verdict {
        let [with, without] = [true, false].map(|reuse| {
            let _ = fs::remove_file(file(repo, "failed.json"));
            let report = settle_with(repo, target, true, reuse).unwrap();
            eprintln!("{label}, reuse {reuse}: {:?}", report.verdict);
            report.verdict
        });
        let plain = |verdict: &Verdict| {
            let mut verdict = verdict.clone();
            (verdict.reused, verdict.full) = (0, None);
            format!("{verdict:?}")
        };
        assert_eq!(plain(&with), plain(&without), "{label}");
        with
    }

    /// Between the golden's record of results and its record of main's failures, a settle does
    /// not reuse results: it would not know which reused failures are main's.
    #[test]
    fn no_reuse_until_main_s_failures_are_recorded() {
        let s = Sandbox::new();
        let (repo, target) = workspace(&s);
        let golden = adopt::seeded_from(&repo).unwrap().unwrap();
        fs::remove_file(golden::dir(&repo, &golden).join(MAIN_FAILURES)).unwrap();
        let verdict = agree(&repo, &target, "main's failures unknown");
        assert_eq!(verdict.status, Status::Fail, "c fails, not known as main's");
        assert!(verdict.full.is_some(), "{verdict:?}");
    }

    /// A package: its name, its manifest's lines after `[package]`, and files under it.
    type Package<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);

    /// A workspace of `packages` on main, its golden settled, the checkout adopted.
    fn members(s: &Sandbox, packages: &[Package]) -> (Location, PathBuf) {
        members_and(s, packages, &[])
    }

    /// [`members`], but those of `packages` named in `outside` are not members (`exclude`).
    fn members_and(s: &Sandbox, packages: &[Package], outside: &[&str]) -> (Location, PathBuf) {
        let quoted = |names: Vec<&str>| {
            let names: Vec<String> = names.iter().map(|name| format!("{name:?}")).collect();
            names.join(", ")
        };
        let names = packages.iter().map(|(name, ..)| *name);
        s.write(
            "repo/Cargo.toml",
            &format!(
                "[workspace]\nmembers = [{}]\nexclude = [{}]\nresolver = \"3\"\n",
                quoted(names.filter(|name| !outside.contains(name)).collect()),
                quoted(outside.to_vec())
            ),
        );
        for (name, manifest, files) in packages {
            s.write(
                &format!("repo/{name}/Cargo.toml"),
                &format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
                     {manifest}\n"
                ),
            );
            for (path, text) in *files {
                s.write(&format!("repo/{name}/{path}"), text);
            }
        }
        s.write("repo/.gitignore", "/target/\n");
        s.run("repo", "git", &["init", "-q", "-b", "main"]);
        s.run(
            "repo",
            "cargo",
            &["generate-lockfile", "--quiet", "--offline"],
        );
        s.run("repo", "git", &["add", "."]);
        s.run("repo", "git", &["commit", "-q", "-m", "workspace"]);
        let repo = s.location("repo");
        let (golden, _) = golden::refresh(&repo, "main").unwrap();
        settle_golden(&repo, &golden.id).unwrap();
        adopt::adopt(&repo).unwrap();
        let target = adopt::target_dir(&repo);
        (repo, target)
    }

    /// A test binary that crashed when the golden ran it is not main's clean pass.
    #[test]
    fn a_test_binary_that_crashed_on_main_is_not_reused() {
        let s = Sandbox::new();
        let crash = "#[test]\nfn crash() {\n    std::process::abort();\n}\n";
        let (repo, target) = members(
            &s,
            &[
                ("a", "", &[("src/lib.rs", A)]),
                ("c", "", &[("src/lib.rs", ""), ("tests/crash.rs", crash)]),
            ],
        );
        s.write(
            "repo/a/src/lib.rs",
            &A.replace("    1\n}", "    std::hint::black_box(1)\n}"),
        );
        let verdict = agree(&repo, &target, "crashes on main");
        assert_eq!(verdict.status, Status::Fail);
        assert!(
            verdict.failures.iter().any(|f| f.starts_with("CRASHED")),
            "{verdict:?}"
        );
    }

    /// A test target that fails without naming a failed test (`harness = false`) fails the
    /// settle, though main has failures of its own.
    #[test]
    fn a_target_that_fails_naming_no_test_fails_beside_main_s_failures() {
        let s = Sandbox::new();
        let main_fails = "#[test]\nfn needs_a_display() {\n    panic!(\"no display\");\n}\n";
        let custom = |code: u8| format!("fn main() {{\n    std::process::exit({code});\n}}\n");
        let (repo, target) = members(
            &s,
            &[
                ("c", "", &[("src/lib.rs", main_fails)]),
                (
                    "d",
                    "\n[[test]]\nname = \"custom\"\nharness = false",
                    &[("src/lib.rs", ""), ("tests/custom.rs", &custom(0))],
                ),
            ],
        );
        s.write("repo/d/tests/custom.rs", &custom(1));
        let verdict = agree(&repo, &target, "harness = false fails");
        assert_eq!(verdict.status, Status::Fail, "{verdict:?}");
        assert_eq!(verdict.existing, 1, "{verdict:?}");
    }

    /// Another worktree's settle leaves alone every file this one's runner reads: it may be
    /// running it.
    #[test]
    fn a_settle_s_runner_is_its_own() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        s.run("repo", "git", &["worktree", "add", "-q", "../wt"]);
        let (one, two) = (s.location("repo"), s.location("wt"));
        let setting = runner(&one, &["a-1".to_owned()]).unwrap();
        let read = |setting: &str| -> Vec<(PathBuf, std::time::SystemTime, u64)> {
            setting
                .split('"')
                .map(PathBuf::from)
                .filter(|path| path.is_file())
                .map(|path| {
                    let metadata = fs::metadata(&path).unwrap();
                    (path, metadata.modified().unwrap(), metadata.len())
                })
                .collect()
        };
        let before = read(&setting);
        std::thread::sleep(std::time::Duration::from_millis(20));
        runner(&two, &["b-1".to_owned()]).unwrap();
        assert_eq!(read(&setting), before);
    }

    /// A unit test that links another package's Rust dylib runs again when that library changed.
    #[test]
    fn a_unit_test_linking_a_workspace_dylib_runs_again_when_it_changed() {
        let s = Sandbox::new();
        let b = "#[test]\nfn value_is_one() {\n    assert_eq!(a::value(), 1);\n}\n";
        // Linking a Rust dylib into a test binary needs std as a shared library too.
        s.write(
            "repo/.cargo/config.toml",
            "[build]\nrustflags = [\"-C\", \"prefer-dynamic\"]\n",
        );
        let (repo, target) = members(
            &s,
            &[
                (
                    "a",
                    "\n[lib]\ncrate-type = [\"dylib\"]",
                    &[("src/lib.rs", "pub fn value() -> u32 {\n    1\n}\n")],
                ),
                (
                    "b",
                    "\n[dependencies]\na = { path = \"../a\" }",
                    &[("src/lib.rs", b)],
                ),
            ],
        );
        s.write("repo/a/src/lib.rs", "pub fn value() -> u32 {\n    2\n}\n");
        let verdict = agree(&repo, &target, "the dylib changed");
        assert_eq!(verdict.status, Status::Fail);
    }

    /// A library whose doctests have no identity here (a path dev-dependency for another
    /// platform, not a member of the workspace, was not built) runs them: they are never left
    /// out.
    #[test]
    fn doctests_without_an_identity_run() {
        let s = Sandbox::new();
        let (repo, target) = members_and(
            &s,
            &[
                (
                    "a",
                    "\n[target.'cfg(windows)'.dev-dependencies]\nw = { path = \"../w\" }",
                    &[("src/lib.rs", A)],
                ),
                ("w", "", &[("src/lib.rs", "")]),
            ],
            &["w"],
        );
        s.write(
            "repo/a/src/lib.rs",
            &A.replace("    1\n}", "    std::hint::black_box(1)\n}"),
        );
        let verdict = agree(&repo, &target, "a's library changed");
        assert_eq!((verdict.status, verdict.tests), (Status::Pass, 2));
    }

    /// A test that compiles workspace code while it runs (trybuild's, here one that reads the
    /// proc macro's source as trybuild's compile would) does not keep main's pass when that
    /// code changed, though its binary did not.
    #[test]
    fn a_test_that_compiles_workspace_code_runs_again_when_that_code_changed() {
        let s = Sandbox::new();
        let check = "pub fn check(path: &str, expected: &str) {\n    \
                     let text = std::fs::read_to_string(path).unwrap();\n    \
                     assert!(text.contains(expected), \"{expected} is not in {path}\");\n}\n";
        let macro_ = "use proc_macro::TokenStream;\n\n#[proc_macro]\n\
                      pub fn named(_: TokenStream) -> TokenStream {\n    \
                      \"compile_error!(\\\"expected an identifier\\\")\".parse().unwrap()\n}\n";
        let ui = "#[test]\nfn ui() {\n    trybuild::check(\n        \
                  concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/src/lib.rs\"),\n        \
                  \"expected an identifier\",\n    );\n}\n";
        let (repo, target) = members(
            &s,
            &[
                ("trybuild", "", &[("src/lib.rs", check)]),
                (
                    "m",
                    "[lib]\nproc-macro = true\n\n[dev-dependencies]\n\
                     trybuild = { path = \"../trybuild\" }",
                    &[("src/lib.rs", macro_), ("tests/ui.rs", ui)],
                ),
            ],
        );
        s.write(
            "repo/m/src/lib.rs",
            &macro_.replace("expected an identifier", "expected a name"),
        );
        let verdict = agree(&repo, &target, "proc macro's message changed");
        assert_eq!(verdict.status, Status::Fail);
    }

    /// The same for a test that runs cargo itself.
    #[test]
    fn a_test_that_runs_cargo_runs_again_after_any_change() {
        let s = Sandbox::new();
        let lib = "pub fn one() -> u32 {\n    1\n}\n";
        // Stands in for building `a` with cargo: what it would compile, it reads.
        let build = "#[test]\nfn builds() {\n    let _cargo = env!(\"CARGO\");\n    \
                     let lib = concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../a/src/lib.rs\");\n    \
                     assert!(std::fs::read_to_string(lib).unwrap().contains(\"1\"));\n}\n";
        let (repo, target) = members(
            &s,
            &[
                ("a", "", &[("src/lib.rs", lib)]),
                ("x", "", &[("src/lib.rs", ""), ("tests/build.rs", build)]),
            ],
        );
        s.write("repo/a/src/lib.rs", &lib.replace('1', "2"));
        let verdict = agree(&repo, &target, "a changed");
        assert_eq!(verdict.status, Status::Fail);
    }
}
