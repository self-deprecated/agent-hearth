#!/usr/bin/env bash
# The other way to seed without reflinks: a worktree's build directory as an overlay, the golden's
# as its lower layer, under bwrap. Does cargo run there (its rustc probe, a build script, a test
# binary), and does the golden stay as built? The lower layer in BENCH_DIR, upper and work in
# EXT4_DIR (another filesystem); then all in BENCH_DIR; then with the flags the lab used
# (`--bind / /` rather than `--dev-bind / /`), where Cargo's rustc probe failed with "permission
# denied". Removed at the end.
# Usage (in a shell with cargo and bwrap): BENCH_DIR=<dir> EXT4_DIR=<dir> overlay-probe.sh
set -u
X=${BENCH_DIR:?set BENCH_DIR}/agent-hearth-overlay-probe E=${EXT4_DIR:?set EXT4_DIR}/agent-hearth-overlay-probe
# overlayfs leaves work/work without permissions.
clean() { chmod -R u+rwx "$@" 2>/dev/null; rm -rf "$@"; }
clean $X $E; mkdir -p $X/src/src $E
cd $X/src
printf '[package]\nname = "p"\nversion = "0.1.0"\nedition = "2024"\n' > Cargo.toml
printf 'fn main() {\n    println!("cargo::rerun-if-changed=build.rs");\n}\n' > build.rs
printf 'pub fn one() -> u32 {\n    1\n}\n\n#[test]\nfn one_is_one() {\n    assert_eq!(one(), 1);\n}\n' > src/lib.rs
cargo generate-lockfile -q
CARGO_TARGET_DIR=$X/golden cargo test -q >/dev/null 2>&1; echo "golden: exit $?"
state() { (cd $X/golden && find . -type f -printf '%P %T@ %s\n' | sort | md5sum | cut -c1-12); }
before=$(state)
try() {  # try <label> <upper/work base> <bind flag>
  local b=$2; clean $b/upper $b/work $X/mnt; mkdir -p $b/upper $b/work $X/mnt
  printf '\n// %s\n' "$1" >> src/lib.rs
  out=$(bwrap $3 / / --overlay-src $X/golden --overlay $b/upper $b/work $X/mnt --chdir $X/src -- \
    env CARGO_TARGET_DIR=$X/mnt CARGO_INCREMENTAL=1 sh -c 'rustc -vV >/dev/null && echo "rustc -vV ok"; cargo test 2>&1 | grep -E "Compiling|Running|test result|error|denied|Caused" ' 2>&1)
  echo "== $1 ($3, upper on $(findmnt -no FSTYPE -T $b)): $(echo "$out" | tr '\n' ';')"
  echo "   upper: $(find $b/upper -type f | wc -l) files; golden's files, mtimes and sizes as built: $([ "$(state)" = "$before" ] && echo yes || echo NO)"
}
try "split filesystems" $E --dev-bind
try "one filesystem" $X --dev-bind
try "the lab's --bind" $E --bind
clean $X $E
