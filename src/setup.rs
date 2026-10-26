//! `agent-hearth setup`: Agent Hearth's hooks in the repository's Claude Code and Codex settings,
//! merged with what is there, and a golden. An agent in its sandbox can do neither (Codex's keeps
//! even the repository's `.codex/` read-only), so this is one command for its user to run.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::golden;
use crate::locate::Location;

/// A hook: its event, the tools it matches (`None`: all), the `agent-hearth hook` it runs, and
/// its timeout in seconds.
type Hook = (
    &'static str,
    Option<&'static str>,
    &'static str,
    Option<u64>,
);

const CLAUDE: [Hook; 6] = [
    ("PreToolUse", Some("Bash"), "claude-pre-tool-use", None),
    (
        "PostToolUse",
        Some("Edit|Write|MultiEdit|NotebookEdit|Bash"),
        "claude-post-tool-use",
        None,
    ),
    // Long enough for a settle the Stop hook waits for.
    ("Stop", None, "claude-stop", Some(600)),
    ("UserPromptSubmit", None, "claude-user-prompt-submit", None),
    ("WorktreeCreate", None, "claude-worktree-create", None),
    ("WorktreeRemove", None, "claude-worktree-remove", None),
];

/// Codex edits with `apply_patch`, which Claude Code's matcher would miss: its PostToolUse
/// matches every tool, and the hook ignores those that edit nothing.
const CODEX: [Hook; 4] = [
    ("PreToolUse", Some("Bash"), "codex-pre-tool-use", None),
    ("PostToolUse", None, "codex-post-tool-use", None),
    ("Stop", None, "codex-stop", Some(600)),
    ("UserPromptSubmit", None, "codex-user-prompt-submit", None),
];

pub struct Report {
    /// The settings files written, and the ones that already had every hook.
    pub written: Vec<PathBuf>,
    pub unchanged: Vec<PathBuf>,
    /// The golden, and whether it had to be built; `None` when not asked for.
    pub golden: Option<(golden::Golden, bool)>,
}

/// Merges the hooks into the repository's `.claude/settings.json` (`claude`) and
/// `.codex/hooks.json` (`codex`), with `agent_hearth` as the binary, then refreshes the golden of
/// main when `refresh`.
pub fn setup(
    location: &Location,
    agent_hearth: &Path,
    claude: bool,
    codex: bool,
    refresh: bool,
) -> Result<Report> {
    let exe = crate::shell_word(&agent_hearth.to_string_lossy());
    let mut report = Report {
        written: Vec::new(),
        unchanged: Vec::new(),
        golden: None,
    };
    let files = [
        (claude, ".claude/settings.json", &CLAUDE[..]),
        (codex, ".codex/hooks.json", &CODEX[..]),
    ];
    for (wanted, file, hooks) in files {
        if !wanted {
            continue;
        }
        let path = location.worktree.join(file);
        let mut settings = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("{} is not valid JSON; fix it first", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => json!({}),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        if merge(&mut settings, &exe, hooks)? {
            fs::create_dir_all(path.parent().context("settings have no parent")?)?;
            let mut text = serde_json::to_string_pretty(&settings)?;
            text.push('\n');
            fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
            report.written.push(path);
        } else {
            report.unchanged.push(path);
        }
    }
    if refresh {
        location.create_dirs()?;
        report.golden = Some(golden::refresh(location, "main")?);
    }
    Ok(report)
}

/// Adds each of `hooks` to `settings` that is not there yet, and points those that are at `exe`.
/// Other hooks and settings stay. Returns whether anything changed.
fn merge(settings: &mut Value, exe: &str, hooks: &[Hook]) -> Result<bool> {
    let Some(root) = settings.as_object_mut() else {
        bail!("the settings are not a JSON object");
    };
    let all = root.entry("hooks").or_insert_with(|| json!({}));
    let Some(all) = all.as_object_mut() else {
        bail!("`hooks` in the settings is not a JSON object");
    };
    let mut changed = false;
    for &(event, matcher, name, timeout) in hooks {
        let command = format!("{exe} hook {name}");
        let entries = all.entry(event).or_insert_with(|| json!([]));
        let Some(entries) = entries.as_array_mut() else {
            bail!("`hooks.{event}` in the settings is not a JSON array");
        };
        // An earlier setup's hook, perhaps from another install of the binary.
        let mut found = false;
        for hook in entries
            .iter_mut()
            .filter_map(|entry| entry.get_mut("hooks")?.as_array_mut())
            .flatten()
        {
            let Some(existing) = hook.get("command").and_then(Value::as_str) else {
                continue;
            };
            if existing.ends_with(&format!(" hook {name}")) {
                found = true;
                if existing != command {
                    hook["command"] = json!(command);
                    changed = true;
                }
            }
        }
        if found {
            continue;
        }
        let mut hook = json!({"type": "command", "command": command});
        if let Some(timeout) = timeout {
            hook["timeout"] = json!(timeout);
        }
        let mut entry = json!({"hooks": [hook]});
        if let Some(matcher) = matcher {
            entry["matcher"] = json!(matcher);
        }
        entries.push(entry);
        changed = true;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_into_existing_settings_once_and_follows_the_binary() {
        let mut settings = json!({
            "permissions": {"allow": ["Bash(ls:*)"]},
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "notify-me"}]}]}
        });
        assert!(merge(&mut settings, "/bin/ah", &CLAUDE).unwrap());
        assert_eq!(settings["permissions"]["allow"][0], "Bash(ls:*)");
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "{stop:?}");
        assert_eq!(stop[0]["hooks"][0]["command"], "notify-me");
        assert_eq!(stop[1]["hooks"][0]["command"], "/bin/ah hook claude-stop");
        assert_eq!(stop[1]["hooks"][0]["timeout"], 600);
        assert_eq!(settings["hooks"]["PreToolUse"][0]["matcher"], "Bash");

        let once = settings.clone();
        assert!(!merge(&mut settings, "/bin/ah", &CLAUDE).unwrap());
        assert_eq!(settings, once);

        // Installed elsewhere since: the same hooks, pointed at the new binary.
        assert!(merge(&mut settings, "/opt/ah", &CLAUDE).unwrap());
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[1]["hooks"][0]["command"], "/opt/ah hook claude-stop");
    }

    #[test]
    fn codex_s_post_tool_use_matches_every_tool() {
        let mut settings = json!({});
        merge(&mut settings, "ah", &CODEX).unwrap();
        assert!(settings["hooks"]["PostToolUse"][0].get("matcher").is_none());
        assert_eq!(
            settings["hooks"]["Stop"][0]["hooks"][0]["command"],
            "ah hook codex-stop"
        );
    }

    #[test]
    fn writes_both_files_then_changes_nothing() {
        let s = crate::testing::Sandbox::new();
        s.crate_repo("repo");
        let repo = s.location("repo");
        let report = setup(&repo, Path::new("/bin/ah"), true, true, false).unwrap();
        assert_eq!(report.written.len(), 2);
        let claude = fs::read_to_string(s.path("repo/.claude/settings.json")).unwrap();
        assert!(
            claude.contains("/bin/ah hook claude-worktree-remove"),
            "{claude}"
        );
        let again = setup(&repo, Path::new("/bin/ah"), true, true, false).unwrap();
        assert!(again.written.is_empty());
        assert_eq!(again.unchanged.len(), 2);
    }
}
