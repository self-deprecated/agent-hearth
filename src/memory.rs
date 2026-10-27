//! Total and available memory, for admitting settles (see `settle`).

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy)]
pub struct Memory {
    pub total: u64,
    pub available: u64,
}

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

/// This machine's total and available memory, within its cgroup's limits on Linux.
pub fn memory() -> Result<Memory> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Sandbox;

    const GIB: u64 = 1 << 30;

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
}
