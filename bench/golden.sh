#!/usr/bin/env bash
# What a golden refresh costs when main moves: a fresh golden of commit B against one seeded from
# a golden of A (`agent-hearth golden refresh` with the current golden at A). Each pair in PAIRS
# ("A..B", space-separated) gets its own state. Then, from each golden, a new worktree at B: does
# its test build compile anything? The build log is empty, so no per-member variants are
# built (agents built no package alone yet). Everything lives under
# ~/.cache/agent-hearth-golden-bench, removed at the end. Linux only (btrfs for the sizes).
# Usage (in a shell with the repository's toolchain):
#   AH=<agent-hearth binary> PAIRS="<A>..<B> …" golden.sh <git clone>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
SRC=$1 AH=$(realpath "${AH:-$(command -v agent-hearth)}") PAIRS=${PAIRS:?set PAIRS}
BASE=$HOME/.cache/agent-hearth-golden-bench; rm -rf "$BASE"; mkdir -p "$BASE"; cd "$BASE"
unset CARGO_TARGET_DIR CI

# refresh <state> <rev>: a golden refresh; prints seconds, exit code, generation and exclusive size.
refresh() {
  local s rc g dir
  local took
  s=$(now); (cd "$BASE/repo" && AGENT_HEARTH_STATE_DIR=$1 AGENT_HEARTH_CACHE_DIR=$1-cache "$AH" golden refresh --rev "$2" >/dev/null 2>&1); rc=$?
  took=$(dt "$s"); wait_golden_settle
  g=$(echo "$1"/*/goldens); dir=$g/$(cat "$g/current")
  sync -f "$dir"
  printf '%.1fs rc=%d generation %s, exclusive %s' "$took" "$rc" \
    "$(jq -r '.generation // 0' "$dir/golden.json")" "$(btrfs filesystem du -s "$dir" 2>/dev/null | awk 'NR==2 {print $2}')"
}

echo "agent-hearth: $("$AH" --version), $(cargo --version), $(host), load $(load), $(date -I)"
git -c advice.detachedHead=false clone -q "$SRC" repo && (cd repo && jj git init --colocate >/dev/null 2>&1)
echo "repository: $(git -C "$SRC" remote get-url origin 2>/dev/null || echo "$SRC")"
n=0
for pair in $PAIRS; do
  n=$((n + 1))
  a=$(git -C repo rev-parse --short=12 "${pair%..*}") b=$(git -C repo rev-parse --short=12 "${pair#*..}")
  echo "=== $a..$b: $(git -C repo rev-list --count "$a..$b") commits, $(git -C repo diff --shortstat "$a" "$b")"
  echo "fresh golden of $b:  $(refresh "$BASE/fresh-$b" "$b"), load $(load)"
  echo "golden of $a:        $(refresh "$BASE/seeded-$b" "$a"), load $(load)"
  echo "seeded golden of $b: $(refresh "$BASE/seeded-$b" "$b"), load $(load)"
  # A worktree at B from each golden: what does its test build compile? Named for the pair, as
  # two pairs may end at the same commit.
  for kind in fresh seeded; do
    export AGENT_HEARTH_STATE_DIR=$BASE/$kind-$b AGENT_HEARTH_CACHE_DIR=$BASE/$kind-$b-cache AGENT_HEARTH_WARM=off
    W=$(cd repo && "$AH" spawn "check-$kind-$n" --rev "$b" 2>/dev/null)
    # To a file: nix shells export `out`, and a variable of that name would carry the JSON into
    # every later command's environment.
    rm -f "$BASE/build.json"
    (cd "$W" && "$AH" cargo test --workspace --no-run --message-format=json > "$BASE/build.json" 2>/dev/null); rc=$?
    echo "worktree at $b from the $kind golden: test build rc=$rc, compiled $(grep -c '"fresh":false' "$BASE/build.json") of $(grep -c '"reason":"compiler-artifact"' "$BASE/build.json") artifacts"
    unset AGENT_HEARTH_STATE_DIR AGENT_HEARTH_CACHE_DIR AGENT_HEARTH_WARM
    rm -rf "$W"
  done
  rm -rf "$BASE/fresh-$b" "$BASE/seeded-$b" "$BASE/fresh-$b-cache" "$BASE/seeded-$b-cache"
done
cd /; rm -rf "$BASE"
