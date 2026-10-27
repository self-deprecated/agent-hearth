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

/// Windows' `BELOW_NORMAL_PRIORITY_CLASS` process creation flag.
#[cfg(windows)]
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

/// Part of every settle fingerprint, as `check`'s gate spec is of its.
const SETTLE: &str = "settle 2: cargo test --workspace --no-fail-fast; main's failures";
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
/// How long a retry of failed tests waits for other settles to finish; tests do not wait on the
/// machine they run on.
const RETRY_WAIT: std::time::Duration = if cfg!(test) {
    std::time::Duration::ZERO
} else {
    std::time::Duration::from_secs(180)
};
const TEST: [&str; 4] = [
    "test",
    "--workspace",
    "--no-fail-fast",
    "--message-format=json",
];
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
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// The settle gate for the worktree, and the failures of the main its golden was built from:
/// tests that fail there too (they need a display, a service, the network the machine lacks) are
/// not the agent's to fix. Until the golden's own run has found them, there are none, and the
/// gate differs, so a verdict reached without them is not reused once they are known.
fn gate(location: &Location) -> Result<(String, BTreeSet<String>)> {
    let main = match adopt::seeded_from(location)? {
        Some(id) => main_failures(location, &id),
        None => None,
    };
    let key = match &main {
        Some(failures) => serde_json::to_string(failures)?,
        None => "unknown".to_owned(),
    };
    Ok((format!("{SETTLE}: {key}"), main.unwrap_or_default()))
}

/// The failures main has in the golden `id`, once its run of the suite has found them.
fn main_failures(location: &Location, id: &str) -> Option<BTreeSet<String>> {
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
    let (src, target) = (dir.join("src"), dir.join("target"));
    let pinned = location.pinned();
    let (pin, mounts) = match &pinned {
        Some((pin, mounts)) => (Some(*pin), mounts.as_slice()),
        None => (None, &[][..]),
    };
    // Diagnostics name files under the path the golden builds at.
    let root = pin.map_or_else(|| src.clone(), |pin| pin.join("src"));
    let args = test_args();
    let run = |names: &[String]| -> Result<Run> {
        let mut command = cargo::command(&src, &target, Path::new(""), pin, mounts);
        command.args(&args);
        if !names.is_empty() {
            command.args(["--", "--exact"]).args(names);
        }
        let output = command
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .context("running the golden's tests")?;
        Ok(parse(&String::from_utf8_lossy(&output.stdout), &root))
    };
    let first = run(&[])?;
    let mut failures: BTreeSet<String> = first
        .keys
        .iter()
        .filter(|key| key.starts_with("diagnostic "))
        .cloned()
        .collect();
    // Those that pass when run again are flaky, which a settle retries anyway.
    if !first.failed.is_empty() {
        let again = run(&first.failed)?;
        failures.extend(again.failed.iter().map(|name| format!("test {name}")));
    }
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
    Ok(Snapshot::take(location, &gate(location)?.0)?.fingerprint)
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
    let snapshot = Snapshot::take(location, &gate(location)?.0)?;
    check::load_verdict(location, &snapshot.fingerprint)
}

/// Runs the full suite into `target` and stores its verdict. Fails when fewer tests ran than in
/// the worktree's last passing settle, unless `accept_fewer`.
pub fn settle(location: &Location, target: &Path, accept_fewer: bool) -> Result<Report> {
    let lock = lock_file(location)?;
    lock.lock()
        .context("waiting for another settle of this worktree")?;
    let (gate, main) = gate(location)?;
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
        let tried = run_tests(location, target, &args, &first, &main)?;
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
            };
            return finish(location, before, verdict, start, &tried.failed, true);
        }
    }
    let tried = run_tests(location, target, &args, &[], &main)?;
    let (code, mut run, mut flaky, mut stderr) = (tried.code, tried.run, tried.flaky, tried.stderr);
    let crashed = crashes(&String::from_utf8_lossy(&stderr));
    // A test binary killed by a signal names no failed test (a UI test crashed in libvulkan under
    // load, with real agents on a private workspace): run that target once more, like a failed
    // test.
    let mut crash_failures = Vec::new();
    for (rerun, signal) in &crashed {
        let mut retry: Vec<String> = args
            .iter()
            .filter(|arg| *arg != "--workspace")
            .cloned()
            .collect();
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
            flaky.push(format!("`{rerun}` (crashed with signal {signal})"));
        } else {
            crash_failures.push(format!("CRASHED `{rerun}`: signal {signal}"));
        }
    }
    let after = Snapshot::take(location, &gate)?;
    let stderr = String::from_utf8_lossy(&stderr);
    let (mut failures, existing) = new_failures(&run, &main);
    failures.extend(crash_failures);
    // Cargo failed without a failed test or compile error: a tool failure (a killed build, say)
    // that would outlive its fix if stored.
    let unexplained = code != 0 && failures.is_empty() && flaky.is_empty() && existing == 0;
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
    // A crashed binary's tests are not counted: the crash is the failure to report.
    if let Some(baseline) = baseline
        && run.tests < baseline.tests
        && !accept_fewer
        && crashed.is_empty()
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

/// One run of the tests, only those named in `only` when it is not empty. Failed tests run once
/// more: timing-sensitive tests fail under load (UI tests on a private workspace, while other
/// agents built), so those that pass then are flaky rather than failed.
fn run_tests(
    location: &Location,
    target: &Path,
    args: &[String],
    only: &[String],
    main: &BTreeSet<String>,
) -> Result<Tried> {
    let with = |names: &[String]| {
        let mut args = args.to_vec();
        if !names.is_empty() {
            args.extend(targets(names).map(str::to_owned));
            args.extend(["--".to_owned(), "--exact".to_owned()]);
            args.extend(names.iter().cloned());
        }
        args
    };
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let cwd = &location.worktree;
    let code = cargo::run_to(
        location,
        target,
        cwd,
        &with(only),
        &[],
        &mut stdout,
        &mut stderr,
    )?;
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
        let mut retried = with(&ours);
        retried.push("--test-threads=1".to_owned());
        let code = cargo::run_to(
            location,
            target,
            cwd,
            &retried,
            &[],
            &mut again,
            &mut stderr,
        )?;
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

struct Tried {
    code: u8,
    run: Run,
    /// Tests that failed twice.
    failed: Vec<String>,
    /// Tests that failed, then passed when run again.
    flaky: Vec<String>,
    stderr: Vec<u8>,
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

/// The cargo targets that can hold the tests `names`: doctests (`src/lib.rs - item (line 3)`)
/// or the others. Without it, running a few tests by name still built and ran every crate's
/// doctests: 73s for one failing unit test on atuin on an M2 (`bench/feedback-atuin-mac1.txt`).
fn targets(names: &[String]) -> Option<&'static str> {
    // Older rustdoc adds ` - compile fail` and the like after the line.
    let doctest = |name: &String| name.contains(" - ") && name.contains(" (line ");
    if names.iter().all(doctest) {
        Some("--doc")
    } else if names.iter().any(doctest) {
        None
    } else {
        Some("--tests")
    }
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
    take_place(&places(location), PLACES, room)
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
/// says so, waiting until one is.
fn take_place(dir: &Path, places: usize, room: impl Fn() -> bool) -> Result<File> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut said = false;
    loop {
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
                    return Err(err).context("taking a place among the settles");
                }
            }
        }
        if !said {
            eprintln!("agent-hearth: waiting for other settles to finish");
            said = true;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
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
}

/// Reads `cargo test --message-format=json` output: compiler errors as JSON lines, and the test
/// harness's own lines for counts, failed tests and their panic messages.
fn parse(stdout: &str, worktree: &Path) -> Run {
    let mut tests = 0;
    let mut failures = Vec::new();
    let mut keys = Vec::new();
    let mut failed = Vec::new();
    let mut compile_errors = false;
    let mut panics = BTreeMap::new();
    let mut lines = stdout.lines().peekable();
    while let Some(line) = lines.next() {
        if let Ok(message) = serde_json::from_str::<Value>(line) {
            if message["reason"] == "compiler-message"
                && message["message"]["level"] == "error"
                && let Some(diagnostic) = Diagnostic::from_rustc(&message["message"], worktree)
            {
                compile_errors = true;
                let summary = diagnostic.summary();
                if !failures.contains(&summary) {
                    failures.push(summary);
                    keys.push(format!("diagnostic {}", diagnostic.key()));
                }
            }
            continue;
        }
        if let Some(result) = line.strip_prefix("test result: ") {
            tests += count(result);
        } else if let Some(name) = line
            .strip_prefix("test ")
            .and_then(|rest| rest.strip_suffix(" ... FAILED"))
        {
            failed.push(name.to_owned());
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
        }
    }
    for name in &failed {
        let failure = match panics.get(name) {
            Some(message) => format!("FAILED {name}: {message}"),
            None => format!("FAILED {name}"),
        };
        if !failures.contains(&failure) {
            failures.push(failure);
            keys.push(format!("test {name}"));
        }
    }
    Run {
        tests,
        failures,
        keys,
        failed,
        compile_errors,
    }
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
        assert_eq!(targets(&names(&["utils::tests::unquote"])), Some("--tests"));
        assert_eq!(
            targets(&names(&["src/lib.rs - one (line 3)"])),
            Some("--doc")
        );
        assert_eq!(
            targets(&names(&["src/lib.rs - one (line 3) - compile fail"])),
            Some("--doc")
        );
        assert_eq!(
            targets(&names(&[
                "utils::tests::unquote",
                "src/lib.rs - one (line 3)"
            ])),
            None
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
            let held = take_place(&dir, places, || room).unwrap();
            std::thread::scope(|scope| {
                let waiter = scope.spawn(|| {
                    let start = std::time::Instant::now();
                    take_place(&dir, places, || room).unwrap();
                    start.elapsed()
                });
                std::thread::sleep(std::time::Duration::from_millis(700));
                drop(held);
                waiter.join().unwrap() >= std::time::Duration::from_millis(600)
            })
        };
        let held = take_place(&dir, 2, || true).unwrap();
        assert_eq!(settles_running(&dir), 1);
        drop(held);
        assert_eq!(settles_running(&dir), 0);
        assert!(waits(1, true), "one place");
        assert!(waits(2, false), "no memory for a second");
        assert!(!waits(2, true), "a second place with room");
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
}
