#!/usr/bin/env bash
# The slot pool, where there is no pin (macOS): a new slot's first and second edit, then the same
# slot reused by the next agent, through `spawn`, `agent-hearth cargo` and the WorktreeRemove hook
# that frees it; then `check`. A turn is `agent-hearth cargo test --workspace --no-run`. REPS
# agents one after another reuse the slot. Everything lives under
# ~/.cache/agent-hearth-slots-bench, removed at the end.
# Usage (in a shell with the repository's toolchain):
#   AH=<agent-hearth binary> REV=<commit> FILES="<files agents edit>" slots.sh <git clone>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
: "${FILES[0]:?set FILES to the files the agents edit}"
SRC=$1 REV=${REV:?set REV} REPS=${REPS:-3} AH=$(realpath "${AH:-$(command -v agent-hearth)}")
BASE=$HOME/.cache/agent-hearth-slots-bench; rm -rf "$BASE"; mkdir -p "$BASE"; cd "$BASE"
export AGENT_HEARTH_STATE_DIR=$BASE/state AGENT_HEARTH_CACHE_DIR=$BASE/cache AGENT_HEARTH_WARM=off
unset CARGO_TARGET_DIR CI

turn() {
  local s rc; s=$(now)
  (cd "$1" && "$AH" cargo test --workspace --no-run >/dev/null 2>&1); rc=$?
  printf '%.1fs rc=%d' "$(dt "$s")" "$rc"
}
free_space() { df -h "$BASE" | awk 'NR==2 {print $4}'; }

echo "agent-hearth: $("$AH" --version), $(cargo --version), $(host), load $(load), $(date -I)"
git -c advice.detachedHead=false clone -q "$SRC" repo &&
  (cd repo && git checkout -q -B main "$REV" && jj git init --colocate >/dev/null 2>&1)
echo "repository: $(git -C "$SRC" remote get-url origin 2>/dev/null || echo "$SRC") at $(git -C repo rev-parse --short=12 HEAD)"
(cd repo && "$AH" doctor | grep -E '^(FAIL|ok  )' | sed 's/^/  /')
s=$(now); (cd repo && "$AH" golden refresh >/dev/null 2>&1); rc=$?
printf 'golden refresh: %.1fs rc=%d, free %s\n' "$(dt "$s")" $rc "$(free_space)"
wait_golden_settle
for r in $(seq 1 "$REPS"); do
  W=$(cd repo && "$AH" spawn "agent-$r" 2>/dev/null)
  printf '\n#[allow(dead_code)]\nfn slot_edit_%s_1() -> u32 { 1 }\n' "$r" >> "$W/${FILES[0]}"
  first=$(turn "$W")
  printf '\n#[allow(dead_code)]\nfn slot_edit_%s_2() -> u32 { 2 }\n' "$r" >> "$W/${FILES[0]}"
  second=$(turn "$W")
  echo "agent $r in $(basename "$W") ($( [ "$r" = 1 ] && echo new || echo reused)): first edit $first, second edit $second, free $(free_space)"
  if [ "$r" = "$REPS" ]; then
    s=$(now); (cd "$W" && "$AH" check > "$BASE/check.txt" 2>&1); rc=$?
    printf 'check: %.1fs exit %d, files changed: %s\n' "$(dt "$s")" $rc "$(cd "$W" && git status --short | wc -l | tr -d ' ')"
    sed 's/^/  /' "$BASE/check.txt"
  fi
  # The agent's edits are its work: committed, so freeing the slot keeps them, as an agent's
  # finished change would be.
  (cd "$W" && jj commit --quiet -m "agent $r" >/dev/null 2>&1)
  printf '{"worktree_path":"%s"}' "$W" | "$AH" hook claude-worktree-remove || echo "freeing the slot failed"
done
cd /; rm -rf "$BASE"
