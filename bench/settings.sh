#!/usr/bin/env bash
# Build settings Agent Hearth could apply to every build (the golden's, the worktree's, warm's)
# without touching the repository: what do they do to a warm worktree's later turns? Each variant
# gets its own clone and target, warmed first, then REPS turns, variants interleaved. A turn
# edits a file (alternately the two in FILES: say, a leaf crate and one most crates depend on),
# then times `cargo test --workspace --no-run` (the build, every test binary linked) and
# `cargo test --workspace $TESTS` (running the tests, already built; by default the unit tests,
# `--lib --bins`, since end-to-end tests may need shells or services and time out instead; it
# can end in `-- --skip <test>`). A cargo run that takes TIMEOUT seconds (1800) is stopped, rc=124.
# Variants, as `cargo --config` settings:
#   base       the repository's own settings
#   lines      workspace crates with line tables only, no full debuginfo
#   deps       dependencies optimised (opt-level 1) and without debuginfo; built once, in the golden
#   both       lines and deps (`setup --build-settings`); the variants below add to it
#   mold       linked by mold (clang --ld-path) instead of Rust's own LLD; needs CLANG and MOLD
#   wild       linked by wild the same way; needs CLANG and WILD
#   nightly    the NIGHTLY toolchain, as it is: the control for the two below
#   cranelift  NIGHTLY, workspace crates compiled by Cranelift, dependencies still by LLVM
#   threads    NIGHTLY, the parallel front end (-Zthreads=$THREADS, by default 8)
# Everything lives under ~/.cache/agent-hearth-settings-bench, removed at the end.
# Usage (in a shell with the repository's toolchain):
#   REV=<commit> FILES="<leaf file> <core file>" settings.sh <git clone>
#   NIGHTLY=<toolchain dir with bin/cargo> CLANG=<clang> MOLD=<mold> WILD=<wild>, for those variants
#   Under Nix, give MOLD and WILD the wrapped linkers (nixpkgs' mold and wild, not wild-unwrapped):
#   the wrapper adds the rpaths the default link gets, without which proc macros that link
#   OpenSSL (sqlx's) cannot load.
set -uo pipefail
source "$(dirname "$0")/portable.sh"
: "${FILES[0]:?set FILES to the files the agents edit}"
SRC=$1 REV=${REV:?set REV} REPS=${REPS:-5} TESTS=${TESTS:---lib --bins} TIMEOUT=${TIMEOUT:-1800} VARIANTS=${VARIANTS:-base lines deps both}
BASE=$HOME/.cache/agent-hearth-settings-bench; rm -rf "$BASE"; mkdir -p "$BASE"; cd "$BASE"
unset CARGO_TARGET_DIR CI; export CARGO_INCREMENTAL=1

THREADS=${THREADS:-8} HOST=$(rustc -vV | sed -n 's/^host: //p')
mkdir -p "$BASE/bin"
for l in mold wild; do
  case " $VARIANTS " in *" $l "*)
    L=${l^^}; : "${!L:?set $L and CLANG for the $l variant}"
    printf '#!/bin/sh\nexec %s --ld-path=%s "$@"\n' "${CLANG:?set CLANG}" "${!L}" > "$BASE/bin/cc-$l"
    chmod +x "$BASE/bin/cc-$l" ;;
  esac
done
nightly() { case $1 in nightly | cranelift | threads) return 0 ;; *) return 1 ;; esac; }

# config <variant>: its cargo arguments before the subcommand (-Z flags and `--config`).
config() {
  local lines=(--config 'profile.dev.debug="line-tables-only"')
  local deps=(--config 'profile.dev.package."*".opt-level=1' --config 'profile.dev.package."*".debug=false')
  case $1 in
    base) ;;
    lines) printf '%s\n' "${lines[@]}" ;;
    deps) printf '%s\n' "${deps[@]}" ;;
    both | nightly) printf '%s\n' "${lines[@]}" "${deps[@]}" ;;
    mold | wild) printf '%s\n' "${lines[@]}" "${deps[@]}" --config "target.$HOST.linker=\"$BASE/bin/cc-$1\"" ;;
    cranelift) printf '%s\n' -Zcodegen-backend "${lines[@]}" "${deps[@]}" \
      --config 'profile.dev.codegen-backend="cranelift"' --config 'profile.dev.package."*".codegen-backend="llvm"' ;;
    threads) printf '%s\n' "${lines[@]}" "${deps[@]}" --config "build.rustflags=[\"-Zthreads=$THREADS\"]" ;;
  esac
}
# timed <variant> <args…>: cargo in the variant's clone; prints seconds and exit code.
timed() {
  local v=$1 s rc path=$PATH; shift
  local c=(); mapfile -t c < <(config "$v")
  if nightly "$v"; then path=${NIGHTLY:?set NIGHTLY for the $v variant}/bin:$PATH; fi
  s=$(now); (cd "$BASE/$v" && PATH=$path timeout "$TIMEOUT" cargo "${c[@]}" "$@" >/dev/null 2>&1); rc=$?
  printf '%.2f %d\n' "$(dt "$s")" "$rc"
}
size() { du -sb "$BASE/$v/target" | awk '{printf "%.1f", $1 / 1073741824}'; }

echo "$(cargo --version), $(host), load $(load), $(date -I)"
if [ -n "${NIGHTLY:-}" ]; then echo "nightly: $("$NIGHTLY/bin/rustc" --version)"; fi
for l in mold wild; do
  L=${l^^}; if [ -x "$BASE/bin/cc-$l" ]; then echo "$l: $("$CLANG" --version | head -1), $("${!L}" --version | head -1)"; fi
done
git -c advice.detachedHead=false clone -q "$SRC" "$BASE/src" && git -C "$BASE/src" checkout -q "$REV"
echo "tests run: cargo test --workspace --no-fail-fast $TESTS, each cargo stopped after ${TIMEOUT}s"
echo "repository: $(git -C "$SRC" remote get-url origin 2>/dev/null || echo "$SRC") at $(git -C "$BASE/src" rev-parse --short=12 HEAD)"
for v in $VARIANTS; do
  cp -a "$BASE/src" "$BASE/$v"
  b=$(timed "$v" test --workspace --no-run); t=$(timed "$v" test --workspace --no-fail-fast $TESTS)
  printf '%-9s warm build %7.1fs rc=%s, tests %6.1fs rc=%s, target %s GiB, load %s\n' \
    "$v" "${b% *}" "${b#* }" "${t% *}" "${t#* }" "$(size)" "$(load)"
done

: > "$BASE/results"
for r in $(seq 1 "$REPS"); do
  f=${FILES[$(( (r - 1) % ${#FILES[@]} ))]}
  for v in $VARIANTS; do
    printf '\n#[allow(dead_code)]\nfn settings_edit_%s() -> u32 { %s }\n' "$r" "$r" >> "$BASE/$v/$f"
    b=$(timed "$v" test --workspace --no-run); t=$(timed "$v" test --workspace --no-fail-fast $TESTS)
    echo "$v $f ${b% *} ${t% *}" >> "$BASE/results"
    printf '%-9s turn %d (%s): build %6.2fs rc=%s  tests %6.2fs rc=%s  load %s\n' \
      "$v" "$r" "$f" "${b% *}" "${b#* }" "${t% *}" "${t#* }" "$(load)"
  done
done
med() { sort -n | awk '{a[NR]=$1} END {printf "%.1fs [%.1f-%.1f]", a[int((NR+1)/2)], a[1], a[NR]}'; }
for v in $VARIANTS; do
  for f in "${FILES[@]}"; do
    printf '%-9s %s: build %s, tests %s\n' "$v" "$f" \
      "$(awk -v v="$v" -v f="$f" '$1==v && $2==f {print $3}' "$BASE/results" | med)" \
      "$(awk -v v="$v" -v f="$f" '$1==v && $2==f {print $4}' "$BASE/results" | med)"
  done
done
cd /; rm -rf "$BASE"
