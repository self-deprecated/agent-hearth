#!/usr/bin/env bash
# Can a worktree's build directory share files with the golden's by hardlinks, where the
# filesystem has no reflinks (ext4)? Only if cargo and rustc never write a file of a warm build
# directory in place. On a public repository at a pinned commit: a clone built warm into G as a
# golden is (`clippy --workspace --all-targets`, `test --workspace --no-run`); then, for each
# scenario, a copy of G (a reflink clone, so G stays as built) and W, that copy with every file
# hardlinked (`cp -al`); the scenario builds into W under strace. Scenarios (SCENARIOS, all by
# default): an edit of FILE (`edit`); `cargo update -p $DEP --precise $PRECISE` (`update`);
# RUSTFLAGS changed (`flags`); BUILD_RS touched (`build-rs`); each on a fresh copy. Reported per
# scenario (bench/ext4-writes.py): how the build reached the inodes W shared with the copy,
# following each through renames and links (opened for writing, written, mapped shared and
# writable, truncated, renamed aside or over, unlinked, times or modes set), by kind of file, and
# which of the copy's files then differ from G in content or mtime: those a hardlinked worktree
# would have changed in the golden.
# LINKER: the linker every build uses, golden's included: rustc's default (rust-lld on x86_64
# Linux), `mold` or `wild` (clang --ld-path; clang must be on PATH too). A program's
# .comment section is printed to show which linked it.
# Everything lives under $BENCH_DIR/agent-hearth-ext4-writes (BENCH_DIR: ~/.cache; a filesystem
# with reflinks), removed at the end; the run stops when the disk has less than 15 GB free.
# Usage (in a shell with the repository's toolchain, strace, b3sum, readelf and python3):
#   [LINKER=mold|wild] [SCENARIOS="edit build-rs"] REV=<commit> FILE=<file> DEP=<package> PRECISE=<version> BUILD_RS=<build.rs> ext4-writes.sh <git clone>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
HERE=$(cd "$(dirname "$0")" && pwd)
SRC=$1 REV=${REV:?set REV} FILE=${FILE:?set FILE} DEP=${DEP:?set DEP} PRECISE=${PRECISE:?set PRECISE}
BUILD_RS=${BUILD_RS:?set BUILD_RS} FLAGS=${FLAGS:---cfg agent_hearth_probe}
SCENARIOS=${SCENARIOS:-edit update flags build-rs} LINKER=${LINKER:-}
case $LINKER in
  "") LINK_FLAGS="" ;;
  # Through clang: Nix's gcc wrapper ignores -fuse-ld=mold, and rustc's own LLD linked instead
  # (seen in the .comment line of a first attempt on ruff).
  mold | wild) LINK_FLAGS="-C linker=clang -C link-arg=--ld-path=$(command -v "$LINKER")" ;;
  *) echo "LINKER: rustc's default, mold or wild"; exit 2 ;;
esac
BASE=${BENCH_DIR:-$HOME/.cache}/agent-hearth-ext4-writes; rm -rf "$BASE"; mkdir -p "$BASE"; cd "$BASE"
trap 'cd /; rm -rf "$BASE"' EXIT
export CARGO_INCREMENTAL=1
unset CARGO_TARGET_DIR CI RUSTFLAGS
[ -n "$LINK_FLAGS" ] && export RUSTFLAGS="$LINK_FLAGS"

guard() {
  local free; free=$(df --output=avail -BG "$BASE" | tail -1 | tr -dc 0-9)
  [ "$free" -ge 15 ] || { echo "stopped: $free GB free"; exit 1; }
}
# builds <target> [strace log]: the golden's two builds of the clone into <target>, each traced
# into <log>.clippy and <log>.test when a log is given.
builds() {
  local rc1 rc2
  traced() {
    local log=$1; shift
    if [ -n "$log" ]; then
      strace -f -qq -ttt -y -s 0 -o "$log" -e trace=open,openat,creat,truncate,ftruncate,fallocate,rename,renameat,renameat2,link,linkat,unlink,unlinkat,utimensat,utimes,futimesat,chmod,fchmod,fchmodat,write,pwrite64,writev,pwritev,pwritev2,mmap,copy_file_range,sendfile "$@"
    else
      "$@"
    fi
  }
  cd "$BASE/src" || return
  CARGO_TARGET_DIR=$1 traced "${2:+$2.clippy}" cargo clippy --workspace --all-targets --keep-going --quiet >/dev/null 2>&1; rc1=$?
  CARGO_TARGET_DIR=$1 traced "${2:+$2.test}" cargo test --workspace --no-run --no-fail-fast --quiet >/dev/null 2>&1; rc2=$?
  cd "$BASE" || return
  echo "$rc1 $rc2"
}
# manifest <dir>: path, inode, mtime, size and blake3 of every file under it, tab-separated.
manifest() {
  (cd "$1" && find . -type f -printf '%P\t%i\t%T@\t%s\n' | sort > "$BASE/m.stat"
   find . -type f -printf '%P\0' | xargs -0 -r b3sum -- | sed 's/  /\t/' | awk -F'\t' '{print $2 "\t" $1}' | sort > "$BASE/m.hash")
  join -t $'\t' "$BASE/m.stat" "$BASE/m.hash"
}

echo "agent-hearth ext4-writes: $(cargo --version), $(rustc --version), $(strace -V | head -1), $(host), load $(load), $(date -I)"
if [ -n "$LINKER" ]; then echo "linker: $LINKER (RUSTFLAGS=$LINK_FLAGS)"; else echo "linker: rustc's default"; fi
[ -n "$LINKER" ] && echo "  $("$LINKER" --version | head -1)"
[ -n "${NOTE:-}" ] && echo "note: $NOTE"
git -c advice.detachedHead=false clone -q "$SRC" src && git -C src checkout -q -B main "$REV"
echo "repository: $(git -C "$SRC" remote get-url origin 2>/dev/null || echo "$SRC") at $(git -C src rev-parse --short=12 HEAD)"
s=$(now); rc=$(builds "$BASE/G"); printf 'golden builds: %.1fs rc=%s, %s files, load %s\n' "$(dt "$s")" "$rc" "$(find G -type f | wc -l)" "$(load)"
manifest "$BASE/G" > "$BASE/G.manifest"
program=$(find "$BASE/G/debug/deps" -maxdepth 1 -type f -perm -u+x ! -name '*.so' | head -1)
echo "linked by (.comment of $(basename "$program")): $(readelf -p .comment "$program" 2>/dev/null | grep -oiE '(mold|wild|LLD|GNU ld|gold)[^]]*' | sort -u | paste -sd ';')"
cp src/Cargo.lock "$BASE/Cargo.lock.orig"

for scenario in $SCENARIOS; do
  guard
  git -C src checkout -q -- . && cp "$BASE/Cargo.lock.orig" src/Cargo.lock
  rm -rf "$BASE/C" "$BASE/W"; cp -a --reflink=always G C && cp -al C W
  (cd W && find . -type f -printf '%P\t%i\n' | sort) > "$BASE/W.before"
  case $scenario in
    edit) printf '\n#[allow(dead_code)]\nfn ext4_writes_edit() -> u32 { 1 }\n' >> "src/$FILE" ;;
    update) (cd src && cargo update --quiet -p "$DEP" --precise "$PRECISE") || { echo "$scenario: cargo update failed"; continue; } ;;
    flags) export RUSTFLAGS="$LINK_FLAGS $FLAGS" ;;
    build-rs) touch "src/$BUILD_RS" ;;
  esac
  s=$(now); rc=$(builds "$BASE/W" "$BASE/trace.$scenario"); secs=$(dt "$s")
  if [ -n "$LINK_FLAGS" ]; then export RUSTFLAGS="$LINK_FLAGS"; else unset RUSTFLAGS; fi
  manifest "$BASE/C" > "$BASE/C.manifest"
  printf '\n== %s: builds under strace %.1fs rc=%s, load %s\n' "$scenario" "$secs" "$rc" "$(load)"
  python3 -I "$HERE/ext4-writes.py" "$BASE/W" "$BASE/W.before" "$BASE/G.manifest" "$BASE/C.manifest" "$BASE/trace.$scenario.clippy" "$BASE/trace.$scenario.test"
  rm -f "$BASE"/trace.*
done
