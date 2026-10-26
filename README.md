# Agent Hearth

Warm Rust builds for coding agents that each work in their own worktree.

## Why

A new worktree starts with an empty `target/`, so an agent's first build there is a cold build of
the whole workspace. Sharing one target between worktrees makes agents overwrite each other's
artifacts. Copying main's warm target into each worktree (as Worktrunk and similar tools do) saves
some of that, but the first edit still rebuilds the workspace's crates without incremental reuse,
because the compiler records the checkout's path.

Agent Hearth gives each worktree its own build directory, cloned from a warm build of main, and
runs every build at one fixed path, so that clone stays warm. While the agent edits, it builds
in the background, unseen, so a build started a few seconds after the last edit finds most of the
work done. It also gives the agent a turn-end gate: `fmt`, clippy and the full test suite (tests
that failed recently first), one line per diagnostic, cached by content, and measured against
main: formatting, warnings and failing tests main already has are left alone, so the agent is
only sent back for what it broke.

One agent's first edit in a new worktree, then `cargo clippy` and `cargo test --no-run` on the
whole workspace (median of 3, Linux, 32 cores; [`bench/`](bench/)):

| | [atuin](https://github.com/atuinsh/atuin) | [nushell](https://github.com/nushell/nushell) | [ruff](https://github.com/astral-sh/ruff) |
|---|---|---|---|
| Empty `target/` | 182s | 184s | 182s |
| Main's `target/` copied in (`cp -a --reflink`) | 102s | 93s | 167s |
| Copied, plus main's mtimes on unchanged files | 89s | 67s | 159s |
| **Agent Hearth** | **28s** | **56s** | **20s** |
| 4 agents at once, per agent: best copy against Agent Hearth | 193s / 30s | 172s / 176s | 325s / 11s |

Later edits are no faster than in any warm `target/`. On nushell, every edit there costs 36–49s,
even in a warm `target/`, so a new worktree has little left to save; four at once, the machine is
the limit either way. The cost is a golden per main commit: one warm build of the workspace (3–4
minutes and 16–51 GiB), as a copied target needs too, plus the packages agents have built alone.
Each new golden is seeded from the previous one, so when main moves it rebuilds only what changed:
37s for an ordinary atuin commit, about 100s for a release.

With real agents on atuin (six or ten Codex or Claude Code agents, a small task each), against
the same agents with main's target copied in and a plain Stop hook running clippy and the tests:
each agent finished in 2–7 minutes against 13–37, on a fifth to a tenth of the tokens, and was
never sent back. Most of that is the gate: the plain hook kept sending agents back for tests
that fail on main (`bench/agents-atuin-*.txt`; "Real agents" in
[`docs/design.md`](docs/design.md)).

## Use

```sh
cargo install --git https://github.com/self-deprecated/agent-hearth
agent-hearth doctor              # what works on this machine
agent-hearth setup               # in the repository: hooks for Claude Code and Codex, and a golden
agent-hearth golden refresh      # when main moves: the warm build worktrees start from
agent-hearth spawn fix-login     # a new worktree, already warm
agent-hearth cargo test          # cargo, at the fixed path
agent-hearth check               # fmt and clippy; one line per diagnostic
```

## Hook in your agent

Give your agent this:

> Set up Agent Hearth in this repository by following
> https://github.com/self-deprecated/agent-hearth/blob/main/docs/agents.md

It runs `agent-hearth setup`, which wires in the hooks for Claude Code and Codex (warm worktrees,
`cargo` routed through Agent Hearth, the gate when the agent stops), or tells you to run it when
its sandbox won't let it, and passes on the settings left for you.

## Status

Linux is built and measured: the fixed path uses bubblewrap and user namespaces, and seeding
needs btrfs or XFS. macOS uses a pool of fixed worktree slots instead: on atuin, a reused slot's
first edit took 26-38s against 97s for a new one. Windows compiles. Without reflinks or without
bubblewrap it falls back (worktrees build cold, or in slots) rather than fail, and `doctor` says
why. A cached verdict can miss some inputs: see "Known gaps" in
[`docs/design.md`](docs/design.md). The plan is in [`PLAN.md`](PLAN.md), measurements in
[`bench/`](bench/).
