# Set up Agent Hearth (instructions for a coding agent)

You are setting up Agent Hearth for the Rust repository in your working directory. Tell the user
what you changed, and what is left for them.

## 1. Check the machine

Run `agent-hearth doctor`. If `agent-hearth` is not on `PATH`, stop and ask the user to install
it (`cargo install --locked agent-hearth`). Tell the user its first line, `mode`, and every
`FAIL` line. Warm worktrees need Linux with user namespaces
(bubblewrap) and a filesystem with reflinks (btrfs or XFS), or macOS on APFS, where worktrees live
in a pool of fixed slots instead. Without reflinks it runs as the gate only, which still saved
real agents most of their time: their work is checked against main's own failures.

## 2. Run `agent-hearth setup`

In the repository, on any branch, run `agent-hearth setup`. It merges Agent Hearth's hooks into
`.claude/settings.json` and `.codex/hooks.json` (other settings and hooks stay; running it again
changes nothing), builds the golden of main (a full build, minutes on a large workspace, that new
worktrees are cloned from), and prints what is left, with the paths to use. `--claude` or
`--codex` writes only that agent's hooks; `--no-golden` leaves the golden for later. When main
moves, the golden follows by itself, in the background, at the next prompt.

**If your sandbox stops it** (Codex's keeps even the repository's `.codex/` read-only, and both
keep Agent Hearth's state directory out of reach), do not work around it: ask the user to run
`agent-hearth setup` in the repository themselves.

Then pass on every line under "Left to do" to the user: the sandbox settings with their absolute
paths, the Claude Code permission rule, and committing `.codex/hooks.json`. If setup prints
`hint` lines, ask the user whether to run `agent-hearth setup --build-settings`: it adds dev-profile
settings to `Cargo.toml` that made builds after an edit a third faster and tests twice as fast,
for humans too, and they apply once committed to main. Don't run it without asking.

If setup prints a `hint` line about `--nightly`, ask the user whether to run `agent-hearth setup
--nightly`. It makes every build Agent Hearth runs use a pinned nightly with the parallel front
end, which made builds after an edit faster on Linux with 32 cores, and leaves the repository's own
toolchain to humans and CI. It first builds main and runs its suite on both toolchains (minutes),
and turns the nightly on only when main fails nothing more on it. Code that only nightly accepts
then passes the gate and fails in CI. `agent-hearth setup --stable` goes back.

## 3. Verify

In a new worktree (`agent-hearth spawn hearth-check`), run `agent-hearth cargo build` and check it
finishes in seconds, not minutes. Then `agent-hearth check` should print `ok`. Remove the worktree
with `jj workspace forget` (or `git worktree remove`) and delete its directory.

## What the hooks do

- `claude-pre-tool-use` rewrites `cargo …` to `agent-hearth cargo …` and makes no permission
  decision, so the user's deny or ask rules for cargo subcommands no longer match. Codex cannot
  rewrite commands, so `codex-pre-tool-use` denies `cargo …` and names the `agent-hearth cargo …`
  command instead.
- `claude-stop` and `codex-stop` run `agent-hearth check` (fmt, clippy fixes, clippy, against
  main's own state: what main already has is left alone) and send the agent back with the
  diagnostics, at most three times in a row with the same ones. Clean code is then settled (the
  full test suite, recent failures first, without main's own failures). When this repository's
  settles take a minute or less, the hook waits and sends the agent back with its failures;
  longer ones run in the background and reach the agent at its next prompt.
- `claude-post-tool-use` and `codex-post-tool-use` start a build of the worktree in the
  background after each edit, at low priority, so the agent's next build finds most of the work
  done. They print nothing, so the agent never sees errors from half-finished edits.
  `AGENT_HEARTH_WARM=off` turns them off.
- `claude-user-prompt-submit` and `codex-user-prompt-submit` tell the agent at its next prompt
  about a settle that failed after it stopped, and refresh the golden in the background when
  main has moved.
- `claude-worktree-create` makes a warm jj workspace (or git worktree) beside the current one;
  `claude-worktree-remove` deletes it and its build directory, but refuses (and says why) while
  it holds uncommitted or untracked changes with git, or files jj did not snapshot.

Other agents: **Pi**, add `"agent-hearth check"` and `"agent-hearth settle"` to `finalChecks`.
**Anything else**, run `agent-hearth cargo …` instead of `cargo …` (the first one in a worktree
seeds it), and `agent-hearth check` and `agent-hearth settle` before finishing a task.

## Commands

| Command | Does |
|---|---|
| `agent-hearth setup [--build-settings]` | Hooks for Claude Code and Codex, a golden, and what is left to do |
| `agent-hearth setup --nightly [TOOLCHAIN]` | Every build on a pinned nightly, once main's suite fails the same on it; `--stable` goes back |
| `agent-hearth doctor` | What works on this machine, and every worktree's verdicts |
| `agent-hearth golden refresh` | Warm build of main that new worktrees are cloned from; the hooks refresh it when main moves |
| `agent-hearth spawn <name>` | New worktree, already warm |
| `agent-hearth cargo <args…>` | Cargo, at the fixed build path with the worktree's own build directory |
| `agent-hearth check` | fmt, clippy fixes, then one line per remaining diagnostic |
| `agent-hearth settle [--bg]` | The full suite for this exact code, cached by content; recent failures first |
| `agent-hearth warm` | Build the current code in the background (the PostToolUse hook runs it) |
| `agent-hearth gc` | Drop build directories and goldens nothing needs |

Exit codes: 0 pass, 1 diagnostics in the code, 2 tool error, 3 superseded (the code changed during
the run). `agent-hearth cargo` passes cargo's exit code through.

Known gaps (see `docs/design.md`): a cached verdict does not notice changes to git-ignored files
or environment variables that tests read.
