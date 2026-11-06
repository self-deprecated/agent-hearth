//! Test helpers: temporary repositories, isolated from the user's git and jj configuration.

use std::cell::RefCell;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::locate::{BaseDirs, Location};

thread_local! {
    /// Environment for every `jj` the code under test runs on this thread (see `jj_command`).
    pub static JJ_ENV: RefCell<Vec<(String, OsString)>> = const { RefCell::new(Vec::new()) };
}

/// Whether the sandbox's filesystem cannot clone files, saying so past the test harness's
/// capture: a test of cloning then skips.
pub fn no_reflinks(s: &Sandbox) -> bool {
    let Err(err) = crate::adopt::reflinks_work(s.root()) else {
        return false;
    };
    let _ = writeln!(
        std::io::stderr(),
        "skipped: {} cannot clone files ({err}), which this test needs",
        s.root().display()
    );
    true
}

pub struct Sandbox {
    tmp: tempfile::TempDir,
}

impl Sandbox {
    /// Also points this thread's `jj` commands at the sandbox's empty configuration.
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // Without reflinks, worktrees are seeded by hardlinks and goldens are built fresh; the
        // few tests of cloning itself skip, saying so (`no_reflinks`). Say once which way this
        // run seeds, past the test harness's capture.
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
            if let Err(err) = crate::adopt::reflinks_work(tmp.path()) {
                let _ = writeln!(
                    std::io::stderr(),
                    "agent-hearth tests: {} cannot clone files ({err}): worktrees are seeded by \
                     hardlinks, and the tests of cloning skip. Set TMPDIR to a directory on \
                     btrfs, XFS or APFS to run them.",
                    std::env::temp_dir().display()
                );
            }
        });
        let empty = tmp.path().join("empty-config");
        fs::write(&empty, "").unwrap();
        JJ_ENV.set(vec![
            ("JJ_CONFIG".to_owned(), empty.into_os_string()),
            ("JJ_USER".to_owned(), "Test".into()),
            ("JJ_EMAIL".to_owned(), "test@example.com".into()),
        ]);
        Self { tmp }
    }

    pub fn root(&self) -> &Path {
        self.tmp.path()
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.root().join(name)
    }

    /// Writes `contents` to `name`, creating parent directories.
    pub fn write(&self, name: &str, contents: &str) {
        let path = self.path(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// Runs `program` in `dir` (created if missing), asserts it succeeds and returns its stdout.
    pub fn run(&self, dir: &str, program: &str, args: &[&str]) -> String {
        self.run_at(&self.path(dir), program, args)
    }

    /// `run` in any directory, such as a worktree `spawn` placed in a slot.
    pub fn run_at(&self, dir: &Path, program: &str, args: &[&str]) -> String {
        fs::create_dir_all(dir).unwrap();
        let empty = self.path("empty-config");
        let output = Command::new(program)
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", &empty)
            .env("JJ_CONFIG", &empty)
            .envs([
                ("GIT_CONFIG_NOSYSTEM", "1"),
                ("GIT_AUTHOR_NAME", "Test"),
                ("GIT_AUTHOR_EMAIL", "test@example.com"),
                ("GIT_COMMITTER_NAME", "Test"),
                ("GIT_COMMITTER_EMAIL", "test@example.com"),
                ("JJ_USER", "Test"),
                ("JJ_EMAIL", "test@example.com"),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{program} {args:?} failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    /// State and cache directories inside the sandbox.
    pub fn base(&self) -> BaseDirs {
        BaseDirs {
            state: self.path("state"),
            cache: self.path("cache"),
        }
    }

    /// Finds the worktree containing `dir`, keeping Agent Hearth's state and cache in the sandbox.
    pub fn location(&self, dir: &str) -> Location {
        let location = Location::find_with(&self.path(dir), &self.base()).unwrap();
        location.create_dirs().unwrap();
        location
    }

    /// A git repository in `dir` holding one small library crate with a test, committed on `main`.
    pub fn crate_repo(&self, dir: &str) {
        self.write(
            &format!("{dir}/Cargo.toml"),
            "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        self.write(
            &format!("{dir}/src/lib.rs"),
            "pub fn one() -> u32 {\n    1\n}\n\n#[test]\nfn one_is_one() {\n    assert_eq!(one(), 1);\n}\n",
        );
        self.write(&format!("{dir}/.gitignore"), "/target/\n");
        self.run(dir, "git", &["init", "-q", "-b", "main"]);
        self.run(dir, "cargo", &["generate-lockfile", "--quiet"]);
        self.run(dir, "git", &["add", "."]);
        self.run(dir, "git", &["commit", "-q", "-m", "tiny crate"]);
    }
}
