//! `test --affected`: nextest on the reverse dependencies of the packages changed since this
//! worktree's last passing test run, when they are at most half the workspace; otherwise every
//! test, with `cargo test`. Compiling stays `--workspace`, so features unify as in the golden;
//! `-p` would rebuild.
//!
//! ```text
//! <state>/tested/<worktree-id>.json   file hashes at the last passing test run or settle
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adopt;
use crate::check::Snapshot;
use crate::locate::Location;

/// Changes to these affect every package.
const EVERYTHING: [&str; 4] = [
    "Cargo.lock",
    "Cargo.toml",
    "rust-toolchain",
    "rust-toolchain.toml",
];

#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// No passing run to compare with, or a change that touches every package.
    All,
    /// The packages whose files changed.
    Packages(BTreeSet<String>),
    /// Nothing that any package builds from changed.
    Nothing,
}

#[derive(Serialize, Deserialize)]
struct Tested {
    files: BTreeMap<String, String>,
}

/// What to test, comparing the worktree with its last passing run.
pub fn plan(location: &Location, now: &Snapshot) -> Result<Plan> {
    let Some(tested) = read_tested(location)? else {
        return Ok(Plan::All);
    };
    let changed: Vec<&String> = now
        .files
        .iter()
        .filter(|(path, hash)| tested.files.get(*path) != Some(hash))
        .map(|(path, _)| path)
        .chain(
            tested
                .files
                .keys()
                .filter(|path| !now.files.contains_key(*path)),
        )
        .collect();
    let members = members(&location.worktree)?;
    Ok(match plan_for(&changed, &members) {
        Plan::Packages(packages) if most_of_workspace(&packages, &members) => Plan::All,
        plan => plan,
    })
}

fn plan_for(changed: &[&String], members: &[Member]) -> Plan {
    let mut packages = BTreeSet::new();
    for path in changed {
        if EVERYTHING.contains(&path.as_str()) || path.starts_with(".cargo/") {
            return Plan::All;
        }
        // The member with the longest directory containing the file owns it.
        let owner = members
            .iter()
            .filter(|member| member.dir.is_empty() || path.starts_with(&format!("{}/", member.dir)))
            .max_by_key(|member| member.dir.len());
        if let Some(member) = owner {
            packages.insert(member.name.clone());
        }
    }
    if packages.is_empty() {
        Plan::Nothing
    } else {
        Plan::Packages(packages)
    }
}

/// A workspace member.
pub struct Member {
    pub name: String,
    /// Its directory relative to the workspace root.
    pub dir: String,
    /// The packages it depends on, dev-dependencies included.
    pub deps: Vec<String>,
}

/// The workspace members of `worktree`.
pub fn members(worktree: &Path) -> Result<Vec<Member>> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(worktree)
        .output()
        .context("running cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let metadata: Value =
        serde_json::from_slice(&output.stdout).context("parsing cargo metadata")?;
    let root = Path::new(metadata["workspace_root"].as_str().unwrap_or_default());
    let mut members = Vec::new();
    for package in metadata["packages"].as_array().into_iter().flatten() {
        let (Some(name), Some(manifest)) =
            (package["name"].as_str(), package["manifest_path"].as_str())
        else {
            continue;
        };
        let dir = Path::new(manifest).parent().unwrap_or(root);
        let dir = dir.strip_prefix(root).unwrap_or(dir);
        let deps = package["dependencies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|dep| dep["name"].as_str().map(String::from))
            .collect();
        members.push(Member {
            name: name.to_owned(),
            dir: dir.to_string_lossy().replace('\\', "/"),
            deps,
        });
    }
    Ok(members)
}

/// Whether the members in `changed`, with every member that depends on them, are more than half
/// of the workspace. nextest runs each test in its own process, which costs more than it saves
/// then: with most members affected, `--affected` took longer than the full suite.
fn most_of_workspace(changed: &BTreeSet<String>, members: &[Member]) -> bool {
    let mut reached = changed.clone();
    loop {
        let before = reached.len();
        for member in members {
            if member.deps.iter().any(|dep| reached.contains(dep)) {
                reached.insert(member.name.clone());
            }
        }
        if reached.len() == before {
            let affected = members.iter().filter(|m| reached.contains(&m.name)).count();
            return affected * 2 > members.len();
        }
    }
}

/// Whether `cargo nextest` runs here.
pub fn nextest_available(worktree: &Path) -> bool {
    Command::new("cargo")
        .args(["nextest", "--version"])
        .current_dir(worktree)
        .output()
        .is_ok_and(|output| output.status.success())
}

/// The cargo arguments that carry out `plan`, or `None` when there is nothing to run.
pub fn cargo_args(plan: &Plan, nextest: bool, extra: &[String]) -> Option<Vec<String>> {
    let mut args: Vec<String> = match (plan, nextest) {
        (Plan::Nothing, _) => return None,
        // nextest only for a part of the workspace: for all of it, `cargo test` runs the tests
        // in threads instead of a process each, which was faster (see `most_of_workspace`).
        (Plan::Packages(packages), true) => {
            let filter: Vec<String> = packages
                .iter()
                .map(|name| format!("rdeps(={name})"))
                .collect();
            [
                "nextest",
                "run",
                "--workspace",
                "--status-level",
                "fail",
                "--final-status-level",
                "fail",
                "-E",
                &filter.join(" | "),
            ]
            .map(String::from)
            .to_vec()
        }
        _ => ["test", "--workspace", "--quiet"]
            .map(String::from)
            .to_vec(),
    };
    args.extend(extra.iter().cloned());
    Some(args)
}

/// Remembers `snapshot` as the worktree's last passing run.
pub fn record_passing(location: &Location, snapshot: &Snapshot) -> Result<()> {
    let path = tested_path(location);
    fs::create_dir_all(path.parent().context("tested record has no parent")?)?;
    let json = serde_json::to_vec(&Tested {
        files: snapshot.files.clone(),
    })?;
    fs::write(&path, json).with_context(|| format!("writing {}", path.display()))
}

fn read_tested(location: &Location) -> Result<Option<Tested>> {
    let path = tested_path(location);
    match fs::read(&path) {
        Ok(json) => Ok(Some(
            serde_json::from_slice(&json).with_context(|| format!("reading {}", path.display()))?,
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

fn tested_path(location: &Location) -> PathBuf {
    let target = adopt::target_dir(location);
    let id = target.file_name().unwrap_or_default();
    location
        .state_dir
        .join("tested")
        .join(Path::new(id).with_extension("json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cargo;
    use crate::check::Snapshot;
    use crate::golden;
    use crate::testing::Sandbox;

    fn members() -> Vec<Member> {
        [
            ("root", "", &["a"][..]),
            ("a", "crates/a", &[]),
            ("ab", "crates/ab", &["serde"]),
        ]
        .map(|(name, dir, deps)| Member {
            name: name.to_owned(),
            dir: dir.to_owned(),
            deps: deps.iter().map(|dep| dep.to_string()).collect(),
        })
        .into_iter()
        .collect()
    }

    #[test]
    fn a_change_whose_dependents_are_most_of_the_workspace_tests_everything() {
        let set = |names: &[&str]| names.iter().map(|name| name.to_string()).collect();
        // `root` depends on `a`; nothing depends on `ab` or `root`.
        assert!(most_of_workspace(&set(&["a"]), &members()));
        assert!(!most_of_workspace(&set(&["ab"]), &members()));
        assert!(!most_of_workspace(&set(&["root"]), &members()));
    }

    fn plan(changed: &[&str]) -> Plan {
        let changed: Vec<String> = changed.iter().map(|path| path.to_string()).collect();
        plan_for(&changed.iter().collect::<Vec<_>>(), &members())
    }

    fn packages(names: &[&str]) -> Plan {
        Plan::Packages(names.iter().map(|name| name.to_string()).collect())
    }

    #[test]
    fn maps_files_to_the_member_that_owns_them() {
        assert_eq!(plan(&["crates/a/src/lib.rs"]), packages(&["a"]));
        assert_eq!(
            plan(&["crates/ab/src/lib.rs", "crates/a/build.rs"]),
            packages(&["a", "ab"])
        );
        assert_eq!(plan(&["src/main.rs", "README.md"]), packages(&["root"]));
        assert_eq!(plan(&["Cargo.lock"]), Plan::All);
        assert_eq!(plan(&[".cargo/config.toml"]), Plan::All);
        assert_eq!(plan(&[]), Plan::Nothing);
        let virtual_workspace = [Member {
            name: "a".to_owned(),
            dir: "crates/a".to_owned(),
            deps: Vec::new(),
        }];
        let docs = "docs/guide.md".to_owned();
        assert_eq!(plan_for(&[&docs], &virtual_workspace), Plan::Nothing);
    }

    #[test]
    fn nextest_filters_by_reverse_dependencies() {
        let args = cargo_args(&packages(&["a", "c"]), true, &[]).unwrap();
        assert_eq!(args[..3], ["nextest", "run", "--workspace"]);
        assert_eq!(args[args.len() - 2..], ["-E", "rdeps(=a) | rdeps(=c)"]);
        let fallback = cargo_args(&packages(&["a"]), false, &["--".to_owned()]).unwrap();
        assert_eq!(fallback, ["test", "--workspace", "--quiet", "--"]);
        let all = cargo_args(&Plan::All, true, &[]).unwrap();
        assert_eq!(all, ["test", "--workspace", "--quiet"]);
        assert_eq!(cargo_args(&Plan::Nothing, true, &[]), None);
    }

    /// A workspace with `a`, `b` depending on `a`, and unrelated `c` and `d`, each with one test.
    fn workspace(s: &Sandbox) {
        s.write(
            "repo/Cargo.toml",
            "[workspace]\nmembers = [\"a\", \"b\", \"c\", \"d\"]\nresolver = \"3\"\n",
        );
        for (name, deps) in [
            ("a", ""),
            ("b", "a = { path = \"../a\" }\n"),
            ("c", ""),
            ("d", ""),
        ] {
            s.write(
                &format!("repo/{name}/Cargo.toml"),
                &format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{deps}"
                ),
            );
            s.write(
                &format!("repo/{name}/src/lib.rs"),
                &format!("#[test]\nfn {name}_works() {{}}\n"),
            );
        }
        s.write("repo/.gitignore", "/target/\n");
        s.run("repo", "git", &["init", "-q", "-b", "main"]);
        s.run("repo", "cargo", &["generate-lockfile", "--quiet"]);
        s.run("repo", "git", &["add", "."]);
        s.run("repo", "git", &["commit", "-q", "-m", "workspace"]);
    }

    #[test]
    fn runs_only_the_reverse_dependencies_of_what_changed() {
        let s = Sandbox::new();
        workspace(&s);
        let repo = s.location("repo");
        assert!(
            nextest_available(&repo.worktree),
            "cargo-nextest is in the devenv"
        );
        golden::refresh(&repo, "main").unwrap();
        adopt::adopt(&repo).unwrap();
        let target = adopt::target_dir(&repo);
        let run = |plan: &Plan| {
            let args = cargo_args(plan, true, &[]).unwrap();
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = cargo::run_to(
                &repo,
                &target,
                &repo.worktree,
                &args,
                &[],
                &mut out,
                &mut err,
            );
            assert_eq!(code.unwrap(), 0, "{}", String::from_utf8_lossy(&err));
            String::from_utf8_lossy(&out).into_owned() + &String::from_utf8_lossy(&err)
        };

        let snapshot = Snapshot::take(&repo, "test").unwrap();
        assert_eq!(super::plan(&repo, &snapshot).unwrap(), Plan::All);
        let all = run(&Plan::All);
        assert_eq!(all.matches("test result: ok. 1 passed").count(), 4, "{all}");
        record_passing(&repo, &snapshot).unwrap();

        s.write(
            "repo/a/src/lib.rs",
            "#[test]\nfn a_works() {\n    assert!(true);\n}\n",
        );
        let snapshot = Snapshot::take(&repo, "test").unwrap();
        let plan = super::plan(&repo, &snapshot).unwrap();
        assert_eq!(plan, packages(&["a"]));
        let filtered = run(&plan);
        assert!(filtered.contains("2 tests run: 2 passed"), "{filtered}");
        assert!(filtered.contains("(2 binaries skipped)"), "{filtered}");
        record_passing(&repo, &snapshot).unwrap();
        let unchanged = Snapshot::take(&repo, "test").unwrap();
        assert_eq!(super::plan(&repo, &unchanged).unwrap(), Plan::Nothing);
    }
}
