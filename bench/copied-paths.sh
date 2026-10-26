#!/usr/bin/env bash
# Does a target copied from main's checkout into a new worktree still point at main's checkout
# after the worktree builds? Main builds `clippy --workspace --all-targets` and `test --workspace
# --no-run`; a new jj workspace gets that target copied (`cp -a`, cloning where the filesystem
# can), builds the same, and every file in its target that still names main's checkout is listed:
# what a test binary or build script output would read or run from main instead of the worktree.
# Everything lives under ~/.cache/agent-hearth-copied-bench, removed at the end.
# Usage (in a shell with the repository's toolchain): REV=<commit> copied-paths.sh <git clone>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
SRC=$1 REV=${REV:?set REV}
BASE=$HOME/.cache/agent-hearth-copied-bench; rm -rf "$BASE"; mkdir -p "$BASE"; cd "$BASE"
unset CARGO_TARGET_DIR CI
build() { (cd "$1" && cargo clippy --workspace --all-targets >/dev/null 2>&1 && cargo test --workspace --no-run >/dev/null 2>&1); }

echo "$(cargo --version), $(host), $(date -I)"
git -c advice.detachedHead=false clone -q "$SRC" repo &&
  (cd repo && git checkout -q -B main "$REV" && jj git init --colocate >/dev/null 2>&1)
echo "repository: $(git -C "$SRC" remote get-url origin 2>/dev/null || echo "$SRC") at $(git -C repo rev-parse --short=12 HEAD)"
s=$(now); build repo; printf 'main built: %.1fs rc=%d\n' "$(dt "$s")" $?
(cd repo && jj workspace add --quiet --name copy "$BASE/copy" >/dev/null 2>&1)
clone_dir repo/target copy/target || { echo "copying the target failed"; exit 2; }
s=$(now); (cd copy && cargo test --workspace --no-run 2>&1) > "$BASE/build.txt"; printf 'the copy built: %.1fs, %s compiled\n' "$(dt "$s")" "$(grep -c '^ *Compiling' "$BASE/build.txt")"
grep -rlF "$BASE/repo" copy/target/debug 2>/dev/null | grep -v '/incremental/' | sed "s|$BASE/||" > "$BASE/named.txt"
echo "files in the copy's target naming main's checkout: $(wc -l < "$BASE/named.txt" | tr -d ' ')"
sed -E 's|/[^/]*-[0-9a-f]{16}(/\|\.\|$)|/*\1|' "$BASE/named.txt" | sort | uniq -c | sort -rn | head -25 | sed 's/^/  /'
echo "compiled in the copy: $(grep '^ *Compiling' "$BASE/build.txt" | awk '{print $2}' | paste -sd' ' -)"
# Test executables: what `cargo test` runs. Those that name main's checkout run with its paths.
(cd copy && cargo test --workspace --no-run --message-format=json 2>/dev/null) |
  jq -r 'select(.reason == "compiler-artifact" and .executable != null) | .executable' | sort -u > "$BASE/tests.txt"
n=0; while read -r f; do grep -qF "$BASE/repo" "$f" && { n=$((n + 1)); echo "  ${f#"$BASE/"}"; }; done < "$BASE/tests.txt" > "$BASE/stale.txt"
echo "test executables naming main's checkout: $n of $(wc -l < "$BASE/tests.txt" | tr -d ' ')"
head -15 "$BASE/stale.txt"
cd /; rm -rf "$BASE"
