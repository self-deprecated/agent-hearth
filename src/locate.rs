//! Finds the worktree, the repository it belongs to, and where Agent Hearth keeps their state.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::{env, fs, io};

use anyhow::{Context, Result, anyhow, bail};

/// A worktree and the directories Agent Hearth uses for its repository.
#[derive(Debug)]
pub struct Location {
    /// Root of the jj workspace or git worktree.
    pub worktree: PathBuf,
    /// Which tool found the worktree; revisions are resolved with it.
    pub vcs: Vcs,
    /// The repository's Git directory, shared by all of its worktrees.
    pub git_dir: PathBuf,
    /// Identifies the repository, not the worktree, so all worktrees share goldens and the pin.
    pub repo_id: String,
    pub state_dir: PathBuf,
    /// The one fixed build path, where [`pin_problem`] finds none.
    pub pin: Option<PathBuf>,
    base: BaseDirs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vcs {
    Jj,
    Git,
}

impl Vcs {
    pub fn name(self) -> &'static str {
        match self {
            Vcs::Jj => "jj",
            Vcs::Git => "git",
        }
    }
}

impl Location {
    /// Finds the worktree containing `dir`.
    pub fn find(dir: &Path) -> Result<Self> {
        Self::find_with(dir, &BaseDirs::from_env()?)
    }

    pub(crate) fn base(&self) -> &BaseDirs {
        &self.base
    }

    /// Finds the worktree containing `dir`, keeping state where this location keeps it.
    pub fn find_alike(&self, dir: &Path) -> Result<Self> {
        Self::find_with(dir, &self.base)
    }

    pub(crate) fn find_with(dir: &Path, base: &BaseDirs) -> Result<Self> {
        let (worktree, git_dir, vcs) = find_worktree(dir)?;
        let repo_id = repo_id(&git_dir);
        Ok(Self {
            state_dir: base.state.join(&repo_id),
            pin: pin_problem()
                .is_none()
                .then(|| base.cache.join(&repo_id).join("pin")),
            worktree,
            vcs,
            git_dir,
            repo_id,
            base: base.clone(),
        })
    }

    /// The pin, with what it must show besides the worktree and its build directory, as (real
    /// path, path under the pin); `None` without a pin, or when this worktree cannot build at it.
    /// Cargo reads `.cargo/config.toml` in every parent of the directory it runs in, and resolves
    /// `path` dependencies from the manifest's: at the pin, those are the pin's parents. So the
    /// configuration above the checkout is mounted over the pin's own parents, closest first
    /// (at most two, in Agent Hearth's cache directory; `$CARGO_HOME`'s, and any in a parent of
    /// the pin too, cargo reads anyway), and each path dependency outside the worktree where
    /// its relative path points from the pin. One that points further out than the cache
    /// directory cannot be mounted: then the worktree builds at its own path.
    pub fn pinned(&self) -> Option<(&Path, Vec<(PathBuf, PathBuf)>)> {
        let pin = self.pin.as_deref()?;
        let cargo_home = env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
        let configs = self
            .worktree
            .ancestors()
            .skip(1)
            .filter(|dir| !pin.starts_with(dir))
            // Of both, cargo reads the one without an extension.
            .filter_map(|dir| {
                ["config", "config.toml"]
                    .map(|name| dir.join(".cargo").join(name))
                    .into_iter()
                    .find(|path| path.is_file())
            })
            .filter(|path| path.parent() != cargo_home.as_deref());
        let mut mounts = Vec::new();
        for (config, dir) in configs.zip(pin.ancestors()) {
            let mount = dir.join(".cargo").join(config.file_name()?);
            mounts.push((config, mount));
        }
        let (src, target, room) = (pin.join("src"), pin.join("target"), pin.parent()?);
        for dependency in outside_path_dependencies(self) {
            let relative = dependency.strip_prefix(&self.worktree).ok();
            let mount = match relative {
                Some(_) => continue,
                None => normalise(&src.join(diff(&self.worktree, &dependency)?)),
            };
            if !mount.starts_with(room) || mount.starts_with(&src) || mount.starts_with(&target) {
                return None;
            }
            if !mounts.iter().any(|(_, existing)| *existing == mount) {
                mounts.push((dependency, mount));
            }
        }
        Some((pin, mounts))
    }

    /// For a checkout of the repository that lives elsewhere (a golden's source tree, a slot), links
    /// each `path` dependency outside the worktree to where its relative path points from there,
    /// so that it resolves as from the worktree; unpinned, nothing else would make it (a golden
    /// of a crate with `path = "../shared"` failed to build on macOS). Only inside the state
    /// directory, and never over a file already there: a dependency that would point elsewhere
    /// is left unresolved, and cargo says so.
    pub fn link_outside_dependencies(&self, elsewhere: &Path) -> Result<()> {
        #[cfg(unix)]
        for dependency in outside_path_dependencies(self) {
            let Some(relative) = diff(&self.worktree, &dependency) else {
                continue;
            };
            let link = normalise(&elsewhere.join(relative));
            if !link.starts_with(&self.state_dir) || link.starts_with(elsewhere) {
                continue;
            }
            if fs::symlink_metadata(&link).is_ok() {
                continue;
            }
            if let Some(parent) = link.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::os::unix::fs::symlink(&dependency, &link)
                .with_context(|| format!("linking {}", link.display()))?;
        }
        #[cfg(not(unix))]
        let _ = elsewhere;
        Ok(())
    }

    /// Creates the state directory and the pin's empty mount points. Safe to run concurrently.
    pub fn create_dirs(&self) -> Result<()> {
        let mut dirs = vec![self.state_dir.clone()];
        if let Some(pin) = &self.pin {
            dirs.extend([pin.join("src"), pin.join("target")]);
        }
        for dir in dirs {
            fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        // An agent's sandbox may leave it readable but not writable, and cargo would fail later.
        let probe = self
            .state_dir
            .join(format!(".writable-{}", std::process::id()));
        fs::write(&probe, "")
            .with_context(|| format!("writing in {}", self.state_dir.display()))?;
        fs::remove_file(&probe).with_context(|| format!("removing {}", probe.display()))
    }
}

/// When `err` is a write agent-hearth's state directory refused, says how to allow it: agent
/// sandboxes allow writes to the working directory only, unless told otherwise
/// (`bench/sandbox-commands-*.txt`).
pub fn unwritable_hint(err: &anyhow::Error, state: &Path) -> Option<String> {
    let refused = err.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::ReadOnlyFilesystem | std::io::ErrorKind::PermissionDenied
            )
        })
    });
    (refused && format!("{err:#}").contains(&*state.to_string_lossy())).then(|| {
        format!(
            "{} must be writable; in an agent's sandbox, the user allows it: Codex \
             `sandbox_workspace_write.writable_roots`, Claude Code `sandbox.filesystem.allowWrite`",
            state.display()
        )
    })
}

/// Returns the canonical worktree root containing `dir` and its repository's Git directory,
/// asking jj first and git second.
fn find_worktree(dir: &Path) -> Result<(PathBuf, PathBuf, Vcs)> {
    let dir = fs::canonicalize(dir).with_context(|| format!("resolving {}", dir.display()))?;
    // Finding the root must not snapshot the working copy, which is slow and races with agents.
    let jj = |args: &[&str]| {
        let args = [&["--ignore-working-copy", "--color=never"][..], args].concat();
        vcs_paths(&dir, "jj", &args)
    };
    if let Some([root]) = jj(&["workspace", "root"])? {
        let Some([git_dir]) = jj(&["git", "root"])? else {
            bail!(
                "{} is a jj workspace without a Git backend, which Agent Hearth does not support",
                root.display()
            );
        };
        return Ok((root, git_dir, Vcs::Jj));
    }
    let git = vcs_paths(
        &dir,
        "git",
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ],
    )?;
    if let Some([root, git_dir]) = git {
        return Ok((root, git_dir, Vcs::Git));
    }
    bail!(
        "{} is not inside a jj workspace or git worktree",
        dir.display()
    )
}

/// Why builds cannot run at the pin on this machine, or `None` when they can. Linux only, and only
/// where bwrap can give a process its own mount namespace: AppArmor (Ubuntu 24.04 and later),
/// containers and agent sandboxes often forbid it. Checked once per process.
pub fn pin_problem() -> Option<&'static str> {
    static PROBLEM: OnceLock<Option<String>> = OnceLock::new();
    PROBLEM
        .get_or_init(|| {
            if !cfg!(target_os = "linux") {
                return Some("the fixed path needs Linux user namespaces".to_owned());
            }
            match Command::new("bwrap")
                .args(["--dev-bind", "/", "/", "true"])
                .output()
            {
                Ok(output) if output.status.success() => None,
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let why = match stderr.trim() {
                        "" => output.status.to_string(),
                        stderr => stderr.to_owned(),
                    };
                    // Ubuntu 24.04 and later keep unprivileged user namespaces from programs
                    // without an AppArmor profile that allows them.
                    let apparmor = fs::read_to_string(
                        "/proc/sys/kernel/apparmor_restrict_unprivileged_userns",
                    )
                    .is_ok_and(|value| value.trim() == "1");
                    let hint = if apparmor {
                        " (AppArmor restricts unprivileged user namespaces: \
                         kernel.apparmor_restrict_unprivileged_userns is 1; an AppArmor profile \
                         for bwrap that allows `userns` lifts it for bwrap alone)"
                    } else {
                        ""
                    };
                    Some(format!(
                        "bwrap cannot make a mount namespace here: {why}{hint}"
                    ))
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => Some(
                    "bwrap not found: install bubblewrap (the `bubblewrap` package) for the fixed \
                     path"
                        .to_owned(),
                ),
                Err(err) => Some(format!("bwrap cannot run: {err}")),
            }
        })
        .as_deref()
}

/// A `jj` command. In tests it carries the current test's sandbox configuration, so the user's
/// own jj settings (immutable commits, identity) never leak into a test.
pub fn jj_command() -> Command {
    #[allow(unused_mut)]
    let mut jj = Command::new("jj");
    #[cfg(test)]
    crate::testing::JJ_ENV.with_borrow(|vars| {
        jj.envs(vars.iter().map(|(name, value)| (name, value)));
    });
    jj
}

/// Runs a VCS command in `dir` that prints `N` paths, one per line, and returns them canonical, or
/// `None` when the tool is not installed or `dir` is not one of its checkouts.
fn vcs_paths<const N: usize>(
    dir: &Path,
    program: &str,
    args: &[&str],
) -> Result<Option<[PathBuf; N]>> {
    let mut command = if program == "jj" {
        jj_command()
    } else {
        Command::new(program)
    };
    let output = match command.args(args).current_dir(dir).output() {
        Ok(output) => output,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("running {program}")),
    };
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8(output.stdout)
        .with_context(|| format!("{program} printed a path that is not UTF-8"))?;
    let paths = stdout
        .lines()
        .map(|line| {
            fs::canonicalize(line).with_context(|| format!("resolving {line} from {program}"))
        })
        .collect::<Result<Vec<_>>>()?;
    let count = paths.len();
    let paths = paths.try_into().map_err(|_| {
        anyhow!(
            "{program} {}: expected {N} paths, got {count}",
            args.join(" ")
        )
    })?;
    Ok(Some(paths))
}

/// Every worktree of a repository, jj or git, shares its canonical Git directory.
fn repo_id(git_dir: &Path) -> String {
    blake3::hash(git_dir.as_os_str().as_encoded_bytes()).to_hex()[..16].to_owned()
}

/// Per-user directories for Agent Hearth's state and cache, each already specific to it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BaseDirs {
    pub(crate) state: PathBuf,
    pub(crate) cache: PathBuf,
}

impl BaseDirs {
    /// This platform's directories, from this process's environment. `AGENT_HEARTH_STATE_DIR`
    /// and `AGENT_HEARTH_CACHE_DIR` replace them, which is how background runs inherit them.
    pub(crate) fn from_env() -> Result<Self> {
        let mut base = Self::for_os(env::consts::OS, |name| env::var_os(name))?;
        let var = |name| {
            env::var_os(name)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        if let Some(state) = var("AGENT_HEARTH_STATE_DIR") {
            base.state = state;
        }
        if let Some(cache) = var("AGENT_HEARTH_CACHE_DIR") {
            base.cache = cache;
        }
        Ok(base)
    }

    /// XDG on Linux and other Unix; the platform's own directories on macOS and Windows.
    /// Relative or empty variables are ignored, as XDG requires.
    fn for_os(os: &str, var: impl Fn(&str) -> Option<OsString>) -> Result<Self> {
        let path = |name: &str| {
            var(name)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        let required = |name: &str| {
            path(name).with_context(|| format!("{name} is not set to an absolute path"))
        };
        let (state, cache) = match os {
            "windows" => {
                let local = required("LOCALAPPDATA")?.join("agent-hearth");
                return Ok(Self {
                    state: local.join("state"),
                    cache: local.join("cache"),
                });
            }
            "macos" => {
                let library = required("HOME")?.join("Library");
                (library.join("Application Support"), library.join("Caches"))
            }
            _ => {
                let state = match path("XDG_STATE_HOME") {
                    Some(state) => state,
                    None => required("HOME")?.join(".local/state"),
                };
                let cache = match path("XDG_CACHE_HOME") {
                    Some(cache) => cache,
                    None => required("HOME")?.join(".cache"),
                };
                (state, cache)
            }
        };
        Ok(Self {
            state: state.join("agent-hearth"),
            cache: cache.join("agent-hearth"),
        })
    }
}

/// The directories of `path` dependencies outside the worktree, from its tracked manifests
/// (dependency tables of every kind, `[patch]` and `[workspace.dependencies]`).
fn outside_path_dependencies(location: &Location) -> Vec<PathBuf> {
    let manifests = crate::check::source_files(location).unwrap_or_default();
    let mut found = Vec::new();
    for manifest in manifests.iter().filter(|path| path.ends_with("Cargo.toml")) {
        let path = location.worktree.join(manifest);
        let Some(table) = fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.parse::<toml::Table>().ok())
        else {
            continue;
        };
        let dir = path.parent().unwrap_or(&location.worktree);
        let mut paths = Vec::new();
        collect_paths(&table, &mut paths);
        for relative in paths {
            let dependency = normalise(&dir.join(relative));
            if !dependency.starts_with(&location.worktree) && !found.contains(&dependency) {
                found.push(dependency);
            }
        }
    }
    found
}

/// The `path` of every dependency in a manifest: `[dependencies]` and its dev, build, target and
/// workspace kinds (`name = { path = "…" }`), and `[patch.<source>]`.
fn collect_paths(table: &toml::Table, paths: &mut Vec<String>) {
    let path_of = |spec: &toml::Value| spec.get("path")?.as_str().map(str::to_owned);
    for (key, value) in table {
        let Some(inner) = value.as_table() else {
            continue;
        };
        if key.ends_with("dependencies") {
            paths.extend(inner.values().filter_map(path_of));
        } else if key == "patch" {
            for source in inner.values().filter_map(toml::Value::as_table) {
                paths.extend(source.values().filter_map(path_of));
            }
        } else {
            // `[target.<cfg>]` and `[workspace]` hold dependency tables of their own.
            collect_paths(inner, paths);
        }
    }
}

/// `path` with `.` and `..` resolved without looking at the filesystem.
fn normalise(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                normal.pop();
            }
            std::path::Component::CurDir => {}
            other => normal.push(other),
        }
    }
    normal
}

/// The relative path from `from` to `to`, both absolute and normalised.
fn diff(from: &Path, to: &Path) -> Option<PathBuf> {
    let common = from
        .components()
        .zip(to.components())
        .take_while(|(a, b)| a == b)
        .count();
    let mut relative = PathBuf::new();
    for _ in from.components().skip(common) {
        relative.push("..");
    }
    for component in to.components().skip(common) {
        relative.push(component);
    }
    Some(relative)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Sandbox;

    fn base_dirs(os: &str, vars: &[(&str, &str)]) -> Result<BaseDirs> {
        BaseDirs::for_os(os, |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.into())
        })
    }

    fn dirs(state: &str, cache: &str) -> BaseDirs {
        BaseDirs {
            state: state.into(),
            cache: cache.into(),
        }
    }

    #[test]
    fn linux_follows_xdg() {
        let vars = [
            ("HOME", "/home/a"),
            ("XDG_STATE_HOME", "/s"),
            ("XDG_CACHE_HOME", "/c"),
        ];
        assert_eq!(
            base_dirs("linux", &vars).unwrap(),
            dirs("/s/agent-hearth", "/c/agent-hearth")
        );
    }

    #[test]
    fn linux_falls_back_to_home_and_ignores_relative_xdg() {
        let vars = [
            ("HOME", "/home/a"),
            ("XDG_STATE_HOME", "rel"),
            ("XDG_CACHE_HOME", ""),
        ];
        assert_eq!(
            base_dirs("linux", &vars).unwrap(),
            dirs(
                "/home/a/.local/state/agent-hearth",
                "/home/a/.cache/agent-hearth"
            )
        );
    }

    #[test]
    fn macos_uses_library() {
        assert_eq!(
            base_dirs("macos", &[("HOME", "/Users/a"), ("XDG_STATE_HOME", "/s")]).unwrap(),
            dirs(
                "/Users/a/Library/Application Support/agent-hearth",
                "/Users/a/Library/Caches/agent-hearth"
            )
        );
    }

    #[test]
    fn windows_uses_local_app_data() {
        // An absolute path on the test host stands in for C:\Users\a\AppData\Local.
        assert_eq!(
            base_dirs("windows", &[("LOCALAPPDATA", "/local")]).unwrap(),
            dirs("/local/agent-hearth/state", "/local/agent-hearth/cache")
        );
    }

    #[test]
    fn missing_home_is_an_error() {
        let err = base_dirs("linux", &[]).unwrap_err();
        assert_eq!(err.to_string(), "HOME is not set to an absolute path");
    }

    #[test]
    fn repo_id_is_stable_and_distinct() {
        assert_eq!(repo_id(Path::new("/r/.git")), repo_id(Path::new("/r/.git")));
        assert_ne!(repo_id(Path::new("/r/.git")), repo_id(Path::new("/s/.git")));
        assert_eq!(repo_id(Path::new("/r/.git")).len(), 16);
    }

    #[cfg(unix)]
    #[test]
    fn an_unwritable_state_directory_fails_with_how_to_allow_it() {
        use std::os::unix::fs::PermissionsExt;
        let s = Sandbox::new();
        s.crate_repo("repo");
        let location = s.location("repo");
        location.create_dirs().unwrap();
        let readonly = fs::Permissions::from_mode(0o555);
        fs::set_permissions(&location.state_dir, readonly).unwrap();
        let result = location.create_dirs();
        fs::set_permissions(&location.state_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let err = result.expect_err("unwritable");
        let hint = unwritable_hint(&err, &s.base().state).expect("a hint");
        assert!(hint.contains("sandbox.filesystem.allowWrite"), "{hint}");
    }

    #[test]
    fn create_dirs_makes_state_and_pin() {
        let s = Sandbox::new();
        s.run("main", "git", &["init", "-q"]);
        let location = s.location("main");
        location.create_dirs().unwrap();
        assert!(location.state_dir.is_dir());
        assert_eq!(location.state_dir, s.path("state").join(&location.repo_id));
        if let Some(pin) = &location.pin {
            assert_eq!(*pin, s.path("cache").join(&location.repo_id).join("pin"));
            assert!(pin.join("src").is_dir() && pin.join("target").is_dir());
        }
        assert_eq!(location.pin.is_some(), cfg!(target_os = "linux"));
    }

    /// Asserts that `dir` is found as the worktree `root` by `vcs` and returns its Git directory.
    fn git_dir_of(s: &Sandbox, dir: &str, root: &str, vcs: Vcs) -> PathBuf {
        let (found, git_dir, found_vcs) = find_worktree(&s.path(dir)).unwrap();
        assert_eq!(found, fs::canonicalize(s.path(root)).unwrap());
        assert_eq!(found_vcs, vcs);
        git_dir
    }

    #[test]
    fn git_worktrees_share_a_git_dir() {
        let s = Sandbox::new();
        s.run("main", "git", &["init", "-q", "-b", "main"]);
        s.run(
            "main",
            "git",
            &["commit", "-q", "--allow-empty", "-m", "init"],
        );
        s.run("main", "git", &["worktree", "add", "-q", "../wt"]);
        fs::create_dir(s.path("main/sub")).unwrap();

        let main = git_dir_of(&s, "main/sub", "main", Vcs::Git);
        assert_eq!(main, fs::canonicalize(s.path("main/.git")).unwrap());
        assert_eq!(git_dir_of(&s, "wt", "wt", Vcs::Git), main);
    }

    #[test]
    fn jj_workspaces_and_git_worktrees_share_a_git_dir() {
        let s = Sandbox::new();
        s.run("main", "jj", &["git", "init", "--colocate"]);
        s.run("main", "jj", &["commit", "-m", "init"]);
        s.run("main", "jj", &["workspace", "add", "../ws"]);
        s.run(
            "main",
            "git",
            &["worktree", "add", "-q", "--detach", "../wt"],
        );

        let main = git_dir_of(&s, "main", "main", Vcs::Jj);
        assert_eq!(main, fs::canonicalize(s.path("main/.git")).unwrap());
        assert_eq!(git_dir_of(&s, "ws", "ws", Vcs::Jj), main);
        assert_eq!(git_dir_of(&s, "wt", "wt", Vcs::Git), main);
    }

    #[test]
    fn outside_any_repository_is_an_error() {
        let s = Sandbox::new();
        let err = find_worktree(s.root()).unwrap_err();
        assert!(
            err.to_string()
                .ends_with("is not inside a jj workspace or git worktree"),
            "{err}"
        );
    }
}
