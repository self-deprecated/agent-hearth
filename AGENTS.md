# Agent Hearth contributor instructions

Agent Hearth keeps Rust builds warm for coding agents. `PLAN.md` is the plan and
`docs/design.md` the design; keep both current when scope changes.

## Rules

- Agents are the main users. Output is terse by default (one line per diagnostic) with `--json`
  for structure. Exit codes: 0 pass, 1 diagnostics, 2 tool error, 3 superseded.
- Every speed claim needs a measurement in `bench/`, with its script and raw output. Say when
  something is unmeasured.
- Never copy source content between worktrees; only targets and mtimes of identical files.
- Write only under the state directory, the cache directory and a worktree's ignored `target/`.
- Never hard-code paths inside a Cargo build directory; read artifacts from Cargo's JSON messages.

## Source control

Use Jujutsu, not raw Git workflows:

```sh
jj status --no-pager
jj diff --no-pager
jj commit -m "type(scope): message" <paths...>
```

## Commands

Use the devenv shell:

```sh
devenv shell
cargo test
check                     # fmt + clippy, warnings are errors
check-targets             # clippy for macOS and Windows, from Linux
devenv test
```

Benchmarks in `bench/` run inside the target project's devenv shell and only touch disposable
copies under `~/.cache`. See `bench/README.md`.
