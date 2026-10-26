//! Claude Code hooks. Each reads the hook's JSON from stdin and answers in Claude Code's protocol:
//! <https://code.claude.com/docs/en/hooks>. Codex speaks the same protocol, except that its
//! PreToolUse cannot rewrite a command.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::adopt::{self, Outcome};
use crate::check::{self, Status};
use crate::locate::{BaseDirs, Location, Vcs};
use crate::settle;
use crate::spawn;
use crate::warm;

/// Blocks with the same diagnostics at most this many times in a row, then lets Claude stop.
const STOP_ATTEMPTS: u32 = 3;

/// `auto` waits for the settle at Stop when the repository's recent settles took at most this, and
/// at most this long when it has none yet.
const AUTO_WAIT: Duration = Duration::from_secs(60);

/// Whether the Stop hook waits for the settle of the agent's code, set in the hook's command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum SettleMode {
    /// Start it and let the agent stop; failures reach the agent at its next prompt.
    Background,
    /// Wait for it, and block on its failures like on diagnostics.
    Foreground,
    /// Foreground when the repository's recent settles took a minute or less, else background;
    /// with none yet, waits up to a minute.
    Auto,
}

impl SettleMode {
    /// How long Stop waits for the verdict; zero lets the agent stop at once.
    fn wait(self, location: &Location) -> Result<Duration> {
        Ok(match self {
            SettleMode::Background => Duration::ZERO,
            SettleMode::Foreground => Duration::MAX,
            SettleMode::Auto => match settle::recent_seconds(location)? {
                None => AUTO_WAIT,
                Some(seconds) if seconds <= AUTO_WAIT.as_secs_f64() => Duration::MAX,
                Some(_) => Duration::ZERO,
            },
        })
    }
}

/// Subcommands that work outside the worktree, so pinning them would be wrong.
const UNPINNED: [&str; 4] = ["install", "uninstall", "new", "init"];

/// Words after which the next word is a command again.
const PREFIXES: [&str; 11] = [
    "{", "then", "do", "else", "elif", "if", "while", "until", "!", "time", "env",
];

pub fn read_input() -> Result<Value> {
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .context("reading the hook input")?;
    serde_json::from_str(&input).context("parsing the hook input")
}

fn cwd(input: &Value) -> Result<PathBuf> {
    input["cwd"]
        .as_str()
        .map(PathBuf::from)
        .context("the hook input has no cwd")
}

/// Whose PreToolUse protocol to answer in.
#[derive(Clone, Copy)]
pub enum Agent {
    Claude,
    Codex,
}

/// PreToolUse(Bash): returns the hook output that runs `cargo` through `agent-hearth`, or `None`
/// to leave the command alone.
pub fn pre_tool_use(
    input: &Value,
    base: &BaseDirs,
    agent_hearth: &str,
    agent: Agent,
) -> Result<Option<Value>> {
    let Some(command) = input["tool_input"]["command"].as_str() else {
        return Ok(None);
    };
    let Some(rewritten) = rewrite_cargo(command, agent_hearth) else {
        return Ok(None);
    };
    if Location::find_with(&cwd(input)?, base).is_err() {
        return Ok(None);
    }
    Ok(Some(match agent {
        Agent::Claude => {
            let mut tool_input = input["tool_input"].clone();
            tool_input["command"] = Value::String(rewritten);
            // No permissionDecision: the rewritten command still goes through the user's
            // permissions.
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "updatedInput": tool_input,
                }
            })
        }
        // Codex ignores updatedInput, so deny the command and name the one to run instead.
        Agent::Codex => json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": format!(
                    "In this repository cargo runs through agent-hearth, which keeps the \
                     worktree's build warm: cargo by any other name or path builds cold, for \
                     minutes. Run this instead; it takes the same arguments and prints the same \
                     output: {rewritten}"
                ),
            }
        }),
    }))
}

/// Inserts `agent_hearth` before every `cargo` in command position, outside quotes. Returns
/// `None` when there is nothing to rewrite.
pub fn rewrite_cargo(command: &str, agent_hearth: &str) -> Option<String> {
    let bytes = command.as_bytes();
    let mut out = String::with_capacity(command.len() + 32);
    let (mut i, mut at_command, mut changed) = (0, true, false);
    while i < bytes.len() {
        let c = bytes[i];
        if at_command && !matches!(c, b' ' | b'\t') {
            let end = word_end(bytes, i);
            let word = &command[i..end];
            if word.is_empty() {
                at_command = false;
                continue;
            }
            // `devenv shell [--] cmd` runs cmd in the project's shell, as agents in devenv
            // projects do.
            if word == "devenv"
                && let Some(len) = devenv_shell(&command[end..])
            {
                out.push_str(&command[i..end + len]);
                i = end + len;
                continue;
            }
            let assignment = word.split_once('=').is_some_and(|(name, _)| is_name(name));
            // Cargo by its path too: a Codex agent denied `cargo` looked up its path with
            // `command -v cargo` and ran that, building cold (`bench/onboard-codex-linux1.txt`).
            // agent-hearth runs the `cargo` on PATH.
            let mut word = word;
            if word == "cargo" || word.ends_with("/cargo") {
                let rest = command[end..].trim_start();
                let subcommand = &rest[..word_end(rest.as_bytes(), 0)];
                if !UNPINNED.contains(&subcommand) {
                    out.push_str(agent_hearth);
                    out.push(' ');
                    word = "cargo";
                    changed = true;
                }
            }
            out.push_str(word);
            i = end;
            at_command = assignment || PREFIXES.contains(&word);
            continue;
        }
        match c {
            b'\'' | b'"' => {
                let end = quoted_end(bytes, i);
                out.push_str(&command[i..end]);
                i = end;
                continue;
            }
            b'\\' => {
                let end = (i + 2).min(bytes.len());
                out.push_str(&command[i..end]);
                i = end;
                continue;
            }
            b';' | b'&' | b'|' | b'(' | b'\n' | b'`' => at_command = true,
            b'$' if bytes.get(i + 1) == Some(&b'(') => {
                out.push_str("$(");
                i += 2;
                at_command = true;
                continue;
            }
            _ => {}
        }
        let len = command[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&command[i..i + len]);
        i += len;
    }
    changed.then_some(out)
}

/// The length of ` shell` or ` shell --` at the start of `rest`, when a command follows.
fn devenv_shell(rest: &str) -> Option<usize> {
    let after = rest.trim_start_matches([' ', '\t']);
    let after_shell = after.strip_prefix("shell")?;
    if !after_shell.starts_with([' ', '\t']) {
        return None;
    }
    let mut len = rest.len() - after_shell.len();
    let next = after_shell.trim_start_matches([' ', '\t']);
    if let Some(after_dashes) = next.strip_prefix("--")
        && after_dashes.starts_with([' ', '\t'])
    {
        len = rest.len() - after_dashes.len();
    }
    Some(len)
}

fn is_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The end of the shell word starting at `start`: unquoted whitespace or an operator.
fn word_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b';' | b'&' | b'|' | b'(' | b')' | b'<' | b'>' | b'`' => break,
            b'\'' | b'"' => i = quoted_end(bytes, i),
            b'\\' => i = (i + 2).min(bytes.len()),
            _ => i += 1,
        }
    }
    i
}

/// The index just past the quote that closes the one at `start` (or the end).
fn quoted_end(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if quote == b'"' => i += 2,
            c if c == quote => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StopState {
    /// The diagnostics of the last block.
    diagnostics: Vec<String>,
    /// How many times in a row those exact diagnostics blocked.
    attempts: u32,
}

/// Stop: runs `agent-hearth check`, then reads the settle verdict for the same code. Returns the
/// decision that blocks stopping while either finds problems, or `None` to let Claude stop. With
/// no settle verdict yet, starts `<agent_hearth> settle` in the background, and waits for it when
/// `mode` says so: detached, it outlives a hook killed by its timeout.
pub fn stop(
    input: &Value,
    base: &BaseDirs,
    agent_hearth: &Path,
    mode: SettleMode,
) -> Result<Option<Value>> {
    let Ok(location) = Location::find_with(&cwd(input)?, base) else {
        return Ok(None);
    };
    if !location.worktree.join("Cargo.toml").is_file() {
        return Ok(None);
    }
    location.create_dirs()?;
    let target = adopt::target_dir(&location);
    adopt::adopt_or_start_cold(&location)?;
    let _warm = warm::hold(&location, &warm::CHECK.map(String::from))?;
    let mut report = check::check(&location, &target)?;
    if report.status == Status::Superseded {
        report = check::check(&location, &target)?;
    }
    let state_path = session_path(&location, input["session_id"].as_str().unwrap_or("none"));
    let (problems, source, noun) = match report.status {
        Status::Fail => {
            let summaries = report.diagnostics.iter().map(|d| d.summary()).collect();
            (summaries, "`agent-hearth check`", "diagnostic")
        }
        // Still changing after a retry: nothing settled to judge.
        Status::Superseded => return Ok(None),
        Status::Pass => {
            let verdict = match settle::verdict(&location)? {
                Some(verdict) => Some(verdict),
                None => {
                    settle::settle_in_background(&location, agent_hearth)?;
                    match mode.wait(&location)? {
                        Duration::ZERO => None,
                        limit => wait_for_verdict(&location, limit)?,
                    }
                }
            };
            match verdict {
                Some(verdict) if verdict.status == Status::Fail => {
                    // Told now, so not again at the next prompt.
                    settle::take_unreported_failure(&location)?;
                    let source = if verdict.partial {
                        "`agent-hearth settle` (the tests that failed recently, run before the \
                         rest of the suite)"
                    } else {
                        "`agent-hearth settle` (the full test suite)"
                    };
                    (verdict.failures, source, "failure")
                }
                _ => {
                    let _ = fs::remove_file(&state_path);
                    return Ok(None);
                }
            }
        }
    };

    let previous: StopState = fs::read(&state_path)
        .ok()
        .and_then(|json| serde_json::from_slice(&json).ok())
        .unwrap_or_default();
    let attempts = if previous.diagnostics == problems {
        previous.attempts + 1
    } else {
        1
    };
    if attempts > STOP_ATTEMPTS {
        let _ = fs::remove_file(&state_path);
        return Ok(None);
    }
    fs::create_dir_all(state_path.parent().context("session state has no parent")?)?;
    let state = StopState {
        diagnostics: problems.clone(),
        attempts,
    };
    fs::write(&state_path, serde_json::to_vec(&state)?)?;
    let count = problems.len();
    let plural = if count == 1 { "" } else { "s" };
    let reason = format!(
        "{source} found {count} {noun}{plural}:\n{}\nFix them, then finish. (Attempt \
         {attempts} of {STOP_ATTEMPTS} with these exact {noun}s.)",
        problems.join("\n")
    );
    Ok(Some(json!({"decision": "block", "reason": reason})))
}

/// PostToolUse: after a tool that may have changed files, starts a warm of the worktree in the
/// background, unless the tool ran cargo, which builds anyway.
pub fn post_tool_use(input: &Value, base: &BaseDirs, agent_hearth: &Path) -> Result<()> {
    let edits = match input["tool_name"].as_str().unwrap_or_default() {
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "apply_patch" => true,
        // A shell command may write files too; one that runs cargo builds anyway.
        "Bash" => {
            let command = input["tool_input"]["command"].as_str().unwrap_or_default();
            rewrite_cargo(command, "agent-hearth").is_none() && !command.contains("agent-hearth")
        }
        _ => false,
    };
    let Ok(location) = Location::find_with(&cwd(input)?, base) else {
        return Ok(());
    };
    if edits && location.worktree.join("Cargo.toml").is_file() {
        warm::start(&location, agent_hearth)?;
    }
    Ok(())
}

/// The settle verdict for the worktree's current code, once the settle running for it has one;
/// `None` when it ended without one (superseded, or a tool failure) or `limit` passed first.
fn wait_for_verdict(location: &Location, limit: Duration) -> Result<Option<settle::Verdict>> {
    let start = Instant::now();
    loop {
        if let Some(verdict) = settle::verdict(location)? {
            return Ok(Some(verdict));
        }
        if start.elapsed() > limit {
            return Ok(None);
        }
        // A settle just started may not hold its lock yet.
        if !settle::running(location)? && start.elapsed() > Duration::from_secs(5) {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(500));
    }
}

/// UserPromptSubmit: tells the agent, once, about a settle of its code that failed after it
/// stopped, which it would otherwise never hear of (an agent broke two tests it never ran).
pub fn user_prompt_submit(input: &Value, base: &BaseDirs) -> Result<Option<Value>> {
    let Ok(location) = Location::find_with(&cwd(input)?, base) else {
        return Ok(None);
    };
    let Some(last) = settle::take_unreported_failure(&location)? else {
        return Ok(None);
    };
    let count = last.verdict.failures.len();
    let plural = if count == 1 { "" } else { "s" };
    let now = if settle::fingerprint(&location)? == last.fingerprint {
        "The code has not changed since, so they still fail."
    } else {
        "The code has changed since; `agent-hearth settle` checks it again."
    };
    let suite = if last.verdict.partial {
        "the tests that failed recently (`agent-hearth settle` runs them before the rest of the \
         suite)"
    } else {
        "the full test suite (`agent-hearth settle`)"
    };
    let context = format!(
        "agent-hearth: {suite}, run after your last turn, found {count} failure{plural} in this \
         worktree:\n{}\n{now}",
        last.verdict.failures.join("\n")
    );
    Ok(Some(json!({
        "hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": context}
    })))
}

fn session_path(location: &Location, session: &str) -> PathBuf {
    let safe = session
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !session.is_empty();
    let name = if safe {
        session.to_owned()
    } else {
        blake3::hash(session.as_bytes()).to_hex()[..16].to_owned()
    };
    location
        .state_dir
        .join("sessions")
        .join(format!("{name}.json"))
}

/// WorktreeCreate: spawns the worktree `name` from the session's current revision and returns
/// its path, which Claude Code reads from the last line of stdout.
pub fn worktree_create(input: &Value, base: &BaseDirs) -> Result<PathBuf> {
    let from = Location::find_with(&cwd(input)?, base)?;
    let name = input["name"]
        .as_str()
        .context("the hook input has no worktree name")?;
    from.create_dirs()?;
    let rev = match from.vcs {
        Vcs::Jj => "@-",
        Vcs::Git => "HEAD",
    };
    let (spawned, outcome) = spawn::spawn(&from, name, rev)?;
    match outcome {
        Outcome::NoGolden => {
            eprintln!("agent-hearth: not seeded: run `agent-hearth golden refresh` first");
        }
        Outcome::NoReflinks(reason) => eprintln!("agent-hearth: not seeded: {reason}"),
        _ => {}
    }
    Ok(spawned.worktree)
}

/// WorktreeRemove: removes a worktree the WorktreeCreate hook made, and its build directory.
/// Leaves git branches alone: they may hold the only copy of unmerged commits.
pub fn worktree_remove(input: &Value, base: &BaseDirs) -> Result<()> {
    let path = input["worktree_path"]
        .as_str()
        .map(PathBuf::from)
        .context("the hook input has no worktree_path")?;
    let location = Location::find_with(&path, base)?;
    if location.worktree != fs::canonicalize(&path)? {
        bail!("{} is not a worktree root", path.display());
    }
    // A slot is freed for the next agent, its files and build directory kept.
    if crate::slot::release(&location)? {
        return Ok(());
    }
    let target = adopt::target_dir(&location);
    // Never at the cost of work: git refuses a worktree with changes or untracked files without
    // --force; jj keeps the working copy's changes in its commit, but not files it refused to
    // snapshot (larger than snapshot.max-new-file-size), which deleting the directory would lose.
    match location.vcs {
        Vcs::Jj => {
            let status = crate::locate::jj_command()
                .arg("status")
                .current_dir(&location.worktree)
                .output()
                .context("running jj status")?;
            let stderr = String::from_utf8_lossy(&status.stderr);
            if let Some(at) = stderr.find("Refused to snapshot some files:") {
                let files: Vec<&str> = stderr[at..]
                    .lines()
                    .skip(1)
                    .take_while(|line| line.starts_with("  "))
                    .map(str::trim)
                    .collect();
                bail!(
                    "not removing {}: jj did not snapshot {}, which removing it would lose",
                    location.worktree.display(),
                    files.join("; ")
                );
            }
            let name = jj_workspace_name(&location.worktree)?;
            run(crate::locate::jj_command()
                .args(["workspace", "forget", &name])
                .current_dir(&location.worktree))?;
            fs::remove_dir_all(&location.worktree)
                .with_context(|| format!("removing {}", location.worktree.display()))?;
        }
        Vcs::Git => {
            let parent = location.worktree.parent().unwrap_or(Path::new("/"));
            run(Command::new("git")
                .arg("--git-dir")
                .arg(&location.git_dir)
                .args(["worktree", "remove"])
                .arg(&location.worktree)
                .current_dir(parent))
            .with_context(|| {
                format!(
                    "not removing {}: commit or remove its changes first",
                    location.worktree.display()
                )
            })?;
        }
    }
    if target.exists() {
        fs::remove_dir_all(&target).with_context(|| format!("removing {}", target.display()))?;
    }
    Ok(())
}

/// The name of the jj workspace rooted at `root`, as `jj workspace list` knows it.
fn jj_workspace_name(root: &Path) -> Result<String> {
    let output = run(crate::locate::jj_command()
        .args([
            "--ignore-working-copy",
            "workspace",
            "list",
            "--template",
            "name ++ \"\\n\"",
        ])
        .current_dir(root))?;
    let names: Vec<&str> = output.lines().collect();
    // `jj workspace root` under each name would be exact; the spawned name is the directory's.
    let dir = root
        .file_name()
        .and_then(|name| name.to_str())
        .context("the worktree has no name")?;
    if names.contains(&dir) {
        Ok(dir.to_owned())
    } else {
        bail!("no jj workspace named {dir} (have: {})", names.join(", "))
    }
}

pub(crate) fn run(command: &mut Command) -> Result<String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden;
    use crate::testing::Sandbox;

    fn rewrite(command: &str) -> Option<String> {
        rewrite_cargo(command, "ah")
    }

    #[test]
    fn rewrites_cargo_in_command_position_only() {
        let cases = [
            ("cargo test", Some("ah cargo test")),
            (
                "cargo test --workspace -q",
                Some("ah cargo test --workspace -q"),
            ),
            (
                "cd crates/core && cargo clippy",
                Some("cd crates/core && ah cargo clippy"),
            ),
            (
                "cargo fmt; cargo test | tail",
                Some("ah cargo fmt; ah cargo test | tail"),
            ),
            (
                "RUST_LOG=debug CI=1 cargo run",
                Some("RUST_LOG=debug CI=1 ah cargo run"),
            ),
            (
                "FOO=\"a b\" cargo build",
                Some("FOO=\"a b\" ah cargo build"),
            ),
            ("time cargo build 2>&1", Some("time ah cargo build 2>&1")),
            (
                "/nix/store/x-rust/bin/cargo test -p core",
                Some("ah cargo test -p core"),
            ),
            ("cargo-nextest run", None),
            ("ls ~/.cargo/bin/cargo", None),
            (
                "devenv shell -- cargo test -p core",
                Some("devenv shell -- ah cargo test -p core"),
            ),
            (
                "devenv shell cargo test",
                Some("devenv shell ah cargo test"),
            ),
            ("devenv shell", None),
            ("devenv up cargo", None),
            ("x=$(cargo metadata)", Some("x=$(ah cargo metadata)")),
            ("echo $(cargo metadata)", Some("echo $(ah cargo metadata)")),
            ("(cargo test)", Some("(ah cargo test)")),
            (
                "if true; then cargo test; fi",
                Some("if true; then ah cargo test; fi"),
            ),
            ("echo 'cargo test'", None),
            ("echo \"cargo test\" && ls", None),
            ("echo cargo test", None),
            ("ah cargo test", None),
            ("cargo install ripgrep", None),
            ("cargo new demo && cargo init", None),
            ("~/.cargo/bin/cargo test", Some("ah cargo test")),
            ("café && cargo test", Some("café && ah cargo test")),
        ];
        for (command, expected) in cases {
            assert_eq!(rewrite(command).as_deref(), expected, "{command}");
        }
    }

    #[test]
    fn pre_tool_use_replaces_the_whole_input_and_leaves_permission_alone() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let input = json!({
            "cwd": s.path("repo"),
            "tool_name": "Bash",
            "tool_input": {"command": "cargo test", "description": "Run tests", "timeout": 1000},
        });
        let output = pre_tool_use(&input, &s.base(), "/bin/ah", Agent::Claude)
            .unwrap()
            .unwrap();
        let hook = &output["hookSpecificOutput"];
        assert_eq!(hook["hookEventName"], "PreToolUse");
        assert_eq!(hook["updatedInput"]["command"], "/bin/ah cargo test");
        assert_eq!(hook["updatedInput"]["description"], "Run tests");
        assert_eq!(hook["updatedInput"]["timeout"], 1000);
        assert!(hook.get("permissionDecision").is_none());

        let outside = json!({"cwd": s.root(), "tool_input": {"command": "cargo test"}});
        assert!(
            pre_tool_use(&outside, &s.base(), "/bin/ah", Agent::Claude)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn codex_pre_tool_use_denies_cargo_and_names_the_command_to_run() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let input = json!({"cwd": s.path("repo"), "tool_input": {"command": "cargo test"}});
        let output = pre_tool_use(&input, &s.base(), "/bin/ah", Agent::Codex)
            .unwrap()
            .unwrap();
        let hook = &output["hookSpecificOutput"];
        assert_eq!(hook["permissionDecision"], "deny");
        let reason = hook["permissionDecisionReason"].as_str().unwrap();
        assert!(reason.ends_with(": /bin/ah cargo test"), "{reason}");
    }

    #[test]
    fn stop_blocks_on_diagnostics_three_times_then_lets_go() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        golden::refresh(&s.location("repo"), "main").unwrap();
        let input = json!({"cwd": s.path("repo"), "session_id": "s1"});
        assert!(
            stop(&input, &s.base(), Path::new("true"), SettleMode::Background)
                .unwrap()
                .is_none(),
            "clean code stops"
        );

        s.write("repo/src/lib.rs", "pub fn one() -> u32 {\n    \"no\"\n}\n");
        for attempt in 1..=STOP_ATTEMPTS {
            let decision = stop(&input, &s.base(), Path::new("true"), SettleMode::Background)
                .unwrap()
                .expect("blocks");
            assert_eq!(decision["decision"], "block");
            let reason = decision["reason"].as_str().unwrap();
            assert!(reason.contains("src/lib.rs:2:5 E0308"), "{reason}");
            assert!(
                reason.contains(&format!("Attempt {attempt} of 3")),
                "{reason}"
            );
        }
        assert!(
            stop(&input, &s.base(), Path::new("true"), SettleMode::Background)
                .unwrap()
                .is_none(),
            "no progress after three blocks"
        );
        assert!(
            stop(&input, &s.base(), Path::new("true"), SettleMode::Background)
                .unwrap()
                .is_some(),
            "a later stop starts counting again"
        );
    }

    #[test]
    fn stop_blocks_on_a_failed_settle_of_the_same_code() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let repo = s.location("repo");
        golden::refresh(&repo, "main").unwrap();
        s.write(
            "repo/src/lib.rs",
            "pub fn one() -> u32 {\n    1\n}\n\n#[test]\nfn one_is_two() {\n    assert_eq!(one(), 2);\n}\n",
        );
        let input = json!({"cwd": s.path("repo"), "session_id": "s2"});
        assert!(
            stop(&input, &s.base(), Path::new("true"), SettleMode::Background)
                .unwrap()
                .is_none(),
            "no settle yet"
        );
        settle::settle(&repo, &adopt::target_dir(&repo), false).unwrap();
        let decision = stop(&input, &s.base(), Path::new("true"), SettleMode::Background)
            .unwrap()
            .expect("blocks");
        let reason = decision["reason"].as_str().unwrap();
        assert!(
            reason.starts_with("`agent-hearth settle` (the full test suite) found 1 failure:"),
            "{reason}"
        );
        assert!(
            reason.contains("FAILED one_is_two: assertion `left == right` failed"),
            "{reason}"
        );
    }

    #[test]
    fn a_failed_settle_reaches_the_agent_at_stop_in_the_foreground_or_at_its_next_prompt() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let repo = s.location("repo");
        golden::refresh(&repo, "main").unwrap();
        let input = json!({"cwd": s.path("repo"), "session_id": "s3"});
        // No settle has run here yet: auto waits up to a minute.
        assert_eq!(SettleMode::Auto.wait(&repo).unwrap(), AUTO_WAIT);
        s.write(
            "repo/src/lib.rs",
            "pub fn one() -> u32 {\n    1\n}\n\n#[test]\nfn one_is_two() {\n    assert_eq!(one(), 2);\n}\n",
        );

        // Background: the agent stops; the failure waits for its next prompt, told once.
        assert!(
            stop(&input, &s.base(), Path::new("true"), SettleMode::Background)
                .unwrap()
                .is_none()
        );
        settle::settle(&repo, &adopt::target_dir(&repo), false).unwrap();
        let output = user_prompt_submit(&input, &s.base())
            .unwrap()
            .expect("told");
        let context = output["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(
            context.contains("found 1 failure in this worktree"),
            "{context}"
        );
        assert!(context.contains("FAILED one_is_two"), "{context}");
        assert!(context.contains("has not changed since"), "{context}");
        assert!(
            user_prompt_submit(&input, &s.base()).unwrap().is_none(),
            "told once"
        );
        // A quick settle has run here: auto waits for the verdict.
        assert_eq!(SettleMode::Auto.wait(&repo).unwrap(), Duration::MAX);

        // Foreground: the Stop hook waits for the settle (here run by another thread, as the
        // background process would) and blocks on its failure.
        s.write(
            "repo/src/lib.rs",
            "pub fn one() -> u32 {\n    1\n}\n\n#[test]\nfn one_is_three() {\n    assert_eq!(one(), 3);\n}\n",
        );
        // Checked first, so that the hook's own check changes nothing under the settle.
        check::check(&repo, &adopt::target_dir(&repo)).unwrap();
        let (settling, target) = (s.location("repo"), adopt::target_dir(&repo));
        let runner = thread::spawn(move || settle::settle(&settling, &target, false).unwrap());
        let decision = stop(&input, &s.base(), Path::new("true"), SettleMode::Foreground)
            .unwrap()
            .expect("blocks");
        runner.join().unwrap();
        let reason = decision["reason"].as_str().unwrap();
        assert!(reason.contains("FAILED one_is_three"), "{reason}");
        assert!(
            user_prompt_submit(&input, &s.base()).unwrap().is_none(),
            "already told at stop"
        );
    }

    #[test]
    fn worktree_hooks_create_adopt_and_remove() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        golden::refresh(&s.location("repo"), "main").unwrap();
        s.run("repo", "jj", &["git", "init", "--colocate"]);
        let mut created = Vec::new();
        for name in ["bold-oak", "calm-fir"] {
            let path =
                worktree_create(&json!({"cwd": s.path("repo"), "name": name}), &s.base()).unwrap();
            assert!(path.join("src/lib.rs").is_file());
            let location = Location::find_with(&path, &s.base()).unwrap();
            assert!(adopt::seeded_from(&location).unwrap().is_some());
            assert!(adopt::target_dir(&location).is_dir());
            created.push(location);
        }
        let ws = &created[0];
        worktree_remove(&json!({"worktree_path": ws.worktree}), &s.base()).unwrap();
        if ws.pin.is_some() {
            // Beside the repository: removed with its build directory.
            assert!(!ws.worktree.exists());
            assert!(!adopt::target_dir(ws).exists());
        } else {
            // In a slot: freed for the next agent, its files and build directory kept.
            assert!(adopt::target_dir(ws).is_dir());
        }
        let workspaces = s.run("repo", "jj", &["workspace", "list"]);
        assert!(!workspaces.contains("bold-oak") && workspaces.contains("calm-fir"));
    }

    #[test]
    fn worktree_remove_keeps_a_git_worktree_with_changes() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        s.run("repo", "git", &["worktree", "add", "-q", "../wt"]);
        let wt = s.location("wt");
        adopt::adopt_or_start_cold(&wt).unwrap();
        s.write("wt/notes.txt", "unsaved work\n");
        let err = worktree_remove(&json!({"worktree_path": wt.worktree}), &s.base()).unwrap_err();
        assert!(format!("{err:#}").contains("not removing"), "{err:#}");
        assert_eq!(
            fs::read_to_string(s.path("wt/notes.txt")).unwrap(),
            "unsaved work\n"
        );
        assert!(
            adopt::target_dir(&wt).is_dir(),
            "its build directory went too"
        );

        fs::remove_file(s.path("wt/notes.txt")).unwrap();
        worktree_remove(&json!({"worktree_path": wt.worktree}), &s.base()).unwrap();
        assert!(!wt.worktree.exists());
    }

    #[test]
    fn worktree_remove_keeps_a_jj_workspace_with_files_jj_did_not_snapshot() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        s.run("repo", "jj", &["git", "init", "--colocate"]);
        s.run(
            "repo",
            "jj",
            &[
                "config",
                "set",
                "--repo",
                "snapshot.max-new-file-size",
                "64",
            ],
        );
        s.run(
            "repo",
            "jj",
            &["workspace", "add", "--quiet", "--name", "ws", "../ws"],
        );
        let ws = s.location("ws");
        s.write("ws/big.bin", &"x".repeat(1000));
        let err = worktree_remove(&json!({"worktree_path": ws.worktree}), &s.base()).unwrap_err();
        assert!(format!("{err:#}").contains("big.bin"), "{err:#}");
        assert!(s.path("ws/big.bin").is_file());
    }
}
