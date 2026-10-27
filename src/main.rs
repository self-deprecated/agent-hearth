//! Agent Hearth: keeps Rust builds warm for coding agents working in many worktrees at once.
//! See PLAN.md for what is built when and docs/design.md for how it works.

mod adopt;
mod builds;
mod cargo;
mod check;
mod doctor;
mod gc;
mod golden;
mod hook;
mod locate;
mod memory;
mod settle;
mod setup;
mod slot;
mod spawn;
mod status;
#[cfg(test)]
mod testing;
mod warm;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Instant, SystemTime};
use std::{env, fs};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use adopt::{Adopted, Outcome};
use locate::Location;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the warm builds of main that new worktrees are seeded from.
    Golden {
        #[command(subcommand)]
        command: GoldenCommand,
    },
    /// Create a worktree (a jj workspace, or a git worktree on a new branch) and adopt it: beside
    /// this one where builds run at the pin, else in a free slot of the slot pool, whose build
    /// directory has only ever built at that path.
    Spawn {
        name: String,
        /// Revision to start from.
        #[arg(long, default_value = "main")]
        rev: String,
    },
    /// Run cargo pinned to the fixed build path with this worktree's own build directory.
    Cargo {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Turn-end gate: cargo fmt, clippy's fixes, then one line per remaining diagnostic. Exits 0
    /// when clean, 1 with diagnostics, 3 when the code changed during the check.
    Check {
        /// Add each diagnostic's full rendered text.
        #[arg(long)]
        verbose: bool,
        #[arg(long)]
        json: bool,
    },
    /// Run the full test suite, doctests included, for the current code and store the verdict.
    /// Exits 0 when it passes, 1 on failures, 3 when the code changed during the run.
    Settle {
        /// Start it detached, logging to a file, and return at once.
        #[arg(long)]
        bg: bool,
        /// Accept fewer tests than the last passing settle ran.
        #[arg(long)]
        accept_fewer: bool,
        #[arg(long)]
        json: bool,
    },
    /// Remove the build directories of worktrees that are gone, old goldens and month-old
    /// verdicts, then report the state directory's real disk use.
    Gc {
        /// Goldens to keep, the current one included.
        #[arg(long, default_value_t = 2)]
        keep: usize,
        #[arg(long)]
        json: bool,
    },
    /// Build the current code in the background's place of the agent's next build: its recent
    /// build commands and the Stop hook's clippy, output discarded. The PostToolUse hook starts it
    /// after each edit; an agent's own cargo run stops it or waits for it.
    #[command(hide = true)]
    Warm,
    /// Agent hooks: read the hook's JSON on stdin and answer in Claude Code's protocol, which
    /// Codex shares.
    Hook {
        #[command(subcommand)]
        hook: Hook,
    },
    /// Set the repository up: Agent Hearth's hooks merged into `.claude/settings.json` and
    /// `.codex/hooks.json`, a golden of main, and what is left to do, with the paths to use. Run
    /// it again after moving the binary.
    Setup {
        /// Only Claude Code's hooks.
        #[arg(long, conflicts_with = "codex")]
        claude: bool,
        /// Only Codex's hooks.
        #[arg(long)]
        codex: bool,
        /// Leave the golden for later (`agent-hearth golden refresh`).
        #[arg(long)]
        no_golden: bool,
        /// Add the dev-profile settings that made builds after an edit a third faster and tests
        /// twice as fast to `Cargo.toml`, unless it has a `[profile.dev]` of its own. They take
        /// effect once committed to main.
        #[arg(long)]
        build_settings: bool,
    },
    /// Check that this machine can seed and pin builds, show where Agent Hearth keeps this
    /// worktree's state, and where every worktree of the repository stands: verdicts for its
    /// current code, settles in flight, and whether the golden that seeded it is still current.
    /// Exits 1 when a check fails.
    Doctor {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum Hook {
    /// PreToolUse(Bash): run `cargo` through `agent-hearth cargo`. Permissions still apply.
    #[command(name = "claude-pre-tool-use")]
    PreToolUse,
    /// Codex's PreToolUse(Bash), which cannot rewrite a command: deny `cargo` and name the
    /// `agent-hearth cargo` command to run instead.
    #[command(name = "codex-pre-tool-use")]
    CodexPreToolUse,
    /// Stop: run `agent-hearth check` and block while it finds diagnostics, at most three times
    /// in a row with the same ones; then settle the code. Codex's Stop is the same.
    #[command(name = "claude-stop", visible_alias = "codex-stop")]
    Stop,
    /// UserPromptSubmit: tell the agent, once, about a settle of its code that failed after it
    /// stopped.
    #[command(
        name = "claude-user-prompt-submit",
        visible_alias = "codex-user-prompt-submit"
    )]
    UserPromptSubmit,
    /// PostToolUse: after an edit, start a warm of the worktree in the background. Prints
    /// nothing, so the agent sees nothing.
    #[command(name = "claude-post-tool-use", visible_alias = "codex-post-tool-use")]
    PostToolUse,
    /// WorktreeCreate: spawn and adopt the worktree, print its path.
    #[command(name = "claude-worktree-create")]
    WorktreeCreate,
    /// WorktreeRemove: forget or remove a worktree WorktreeCreate made, and its build directory.
    #[command(name = "claude-worktree-remove")]
    WorktreeRemove,
}

#[derive(Subcommand)]
enum GoldenCommand {
    /// Build a revision at the fixed path and make it the seed for new worktrees. Its test suite
    /// then runs in the background, to learn which tests fail on main already.
    Refresh {
        #[arg(long, default_value = "main")]
        rev: String,
    },
    /// Run the golden's test suite and record its failures, which settles of worktrees seeded
    /// from it then leave out. `golden refresh` starts it.
    #[command(hide = true)]
    Settle { id: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Rust ignores SIGPIPE, so printing to a reader that has gone (`doctor | head`) panicked.
    // SAFETY: before any thread starts, setting a signal to its default disposition.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("agent-hearth: {err:#}");
            let state = locate::BaseDirs::from_env().map(|base| base.state);
            if let Some(hint) = state
                .ok()
                .and_then(|state| locate::unwritable_hint(&err, &state))
            {
                eprintln!("agent-hearth: {hint}");
            }
            // 2 is a tool error; 1 is reserved for "your code has diagnostics".
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let here = || env::current_dir().context("reading the current directory");
    match cli.command {
        Command::Settle {
            bg,
            accept_fewer,
            json,
        } => {
            let location = Location::find(&here()?)?;
            let (target, _warm) = prepare(&location, &warm::TEST.map(String::from))?;
            if bg {
                let exe = env::current_exe().context("finding agent-hearth's own path")?;
                match settle::settle_in_background(&location, &exe)? {
                    settle::Background::Started(log) => {
                        println!("settling in the background; log: {}", log.display());
                    }
                    settle::Background::AlreadyRunning => println!("already settling"),
                }
                return Ok(ExitCode::SUCCESS);
            }
            let start = Instant::now();
            let report = settle::settle(&location, &target, accept_fewer);
            settle::end_detached();
            let report = report?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                for failure in &report.verdict.failures {
                    println!("{failure}");
                }
                for name in &report.verdict.flaky {
                    println!("flaky {name}: failed, then passed when run again");
                }
                if report.verdict.existing > 0 {
                    let n = report.verdict.existing;
                    let plural = if n == 1 { "" } else { "s" };
                    println!("({n} failure{plural} main has too, not shown)");
                }
                if report.verdict.partial {
                    println!(
                        "only the tests that failed recently ran; the rest run once they pass"
                    );
                }
                let tests = report.verdict.tests;
                match report.verdict.status {
                    check::Status::Pass if report.cached => {
                        println!("ok  {tests} tests (unchanged code)");
                    }
                    check::Status::Pass => {
                        println!("ok  {tests} tests {:.1}s", start.elapsed().as_secs_f64());
                    }
                    check::Status::Fail => {}
                    check::Status::Superseded => {
                        println!("superseded: the code changed during the run");
                    }
                }
            }
            Ok(ExitCode::from(report.verdict.status.exit_code()))
        }
        Command::Gc { keep, json } => {
            let report = gc::gc(&Location::find(&here()?)?, keep)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
                return Ok(ExitCode::SUCCESS);
            }
            println!(
                "removed {} worktrees' build directories, {} goldens and {} verdicts: {} freed",
                report.worktrees,
                report.goldens,
                report.verdicts,
                gigabytes(report.freed)
            );
            match &report.disk {
                Some(disk) => println!(
                    "state uses {} on disk ({} exclusive, {} shared by clones); {} without \
                     reflinks",
                    gigabytes(disk.used()),
                    gigabytes(disk.exclusive),
                    gigabytes(disk.shared),
                    gigabytes(disk.referenced)
                ),
                None => println!(
                    "state is {} apparent; real disk use unknown (needs `btrfs filesystem du`)",
                    gigabytes(report.remaining)
                ),
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Hook { hook } => {
            let input = hook::read_input()?;
            let base = locate::BaseDirs::from_env()?;
            let output = match hook {
                Hook::PreToolUse | Hook::CodexPreToolUse => {
                    let exe = env::current_exe().context("finding agent-hearth's own path")?;
                    let agent = match hook {
                        Hook::CodexPreToolUse => hook::Agent::Codex,
                        _ => hook::Agent::Claude,
                    };
                    hook::pre_tool_use(&input, &base, &shell_word(&exe.to_string_lossy()), agent)?
                }
                Hook::Stop => {
                    let exe = env::current_exe().context("finding agent-hearth's own path")?;
                    hook::stop(&input, &base, &exe, None)?
                }
                Hook::UserPromptSubmit => {
                    let exe = env::current_exe().context("finding agent-hearth's own path")?;
                    hook::refresh_golden_if_moved(&input, &base, &exe);
                    hook::user_prompt_submit(&input, &base)?
                }
                Hook::PostToolUse => {
                    let exe = env::current_exe().context("finding agent-hearth's own path")?;
                    hook::post_tool_use(&input, &base, &exe)?;
                    None
                }
                Hook::WorktreeCreate => {
                    let exe = env::current_exe().context("finding agent-hearth's own path")?;
                    hook::refresh_golden_if_moved(&input, &base, &exe);
                    println!("{}", hook::worktree_create(&input, &base)?.display());
                    None
                }
                Hook::WorktreeRemove => {
                    hook::worktree_remove(&input, &base)?;
                    None
                }
            };
            if let Some(output) = output {
                println!("{output}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Warm => {
            let location = Location::find(&here()?)?;
            let target = adopt::target_dir(&location);
            if target.is_dir() {
                warm::warm(&location, &target)?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Setup {
            claude,
            codex,
            no_golden,
            build_settings,
        } => {
            let location = Location::find(&here()?)?;
            let exe = env::current_exe().context("finding agent-hearth's own path")?;
            let settings = build_settings && setup::apply_build_settings(&location.worktree)?;
            let doctor = doctor::doctor(&location)?;
            for check in doctor
                .checks
                .iter()
                .filter(|check| !check.ok && check.name != "golden")
            {
                println!("FAIL  {:<8} {}", check.name, check.detail);
            }
            let both = !claude && !codex;
            let start = Instant::now();
            let refresh = !no_golden;
            if refresh {
                println!("building the golden of main; minutes on a large workspace…");
            }
            let report = setup::setup(&location, &exe, claude || both, codex || both, refresh)?;
            if let Some((golden, _)) = &report.golden {
                settle::settle_golden_in_background(&location, &exe, &golden.id)?;
            }
            for path in &report.written {
                println!("wrote     {}", path.display());
            }
            for path in &report.unchanged {
                println!("unchanged {} (its hooks are set up)", path.display());
            }
            match &report.golden {
                Some((golden, true)) => println!(
                    "golden    {} at {}: built in {:.0}s",
                    golden.id,
                    short(&golden.commit),
                    start.elapsed().as_secs_f64()
                ),
                Some((golden, false)) => {
                    println!(
                        "golden    {} at {}: already built",
                        golden.id,
                        short(&golden.commit)
                    );
                }
                None => println!("golden    not built: run `agent-hearth golden refresh` on main"),
            }
            if settings {
                println!(
                    "wrote     {}: build settings; commit it to main, and the golden and new \
                     worktrees build with them",
                    location.worktree.join("Cargo.toml").display()
                );
            } else if build_settings && !doctor.advice.is_empty() {
                println!("hint      Cargo.toml has a [profile.dev] of its own; add these to it:");
            } else if !doctor.advice.is_empty() {
                println!(
                    "hint      `agent-hearth setup --build-settings` adds these to Cargo.toml:"
                );
            }
            for advice in doctor.advice.iter().filter(|_| !settings) {
                println!("hint      {advice}");
            }
            let state = location.base().state.display().to_string();
            let exe = shell_word(&exe.to_string_lossy());
            println!("\nLeft to do:");
            println!(
                "- Nothing when main moves: the golden refreshes itself in the background at the next \
                 prompt or spawn (`agent-hearth golden refresh` does it at once)."
            );
            if claude || both {
                println!(
                    "- Claude Code: where you allow `Bash(cargo:*)`, also allow `Bash({exe} cargo:*)`; \
                     deny or ask rules for cargo subcommands no longer match, as cargo runs through Agent Hearth."
                );
            }
            if codex || both {
                println!(
                    "- Codex: commit .codex/hooks.json (git worktrees read the main checkout's), and \
                     trust the project when Codex asks: it loads project hooks only then."
                );
            }
            println!(
                "- In an agent's sandbox, allow writes to {state}:\n  \
                 Codex, ~/.codex/config.toml: [sandbox_workspace_write] writable_roots = [\"{state}\"]\n  \
                 Claude Code, settings.json: {{\"sandbox\": {{\"filesystem\": {{\"allowWrite\": [\"{state}\"]}}}}}}"
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Doctor { json } => {
            let report = doctor::doctor(&Location::find(&here()?)?)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_report(&report);
            }
            Ok(if report.ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        Command::Golden {
            command: GoldenCommand::Settle { id },
        } => {
            let location = Location::find(&here()?)?;
            let settled = settle::settle_golden(&location, &id);
            settle::end_detached();
            settled?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Golden {
            command: GoldenCommand::Refresh { rev },
        } => {
            let location = Location::find(&here()?)?;
            location.create_dirs()?;
            let start = Instant::now();
            let (golden, built) = golden::refresh(&location, &rev)?;
            let commit = short(&golden.commit);
            if built {
                let secs = start.elapsed().as_secs_f64();
                println!("golden {} at {commit}: built in {secs:.1}s", golden.id);
            } else {
                println!("golden {} at {commit}: already built", golden.id);
            }
            let exe = env::current_exe().context("finding agent-hearth's own path")?;
            settle::settle_golden_in_background(&location, &exe, &golden.id)?;
            let warmed = slot::warm(&location, &golden.commit)?;
            if !warmed.is_empty() {
                let warmed: Vec<String> = warmed.iter().map(u32::to_string).collect();
                println!("free slots warmed at {commit}: {}", warmed.join(", "));
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Cargo { args } => {
            let cwd = fs::canonicalize(here()?).context("resolving the current directory")?;
            let location = Location::find(&cwd)?;
            // A whole-build-directory clean: an agent did this mid-task, which emptied the
            // seeded directory and failed on the pin's mount point. Partial cleans (`-p`,
            // `--release`) go to cargo.
            let flags = ["-q", "--quiet", "-v", "-vv", "--verbose"];
            if args.first().is_some_and(|arg| arg == "clean")
                && args[1..].iter().all(|arg| flags.contains(&arg.as_str()))
            {
                location.create_dirs()?;
                let start = Instant::now();
                match adopt::reseed(&location)? {
                    Outcome::Adopted(adopted) => {
                        eprintln!("agent-hearth: cleaned; {}", adopted_line(&adopted, start));
                    }
                    _ => eprintln!("agent-hearth: cleaned; building cold. {NO_GOLDEN}"),
                }
                return Ok(ExitCode::SUCCESS);
            }
            let started = SystemTime::now();
            let (target, warm) = prepare(&location, &args)?;
            warm::record(&location, &args)?;
            let code = cargo::run(&location, &target, &cwd, &args)?;
            builds::record(&location, &args, code, started);
            drop(warm);
            if warm::warm_command(&args).is_some() {
                warm_next(&location);
            }
            Ok(ExitCode::from(code))
        }
        Command::Check { verbose, json } => {
            let location = Location::find(&here()?)?;
            let (target, _warm) = prepare(&location, &warm::CHECK.map(String::from))?;
            let start = Instant::now();
            let report = check::check(&location, &target)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_check(&report, verbose, start);
            }
            Ok(ExitCode::from(report.status.exit_code()))
        }
        Command::Spawn { name, rev } => {
            let from = Location::find(&here()?)?;
            from.create_dirs()?;
            let exe = env::current_exe().context("finding agent-hearth's own path")?;
            if let Some(log) = golden::refresh_if_moved(&from, &exe)? {
                eprintln!(
                    "agent-hearth: main has moved: refreshing the golden in the background; log: {}",
                    log.display()
                );
            }
            let start = Instant::now();
            let (spawned, outcome) = spawn::spawn(&from, &name, &rev)?;
            // The path alone on stdout, for `cd "$(agent-hearth spawn name)"`.
            println!("{}", spawned.worktree.display());
            match outcome {
                Outcome::Adopted(adopted) => {
                    eprintln!("agent-hearth: {}", adopted_line(&adopted, start));
                }
                Outcome::Already => {}
                Outcome::NoGolden => eprintln!("agent-hearth: not seeded. {NO_GOLDEN}"),
                Outcome::NoReflinks(reason) => eprintln!("agent-hearth: not seeded: {reason}"),
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// After an agent's build, warms its other recent commands while it reads the result: agents
/// run a different command next with no edit between (`check` then `test`, `test` then
/// `clippy`) for a third of their waiting (`bench/agent-gaps-linux1.txt`). Never fails the run.
fn warm_next(location: &Location) {
    if let Err(err) = env::current_exe()
        .map_err(anyhow::Error::from)
        .and_then(|exe| warm::start(location, &exe))
    {
        eprintln!("agent-hearth: no warm: {err:#}");
    }
}

/// Returns the worktree's build directory, adopting the worktree on its first build, and a hold
/// that keeps warms out of it while the caller builds. Notices go to stderr, which keeps cargo's
/// stdout clean.
fn prepare(location: &Location, wants: &[String]) -> Result<(PathBuf, warm::Hold)> {
    location.create_dirs()?;
    if cfg!(target_os = "linux")
        && let Some(problem) = locate::pin_problem()
    {
        eprintln!("agent-hearth: building without the fixed path, so slower: {problem}");
    }
    let target = adopt::target_dir(location);
    let start = Instant::now();
    match adopt::adopt_or_start_cold(location)? {
        Outcome::Adopted(adopted) => {
            eprintln!("agent-hearth: {}", adopted_line(&adopted, start));
        }
        Outcome::Already => {}
        Outcome::NoGolden => eprintln!("agent-hearth: building cold. {NO_GOLDEN}"),
        Outcome::NoReflinks(reason) => eprintln!("agent-hearth: building cold: {reason}"),
    }
    Ok((target, warm::hold(location, wants)?))
}

fn print_check(report: &check::Report, verbose: bool, start: Instant) {
    for path in &report.fixed {
        println!("fixed {}", path.display());
    }
    for diagnostic in &report.diagnostics {
        println!("{}", diagnostic.summary());
        if let Some(rendered) = diagnostic.rendered.as_deref().filter(|_| verbose) {
            println!("{}", rendered.trim_end());
        }
    }
    if report.existing > 0 {
        let plural = if report.existing == 1 { "" } else { "s" };
        println!(
            "({} diagnostic{plural} main has too, not shown)",
            report.existing
        );
    }
    match report.status {
        check::Status::Pass if report.cached => println!("ok  clippy (unchanged code)"),
        check::Status::Pass => println!("ok  clippy {:.1}s", start.elapsed().as_secs_f64()),
        check::Status::Fail => {}
        check::Status::Superseded => println!("superseded: the code changed during the check"),
    }
}

fn status_line(worktree: &status::Worktree) -> String {
    let path = worktree.path.display();
    if !worktree.exists {
        return format!("{path}  missing");
    }
    let verdict = |status: Option<check::Status>| match status {
        Some(check::Status::Pass) => "pass",
        Some(check::Status::Fail) => "fail",
        Some(check::Status::Superseded) | None => "none",
    };
    let settle = if worktree.settling {
        "running"
    } else {
        verdict(worktree.settle)
    };
    let seed = match (&worktree.seeded_from, worktree.stale) {
        (Some(golden), true) => format!("seeded from {golden} (stale)"),
        (Some(golden), false) => format!("seeded from {golden}"),
        (None, _) => "not seeded".to_owned(),
    };
    format!(
        "{path}  check {}  settle {settle}  {seed}",
        verdict(worktree.check)
    )
}

const NO_GOLDEN: &str = "No golden to seed from: run `agent-hearth golden refresh` first.";

fn adopted_line(adopted: &Adopted, start: Instant) -> String {
    format!(
        "adopted from golden {} at {}: cloned {} files ({} MB) in {:.1}s; \
         {} of {} source files unchanged",
        adopted.golden.id,
        short(&adopted.golden.commit),
        adopted.files,
        adopted.bytes / 1_000_000,
        start.elapsed().as_secs_f64(),
        adopted.unchanged,
        adopted.sources,
    )
}

/// `word` quoted for a POSIX shell when it needs it.
pub(crate) fn shell_word(word: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "/._-+:@%=,".contains(c);
    if !word.is_empty() && word.chars().all(plain) {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

fn short(commit: &str) -> &str {
    &commit[..12.min(commit.len())]
}

fn print_report(report: &doctor::Report) {
    println!("mode      {}", report.mode);
    println!("worktree  {}  ({})", report.worktree.display(), report.vcs);
    println!("repo      {}  {}", report.repo_id, report.git_dir.display());
    println!("state     {}", report.state_dir.display());
    match &report.pin {
        Some(pin) => println!("pin       {}", pin.display()),
        None => println!(
            "pin       none: {}",
            locate::pin_problem().unwrap_or("not available")
        ),
    }
    match &report.seeded_from {
        Some(golden) => println!(
            "target    {}  (seeded from {golden})",
            report.target.display()
        ),
        None if report.target.is_dir() => {
            println!("target    {}  (not seeded)", report.target.display());
        }
        None => println!("target    none yet: the first `agent-hearth cargo` here adopts it"),
    }
    for check in &report.checks {
        let status = if check.ok { "ok  " } else { "FAIL" };
        println!("{status}  {:<8} {}", check.name, check.detail);
    }
    for advice in &report.advice {
        println!("hint  profile  {advice}");
    }
    for worktree in &report.worktrees {
        println!("{}", status_line(worktree));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn cargo_passes_flags_through() {
        let cli = Cli::parse_from(["agent-hearth", "cargo", "test", "--workspace", "-q"]);
        let Command::Cargo { args } = cli.command else {
            panic!("expected the cargo command");
        };
        assert_eq!(args, ["test", "--workspace", "-q"]);
    }
}
