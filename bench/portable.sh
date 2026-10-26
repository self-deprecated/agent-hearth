# Helpers the swarm and slot scripts share, so they run on Linux and macOS alike. Sourced.

# Decimal points, whatever the user's locale: printf reads and writes them.
export LC_ALL=C

# Seconds since the epoch, with microseconds (bash 5, else perl, which macOS ships).
now() { if [ -n "${EPOCHREALTIME:-}" ]; then echo "${EPOCHREALTIME/,/.}"; else perl -MTime::HiRes=time -e 'printf "%.6f\n", time'; fi; }
dt()  { awk -v a="$(now)" -v b="$1" 'BEGIN { printf "%.2f\n", a - b }'; }
load() { if [ -r /proc/loadavg ]; then cut -d" " -f1-3 /proc/loadavg; else sysctl -n vm.loadavg | tr -d '{}' | xargs; fi; }
host() { echo "$(uname -sm), $(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu) cores"; }
# Copy directory $1 to $2 as reflinks, keeping mtimes: FICLONE on Linux, clonefile on macOS.
# GNU cp (Linux, or a Nix shell on macOS) clones with --reflink; macOS's own cp with -c.
clone_dir() { if cp --version >/dev/null 2>&1; then cp -a --reflink=always "$1" "$2"; else cp -cpR "$1" "$2"; fi; }

# The files the agents edit, one per agent, round robin.
# shellcheck disable=SC2206 # paths without spaces, split on purpose
FILES=(${FILES:-})
# edit <worktree> <agent> <n>: agent's nth edit, a new function in its file.
edit() { printf '\n#[allow(dead_code)]\nfn swarm_edit_%s_%s() -> u32 { %s }\n' "$2" "$3" "$3" >> "$1/${FILES[$(( ($2 - 1) % ${#FILES[@]} ))]}"; }

# A golden refresh starts the golden's test suite in the background (`golden settle`), to learn
# what fails on main. Timings wait for it, as agents would usually start well after.
wait_golden_settle() { while pgrep -f 'agent-hearth golden settl[e]' >/dev/null; do sleep 2; done; }
