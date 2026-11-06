#!/usr/bin/env bash
# Do tests behave the same compiled by Cranelift as by LLVM? The workspace's unit tests
# (`--lib --bins`), built by the NIGHTLY toolchain three ways: LLVM; Cranelift for workspace crates
# only, dependencies by LLVM (settings.sh's `cranelift` variant); Cranelift for everything. Then,
# REPS times, the tests without those in HANG, listing the ones that fail; then each test in HANG
# alone. Every cargo run is stopped after TIMEOUT seconds (600), a test in HANG after HANG_TIMEOUT
# (120), with rc=124. Everything lives under ~/.cache/agent-hearth-backends-bench, removed at the
# end.
# Usage (in a shell with the repository's build dependencies):
#   NIGHTLY=<toolchain dir with bin/cargo> REV=<commit> HANG="<test name>…" backends.sh <git clone>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
SRC=$1 REV=${REV:?set REV} REPS=${REPS:-3} TIMEOUT=${TIMEOUT:-600} HANG_TIMEOUT=${HANG_TIMEOUT:-120}
read -ra HANG <<<"${HANG:-}"
BASE=$HOME/.cache/agent-hearth-backends-bench; rm -rf "$BASE"; mkdir -p "$BASE"
unset CARGO_TARGET_DIR CI; export CARGO_INCREMENTAL=1 PATH=${NIGHTLY:?set NIGHTLY}/bin:$PATH
git -c advice.detachedHead=false clone -q "$SRC" "$BASE/src" && git -C "$BASE/src" checkout -q "$REV"
cd "$BASE/src" || exit 2
echo "$(rustc --version), $(host), $(date -I)"
echo "repository: $(git -C "$SRC" remote get-url origin 2>/dev/null || echo "$SRC") at $(git rev-parse --short=12 HEAD)"
skips=(); for t in "${HANG[@]}"; do skips+=(--skip "$t"); done

for backend in llvm cranelift cranelift-all; do
  c=(--config 'profile.dev.debug="line-tables-only"' --config 'profile.dev.package."*".opt-level=1'
     --config 'profile.dev.package."*".debug=false')
  case $backend in
    cranelift) c=(-Zcodegen-backend "${c[@]}" --config 'profile.dev.codegen-backend="cranelift"'
                  --config 'profile.dev.package."*".codegen-backend="llvm"') ;;
    cranelift-all) c=(-Zcodegen-backend "${c[@]}" --config 'profile.dev.codegen-backend="cranelift"') ;;
  esac
  export CARGO_TARGET_DIR=$BASE/target-$backend
  s=$(now); timeout "$TIMEOUT" cargo "${c[@]}" test --workspace --lib --bins --no-run >/dev/null 2>&1
  rc=$?; printf '%-13s build  %6.1fs rc=%s\n' "$backend" "$(dt "$s")" "$rc"
  [ "$rc" = 0 ] || continue
  for r in $(seq 1 "$REPS"); do
    s=$(now)
    # `log`, not `out`: Nix shells export `out`, and a long log there overflows every later exec.
    log=$(timeout "$TIMEOUT" cargo "${c[@]}" test --workspace --lib --bins --no-fail-fast -- "${skips[@]}" 2>&1)
    rc=$?
    printf '%-13s run %d  %6.1fs rc=%-3s %s passed, failed: %s\n' "$backend" "$r" "$(dt "$s")" "$rc" \
      "$(grep -oE '^test result: [A-Za-z]+\. [0-9]+ passed' <<<"$log" | awk '{n += $4} END {print n + 0}')" \
      "$(grep -E '^test .* FAILED$' <<<"$log" | awk '{print $2}' | sort | xargs)"
  done
  for t in "${HANG[@]}"; do
    s=$(now); log=$(timeout "$HANG_TIMEOUT" cargo "${c[@]}" test --workspace --lib --bins -- "$t" 2>&1); rc=$?
    printf '%-13s alone  %6.1fs rc=%-3s %s: %s\n' "$backend" "$(dt "$s")" "$rc" "$t" \
      "$(grep -E "^test .*$t" <<<"$log" | sed 's/.* \.\.\. //' | head -1)"
  done
done
cd /; rm -rf "$BASE"
