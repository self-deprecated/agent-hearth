#!/usr/bin/env bash
# Does `agent-hearth warm` shorten the agent's own build after it edits? A replayed agent turn in a
# worktree that has already built at its path: EDITS edits, one per file in FILES, GAP seconds
# after each (an agent thinking before its next tool call; each of GAPS in turn), then the
# agent's build,
# `agent-hearth cargo test --workspace --no-run`, which is what is timed. `on` runs the
# PostToolUse hook after each edit, as Claude Code would; `off` does not. Variants interleave,
# REPS turns each, each in its own worktree. Scenarios:
#   independent  each edit compiles on its own
#   dependent    the first edit calls a function the last one adds to the same file, so its
#                crate (and what depends on it) cannot compile until the agent is done
#   next         one edit, then the agent's `agent-hearth cargo check --workspace`, then GAP
#                seconds, then the timed build: a different command with no edit between, which
#                a warm started after the check builds (agents' most common rerun)
# LOAD=n runs n busy loops at normal priority beside the turns, standing in for other agents'
# builds. Everything lives under ~/.cache/agent-hearth-warm-bench, removed at the end. Linux only.
# Usage (in a shell with the repository's toolchain):
#   AH=<agent-hearth binary> REV=<commit> FILES="<files, in different crates>" warm.sh <git clone>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
: "${FILES[0]:?set FILES to the files the agents edit}"
SRC=$1 REV=${REV:?set REV} REPS=${REPS:-5} AH=$(realpath "${AH:-$(command -v agent-hearth)}")
GAPS=${GAPS:-0 4 12} LOAD=${LOAD:-0} EDITS=${EDITS:-${#FILES[@]}} SCENARIOS=${SCENARIOS:-independent dependent next}
BASE=$HOME/.cache/agent-hearth-warm-bench; rm -rf "$BASE"; mkdir -p "$BASE"; cd "$BASE"
export AGENT_HEARTH_STATE_DIR=$BASE/state AGENT_HEARTH_CACHE_DIR=$BASE/cache
unset CARGO_TARGET_DIR CI
declare -A W

# hook <worktree>: Claude Code's PostToolUse after an Edit.
hook() { printf '{"tool_name":"Edit","cwd":"%s","hook_event_name":"PostToolUse"}' "$1" | "$AH" hook claude-post-tool-use; }
# Waits until no warm runs; prints how long that took.
settled() { local s; s=$(now); while pgrep -f 'agent-hearth war[m]$' >/dev/null; do sleep 0.2; done; dt "$s"; }
# build <worktree>: the agent's build; prints seconds and exit code.
build() {
  local s rc; s=$(now)
  (cd "$1" && "$AH" cargo test --workspace --no-run >/dev/null 2>&1); rc=$?
  printf '%.2f %d\n' "$(dt "$s")" "$rc"
}
# turn <variant> <scenario> <worktree> <n>: the agent's nth turn.
turn() {
  local v=$1 scenario=$2 w=$3 n=$4 k f
  # `off` has no warms at all: agent-hearth's own cargo runs start one after each build too.
  if [ "$v" = off ]; then export AGENT_HEARTH_WARM=off; else unset AGENT_HEARTH_WARM; fi
  if [ "$scenario" = next ]; then
    printf '\n#[allow(dead_code)]\nfn warm_next_%s() -> u32 { %s }\n' "$n" "$n" >> "$w/${FILES[0]}"
    [ "$v" = on ] && hook "$w"
    (cd "$w" && "$AH" cargo check --workspace >/dev/null 2>&1)
    sleep "$GAP"
    build "$w"
    return
  fi
  for k in $(seq 1 "$EDITS"); do
    f=$w/${FILES[$(( (k - 1) % ${#FILES[@]} ))]}
    if [ "$scenario" = dependent ] && [ "$k" = "$EDITS" ]; then f=$w/${FILES[0]}; fi
    if [ "$scenario" = dependent ] && [ "$k" = 1 ]; then
      # Calls what the last edit will add to this file: broken until then.
      printf '\n#[allow(dead_code)]\nfn warm_call_%s() -> u32 { warm_added_%s() }\n' "$n" "$n" >> "$f"
    elif [ "$scenario" = dependent ] && [ "$k" = "$EDITS" ]; then
      printf '\n#[allow(dead_code)]\nfn warm_added_%s() -> u32 { %s }\n' "$n" "$n" >> "$f"
    else
      printf '\n#[allow(dead_code)]\nfn warm_edit_%s_%s() -> u32 { %s }\n' "$n" "$k" "$n" >> "$f"
    fi
    [ "$v" = on ] && hook "$w"
    sleep "$GAP"
  done
  build "$w"
}

echo "agent-hearth: $("$AH" --version), $(cargo --version), $(host), load $(load), $(date -I)"
echo "$EDITS edits, files: ${FILES[*]:0:$EDITS}; gaps after each edit: $GAPS s"
git -c advice.detachedHead=false clone -q "$SRC" repo &&
  (cd repo && git checkout -q -B main "$REV" && jj git init --colocate >/dev/null 2>&1)
echo "repository: $(git -C "$SRC" remote get-url origin 2>/dev/null || echo "$SRC") at $(cd repo && git rev-parse --short=12 HEAD)"
s=$(now); (cd repo && "$AH" golden refresh >/dev/null 2>&1); rc=$?
printf 'golden refresh: %.1fs rc=%d, load %s\n' "$(dt "$s")" $rc "$(load)"
wait_golden_settle
busy=()
for _ in $(seq 1 "$LOAD"); do (while :; do :; done) & busy+=($!); done
trap '[ ${#busy[@]} -gt 0 ] && kill "${busy[@]}" 2>/dev/null' EXIT
[ "$LOAD" -gt 0 ] && echo "$LOAD busy loops beside the turns"

for GAP in $GAPS; do
for scenario in $SCENARIOS; do
  [ "$scenario" = dependent ] && [ "$EDITS" -lt 2 ] && continue
  echo "=== $scenario edits, ${GAP}s after each"
  : > "$BASE/results"
  for v in off on; do
    W[$v]=$(cd repo && "$AH" spawn "$scenario-$GAP-$v" 2>/dev/null)
    # The worktree's first build at its path, which `warm` does not change.
    printf '%s first build: %s\n' "$v" "$(export AGENT_HEARTH_WARM=off; build "${W[$v]}")"
  done
  for r in $(seq 1 "$REPS"); do
    for v in off on; do
      t=$(turn "$v" "$scenario" "${W[$v]}" "$r")
      after=$(settled)
      echo "$v ${t% *}" >> "$BASE/results"
      printf '%-3s turn %d: build %6.2fs rc=%s  warm still running after it %5.1fs  load %s\n' \
        "$v" "$r" "${t% *}" "${t#* }" "$after" "$(load)"
    done
  done
  for v in off on; do
    awk -v v="$v" '$1==v {print $2}' "$BASE/results" | sort -n |
      awk -v v="$v" '{a[NR]=$1} END {printf "%s: median %.1fs [%.1f-%.1f], n=%d\n", v, a[int((NR+1)/2)], a[1], a[NR], NR}'
  done
  # The warm log of the `on` worktree's last turn: did its builds run?
  log=$(ls -t "$BASE"/state/*/warms/*.log 2>/dev/null | head -1)
  [ -n "$log" ] && grep -a 'agent-hearth warm:' "$log" | tail -3 | sed 's/^/  last warm: /'
  for v in off on; do rm -rf "$(cd "${W[$v]}" && "$AH" doctor --json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin).get("target",""))' 2>/dev/null)"; done
done
done
cd /; rm -rf "$BASE"
