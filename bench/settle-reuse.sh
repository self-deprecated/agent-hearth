#!/usr/bin/env bash
# Whether `settle` reusing main's results for unchanged test binaries reaches the whole suite's
# verdict, and how much sooner. On a public repository at a pinned commit: a golden and its test
# suite (`golden settle`, which records each test binary's results), then a worktree spawned from
# it. Each case makes a change and builds its tests (untimed: a settle's own build costs the same
# either way), then settles twice, timed, with reuse and with AGENT_HEARTH_REUSE=off (the whole
# suite: the oracle; another gate, so neither reuses the other's verdict), each with `--json
# --accept-fewer` (no stored verdict is reused, the test count is not guarded) and the worktree's
# failure history cleared (no failed-first pass), and compares status, test count, failures and
# main's failures. One worktree, not one per mode: two atuin worktrees and the golden filled
# 80 GB. The run stops when the disk has less than 15 GB free.
# Cases, each from main, in FILE (by default the first of FILES), a library crate's source:
#   noop     nothing changed
#   lib      a function added at the end of FILE (the agent edit of the other scripts)
#   shift    a comment line added at the top of FILE: every item in it moves
#   failing  a failing test added to FILE
#   fixture  a file that is not Rust added beside FILE
#   doc      FILE's first `///` line edited: a doc comment only
# REPS rounds, the cases in order, reuse first in odd rounds and second in even ones. Everything
# lives under $BENCH_DIR/agent-hearth-settle-reuse-bench (BENCH_DIR: ~/.cache), removed at the end.
# Linux only (reflinks, bwrap).
# Usage (in a shell with the repository's toolchain):
#   AH=<agent-hearth binary> REV=<commit> FILES="<library file> …" settle-reuse.sh <git clone>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
: "${FILES[0]:?set FILES to the files the agents edit}"
SRC=$1 REV=${REV:?set REV} REPS=${REPS:-1} AH=${AH:-agent-hearth}
CASES=${CASES:-noop lib shift failing fixture doc} FILE=${FILE:-${FILES[0]}}
BASE=${BENCH_DIR:-$HOME/.cache}/agent-hearth-settle-reuse-bench; rm -rf "$BASE"; mkdir -p "$BASE"; cd "$BASE"
# Removed however the run ends: a run that stopped early once left 60 GB behind.
trap 'cd /; rm -rf "$BASE"' EXIT
export AGENT_HEARTH_STATE_DIR=$BASE/state AGENT_HEARTH_CACHE_DIR=$BASE/cache CARGO_INCREMENTAL=1 \
  AGENT_HEARTH_WARM=off
unset CARGO_TARGET_DIR CI AGENT_HEARTH_REUSE
FIXTURE=$(dirname "$FILE")/hearth-fixture.txt

# change <worktree> <case>: the case's change, in a worktree put back at main first. Only FILE is
# restored, and only when it differs: `git checkout -- .` rewrites every file whose mtime is not
# the one git recorded, which the seeding gave main's, and cargo then rebuilt the workspace.
change() {
  (cd "$1" && { git diff --quiet -- "$FILE" || git checkout -q -- "$FILE"; } && rm -f "$FIXTURE")
  case $2 in
    noop) ;;
    lib) printf '\n#[allow(dead_code)]\nfn settle_reuse_edit() -> u32 {\n    7\n}\n' >> "$1/$FILE" ;;
    shift) sed -i '1i // settle-reuse: every item below moves down a line' "$1/$FILE" ;;
    failing) printf '\n#[test]\nfn settle_reuse_seeded_failure() {\n    assert_eq!(1 + 1, 3, "seeded");\n}\n' >> "$1/$FILE" ;;
    fixture) echo "settle-reuse fixture" > "$1/$FIXTURE" ;;
    doc) sed -i '0,/^\(\s*\)\/\/\/ /s//\1\/\/\/ (edited) /' "$1/$FILE" ;;
  esac
}
# settle <worktree> <reuse: on|off> <out>: a timed settle; its JSON report in <out>, seconds and
# exit code printed.
settle() {
  rm -f "$BASE"/state/*/settles/*.failed.json
  local s rc; s=$(now)
  (cd "$1" && AGENT_HEARTH_REUSE=$2 "$AH" settle --json --accept-fewer > "$3" 2> "$3.err"); rc=$?
  printf '%.2f %d\n' "$(dt "$s")" $rc
}
# The verdict the oracle compares: status, tests, main's failures and the failure set. Tests that
# failed and then passed (flaky) differ between two whole-suite runs too, so they are shown, not
# compared.
verdict() { jq -c '{status, tests, existing, failures: (.failures // [] | sort)}' "$1" 2>/dev/null; }
flaky() { jq -c '.flaky // [] | sort' "$1" 2>/dev/null; }
# Stops the run when the build directories' filesystem has less than 15 GB free.
free() { df --output=avail -BG "$BASE" | tail -1 | tr -dc 0-9; }
guard() {
  local free; free=$(free)
  [ "$free" -ge 15 ] || { echo "stopped: $free GB free"; exit 1; }
}
stats() { sort -n | awk '{a[NR]=$1} END {if (NR) printf "median %.1fs [%.1f-%.1f], n=%d", a[int((NR+1)/2)], a[1], a[NR], NR}'; }

echo "agent-hearth: $("$AH" --version), $(cargo --version), $(host), load $(load), $(date -I)"
[ -n "${NOTE:-}" ] && echo "note: $NOTE"
git -c advice.detachedHead=false clone -q "$SRC" repo && (cd repo && git checkout -q -B main "$REV")
echo "repository: $(git -C "$SRC" remote get-url origin 2>/dev/null || echo "$SRC") at $(cd repo && git rev-parse --short=12 HEAD); file: $FILE"
s=$(now); (cd repo && "$AH" golden refresh >/dev/null 2>&1); rc=$?
printf 'golden refresh: %.1fs rc=%d, load %s\n' "$(dt "$s")" $rc "$(load)"
s=$(now); wait_golden_settle
G=$(ls -d "$BASE"/state/*/goldens/*/ | grep -v staging | head -1)
printf "golden's test suite, in the background after it: %.1fs; failures on main: %s; recorded for reuse: %s test binaries, %s libraries' doctests\n" \
  "$(dt "$s")" "$(jq -c length "$G/tests.json" 2>/dev/null)" \
  "$(jq '.binaries | length' "$G/reuse.json" 2>/dev/null)" "$(jq '.doctests | length' "$G/reuse.json" 2>/dev/null)"
[ -f "$G/reuse.json" ] || { echo "no reuse.json: see $G/settle.log"; tail -5 "$G/settle.log"; }
echo "disk: $(free) GB free with the golden ($(du -sh "$G/target" | cut -f1) apparent)"
W=$(cd repo && "$AH" spawn settle-reuse 2>/dev/null)
(cd "$W" && "$AH" cargo test --workspace --no-run >/dev/null 2>&1)
echo "disk: $(free) GB free with the worktree built"; guard

: > "$BASE/results"
for r in $(seq 1 "$REPS"); do
  for c in $CASES; do
    change "$W" "$c"; (cd "$W" && "$AH" cargo test --workspace --no-run >/dev/null 2>&1)
    order="on off"; [ $((r % 2)) = 0 ] && order="off on"
    for m in $order; do
      t=$(settle "$W" "$m" "$BASE/$m.json"); echo "$m ${t% *} ${t#* }" > "$BASE/t.$m"; guard
    done
    read -r _ ton rcon < "$BASE/t.on"; read -r _ toff rcoff < "$BASE/t.off"
    von=$(verdict "$BASE/on.json"); voff=$(verdict "$BASE/off.json")
    same=$([ -n "$von" ] && [ "$von" = "$voff" ] && echo equal || echo DIFFERENT)
    reused=$(jq -r '.reused // 0' "$BASE/on.json" 2>/dev/null) full=$(jq -r '.full // "-"' "$BASE/on.json" 2>/dev/null)
    echo "$c $toff $ton $same" >> "$BASE/results"
    printf 'round %d %-8s whole suite %7.1fs rc=%s | reuse %7.1fs rc=%s, %s tests reused, whole suite because: %s | verdicts %s, load %s, %s GB free\n' \
      "$r" "$c" "$toff" "$rcoff" "$ton" "$rcon" "$reused" "$full" "$same" "$(load)" "$(free)"
    printf '  verdict: %s; flaky with reuse %s, without %s\n' "$(echo "$von" | cut -c1-400)" \
      "$(flaky "$BASE/on.json")" "$(flaky "$BASE/off.json")"
    [ "$same" = equal ] || printf '  without reuse: %s\n' "$(echo "$voff" | cut -c1-400)"
  done
done
echo
for c in $CASES; do
  printf '%-8s whole suite %s; reuse %s; verdicts %s\n' "$c" \
    "$(awk -v c="$c" '$1==c {print $2}' "$BASE/results" | stats)" \
    "$(awk -v c="$c" '$1==c {print $3}' "$BASE/results" | stats)" \
    "$(awk -v c="$c" '$1==c {print $4}' "$BASE/results" | sort | uniq -c | xargs)"
done
