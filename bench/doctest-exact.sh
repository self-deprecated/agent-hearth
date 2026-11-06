#!/usr/bin/env bash
# Why a failing doctest's retry by name ran nothing: its name as listed, then run with and
# without `--exact`, in a one-crate workspace of each edition. Under $D, removed at the end.
set -u
D=${BENCH_DIR:-$HOME/.cache}/agent-hearth-doctest-exact; rm -rf "$D"; mkdir -p "$D"; cd "$D" || exit 1
for ed in 2021 2024; do
  mkdir -p w$ed/a/src
  printf '[workspace]\nmembers = ["a"]\nresolver = "2"\n' > w$ed/Cargo.toml
  printf '[package]\nname = "a"\nversion = "0.1.0"\nedition = "%s"\n' $ed > w$ed/a/Cargo.toml
  printf '/// One.\n///\n/// ```\n/// assert_eq!(a::one(), 2);\n/// ```\npub fn one() -> u32 {\n    1\n}\n' > w$ed/a/src/lib.rs
  cd w$ed
  echo "=== edition $ed: full run"; cargo test --workspace --doc 2>&1 | grep -E "^test |test result|Doc-tests"
  name=$(cargo test --workspace --doc 2>&1 | sed -n 's/^test \(.*\) \.\.\. FAILED$/\1/p' | head -1)
  echo "name: [$name]"
  echo "--- --exact"; cargo test --workspace --doc -- --exact "$name" 2>&1 | grep -E "^test |test result|running"
  echo "--- no --exact"; cargo test --workspace --doc -- "$name" 2>&1 | grep -E "^test |test result|running"
  echo "--- --list"; cargo test --workspace --doc -- --list 2>&1 | grep -v "^ *$" | head -5
  cd ..
done
cd /; rm -rf "$D"
