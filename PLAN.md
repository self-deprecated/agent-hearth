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
   for the best copy of main's target (`bench/public-*-linux2.txt`). The golden follows main by
   itself, 37s after an ordinary atuin commit (`bench/golden-atuin-linux2.txt`).
2. **The gate, against main.** `check` (fmt, clippy) and `settle` (the full suite, recent failures
   first, flaky tests retried on a quieter machine) leave out what main already has, through the
   Stop and UserPromptSubmit hooks. With real agents on atuin it was most of the gain: 2–7
   minutes per agent against 13–37 with a plain Stop hook, and a fifth to a tenth of the tokens
   ("Real agents" in `docs/design.md`).
3. **Onboarding.** `setup` (hooks for Claude Code and Codex, a golden, `--build-settings`),
   `doctor` (what works on this machine, every worktree's verdicts) and `gc`.

Background warming (`warm`, the PostToolUse hook) stays too, though it hides only 7–18% of real
agents' waiting: they build straight after they edit (`bench/agent-gaps-linux1.txt`).

## Platforms

| | Seed (reflink clone) | mtime sync | Fixed path |
|---|---|---|---|
| Linux | btrfs or XFS; not ext4 | Yes | bwrap pin (measured) |
| macOS | APFS `clonefile` | Yes | Slot pool (measured on an M2) |
| Windows | ReFS or Dev Drive block cloning; not NTFS | Yes | Slot pool (unmeasured) |

Without reflinks, worktrees build cold and Agent Hearth is the gate only. Keep platform-specific
code to cloning, the fixed-path strategy and path normalisation. Never quote a Linux number for
another platform; say it is unmeasured. A virtualised Windows VM can show correctness, not speed.

## Dates

- **2026-11-12:** Cargo 1.100 ships the new build-dir layout. Re-run `bench/` and `doctor` on it.
- **2026-11-19:** Rust Aarhus talk, "Self-Healing Rust".

## Tried and dropped

- **`land`**, a merge queue for jj workspaces: it worked (on ripgrep), but it is not speed, and it
  doubled what a newcomer had to learn.
- **`test --affected`** (nextest on the reverse dependencies of what changed): never measured on
  a public repository, and agents run `cargo test`, which the hook routes anyway.
- **The settle daemon** (admission by measured memory peaks, about 600 lines): lock-file places
  that admit by available memory do the same job (see "Settles at once" in `docs/design.md`).
- **`stats`**, where agents' build time went: experiment tooling. The build log stays, for the
  golden's `-p` variants.
- **A shared jobserver** saved neither time nor memory for ten lockstep turns (private
  workspaces).
- **Answering repeats from a cache:** the same command on unchanged code was 1% of agents'
  waiting.
- **Async builds through a skill:** agents had nothing else to do while they waited.
- **Other linkers, Cranelift, the parallel front end, and a mode that searches for the fastest
  settings:** mold saved nothing, wild about a second on atuin; Cranelift was no faster on Linux,
  70–90% slower on an M2, and broke tests that catch panics; `-Zthreads` saved 7–12% on Linux
  only, on a nightly toolchain ("Linkers, Cranelift and the parallel front end" in
  `docs/design.md`). The only win is a choice of toolchain, so a search has nothing to find.
- **Speculative checks whose results reach the agent:** results of half-finished edits flood the
  transcript; `warm` builds silently instead.

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
- **Filesystems without reflinks** (ext4, NTFS): allow an explicit opt-in full copy? Most Windows
  drives are NTFS.
- **cargo-hauler.** Integrate with it (it brokers cargo runs across agents with a global
  jobserver and Kache, but neither seeds nor pins) or stay separate?
