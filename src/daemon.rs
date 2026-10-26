//! The settle daemon (`agent-hearth daemon`): admits settles by free memory, so several run at
//! once when memory allows and one at a time when it does not. Settles queued one at a time
//! reached the last of ten agents many minutes after it stopped; several at once finish sooner
//! alone but filled memory next to agents' builds.
//!
//! A settle connects to `<state>/daemon.sock`, sends one JSON line (its pid, repository and
//! worktree) and waits for `go`, after `wait` when it has to. It holds the connection while it
//! runs; closing it, or exiting, frees its place. The first settle that finds no daemon starts
//! one, which exits after ten idle minutes. Without a daemon, settles queue one at a time.
//!
//! The daemon admits the oldest waiting settle when nothing else runs, or when available memory,
//! less what running settles may still grow into, covers the largest of its repository's last
//! five measured peaks
//! plus a quarter of the machine's memory as headroom for agents' builds. A settle's memory is
//! that of its process tree, sampled twice a second: anonymous memory on Linux, the physical
//! footprint on macOS.
//!
//! ```text
//! <state>/daemon.sock        the socket
//! <state>/daemon.lock        held by the running daemon
//! <state>/daemon.log         its log: every admission and finished settle
//! <state>/settle-memory.json each repository's last five settle peaks, in bytes
//! ```

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::golden;
use crate::locate::{BaseDirs, Location};

const GIB: u64 = 1 << 30;
/// What a repository's settle is assumed to need until one has been measured.
const DEFAULT_ESTIMATE: u64 = 4 * GIB;
/// How many of a repository's last settle peaks its estimate covers.
const PEAKS: usize = 5;
const TICK: Duration = Duration::from_millis(500);
const IDLE: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy)]
pub struct Memory {
    pub total: u64,
    pub available: u64,
}

pub struct Config {
    /// How long the daemon stays without settles before it exits.
    pub idle: Duration,
    pub memory: fn() -> Result<Memory>,
}

impl Default for Config {
    fn default() -> Self {
        Self { idle: IDLE, memory }
    }
}

#[derive(Serialize, Deserialize)]
struct Request {
    pid: u32,
    repo: String,
    worktree: PathBuf,
}

struct Settle {
    id: u64,
    request: Request,
    /// Dropped to turn a waiting settle away; sent on to admit it.
    grant: Option<Sender<()>>,
    rss: u64,
    peak: u64,
}

#[derive(Default)]
struct State {
    next: u64,
    waiting: VecDeque<Settle>,
    running: Vec<Settle>,
    /// Each repository's last settle peaks, newest last.
    estimates: BTreeMap<String, Vec<u64>>,
}

impl State {
    /// The largest of the repository's last peaks: one settle's peak depends on what changed
    /// (several times larger after an edit to the crate everything depends on than to a leaf),
    /// and a small last peak let three large settles start at once.
    fn estimate(&self, repo: &str) -> u64 {
        self.estimates
            .get(repo)
            .and_then(|peaks| peaks.iter().max().copied())
            .unwrap_or(DEFAULT_ESTIMATE)
    }
}

/// How many settles from the front of `waiting` (their estimates) to start now, beside `running`
/// (estimate and current memory of each).
fn admissible(waiting: &[u64], running: &[(u64, u64)], memory: Memory) -> usize {
    let headroom = memory.total / 4;
    let mut reserved: u64 = running
        .iter()
        .map(|(estimate, rss)| estimate.saturating_sub(*rss))
        .sum();
    let mut busy = !running.is_empty();
    let mut admitted = 0;
    for estimate in waiting {
        if busy && memory.available.saturating_sub(reserved) < estimate + headroom {
            break;
        }
        reserved += estimate;
        busy = true;
        admitted += 1;
    }
    admitted
}

/// Runs the daemon until it has been idle for `config.idle`, or its socket is removed. Returns
/// at once when another daemon holds the lock.
pub fn serve(base: &BaseDirs, config: Config) -> Result<()> {
    let dir = &base.state;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("daemon.lock"))
        .context("opening the daemon lock")?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(()),
        Err(TryLockError::Error(err)) => return Err(err).context("taking the daemon lock"),
    }
    let socket = dir.join("daemon.sock");
    let _ = fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    let memory = (config.memory)()?;
    log(&format!(
        "started: {} total, {} available",
        gb(memory.total),
        gb(memory.available)
    ));
    let state = Arc::new(Mutex::new(State {
        estimates: fs::read(dir.join("settle-memory.json"))
            .ok()
            .and_then(|json| serde_json::from_slice(&json).ok())
            .unwrap_or_default(),
        ..State::default()
    }));
    {
        let (state, dir) = (Arc::clone(&state), dir.clone());
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (state, dir) = (Arc::clone(&state), dir.clone());
                thread::spawn(move || {
                    if let Err(err) = connection(stream, &state, &dir) {
                        log(&format!("connection: {err:#}"));
                    }
                });
            }
        });
    }

    let mut idle_since = Instant::now();
    loop {
        thread::sleep(TICK);
        if !socket.exists() {
            log("socket removed: exiting");
            return Ok(());
        }
        let mut state = state.lock().expect("daemon state");
        if state.waiting.is_empty() && state.running.is_empty() {
            if idle_since.elapsed() >= config.idle {
                log("idle: exiting");
                let _ = fs::remove_file(&socket);
                return Ok(());
            }
            continue;
        }
        idle_since = Instant::now();
        tick(&mut state, (config.memory)()?);
    }
}

/// Updates the running settles' memory, drops waiting settles whose process is gone, and admits
/// what memory allows.
fn tick(state: &mut State, memory: Memory) {
    if let Some(processes) = processes() {
        for settle in &mut state.running {
            settle.rss = tree_rss(&processes, settle.request.pid);
            settle.peak = settle.peak.max(settle.rss);
        }
        state
            .waiting
            .retain(|settle| processes.iter().any(|p| p.0 == settle.request.pid));
    }
    let waiting: Vec<u64> = state
        .waiting
        .iter()
        .map(|settle| state.estimate(&settle.request.repo))
        .collect();
    let running: Vec<(u64, u64)> = state
        .running
        .iter()
        .map(|settle| (state.estimate(&settle.request.repo), settle.rss))
        .collect();
    for _ in 0..admissible(&waiting, &running, memory) {
        let mut settle = state.waiting.pop_front().expect("an admissible settle");
        log(&format!(
            "admit {} (estimate {}, {} available, {} running)",
            settle.request.worktree.display(),
            gb(state.estimate(&settle.request.repo)),
            gb(memory.available),
            state.running.len(),
        ));
        if let Some(grant) = settle.grant.take() {
            let _ = grant.send(());
        }
        state.running.push(settle);
    }
}

/// One settle's connection: queue it, tell it when it may start, and record its peak when it
/// closes.
fn connection(stream: UnixStream, state: &Mutex<State>, dir: &Path) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let request: Request = serde_json::from_str(&line).context("reading the request")?;
    let (grant, granted) = mpsc::channel();
    let id = {
        let mut state = state.lock().expect("daemon state");
        state.next += 1;
        let id = state.next;
        state.waiting.push_back(Settle {
            id,
            request,
            grant: Some(grant),
            rss: 0,
            peak: 0,
        });
        id
    };
    let mut writer = &stream;
    let admitted = match granted.recv_timeout(2 * TICK) {
        Ok(()) => true,
        Err(RecvTimeoutError::Timeout) => {
            writeln!(writer, "wait")?;
            granted.recv().is_ok()
        }
        Err(RecvTimeoutError::Disconnected) => false,
    };
    if admitted && writeln!(writer, "go").is_ok() {
        // The settle sends nothing more: this returns when it closes the connection or exits.
        let _ = reader.read(&mut [0; 1]);
    }
    finish(state, id, dir)
}

fn finish(state: &Mutex<State>, id: u64, dir: &Path) -> Result<()> {
    let mut state = state.lock().expect("daemon state");
    state.waiting.retain(|settle| settle.id != id);
    let Some(index) = state.running.iter().position(|settle| settle.id == id) else {
        return Ok(());
    };
    let settle = state.running.remove(index);
    log(&format!(
        "done {} (peak {})",
        settle.request.worktree.display(),
        gb(settle.peak)
    ));
    if settle.peak > 0 {
        let peaks = state.estimates.entry(settle.request.repo).or_default();
        peaks.push(settle.peak);
        if peaks.len() > PEAKS {
            peaks.remove(0);
        }
        golden::write_atomically(
            &dir.join("settle-memory.json"),
            &serde_json::to_vec_pretty(&state.estimates)?,
        )?;
    }
    Ok(())
}

/// Waits for the daemon to admit this settle, starting one if none runs. The returned connection
/// is held for the whole settle. `None` when no daemon can be reached: the caller queues instead.
pub fn admit(location: &Location) -> Option<UnixStream> {
    let base = location.base();
    let socket = base.state.join("daemon.sock");
    let mut stream = UnixStream::connect(&socket).ok();
    if stream.is_none() && !cfg!(test) && start(base).is_ok() {
        let deadline = Instant::now() + Duration::from_secs(3);
        while stream.is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
            stream = UnixStream::connect(&socket).ok();
        }
    }
    let mut stream = stream?;
    let request = Request {
        pid: std::process::id(),
        repo: location.repo_id.clone(),
        worktree: location.worktree.clone(),
    };
    writeln!(stream, "{}", serde_json::to_string(&request).ok()?).ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return None,
            Ok(_) if line.trim() == "go" => return Some(stream),
            Ok(_) => eprintln!("agent-hearth: waiting for memory: other settles are running"),
        }
    }
}

/// Starts a daemon in the background, logging to `<state>/daemon.log`.
fn start(base: &BaseDirs) -> Result<()> {
    fs::create_dir_all(&base.state)?;
    let log = File::create(base.state.join("daemon.log"))?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("daemon")
        .env("AGENT_HEARTH_STATE_DIR", &base.state)
        .env("AGENT_HEARTH_CACHE_DIR", &base.cache)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command.spawn().context("starting the daemon")?;
    Ok(())
}

/// Every process's pid, parent pid and memory in bytes, or `None` when they cannot be read. On
/// Linux the memory is anonymous memory (`RssAnon`): resident memory also counts shared file
/// pages once per process, which overstated one settle several times over. On macOS it is the
/// physical footprint, what Activity Monitor shows; elsewhere `ps`'s resident memory.
fn processes() -> Option<Vec<(u32, u32, u64)>> {
    if cfg!(target_os = "linux") {
        let mut processes = Vec::new();
        for entry in fs::read_dir("/proc").ok()?.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            // Gone since the listing, or a kernel thread without the fields: skipped.
            let Ok(status) = fs::read_to_string(entry.path().join("status")) else {
                continue;
            };
            let field = |name: &str| {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix(name))
                    .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
            };
            if let Some(ppid) = field("PPid:") {
                processes.push((pid, ppid as u32, field("RssAnon:").unwrap_or(0) * 1024));
            }
        }
        return Some(processes);
    }
    let output = Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "ppid=", "-o", "rss="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(
        text.lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace().map(str::parse::<u64>);
                let (pid, ppid, rss) = (fields.next()?, fields.next()?, fields.next()?);
                let (pid, rss) = (pid.ok()? as u32, rss.ok()? * 1024);
                Some((pid, ppid.ok()? as u32, footprint(pid).unwrap_or(rss)))
            })
            .collect(),
    )
}

/// A process's physical footprint: its private memory, compressed pages included.
#[cfg(target_os = "macos")]
fn footprint(pid: u32) -> Option<u64> {
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    // SAFETY: proc_pid_rusage writes one rusage_info_v2 into `info`, which outlives the call.
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid as libc::c_int,
            libc::RUSAGE_INFO_V2,
            (&mut info as *mut libc::rusage_info_v2).cast(),
        )
    };
    (rc == 0).then_some(info.ri_phys_footprint)
}

#[cfg(not(target_os = "macos"))]
fn footprint(_: u32) -> Option<u64> {
    None
}

/// The memory of `root` and all its descendants.
fn tree_rss(processes: &[(u32, u32, u64)], root: u32) -> u64 {
    let mut total = 0;
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        for &(child, parent, rss) in processes {
            if child == pid {
                total += rss;
            }
            if parent == pid && child != pid {
                stack.push(child);
            }
        }
    }
    total
}

/// This machine's total and available memory.
/// `machine` narrowed by the memory limits of the cgroup at `group` and its ancestors up to
/// `root`: a container's or systemd scope's `memory.max`, less what the group uses. Its file
/// cache counts as free, as in `MemAvailable`, since the kernel reclaims it before it kills.
fn within_cgroups(mut machine: Memory, group: &Path, root: &Path) -> Memory {
    let read = |dir: &Path, file: &str| fs::read_to_string(dir.join(file)).unwrap_or_default();
    for dir in group.ancestors().take_while(|dir| dir.starts_with(root)) {
        let Ok(limit) = read(dir, "memory.max").trim().parse::<u64>() else {
            continue;
        };
        let current = read(dir, "memory.current")
            .trim()
            .parse::<u64>()
            .unwrap_or(0);
        let file: u64 = read(dir, "memory.stat")
            .lines()
            .filter_map(|line| line.split_once(' '))
            .filter(|(name, _)| matches!(*name, "active_file" | "inactive_file"))
            .filter_map(|(_, bytes)| bytes.parse::<u64>().ok())
            .sum();
        machine.total = machine.total.min(limit);
        machine.available = machine
            .available
            .min(limit.saturating_sub(current.saturating_sub(file)));
    }
    machine
}

fn memory() -> Result<Memory> {
    if cfg!(target_os = "linux") {
        let meminfo = fs::read_to_string("/proc/meminfo").context("reading /proc/meminfo")?;
        let field = |name: &str| {
            meminfo
                .lines()
                .find_map(|line| line.strip_prefix(name)?.trim().strip_suffix("kB"))
                .and_then(|kb| kb.trim().parse::<u64>().ok())
                .map(|kb| kb * 1024)
                .with_context(|| format!("no {name} in /proc/meminfo"))
        };
        let machine = Memory {
            total: field("MemTotal:")?,
            available: field("MemAvailable:")?,
        };
        // cgroup v2 names this process's group on a "0::" line.
        let group = fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        return Ok(
            match group.lines().find_map(|line| line.strip_prefix("0::")) {
                Some(group) => {
                    let root = Path::new("/sys/fs/cgroup");
                    within_cgroups(machine, &root.join(group.trim_start_matches('/')), root)
                }
                None => machine,
            },
        );
    }
    if cfg!(target_os = "macos") {
        let output = |program: &str, args: &[&str]| -> Result<String> {
            let output = Command::new(program).args(args).output()?;
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        };
        let total = output("sysctl", &["-n", "hw.memsize"])?
            .trim()
            .parse::<u64>()?;
        let vm_stat = output("vm_stat", &[])?;
        let page = vm_stat
            .split("page size of ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
            .context("no page size in vm_stat")?;
        let pages = |name: &str| {
            vm_stat
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .and_then(|rest| rest.trim().trim_end_matches('.').parse::<u64>().ok())
                .unwrap_or(0)
        };
        let free = pages("Pages free:") + pages("Pages inactive:") + pages("Pages speculative:");
        return Ok(Memory {
            total,
            available: free * page,
        });
    }
    bail!("reading free memory is only implemented on Linux and macOS")
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / GIB as f64)
}

fn log(message: &str) {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    eprintln!("{secs} {message}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Sandbox;

    #[test]
    fn admits_while_memory_covers_the_estimate_and_headroom() {
        let memory = |available| Memory {
            total: 64 * GIB,
            available: available * GIB,
        };
        // Nothing running: the first settle always starts, however short memory is.
        assert_eq!(admissible(&[4 * GIB], &[], memory(1)), 1);
        // 16 GB headroom: with 30 GB available, the third 4 GB settle still fits (30 - 8 >= 4 +
        // 16), the fourth does not (30 - 12 < 20).
        assert_eq!(admissible(&[4 * GIB; 6], &[], memory(30)), 3);
        // A running settle at 1 GB of its 4 GB still reserves 3 GB.
        assert_eq!(admissible(&[4 * GIB], &[(4 * GIB, GIB)], memory(22)), 0);
        assert_eq!(admissible(&[4 * GIB], &[(4 * GIB, GIB)], memory(23)), 1);
    }

    #[test]
    fn tree_rss_sums_a_process_and_its_descendants() {
        let processes = [(1, 0, 10), (2, 1, 20), (3, 2, 30), (4, 1, 40), (5, 0, 50)];
        assert_eq!(tree_rss(&processes, 2), 50);
        assert_eq!(tree_rss(&processes, 1), 100);
    }

    #[test]
    fn processes_include_this_one_with_its_parent_and_memory() {
        let processes = processes().unwrap();
        let me = std::process::id();
        let (_, parent, memory) = processes.iter().find(|p| p.0 == me).unwrap();
        assert_eq!(*parent, std::os::unix::process::parent_id());
        assert!(*memory > 0);
    }

    #[test]
    fn memory_reads_this_machine() {
        let memory = memory().unwrap();
        assert!(memory.available > 0 && memory.available <= memory.total);
    }

    #[test]
    fn cgroup_limits_narrow_the_machine_memory() {
        let s = Sandbox::new();
        let root = s.path("cgroup");
        let scope = root.join("user.slice/bench.scope");
        fs::create_dir_all(&scope).unwrap();
        let write = |dir: &Path, file: &str, text: &str| fs::write(dir.join(file), text).unwrap();
        // The slice has no limit; the scope allows 10 GiB and uses 7, 3 of them file cache.
        write(&root.join("user.slice"), "memory.max", "max\n");
        write(&scope, "memory.max", &format!("{}\n", 10 * GIB));
        write(&scope, "memory.current", &format!("{}\n", 7 * GIB));
        let stat = format!(
            "anon {}\nactive_file {}\ninactive_file {}\n",
            4 * GIB,
            GIB,
            2 * GIB
        );
        write(&scope, "memory.stat", &stat);
        let machine = Memory {
            total: 64 * GIB,
            available: 40 * GIB,
        };
        let memory = within_cgroups(machine, &scope, &root);
        assert_eq!((memory.total, memory.available), (10 * GIB, 6 * GIB));
        // Without limits the machine's memory stands.
        let memory = within_cgroups(machine, &root.join("user.slice"), &root);
        assert_eq!((memory.total, memory.available), (64 * GIB, 40 * GIB));
    }

    #[test]
    fn a_second_settle_waits_for_the_first_when_memory_is_short() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let (one, two) = (s.location("repo"), s.location("repo"));
        let base = one.base().clone();
        thread::spawn(move || {
            let short = || {
                Ok(Memory {
                    total: 8 * GIB,
                    available: GIB,
                })
            };
            serve(
                &base,
                Config {
                    idle: Duration::from_secs(60),
                    memory: short,
                },
            )
        });
        let socket = one.base().state.join("daemon.sock");
        let deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&socket).is_err() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }

        let first = admit(&one).expect("the first settle starts at once");
        let (started, second) = mpsc::channel();
        thread::spawn(move || started.send(admit(&two).is_some()).unwrap());
        assert!(second.recv_timeout(Duration::from_secs(2)).is_err());
        drop(first);
        assert!(second.recv_timeout(Duration::from_secs(5)).unwrap());
    }
}
