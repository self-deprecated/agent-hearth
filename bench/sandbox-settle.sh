#!/usr/bin/env bash
# A settle inside Codex's sandbox (workspace-write, no network) of a crate whose one test binds a
# loopback socket: it fails there, says why, and keeps no verdict; the same settle outside passes.
# A disposable crate and agent-hearth's state live under ~/.cache/agent-hearth-sandbox-probe,
# removed at the end.
# Usage (inside agent-hearth's devenv shell): sandbox-settle.sh <agent-hearth binary>
set -u; B=$HOME/.cache/agent-hearth-sandbox-probe; rm -rf $B; mkdir -p $B/repo/src && cd $B/repo
export AGENT_HEARTH_STATE_DIR=$B/state AGENT_HEARTH_CACHE_DIR=$B/cache
AH=$1
printf '[package]\nname = "probe"\nversion = "0.1.0"\nedition = "2021"\n' > Cargo.toml
printf '#[test]\nfn binds() {\n    std::net::TcpListener::bind("127.0.0.1:0").unwrap();\n}\n' > src/lib.rs
printf 'target\n' > .gitignore
cargo generate-lockfile -q --offline 2>/dev/null || cargo generate-lockfile -q; git init -q && git add -A && git -c user.email=a@b -c user.name=a commit -qm init && git branch -M main
echo "agent-hearth: $($1 --version), $(codex --version), $(uname -sm), $(date -I)"
echo "== golden: $($AH golden refresh 2>&1 | tail -1)"
echo "== settle in the sandbox"; codex sandbox -c sandbox_mode='"workspace-write"' -c "sandbox_workspace_write.writable_roots=[\"$B\",\"$HOME/.cargo\"]" -- $AH settle 2>&1 | tail -4; echo "exit ${PIPESTATUS[0]}"
echo "== status: $($AH status 2>&1 | tail -2)"
echo "== settle outside"; $AH settle 2>&1 | tail -3; echo "exit ${PIPESTATUS[0]}"; echo "== again"; $AH settle 2>&1 | tail -3; echo "exit ${PIPESTATUS[0]}"
cd /; rm -rf $B
