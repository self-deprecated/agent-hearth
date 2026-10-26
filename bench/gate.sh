#!/usr/bin/env bash
# The turn-end gate on a public repository whose main may not be clean under this toolchain (not
# formatted for it, or with clippy warnings): `agent-hearth check` in a new worktree with no
# change must pass and leave every file alone; then a seeded type error must come back as one
# line. Everything lives under ~/.cache/agent-hearth-gate-bench, removed at the end.
# Usage (in a shell with the repository's toolchain):
#   AH=<agent-hearth binary> REV=<commit> FILE=<file for the type error> gate.sh <git clone>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
SRC=$1 REV=${REV:?set REV} FILE=${FILE:?set FILE} AH=$(realpath "${AH:-$(command -v agent-hearth)}")
BASE=$HOME/.cache/agent-hearth-gate-bench; rm -rf "$BASE"; mkdir -p "$BASE"; cd "$BASE"
export AGENT_HEARTH_STATE_DIR=$BASE/state AGENT_HEARTH_CACHE_DIR=$BASE/cache AGENT_HEARTH_WARM=off
unset CARGO_TARGET_DIR CI

echo "agent-hearth: $("$AH" --version), $(cargo --version), $(host), load $(load), $(date -I)"
git -c advice.detachedHead=false clone -q "$SRC" repo &&
  (cd repo && git checkout -q -B main "$REV" && jj git init --colocate >/dev/null 2>&1)
echo "repository: $(git -C "$SRC" remote get-url origin 2>/dev/null || echo "$SRC") at $(git -C repo rev-parse --short=12 HEAD)"
echo "main under this toolchain: $(cd repo && cargo fmt --all --check >/dev/null 2>&1 && echo formatted || echo "not formatted ($(cargo fmt --all --check 2>/dev/null | grep -c '^Diff in') diffs)")"
s=$(now); (cd repo && "$AH" golden refresh >/dev/null 2>&1); rc=$?
printf 'golden refresh: %.1fs rc=%d\n' "$(dt "$s")" $rc
wait_golden_settle
echo "baseline: $(jq -c '{fmt_clean, fixable, diagnostics: (.diagnostics | length)}' "$BASE"/state/*/goldens/*/baseline.json)"
W=$(cd repo && "$AH" spawn gate 2>/dev/null)
s=$(now); (cd "$W" && "$AH" check > "$BASE/check.txt" 2>&1); rc=$?
printf 'check, no change: %.1fs exit %d; files changed: %s\n' "$(dt "$s")" $rc "$(cd "$W" && git status --short | wc -l)"
sed 's/^/  /' "$BASE/check.txt"
printf '\npub fn gate_broken() -> u32 { "not a number" }\n' >> "$W/$FILE"
s=$(now); (cd "$W" && "$AH" check > "$BASE/check.txt" 2>&1); rc=$?
printf 'check, a type error: %.1fs exit %d; files changed: %s\n' "$(dt "$s")" $rc "$(cd "$W" && git status --short | wc -l)"
sed 's/^/  /' "$BASE/check.txt"
cd /; rm -rf "$BASE"
