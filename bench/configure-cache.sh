#!/usr/bin/env bash
# Autoconf results kept between golden builds (`CONFIG_SITE`): a golden refresh after a lockfile
# bump that reruns native build scripts, with and without the kept results. On a public
# repository at a pinned commit: a fresh golden, which runs every build script once and keeps
# what configure found; then main gets a commit that only bumps DEP to PRECISE in Cargo.lock
# (`cc`, which every native build script builds with, so they all rebuild and rerun), and the
# refresh to it runs REPS times each with `AGENT_HEARTH_CONFIGURE_CACHE=off` and on, alternating,
# the new golden removed and the current one put back after each. Reported: each refresh's time,
# and how many configure checks in the build scripts' output said `(cached)`. The golden's test
# suite, which each refresh starts in the background, is stopped.
# Everything lives under $BENCH_DIR/agent-hearth-configure-bench, removed at the end.
# Usage (in a shell with the repository's toolchain):
#   AH=<agent-hearth binary> REV=<commit> DEP=<package> PRECISE=<version> configure-cache.sh <git clone>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
SRC=$1 REV=${REV:?set REV} DEP=${DEP:?set DEP} PRECISE=${PRECISE:?set PRECISE} REPS=${REPS:-3} AH=${AH:-agent-hearth}
BASE=${BENCH_DIR:-$HOME/.cache}/agent-hearth-configure-bench; rm -rf "$BASE"; mkdir -p "$BASE"; cd "$BASE"
trap 'stop_golden_settle; cd /; rm -rf "$BASE"' EXIT
export AGENT_HEARTH_STATE_DIR=$BASE/state AGENT_HEARTH_CACHE_DIR=$BASE/cache CARGO_INCREMENTAL=1 \
  AGENT_HEARTH_WARM=off
unset CARGO_TARGET_DIR CI AGENT_HEARTH_CONFIGURE_CACHE CONFIG_SITE

guard() {
  local free; free=$(df --output=avail -BG "$BASE" | tail -1 | tr -dc 0-9)
  [ "$free" -ge 15 ] || { echo "stopped: $free GB free"; exit 1; }
}
stop_golden_settle() {
  local p; for p in $(pgrep -f 'agent-hearth golden settl[e]'); do kill -- "-$(ps -o pgid= "$p" | tr -d ' ')" 2>/dev/null; done
  while pgrep -f 'agent-hearth golden settl[e]' >/dev/null; do sleep 1; done
}
# cached <golden dir>: configure checks answered from kept results, of all, in the output of the
# build scripts that ran since the stamp. (Not in a variable named `out`: nix shells export it.)
cached() {
  find "$1"/target/debug/build -name output -newer "$BASE/stamp" -exec cat {} + 2>/dev/null > "$BASE/outputs"
  printf '%s of %s configure checks cached' "$(grep -c '^checking .*(cached)' "$BASE/outputs")" \
    "$(grep -c '^checking ' "$BASE/outputs")"
}

echo "agent-hearth: $("$AH" --version), $(cargo --version), $(host), load $(load), $(date -I)"
[ -n "${NOTE:-}" ] && echo "note: $NOTE"
git -c advice.detachedHead=false clone -q "$SRC" repo && (cd repo && git checkout -q -B main "$REV" && jj git init --colocate >/dev/null 2>&1)
echo "repository: $(git -C "$SRC" remote get-url origin 2>/dev/null || echo "$SRC") at $(cd repo && git rev-parse --short=12 HEAD)"
touch "$BASE/stamp"
s=$(now); (cd repo && "$AH" golden refresh >/dev/null 2>&1); rc=$?; took=$(dt "$s")
stop_golden_settle
GOLDENS=$(echo "$BASE"/state/*/goldens); first=$(cat "$GOLDENS/current")
printf 'fresh golden: %.1fs rc=%d, %s; results kept: %s, load %s\n' "$took" $rc "$(cached "$GOLDENS/$first")" \
  "$(find "$BASE/cache/configure" -name '*.cache' -size +0 2>/dev/null | wc -l) files" "$(load)"
from=$(grep -A1 "^name = \"$DEP\"$" repo/Cargo.lock | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)
(cd repo && cargo update --quiet -p "$DEP" --precise "$PRECISE" && jj commit --quiet -m "bump $DEP" Cargo.lock >/dev/null 2>&1 &&
  jj bookmark set main -r @- >/dev/null 2>&1 && jj git export >/dev/null 2>&1) || { echo "the bump failed"; exit 1; }
echo "main: $DEP $from -> $PRECISE in Cargo.lock ($(cd repo && jj diff --stat -r main 2>/dev/null | tail -1))"

for r in $(seq 1 "$REPS"); do
  order="off on"; [ $((r % 2)) = 0 ] && order="on off"
  for arm in $order; do
    guard; touch "$BASE/stamp"
    s=$(now); (cd repo && AGENT_HEARTH_CONFIGURE_CACHE=$arm "$AH" golden refresh >/dev/null 2>&1); rc=$?; took=$(dt "$s")
    stop_golden_settle
    new=$(cat "$GOLDENS/current")
    printf 'cache %-3s round %d: refresh %6.1fs rc=%d, generation %s, %s, load %s\n' "$arm" "$r" "$took" $rc \
      "$(jq -r '.generation // 0' "$GOLDENS/$new/golden.json")" "$(cached "$GOLDENS/$new")" "$(load)"
    echo "$arm $took" >> "$BASE/results"
    [ "$new" != "$first" ] && rm -rf "${GOLDENS:?}/$new"
    echo "$first" > "$GOLDENS/current"
  done
done
echo
for arm in off on; do
  awk -v a="$arm" '$1 == a {print $2}' "$BASE/results" | sort -n | awk -v a="$arm" '{x[NR]=$1} END {printf "cache %-3s refresh after the bump: median %.1fs [%.1f-%.1f], n=%d\n", a, x[int((NR+1)/2)], x[1], x[NR], NR}'
done
