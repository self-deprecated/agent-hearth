#!/usr/bin/env bash
# How soon `settle` tells an agent its tests fail, on a public repository. One worktree: a failing
# test added, settled (the whole suite); an unrelated change with the test still failing, settled
# again (the recent failure runs first, on its own); the test fixed, settled (the whole suite).
# Then N worktrees settle at once, admitted by the daemon, against N times one settle alone (how
# long they would take queued one at a time). Everything lives under
# ~/.cache/agent-hearth-feedback-bench, removed at the end.
# Usage (in a shell with the repository's toolchain):
#   AH=<agent-hearth binary> REV=<commit> FILES="<lib.rs files with tests>" feedback.sh <git clone>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
: "${FILES[0]:?set FILES to library files the agents add tests to}"
SRC=$1 REV=${REV:?set REV} N=${N:-3} AH=$(realpath "${AH:-$(command -v agent-hearth)}")
BASE=$HOME/.cache/agent-hearth-feedback-bench; rm -rf "$BASE"; mkdir -p "$BASE"; cd "$BASE"
export AGENT_HEARTH_STATE_DIR=$BASE/state AGENT_HEARTH_CACHE_DIR=$BASE/cache AGENT_HEARTH_WARM=off
unset CARGO_TARGET_DIR CI

file() { echo "${FILES[$(( ($1 - 1) % ${#FILES[@]} ))]}"; }
# settle <worktree> <label>: a settle in the foreground, timed; its first lines indented.
settle() {
  local s rc; s=$(now)
  (cd "$1" && "$AH" settle > "$BASE/settle.txt" 2>&1); rc=$?
  printf '%s: %.1fs exit %d\n' "$2" "$(dt "$s")" $rc
  head -4 "$BASE/settle.txt" | cut -c1-160 | sed 's/^/  /'
}

echo "agent-hearth: $("$AH" --version), $(cargo --version), $(host), load $(load), $(date -I)"
git -c advice.detachedHead=false clone -q "$SRC" repo &&
  (cd repo && git checkout -q -B main "$REV" && jj git init --colocate >/dev/null 2>&1)
echo "repository: $(git -C "$SRC" remote get-url origin 2>/dev/null || echo "$SRC") at $(git -C repo rev-parse --short=12 HEAD)"
s=$(now); (cd repo && "$AH" golden refresh >/dev/null 2>&1); rc=$?
printf 'golden refresh: %.1fs rc=%d\n' "$(dt "$s")" $rc
s=$(now); wait_golden_settle
printf "golden's test suite: %.1fs; failures on main: %s\n" "$(dt "$s")" \
  "$(cat "$BASE"/state/*/goldens/*/tests.json 2>/dev/null | jq -c length | paste -sd, -)"

W=$(cd repo && "$AH" spawn one 2>/dev/null)
F=$W/$(file 1)
printf '\n#[test]\nfn feedback_bench() {\n    assert_eq!(1, 2);\n}\n' >> "$F"
(cd "$W" && "$AH" cargo test --workspace --no-run >/dev/null 2>&1)  # built, as an agent's would be
settle "$W" "a failing test, first settle"
printf '\n#[allow(dead_code)]\nfn feedback_bench_other() -> u32 { 1 }\n' >> "$F"
(cd "$W" && "$AH" cargo test --workspace --no-run >/dev/null 2>&1)
settle "$W" "an unrelated change, the test still failing"
sed -i.bak 's/assert_eq!(1, 2);/assert_eq!(1, 1);/' "$F" && rm -f "$F.bak"
(cd "$W" && "$AH" cargo test --workspace --no-run >/dev/null 2>&1)
settle "$W" "the test fixed"

for i in $(seq 1 "$N"); do
  M[i]=$(cd repo && "$AH" spawn "many$i" 2>/dev/null)
  printf '\n#[allow(dead_code)]\nfn feedback_bench_%s() -> u32 { %s }\n' "$i" "$i" >> "${M[i]}/$(file "$i")"
  (cd "${M[i]}" && "$AH" cargo test --workspace --no-run >/dev/null 2>&1)
done
s=$(now)
for i in $(seq 1 "$N"); do
  ( t=$(now); (cd "${M[i]}" && "$AH" settle >/dev/null 2>&1); rc=$?
    printf '%.1f %d\n' "$(dt "$t")" $rc > "$BASE/m.$i" ) &
done
wait
printf '%s settles at once: wall %.1fs; each %s\n' "$N" "$(dt "$s")" "$(cat "$BASE"/m.* | awk '{printf "%s%.1fs rc=%d", (NR>1?", ":""), $1, $2}')"
grep -E 'admit|queue|done' "$AGENT_HEARTH_STATE_DIR/daemon.log" 2>/dev/null | sed "s|$BASE/||; s/^/  /" | tail -$((2 * N))
cd /; rm -rf "$BASE"
