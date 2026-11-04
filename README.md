# Agent Hearth

Warm Rust builds for coding agents that each work in their own worktree.

## Why

A new worktree starts with an empty `target/`, so an agent's first build there is a cold build of
the whole workspace. Sharing one target between worktrees makes agents overwrite each other's
artifacts. Copying main's warm target into each worktree (as Worktrunk and similar tools do) saves
some of that, but the first edit still rebuilds the workspace's crates without incremental reuse,
because the compiler records the checkout's path.

Agent Hearth gives each worktree its own build directory, cloned from a warm build of main (a
golden), and runs every build at one fixed path, so that clone stays warm. It also gives the agent
a turn-end gate: `fmt`, clippy and the full test suite, one line per diagnostic, cached by
content, and measured against main: formatting, warnings and failing tests main already has are
left alone, so the agent is only sent back for what it broke.

## What it saves

Most first, on Linux with 32 cores unless it says otherwise:

| | How | Saving |
|---|---|---|
| **Agents not sent back for main's failures** | The gate leaves out what main already fails: the golden's own test suite runs once per main commit, and `check` and `settle` report only what is new | Real agents on atuin finished in 2–7 minutes each against 13–37, on a fifth to a tenth of the tokens, and were never sent back (a plain Stop hook sent them back 16–30 times a round) |
| **A warm first build in a new worktree** | Its build directory is cloned from the golden (reflinks: no copying), main's mtimes on unchanged files, and every build at one fixed path, where incremental state stays valid | 20–56s against 67–159s for the best copy of main's target, and 182–184s for an empty one (table below) |
| **Faster builds after each edit** | `setup --build-settings`: line tables instead of full debuginfo, dependencies optimised once, in the golden | atuin's builds after an edit 34–40% faster, its unit tests twice as fast |
| **Failures heard sooner** | `settle` reruns the tests that failed recently on their own before the whole suite | On an M2, a still-failing test reported in 52s against about 600s for the suite |
| **Settles that run only what an edit reached** | `settle` keeps main's results for test binaries that are byte for byte the golden's, when only compiled Rust changed | On an 8 vCPU VM, a failing test added to a library reported in 33s against 662s for atuin's whole suite, 34s against 125s for ruff's; nothing for an edit that moves lines in a crate ruff's programs link, and anything else runs the whole suite |
| **Test edits that rebuild one crate** | `setup --split-tests`, once: inline test modules moved to `tests.rs`, verified byte-identical | On an 8 vCPU VM, a test-only edit's test build 0.43× as long on atuin and 0.067× on ruff (48 and 100 units compiled before, 1 after); no-op builds unchanged |
| **Test suites side by side** | Settles run several at once while memory allows, one always | On an M2, three suites in 683s together, where one alone takes 575–595s; ten agents' settles on Linux all passed, the last 781s after the agents started |
| **A golden that follows main** | Each golden is seeded from the previous one, and the next prompt starts it when main has moved | 37s after an ordinary atuin commit, about 100s after a release, against 163s from scratch |
| **A nightly toolchain, if you want it** | `setup --nightly`: every build Agent Hearth runs on a pinned nightly with `-Zthreads`, turned on only once main's suite fails the same on it; your own toolchain stays for humans and CI | atuin's turns in a new worktree 18–23% faster for one agent, and four agents at once done 16% sooner |
| Builds while the agent edits | After each edit, a silent build in the background | Small: real agents build straight after editing, so it can hide 7–18% of their waiting at most |

One agent's first edit in a new worktree, then `cargo clippy` and `cargo test --no-run` on the
whole workspace (median of 3):

| | [atuin](https://github.com/atuinsh/atuin) | [nushell](https://github.com/nushell/nushell) | [ruff](https://github.com/astral-sh/ruff) |
|---|---|---|---|
| Empty `target/` | 182s | 184s | 182s |
| Main's `target/` copied in (`cp -a --reflink`) | 102s | 93s | 167s |
| Copied, plus main's mtimes on unchanged files | 89s | 67s | 159s |
| **Agent Hearth** | **28s** | **56s** | **20s** |
| 4 agents at once, per agent: best copy against Agent Hearth | 193s / 30s | 172s / 176s | 325s / 11s |

Later edits are no faster than in any warm `target/`. On nushell an edit costs 36–49s even in a
warm `target/`, so a new worktree has little left to save, and four at once gain nothing. The cost
is the golden: one warm build of the workspace per main commit (3–4 minutes and 16–51 GiB when
built from scratch), as a copied target needs too.

**How these were measured.** Every number comes from a script in [`bench/`](bench/), with its raw
output, on atuin, nushell and ruff at pinned commits. Real agents (Codex and Claude Code, six and
ten at once, a small task each) were compared with the setup a careful user would make without
Agent Hearth: main's warm target copied into each worktree and a Stop hook running clippy and the
tests. Much of that gap comes from atuin's main failing tests on the test machine, which the plain
hook could not tell from the agent's own; a repository whose main passes its suite is not
measured yet. What did not pay was removed: a merge queue, tests of affected crates only, a
results cache, asynchronous builds and a settle daemon. Other linkers and Cranelift were not
added: they saved too little or broke tests ("Tried and dropped" in [`PLAN.md`](PLAN.md)). The details are in [`docs/design.md`](docs/design.md).

## Use

```sh
cargo install --git https://github.com/self-deprecated/agent-hearth
agent-hearth doctor              # what works on this machine
agent-hearth setup               # in the repository: hooks for Claude Code and Codex, and a golden
agent-hearth spawn fix-login     # a new worktree, already warm
agent-hearth cargo test          # cargo, at the fixed path
agent-hearth check               # fmt and clippy, against main; one line per diagnostic
agent-hearth settle              # the full suite, against main, recent failures first; only
                                 # what the change reached when only Rust sources changed
agent-hearth setup --nightly     # optional: every build on a pinned nightly with -Zthreads
```

When main moves, the golden follows by itself at the agent's next prompt. `setup
--build-settings` adds the build settings to `Cargo.toml`; they apply once committed to main.
`setup --nightly` tries a pinned nightly on main first and turns it on only if main's suite fails
the same on it; your repository's own toolchain stays for humans and CI.
`setup --split-tests` moves inline test modules to their own files and verifies the release
build is unchanged; commit it to main too.

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
first edit took 26-38s against 97s for a new one. Windows compiles. Without bubblewrap,
worktrees are still seeded; without reflinks, Agent Hearth is the gate only, which is where most
of real agents' gain came from. `doctor`'s first line says which you have. A cached verdict can
miss some inputs: see "Known gaps" in [`docs/design.md`](docs/design.md). The plan is in
[`PLAN.md`](PLAN.md), measurements in [`bench/`](bench/).
