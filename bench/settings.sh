#!/usr/bin/env bash
# Build settings Agent Hearth could apply to every build (the golden's, the worktree's, warm's)
# without touching the repository: what do they do to a warm worktree's later turns? Each variant
# gets its own clone and target, warmed first, then REPS turns, variants interleaved. A turn
# edits a file (alternately the two in FILES: say, a leaf crate and one most crates depend on),
# then times `cargo test --workspace --no-run` (the build, every test binary linked) and
# `cargo test --workspace $TESTS` (running the tests, already built; by default the unit tests,
# `--lib --bins`, since end-to-end tests may need shells or services and time out instead). Variants, as `cargo --config` settings:
#   base   the repository's own settings
#   lines  workspace crates with line tables only, no full debuginfo
#   deps   dependencies optimised (opt-level 1) and without debuginfo; built once, in the golden
#   both   lines and deps
# Everything lives under ~/.cache/agent-hearth-settings-bench, removed at the end.
# Usage (in a shell with the repository's toolchain):
#   REV=<commit> FILES="<leaf file> <core file>" settings.sh <git clone>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
: "${FILES[0]:?set FILES to the files the agents edit}"
SRC=$1 REV=${REV:?set REV} REPS=${REPS:-5} TESTS=${TESTS:---lib --bins} VARIANTS=${VARIANTS:-base lines deps both}
BASE=$HOME/.cache/agent-hearth-settings-bench; rm -rf "$BASE"; mkdir -p "$BASE"; cd "$BASE"
unset CARGO_TARGET_DIR CI; export CARGO_INCREMENTAL=1

# config <variant>: its `cargo --config` arguments.
config() {
  local lines=(--config 'profile.dev.debug="line-tables-only"')
  local deps=(--config 'profile.dev.package."*".opt-level=1' --config 'profile.dev.package."*".debug=false')
  case $1 in
    base) ;;
    lines) printf '%s\n' "${lines[@]}" ;;
    deps) printf '%s\n' "${deps[@]}" ;;
    both) printf '%s\n' "${lines[@]}" "${deps[@]}" ;;
  esac
}
# timed <variant> <args…>: cargo in the variant's clone; prints seconds and exit code.
timed() {
  local v=$1 s rc; shift
  local c=(); mapfile -t c < <(config "$v")
  s=$(now); (cd "$BASE/$v" && cargo "${c[@]}" "$@" >/dev/null 2>&1); rc=$?
  printf '%.2f %d\n' "$(dt "$s")" "$rc"
}
size() { du -sb "$BASE/$v/target" | awk '{printf "%.1f", $1 / 1073741824}'; }

echo "$(cargo --version), $(host), load $(load), $(date -I)"
git -c advice.detachedHead=false clone -q "$SRC" "$BASE/src" && git -C "$BASE/src" checkout -q "$REV"
echo "tests run: cargo test --workspace $TESTS"
echo "repository: $(git -C "$SRC" remote get-url origin 2>/dev/null || echo "$SRC") at $(git -C "$BASE/src" rev-parse --short=12 HEAD)"
for v in $VARIANTS; do
  cp -a "$BASE/src" "$BASE/$v"
  b=$(timed "$v" test --workspace --no-run); t=$(timed "$v" test --workspace $TESTS --no-fail-fast)
  printf '%-5s warm build %7.1fs rc=%s, tests %6.1fs rc=%s, target %s GiB, load %s\n' \
    "$v" "${b% *}" "${b#* }" "${t% *}" "${t#* }" "$(size)" "$(load)"
done

: > "$BASE/results"
for r in $(seq 1 "$REPS"); do
  f=${FILES[$(( (r - 1) % ${#FILES[@]} ))]}
  for v in $VARIANTS; do
    printf '\n#[allow(dead_code)]\nfn settings_edit_%s() -> u32 { %s }\n' "$r" "$r" >> "$BASE/$v/$f"
    b=$(timed "$v" test --workspace --no-run); t=$(timed "$v" test --workspace $TESTS --no-fail-fast)
    echo "$v $f ${b% *} ${t% *}" >> "$BASE/results"
    printf '%-5s turn %d (%s): build %6.2fs rc=%s  tests %6.2fs rc=%s  load %s\n' \
      "$v" "$r" "$f" "${b% *}" "${b#* }" "${t% *}" "${t#* }" "$(load)"
  done
done
med() { sort -n | awk '{a[NR]=$1} END {printf "%.1fs [%.1f-%.1f]", a[int((NR+1)/2)], a[1], a[NR]}'; }
for v in $VARIANTS; do
  for f in "${FILES[@]}"; do
    printf '%-5s %s: build %s, tests %s\n' "$v" "$f" \
      "$(awk -v v="$v" -v f="$f" '$1==v && $2==f {print $3}' "$BASE/results" | med)" \
      "$(awk -v v="$v" -v f="$f" '$1==v && $2==f {print $4}' "$BASE/results" | med)"
  done
done
cd /; rm -rf "$BASE"
