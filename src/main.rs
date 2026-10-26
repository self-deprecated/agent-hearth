//! Agent Hearth: keeps Rust builds warm for coding agents working in many worktrees at once.
//! See PLAN.md for what is built when and docs/design.md for how it works.

mod adopt;
mod affected;
mod builds;
mod cargo;
mod check;
#[cfg(unix)]
mod daemon;
mod doctor;
mod gc;
mod golden;
mod hook;
mod land;
mod locate;
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

use anyhow::{Context, Result, bail};
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
    /// Register an existing worktree and seed its build directory from the nearest golden.
    Adopt {
        /// Worktree to adopt; defaults to the current directory.
        path: Option<PathBuf>,
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
    /// Run tests at the pin, always compiling the whole workspace: nextest when it is installed,
    /// otherwise cargo test. Passes cargo's exit code through.
    Test {
        /// Only what depends on the packages changed since the last passing run (needs nextest).
        #[arg(long)]
        affected: bool,
        /// Passed on to nextest or cargo test.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
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
    /// Admit settles by free memory, several at once when it allows (Linux and macOS). The first
    /// settle that finds none starts it in the background; it exits after ten idle minutes.
    /// Without it, settles run one at a time.
    Daemon,
    /// Every worktree of this repository: verdicts for its current code, settles in flight, and
    /// whether the golden that seeded it is still current.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Land finished jj workspaces on main, gated at the golden: copies of their changes are
    /// checked and settled on main first, and main moves only when they pass. Never pushes.
    /// Exits 1 when a workspace is refused.
    Land {
        /// Workspace names, landed in this order.
        #[arg(required = true)]
        names: Vec<String>,
        /// Accept fewer tests than the last land ran, for stacks that delete tests.
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
    Warm,
    /// Where the agent's own cargo runs spent their time, from the log every `agent-hearth cargo`
    /// and `agent-hearth test` run adds to: building, running tests, waiting for a warm, and runs
    /// of the same command on unchanged code.
    Stats {
        #[arg(long)]
        json: bool,
    },
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
    },
    /// Check that this machine can seed and pin builds, and show where Agent Hearth keeps this
    /// worktree's state. Exits 1 when a check fails.
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
    Stop {
        /// Whether to wait for the settle and block on its failures. Waiting needs a hook timeout
        /// longer than the settle.
        #[arg(long, value_enum, default_value_t = hook::SettleMode::Auto)]
        settle: hook::SettleMode,
    },
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
    // Not for the daemon and settles: they write to each other's socket, where a gone peer is
    // an error to handle, not a reason to die.
    #[cfg(unix)]
    if !matches!(cli.command, Command::Daemon | Command::Settle { .. }) {
        // SAFETY: before any thread starts, setting a signal to its default disposition.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
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
        Command::Test { affected, args } => {
            let location = Location::find(&here()?)?;
            let started = (SystemTime::now(), Instant::now());
            let (target, warm) = prepare(&location, &warm::TEST.map(String::from))?;
            let snapshot = check::Snapshot::take(&location, "test")?;
            let nextest = affected::nextest_available(&location.worktree);
            let plan = if affected {
                affected::plan(&location, &snapshot)?
            } else {
                affected::Plan::All
            };
            if affected && !nextest {
                eprintln!("agent-hearth: cargo-nextest not found, so running every test");
            }
            if let (affected::Plan::Packages(packages), true) = (&plan, nextest) {
                let names: Vec<&str> = packages.iter().map(String::as_str).collect();
                eprintln!("agent-hearth: testing what depends on {}", names.join(", "));
            }
            // Extra arguments may filter the tests, and a filtered pass says nothing of the rest.
            let unfiltered = args.is_empty();
            let Some(args) = affected::cargo_args(&plan, nextest, &args) else {
                println!("ok  nothing changed since the last passing run");
                return Ok(ExitCode::SUCCESS);
            };
            let (code, timing) = cargo::run(&location, &target, &location.worktree, &args)?;
            log_build(&location, &args, code, started, warm.waited, &timing);
            drop(warm);
            warm_next(&location);
            if code == 0 && unfiltered {
                affected::record_passing(&location, &snapshot)?;
            }
            Ok(ExitCode::from(code))
        }
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
        Command::Stats { json } => {
            let location = Location::find(&here()?)?;
            let summaries = builds::summarize(&builds::read(&location));
            if json {
                println!("{}", serde_json::to_string_pretty(&summaries)?);
            } else if summaries.is_empty() {
                println!("no agent builds logged yet for this repository");
            } else {
                for (subcommand, s) in &summaries {
                    let share = |part: f64| 100.0 * part / s.seconds.max(f64::EPSILON);
                    println!(
                        "{subcommand:<8} {} runs, {:.0}s, median {:.1}s: building {:.0}%, after \
                         {:.0}%, waiting for a warm {:.0}%; {} repeats on unchanged code, {:.0}s",
                        s.runs,
                        s.seconds,
                        s.median,
                        share(s.build),
                        share(s.after_build),
                        share(s.warm_wait),
                        s.repeats,
                        s.repeat_seconds,
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Status { json } => {
            let location = Location::find(&here()?)?;
            location.create_dirs()?;
            let worktrees = status::status(&location)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&worktrees)?);
            } else {
                for worktree in &worktrees {
                    println!("{}", status_line(worktree));
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Land {
            names,
            accept_fewer,
            json,
        } => {
            let exe = env::current_exe().context("finding agent-hearth's own path")?;
            let location = Location::find(&here()?)?;
            let report = land::land(&location, &names, accept_fewer, Some(&exe))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                if !report.landed.is_empty() {
                    let landed = report.landed.join(", ");
                    println!("landed {landed}: main is now {}", short(&report.main));
                }
                if let Some(log) = &report.refreshing {
                    println!(
                        "refreshing the golden in the background; log: {}",
                        log.display()
                    );
                }
                for warning in &report.warnings {
                    println!("warning: {warning}");
                }
                for (name, problems) in &report.refused {
                    println!("refused {name}:");
                    for problem in problems {
                        println!("  {problem}");
                    }
                }
            }
            Ok(if report.refused.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
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
                Hook::Stop { settle } => {
                    let exe = env::current_exe().context("finding agent-hearth's own path")?;
                    hook::stop(&input, &base, &exe, settle)?
                }
                Hook::UserPromptSubmit => hook::user_prompt_submit(&input, &base)?,
                Hook::PostToolUse => {
                    let exe = env::current_exe().context("finding agent-hearth's own path")?;
                    hook::post_tool_use(&input, &base, &exe)?;
                    None
                }
                Hook::WorktreeCreate => {
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
        Command::Daemon => {
            #[cfg(unix)]
            daemon::serve(&locate::BaseDirs::from_env()?, daemon::Config::default())?;
            #[cfg(not(unix))]
            bail!("the daemon needs Unix sockets: settles run one at a time here");
            #[cfg(unix)]
            Ok(ExitCode::SUCCESS)
        }
        Command::Setup {
            claude,
            codex,
            no_golden,
        } => {
            let location = Location::find(&here()?)?;
            let exe = env::current_exe().context("finding agent-hearth's own path")?;
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
            for advice in &doctor.advice {
                println!("hint      {advice}");
            }
            let state = location.base().state.display().to_string();
            let exe = shell_word(&exe.to_string_lossy());
            println!("\nLeft to do:");
            println!(
                "- When main moves, run `agent-hearth golden refresh` (or land with `agent-hearth land`)."
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
        Command::Adopt { path } => {
            let dir = match path {
                Some(path) => path,
                None => here()?,
            };
            let location = Location::find(&dir)?;
            location.create_dirs()?;
            let start = Instant::now();
            match adopt::adopt(&location)? {
                Outcome::Adopted(adopted) => println!("{}", adopted_line(&adopted, start)),
                Outcome::Already => println!(
                    "already adopted: {}",
                    adopt::target_dir(&location).display()
                ),
                Outcome::NoGolden => bail!(NO_GOLDEN),
                Outcome::NoReflinks(reason) => bail!("not seeded: {reason}"),
            }
            Ok(ExitCode::SUCCESS)
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
            let started = (SystemTime::now(), Instant::now());
            let (target, warm) = prepare(&location, &args)?;
            warm::record(&location, &args)?;
            let (code, timing) = cargo::run(&location, &target, &cwd, &args)?;
            log_build(&location, &args, code, started, warm.waited, &timing);
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

/// Adds an agent's cargo run, started at `started`, to the build log.
fn log_build(
    location: &Location,
    args: &[String],
    code: u8,
    started: (SystemTime, Instant),
    warm_wait: std::time::Duration,
    timing: &cargo::Timing,
) {
    let seconds = started.1.elapsed();
    let fingerprint =
        check::Snapshot::take(location, "builds").map_or_else(|_| String::new(), |s| s.fingerprint);
    builds::record(
        location,
        args,
        code,
        started.0,
        seconds,
        warm_wait,
        timing,
        fingerprint,
    );
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
