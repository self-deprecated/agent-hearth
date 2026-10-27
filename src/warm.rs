//! Warming: after each edit the agent makes, and after each of its builds, build the worktree's
//! code in the background with the agent's recent build commands, so its own next build finds
//! most of the work done: after an edit, the same command; after a build, the next one (`check`
//! then `test`, `test` then `clippy`). Nothing a warm prints reaches the agent, so its failures on
//! half-finished edits cost nothing. Each edit's warm stops one still building older code,
//! keeping the crates it finished. An agent's own cargo run comes first: it waits for a warm
//! building the same command on the same code to finish it, and stops any other.
//!
//! ```text
//! <state>/warms/<worktree-id>.lock      held by a running warm, and shared by agents' cargo runs
//! <state>/warms/<worktree-id>.next      held by the one warm waiting to run next
//! <state>/warms/<worktree-id>.yield     there while an agent's cargo run waits for a warm
//! <state>/warms/<worktree-id>.json      the warm running now, and the code last warmed
//! <state>/warms/<worktree-id>.commands  the agent's recent build commands, which a warm repeats
//! <state>/warms/<worktree-id>.log       what the last warms printed
//! ```

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::adopt;
use crate::cargo;
use crate::check::Snapshot;
use crate::locate::Location;

/// Part of the warm fingerprint, which is only ever compared with itself.
const WARM: &str = "warm 1";
/// How many of the agent's most recent distinct build commands a warm repeats.
const COMMANDS: usize = 3;
/// The Stop hook's `check` builds this, so a warm always does too.
pub const CHECK: [&str; 3] = ["clippy", "--workspace", "--all-targets"];
/// What a settle builds.
pub const TEST: [&str; 2] = ["test", "--workspace"];

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// The running warm's process, also its process group.
    pid: Option<u32>,
    /// The fingerprint of the code the running warm builds.
    building: Option<String>,
    /// The command it runs now.
    #[serde(default)]
    command: Option<Vec<String>>,
    /// The fingerprint of the code the last finished warm built.
    warmed: Option<String>,
}

/// Starts `agent-hearth warm` for the worktree, detached and at low CPU priority, if it has a
/// build directory and `AGENT_HEARTH_WARM` is not `off`. Returns at once.
pub fn start(location: &Location, agent_hearth: &Path) -> Result<()> {
    let off = std::env::var_os("AGENT_HEARTH_WARM").is_some_and(|value| value == "off");
    if off || !cfg!(unix) || !adopt::target_dir(location).is_dir() {
        return Ok(());
    }
    let log = file(location, "log");
    fs::create_dir_all(log.parent().context("warm log has no parent")?)?;
    let log = File::create(&log).with_context(|| format!("creating {}", log.display()))?;
    let base = location.base();
    let mut command = Command::new("nice");
    command
        .args(["-n", "10"])
        .arg(agent_hearth)
        .arg("warm")
        .current_dir(&location.worktree)
        .env("AGENT_HEARTH_STATE_DIR", &base.state)
        .env("AGENT_HEARTH_CACHE_DIR", &base.cache)
        // So that tools watching cargo runs (bench/agents.sh's shim) can tell a warm's from the
        // agent's own.
        .env("AGENT_HEARTH_WARMING", "1")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // Its own process group, so that stopping it stops its cargo and compilers too.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command.spawn().context("starting a warm")?;
    Ok(())
}

/// Builds the worktree's current code with the agent's recent build commands and the Stop hook's
/// clippy, unless it was warmed already or another warm is waiting to run after the current one.
pub fn warm(location: &Location, target: &Path) -> Result<()> {
    // One warm runs and at most one waits: edits while one runs need one more round, not one each.
    let next = open(location, "next")?;
    match next.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(()),
        Err(TryLockError::Error(err)) => return Err(err).context("taking the warm queue"),
    }
    let running = open(location, "lock")?;
    match running.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            // A warm of code that has changed since builds what the agent will not ask for:
            // with edits that only compile together, it built the broken state while the
            // finished one waited. Stopped, it keeps every crate it finished.
            let state = read_state(location);
            if let (Some(pid), Some(building)) = (state.pid, state.building)
                && building != Snapshot::take(location, WARM)?.fingerprint
            {
                stop(pid, &running)?;
            }
            running.lock().context("waiting for the running warm")?;
        }
        Err(TryLockError::Error(err)) => return Err(err).context("taking the warm lock"),
    }
    drop(next);
    let snapshot = Snapshot::take(location, WARM)?;
    let mut state = read_state(location);
    if state.warmed.as_deref() == Some(&snapshot.fingerprint) {
        return Ok(());
    }
    state.pid = Some(std::process::id());
    state.building = Some(snapshot.fingerprint.clone());
    write_state(location, &state)?;
    for args in commands(location) {
        if file(location, "yield").exists() {
            state.pid = None;
            state.building = None;
            state.command = None;
            return write_state(location, &state);
        }
        state.command = Some(args.clone());
        write_state(location, &state)?;
        // Failures are expected mid-edit; with --keep-going all that compiles is built anyway.
        let code = cargo::run_to(
            location,
            target,
            &location.worktree,
            &args,
            &[],
            io::stdout(),
            io::stderr(),
        )?;
        println!(
            "agent-hearth warm: cargo {} exited with {code}",
            args.join(" ")
        );
    }
    state.pid = None;
    state.building = None;
    state.command = None;
    state.warmed = Some(snapshot.fingerprint);
    write_state(location, &state)
}

/// Held by an agent's own cargo run, so no warm runs alongside it.
pub struct Hold {
    _lock: Option<File>,
}

/// Lets an agent's cargo run of `wants` go first. A running warm building the same on the same
/// code is waited for, to finish that command; any other is stopped at once, keeping the crates it
/// finished: one of code that has changed since, whose builds would be stale; one of another
/// command, which the agent does not want now; and any on a busy machine, where the warm's low
/// priority would make the agent wait longer than building itself (at a load of 50-65 on 32
/// cores, `bench/warm-atuin-linux2.txt`). It keeps looking while it waits: a warm that has just
/// taken the lock has not said yet what it builds, and real agents waited for whole warms that
/// way (22% of their cargo time, `bench/agents-atuin-codex-warm-on-1.txt`).
pub fn hold(location: &Location, wants: &[String]) -> Result<Hold> {
    let Ok(lock) = open(location, "lock") else {
        // A state directory this process cannot write (a sandbox): no warm runs here either.
        return Ok(Hold { _lock: None });
    };
    match lock.try_lock_shared() {
        Ok(()) => {
            return Ok(Hold { _lock: Some(lock) });
        }
        Err(TryLockError::WouldBlock) => {}
        Err(TryLockError::Error(err)) => return Err(err).context("checking for a warm"),
    }
    let yield_path = file(location, "yield");
    fs::write(&yield_path, "").with_context(|| format!("writing {}", yield_path.display()))?;
    let code = Snapshot::take(location, WARM)?.fingerprint;
    let wanted = warm_command(wants);
    let mut stopped = None;
    loop {
        match lock.try_lock_shared() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(err)) => return Err(err).context("waiting for a warm"),
        }
        let state = read_state(location);
        if let (Some(pid), Some(building)) = (state.pid, state.building.as_deref())
            && stopped != Some(pid)
            && (busy() || state.command != wanted || building != code)
        {
            stop(pid, &lock)?;
            stopped = Some(pid);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = fs::remove_file(&yield_path);
    Ok(Hold { _lock: Some(lock) })
}

/// Remembers a build command the agent ran, for warms to repeat.
pub fn record(location: &Location, args: &[String]) -> Result<()> {
    let Some(command) = warm_command(args) else {
        return Ok(());
    };
    let mut commands = recorded(location);
    commands.retain(|known| *known != command);
    commands.insert(0, command);
    commands.truncate(COMMANDS);
    let path = file(location, "commands");
    fs::create_dir_all(path.parent().context("warm commands have no parent")?)?;
    fs::write(&path, serde_json::to_vec(&commands)?)
        .with_context(|| format!("writing {}", path.display()))
}

/// What a warm runs: the agent's recent build commands, then the Stop hook's clippy.
fn commands(location: &Location) -> Vec<Vec<String>> {
    let mut commands = recorded(location);
    let check = warm_command(&CHECK.map(String::from)).expect("clippy is a build command");
    if !commands.contains(&check) {
        commands.push(check);
    }
    commands
}

fn recorded(location: &Location) -> Vec<Vec<String>> {
    fs::read(file(location, "commands"))
        .ok()
        .and_then(|json| serde_json::from_slice(&json).ok())
        .unwrap_or_default()
}

/// Cargo options that take a value as the next argument, which is not a test name filter.
const VALUED: [&str; 22] = [
    "-p",
    "--package",
    "--exclude",
    "-F",
    "--features",
    "--bin",
    "--test",
    "--example",
    "--bench",
    "--target",
    "--profile",
    "-j",
    "--jobs",
    "--manifest-path",
    "--target-dir",
    "--color",
    "--message-format",
    "--config",
    "-Z",
    "--lockfile-path",
    "--timings",
    "--artifact-dir",
];

/// The build an agent's cargo command does, without running anything: `test` compiles its tests
/// with `--no-run` and without a test name filter, which builds nothing different; `run` becomes
/// `build`. Every one keeps going past a crate that fails, so all that compiles is built. `None`
/// for commands that build nothing a turn needs.
pub fn warm_command(args: &[String]) -> Option<Vec<String>> {
    let (subcommand, rest) = args.split_first()?;
    // Arguments after `--` go to the test harness or the program; clippy's are lint flags,
    // which are part of what it builds.
    let before_dashes = || rest.iter().take_while(|arg| *arg != "--").cloned();
    let without_filters = || {
        let mut kept = Vec::new();
        let mut value_next = false;
        for arg in before_dashes() {
            let value = std::mem::take(&mut value_next);
            if arg.starts_with('-') {
                value_next = VALUED.contains(&arg.as_str()) && !arg.contains('=');
            } else if !value {
                continue;
            }
            kept.push(arg);
        }
        kept
    };
    let mut command = vec![];
    match subcommand.as_str() {
        "build" | "b" | "check" | "c" => {
            command.push(subcommand.clone());
            command.extend(before_dashes());
            command.push("--keep-going".to_owned());
        }
        "run" | "r" => {
            command.push("build".to_owned());
            command.extend(before_dashes());
            command.push("--keep-going".to_owned());
        }
        "test" | "t" => {
            command.push("test".to_owned());
            command.extend(
                without_filters()
                    .into_iter()
                    .filter(|arg| arg != "--no-run"),
            );
            command.extend(["--no-run".to_owned(), "--no-fail-fast".to_owned()]);
        }
        "clippy" => {
            // `agent-hearth cargo` runs `clippy … -- -D warnings` without the clippy arguments.
            let args = cargo::deny_warnings_args(args).unwrap_or(args);
            let split = args
                .iter()
                .position(|arg| arg == "--")
                .unwrap_or(args.len());
            command.extend(args[..split].iter().cloned());
            command.push("--keep-going".to_owned());
            command.extend(args[split..].iter().cloned());
        }
        _ => return None,
    }
    Some(command)
}

/// Whether the machine's load over the last minute is at least half its cores: a niced warm would
/// get too little of it to be worth waiting for, where settles, at the same low priority, and
/// other agents' builds share it, and timing tests fail. Unknown (Windows) counts as not busy.
fn busy() -> bool {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    load().is_some_and(|load| load >= cores as f64 / 2.0)
}

#[cfg(unix)]
fn load() -> Option<f64> {
    let mut load = [0.0];
    // SAFETY: getloadavg writes at most one sample into the one-element buffer.
    (unsafe { libc::getloadavg(load.as_mut_ptr(), 1) } == 1).then_some(load[0])
}

#[cfg(not(unix))]
fn load() -> Option<f64> {
    None
}

/// Stops the warm whose process group is `pid` and waits until it has let go of `lock`: as
/// Ctrl-C would, then for good after five seconds.
#[cfg(unix)]
fn stop(pid: u32, lock: &File) -> Result<()> {
    use std::time::{Duration, Instant};
    let signal = |name: &str| {
        Command::new("kill")
            .args([name, "--", &format!("-{pid}")])
            .stderr(Stdio::null())
            .status()
    };
    signal("-INT").context("stopping a warm")?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        match lock.try_lock_shared() {
            Ok(()) => return lock.unlock().context("checking for a warm"),
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(50)),
            Err(TryLockError::Error(err)) => return Err(err).context("checking for a warm"),
        }
    }
    signal("-KILL").context("stopping a warm")?;
    Ok(())
}

#[cfg(not(unix))]
fn stop(_: u32, _: &File) -> Result<()> {
    Ok(())
}

fn read_state(location: &Location) -> State {
    fs::read(file(location, "json"))
        .ok()
        .and_then(|json| serde_json::from_slice(&json).ok())
        .unwrap_or_default()
}

fn write_state(location: &Location, state: &State) -> Result<()> {
    let path = file(location, "json");
    fs::write(&path, serde_json::to_vec(state)?)
        .with_context(|| format!("writing {}", path.display()))
}

fn open(location: &Location, extension: &str) -> Result<File> {
    let path = file(location, extension);
    fs::create_dir_all(path.parent().context("warm lock has no parent")?)?;
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))
}

fn file(location: &Location, extension: &str) -> PathBuf {
    let target = adopt::target_dir(location);
    let id = target.file_name().unwrap_or_default();
    location
        .state_dir
        .join("warms")
        .join(Path::new(id).with_extension(extension))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden;
    use crate::testing::Sandbox;

    fn words(command: &str) -> Vec<String> {
        command.split(' ').map(String::from).collect()
    }

    #[test]
    fn warm_commands_build_what_the_agent_s_command_builds_and_run_nothing() {
        let cases = [
            (
                "test -p a one_is -- --nocapture",
                Some("test -p a --no-run --no-fail-fast"),
            ),
            (
                "test one --features x two",
                Some("test --features x --no-run --no-fail-fast"),
            ),
            ("test --no-run", Some("test --no-run --no-fail-fast")),
            (
                "run --bin app -- --port 1",
                Some("build --bin app --keep-going"),
            ),
            (
                "check --all-targets",
                Some("check --all-targets --keep-going"),
            ),
            (
                "clippy -p a -- -D warnings",
                Some("clippy -p a --keep-going"),
            ),
            (
                "clippy -- -W clippy::pedantic",
                Some("clippy --keep-going -- -W clippy::pedantic"),
            ),
            ("fmt", None),
            ("nextest run", None),
        ];
        for (command, expected) in cases {
            let warm = warm_command(&words(command)).map(|args| args.join(" "));
            assert_eq!(warm.as_deref(), expected, "{command}");
        }
    }

    fn adopted(s: &Sandbox) -> (Location, PathBuf) {
        s.crate_repo("repo");
        let repo = s.location("repo");
        golden::refresh(&repo, "main").unwrap();
        adopt::adopt(&repo).unwrap();
        let target = adopt::target_dir(&repo);
        (repo, target)
    }

    #[test]
    fn keeps_the_agent_s_two_latest_distinct_commands_then_the_stop_hook_s_clippy() {
        let s = Sandbox::new();
        let (repo, _) = adopted(&s);
        for command in ["test", "build", "test", "fmt"] {
            record(&repo, &words(command)).unwrap();
        }
        let expected = [
            "test --no-run --no-fail-fast",
            "build --keep-going",
            "clippy --workspace --all-targets --keep-going",
        ];
        let commands: Vec<String> = commands(&repo).iter().map(|args| args.join(" ")).collect();
        assert_eq!(commands, expected);
    }

    #[test]
    fn after_a_warm_the_agent_s_next_build_compiles_nothing() {
        let s = Sandbox::new();
        let (repo, target) = adopted(&s);
        record(&repo, &words("test")).unwrap();
        s.write("repo/src/lib.rs", "pub fn one() -> u32 {\n    2 - 1\n}\n");
        warm(&repo, &target).unwrap();
        assert_eq!(
            read_state(&repo).warmed,
            Some(Snapshot::take(&repo, WARM).unwrap().fingerprint)
        );
        for command in ["test --no-run", "clippy --workspace --all-targets"] {
            let mut args = words(command);
            args.insert(1, "--message-format=json".to_owned());
            let mut out = Vec::new();
            let code = cargo::run_to(
                &repo,
                &target,
                &repo.worktree,
                &args,
                &[],
                &mut out,
                io::sink(),
            );
            assert_eq!(code.unwrap(), 0, "{command}");
            let out = String::from_utf8(out).unwrap();
            assert!(
                !out.contains("\"fresh\":false"),
                "{command} compiled:\n{out}"
            );
        }
    }

    #[test]
    fn a_warm_gives_way_to_a_waiting_agent_and_to_a_queued_warm() {
        let s = Sandbox::new();
        let (repo, target) = adopted(&s);
        open(&repo, "yield").unwrap();
        warm(&repo, &target).unwrap();
        assert_eq!(
            read_state(&repo).warmed,
            None,
            "it built while an agent waited"
        );
        fs::remove_file(file(&repo, "yield")).unwrap();

        // One waits already: this one returns at once rather than wait behind it.
        let next = open(&repo, "next").unwrap();
        next.lock().unwrap();
        warm(&repo, &target).unwrap();
        assert_eq!(read_state(&repo).warmed, None);
    }

    /// A stand-in warm of older code in its own process group, holding the lock like a real one.
    #[cfg(target_os = "linux")]
    fn running_warm_of_older_code(repo: &Location) -> std::process::Child {
        running_warm(repo, "older code".to_owned(), None)
    }

    /// A stand-in warm of `building` running `command`, in its own process group, holding the lock
    /// like a real one.
    #[cfg(target_os = "linux")]
    fn running_warm(
        repo: &Location,
        building: String,
        running: Option<Vec<String>>,
    ) -> std::process::Child {
        let lock = file(repo, "lock");
        open(repo, "lock").unwrap();
        let mut command = Command::new("flock");
        command.arg(&lock).args(["sleep", "60"]);
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let warm = command.spawn().unwrap();
        let start = std::time::Instant::now();
        while open(repo, "lock").unwrap().try_lock_shared().is_ok() {
            assert!(
                start.elapsed().as_secs() < 5,
                "the stand-in never took the lock"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let state = State {
            pid: Some(warm.id()),
            building: Some(building),
            command: running,
            warmed: None,
        };
        write_state(repo, &state).unwrap();
        warm
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_agent_s_build_stops_a_warm_of_code_that_has_changed_since() {
        let s = Sandbox::new();
        let (repo, _) = adopted(&s);
        let mut warm = running_warm_of_older_code(&repo);
        let start = std::time::Instant::now();
        let held = hold(&repo, &words("check")).unwrap();
        assert!(start.elapsed().as_secs() < 5, "{:?}", start.elapsed());
        assert!(warm.try_wait().unwrap().is_some(), "the warm still runs");
        assert!(!file(&repo, "yield").exists());
        drop(held);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_new_edit_s_warm_stops_one_still_building_older_code() {
        let s = Sandbox::new();
        let (repo, target) = adopted(&s);
        let mut older = running_warm_of_older_code(&repo);
        // So that this warm returns as soon as it holds the lock, without building.
        open(&repo, "yield").unwrap();
        let start = std::time::Instant::now();
        warm(&repo, &target).unwrap();
        assert!(start.elapsed().as_secs() < 5, "{:?}", start.elapsed());
        assert!(
            older.try_wait().unwrap().is_some(),
            "the older warm still runs"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_agent_s_build_stops_a_warm_of_another_command_on_the_same_code() {
        let s = Sandbox::new();
        let (repo, _) = adopted(&s);
        let code = Snapshot::take(&repo, WARM).unwrap().fingerprint;
        let mut warm = running_warm(&repo, code, warm_command(&words("test")));
        let start = std::time::Instant::now();
        let held = hold(&repo, &words("check")).unwrap();
        assert!(start.elapsed().as_secs() < 5, "{:?}", start.elapsed());
        assert!(warm.try_wait().unwrap().is_some(), "the warm still runs");
        drop(held);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_agent_s_build_stops_a_warm_that_says_what_it_builds_only_later() {
        let s = Sandbox::new();
        let (repo, _) = adopted(&s);
        let mut warm = running_warm(&repo, "older code".to_owned(), None);
        // As a warm that has just taken the lock: it has not written its state yet.
        let state = read_state(&repo);
        write_state(&repo, &State::default()).unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(300));
                write_state(&repo, &state).unwrap();
            });
            let start = std::time::Instant::now();
            let held = hold(&repo, &words("check")).unwrap();
            assert!(start.elapsed().as_secs() < 5, "{:?}", start.elapsed());
            drop(held);
        });
        assert!(warm.try_wait().unwrap().is_some(), "the warm still runs");
    }
}
