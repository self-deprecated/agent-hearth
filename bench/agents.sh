#!/usr/bin/env bash
# Experiment 1: real agents instead of lockstep scripts. How much do their builds overlap? N Codex
# agents ($MODEL, GPT-6-Luna by default), or Claude Code agents with CLI=claude (Haiku 4.5 by
# default), each in a worktree spawned by agent-hearth, work on one real task each from $TASKS
# (one per line) with agent-hearth's hooks for that agent; nobody tells them about agent-hearth.
# A cargo shim on PATH logs every cargo run with the agent it belongs to ($AGENT, inherited by
# hooks and background settles), and agents.py reads the overlap off that log. Codex agents run in
# its workspace-write sandbox; Claude Code agents skip permission checks. A disposable jj clone,
# worktrees and state live under ~/.cache/agent-hearth-agents, removed at the end unless KEEP=1.
# BASELINE=1 is the same run without agent-hearth: plain jj workspaces, no golden, no hooks, each
# agent building cold in its worktree's target/. FAIR=1 is the baseline a careful user would set
# up without agent-hearth: main's warm target copied into each workspace (`cp -a --reflink`) and
# a plain Stop hook that runs `cargo clippy --workspace --all-targets -- -D warnings` and
# `cargo test --workspace`, sending the agent back with the output's tail, at most three times. WARM=off keeps agent-hearth but turns its warms
# off. REV picks the commit (default origin/main). Every tool call is logged with its time to
# timeline.log, which agents-timeline.py reads: how long agents leave between an edit and their
# next cargo run, and how long they wait for cargo.
# Usage (inside the project's devenv shell): AH=<binary> TASKS=<file> N=3 agents.sh <src-workspace>
set -uo pipefail
source "$(dirname "$0")/portable.sh"
HERE=$(cd "$(dirname "$0")" && pwd)
SRC=$1 AH=$(realpath "${AH:-$(command -v agent-hearth)}") CLI=${CLI:-codex} N=${N:-3}
[ "${FAIR:-}" = 1 ] && BASELINE=1
if [ "$CLI" = claude ]; then MODEL=${MODEL:-claude-haiku-5-5}; else MODEL=${MODEL:-gpt-6-luna}; fi
TASKS=$(realpath "$TASKS") EFFORT=${EFFORT:-medium}
# SETTLE=background|foreground|auto: the Stop hook's --settle (agent-hearth's default when unset).
STOP_ARGS=${SETTLE:+ --settle $SETTLE}
BASE=$HOME/.cache/agent-hearth-agents; rm -rf "$BASE"; mkdir -p "$BASE/bin"; cd "$BASE"
cp "$AH" "$BASE/agent-hearth"; AH=$BASE/agent-hearth  # rebuilding agent-hearth mid-run changes nothing
export AGENT_HEARTH_STATE_DIR=$BASE/state AGENT_HEARTH_CACHE_DIR=$BASE/cache
unset CARGO_TARGET_DIR

# The shim: one line per cargo run, appended when it ends. cargo's own subcommands (clippy) call
# the real cargo through $CARGO, so they are not logged twice.
REAL_CARGO=$(command -v cargo)
cat > "$BASE/bin/cargo" <<EOF
#!/usr/bin/env bash
t() { if [ -n "\${EPOCHREALTIME:-}" ]; then echo "\$EPOCHREALTIME"; else perl -MTime::HiRes=time -e 'printf "%.6f", time'; fi; }
s=\$(t)
"$REAL_CARGO" "\$@"; rc=\$?
who=\${AGENT:-none}; [ -n "\${AGENT_HEARTH_WARMING:-}" ] && who=\$who-warm
printf '%s %s %s %s %s\n' "\$who" "\$s" "\$(t)" "\$rc" "\$*" >> "$BASE/cargo.log"
exit \$rc
EOF
chmod +x "$BASE/bin/cargo"
export PATH=$BASE/bin:$PATH
[ "${WARM:-}" = off ] && export AGENT_HEARTH_WARM=off
# hook-<event> <agent-hearth hook…>: logs the tool call (agent, time, event, tool, whether it runs
# cargo) to timeline.log, then passes it to the hook, if any.
for event in pre post; do
  cat > "$BASE/bin/hook-$event" <<EOF
#!/usr/bin/env bash
in=\$(cat)
printf '%s %s $event %s\n' "\${AGENT:-none}" "\$EPOCHREALTIME" "\$(printf '%s' "\$in" | jq -r '[.tool_name, ((.tool_input.command // "") | if type == "array" then join(" ") else tostring end | test("(^|[^a-z_-])cargo "))] | @tsv')" >> "$BASE/timeline.log"
[ \$# -gt 0 ] && printf '%s' "\$in" | "\$@"
exit 0
EOF
  chmod +x "$BASE/bin/hook-$event"
done
POST="$BASE/bin/hook-post"; [ "${WARM:-}" = off ] || POST="$POST $AH hook claude-post-tool-use"
CODEX_POST="$BASE/bin/hook-post"; [ "${WARM:-}" = off ] || CODEX_POST="$CODEX_POST $AH hook codex-post-tool-use"

echo "agent-hearth: $("$AH" --version), $($CLI --version), model $MODEL ($EFFORT), settle ${SETTLE:-default}, $(cargo --version), $(host), load $(load), $(date -I)"
git clone -q "$SRC" "$BASE/repo"
(cd "$BASE/repo" && git -c advice.detachedHead=false checkout -q -B main "${REV:-origin/main}" && jj git init --colocate >/dev/null 2>&1)
if [ "${FAIR:-}" = 1 ]; then
  s=$(now); (cd "$BASE/repo" && cargo clippy --workspace --all-targets >/dev/null 2>&1 &&
    cargo test --workspace --no-run >/dev/null 2>&1)
  echo "fair baseline: no agent-hearth; warm build of main to copy: $(dt "$s")s; a plain Stop hook"
elif [ "${BASELINE:-}" = 1 ]; then
  echo "baseline: no agent-hearth"
else
  s=$(now); (cd "$BASE/repo" && "$AH" golden refresh >/dev/null 2>&1) || { echo "golden refresh failed"; exit 2; }
  echo "golden refresh: $(dt "$s")s"
  s=$(now); wait_golden_settle
  echo "golden's test suite, after it: $(dt "$s")s; failures on main: $(jq -c length "$AGENT_HEARTH_STATE_DIR"/*/goldens/*/tests.json 2>/dev/null)"
fi
# The hooks. jj makes each workspace of a colocated repo a git worktree, and Codex reads a git
# worktree's hooks from the main checkout's .codex/, but only when the worktree has a .codex/ of
# its own (codex-cli 0.163). info/exclude keeps both out of the source tree.
echo ".codex/" >> "$BASE/repo/.git/info/exclude"
mkdir -p "$BASE/repo/.codex"
# Claude Code reads its hooks from --settings; --setting-sources project leaves out the user's own.
cat > "$BASE/claude-settings.json" <<EOF
{"hooks": {
  "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "$BASE/bin/hook-pre $AH hook claude-pre-tool-use | tee -a $BASE/pre-tool-use.log"}]}],
  "PostToolUse": [{"matcher": "Edit|Write|MultiEdit|NotebookEdit|Bash", "hooks": [{"type": "command", "command": "$POST"}]}],
  "Stop": [{"hooks": [{"type": "command", "command": "echo \\"\$AGENT \$(date +%s) start\\" >> $BASE/hooks.log; out=\$($AH hook claude-stop$STOP_ARGS 2>>$BASE/stop.err); rc=\$?; printf '%s\\\\n' \\"\$out\\" | tee -a $BASE/stop.log; echo \\"\$AGENT \$(date +%s) exit \$rc\\" >> $BASE/hooks.log", "timeout": 600}]}],
  "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "$AH hook claude-user-prompt-submit"}]}]
}}
EOF
cat > "$BASE/repo/.codex/hooks.json" <<EOF
{"hooks": {
  "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "$BASE/bin/hook-pre $AH hook codex-pre-tool-use | tee -a $BASE/pre-tool-use.log"}]}],
  "PostToolUse": [{"hooks": [{"type": "command", "command": "$CODEX_POST"}]}],
  "Stop": [{"hooks": [{"type": "command", "command": "echo \\"\$AGENT \$(date +%s) start\\" >> $BASE/hooks.log; out=\$($AH hook codex-stop$STOP_ARGS 2>>$BASE/stop.err); rc=\$?; printf '%s\\\\n' \\"\$out\\" | tee -a $BASE/stop.log; echo \\"\$AGENT \$(date +%s) exit \$rc\\" >> $BASE/hooks.log", "timeout": 600}]}],
  "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "$AH hook codex-user-prompt-submit"}]}]
}}
EOF
if [ "${BASELINE:-}" = 1 ]; then
  # The plain Stop hook: clippy with warnings denied, then the full suite, in the agent's
  # worktree; blocks with the output's tail while they fail, at most three times in a row.
  cat > "$BASE/bin/plain-stop" <<'EOF'
#!/usr/bin/env bash
in=$(cat); cwd=$(printf '%s' "$in" | jq -r .cwd); id=$(printf '%s' "$in" | jq -r '.session_id // "none"')
tries=$BASE/plain-stop-$id; cd "$cwd" || exit 0
out=$( { cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace; } 2>&1 ) && { rm -f "$tries"; exit 0; }
n=$(( $(cat "$tries" 2>/dev/null || echo 0) + 1 )); echo "$n" > "$tries"
[ "$n" -gt 3 ] && { rm -f "$tries"; exit 0; }
printf '%s' "$out" | tail -60 | jq -Rs '{decision: "block", reason: ("clippy or the tests failed; fix them, then finish:\n" + .)}'
EOF
  sed -i "s|\$BASE|$BASE|g" "$BASE/bin/plain-stop"; chmod +x "$BASE/bin/plain-stop"
  STOP=""
  [ "${FAIR:-}" = 1 ] && STOP=', "Stop": [{"hooks": [{"type": "command", "command": "echo \"$AGENT $(date +%s) start\" >> '"$BASE"'/hooks.log; out=$('"$BASE"'/bin/plain-stop); rc=$?; printf \"%s\\n\" \"$out\" | tee -a '"$BASE"'/stop.log; echo \"$AGENT $(date +%s) exit $rc\" >> '"$BASE"'/hooks.log", "timeout": 1200}]}]'
  cat > "$BASE/claude-settings.json" <<EOF
{"hooks": {
  "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "$BASE/bin/hook-pre"}]}],
  "PostToolUse": [{"matcher": "Edit|Write|MultiEdit|NotebookEdit|Bash", "hooks": [{"type": "command", "command": "$BASE/bin/hook-post"}]}]$STOP
}}
EOF
  cat > "$BASE/repo/.codex/hooks.json" <<EOF
{"hooks": {
  "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "$BASE/bin/hook-pre"}]}],
  "PostToolUse": [{"hooks": [{"type": "command", "command": "$BASE/bin/hook-post"}]}]$STOP
}}
EOF
fi
mapfile -t TASK < "$TASKS"
for i in $(seq 1 "$N"); do
  if [ "${BASELINE:-}" = 1 ]; then
    (cd "$BASE/repo" && jj workspace add --name "a$i" "../a$i" >/dev/null 2>&1); W[i]=$BASE/a$i
    [ "${FAIR:-}" = 1 ] && clone_dir "$BASE/repo/target" "${W[i]}/target"
  else
    W[i]=$(cd "$BASE/repo" && "$AH" spawn "a$i" 2>/dev/null | tail -1)
  fi
  [ -d "${W[i]}" ] || { echo "spawn a$i failed"; exit 2; }
  mkdir "${W[i]}/.codex"
done
: > "$BASE/cargo.log"

echo "=== $N agents"
s=$(now)
for i in $(seq 1 "$N"); do
  task=${TASK[$(( (i - 1) % ${#TASK[@]} ))]}
  echo "a$i: $task"
  ( a=$(now)
    cd "${W[i]}" || exit
    if [ "$CLI" = claude ]; then
      AGENT=a$i claude -p "$task" --model "$MODEL" --effort "$EFFORT" --settings "$BASE/claude-settings.json" \
        --setting-sources project --strict-mcp-config --no-session-persistence \
        --dangerously-skip-permissions --add-dir "$BASE" --add-dir "$HOME/.cargo" \
        --output-format stream-json --verbose < /dev/null > "$BASE/a$i.jsonl" 2> "$BASE/a$i.err"
      rc=$?
      jq -R -r 'fromjson? | select(.type == "result") | .result // ""' "$BASE/a$i.jsonl" > "$BASE/a$i.last"
    else
      # Not --ignore-user-config: it drops the project's hooks too.
      AGENT=a$i codex exec -m "$MODEL" -c model_reasoning_effort="$EFFORT" --ephemeral \
        --dangerously-bypass-hook-trust -s workspace-write --add-dir "$BASE" --add-dir "$HOME/.cargo" \
        --json -o "$BASE/a$i.last" "$task" < /dev/null > "$BASE/a$i.jsonl" 2> "$BASE/a$i.err"
      rc=$?
    fi
    echo "a$i $a $(now) $rc" >> "$BASE/agents.log" ) &
done
wait
# Background settles the Stop hook started are builds too: wait for them. jq reads the whole
# output (grep -q would stop early, and pipefail would take doctor's SIGPIPE for "done").
while (cd "$BASE/repo" && { "$AH" doctor --json || true; }) | jq -e 'any(.worktrees[]; .settling)' > /dev/null; do sleep 5; done
echo "all agents done: $(dt "$s")s"

python3 "$HERE/agents.py" "$BASE/agents.log" "$BASE/cargo.log"
echo "=== timeline: edits, cargo runs and the gaps between them"
python3 -I "$HERE/agents-timeline.py" "$BASE/timeline.log"
echo "=== per agent: final check, settle, files changed, tokens, last message"
for i in $(seq 1 "$N"); do
  if [ "${BASELINE:-}" = 1 ]; then
    c=$(cd "${W[i]}" && cargo clippy --workspace --all-targets --quiet -- -D warnings >/dev/null 2>&1 && echo "clippy clean" || echo "clippy FAILED")
    # What a settle would have found: the full suite on the code the agent left.
    c="$c, $(cd "${W[i]}" && cargo test --workspace --quiet >/dev/null 2>&1 && echo "tests pass" || echo "tests FAILED")"
  else
    c=$(cd "${W[i]}" && "$AH" check 2>/dev/null | tail -1)
  fi
  files=$(cd "${W[i]}" && jj diff --stat -r @ 2>/dev/null | tail -1)
  if [ "$CLI" = claude ]; then
    tokens=$(jq -R -r 'fromjson? | select(.type == "result") | "\(.usage.input_tokens + .usage.cache_read_input_tokens + .usage.cache_creation_input_tokens + .usage.output_tokens), $\(.total_cost_usd), \(.num_turns) turns"' "$BASE/a$i.jsonl")
  else
    tokens=$(grep '"turn.completed"' "$BASE/a$i.jsonl" | jq -s '[.[].usage | (.input_tokens // 0) + (.output_tokens // 0)] | add // 0')
  fi
  settle=$(cd "${W[i]}" && { "$AH" doctor --json 2>/dev/null || true; } | jq -r '.worktrees[0].settle // "none"')
  echo "a$i: check: ${c:-?}; settle: $settle; $files; tokens $tokens; $(head -c 300 "$BASE/a$i.last" 2>/dev/null | tr '\n' ' ')"
done
echo "=== hooks"
echo "cargo denied (agent-hearth cargo named instead): $(cat "$BASE/pre-tool-use.log" 2>/dev/null | grep -c '"deny"')"
echo "cargo rewritten to agent-hearth cargo: $(cat "$BASE/pre-tool-use.log" 2>/dev/null | grep -c '"updatedInput"')"
echo "Stop blocks: $(cat "$BASE/stop.log" 2>/dev/null | grep -c '"block"')"
echo "Stop hook calls (agent, time, start or exit code):"; sed 's/^/  /' "$BASE/hooks.log" 2>/dev/null
[ -s "$BASE/stop.err" ] && { echo "Stop hook errors:"; sed 's/^/  /' "$BASE/stop.err" | cut -c1-300; }
grep -h -o '"reason":"[^"]*' "$BASE/stop.log" 2>/dev/null | cut -c11- | cut -c1-200 | sed 's/^/  block: /'
echo "=== the agents' cargo commands"
for i in $(seq 1 "$N"); do
  jq -R -r 'fromjson? | (select(.type == "item.completed" and .item.type == "command_execution") | .item.command),
    (select(.type == "assistant") | .message.content[]? | select(.type == "tool_use" and .name == "Bash") | .input.command)' "$BASE/a$i.jsonl" |
    grep -E 'cargo' | cut -c1-160 | sed "s/^/a$i: /"
done
grep -h FAILED "$AGENT_HEARTH_STATE_DIR"/*/settles/*.log 2>/dev/null | sort | uniq -c | sed 's/^/settle failure: /'
echo "=== cargo log: agent, seconds, exit, args"
awk '{printf "%s %.1fs rc=%s", $1, $3 - $2, $4; for (i = 5; i <= NF; i++) printf " %s", $i; print ""}' "$BASE/cargo.log"
[ "${KEEP:-}" = 1 ] || { cd /; rm -rf "$BASE"; }
