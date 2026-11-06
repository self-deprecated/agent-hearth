# Agent Hearth design

## Mental model

Cargo is fast when a build directory has already seen *this code at this path*. Agent swarms
break both halves: every worktree is a new path with an empty `target/`, and every turn ends
with "is it green?". Agent Hearth owns the build directories so every worktree starts as a warm
clone of main, always builds at the same path, and answers "green?" from a stored verdict when
the code has not changed.

It targets Linux, macOS and Windows. Seeding and verdicts work the same on all three; how a
worktree gets to build at the same path depends on the platform (see "Building at one path").
Linux and macOS are measured; Windows is not.

## Names

- Crate and binary: `agent-hearth`. A short personal alias such as `ah` is fine; the tool does
  not install one.
- Environment variables: `AGENT_HEARTH_*`.
- `hearth` alone is taken as a binary name by `hearth-serve` (aiassistsecure/hearth), so the tool
  never installs a `hearth` command.

## Measured basis

What each part saves, most first, and where its evidence is below:

1. **The gate against main** sent real agents back only for what they broke: 2–7 minutes per
   agent against 13–37 with a plain Stop hook ("Real agents").
2. **Seeding plus the fixed path** made a new worktree's first build 20–56s against 67–159s for
   the best copy of main's target (the table below).
3. **Build settings** made atuin's builds after an edit 34–40% faster ("Build settings").
4. **Failed tests first** reported a still-failing test in 52s against about 600s ("Failed tests
   first").
5. **Settles at once** finished three suites in 683s where one takes 575–595s ("Settles at
   once").
6. **Seeded goldens** follow main in 37s after an ordinary commit (the table below).
7. **Warming** hides at most 7–18% of real agents' waiting ("Warming while the agent edits").

`bench/public.sh` on atuin, nushell and ruff at pinned commits (`bench/README.md`), Linux, 32
cores, 62 GB RAM, btrfs with zstd, Cargo 1.98. A turn is `cargo clippy --workspace
--all-targets`, then `cargo test --workspace --no-run`. Medians of three rounds for one agent,
of four agents for "4 at once"; other work on the machine shows in each line's load average.
From `public-*-linux2.txt` (2026-10-26), with warming on; the `-p` golden row from `linux1`.

| New worktree, first edit | atuin | nushell | ruff |
|---|---|---|---|
| Empty target | 182s | 184s | 182s |
| Main's target copied (`cp -a --reflink`) | 102s | 93s | 167s |
| … plus mtime sync | 89s | 67s | 159s |
| Agent Hearth (seed + mtime sync + fixed path) | 28s | 56s | 20s |
| 4 at once, per agent: empty / copy / sync / Agent Hearth | 378 / 218 / 193 / 30s | 611 / 253 / 172 / 176s | 425 / 334 / 325 / 11s |
| Second edit in the same worktree, every variant | 28s | 36–49s | 18–22s |
| Warm build of main, for the copies | 164s | 184s | 184s |
| Golden refresh, when it built each member with `-p` too (2026-10-21) | 867s, 70 GiB | 937s, 118 GiB | 1815s, 67 GiB |
| … the workspace only (no member built alone yet), fresh | 163–169s, 28 GiB | 188–225s, 51 GiB | 188s, 16 GiB |
| … seeded from the previous golden: one commit / a release | 37s / 103s | 97s / 103s | — |
| `check`, seeded type error | one 89-byte line (raw JSON 700 KB) | 3 lines, 161 bytes (1.0 MB) | 3 lines, 163 bytes (843 KB) |
| `check` again on unchanged code | 42–45 ms | 55–58 ms | 140–143 ms |

What these show:

1. **Copying a warm target helps unevenly.** It saved 44–49% of the first edit on atuin and
   nushell, 8% on ruff. Syncing main's mtimes onto unchanged files saves a little more. Copies
   must keep the target's timestamps (`cp -a`, not `cp -r`), or cargo rebuilds what they hold.
2. **A checkout's path is part of the compiled state.** At a new path the first edit rebuilds
   the workspace crates without incremental reuse. Building at one fixed path removes that: 3–8×
   faster than the best copy for one agent on atuin and ruff, 6–30× with four at once. On
   nushell an edit costs 36–49s even in a warm target, so the first edit gains less (1.2×), and
   four at once gain nothing: the machine is the limit.
3. **Later edits gain nothing.** Once a worktree has built at its own path, every variant is as
   fast as any warm target.
4. **The golden is the cost.** It is a build per main commit: one warm build of the workspace
   when fresh, less when seeded from the previous golden, plus each member agents have built
   alone (see "Risks").
5. **A shared target directory is wrong for parallel agents** (measured earlier on a private
   workspace): workspace crates are not keyed by checkout path, so divergent worktrees overwrite
   each other's artifacts and queue on one lock.

### Warming while the agent edits

`bench/warm.sh` on atuin replays an agent's turn in a worktree that has built at its path: three
edits in three crates with a gap after each, then the agent's `cargo test --workspace --no-run`,
timed, with and without `warm`. "Dependent": the first edit calls what the last adds, so that
crate compiles only at the end. Medians of five turns, the agent's build without / with `warm`:

| Gap after each edit | Independent | Dependent | Independent, 32 busy loops | Dependent, 32 busy loops |
|---|---|---|---|---|
| 0s | 19.8 / 22.2s (load 50-65) | 18.6 / 17.4s | 45.4 / 42.8s | 40.4 / 42.7s |
| 4s | 17.4 / 10.1s | 17.9 / 13.3s | 50.6 / 42.0s | 41.8 / 42.1s |
| 12s | 17.2 / 5.6s | 16.3 / 5.5s | 47.8 / 34.6s | 40.7 / 38.8s |

(`warm-atuin-linux2.txt`, `warm-atuin-loaded-linux1.txt`.) The first version gained nothing on
dependent edits: the warm of the broken state kept building while the finished state's waited
(`warm-atuin-linux1.txt`); a new edit's warm now stops it. At 0s on a busy machine the agent
waited for a niced warm (22.2s against 19.8s); a busy machine's agent now stops the warm instead.
Under load a niced warm gets little CPU, so the gains shrink.

After a build, a warm builds the agent's next command. In `warm.sh`'s `next` scenario the agent
runs `agent-hearth cargo check --workspace`, waits, then builds its tests: 16.6s against 16.3s
without warms at a 0s gap, 13.4s against 16.4s at 4s, 5.9s against 16.1s at 12s
(`warm-next-atuin-linux1.txt`).

How much this saves real agents depends on their gaps, and those are mostly zero: in 4,116 cargo
calls from Claude Code and Codex transcripts (28.6 hours of waiting), most builds after a write
started within a second, often in the same shell command, which no hook sees in time
(`bench/agent-gaps-linux1.txt`). A silent build after each write could hide 7-18% of that
waiting. Builds with no write since the previous one were 41% of it: the same command on
unchanged code 1%, a different command 35% (`test` with other arguments, `check` then `test`,
`test` then `clippy`).

### Build settings

`bench/settings.sh` times a warm worktree's later turns with build settings a repository's
`Cargo.toml` could have, passed as `cargo --config`: line tables only for workspace crates, and
dependencies optimised (`opt-level = 1`) without debuginfo. Dependencies are built once, in the
golden, so agents never pay for optimising them. Medians of five turns, build after the edit /
unit tests:

| | Own settings | Line tables | Optimised deps | Both |
|---|---|---|---|---|
| atuin, leaf crate edited | 6.8 / 42.1s | 5.4 / 36.7s | 4.9 / 19.6s | 4.1 / 19.9s |
| atuin, `atuin-common` edited | 20.1 / 38.1s | 15.0 / 35.3s | 15.2 / 19.8s | 13.2 / 19.4s |
| atuin target size | 25.0 GiB | 11.2 GiB | 17.5 GiB | 8.4 GiB |
| ruff, leaf crate edited | 3.5 / 9.7s | 5.0 / 12.5s | 3.2 / 7.9s | 3.0 / 7.8s |
| ruff, `ruff_text_size` edited | 25.1 / 9.4s | 23.9 / 8.9s | 22.6 / 8.9s | 22.5 / 8.9s |

On atuin, whose dev profile optimises only six dependencies, both settings make builds 34-40%
faster, halve the unit tests and shrink the build directory to a third. ruff already has
`opt-level = 1` and line tables in its dev profile, so only dropping dependencies' debuginfo
gains anything (about 10%). nushell's dev profile is the default. Two of atuin's line-tables
test runs failed: flaky, not the setting (`both`, with line tables too, never did, and 12 more
line-tables runs, with and without the benchmark's edit, all passed).

### Linkers, Cranelift and the parallel front end

The same benchmark, each variant on top of `both`: mold and wild instead of Rust's own LLD (which
Rust 1.98 already uses on x86_64 Linux), and three on a pinned nightly (1.101, 2026-11-01): the
nightly as it is, as the control; Cranelift compiling workspace crates (dependencies still by
LLVM); and the parallel front end, `-Zthreads=8`. Build after the edit, medians of three turns
for the leaf crate, the lower of two for the core one; from `settings-{atuin,ruff}-linux2.txt`
and, on the M2 Mac mini, `settings-atuin-mac{1,2}.txt` (two runs, each with its own nightly
control, shown as Cranelift's run / `-Zthreads`' run; mold and wild do not run on macOS):

| | Stable, `both` | mold | wild | Nightly | Nightly, Cranelift | Nightly, `-Zthreads=8` |
|---|---|---|---|---|---|---|
| atuin, leaf crate | 4.3s | 4.2s | 3.5s | 3.7s | 3.5s | 3.4s |
| atuin, `atuin-common` | 13.4s | 13.4s | 12.4s | 12.1s | 12.3s | 11.3s |
| ruff, leaf crate | 3.1s | 3.7s | 2.8s | 2.7s | 3.0s | 2.5s |
| ruff, `ruff_text_size` | 22.4s | 23.9s | 21.4s | 21.3s | 22.1s | 18.7s |
| M2, atuin, leaf crate | — | — | — | 8.9s / 8.5s | 16.8s | 7.9s |
| M2, atuin, `atuin-common` | — | — | — | 24.8s / 26.9s | 42.0s | 26.3s |
| Unit tests, leaf-crate turns (atuin / ruff, Linux) | 19.2 / 8.8s | 19.4s / — | 19.3s / — | 19.2 / 8.0s | 26.5 / 35.8s, failing | 19.4 / 8.1s |

None of them is worth adding:

- **mold** saved nothing.
- **wild** saved about a second on atuin and less on ruff, where one of its two `ruff_text_size`
  turns was slower than stable's. Through clang it could not link one of ruff's test crates: a C
  dependency compiled by GCC with link-time optimisation needs GCC's linker plugin, which the
  default link passes and this one does not.
- **The nightly alone** is 5–14% faster than 1.98; stable gets that by itself.
- **`-Zthreads=8`** adds 7–12% to the nightly on Linux with 32 cores and one build at a time, and
  nothing beyond noise on the 8-core M2, where it took the load to 80 during the warm build.
  With several agents building at once, the cores it uses are taken already (unmeasured). It
  needs the nightly toolchain for every build, the golden's included, and a nightly gate can pass
  code that stable rejects (a library API stabilised on nightly first).
- **Cranelift** was within 5% of the nightly on Linux and 70–90% slower on the M2, and it changes
  what tests do (`backends.sh`: `backends-atuin-linux1.txt`, `backends-ruff-linux1.txt`,
  `backends-atuin-mac1.txt`). Tests that catch a panic fail, every run: 12 of atuin's on the M2,
  3 on Linux, 2 of ruff's; two more of atuin's hang until killed. On Linux 1,188 of atuin's
  7,116 tests and 79 of ruff's 6,740 report no result at all. Compiling dependencies with
  Cranelift too turns the hangs into failures and fixes nothing. With LLVM every test passes but
  one flaky atuin test on the M2 (`test_try_open_contention`, a lock contention test). A gate
  that fails for the compiler's sake sends agents back for nothing.

So there is nothing for a mode that searches for the fastest settings to find: the one setting
that wins anywhere, `-Zthreads`, is a choice of toolchain, not of settings.

### Failed tests first

`bench/feedback.sh` on atuin on an M2: a test the agent broke, settled; then an unrelated change
with the test still failing, settled again; then the test fixed. The first settle ran the whole
suite (575–622s); the second ran only the test that had failed and reported it in 52s (73s before
it left out doctests, which naming tests still built and ran); the third, with it fixed, the
whole suite again (`bench/feedback-atuin-mac4.txt`, `mac1` and `mac2` before). An agent that
has not fixed what it broke hears so in a tenth of the time.

On Cargo 1.98, main has two clippy diagnostics in ruff (which pins Rust 1.99) and in nushell;
`check` leaves them out and says so. Windows is unmeasured.

### Real agents

`bench/agents.sh` on atuin (`tasks-atuin.txt`: six small tasks, adding tests or a function,
each saying "make sure the tests and clippy pass"), each agent in its own worktree, nobody telling
them about Agent Hearth. Against the fair baseline (`FAIR=1`): main's warm target copied into
each worktree and a plain Stop hook that runs `cargo clippy --workspace --all-targets -- -D
warnings` and `cargo test --workspace` and sends the agent back with the output's tail, at most
three times. Codex is GPT-6-Luna, Claude Code is Haiku 5.5; Linux, 32 cores.

| | Time per agent | Tokens per agent | Stop blocks | Median own build | Suite at the end |
|---|---|---|---|---|---|
| 6 Codex, fair (2 rounds) | 772–1424s | 1.7–8.3M | 18, 18 | 33.5s | failing for all 12 |
| 6 Codex, Agent Hearth (2 rounds) | 147–292s | 0.28–1.2M | 0, 0 | 10.6s, 24.8s | settle passed for 11 of 12 |
| 6 Claude Code, fair | 852–2243s | 2.2–7.4M | 16 | 34.1s | failing for all 6 |
| 6 Claude Code, Agent Hearth | 148–432s | 0.18–0.49M | 0 | 32.4s | settle passed for 4 of 6 |
| 10 Codex, fair | 1264–2118s | 2.5–11.7M | 30 | 34.0s | failing for all 10 |
| 10 Codex, Agent Hearth | 132–397s | 0.25–1.0M | 0 | 23.2s | settle passed for 10 of 10 |

Most of the difference is the gate, not the build. On this machine main fails 69–70 of atuin's
tests, end-to-end terminal tests among them (apparently running an older `atuin` from `PATH`); the
plain hook, whose `cargo test` stops at the first failing test binary, showed six at a time and sent
every agent back for them until its three tries ran out, and agents spent their time and tokens on
them, editing the test harness, rebuilding, one reformatting 92 files with stable rustfmt, and
several deciding the hook ran in main's checkout and editing it (`copied-paths.sh` found nothing in
the copy pointing there; Codex reads its hooks from main's `.codex/`). Agent Hearth's settle knew
them as main's. The rest is the build: the plain hook's every turn and the agents' own builds in a
copied target took a median 34s, Agent Hearth's 11–32s. The settles that failed were timing tests
under six settles at once, retried while the others still ran; since retries wait for a quieter
machine, all ten passed with ten agents. Agents spent from a sixth to nearly all of their time
waiting on their own builds and had nothing else to do in it, so building asynchronously (`PLAN.md`,
"Tried and dropped") would not have helped on these tasks.

## Shape

One Rust binary and no daemon: each command does its work, state lives in files, coordination
uses file locks (std `File::lock`, one API on every platform).

```
~/.local/state/agent-hearth/<repo-id>/
  goldens/current                    pointer file: id of the golden new worktrees seed from
  goldens/<golden-id>/{src,target}   warm checkout of main; id = commit + Cargo.lock + toolchain + flags
  targets/<worktree-id>/             private build directory per worktree (reflink clone of a golden)
  worktrees/<worktree-id>.json       worktree path, the golden that seeded it, last verdicts
  verdicts/<fingerprint>.json        check and settle results keyed by content fingerprint
  settles/<worktree-id>.{json,log,lock}  last passing test count, background log, run lock
  sessions/<session-id>.json         Stop-hook attempts with identical problems
  builds.jsonl                       one line per agent cargo run: its arguments (the golden builds the packages built alone)
  warms/<worktree-id>.*              the running warm, its queue, the agent's recent build commands
  slots/<n>/, slots/<n>.json, slots/lock  slot pool (no pin): worktrees at fixed paths, who holds each
~/.local/state/agent-hearth/settle-places/<n>.lock  held by each running settle, whatever its repository
~/.cache/agent-hearth/<repo-id>/pin/{src,target}   Linux only: empty mount points, the one fixed path
```

Everything an agent's own commands write is under the state directory, so an agent sandbox that
lets it write there runs them all (see "Agent sandboxes").

The paths shown are Linux's (XDG); macOS and Windows use their platform's state and cache
directories. `AGENT_HEARTH_STATE_DIR` and `AGENT_HEARTH_CACHE_DIR` replace them; `settle --bg`
passes them to its detached child, which runs at low CPU priority (`nice -n 10`; below normal on
Windows) and waits for a place among the machine's settles (see "Settles at once"): ten agents'
full test suites at once ran out of memory on 62 GB. `<repo-id>` identifies the repository, not the worktree, so all
worktrees of a repository share goldens and the pin.

A golden refresh writes a new golden directory, then atomically replaces `goldens/current`.
`gc` deletes goldens holding `goldens/refresh.lock` and `goldens/gc.lock`; every adopt holds
`gc.lock` shared while it reads a golden, so no golden disappears mid-clone.
Replacing a file works the same everywhere; renaming over a non-empty directory does not work on
Windows, and symlinks there need Developer Mode. Old goldens stay until `gc` finds them unused.

### Running cargo

`agent-hearth cargo` runs cargo with the worktree's own build directory as `CARGO_TARGET_DIR` and
`CARGO_INCREMENTAL=1`. Cargo 1.99 turns incremental builds off when `CI` is set, and agent
harnesses often set it. Gates also set `CARGO_BUILD_WARNINGS=deny`, not `-D warnings` in
`RUSTFLAGS`, because flags are part of the fingerprint and changing them rebuilds everything. The
exit code passes through. Where the platform allows, the build runs at the fixed path (below).

Cargo's `build.build-dir` setting separates intermediate artifacts, the part that stays warm, from
final artifacts. Seeding and pointing `build-dir` rather than `CARGO_TARGET_DIR` may fit Cargo
1.100's new layout better; see "Open" in `PLAN.md`.

### Building at one path

A checkout's path is part of the compiled state (finding 2), so every build of a repository
should happen at one path. There are three strategies; `doctor` reports which one a machine gets.

**Pin (Linux, measured).** Every cargo invocation runs as

```
bwrap --dev-bind / / \
  --bind <worktree> <pin>/src --bind <targets/id> <pin>/target \
  --chdir <pin>/src/<relative cwd> \
  cargo …
```

with `CARGO_TARGET_DIR=<pin>/target`. Paths under `<pin>/src` in any output are mapped back to the
real worktree before anyone sees them. Each bwrap call has its own mount namespace, so ten agents
can all see "their" checkout at the same path at once. This was measured.

Unprivileged user namespaces can be blocked even on Linux: Ubuntu 24.04 and later restrict them
through AppArmor, and containers and agent sandboxes often disable them. Each process probes
bwrap once; when it fails, `spawn` uses the slot pool, other worktrees build without a fixed
path, `agent-hearth cargo` says so on stderr, and `doctor` fails its path check.

**Slot pool (built; wherever there is no pin).** macOS and Windows have no per-process mount
namespaces: a mount is visible to every process, so only one worktree at a time could sit at a
shared path. Instead, keep N worktrees at fixed paths ("slots"), each with a build directory that
has only ever built at that slot's path. A new agent gets a free slot, reset to its starting
revision; cargo rebuilds what changed at the same path, so incremental state stays valid and no
path mapping is needed. The costs: Agent Hearth must choose where worktrees live (Claude Code's
`WorktreeCreate` hook allows that; T3 picks its own paths), and moving a slot to a new main costs
an incremental build rather than a reflink clone. `spawn` claims the lowest free slot under
`slots/` (or makes the next one), moves it to the revision (jj: `workspace rename`, then `new`;
git: `switch --create`, refused while the slot has uncommitted changes) and adopts it, which
keeps a reused slot's build directory. `WorktreeRemove` frees the slot (jj renames the workspace
to `slot-<n>`, git detaches) and keeps its files. A new slot's first agent pays the path once.
Free slots follow main: once `golden refresh` has made a new golden current, it moves each free
slot to the golden's commit and runs the golden's builds there, holding `slots/<n>.lock` so that
`spawn` passes the slot by (a crash frees the lock). Otherwise the next agent in a reused slot
first builds main's changes at that path. Each slot holds a whole build directory of its own, so
slots fill a small disk quickly. A slot that fails to warm is reported and skipped; the golden
refresh still succeeds. On atuin on an M2 Mac mini (`bench/slots-atuin-mac1.txt`): a new slot's
first edit 96.9s, the next agents' in the reused slot 37.9s and 25.5s; second edits 26-31s.

**None.** Seed plus mtime sync only: the `sync` variant of `bench/public.sh` on Linux.
Unmeasured on macOS and Windows.

### Seeding a worktree (`adopt`)

1. Pick the golden whose `Cargo.lock`, toolchain and flags match and whose commit is the nearest
   ancestor of the worktree's revision. If none matches, seed from the nearest and let cargo
   rebuild the difference. Goldens are rebuilt fresh, never accumulated, so they don't bloat.
2. Reflink-clone the golden's target in-process, file by file: FICLONE on btrfs/XFS, clonefile on
   APFS, block cloning (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`) on ReFS and Dev Drive. Set each
   clone's mtime from its source with `File::set_modified`. The benchmarks used
   `cp -a --reflink`, which is GNU-only; copying 5.5 GB took 0.7s on btrfs. Where the filesystem
   can't reflink (ext4, NTFS, tmpfs), never copy: the worktree starts cold, as with plain cargo,
   and says so once; a golden seeded from the previous one is built fresh instead.
3. For every file git does not ignore (the files a fingerprint hashes): when its content is
   identical to the golden's copy, set its mtime to the golden's; otherwise set it to now, so
   cargo rebuilds exactly what differs. Never copy content. A differing file must not keep an
   mtime older than the golden's build: cargo then takes the golden's outputs, compiled from
   main's copy, for fresh. Adopting a worktree whose edits predated the golden passed broken
   tests, a full settle and the Stop hook that way, and so did `cargo clean` reseeding from a later
   golden (a test in `src/adopt.rs` covers it). Fresh worktrees have no differing files, so this
   costs them nothing. Directories get the golden's mtime when they hold the same entries, and
   now otherwise: cargo counts a directory's mtime when a build script names it in
   `rerun-if-changed` (atuin's `migrations`), and before this every new atuin worktree reran four
   build scripts and rebuilt 50 of 756 artifacts.

### Verdicts

A fingerprint hashes the contents of every file git does not ignore (tracked or not; listed
through an index that does not exist, which works the same in jj workspaces and git worktrees),
`Cargo.lock`, toolchain, flags and gate spec (blake3). There is no mtime+size cache yet; a
cached check's time is in `bench/public-*.txt`, against a 100 ms target. A verdict counts only if the fingerprint is the same at the
start and the end of the run. Otherwise the result is "superseded" (exit 3).

## Commands

Agent-facing first. The default output is terse text with one line per diagnostic; `--json` gives
structured output.

| Command | Does |
|---|---|
| `agent-hearth golden refresh [--rev main]` | Build a revision at the fixed path in a golden, seeded from the current one (`clippy --workspace --all-targets`, `test --workspace --no-run`, and the same with `-p` for each member agents have built alone; a member that does not build alone is skipped with a warning), then point `goldens/current` at it and warm the free slots there. When main has moved since the current golden, the `UserPromptSubmit` hook and `spawn` start a refresh in the background (log: `goldens/refresh.log`), so it follows main without anyone running it. The builds keep going past errors: errors in main's own code (ripgrep's benches need nightly, so `--all-targets` fails on stable) are recorded in its baseline and the golden holds the rest; a failure that names no file (a build script, a missing library) fails the refresh and leaves the current golden |
| `agent-hearth spawn <name> [--rev]` | `jj workspace add` (or `git worktree add`), then adopt: register it, seed its build directory from the golden and sync mtimes. A worktree made any other way is adopted by its first `agent-hearth cargo` |
| `agent-hearth cargo <args…>` | Run any cargo command at the fixed path where the platform has one, with paths mapped back. A plain `cargo clean` instead deletes the build directory and seeds it again from the golden |
| `agent-hearth check` | Turn-end gate: `cargo fmt`, `clippy --workspace --all-targets` JSON (plus `clippy --fix` and a rerun when something is machine-fixable) → deduplicated one-line diagnostics; stored by fingerprint. Measured against main: each golden records whether main is formatted (`cargo fmt --all --check`), has fixable diagnostics, and which diagnostics it has (by file, code and message, without line). `check` formats only when main was formatted, fixes only when main had nothing to fix, and reports only diagnostics main does not have, saying how many it left out; those alone pass |
| `agent-hearth settle [--bg]` | Full gate (all tests and doctests) for the current fingerprint, measured against main: after each golden refresh, the golden's own suite runs in the background (`golden settle`, admitted like a settle), and the tests that fail twice there, and compile errors, are main's; a settle of a worktree seeded from that golden leaves those out, says how many, and passes when they are all that fails (atuin's end-to-end terminal tests fail on this machine, so every settle did, and the Stop hook sent agents back for them). First the tests that failed in the worktree's last five settles run on their own (most often failing first, at most 32, in test targets only unless they are doctests): if one still fails, that is the verdict, marked partial, and the agent hears in a tenth of the suite's time (52s against 576s on atuin on an M2); otherwise the full suite runs in its own order, so order-dependent tests behave as before. Failed tests run once more, one at a time, after waiting up to three minutes for other settles to finish; those that pass are reported flaky, not failed (timing tests fail under load: with six settles at once, every one of atuin's had 6–10 terminal tests fail, and retries run while the others still ran failed again, in parallel or one at a time; waiting on the machine's load instead held back a failed-first retry on an M2, 88s against 52s, its own build having loaded the machine). A test binary killed by a signal is a failure (`CRASHED -p app --test ui: signal 11 …`), its target also run once more |
| `agent-hearth warm` | Build the worktree's code in the background, output discarded: the agent's three most recent distinct build commands (`test` with `--no-run` and without test name filters, `run` as `build`) and the Stop hook's clippy, each with `--keep-going` (`--no-fail-fast` for `test`), so every crate that compiles is built even mid-edit. Started detached and niced by the PostToolUse hook after each edit, and by the agent's own `agent-hearth cargo` when it ends, for the command it runs next with no edit between (`check` then `test`); one runs and at most one waits per worktree, and a new one stops one still building older code. An agent's `cargo`, `check` or `settle` holds a shared lock: it waits for a warm building the same command on the same code to finish it, and stops (SIGINT to its process group) any other, or any on a machine whose load is at least half its cores, and keeps looking while it waits. `AGENT_HEARTH_WARM=off` turns it off. Unix only |
| `agent-hearth setup [--claude\|--codex] [--no-golden] [--build-settings]` | `--build-settings` appends the measured dev-profile settings (see "Build settings") to `Cargo.toml` unless it has a `[profile.dev]` of its own; they take effect once committed to main. Merge the hooks into `.claude/settings.json` and `.codex/hooks.json` (other settings stay; an earlier setup's hooks are pointed at this binary), refresh the golden of main, and print what is left: sandbox settings with the state directory's absolute path, the Claude Code permission rule, committing `.codex/hooks.json`. An agent in its sandbox cannot do this (Codex's keeps even the repository's `.codex/` read-only; `bench/onboard-codex-linux1.txt`), so `docs/agents.md` has it ask the user to |
| `agent-hearth doctor` | Say what Agent Hearth does on this machine (warm worktrees at a fixed path and the gate; seeded worktrees and the gate; or the gate only, where nothing clones), list every worktree of the repository with its verdicts, settles in flight and whether its golden is current, and report the fixed-path strategy (bwrap and user namespaces on Linux), reflink support and toolchain; re-measure whether seeding and pinning still pay off. Hints, never failures: a dev profile (in `Cargo.toml` or `.cargo/config.toml`) with full debuginfo for the workspace, or dependencies with debuginfo or unoptimised (see "Build settings") |
| `agent-hearth gc [--keep 2]` | Drop build directories of vanished worktrees, all goldens but the current one and the newest others up to `--keep`, and month-old verdicts; report real disk use (`btrfs filesystem du`: exclusive plus shared extents counted once) |

Exit codes: 0 pass, 1 diagnostics in the code, 2 tool error, 3 superseded (the code changed
during the run).

Example agent turn:

```
$ agent-hearth check
crates/core/src/lib.rs:43:32 E0308 mismatched types: expected `u32`, found `&str`
$ agent-hearth check
ok  clippy 1.8s
$ agent-hearth settle
ok  412 tests 38.2s
```

## Integrations

- **Claude Code** (`agent-hearth hook claude-*`). `WorktreeCreate` replaces Claude Code's own
  worktree creation, so it runs `spawn` (beside the current worktree, or in a slot without the
  pin; from `HEAD` or jj's `@-`) and prints the path; `WorktreeRemove` forgets or removes it and
  its build directory, never a git branch, or frees its slot. `PreToolUse(Bash)` rewrites `cargo` in command position (also after `env`, `time`, assignments and `devenv shell --`) to `agent-hearth cargo`
  through `updatedInput`, without a permission decision, so the user's permissions still apply.
  `Stop` runs `agent-hearth check` and blocks with the diagnostics, at most three times in a row
  with identical ones. Clean, it settles the code, and waits for the verdict, blocking on its
  failures, when the repository's five most recent settles took at most 60s, and up to 60s when
  it has none yet (without that, the first agents of a fresh repository stopped unchecked);
  longer suites run in the background and reach the agent at its next prompt. Waiting is a
  background settle plus a wait, so a hook killed by its timeout leaves the settle running.
  `UserPromptSubmit` tells the agent once, at its next prompt, about the last settle of its
  worktree if it failed unreported, even after the code changed: a one-shot agent broke two
  tests it never ran, and no `Stop` came after the settle.
  Each settle that reaches a verdict leaves `settles/<id>.last.json`: its verdict, duration, and
  whether the agent was told.
  A failing settle where a loopback or Unix socket cannot be bound (an agent's sandbox, when the
  agent runs `agent-hearth settle` itself) keeps no verdict and says so: the Stop hook reused one and sent
  two agents back six times over network tests the sandbox broke (`bench/sandbox-settle-linux1.txt`;
  the same on macOS, `sandbox-settle-mac1.txt`).
- **Codex** (`agent-hearth hook codex-*`). Codex's hooks speak Claude Code's protocol, but its
  PreToolUse ignores `updatedInput`, so `codex-pre-tool-use` denies `cargo` with the
  `agent-hearth cargo` command to run instead; in early runs with real agents (private workspaces)
  every agent ran the named command
  and used it from then on. `codex-stop` is `claude-stop` and `codex-user-prompt-submit` is `claude-user-prompt-submit`
  (Codex delivers its `additionalContext`); Codex loads a project's hooks only when the project
  is trusted. In a git worktree (which every jj
  workspace of a colocated repository is), Codex 0.163 reads the hooks from the main checkout's
  `.codex/`, and only when the worktree has a `.codex/` too.
- **Pi.** `finalChecks: ["agent-hearth check", "agent-hearth settle"]`.
- **T3** and other tools that make their own worktrees: nothing to add; the first
  `agent-hearth cargo` in a worktree adopts it.
- **rust-analyzer** keeps its own target (`rust-analyzer.cargo.targetDir: true`). Agent Hearth
  never touches it.

## Settles at once

Settles one at a time left a 32-core machine mostly idle and reached the last of ten agents many
minutes after it stopped; several at once with no regard to memory filled it next to agents'
builds. Memory, not cores, is the limit. Each settle holds one of sixteen lock files in
`settle-places/`, whatever its repository; its end or its death frees the place. The first place
is always free to take, so the worst case is one at a time; another only while available memory
(`MemAvailable`, narrowed by the `memory.max` of this process's cgroup and its ancestors, as in a
container or systemd scope; free, inactive and speculative pages on macOS) covers a settle, taken
as a sixteenth of the machine, plus an eighth left to agents' builds. atuin's settles peaked at
2–4 GB each with ten at once on 62 GB, and under 1 GB on a 16 GB M2. With ten agents on Linux the
places matched the daemon: every settle in after 781s against 792s, all ten passing
(`bench/agents-atuin-codex-hearth-n10-places.txt`). A first version asked for 4 GB plus a quarter
of the machine, which a 16 GB Mac never had free: three settles ran one at a time, 1736s against
694s (`bench/feedback-atuin-mac3.txt`); sized by the machine, they ran together in 683s
(`bench/feedback-atuin-mac4.txt`). Settles build with an eighth of the
cores, at least 4 (`--jobs`): most of one settle's peak was its test binaries linking at once;
with 4 jobs it used a third of the memory and took 12% longer.

Before this, a daemon admitted settles by measured memory peaks (its admissions with ten agents:
`bench/agents-atuin-codex-hearth-n10.txt`). It was 600 lines with its socket and sampling, and
three settles at once on an M2 took 694s with it against about 1720s one at a time
(`bench/feedback-atuin-mac2.txt`).

**Not built, and why** (private workspaces):

- A shared jobserver saved neither time nor memory for ten lockstep turns.
- Speculative checks whose results reach the agent: results of half-finished edits flood the
  transcript. `warm` builds instead, and shows nothing.

## Prior art

Every part of Agent Hearth exists elsewhere; nothing found combines them, and none of these
builds at one fixed path, which is where Agent Hearth's remaining margin comes from. Surveyed in
an audit (2026-10-20); only the copies are measured on public repositories.

- **Warm worktrees by copying.** Worktrunk (`wt step copy-ignored`), Orch's git-cow and lane
  reflink-copy a warm `target/` into a new worktree, keeping mtimes (`cp -a`); cargo-mutants does
  the same for its parallel workers. One agent's first edit this way: 93–167s, against 20–56s
  (`bench/public-*.txt`).
- **Content-matched mtimes.** zed-industries' cargo-mtime restores a file's mtime when its sha256
  is unchanged, for sandboxed builds: the mtime sync. git-restore-mtime sets mtimes from commits.
  CI caches (Swatinem/rust-cache, crane, cargo-chef) document the same path and mtime limits.
- **Compiler caches across worktrees.** Kache caches workspace libraries, executables and build
  scripts too, and seeds new targets with dependencies (never workspace members, whose freshness
  is left to their mtimes); its incremental import across worktrees needs nightly. cargo-reapi
  and zccache work similarly. sccache needs incremental off. In the audit, on a private
  workspace, Kache's first edit took about twice Agent Hearth's and sccache's about eight times;
  none is measured on a public repository.
- **Path independence.** Stable `--remap-path-prefix` with the same mappings for every known
  checkout path removed the new-path cost for one package, but not for the full workspace with
  private targets (audit, private workspace). New paths change the flags, which rebuilds
  everything.
- **cargo-hauler** (Node, created 2026-09): brokers and coalesces cargo runs, with a global
  jobserver and resource admission, agent hooks whose Stop waits for pending builds, and Kache
  integration. It does not seed or pin build directories. The reference for hooks and the
  jobserver.
- **Gates on agents' turns.** Slopbrake (Python and TypeScript) and MaruCheck block Claude Code's
  Stop until checks pass, with a retry limit; Slopbrake keeps a last-green ref. Bazel, Buck2 and
  Turborepo cache test results by declared inputs: the verdict cache, with a stricter input model.
- **bacon** (3.26, headless mode, exports): the reference for watch behaviour; a candidate watcher.
- **Upstream Cargo.**
  - The cross-workspace cache (2026 goal) starts with immutable crates and leaves out workspace
    crates, which are the part seeding covers.
  - Relative fingerprint paths (cargo#17573, 1.101) and `build.fingerprint = "content"`
    (cargo#17382, nightly) may make the mtime sync unnecessary; `rerun-if-changed` still uses
    mtimes. Neither changes the compiler's working directory, which the pin keeps fixed.
  - `-Zfine-grain-locking` is paused after a deadlock (cargo#17508).
  - The new build-dir layout ships in 1.100 on 2026-11-12. A fixed `build-dir` alone did not
    remove the new-path cost (audit).

## Risks

- **The pin is Linux-only** (user namespaces), and some Linux machines block it. Elsewhere the
  slot pool stands in (a reused slot's first edit 26–38s on atuin on an M2, against 97s for a
  new one; see "Building at one path"); without
  either, seed plus mtime sync gave 67–159s for a first edit on Linux, against 20–56s pinned.
- **Seeding needs a reflink-capable filesystem:** btrfs or XFS on Linux (not ext4), APFS on
  macOS, ReFS or Dev Drive on Windows (not NTFS). Elsewhere worktrees build cold, with a
  notice, and `doctor` says to put the state directory on such a filesystem.
- **Windows specifics are unmeasured.** Defender scanning build directories is expected to be a
  large cost outside a Dev Drive in performance mode. Windows file locks are mandatory, and a
  running test binary can't be replaced or deleted, so golden swaps and `gc` must tolerate files
  in use.
- **Agent sandboxes need one setting.** Codex's and Claude Code's sandboxes allow writes to the
  working directory and a temp directory only, so every command failed in them
  (`bench/sandbox-commands-*.txt`). With the state directory writable, everything ran, on macOS
  too (Codex); the pin's bwrap nests inside both on Linux. Build directories stay in the state directory rather than the worktree's
  `target/`: reflinks need them on the golden's filesystem, which the state directory is and a
  worktree need not be.
- **Per-worktree environments may rebuild.** Each worktree's devenv shell has variables that
  contain its own path; build scripts that rerun on them would rebuild in every new worktree.
  Unmeasured on a public repository.
- **mtime sync makes identical files look older than they are.** A build directory other than the
  worktree's own (a plain `target/`, rust-analyzer's) that compiled different content in between
  could then miss a change (reproduced in the audit). `adopt` runs once,
  before the worktree's own build directory exists, and never again for an adopted worktree; it
  does not check for other build directories.
- **Each main commit needs a golden.** Built with every member alone too, a golden took
  867–1815s and 67–118 GiB, against 165–191s for one warm build of the workspace
  (`bench/public-*.txt`). Now it builds members alone only when agents have, and is seeded from
  the previous golden: fresh, it costs one warm build (163–195s, 28–51 GiB), which a copy of a
  warm target needs too; seeded, 37s after an ordinary atuin commit and 97–107s after a release
  or a version bump, which rebuilds every crate (`bench/golden-*.txt`). Goldens stay until `gc`
  runs; a seeded one shares most of its extents with the previous one. Each worktree adds
  14–32 GiB of its own after its first full turn. A second edit in the same worktree saves
  nothing.
- **A main that is not clean under this toolchain.** `check` used to format and fix the whole
  workspace and report every diagnostic. nushell 0.116.1 has fixable clippy warnings on Cargo 1.98
  (its first check took 140s, `bench/public-nushell-linux1.txt`), ruff has two warnings, and atuin
  formats with nightly rustfmt: Codex agents' worktrees each ended with about 90 reformatted
  files (`bench/agents-atuin-codex-warm-on-1.txt`). Each golden now records main's baseline, and
  `check` measures against it (see the command table). Left over: on such a main, `check` does
  not format the agent's own changes either.
- **Copied incremental directories can ICE** (rust#79062). On an ICE, delete that crate's
  incremental directory and retry once.
- **Cargo 1.100 changes the build-dir layout.** Never hard-code paths inside a build directory; read
  artifacts from cargo's JSON messages, and key goldens by toolchain.
- **Upstream may make tricks redundant.** `doctor` re-measures rather than assumes.
- **Some benefits are unmeasured:** jobserver priority, speculative builds, and how real agents
  (which build less in lockstep than the benchmark) contend.

## Known gaps

Reproduced in an audit (2026-10-20), not fixed. Each lets a verdict or the Stop hook say green,
or lets an agent stop, when plain cargo would fail:

- **The fingerprint misses inputs.** Git-ignored files tests or build scripts read (fixtures,
  `.env`, generated code), environment variables tests read, `CARGO_PROFILE_*`, and crates in
  nested repositories or submodules. A cached pass then outlives the change, and verdicts are
  shared by every worktree of the repository.
- **Configuration and path dependencies above the worktree, at the pin** (fixed: cargo looked for
  `.cargo/config.toml` upward from the pin and resolved `path = "../x"` from it, so a config
  above the worktree was ignored and such dependencies did not build). They are mounted over the
  pin's parents now: at most two configs, and dependencies that point no further out than the
  cache directory (`../x`, `../../x`); a worktree with one further out builds at its own path.
  Goldens get those of the checkout they are refreshed from.
- **The Stop hook fails open without a verdict.** A settle whose tests write into the worktree
  (insta's `.snap.new`, logs) is superseded every time and stores nothing, so the agent may stop
 . A tool error exits 2, which Claude Code treats as non-blocking when cargo is missing from
  the hook's environment.
- **Flaky retries hide test interference.** A test that fails alongside others and passes alone
  is reported flaky and the settle passes.
- **WorktreeRemove deletes ignored files** with the worktree (fixed: it used to delete uncommitted
  work too; now git refuses a worktree with changes or untracked files, and with jj, one with
  files jj did not snapshot).
- **Program output is path-rewritten too.** Whole paths only (fixed: a plain substring replace
  mangled a sibling worktree's paths, `agent1` in `agent10`), but a test's own output that names
  the pin's path is shown with the worktree's.
