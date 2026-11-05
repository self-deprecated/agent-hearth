#!/usr/bin/env bash
# Warm worktrees without reflinks: on ext4, a new worktree's first and second edit turn with
#   empty   an empty target
#   copy    main's warm target copied in full (`cp -a`; ext4 cannot clone)
#   hearth  agent-hearth: `spawn` from a golden, seeded by hardlinking the artifacts cargo and
#           rustc only replace, read-only, and copying the rest; then `agent-hearth cargo`
# One agent REPS times, variants interleaved, as public.sh does; a turn is `cargo clippy
# --workspace --all-targets`, then `cargo test --workspace --no-run`. Setup is timed apart (the
# copy, or spawn). Then the golden's safety: after more builds in a hearth worktree, each after
# a change that rebuilds what its build directory shares with the golden (`cargo update -p $DEP
# --precise $PRECISE`, RUSTFLAGS changed, BUILD_RS touched), every file of the golden's build
# directory must have the content and mtime it had before any worktree existed.
# Everything lives under $BENCH_DIR/agent-hearth-ext4-bench (BENCH_DIR on ext4), removed at the
# end; a round starts only with 60 GB free. Linux only (bwrap). Warming is off. Main's own warm
# build runs only for the copy variant; STOP_GOLDEN_SETTLE=1 stops the golden's test suite rather
# than wait for it.
# Usage (in a shell with the repository's toolchain and b3sum):
#   AH=<agent-hearth binary> REV=<commit> FILES="<files agents edit>" DEP=<package> PRECISE=<version> BUILD_RS=<build.rs> ext4.sh <git clone>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
: "${FILES[0]:?set FILES to the files the agents edit}"
SRC=$1 REV=${REV:?set REV} REPS=${REPS:-3} AH=${AH:-agent-hearth} VARIANTS=${VARIANTS:-empty copy hearth}
DEP=${DEP:?set DEP} PRECISE=${PRECISE:?set PRECISE} BUILD_RS=${BUILD_RS:?set BUILD_RS} MIN_FREE=${MIN_FREE:-60}
BASE=${BENCH_DIR:-$HOME/.cache}/agent-hearth-ext4-bench; rm -rf "$BASE"; mkdir -p "$BASE"; cd "$BASE"
trap 'cd /; chmod -R u+w "$BASE" 2>/dev/null; rm -rf "$BASE"' EXIT
export AGENT_HEARTH_STATE_DIR=$BASE/state AGENT_HEARTH_CACHE_DIR=$BASE/cache CARGO_INCREMENTAL=1 \
  AGENT_HEARTH_WARM=off
unset CARGO_TARGET_DIR CI RUSTFLAGS
RESULTS=$BASE/results

guard() {
  local free; free=$(df --output=avail -BG "$BASE" | tail -1 | tr -dc 0-9)
  [ "$free" -ge "$MIN_FREE" ] || { echo "stopped: $free GB free"; exit 1; }
}
# mk <variant> <name>: a new jj workspace at main, prepared for the variant; prints its path.
mk() {
  if [ "$1" = hearth ]; then (cd repo && "$AH" spawn "$2" 2>"$BASE/spawn.err"); return; fi
  local w=$BASE/$2
  (cd repo && jj workspace add --quiet --name "$2" -r main "$w" >/dev/null 2>&1)
  [ "$1" = copy ] && cp -a repo/target "$w/target"
  echo "$w"
}
# turn <variant> <worktree>: one agent turn; prints seconds and exit code.
turn() {
  local s c=(cargo); s=$(now); [ "$1" = hearth ] && c=("$AH" cargo)
  (cd "$2" && "${c[@]}" clippy --workspace --all-targets >/dev/null 2>&1 &&
    "${c[@]}" test --workspace --no-run >/dev/null 2>&1)
  local rc=$?; printf '%.2f %d\n' "$(dt "$s")" "$rc"
}
# target <variant> <worktree>: the worktree's build directory.
target() {
  if [ "$1" != hearth ]; then echo "$2/target"; return; fi
  for t in "$BASE"/state/*/targets/*/; do
    grep -q "\"$2\"" "${t%/}/../../worktrees/$(basename "$t").json" 2>/dev/null && echo "${t%/}"
  done
}
# own <dir>: GiB of the files under it that no other directory shares (one link).
own() { find "$1" -type f -links 1 -printf '%s\n' 2>/dev/null | awk '{s += $1} END {printf "%.1f GiB", s / 1073741824}'; }
# manifest <dir>: path, mtime and blake3 of every file under it.
manifest() {
  (cd "$1" && find . -type f -printf '%P\t%T@\n' | sort > "$BASE/m.stat"
   find . -type f -printf '%P\0' | xargs -0 -r b3sum -- | sed 's/  /\t/' | awk -F'\t' '{print $2 "\t" $1}' | sort > "$BASE/m.hash")
  join -t $'\t' "$BASE/m.stat" "$BASE/m.hash"
}
stats() { sort -n | awk -v l="$1" '{a[NR]=$1} END {printf "%s median %.1fs [%.1f-%.1f], n=%d\n", l, a[int((NR+1)/2)], a[1], a[NR], NR}'; }

echo "agent-hearth: $("$AH" --version), $(cargo --version), $(host), load $(load), $(date -I)"
[ -n "${NOTE:-}" ] && echo "note: $NOTE"
echo "filesystem: $(findmnt -no FSTYPE -T "$BASE") at $BASE, $(df --output=avail -BG "$BASE" | tail -1 | tr -d ' ') free"
git -c advice.detachedHead=false clone -q "$SRC" repo && (cd repo && git checkout -q -B main "$REV" && jj git init --colocate >/dev/null 2>&1)
echo "repository: $(git -C "$SRC" remote get-url origin 2>/dev/null || echo "$SRC") at $(cd repo && git rev-parse --short=12 HEAD)"
if [[ " $VARIANTS " == *" copy "* ]]; then
  s=$(now); (cd repo && cargo clippy --workspace --all-targets >/dev/null 2>&1 &&
    cargo test --workspace --no-run >/dev/null 2>&1); rc=$?
  printf 'warm build of main at its own path (what copy copies): %.1fs rc=%d, %s, load %s\n' "$(dt "$s")" $rc "$(own repo/target)" "$(load)"
fi
guard
s=$(now); (cd repo && "$AH" golden refresh >/dev/null 2>&1); rc=$?
printf 'golden refresh (fresh: no reflinks): %.1fs rc=%d, load %s\n' "$(dt "$s")" $rc "$(load)"
if [ -n "${STOP_GOLDEN_SETTLE:-}" ]; then
  for p in $(pgrep -f 'agent-hearth golden settl[e]'); do kill -- "-$(ps -o pgid= "$p" | tr -d ' ')" 2>/dev/null; done
  echo "golden's test suite, in the background after it: stopped"
else
  s=$(now); wait_golden_settle; printf "golden's test suite, in the background after it: %.1fs\n" "$(dt "$s")"
fi
g=$(echo "$BASE"/state/*/goldens); g=$g/$(cat "$g/current")
(cd repo && "$AH" doctor | grep -E '^(mode|FAIL|ok  )' | sed 's/^/  /')
manifest "$g/target" > "$BASE/golden.before"
echo "golden build directory: $(wc -l < "$BASE/golden.before") files, $(du -sh "$g/target" | cut -f1)"

echo "=== one agent: a new worktree's first and second edit, $REPS rounds, variants interleaved"
for r in $(seq 1 "$REPS"); do
  for v in $VARIANTS; do
    guard
    s=$(now); W=$(mk "$v" "$v-$r"); setup=$(dt "$s")
    edit "$W" 1 1; first=$(turn "$v" "$W"); edit "$W" 1 2; second=$(turn "$v" "$W")
    echo "$v $setup $first $second" >> "$RESULTS"
    target=$(target "$v" "$W")
    printf '%-6s round %d: setup %6.2fs  first %s  second %s  own files %s  load %s\n' \
      "$v" "$r" "$setup" "${first% *}s rc=${first#* }" "${second% *}s rc=${second#* }" "$(own "$target")" "$(load)"
    [ "$v" = hearth ] && [ "$r" = 1 ] && sed 's/^/  spawn: /' "$BASE/spawn.err"
    chmod -R u+w "$target" 2>/dev/null; rm -rf "$W" "$target"
  done
done
for v in $VARIANTS; do
  awk -v v="$v" '$1==v {print $2}' "$RESULTS" | stats "$v setup:      "
  awk -v v="$v" '$1==v {print $3}' "$RESULTS" | stats "$v first edit: "
  awk -v v="$v" '$1==v {print $5}' "$RESULTS" | stats "$v second edit:"
done

echo "=== the golden after builds that rebuild what a hearth worktree shares with it"
W=$(mk hearth safety)
printf 'edit:                     %s\n' "$(edit "$W" 1 1; turn hearth "$W")"
printf 'cargo update -p %s --precise %s: %s\n' "$DEP" "$PRECISE" "$( (cd "$W" && cargo update --quiet -p "$DEP" --precise "$PRECISE") && turn hearth "$W")"
printf 'RUSTFLAGS="--cfg agent_hearth_probe": %s\n' "$(RUSTFLAGS="--cfg agent_hearth_probe" turn hearth "$W")"
printf 'touch %s: %s\n' "$BUILD_RS" "$(touch "$W/$BUILD_RS"; turn hearth "$W")"
manifest "$g/target" > "$BASE/golden.after"
changed=$(join -t $'\t' "$BASE/golden.before" "$BASE/golden.after" | awk -F'\t' '$3 != $5 {c++} END {print c+0}')
touched=$(join -t $'\t' "$BASE/golden.before" "$BASE/golden.after" | awk -F'\t' '$3 == $5 && $2 != $4 {c++} END {print c+0}')
missing=$(join -t $'\t' -v1 "$BASE/golden.before" "$BASE/golden.after" | wc -l)
echo "golden's files afterwards: $changed with other content, $touched with only another mtime, $missing missing, of $(wc -l < "$BASE/golden.before")"
join -t $'\t' "$BASE/golden.before" "$BASE/golden.after" | awk -F'\t' '$3 != $5 || $2 != $4 {printf "  %s %s (mtime %s -> %s)\n", ($3 != $5 ? "content" : "mtime"), $1, $2, $4}' | head -20
