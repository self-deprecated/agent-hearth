# Agent Hearth plan

Keep this current when scope changes. How it works is in `docs/design.md`; the evidence is in
`bench/`.

## Goal

Coding agents, each in its own worktree of the same Rust workspace, get their first feedback in
seconds rather than minutes, later feedback at single-agent speed (the CPU allowing), and are sent
back only for what they broke, in diagnostics short enough to feed straight back to them.

## What it is

Three parts, each kept because a measurement earned it:

1. **Warm worktrees.** A golden (a warm build of main) per main commit, each seeded from the one
   before; every worktree's build directory cloned from it, with main's mtimes on unchanged
   files; every build at one fixed path (bubblewrap on Linux, a pool of fixed slots elsewhere).
   One agent's first edit in a new worktree: 20–56s on atuin, nushell and ruff, against 67–159s
   for the best copy of main's target (`bench/public-*-linux2.txt`); on ext4, where nothing
   clones, what rustc's own records name as its outputs is hardlinked instead, checked against
   the golden's record of those files at every adopt (on an 8 vCPU VM 74s on atuin, 36s on ruff,
   against 224s and 408s for a full copy, `bench/ext4-*-tay{1,2}.txt`; 40s on ruff after the review's fixes, another day, `-tay3.txt`). The golden
   follows main by itself, 37s after an ordinary atuin commit (`bench/golden-atuin-linux2.txt`),
   and when main fast-forwards to a tree a worktree built, it starts from that worktree's build
   directory: 12s against 81s from the previous golden on the VM (`bench/promote-atuin-tay1.txt`).
2. **The gate, against main.** `check` (fmt, clippy) and `settle` (the full suite, recent failures
   first, flaky tests retried on a quieter machine) leave out what main already has, through the
   Stop and UserPromptSubmit hooks. With real agents on atuin it was most of the gain: 2–7
   minutes per agent against 13–37 with a plain Stop hook, and a fifth to a tenth of the tokens
   ("Real agents" in `docs/design.md`). When only compiled Rust changed, a settle runs only the
   test binaries that differ from the golden's and keeps main's results for the rest: on an
   8 vCPU VM a failing test was reported in 33s against 662s on atuin, 34s against 125s on ruff
   (`bench/settle-reuse-*-tay1.txt`). A failing doctest is a failure, not flaky; a test target
   that does not compile on main is left out alone, and the rest of its package runs; a library
   that does not compile leaves out its package and dependents, and the settle says it is not a
   full pass.
3. **Onboarding.** `setup` (hooks for Claude Code and Codex, a golden, `--build-settings`,
   `--split-tests`: a test-only edit's build 0.43× on atuin, 0.067× on ruff, on the VM, and
   `--nightly`: every build on a pinned nightly with `-Zthreads`, once main's suite fails the same
   on it), `doctor` (what works on this machine, every worktree's verdicts, and hints on the
   workspace's structure from the golden's `--timings`: a slow build script on the critical path, a
   crate the rest waits for, crates compiled twice with other features) and `gc`.

Background warming (`warm`, the PostToolUse hook) stays too, though it hides only 7–18% of real
agents' waiting: they build straight after they edit (`bench/agent-gaps-linux1.txt`). Build places
(`AGENT_HEARTH_BUILD_PLACES`, off by default) let only so many agents' builds run at once where
agents outnumber cores: with 8 and 20 agents on 8 vCPUs and two places, each agent's median
build was 24–34% shorter and the last agent 1–22% later (`bench/places-atuin-n*-tay1.txt`). A
place is held while compiling, not while the program built runs, and waiters go in turn (both
unmeasured for speed).

## Platforms

| | Seed (reflink clone) | mtime sync | Fixed path |
|---|---|---|---|
| Linux | btrfs or XFS; on ext4, hardlinks (measured on an 8 vCPU VM) | Yes | bwrap pin (measured) |
| macOS | APFS `clonefile` | Yes | Slot pool (measured on an M2) |
| Windows | ReFS or Dev Drive block cloning; not NTFS | Yes | Slot pool (unmeasured) |

Without reflinks, on Unix, a worktree hardlinks the golden's compiled artifacts (as rustc's
dep-info names them) and copies the rest: on ext4 a new worktree's first edit took 74s on atuin and 36s on ruff, against 224s and
408s with a full copy of main's target and 392s and 483s with an empty one, on an 8 vCPU VM
(`bench/ext4-{atuin,ruff}-tay{1,2}.txt`). Where neither clones nor hardlinks work (NTFS, exFAT,
build directories on another filesystem), worktrees build cold and Agent Hearth is the gate only;
`doctor` finds which by trying. Keep platform-specific
code to cloning, the fixed-path strategy and path normalisation. Never quote a Linux number for
another platform; say it is unmeasured. A virtualised Windows VM can show correctness, not speed.

## Dates

- **2026-11-12:** Cargo 1.100 ships the new build-dir layout. Re-run `bench/` and `doctor` on it.
- **2026-11-19:** Rust Aarhus talk, "Self-Healing Rust".

## Tried and dropped

- **`land`**, a merge queue for jj workspaces: it worked (on ripgrep), but it is not speed, and it
  doubled what a newcomer had to learn.
- **`test --affected`** (nextest on the reverse dependencies of what changed): never measured on
  a public repository, and agents run `cargo test`, which the hook routes anyway. `settle`'s
  reuse of main's results differs: binary identity instead of a reverse-dependency guess, in
  `settle` only, and measured ("Reusing main's results" in `docs/design.md`).
- **The settle daemon** (admission by measured memory peaks, about 600 lines): lock-file places
  that admit by available memory do the same job (see "Settles at once" in `docs/design.md`).
- **`stats`**, where agents' build time went: experiment tooling. The build log stays, for the
  golden's `-p` variants.
- **A shared jobserver** saved neither time nor memory for ten lockstep turns (private
  workspaces).
- **Answering repeats from a cache:** the same command on unchanged code was 1% of agents'
  waiting.
- **Async builds through a skill:** agents had nothing else to do while they waited.
- **Other linkers, Cranelift, and a mode that searches for the fastest settings:** mold saved
  nothing, wild about a second on atuin; Cranelift was no faster on Linux, 70–90% slower on an M2,
  and broke tests that catch panics ("Linkers, Cranelift and the parallel front end" in
  `docs/design.md`). The one win, nightly with `-Zthreads`, is a choice of toolchain, so a search
  has nothing to find; `setup --nightly` offers it instead.
- **Speculative checks whose results reach the agent:** results of half-finished edits flood the
  transcript; `warm` builds silently instead.
- **Configure results kept between golden builds** (`CONFIG_SITE`): configure took them (98 of
  143 checks), but ruff's refresh after a `cc` bump was 405s against 420s, inside its spread
  (380–422s against 393–420s), as configure ran beside the rest of the rebuild on 8 vCPUs
  (`bench/configure-cache-ruff-tay1.txt`).
- **Padding for `--split-tests`**, to move tests modules that code follows: rustfmt collapses the
  padding, and only 6 modules on atuin and 2 on ruff would gain (`bench/split-padding-tay1.txt`).
- **An overlay seed** (bwrap, the golden as the lower layer) where there are no reflinks: it
  works with `--dev-bind` (the lab's attempt failed in Cargo's rustc probe because `--bind`
  mounted `/dev` nodev), but every reader outside the namespace would see only the upper layer;
  hardlinks keep a plain directory (`bench/overlay-probe-tay1.txt`).

## Open

1. **A repository whose main passes its suite**, to measure the build's share of the real-agent
   gain alone; on atuin main's own failures made the gate most of it.
2. **nushell**, where an edit costs 36–49s even in a warm target, so a new worktree has little to
   save and four at once gain nothing.
3. **Cargo 1.100 and 1.101:** does relative fingerprint pathing (cargo#17573) or
   `build.fingerprint = "content"` make mtime sync or pinning unnecessary, and should `adopt` seed
   `build.build-dir` instead of `CARGO_TARGET_DIR`?
4. **Windows**, on a real machine with a Dev Drive.
5. **Settles on another machine,** leaving local cores to the builds agents wait on.
6. **wild as an option of `setup --nightly`:** another 0.3–1.7s after an edit on top of
   `-Zthreads` (`bench/settings-*-linux3.txt`), but it needs clang and wild installed, and through
   clang it could not link a ruff test crate holding a GCC LTO object. Setup's try on main would
   catch that.
7. **The nightly on macOS:** 11–15% on the M2 against stable, measured with Xcode building beside
   it; `-Zthreads` is off below 16 cores, and the nightly alone there is unmeasured.
8. **Structure hints' blind spot.** `doctor` reads the golden's test build's `--timings` only,
   so build scripts that ran in its clippy build before (ruff's jemalloc configure) are never
   hinted; on ruff it gave none, on atuin ten (`bench/structure-*-tay1.txt`). Unmeasured on
   nushell.
9. **Hardlinked goldens and other places.** On ext4 a golden is built fresh; seeding it from the
   previous one by hardlinks, as worktrees are, is unmeasured, and so are build-place counts
   other than two, and what releasing a place when the program runs and taking turns save.
10. **A main that does not compile somewhere.** Leaving out only the targets that do not compile
   runs their packages alone (`-p`), which may rebuild dependencies with other features; how much
   that costs, and whether reuse could apply then, is unmeasured.
11. **Reuse when lines move or doctests are slow.** An edit that moves lines changes every program
   that links the crate, and every integration test reruns (ruff saved nothing); any doctest rerun
   runs all of them (most of ruff's 33s). Per-crate doctests without `-p` (which resolves features
   apart and rebuilt half of ruff), and whether line tables only (`--build-settings`) keep more
   binaries identical, are unmeasured. nushell is unmeasured for reuse and for `--split-tests`
   (its main's tests did not compile there).

## Open decisions

- **License and publishing.** `publish = false` until decided.
- **Stop without a verdict.** The Stop hook lets the agent stop when a settle is superseded, the
  tool errors or the hook times out. Block instead ("no verdict"), at the cost of agents whose
  tests write into the worktree being sent back until the tests stop doing so?
- **What a verdict covers.** The fingerprint misses ignored files, environment variables and
  `CARGO_PROFILE_*`, and verdicts are shared across worktrees. Hash what Cargo's dep-info names
  plus `CARGO_*` and `RUST*` variables, or keep verdicts per worktree?
- **Worktree placement.** Without the pin, `spawn` (and Claude Code's `WorktreeCreate`) puts
  worktrees in slots under the state directory; worktrees made elsewhere build at their own path.
  Should new slots be made ahead of the first agent?
- **NTFS, without reflinks:** hardlinks as on ext4 (unmeasured on Windows, where a running
  program's file cannot be replaced), or an explicit opt-in full copy? Most Windows drives are
  NTFS. ext4 is answered: hardlinks, measured ("Seeding without reflinks" in `docs/design.md`).
- **cargo-hauler.** Integrate with it (it brokers cargo runs across agents with a global
  jobserver and Kache, but neither seeds nor pins) or stay separate?
