#!/usr/bin/env bash
# Build places when agents outnumber cores: N agents' builds at once, with and without
# AGENT_HEARTH_BUILD_PLACES=$PLACES (at most that many `agent-hearth cargo` builds at once, the
# rest waiting their turn). On a public repository at a pinned commit: a golden, then N
# worktrees from `agent-hearth spawn`. A warm-up round gives each its first edit and turn, without
# places. Then 2*ROUNDS rounds, the variants alternating (without places first in odd rounds):
# each agent makes a new edit to its file (FILES, round robin), and all N start their turn at
# once; a turn is TURN's cargo commands (default `clippy --workspace --all-targets` then
# `test --workspace --no-run`), each through `agent-hearth cargo`. Reported per round: the
# agents' median turn time from the common start, the last finisher, failures and load; then
# per variant the median of the rounds' medians and of their last finishers.
# Everything lives under $BENCH_DIR/agent-hearth-places-bench (BENCH_DIR: ~/.cache), removed at
# the end; a round starts only with MIN_FREE (15) GB free. Linux; warming is off.
# Usage (in a shell with the repository's toolchain):
#   AH=<agent-hearth binary> REV=<commit> FILES="<files agents edit>" N=8 ROUNDS=3 PLACES=2 places.sh <git clone>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
: "${FILES[0]:?set FILES to the files the agents edit}"
SRC=$1 REV=${REV:?set REV} N=${N:-8} ROUNDS=${ROUNDS:-3} PLACES=${PLACES:-2} AH=${AH:-agent-hearth} MIN_FREE=${MIN_FREE:-15}
TURN=${TURN:-clippy --workspace --all-targets;test --workspace --no-run}
BASE=${BENCH_DIR:-$HOME/.cache}/agent-hearth-places-bench; rm -rf "$BASE"; mkdir -p "$BASE"; cd "$BASE"
trap 'cd /; rm -rf "$BASE"' EXIT
export AGENT_HEARTH_STATE_DIR=$BASE/state AGENT_HEARTH_CACHE_DIR=$BASE/cache CARGO_INCREMENTAL=1 \
  AGENT_HEARTH_WARM=off
unset CARGO_TARGET_DIR CI AGENT_HEARTH_BUILD_PLACES

guard() {
  local free; free=$(df --output=avail -BG "$BASE" | tail -1 | tr -dc 0-9)
  [ "$free" -ge "$MIN_FREE" ] || { echo "stopped: $free GB free"; exit 1; }
}
# turn <worktree> <places or empty>: one agent's turn; prints seconds and exit code.
turn() {
  local s rc=0 cmd; s=$(now)
  IFS=';' read -ra cmds <<< "$TURN"
  for cmd in "${cmds[@]}"; do
    # shellcheck disable=SC2086 # cargo arguments, split on purpose
    (cd "$1" && AGENT_HEARTH_BUILD_PLACES=$2 "$AH" cargo $cmd >/dev/null 2>&1) || { rc=$?; break; }
  done
  printf '%.2f %d\n' "$(dt "$s")" "$rc"
}
# round <label> <places or empty> <n>: every agent's nth edit, then all turns at once.
round() {
  local i s wall
  guard
  for i in $(seq 1 "$N"); do edit "${W[i]}" "$i" "$3"; done
  rm -f "$BASE"/r.*
  s=$(now); for i in $(seq 1 "$N"); do turn "${W[i]}" "$2" > "$BASE/r.$i" & done; wait
  wall=$(dt "$s")
  local median last failed
  median=$(cat "$BASE"/r.* | awk '{print $1}' | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}')
  last=$(cat "$BASE"/r.* | awk '{print $1}' | sort -n | tail -1)
  failed=$(cat "$BASE"/r.* | awk '$2 != 0' | wc -l)
  printf '%-8s round %2d: per agent median %6.1fs, last %6.1fs, wall %6.1fs, failed %d, load %s, %sG free\n' \
    "$1" "$3" "$median" "$last" "$wall" "$failed" "$(load)" "$(df --output=avail -BG "$BASE" | tail -1 | tr -dc 0-9)"
  echo "$1 $median $last" >> "$BASE/results"
}

echo "agent-hearth: $("$AH" --version), $(cargo --version), $(host), load $(load), $(date -I)"
[ -n "${NOTE:-}" ] && echo "note: $NOTE"
echo "N=$N agents, ROUNDS=$ROUNDS per variant, PLACES=$PLACES, turn: $TURN; $(findmnt -no FSTYPE -T "$BASE") at $BASE"
git -c advice.detachedHead=false clone -q "$SRC" repo && (cd repo && git checkout -q -B main "$REV" && jj git init --colocate >/dev/null 2>&1)
echo "repository: $(git -C "$SRC" remote get-url origin 2>/dev/null || echo "$SRC") at $(cd repo && git rev-parse --short=12 HEAD)"
s=$(now); (cd repo && "$AH" golden refresh >/dev/null 2>&1); rc=$?
printf 'golden refresh: %.1fs rc=%d, load %s\n' "$(dt "$s")" $rc "$(load)"
wait_golden_settle
for i in $(seq 1 "$N"); do W[i]=$(cd repo && "$AH" spawn "agent-$i" 2>/dev/null); done
round warm-up "" 1
for r in $(seq 1 "$ROUNDS"); do
  order="off on"; [ $((r % 2)) = 0 ] && order="on off"
  for v in $order; do
    n=$(( $(wc -l < "$BASE/results") + 1 ))
    if [ "$v" = on ]; then round "places=$PLACES" "$PLACES" "$n"; else round "off" "" "$n"; fi
  done
done
echo
for v in off "places=$PLACES"; do
  awk -v v="$v" '$1 == v {print $2}' "$BASE/results" | sort -n | awk -v v="$v" '{a[NR]=$1} END {printf "%-8s per-agent median, median of %d rounds: %.1fs [%.1f-%.1f]\n", v, NR, a[int((NR+1)/2)], a[1], a[NR]}'
  awk -v v="$v" '$1 == v {print $3}' "$BASE/results" | sort -n | awk -v v="$v" '{a[NR]=$1} END {printf "%-8s last finisher, median of %d rounds:     %.1fs [%.1f-%.1f]\n", v, NR, a[int((NR+1)/2)], a[1], a[NR]}'
done
