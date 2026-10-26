#!/usr/bin/env bash
# `land` and `gc` end to end on a public repository. Four agents' workspaces: two with a passing
# test each, one with a failing test, one with no change. `land` them all: the two land, the other
# two are refused with why, main moves and the golden refreshes in the background. Then a new
# worktree's first build, the landed workspaces removed through the WorktreeRemove hook, and `gc`.
# Everything lives under ~/.cache/agent-hearth-land-bench, removed at the end.
# Usage (in a shell with the repository's toolchain):
#   AH=<agent-hearth binary> REV=<commit> FILES="<lib.rs files with tests>" land.sh <git clone>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
: "${FILES[0]:?set FILES to library files the agents add tests to}"
SRC=$1 REV=${REV:?set REV} AH=$(realpath "${AH:-$(command -v agent-hearth)}")
BASE=$HOME/.cache/agent-hearth-land-bench; rm -rf "$BASE"; mkdir -p "$BASE"; cd "$BASE"
export AGENT_HEARTH_STATE_DIR=$BASE/state AGENT_HEARTH_CACHE_DIR=$BASE/cache AGENT_HEARTH_WARM=off
unset CARGO_TARGET_DIR CI

file() { echo "${FILES[$(( ($1 - 1) % ${#FILES[@]} ))]}"; }
golden_count() { find "$BASE"/state/*/goldens -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l | tr -d ' '; }
wait_refresh() { while pgrep -f 'agent-hearth golden refres[h]' >/dev/null; do sleep 2; done; wait_golden_settle; }

echo "agent-hearth: $("$AH" --version), $(cargo --version), $(host), load $(load), $(date -I)"
git -c advice.detachedHead=false clone -q "$SRC" repo &&
  (cd repo && git checkout -q -B main "$REV" && jj git init --colocate >/dev/null 2>&1)
echo "repository: $(git -C "$SRC" remote get-url origin 2>/dev/null || echo "$SRC") at $(git -C repo rev-parse --short=12 HEAD)"
s=$(now); (cd repo && "$AH" golden refresh >/dev/null 2>&1); rc=$?
printf 'golden refresh: %.1fs rc=%d\n' "$(dt "$s")" $rc
s=$(now); wait_golden_settle; printf "golden's test suite: %.1fs\n" "$(dt "$s")"

for i in 1 2 3 4; do W[i]=$(cd repo && "$AH" spawn "a$i" 2>/dev/null); done
for i in 1 2; do
  printf '\n#[test]\nfn land_bench_%s() {\n    assert_eq!(%s + 1, %s);\n}\n' "$i" "$i" "$((i + 1))" >> "${W[i]}/$(file "$i")"
done
printf '\n#[test]\nfn land_bench_fails() {\n    assert_eq!(1, 2);\n}\n' >> "${W[3]}/$(file 3)"
for i in 1 2 3; do (cd "${W[i]}" && jj commit --quiet -m "agent $i: a test" >/dev/null 2>&1); done
echo "workspaces: a1, a2 a passing test each; a3 a failing test; a4 no change"

s=$(now); (cd repo && "$AH" land a1 a2 a3 a4 > "$BASE/land.txt" 2>&1); rc=$?
printf 'land a1 a2 a3 a4: %.1fs exit %d\n' "$(dt "$s")" $rc
sed 's/^/  /' "$BASE/land.txt"
echo "main now: $(cd repo && jj log --no-graph -r 'main' -T 'description.first_line()' --quiet 2>/dev/null)"
echo "  and before it: $(cd repo && jj log --no-graph -r 'main-' -T 'description.first_line()' --quiet 2>/dev/null)"
s=$(now); wait_refresh; printf 'golden refresh in the background, after land: done %.1fs later; goldens: %s\n' "$(dt "$s")" "$(golden_count)"

W5=$(cd repo && "$AH" spawn a5 2>/dev/null)
s=$(now); (cd "$W5" && "$AH" cargo test --workspace --no-run > "$BASE/a5.txt" 2>&1); rc=$?
printf 'a new worktree after land, first test build: %.1fs rc=%d, %s crates compiled\n' "$(dt "$s")" $rc "$(grep -c '^ *Compiling' "$BASE/a5.txt")"

for i in 1 2 3; do
  printf '{"worktree_path":"%s"}' "${W[i]}" | "$AH" hook claude-worktree-remove > "$BASE/rm.txt" 2>&1
  rc=$?; case ${W[i]} in */slots/*) left="kept: a slot, free for the next agent" ;; *) left=$([ -e "${W[i]}" ] && echo "still there" || echo gone) ;; esac
  echo "remove a$i: exit $rc, $left $(head -c 200 "$BASE/rm.txt")"
done
# a3's refused commit is still its work after its workspace is gone.
echo "a3's commit after removal: $(cd repo && jj log --no-graph -r 'description(substring:"agent 3")' -T 'commit_id.short() ++ " " ++ description.first_line()' --quiet 2>&1)"
s=$(now); (cd repo && "$AH" gc > "$BASE/gc.txt" 2>&1); rc=$?
printf 'gc: %.1fs exit %d; goldens left: %s\n' "$(dt "$s")" $rc "$(golden_count)"
sed 's/^/  /' "$BASE/gc.txt"
(cd repo && "$AH" status 2>&1 | sed 's/^/  /')
cd /; rm -rf "$BASE"
