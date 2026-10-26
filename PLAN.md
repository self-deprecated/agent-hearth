# Agent Hearth plan

Keep this current when scope changes. How it works is in `docs/design.md`; the evidence is in `bench/`.

## Goal

Ten coding agents, each in its own worktree of the same Rust workspace, each get their first
feedback in under 30 seconds and later feedback at single-agent speed (the CPU allowing), with
diagnostics short enough to feed straight back to the agent.

On atuin, nushell and ruff with 32 cores (`bench/public-*.txt`), one agent's first clippy and
test build in a new worktree takes 182–184s. Copying main's warm target into it (`cp -a
--reflink`, what Worktrunk and similar tools do) gives 93–167s, and with mtime sync 67–159s.
Agent Hearth gives **20–56s**; with four agents at once, 11–176s per agent. Still open: nushell,
where an edit costs 36–49s even in a warm target, and ten agents at once.

## Platforms

Agent Hearth targets Linux, macOS and Windows. Linux is the first measured target: phases 1 and 2
are accepted there first, every number in `bench/` so far is from Linux, and the speed targets in
this plan are Linux targets. Never quote a Linux number for another platform; say it is unmeasured.

| | Seed (reflink clone) | mtime sync | Fixed path |
|---|---|---|---|
| Linux | btrfs or XFS; not ext4 | Yes | bwrap pin (measured) |
| macOS | APFS `clonefile` | Yes | None yet; the slot pool is the candidate (experiment 5) |
| Windows | ReFS or Dev Drive block cloning; not NTFS | Yes | None yet; the slot pool is the candidate (experiment 5) |

Keep platform-specific code to three places: cloning, the fixed-path strategy, and path
normalisation. The phase 2 gate, verdicts and diagnostics are the same everywhere. Measure macOS on
a real Mac. A virtualised Windows VM can show correctness but not speed.

## Dates

- **2026-11-12:** Cargo 1.100 ships the new build-dir layout. Re-run `bench/` and `doctor` on it.
- **2026-11-19:** Rust Aarhus talk, "Self-Healing Rust". Phases 1 and 2 must be demo-ready on
  Linux, with numbers from public repositories.

## Progress (2026-10-21)

Public numbers are from `bench/public.sh` on atuin, nushell and ruff (`bench/public-*.txt`).
Earlier results from private workspaces were removed with their raw output; where this plan
still says what they found, it says so.

- **Phase 1: done on Linux.** `bench/public.sh` through the tool: one agent's first edit in a
  new worktree 20–56s median on atuin, nushell and ruff, against 67–159s for the best copy;
  later edits are no faster than a warm target's.
  `agent-hearth cargo test` and `cargo test` agree; subdirectories work; a refresh during adopts
  and stray writes are covered by tests. Golden refresh also builds every member's `-p` variant,
  since agents build one package at a time; a member that does not build alone (nushell has
  some) is skipped with a warning instead of failing the golden.
- **macOS: builds, tests pass.** The slot pool is built; its speed is unmeasured on a public
  repository. Windows: compiles and lints only.
- **Phase 2: done.** A seeded type error gives one line of 86–88 bytes (raw JSON 0.7–1.0 MB);
  a repeated check takes 39–63 ms on atuin and nushell, 138–145 ms on ruff, over the 100 ms
  target. Claude Code and Codex hooks are built and were exercised by
  real agents (private workspaces); Pi and T3 are documented, not exercised.
- **Phase 3: built.** Background settles run one at a time on the machine (or as the daemon
  admits them), at low CPU priority, and retry failed tests once (flaky ones pass). Tests that
  failed in recent settles run first, on their own; if one still fails, the settle stops there
  with a partial verdict. Unmeasured: how much sooner agents hear of failures.
  `test --affected` falls back to the full suite when the affected crates are most of the
  workspace. "Full" for `settle` is `cargo test --workspace`, which leaves out `#[ignore]` tests;
  no per-repo setting exists yet. Unmeasured on a public repository.
- **Phase 5: `gc` and `land` built.** `land` handles jj workspaces only. It gates copies of the
  changes, so the workspaces and other agents' operations are never rewritten before `main`
  moves. A gate fails when `check` would format or fix anything, since those fixes belong in the
  workspace. A failing batch is split in halves rather than bisected commit by commit.
  `--accept-fewer` lifts the gate's test-count guard. The golden refresh at its end runs in the
  background.
- **Audit (2026-10-20).** Four models checked the claims, one after another. The mechanism holds,
  and the fixed path is what no existing tool has; copying a warm target alone gives most of the
  gain over empty targets, later edits are no faster, and each golden is a cold build. Two false
  passes were found and fixed: `adopt` left files that differ from the golden with their own
  mtimes, so an edit older than the golden's build ran main's code. Other gaps remain, listed
  under "Known gaps" in `docs/design.md`. The real-agent runs compared against empty targets with
  no hooks, so their quality results are what any blocking gate gives (experiment 8).
- **Phase 4: partly built.** The settle daemon admits settles by free memory. `warm` builds the
  worktree in the background after each edit, silently, so the agent's next build finds the work
  done (the PostToolUse hook starts it). It was first put off because the Stop hook's check took
  only a few seconds; that measured the wrong thing, since the waiting is in agents' own builds
  during the turn. On atuin the agent's next build took 5.6s against 17.2s when it left 12s after
  each edit, 10.1s against 17.4s at 4s, and the same as without at 0s (`bench/warm-*.txt`; less
  under load). But real agents mostly build in the same breath as they edit: the median gap is
  0s, so it could hide 7-18% of their waiting (`bench/agent-gaps-linux1.txt`). Builds with no
  edit since the previous one are 41% of it, mostly a different command (35%; experiment 13). A shared jobserver is not built: it saved
  neither time nor memory (private workspaces).

## Phase 1: warm start

Commands: `golden refresh`, `adopt`, `spawn`, `cargo`, `doctor`.

1. **Locate things.** Find the worktree root (`jj workspace root`, falling back to
   `git rev-parse --show-toplevel`) and a repo id shared by all of a repository's worktrees.
   Create the state directory and, on Linux, the pin.
2. **`golden refresh`.**
   - Materialise the revision as the golden's source tree.
   - Build it at the fixed path: `clippy --workspace --all-targets`, then
     `test --workspace --no-run`.
   - Record the golden id (commit, `Cargo.lock` hash, toolchain, flags).
   - Swap it in by atomically replacing a pointer file, so concurrent `adopt`s see either the old
     golden or the new one. Renaming directories or symlinks does not work the same on Windows.
3. **`adopt`.**
   - Choose the golden (matching lockfile, toolchain and flags; nearest ancestor commit).
   - Reflink-clone its target in-process, file by file, and copy each file's mtime with
     `File::set_modified`. Don't shell out to `cp -a --reflink=always`: it is GNU-only.
   - Copy the golden's mtimes onto identical source files, give every other file the time now,
     and record the worktree.
   - If it can't reflink, refuse with a clear message; never do a slow full copy silently.
4. **`cargo`.**
   - Run cargo with the worktree's own target and `CARGO_INCREMENTAL=1`, at the fixed path where
     the platform has one: through bwrap at the pin on Linux.
   - Map the agent's cwd into the pin.
   - Pass the exit code through.
   - Stream stdout and stderr with pin paths rewritten to real paths.
5. **`spawn`.** Run `jj workspace add` at `--rev`, then `adopt`.
6. **`doctor`.** Report the fixed-path strategy this machine gets (on Linux: do bwrap and user
   namespaces work?), whether the state filesystem supports reflinks, and which cargo and rustc
   are in use.

**Done when, on Linux:**

- `bench/public.sh` drives the tool through `spawn` and `cargo` on public repositories: a
  new worktree's first edit is well below a copied target's, alone and with several agents at
  once, and later edits are no slower than in a warm `target/`.
- `agent-hearth cargo test` and plain `cargo test` agree on pass/fail and test counts in the same
  worktree.
- Running from a subdirectory works, and every diagnostic path shown is a real worktree path.
- Ten concurrent `agent-hearth cargo` runs work, and a `golden refresh` during `adopt`s breaks
  nothing.
- Nothing is written outside the state directory, the cache directory and the worktree's ignored
  `target/`.

**Then, on macOS and Windows:** the tool builds and its own tests pass; `adopt` seeds and syncs
mtimes on a reflink-capable filesystem; `cargo` runs without a fixed path. No speed target until
experiments 4 and 5 have run.

## Phase 2: turn-end gate

Commands: `check`, plus hook wiring.

1. `cargo fmt`, then `clippy --workspace --all-targets --message-format=json` with
   `CARGO_BUILD_WARNINGS=deny`. Only when a diagnostic has a machine-applicable suggestion, run
   `clippy --fix --allow-dirty --allow-staged` and clippy again. Unmeasured: what `--fix` costs
   when nothing is fixable; skipping it then avoids finding out on every turn.
2. Filter to compiler messages and deduplicate across targets. Print one line per diagnostic:
   `file:line:col CODE message`. Add the rendered text only with `--verbose`.
3. Fingerprint the code and store verdicts. A verdict counts only if the fingerprint is unchanged
   at the end of the run; otherwise exit 3 (superseded).
4. Hooks:
   - Claude Code `Stop` hook: block with diagnostics; stop blocking after 3 attempts that make no
     progress.
   - Claude Code `PreToolUse(Bash)`: rewrite `cargo` to `agent-hearth cargo`.
   - Claude Code `WorktreeCreate`: `spawn`, since the hook replaces Claude Code's own worktree
     creation; `WorktreeRemove` to forget or remove what it made.
   - Pi `finalChecks`.
   - T3 setup script.

**Done when:**

- A seeded type error yields a diagnostic of at most 100 bytes (`bench/public.sh`).
- A repeated `check` on unchanged code returns the stored verdict in under 100 ms.
- A Claude Code session fixes a seeded error through the Stop hook without human input (done on a
  private workspace; not re-run on a public one).

## Phase 3: tests and settle

Commands: `test [--affected]`, `settle [--bg]`, `status`.

- `--affected` runs nextest with `rdeps(changed crates)` against the last verified fingerprint's
  base. Compiling is always `--workspace`, never `-p`, which cost 5.4s in feature-unification
  rebuilds.
- `settle --bg` runs the full suite and doctests, then writes the verdict. The next `Stop` reads
  it.
- Declare what "full" means per repo (for example, leaving out tests that need the network).
- Guard against the test count dropping between settles.

## Phase 4: daemon

Commands: `warm` (built), `watch`.

- `warm`: after each edit, build silently in the background; the agent's own build waits for it
  or stops it. Results never reach the agent: a linter between writes floods the transcript with
  errors from half-finished edits.
- Share one jobserver FIFO across all cargo runs, with priorities: blocking gate first, then
  speculative, then settle, then golden refresh.
- **Measure before building it out:** do speculative builds and priorities shorten the time an
  agent waits, with real agents rather than lockstep benchmarks?

**What the measurements said** (experiments 1 and 3, private workspaces, removed): agents'
builds overlapped heavily, but a jobserver sized to the cores neither shortened a round nor saved
memory, and the costs real agents hit were fixed without a daemon (settle queue, `nice`, `-p`
variants, `-D warnings` moved to the environment). The Stop hook's check took a few seconds, so
speculative checks have little to hide. What only a daemon could do was admit settles by free
memory.

**Built: `agent-hearth daemon`**, settle admission by free memory (see `docs/design.md`). With ten
agents, every verdict came in about twice as soon as one settle at a time. Getting there took two
corrections: a settle's resident memory double-counted shared pages (anonymous memory matched the
machine's), and most of a settle's peak was its test binaries linking at once, so settles build
with fewer jobs. macOS measures the physical footprint, and a cgroup's `memory.max` caps available
memory. Still open: `watch` and speculative checks, not built.

## Phase 5: land and gc

- **`land`:** rebase finished worktrees onto main one at a time, run the full gate at the golden,
  advance main, refresh the golden. Batch when green; bisect a failing batch.
- **`gc`:** remove targets of worktrees that no longer exist and old goldens. Report the real disk
  use of shared extents.

## Experiments

Experiments 1, 3, 4, 5 and 6 ran on private workspaces; their results were removed with their
raw output and are unmeasured on a public repository. What they found shaped the tool:

1. **Real agents** (Codex and Claude Code, `bench/agents.sh`). Builds overlapped heavily (about
   half of ten agents building at any moment). The fixes it led to: one settle at a time at low
   priority, then the daemon; `-p` variants in the golden, since every agent built one package;
   `-- -D warnings` moved to `CARGO_BUILD_WARNINGS`, since clippy arguments re-lint every run;
   `devenv shell --` as a prefix the rewrite sees; `agent-hearth cargo clean` seeding again; the
   Stop hook waiting for short suites (`--settle auto`) and `UserPromptSubmit` reporting the
   rest; no verdicts kept from sandboxed settles. Codex agents ran the `agent-hearth cargo`
   command the hook named and kept using it. Against plain worktrees with no hooks, agents' first
   builds went from tens of seconds to a few, and given changes that broke tests they were not
   told to run, agents without the gate stopped with the suite failing while agents with it were
   sent back and fixed them. Re-run on a public repository with a fair baseline (experiment 8).
2. Cargo 1.100 and 1.101: does relative fingerprint pathing (cargo#17573) or
   `build.fingerprint = "content"` make mtime sync or pinning unnecessary?
3. **A shared jobserver.** Cargo joins a FIFO jobserver from `MAKEFLAGS`, which caps the
   compilers of ten lockstep turns near the core count. It saved neither time nor memory. Still
   open: `-j` or `build.jobs` given alongside it.
4. **Seed plus mtime sync without a fixed path,** on macOS (APFS) and Windows (Dev Drive). On
   Linux this is `bench/public.sh`'s `sync` variant. On macOS one agent's first edit was several
   times slower than a later one, and ten at once on an 8-core, 16 GB machine swapped. Windows
   unmeasured: no Windows machine yet.
5. **Slot pool: built.** A reused slot's first edit matched the pin's; a new slot's first agent
   pays the path once; after main moves, free slots need warming at each new golden, which
   `golden refresh` does in the background. Open: new slots ahead of their first agent, and
   Windows.
6. **Per-worktree devenv shells.** A worktree's own shell holds its path in six variables
   (`CARGO_INSTALL_ROOT`, `DEVENV_DOTFILE`, `DEVENV_ROOT`, `DEVENV_STATE`, `PATH`, `PWD`); it
   rebuilt nothing in the one project measured. A project whose build scripts rerun on one of
   them would rebuild.
7. **Cargo's `build-dir`.** On Cargo 1.100, should `adopt` seed and point `build.build-dir` (the
   intermediate artifacts, which are what stays warm) instead of `CARGO_TARGET_DIR`, leaving final
   artifacts in the worktree's `target/`?
8. **A fair agent baseline: measured** ("Real agents" in `docs/design.md`). On atuin, with main's
   target copied in and a plain Stop hook, agents took 772–2243s each and 1.7–11.7M tokens;
   with Agent Hearth 132–432s and 0.18–1.2M, Codex and Claude Code, six and ten at once. Most of
   it is the gate: main's failing tests sent every fair agent back. Left: a repository whose main
   passes its suite, to measure the build's share alone.
9. **Incremental goldens.** A golden is a cold build of each main commit (867–1815s and 67–118 GiB on the public repositories, against 165–191s for one warm build). Seed it
   from the previous golden with the same mtime rule as `adopt`, and have `land` run `gc`.

10. **Where an agent's build time goes** (`agent-hearth stats`, built): building against running
    tests, waiting for warms, and repeats of a command on unchanged code. Run real agents on a
    public repository with the log on; it decides 11-14.
11. **Build settings: measured** (`bench/settings-*.txt`; "Build settings" in `docs/design.md`).
    Line tables for workspace crates and optimised dependencies without debuginfo made atuin's
    builds 34-40% faster and its unit tests twice as fast; ruff already has them. They belong in
    the repository's `Cargo.toml`, where humans get them too and nothing rebuilds when cargo runs
    outside Agent Hearth: `doctor` reports a dev profile without them. The toolchain already links
    with LLD; mold through cargo did not take effect in this Nix shell.
12. **Answer repeats from a cache: measured, not worth building.** The same command on
    unchanged code, with no write since it last ran, was 1% of agents' waiting (2% counted
    loosely; `bench/agent-gaps-linux1.txt`).
13. **Warm the next command after a build: built.** A different command with no write since the
    previous one was 35% of agents' waiting (`test` again with other arguments, `check` or
    `build` then `test`, `test` then `clippy`). After each agent build, a warm builds its three
    most recent commands: on atuin, `check` then the test build took 5.9s against 16.1s with 12s
    between them, 13.4s against 16.4s with 4s, the same at 0s (`bench/warm-next-atuin-linux1.txt`).
    Open: how long real agents leave between two cargo calls with no edit.
14. **Async builds through a skill.** The agent starts `agent-hearth cargo …` in the background
    (Claude Code notifies it when it ends) and keeps working; Agent Hearth delivers one line per
    diagnostic at its next tool call (PostToolUse context) or at Stop, marked stale when the code
    changed since. Pays only when the agent has other work while it waits; on atuin's small
    tasks agents had none (experiment 8), so it is not built.
15. **A cheaper golden: built.** `-p` variants only for the members the build log shows agents
    building alone (all of them took atuin's golden from 168s to 867s), and each golden seeded
    from the previous one: 37s against 163s fresh after an ordinary atuin commit, 97-107s against
    168-189s after a version bump or a release (`bench/golden-*.txt`). Cargo's workspace feature
    unification, which would make the variants unnecessary, still needs nightly on 1.98.
16. **Settles on another machine,** leaving local cores to the builds agents wait on.

## Open decisions

- **License and publishing.** `publish = false` until decided.
- **Golden source tree.** A persistent jj workspace per golden, or an exported tree outside jj's
  workspace list?
- **Git worktrees.** T3 creates git worktrees. Phase 1's `adopt` is path-based, so both work; only
  `spawn` and `golden refresh` need a VCS.
- **cargo-hauler.** Integrate with it (it brokers cargo runs across agents with a global
  jobserver and Kache, but neither seeds nor pins) or stay separate?
- **Stop without a verdict.** The Stop hook lets the agent stop when a settle is superseded, the
  tool errors or the hook times out. Block instead ("no verdict"), at the cost of agents whose
  tests write into the worktree being sent back until the tests stop doing so?
- **What a verdict covers.** The fingerprint misses ignored files, environment variables and
  `CARGO_PROFILE_*`, and verdicts are shared across worktrees. Hash what Cargo's dep-info names
  plus `CARGO_*` and `RUST*` variables, or keep verdicts per worktree?
- **Worktree placement.** Decided for the slot pool: without the pin, `spawn` (and so Claude
  Code's `WorktreeCreate`) puts worktrees in slots under the state directory. Worktrees T3 or a
  user create elsewhere still build at their own path. Free slots are warmed at each new golden.
  Open: should new slots be made ahead of the first agent, and should `land` free them?
- **Where per-worktree build directories live: decided**, in the state directory. Agent sandboxes
  (Codex's, Claude Code's) block writes there by default; one setting allows them, every command
  then works (`bench/sandbox-commands-*-linux1.txt`), and each command without it names the
  setting. Moving them into the worktree's `target/` would lose the guarantee that they share
  the golden's filesystem, which reflinks need. Machine-wide files (the daemon's, the settle
  queue) moved from the cache to the state directory, so one directory is enough.
- **Filesystems without reflinks** (ext4, NTFS). Worktrees there build cold, with a notice,
  rather than fail. Allow an explicit opt-in full copy? Most Windows drives are NTFS.
