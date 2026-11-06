#!/usr/bin/env bash
# Promoting a landed worktree to the next golden: what a golden refresh costs when main
# fast-forwards to a commit an agent's worktree built, seeded from the previous golden
# (`AGENT_HEARTH_PROMOTE=off`) against seeded from that worktree's build directory. Each round:
# a worktree from `agent-hearth spawn` commits an edit to FILE (jj), builds a turn
# (`clippy --workspace --all-targets`, `test --workspace --no-run`, through `agent-hearth cargo`),
# and main moves to its commit; then both refreshes, in alternating order, each followed by a new
# worktree's first edit of FILE2 and turn, and the new golden removed before the other (the
# current golden put back). The golden's test suite, which each refresh starts in the background,
# is stopped: its run takes minutes and nothing here reads it.
# Everything lives under $BENCH_DIR/agent-hearth-promote-bench, removed at the end; a round
# starts only with 15 GB free. Linux (the pin). Warming is off.
# Usage (in a shell with the repository's toolchain):
#   AH=<agent-hearth binary> REV=<commit> FILE=<file> FILE2=<file> promote.sh <git clone>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
SRC=$1 REV=${REV:?set REV} FILE=${FILE:?set FILE} FILE2=${FILE2:?set FILE2} REPS=${REPS:-3} AH=${AH:-agent-hearth}
BASE=${BENCH_DIR:-$HOME/.cache}/agent-hearth-promote-bench; rm -rf "$BASE"; mkdir -p "$BASE"; cd "$BASE"
trap 'stop_golden_settle; cd /; rm -rf "$BASE"' EXIT
export AGENT_HEARTH_STATE_DIR=$BASE/state AGENT_HEARTH_CACHE_DIR=$BASE/cache CARGO_INCREMENTAL=1 \
  AGENT_HEARTH_WARM=off
unset CARGO_TARGET_DIR CI AGENT_HEARTH_PROMOTE

guard() {
  local free; free=$(df --output=avail -BG "$BASE" | tail -1 | tr -dc 0-9)
  [ "$free" -ge 15 ] || { echo "stopped: $free GB free"; exit 1; }
}
# The golden's test suite in the background, with its cargo and tests: its process group.
stop_golden_settle() {
  local p; for p in $(pgrep -f 'agent-hearth golden settl[e]'); do kill -- "-$(ps -o pgid= "$p" | tr -d ' ')" 2>/dev/null; done
  while pgrep -f 'agent-hearth golden settl[e]' >/dev/null; do sleep 1; done
}
# turn <worktree>: one agent turn through agent-hearth; prints seconds and exit code.
turn() {
  local s; s=$(now)
  (cd "$1" && "$AH" cargo clippy --workspace --all-targets >/dev/null 2>&1 &&
    "$AH" cargo test --workspace --no-run >/dev/null 2>&1)
  local rc=$?; printf '%.2f %d\n' "$(dt "$s")" "$rc"
}
# refresh <label> <n>: a golden refresh, then a new worktree's first edit; records both.
refresh() {
  local s rc out took W next
  s=$(now); out=$(cd repo && "$AH" golden refresh 2>/dev/null); rc=$?; took=$(dt "$s")
  stop_golden_settle
  W=$(cd repo && "$AH" spawn "next-$1-$2" 2>/dev/null)
  printf '\n#[allow(dead_code)]\nfn promote_next_%s() -> u32 { 1 }\n' "$2" >> "$W/$FILE2"
  next=$(turn "$W")
  printf '%-8s round %d: refresh %6.1fs rc=%d, then a new worktree'"'"'s first edit %ss rc=%s, load %s\n    %s\n' \
    "$1" "$2" "$took" "$rc" "${next% *}" "${next#* }" "$(load)" "$out"
  echo "$1 $took ${next% *}" >> "$BASE/results"
  (cd repo && jj workspace forget "next-$1-$2" >/dev/null 2>&1); rm -rf "$W"; drop_targets
}
# Build directories of worktrees that are gone (each holds 20 GiB of its own after a turn on
# atuin), goldens kept.
drop_targets() { (cd repo && "$AH" gc --keep 100 >/dev/null 2>&1); }

echo "agent-hearth: $("$AH" --version), $(cargo --version), $(host), load $(load), $(date -I)"
[ -n "${NOTE:-}" ] && echo "note: $NOTE"
git -c advice.detachedHead=false clone -q "$SRC" repo && (cd repo && git checkout -q -B main "$REV" && jj git init --colocate >/dev/null 2>&1)
echo "repository: $(git -C "$SRC" remote get-url origin 2>/dev/null || echo "$SRC") at $(cd repo && git rev-parse --short=12 HEAD); FILE $FILE, FILE2 $FILE2"
s=$(now); (cd repo && "$AH" golden refresh >/dev/null 2>&1); rc=$?
printf 'first golden: %.1fs rc=%d, load %s\n' "$(dt "$s")" $rc "$(load)"
stop_golden_settle
GOLDENS=$(echo "$BASE"/state/*/goldens)

for r in $(seq 1 "$REPS"); do
  guard
  # An agent's change: a commit in its worktree, built there, then landed on main.
  W=$(cd repo && "$AH" spawn "agent-$r" 2>/dev/null)
  printf '\n#[allow(dead_code)]\nfn promote_landed_%s() -> u32 { %s }\n' "$r" "$r" >> "$W/$FILE"
  (cd "$W" && jj commit --quiet -m "landed $r" >/dev/null 2>&1)
  t=$(turn "$W")
  commit=$(cd "$W" && jj log --no-graph -r @- -T commit_id 2>/dev/null)
  (cd repo && jj bookmark set main -r "$commit" >/dev/null 2>&1 && jj git export >/dev/null 2>&1)
  printf 'round %d: the agent'"'"'s turn after its commit %ss rc=%s; main at %s\n' "$r" "${t% *}" "${t#* }" "${commit:0:12}"
  before=$(cat "$GOLDENS/current")
  order="seeded promoted"; [ $((r % 2)) = 0 ] && order="promoted seeded"
  for arm in $order; do
    guard
    if [ "$arm" = seeded ]; then AGENT_HEARTH_PROMOTE=off refresh "$arm" "$r"; else refresh "$arm" "$r"; fi
    new=$(cat "$GOLDENS/current")
    if [ "$arm" != "${order##* }" ]; then
      [ "$new" != "$before" ] && rm -rf "${GOLDENS:?}/$new"
      echo "$before" > "$GOLDENS/current"
    fi
  done
  (cd repo && jj workspace forget "agent-$r" >/dev/null 2>&1); rm -rf "$W"; drop_targets
  rm -rf "${GOLDENS:?}/$before"
done
echo
for arm in seeded promoted; do
  awk -v a="$arm" '$1 == a {print $2}' "$BASE/results" | sort -n | awk -v a="$arm" '{x[NR]=$1} END {printf "%-8s refresh: median %.1fs [%.1f-%.1f], n=%d\n", a, x[int((NR+1)/2)], x[1], x[NR], NR}'
  awk -v a="$arm" '$1 == a {print $3}' "$BASE/results" | sort -n | awk -v a="$arm" '{x[NR]=$1} END {printf "%-8s next worktree first edit: median %.1fs [%.1f-%.1f], n=%d\n", a, x[int((NR+1)/2)], x[1], x[NR], NR}'
done
