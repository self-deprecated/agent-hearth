#!/usr/bin/env bash
# Whether `doctor`'s hints on a workspace's structure are right. On a public repository at a
# pinned commit: a fresh golden (its test build runs with `--timings`), then `agent-hearth doctor`'s
# `hint  build` lines, then each checked against cargo's own view of the dependency graph rather
# than the timings: a crate said to compile with several feature sets must show those sets in
# `cargo tree -e features`; a build script said to hold up a crate must have that crate depend on
# it (`cargo tree -i`); a crate said to hold up the build is shown with its direct dependents.
# No speed claim: none of the hinted changes is made.
# Everything lives under $BENCH_DIR/agent-hearth-structure-bench, removed at the end.
# Usage (in a shell with the repository's toolchain):
#   AH=<agent-hearth binary> REV=<commit> structure.sh <git clone>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
SRC=$1 REV=${REV:?set REV} AH=${AH:-agent-hearth}
BASE=${BENCH_DIR:-$HOME/.cache}/agent-hearth-structure-bench; rm -rf "$BASE"; mkdir -p "$BASE"; cd "$BASE"
trap 'stop_golden_settle; cd /; rm -rf "$BASE"' EXIT
export AGENT_HEARTH_STATE_DIR=$BASE/state AGENT_HEARTH_CACHE_DIR=$BASE/cache CARGO_INCREMENTAL=1 AGENT_HEARTH_WARM=off
unset CARGO_TARGET_DIR CI
stop_golden_settle() {
  local p; for p in $(pgrep -f 'agent-hearth golden settl[e]'); do kill -- "-$(ps -o pgid= "$p" | tr -d ' ')" 2>/dev/null; done
}

echo "agent-hearth: $("$AH" --version), $(cargo --version), $(host), load $(load), $(date -I)"
[ -n "${NOTE:-}" ] && echo "note: $NOTE"
git -c advice.detachedHead=false clone -q "$SRC" repo && (cd repo && git checkout -q -B main "$REV")
echo "repository: $(git -C "$SRC" remote get-url origin 2>/dev/null || echo "$SRC") at $(cd repo && git rev-parse --short=12 HEAD)"
s=$(now); (cd repo && "$AH" golden refresh >/dev/null 2>&1); rc=$?
printf 'fresh golden: %.1fs rc=%d, load %s\n' "$(dt "$s")" $rc "$(load)"
stop_golden_settle
(cd repo && "$AH" doctor) | grep '^hint  build' > "$BASE/hints"
# The report itself, read with jq rather than Agent Hearth: what the hints are drawn from.
report=$(ls "$BASE"/state/*/goldens/*/timings.html 2>/dev/null | head -1)
if [ -n "$report" ]; then
  awk '/^const UNIT_DATA = \[/{f=1; sub(/^const UNIT_DATA = /, "")} f{print} f&&/^\];/{exit}' "$report" | sed '$ s/;$//' > "$BASE/units.json"
  echo "=== the report ($(jq length "$BASE/units.json") units, $(jq '[.[] | .start + .duration] | max | floor' "$BASE/units.json")s)"
  echo "  longest build-script runs: $(jq -r '[.[] | select(.mode == "run-custom-build")] | sort_by(-.duration) | .[:4] | map("\(.name) \(.duration)s from \(.start)s") | join(", ")' "$BASE/units.json")"
  echo "  last to finish: $(jq -r 'sort_by(-(.start + .duration)) | .[:3] | map("\(.name)\(.target) \(.start)+\(.duration)s") | join(", ")' "$BASE/units.json")"
  echo "  compiled more than once with other features, extra seconds: $(jq -r '[.[] | select(.mode != "run-custom-build")] | group_by([.name, .version, .mode, .target]) | map(select((map(.features) | unique | length) > 1) | {n: .[0].name, x: ((map(.duration) | add) - (map(.duration) | max))}) | sort_by(-.x) | .[:6] | map("\(.n) \(.x * 10 | floor / 10)") | join(", ")' "$BASE/units.json")"
fi
echo "=== doctor: $(wc -l < "$BASE/hints") structure hints"
sed 's/^/  /' "$BASE/hints"

echo "=== checked against cargo tree"
(cd repo && cargo tree --workspace -e normal,build,dev -e features --prefix none --format '{p}|{f}' 2>/dev/null) |
  sed 's/ (.*)//; s/ (\*)$//' | grep '|' | sort -u > "$BASE/features"
while IFS= read -r hint; do
  hint=${hint#hint  build    }
  case $hint in
    *" compiled "*" times with different features ("*)
      crate=${hint%% *}; rest=${hint#* }; version=${rest%% *}
      sets=$(awk -F'|' -v p="$crate v$version" '$1 == p {print $2}' "$BASE/features" | sort -u | wc -l)
      echo "  $crate $version: cargo tree shows $sets feature sets$( [ "$sets" -ge 2 ] && echo ': right' || echo ': WRONG')" ;;
    *"'s build script ran "*" waited for it"*)
      crate=${hint%%\'s build script*}; waiting=$(sed -n 's/.*and \(.*\) waited for it.*/\1/p' <<< "$hint"); waiting=${waiting%% *}
      if (cd repo && cargo tree --workspace -i "$crate" -e normal,build --prefix none 2>/dev/null) | grep -q "^$waiting v"; then
        echo "  $crate's build script holds up $waiting: cargo tree -i $crate lists $waiting: right"
      else
        echo "  $crate's build script holds up $waiting: cargo tree -i $crate does not list $waiting: WRONG"
      fi
      echo "    built on by: $( (cd repo && cargo tree --workspace -i "$crate" -e normal --prefix none --depth 1 2>/dev/null) | tail -n +2 | awk '{print $1}' | sort -u | paste -sd' ')" ;;
    *" with the machine less than half busy"*)
      crate=${hint%% *}
      n=$( (cd repo && cargo tree --workspace -i "$crate" -e normal,dev --prefix none --depth 1 2>/dev/null) | tail -n +2 | awk '{print $1}' | sort -u | wc -l)
      echo "  $crate on the critical path with the machine idle: $n crates depend on it directly (cargo tree -i)" ;;
    *) echo "  unchecked: $hint" ;;
  esac
done < "$BASE/hints"
echo "crates cargo tree shows with more than one feature set: $(awk -F'|' '{print $1}' "$BASE/features" | uniq -d | wc -l) ($(awk -F'|' '{print $1}' "$BASE/features" | uniq -d | head -12 | paste -sd, -))"
