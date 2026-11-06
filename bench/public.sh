#!/usr/bin/env bash
# The README's claims on a public repository at a pinned commit. A new worktree's first and
# second edit turn, with:
#   empty   an empty target (what a new worktree gets)
#   copy    main's warm target copied with `cp -a --reflink` (what Worktrunk and git-cow do)
#   sync    the copy, plus main's mtimes on identical source files
#   hearth  agent-hearth: `spawn` from a golden, then `agent-hearth cargo`
#   nightly agent-hearth with its own state, set up with `setup --nightly "$NIGHTLY"` (a toolchain
#           directory or rustup name): every build on that toolchain, with -Zthreads where it is on
#   nightly0 the same, with -Zthreads off (`setup --threads 0`), sharing its golden and pin
# A variant may appear in SWARM more than once: each is its own round of N agents.
# One agent REPS times, variants interleaved; then N agents at once per variant in SWARM. A turn
# is `cargo clippy --workspace --all-targets`, then `cargo test --workspace --no-run`. Last, the
# check gate on a seeded type error. Everything lives under ~/.cache/agent-hearth-public-bench,
# removed at the end. Linux only (btrfs reflinks, bwrap). Warming is off: a warm after one
# variant's turn would run into the next variant's (`warm.sh` measures warming).
# Usage (in a shell with the repository's toolchain):
#   AH=<agent-hearth binary> REV=<commit> FILES="<files agents edit>" public.sh <git clone>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
: "${FILES[0]:?set FILES to the files the agents edit}"
SRC=$1 REV=${REV:?set REV} N=${N:-10} REPS=${REPS:-3} AH=${AH:-agent-hearth}
VARIANTS=${VARIANTS:-empty copy sync hearth} SWARM=${SWARM:-empty copy sync hearth}
BASE=$HOME/.cache/agent-hearth-public-bench; rm -rf "$BASE"; mkdir -p "$BASE"; cd "$BASE"
export AGENT_HEARTH_STATE_DIR=$BASE/state AGENT_HEARTH_CACHE_DIR=$BASE/cache CARGO_INCREMENTAL=1 \
  AGENT_HEARTH_WARM=off
unset CARGO_TARGET_DIR CI
RESULTS=$BASE/results

# ah <variant> <args…>: agent-hearth with the variant's own state and cache.
ah() {
  local v=$1; shift
  case $v in
    nightly | nightly0)
      AGENT_HEARTH_STATE_DIR=$BASE/state-$v AGENT_HEARTH_CACHE_DIR=$BASE/cache-nightly "$AH" "$@" ;;
    *) "$AH" "$@" ;;
  esac
}
hearth() { case $1 in hearth | nightly | nightly0) return 0 ;; *) return 1 ;; esac; }
# mk <variant> <name>: a new jj workspace at main, prepared for the variant; prints its path.
mk() {
  if hearth "$1"; then (cd repo && ah "$1" spawn "$2" 2>/dev/null); return; fi
  local w=$BASE/$2
  (cd repo && jj workspace add --quiet --name "$2" -r main "$w" >/dev/null 2>&1)
  case $1 in
    copy) clone_dir repo/target "$w/target" ;;
    sync) clone_dir repo/target "$w/target"; sync_mtimes "$BASE/repo" "$w" ;;
  esac
  echo "$w"
}
# Main's mtime on every tracked file whose content is the same in the worktree.
sync_mtimes() {
  (cd "$2" && git ls-files -z | python3 -c '
import os, sys
main = sys.argv[1]
for f in sys.stdin.buffer.read().split(b"\0"):
    if not f: continue
    a, b = os.path.join(main.encode(), f), f
    try:
        if os.path.isfile(a) and not os.path.islink(b) and open(a, "rb").read() == open(b, "rb").read():
            st = os.stat(a); os.utime(b, ns=(st.st_atime_ns, st.st_mtime_ns))
    except FileNotFoundError: pass' "$1")
}
# turn <variant> <worktree>: one agent turn; prints seconds and exit code.
turn() {
  local s c=(cargo); s=$(now); hearth "$1" && c=(ah "$1" cargo)
  (cd "$2" && "${c[@]}" clippy --workspace --all-targets >/dev/null 2>&1 &&
    "${c[@]}" test --workspace --no-run >/dev/null 2>&1)
  local rc=$?; printf '%.2f %d\n' "$(dt "$s")" "$rc"
}
# target <variant> <worktree>: the worktree's build directory.
target() {
  if ! hearth "$1"; then echo "$2/target"; return; fi
  local state=$BASE/state; [ "$1" != hearth ] && state=$BASE/state-$1
  for t in "$state"/*/targets/*/; do
    grep -q "\"$2\"" "${t%/}/../../worktrees/$(basename "$t").json" 2>/dev/null && echo "${t%/}"
  done
}
# Exclusive bytes of a directory: what it holds that nothing else shares. btrfs counts data only
# once it is on disk, hence the sync.
exclusive() { sync -f "$1" 2>/dev/null; btrfs filesystem du -s "$1" 2>/dev/null | awk 'NR==2 {print $2}'; }
# stats <label>: min, median and max of the numbers on stdin.
stats() { sort -n | awk -v l="$1" '{a[NR]=$1} END {printf "%s median %.1fs [%.1f-%.1f], n=%d\n", l, a[int((NR+1)/2)], a[1], a[NR], NR}'; }

echo "agent-hearth: $("$AH" --version), $(cargo --version), $(host), load $(load), $(date -I)"
git -c advice.detachedHead=false clone -q "$SRC" repo && (cd repo && git checkout -q -B main "$REV" && jj git init --colocate >/dev/null 2>&1)
echo "repository: $(git -C "$SRC" remote get-url origin 2>/dev/null || echo "$SRC") at $(cd repo && git rev-parse --short=12 HEAD)"
s=$(now); (cd repo && cargo clippy --workspace --all-targets >/dev/null 2>&1 &&
  cargo test --workspace --no-run >/dev/null 2>&1); rc=$?
printf 'warm build of main at its own path (what copy and sync copy): %.1fs rc=%d, load %s\n' "$(dt "$s")" $rc "$(load)"
s=$(now); (cd repo && "$AH" golden refresh >/dev/null 2>&1); rc=$?
printf 'golden refresh: %.1fs rc=%d, load %s\n' "$(dt "$s")" $rc "$(load)"
s=$(now); wait_golden_settle
printf "golden's test suite, in the background after it: %.1fs; failures on main: %s\n" "$(dt "$s")" \
  "$(cat "$BASE"/state/*/goldens/*/tests.json 2>/dev/null | jq -c 'length' | paste -sd, -)"
g=$(echo "$BASE"/state/*/goldens); g=$g/$(cat "$g/current")
echo "golden $(basename "$g"): exclusive $(exclusive "$g"), apparent $(du -sh --apparent-size "$g" | cut -f1)"
(cd repo && "$AH" doctor | grep -E '^(FAIL|ok  )' | sed 's/^/  /')
case " $VARIANTS $SWARM " in *" nightly "*)
  s=$(now); out=$(cd repo && ah nightly setup --claude --nightly "${NIGHTLY:?set NIGHTLY for the nightly variant}" 2>&1)
  rc=$?; printf 'setup --nightly (a golden and main'"'"'s suite on each toolchain): %.1fs rc=%d\n' "$(dt "$s")" $rc
  printf '%s\n' "$out" | grep -E '^(testing|toolchain|          )' | sed 's/^/  /'
  (cd repo && ah nightly doctor | grep -E '^toolchain' | sed 's/^/  /') ;;
esac
case " $VARIANTS $SWARM " in *" nightly0 "*)
  clone_dir "$BASE/state-nightly" "$BASE/state-nightly0"
  (cd repo && ah nightly0 setup --claude --no-golden --threads 0 2>&1 | grep -E '^toolchain' | sed 's/^/  /') ;;
esac

echo "=== one agent: a new worktree's first and second edit, $REPS rounds, variants interleaved"
for r in $(seq 1 "$REPS"); do
  for v in $VARIANTS; do
    s=$(now); W=$(mk "$v" "$v-$r"); setup=$(dt "$s")
    edit "$W" 1 1; first=$(turn "$v" "$W"); edit "$W" 1 2; second=$(turn "$v" "$W")
    echo "$v $first $second" >> "$RESULTS.one"
    target=$(target "$v" "$W")
    printf '%-6s round %d: setup %5.2fs  first %s  second %s  exclusive %s  load %s\n' \
      "$v" "$r" "$setup" "${first% *}s rc=${first#* }" "${second% *}s rc=${second#* }" "$(exclusive "$target")" "$(load)"
    rm -rf "$W" "$target"  # disk: each edited build directory holds gigabytes of its own
  done
done
for v in $VARIANTS; do
  awk -v v="$v" '$1==v {print $2}' "$RESULTS.one" | stats "$v first edit: "
  awk -v v="$v" '$1==v {print $4}' "$RESULTS.one" | stats "$v second edit:"
done

echo "=== $N agents at once, each in a new worktree with its own first edit"
k=0
for v in $SWARM; do
  k=$((k + 1))
  for i in $(seq 1 "$N"); do W[i]=$(mk "$v" "$v-s$k-$i"); edit "${W[i]}" "$i" 1; done
  s=$(now); for i in $(seq 1 "$N"); do turn "$v" "${W[i]}" > "$BASE/r.$i" & done; wait
  wall=$(dt "$s")
  cat "$BASE"/r.* | awk '{print $1}' | stats "$v $N at once, per agent:" | tr -d '\n'
  printf ', wall %.1fs, failed %d, load %s\n' "$wall" "$(cat "$BASE"/r.* | awk '$2 != 0' | wc -l)" "$(load)"
  for i in $(seq 1 "$N"); do rm -rf "$(target "$v" "${W[i]}")" "${W[i]}"; done
  rm -f "$BASE"/r.*
done

echo "=== check gate: a type error appended to ${FILES[0]}"
W=$(mk hearth gate) && cd "$W"
ms() { local s rc; s=$(now); "$@" >/dev/null 2>&1; rc=$?; printf '%.0f ms rc=%d' "$(awk -v a="$(now)" -v b="$s" 'BEGIN {print (a-b)*1000}')" "$rc"; }
echo "first check, clean: $(ms "$AH" check)"
for i in 1 2 3; do echo "check again, unchanged: $(ms "$AH" check)"; done
printf '\npub fn bench_broken() -> u32 { "not a number" }\n' >> "${FILES[0]}"
out=$("$AH" check 2>/dev/null); rc=$?
printf '%s\n' "$out" | sed 's/^/  /'
echo "exit $rc, $(printf '%s\n' "$out" | wc -l) line(s), $(printf '%s' "$out" | wc -c) bytes"
echo "raw clippy --workspace --all-targets --message-format=json: $("$AH" cargo clippy --workspace --all-targets --message-format=json 2>/dev/null | wc -c) bytes"
cd /; rm -rf "$BASE"
