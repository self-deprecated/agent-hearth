#!/usr/bin/env bash
# What `agent-hearth setup --split-tests` saves a test-only edit, and whether it slows a build
# with nothing to do. On a public repository at a pinned commit, two clones with targets of their
# own (plain cargo, each at its own path, as `settings.sh` does): `before`, as it is, and `after`,
# split by `agent-hearth setup --split-tests` (timed; it builds the release and lists the tests
# before and after, and undoes the split unless the release programs are byte-identical and the
# test inventory the same). Both warmed with `cargo test --workspace --no-run`; then REPS turns,
# the two interleaved: a test-only edit, a new test in FILE's tests module (inline before, in its
# `tests.rs` after), and the timed `cargo test --workspace --no-run` after it, with how many units
# it compiled. Last, NOOP pairs of the same build with nothing changed, before then after.
# Everything lives under $BENCH_DIR/agent-hearth-split-tests-bench (BENCH_DIR: ~/.cache),
# removed at the end; the run stops when the disk has less than 15 GB free.
# Usage (in a shell with the repository's toolchain):
#   AH=<agent-hearth binary> REV=<commit> FILE=<file ending in a tests module> split-tests.sh <git clone>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
SRC=$1 REV=${REV:?set REV} FILE=${FILE:?set FILE} REPS=${REPS:-5} NOOP=${NOOP:-10} AH=${AH:-agent-hearth}
BASE=${BENCH_DIR:-$HOME/.cache}/agent-hearth-split-tests-bench; rm -rf "$BASE"; mkdir -p "$BASE"; cd "$BASE"
# Removed however the run ends: a run that stopped early once left 60 GB behind.
trap 'cd /; rm -rf "$BASE"' EXIT
export AGENT_HEARTH_STATE_DIR=$BASE/state AGENT_HEARTH_CACHE_DIR=$BASE/cache CARGO_INCREMENTAL=1
unset CARGO_TARGET_DIR CI

# build <clone>: `cargo test --workspace --no-run`; prints seconds, exit code and units compiled.
build() {
  local s rc; s=$(now)
  (cd "$BASE/$1" && cargo test --workspace --no-run --message-format=json 2>/dev/null > "$BASE/build.json"); rc=$?
  printf '%.2f %d %d\n' "$(dt "$s")" $rc "$(grep -c '"reason":"compiler-artifact".*"fresh":false' "$BASE/build.json")"
}
# edit <clone> <n>: the nth test-only edit, a new test in FILE's tests module.
edit() {
  local test="#[test]\nfn split_tests_edit_$2() {\n    assert_eq!($2, $2);\n}\n"
  if [ "$1" = before ]; then
    sed -i "/^mod tests {\$/a $test" "$BASE/before/$FILE"
  else
    printf "\n$test" >> "$BASE/after/$TESTS"
  fi
}
guard() {
  local free; free=$(df --output=avail -BG "$BASE" | tail -1 | tr -dc 0-9)
  [ "$free" -ge 15 ] || { echo "stopped: $free GB free"; exit 1; }
}
size() { du -sb "$BASE/$1/target" | awk '{printf "%.1f", $1 / 1073741824}'; }
stats() { sort -n | awk '{a[NR]=$1} END {if (NR) printf "median %.2fs [%.2f-%.2f], n=%d", a[int((NR+1)/2)], a[1], a[NR], NR}'; }

echo "agent-hearth: $("$AH" --version), $(cargo --version), $(host), load $(load), $(date -I)"
[ -n "${NOTE:-}" ] && echo "note: $NOTE"
git -c advice.detachedHead=false clone -q "$SRC" before && git -C before checkout -q -B main "$REV"
echo "repository: $(git -C "$SRC" remote get-url origin 2>/dev/null || echo "$SRC") at $(git -C before rev-parse --short=12 HEAD); file: $FILE"
b=$(build before); printf 'before: warm build %.1fs rc=%s (%s units), load %s\n' "${b%% *}" "$(echo "$b" | cut -d' ' -f2)" "${b##* }" "$(load)"
guard; clone_dir before after
s=$(now); (cd after && "$AH" setup --split-tests > "$BASE/split.txt" 2> "$BASE/split.err"); rc=$?
printf 'setup --split-tests: %.1fs rc=%d, load %s\n' "$(dt "$s")" $rc "$(load)"
grep -v '^moved ' "$BASE/split.txt" | sed 's/^/  /'
echo "  moved: $(grep -c '^moved ' "$BASE/split.txt") test modules; diff: $(git -C after diff --shortstat) plus $(git -C after ls-files --others --exclude-standard | wc -l) new files"
[ $rc = 0 ] || { tail -5 "$BASE/split.err"; exit 1; }
TESTS=$(awk -v f="$FILE" '$1 == "moved" && $2 == f {print $4}' "$BASE/split.txt")
[ -n "$TESTS" ] || { echo "$FILE was not split"; exit 1; }
echo "  $FILE -> $TESTS"
b=$(build after); printf 'after: warm build %.1fs rc=%s (%s units), load %s\n' "${b%% *}" "$(echo "$b" | cut -d' ' -f2)" "${b##* }" "$(load)"
guard; echo "targets: before $(size before) GiB, after $(size after) GiB (shared extents counted in both)"

: > "$BASE/results"
for r in $(seq 1 "$REPS"); do
  order="before after"; [ $((r % 2)) = 0 ] && order="after before"
  for v in $order; do
    edit "$v" "$r"; b=$(build "$v")
    echo "edit $v ${b%% *}" >> "$BASE/results"
    printf 'turn %d %-6s test-only edit: build %6.2fs rc=%s, %s units compiled, load %s\n' "$r" "$v" "${b%% *}" "$(echo "$b" | cut -d' ' -f2)" "${b##* }" "$(load)"
  done
done
for r in $(seq 1 "$NOOP"); do
  for v in before after; do
    b=$(build "$v"); echo "noop $v ${b%% *}" >> "$BASE/results"
    printf 'no-op %2d %-6s build %6.2fs rc=%s, %s units compiled\n' "$r" "$v" "${b%% *}" "$(echo "$b" | cut -d' ' -f2)" "${b##* }"
  done
done
echo
for k in edit noop; do
  for v in before after; do
    printf '%-4s %-6s %s\n' "$k" "$v" "$(awk -v k="$k" -v v="$v" '$1==k && $2==v {print $3}' "$BASE/results" | stats)"
  done
done
