# Benchmarks

Scripts and their raw output. They only touch disposable copies under `~/.cache`. Linux: 32
cores, 62 GB RAM, btrfs with zstd, Cargo 1.98. Run them under a memory cap, for example:

```sh
systemd-run --user --scope -p MemoryHigh=36G -p MemoryMax=42G -- devenv shell -- \
  env AH=<agent-hearth binary> REV=<commit> FILES="<files agents edit>" bash bench/public.sh <git clone>
```

| Script | Question | Result |
|---|---|---|
| `public.sh` | On a public repository at a pinned commit: a new worktree's first and second edit with an empty target, main's target copied (`cp -a --reflink`), the copy plus mtime sync, and Agent Hearth; 1 agent (3 rounds, interleaved) and 4 at once; golden cost; the check gate on a seeded type error | `public-*-linux1.txt` (goldens built every member alone), `public-*-linux2.txt` (seeded, lazy goldens; warming was still on, so a warm could run into the next variant's turn) |
| `sandbox-settle.sh` | A settle in Codex's sandbox of a crate whose test binds a socket: it fails, says why and keeps no verdict; outside, it passes | `sandbox-settle-linux1.txt`, `sandbox-settle-mac1.txt` (macOS, Seatbelt) |
| `sandbox-commands.sh` (with `sandbox-inner.sh`) | `cargo build`, `check`, `cargo test`, `settle` and `status` (now part of `doctor`) inside Codex's and Claude Code's sandboxes (`CLI=claude`): with their default writable paths each fails and names the setting; with the state directory added all run | `sandbox-commands-codex-linux1.txt`, `sandbox-commands-claude-linux1.txt`, `sandbox-commands-codex-mac1.txt` |
| `agent-gaps.py` | From Claude Code and Codex transcripts on this machine: how long agents wait for cargo, and how long they leave between their last file write and their next build, which a silent background build could hide. Aggregates only: no names, paths or commands | `agent-gaps-linux1.txt` |
| `warm.sh` | A replayed agent turn on a public repository: edits with a gap after each, then the agent's build, with and without `agent-hearth warm` (the PostToolUse hook), for edits that compile on their own and edits that only compile once the last is made | `warm-atuin-linux1.txt` (before a new edit's warm stopped an older one), `warm-atuin-linux2.txt`, `warm-atuin-loaded-linux1.txt` (`LOAD=32` busy loops), `warm-next-atuin-linux1.txt` (`SCENARIOS=next`: `check`, then the test build) |
| `settings.sh` | A warm worktree's later turns with build settings passed as `cargo --config`: line tables only for workspace crates, optimised dependencies without debuginfo, both; the build after an edit and the unit tests (`TESTS`, by default `--lib --bins`) | `settings-atuin-linux1.txt`, `settings-ruff-linux1.txt` (`TESTS="--lib --bins --exclude ruff_unicode_names2"`: its tests need nightly) |
| `gate.sh` | `agent-hearth check` on a main that may not be clean under this toolchain: a new worktree with no change must pass and change no file; then a seeded type error must be one line | `gate-atuin-linux1.txt` (main formatted with nightly rustfmt), `gate-nushell-linux1.txt`, `gate-ruff-linux1.txt` (clippy warnings on main), `gate-atuin-mac1.txt` (an M2 Mac mini) |
| `slots.sh` | The slot pool where there is no pin (macOS): a new slot's first and second edit, then the same slot reused by the next agents, through `spawn`, `agent-hearth cargo` and the WorktreeRemove hook; then `check` | `slots-atuin-mac1.txt` (an M2 Mac mini, 8 cores, 16 GB, APFS) |
| `feedback.sh` | How soon `settle` tells an agent its tests fail: a failing test settled (the whole suite), an unrelated change with it still failing (the recent failure first, alone), the test fixed; then N settles at once | `feedback-atuin-mac1.txt` (the M2 Mac mini; before failed-first skipped doctests), `feedback-atuin-mac2.txt` (after; both through the settle daemon, since replaced), `feedback-atuin-mac3.txt` (settle places asking 4 GB plus a quarter of the machine, which never fitted: one at a time) |
| `copied-paths.sh` | Whether a target copied from main's checkout still runs main's code after the new worktree builds: which files in it name main's checkout, and whether any test executable does | `copied-paths-atuin-mac1.txt` (the M2 Mac mini: the 14 workspace crates rebuilt and no test executable names main's checkout; dependencies' artifacts do, harmlessly) |
| `golden.sh` | What a golden refresh costs when main moves: fresh against seeded from the previous commit's golden, and what a new worktree from each then compiles | `golden-atuin-linux1.txt`, `golden-nushell-linux1.txt` (their worktree checks after the first pair failed: two pairs shared a worktree name), `golden-atuin-linux2.txt` (with fresh-golden controls) |
| `agents.sh`, `agents.py`, `agents-timeline.py`, `agents-compare.sh` | Real Codex or Claude Code agents with Agent Hearth's hooks (or `BASELINE=1` without, `WARM=off` without warms), one task each from `$TASKS`; how their builds overlap, the gaps between edits and builds, `agent-hearth stats`, and how they end | `agents-atuin-codex-warm-on-1.txt`, `agents-atuin-codex-warm-off-1.txt` (`tasks-atuin.txt`, 3 GPT-6-Luna agents; their settles fail on atuin's end-to-end terminal tests, and `check` reformatted 90 files, both before the fixes they led to); against the fair baseline (`FAIR=1`): `agents-atuin-codex-{fair,hearth}-{1,2}.txt` (6 GPT-6-Luna agents), `agents-atuin-claude-{fair,hearth}-1.txt` (6 Haiku 5.5 agents), `agents-atuin-codex-{fair,hearth}-n10.txt` (10 at once, after retries waited for a quieter machine), `agents-atuin-codex-hearth-n10-places.txt` (the same with settle places instead of the daemon) |

The public repositories, as cloned from GitHub:

| Repository | Commit | `FILES` (agent *n* edits file *n*, round robin; the first gets the type error) |
|---|---|---|
| [atuinsh/atuin](https://github.com/atuinsh/atuin) | `c58d2e99` (v18.24.1) | `crates/atuin-client/src/lib.rs crates/atuin-history/src/lib.rs crates/atuin-server/src/lib.rs crates/atuin-common/src/lib.rs` |
| [nushell/nushell](https://github.com/nushell/nushell) | `b632ef099` (0.116.1) | `crates/nu-command/src/lib.rs crates/nu-cli/src/lib.rs crates/nu-engine/src/lib.rs crates/nu-parser/src/lib.rs` |
| [astral-sh/ruff](https://github.com/astral-sh/ruff) | `4683270c6` (0.17.1) | `crates/ruff_linter/src/lib.rs crates/ruff_python_formatter/src/lib.rs crates/ruff_workspace/src/lib.rs crates/ruff_server/src/lib.rs` |

atuin and nushell need OpenSSL and pkg-config on the path. ruff pins Rust 1.99 in
`rust-toolchain.toml`; it was built with 1.98.

Earlier measurements on private workspaces were removed, with their scripts and raw output: real
agents against plain worktrees, the slot pool on Linux, a shared jobserver, `-p` builds and clippy
arguments, per-worktree devenv shells, and the audit's comparison with Kache, sccache and
`--remap-path-prefix`. Where `PLAN.md` and `docs/design.md` still describe what those found,
they say it is unmeasured on a public repository. Some results here came from commands since
removed (`land`, `stats`, the settle daemon); the code and docs cite them as evidence for what
was kept.

Single machine, one session; other work on the machine shows in each line's load average. Treat
the numbers as indicative.
