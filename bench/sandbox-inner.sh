#!/usr/bin/env bash
# Run by sandbox-commands.sh inside the sandbox: agent-hearth's agent-facing commands, one result
# line each, appended to <results>. Usage: sandbox-inner.sh <agent-hearth> <results>
AH=$1 OUT=$2
source "$(dirname "$0")/portable.sh"
: > "$OUT"
run() {
  local s out rc; s=$(now)
  out=$("$AH" "$@" 2>&1); rc=$?
  printf '  agent-hearth %-22s exit %d, %5.1fs: %s\n' "$*" "$rc" "$(dt "$s")" \
    "$(echo "$out" | grep -v '^ *$' | tail -1 | cut -c1-150)" >> "$OUT"; }
run cargo build
echo '// an edit' >> "$(git ls-files '*.rs' | head -1)"
run check
run cargo test --no-run
run settle
run doctor
