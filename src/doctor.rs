//! Checks that this machine can seed and pin builds, and reports where everything lives.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use serde::Serialize;

use crate::adopt;
use crate::golden;
use crate::locate::{self, Location};
use crate::status;
use crate::structure;
use crate::toolchain::{self, Toolchain};

#[derive(Debug, Serialize)]
pub struct Report {
    pub worktree: PathBuf,
    pub vcs: &'static str,
    pub repo_id: String,
    pub git_dir: PathBuf,
    pub state_dir: PathBuf,
    pub pin: Option<PathBuf>,
    pub target: PathBuf,
    /// The golden this worktree was seeded from, if it was adopted.
    pub seeded_from: Option<String>,
    pub checks: Vec<Check>,
    /// What Agent Hearth does on this machine, given its checks.
    pub mode: String,
    /// Changes to the repository that would make its builds faster; never a failure.
    pub advice: Vec<String>,
    /// The toolchain every build uses, when `setup --nightly` chose one.
    pub toolchain: Option<Toolchain>,
    /// Whether to suggest `setup --nightly`: the repository's own toolchain, on a machine like
    /// the one where nightly with `-Zthreads` paid.
    pub nightly_hint: bool,
    /// What holds the golden's build up, from its timings: changes a maintainer could make to the
    /// workspace, unmeasured; never a failure.
    pub structure: Vec<String>,
    /// This worktree, then every other worktree adopted in its repository.
    pub worktrees: Vec<status::Worktree>,
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|check| check.ok)
    }
}

pub fn doctor(location: &Location) -> Result<Report> {
    location.create_dirs()?;
    let mut checks = Vec::new();
    checks.push(match locate::pin_problem() {
        None => Check {
            name: "path",
            ok: true,
            detail: "user namespaces work: builds run at the pin".to_owned(),
        },
        // Linux could pin, so a failing bwrap is worth fixing.
        Some(problem) => Check {
            name: "path",
            ok: !cfg!(target_os = "linux"),
            detail: format!(
                "{problem}: spawned worktrees reuse slots at fixed paths; others build at their \
                 own path, slower"
            ),
        },
    });
    checks.push(reflink_check(location));
    for (name, program) in [("cargo", "cargo"), ("rustc", "rustc")] {
        checks.push(command_check(
            name,
            toolchain::command(program, location.toolchain.as_ref())
                .arg("--version")
                .current_dir(&location.worktree),
            |stdout| stdout.trim().to_owned(),
        ));
    }
    checks.push(match golden::current(location)? {
        Some(golden) => Check {
            name: "golden",
            ok: true,
            detail: format!("{} at {}", golden.id, golden.commit),
        },
        None => Check {
            name: "golden",
            ok: false,
            detail: "none yet: run `agent-hearth golden refresh`".to_owned(),
        },
    });
    let failed = |name: &str| checks.iter().any(|check| check.name == name && !check.ok);
    let hardlinks = if adopt::reflinks_work(&location.state_dir).is_err() {
        " (seeded by hardlinks)"
    } else {
        ""
    };
    let mode = if failed("reflink") {
        "gate only: worktrees build cold, but `check` and `settle` still leave out what main \
         already fails, which saved real agents most of their time"
            .to_owned()
    } else if locate::pin_problem().is_some() {
        format!(
            "seeded worktrees{hardlinks} and the gate; no fixed build path, so `spawn` reuses \
             slots at fixed paths and other worktrees rebuild their own crates on their first \
             build"
        )
    } else {
        format!("warm worktrees{hardlinks} at a fixed build path, and the gate")
    };
    Ok(Report {
        mode,
        worktree: location.worktree.clone(),
        vcs: location.vcs.name(),
        repo_id: location.repo_id.clone(),
        git_dir: location.git_dir.clone(),
        state_dir: location.state_dir.clone(),
        pin: location.pin.clone(),
        target: adopt::target_dir(location),
        seeded_from: adopt::seeded_from(location)?,
        checks,
        advice: profile_advice(&location.worktree),
        toolchain: location.toolchain.clone(),
        nightly_hint: location.toolchain.is_none()
            && toolchain::threads_for(std::thread::available_parallelism().map_or(1, |n| n.get()))
                .is_some(),
        structure: structure_hints(location),
        worktrees: status::status(location)?,
    })
}

/// What the dev profile lacks of the settings that made atuin's builds after an edit 34-40% faster
/// and its unit tests twice as fast (`bench/settings-atuin-linux1.txt`). Read from `Cargo.toml`
/// and `.cargo/config.toml` in the worktree, the latter winning, as in Cargo.
pub fn profile_advice(worktree: &Path) -> Vec<String> {
    let read = |path: &str| -> toml::Table {
        fs::read_to_string(worktree.join(path))
            .ok()
            .and_then(|text| text.parse().ok())
            .unwrap_or_default()
    };
    let (manifest, config) = (read("Cargo.toml"), read(".cargo/config.toml"));
    let setting = |keys: &[&str]| {
        [&config, &manifest].into_iter().find_map(|table| {
            let mut value = table.get("profile")?;
            for key in keys {
                value = value.get(key)?;
            }
            Some(value.clone())
        })
    };
    let full_debuginfo = |value: Option<toml::Value>| match value {
        None => true,
        Some(toml::Value::Boolean(debug)) => debug,
        Some(toml::Value::Integer(level)) => level >= 2,
        Some(toml::Value::String(level)) => level == "full",
        Some(_) => false,
    };
    let mut advice = Vec::new();
    if full_debuginfo(setting(&["dev", "debug"])) {
        advice.push(
            "full debuginfo for the workspace: `[profile.dev] debug = \"line-tables-only\"` keeps \
             file and line in backtraces and shrinks what every build writes and links"
                .to_owned(),
        );
    }
    let deps = |key: &str| setting(&["dev", "package", "*", key]);
    let unoptimised = match deps("opt-level") {
        None => setting(&["dev", "opt-level"]).is_none(),
        Some(level) => matches!(level, toml::Value::Integer(0)),
    };
    if full_debuginfo(deps("debug")) || unoptimised {
        advice.push(
            "dependencies with debuginfo or unoptimised: `[profile.dev.package.\"*\"] debug = \
             false` and `opt-level = 1` cost only the golden, which builds them once, and make \
             tests run faster"
                .to_owned(),
        );
    }
    advice
}

/// [`structure::hints`] from the newest golden timings report, if there is one.
fn structure_hints(location: &Location) -> Vec<String> {
    let Some(report) = structure::report(&location.state_dir.join("goldens")) else {
        return Vec::new();
    };
    let Ok(html) = fs::read_to_string(report) else {
        return Vec::new();
    };
    let members =
        golden::members(&location.worktree, location.toolchain.as_ref()).unwrap_or_default();
    structure::hints(&html, &members)
}

fn command_check(
    name: &'static str,
    command: &mut Command,
    detail: impl Fn(&str) -> String,
) -> Check {
    let (ok, detail) = match command.output() {
        Ok(output) if output.status.success() => {
            (true, detail(&String::from_utf8_lossy(&output.stdout)))
        }
        Ok(output) => (
            false,
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ),
        Err(err) => (false, err.to_string()),
    };
    Check { name, ok, detail }
}

/// Clones a scratch file inside the state directory, where every golden and target lives.
fn reflink_check(location: &Location) -> Check {
    let result = adopt::reflinks_work(&location.state_dir);
    Check {
        name: "reflink",
        ok: result.is_ok() || cfg!(unix),
        detail: match result {
            Ok(()) => "the state directory's filesystem clones files".to_owned(),
            Err(err) if cfg!(unix) => format!(
                "the state directory's filesystem can't clone files ({err}): worktrees are seeded \
                 by hardlinking the golden's compiled artifacts and copying the rest; \
                 goldens are built fresh. btrfs or XFS clone everything"
            ),
            Err(err) => format!(
                "the state directory's filesystem can't clone files ({err}): seeding needs ReFS \
                 or a Dev Drive, so worktrees build cold; set AGENT_HEARTH_STATE_DIR to a \
                 directory on one"
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Sandbox;

    #[test]
    fn reports_a_missing_golden_then_a_healthy_worktree() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let repo = s.location("repo");
        let report = doctor(&repo).unwrap();
        let failed: Vec<_> = report
            .checks
            .iter()
            .filter(|c| !c.ok)
            .map(|c| c.name)
            .collect();
        assert_eq!(failed, ["golden"]);
        assert_eq!(report.seeded_from, None);

        let (golden, _) = golden::refresh(&repo, "main").unwrap();
        adopt::adopt(&repo).unwrap();
        let report = doctor(&repo).unwrap();
        assert!(report.ok(), "{:?}", report.checks);
        assert_eq!(report.seeded_from, Some(golden.id));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["checks"][0]["ok"], true);
    }

    #[test]
    fn advises_line_tables_and_optimised_dependencies_only_where_missing() {
        let s = Sandbox::new();
        let advice = |manifest: &str, config: &str| {
            s.write("w/Cargo.toml", manifest);
            s.write("w/.cargo/config.toml", config);
            profile_advice(&s.path("w"))
        };
        assert_eq!(advice("[package]\nname = \"a\"\n", "").len(), 2);
        // atuin: a few dependencies optimised by name is not all of them.
        let named = "[profile.dev.package]\nlibsqlite3-sys.opt-level = 3\n";
        assert_eq!(advice(named, "").len(), 2);
        let tuned = "[profile.dev]\ndebug = \"line-tables-only\"\n\
                     [profile.dev.package.\"*\"]\ndebug = false\nopt-level = 1\n";
        assert!(advice(tuned, "").is_empty());
        // ruff: everything at opt-level 1 counts, and line tables; dependencies still have them.
        let ruff = "[profile.dev]\nopt-level = 1\ndebug = \"line-tables-only\"\n";
        let ruff_advice = advice(ruff, "");
        assert_eq!(ruff_advice.len(), 1, "{ruff_advice:?}");
        assert!(ruff_advice[0].starts_with("dependencies"));
        // .cargo/config.toml wins over Cargo.toml.
        assert_eq!(advice(tuned, "[profile.dev]\ndebug = true\n").len(), 1);
    }
}
