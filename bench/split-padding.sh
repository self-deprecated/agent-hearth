#!/usr/bin/env bash
# Would padding keep line positions if `setup --split-tests` moved a tests module that is not a
# file's last item? A crate whose lib.rs has a tests module, then a function that reports its own
# line; the module moved to tests.rs and its lines replaced by `mod tests;` and blank lines, so
# the function keeps its line. Then: does `cargo fmt --check` pass, and where is the function
# after `cargo fmt`? Under $D, removed at the end.
set -u
D=${BENCH_DIR:-$HOME/.cache}/agent-hearth-split-padding; rm -rf "$D"; mkdir -p "$D/src"; cd "$D" || exit 1
printf '[package]\nname = "p"\nversion = "0.1.0"\nedition = "2024"\n' > Cargo.toml
printf 'pub fn a() -> u32 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn one() {\n        assert_eq!(a(), 1);\n    }\n}\n\npub fn b() -> u32 {\n    line!()\n}\n' > before.rs
printf 'pub fn a() -> u32 {\n    1\n}\n\n#[cfg(test)]\nmod tests;\n\n\n\n\n\n\n\n\npub fn b() -> u32 {\n    line!()\n}\n' > src/lib.rs
printf 'use super::*;\n\n#[test]\nfn one() {\n    assert_eq!(a(), 1);\n}\n' > src/tests.rs
echo "b's line!() before the move: line $(grep -n 'line!()' before.rs | cut -d: -f1); padded: line $(grep -n 'line!()' src/lib.rs | cut -d: -f1)"
cargo fmt --check >/dev/null 2>&1; echo "cargo fmt --check on the padded file: exit $?"
cargo fmt; echo "after cargo fmt: line $(grep -n 'line!()' src/lib.rs | cut -d: -f1)"
cd /; rm -rf "$D"
