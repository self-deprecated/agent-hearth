#!/usr/bin/env bash
# Agent Hearth's agent-facing commands inside an agent's sandbox, in a worktree spawned outside
# it: Codex's workspace-write sandbox (`codex sandbox`), or with CLI=claude Claude Code's sandboxed
# Bash tool (a headless session asked to run one script). First with the sandbox's default
# writable paths (the working directory and a temp directory), then with agent-hearth's state
# directory added. sandbox-inner.sh runs the commands and writes one line each: the command, its
# exit code, its time and its last output line. A disposable clone of <src-workspace> and
# agent-hearth's state live under ~/.cache/agent-hearth-sandbox-commands, removed at the end.
# Usage (inside the project's devenv shell): AH=<binary> [CLI=claude] sandbox-commands.sh <src-workspace>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
HERE=$(cd "$(dirname "$0")" && pwd)
SRC=$1 AH=$(realpath "${AH:-$(command -v agent-hearth)}") CLI=${CLI:-codex}
BASE=$HOME/.cache/agent-hearth-sandbox-commands; rm -rf "$BASE"; mkdir -p "$BASE"
export AGENT_HEARTH_STATE_DIR=$BASE/state AGENT_HEARTH_CACHE_DIR=$BASE/cache
unset CARGO_TARGET_DIR
echo "agent-hearth: $("$AH" --version), $($CLI --version), $(cargo --version), $(host), $(date -I)"
git clone -q "$SRC" "$BASE/repo"
(cd "$BASE/repo" && git checkout -q -B main origin/main && jj git init --colocate >/dev/null 2>&1)
echo "golden refresh: $(cd "$BASE/repo" && "$AH" golden refresh 2>&1 | tail -1)"

round() {  # round <name> <extra writable path, or empty>
  local w; w=$(cd "$BASE/repo" && "$AH" spawn "$1" 2>/dev/null | tail -1)
  echo "== $1: writable${2:+ also $2}"
  cd "$w" || return
  local inner=("bash" "$HERE/sandbox-inner.sh" "$AH" "$w/sandbox-results.txt")
  if [ "$CLI" = claude ]; then
    local settings
    settings=$(jq -nc --arg w "$2" '{sandbox: {enabled: true, allowUnsandboxedCommands: false,
      filesystem: {allowWrite: ([$w] | map(select(. != "")))}}}')
    claude -p "Run this exact command with the Bash tool, once, and reply only with done: ${inner[*]}" \
      --model "${MODEL:-claude-haiku-4-5-20251001}" --settings "$settings" --setting-sources project \
      --strict-mcp-config --no-session-persistence --allowedTools Bash > "$BASE/$1.out" 2>&1
  else
    codex sandbox -c sandbox_mode='"workspace-write"' \
      -c "sandbox_workspace_write.writable_roots=$(jq -nc --arg w "$2" '[$w] | map(select(. != ""))')" \
      -- "${inner[@]}" > "$BASE/$1.out" 2>&1
  fi
  cat sandbox-results.txt 2>/dev/null || { echo "  no results:"; tail -5 "$BASE/$1.out"; }
  cd "$BASE"; }
round sandbox-defaults ""
round plus-state-dir "$AGENT_HEARTH_STATE_DIR"
cd /; rm -rf "$BASE"
