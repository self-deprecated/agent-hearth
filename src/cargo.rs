//! Runs cargo on a source tree with a given build directory, at the fixed path where there is one.

use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::{env, thread};

use anyhow::{Context, Result, anyhow};

use crate::locate::Location;
use crate::toolchain::Toolchain;

/// Returns a `cargo` command, ready for its arguments, that builds `src` into `target` from `cwd`
/// (relative to `src`). With a pin, `src` and `target` are bind-mounted at the pin in a private
/// mount namespace, so every build of the repository sees the same paths, and so is each of
/// `mounts` (real path, path under the pin; [`Location::pinned`]). With `toolchain`, cargo and
/// everything it runs use it.
pub fn command(
    src: &Path,
    target: &Path,
    cwd: &Path,
    pin: Option<&Path>,
    mounts: &[(PathBuf, PathBuf)],
    toolchain: Option<&Toolchain>,
) -> Command {
    let mut command = match pin {
        Some(pin) => {
            let mut bwrap = Command::new("bwrap");
            bwrap
                .args(["--dev-bind", "/", "/", "--bind"])
                .args([src, &pin.join("src")])
                .arg("--bind")
                .args([target, &pin.join("target")]);
            for (real, mount) in mounts {
                // Mount points in the cache directory: an empty file (which is also valid cargo
                // configuration) or directory.
                let dir = real.is_dir();
                if !mount.exists() {
                    let _ = match (dir, mount.parent()) {
                        (true, _) => fs::create_dir_all(mount),
                        (false, Some(parent)) => {
                            fs::create_dir_all(parent).and_then(|()| fs::write(mount, ""))
                        }
                        (false, None) => Ok(()),
                    };
                }
                bwrap
                    .arg(if dir { "--bind" } else { "--ro-bind" })
                    .arg(real)
                    .arg(mount);
            }
            bwrap
                .arg("--chdir")
                .arg(pin.join("src").join(cwd))
                .args(["--", "cargo"])
                .env("CARGO_TARGET_DIR", pin.join("target"))
                .env("PWD", pin.join("src").join(cwd));
            bwrap
        }
        None => {
            let mut cargo = Command::new("cargo");
            cargo
                .current_dir(src.join(cwd))
                .env("CARGO_TARGET_DIR", target);
            cargo
        }
    };
    // Cargo 1.99 turns incremental builds off when CI is set, and agent harnesses often set it.
    command.env("CARGO_INCREMENTAL", "1");
    if let Some(toolchain) = toolchain {
        toolchain.apply(&mut command);
    }
    command
}

/// Runs `command` with inherited stdio and returns its exit status.
pub fn status(command: &mut Command) -> Result<ExitStatus> {
    let program = command.get_program().to_string_lossy().into_owned();
    command.status().map_err(|err| match err.kind() {
        io::ErrorKind::NotFound if program == "bwrap" => {
            anyhow!("bwrap not found: the fixed build path on Linux needs bubblewrap")
        }
        io::ErrorKind::NotFound => anyhow!("{program} not found"),
        _ => anyhow::Error::new(err).context(format!("running {program}")),
    })
}

/// Runs `cargo <args>` for the worktree from `cwd`, with its own build directory `target`, and
/// returns cargo's exit code. Pinned, output has pin paths mapped back to real ones.
/// Fails a build on any warning, without changing the compiler's arguments.
pub const DENY_WARNINGS: [(&str, &str); 1] = [("CARGO_BUILD_WARNINGS", "deny")];

/// Runs `cargo args` for `agent-hearth cargo`. A clippy run whose only clippy arguments are
/// `-D warnings` runs with [`DENY_WARNINGS`] instead: it fails on the same warnings, but clippy
/// arguments make clippy lint again on every run.
pub fn run(location: &Location, target: &Path, cwd: &Path, args: &[String]) -> Result<u8> {
    let (args, envs): (&[String], &[(&str, &str)]) = match deny_warnings_args(args) {
        Some(args) => (args, &DENY_WARNINGS),
        None => (args, &[]),
    };
    run_to(
        location,
        target,
        cwd,
        args,
        envs,
        io::stdout(),
        io::stderr(),
    )
}

/// `args` without a trailing `-- -D warnings`, when it is a clippy run that ends so.
pub fn deny_warnings_args(args: &[String]) -> Option<&[String]> {
    if args.first().map(String::as_str) != Some("clippy") {
        return None;
    }
    let split = args.iter().position(|arg| arg == "--")?;
    let rest: Vec<&str> = args[split + 1..].iter().map(String::as_str).collect();
    matches!(rest[..], ["-D", "warnings"] | ["-Dwarnings"]).then(|| &args[..split])
}

/// [`run`] with extra environment variables, writing cargo's output to `stdout` and `stderr`.
pub fn run_to(
    location: &Location,
    target: &Path,
    cwd: &Path,
    args: &[String],
    envs: &[(&str, &str)],
    stdout: impl Write + Send,
    stderr: impl Write + Send,
) -> Result<u8> {
    let cwd = cwd.strip_prefix(&location.worktree).with_context(|| {
        format!(
            "{} is outside the worktree {}",
            cwd.display(),
            location.worktree.display()
        )
    })?;
    let pinned = location.pinned();
    let pin = pinned.as_ref().map(|(pin, _)| *pin);
    let mounts = pinned
        .as_ref()
        .map_or(&[][..], |(_, mounts)| mounts.as_slice());
    let mut cargo = command(
        &location.worktree,
        target,
        cwd,
        pin,
        mounts,
        location.toolchain.as_ref(),
    );
    let mut mappings = Vec::new();
    match pin {
        Some(pin) => {
            // Absolute worktree paths in arguments must point into the pin, or cargo builds at
            // the real path, cold.
            let worktree = location.worktree.as_os_str().as_encoded_bytes();
            let pinned_src = pin.join("src");
            let pinned_src = pinned_src.as_os_str().as_encoded_bytes();
            cargo.args(args.iter().map(|arg| {
                let mapped = replace_path(arg.as_bytes(), worktree, pinned_src);
                String::from_utf8_lossy(&mapped).into_owned()
            }));
            mappings = [
                (pin.join("target"), target.to_path_buf()),
                (pin.join("src"), location.worktree.clone()),
            ]
            .map(|(from, to)| {
                (
                    from.into_os_string().into_encoded_bytes(),
                    to.into_os_string().into_encoded_bytes(),
                )
            })
            .to_vec();
        }
        None => {
            cargo.args(args);
        }
    }
    // Piped output is no longer a terminal to cargo; keep its colours for one that is.
    if io::stderr().is_terminal() && env::var_os("CARGO_TERM_COLOR").is_none() {
        cargo.env("CARGO_TERM_COLOR", "always");
    }
    let program = cargo.get_program().to_string_lossy().into_owned();
    let mut child = cargo
        .envs(envs.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| match err.kind() {
            io::ErrorKind::NotFound if program == "bwrap" => {
                anyhow!("bwrap not found: the fixed build path on Linux needs bubblewrap")
            }
            io::ErrorKind::NotFound => anyhow!("{program} not found"),
            _ => anyhow::Error::new(err).context(format!("running {program}")),
        })?;
    let (child_out, child_err) = (child.stdout.take(), child.stderr.take());
    thread::scope(|scope| {
        let out = scope.spawn(|| child_out.map(|from| copy_lines(from, stdout, &mappings)));
        let err = scope.spawn(|| child_err.map(|from| copy_lines(from, stderr, &mappings)));
        for copied in [out.join(), err.join()] {
            copied.expect("output copier panicked").transpose()?;
        }
        anyhow::Ok(())
    })?;
    Ok(exit_code(child.wait().context("waiting for cargo")?))
}

/// Copies `from` to `to` a line at a time, replacing each mapped path prefix.
fn copy_lines(from: impl Read, mut to: impl Write, mappings: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
    let mut from = BufReader::new(from);
    let mut line = Vec::new();
    while from.read_until(b'\n', &mut line)? > 0 {
        to.write_all(&rewrite(&line, mappings))?;
        to.flush()?;
        line.clear();
    }
    Ok(())
}

fn rewrite(line: &[u8], mappings: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut line = line.to_vec();
    for (from, to) in mappings {
        line = replace_path(&line, from, to);
    }
    line
}

/// `text` with every occurrence of the path `from` replaced by `to`: whole paths only, so a sibling
/// worktree whose name starts with this one's (`agent1`, `agent10`) is left alone, and so is a
/// longer path that ends in `from`.
fn replace_path(text: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    // Bytes that continue a path's last component, before or after it.
    let continues = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-');
    let mut replaced = Vec::with_capacity(text.len());
    let mut at = 0;
    while let Some(found) = text[at..]
        .windows(from.len())
        .position(|window| window == from)
    {
        let start = at + found;
        let end = start + from.len();
        let whole = !(start > 0 && continues(text[start - 1]))
            && !text.get(end).is_some_and(|&byte| continues(byte));
        replaced.extend_from_slice(&text[at..start]);
        replaced.extend_from_slice(if whole { to } else { from });
        at = end;
    }
    replaced.extend_from_slice(&text[at..]);
    replaced
}

/// Cargo's exit code; a signal becomes 128 + its number, as shells report it.
fn exit_code(status: ExitStatus) -> u8 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128u8.saturating_add(u8::try_from(signal).unwrap_or(0));
    }
    status
        .code()
        .map_or(1, |code| u8::try_from(code).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adopt;
    use crate::golden;
    use crate::testing::Sandbox;

    #[test]
    fn rewrite_maps_every_occurrence() {
        let mappings = [
            (b"/pin/target".to_vec(), b"/state/t1".to_vec()),
            (b"/pin/src".to_vec(), b"/work/wt".to_vec()),
        ];
        assert_eq!(
            rewrite(
                b"/pin/src/a.rs and /pin/target/debug (/pin/src)\n",
                &mappings
            ),
            b"/work/wt/a.rs and /state/t1/debug (/work/wt)\n"
        );
        assert_eq!(rewrite(b"nothing here", &mappings), b"nothing here");
    }

    #[test]
    fn replace_path_maps_whole_paths_only() {
        let (from, to) = (b"/w/agent1".as_slice(), b"/pin/src".as_slice());
        let cases: [(&[u8], &[u8]); 6] = [
            (b"/w/agent1", b"/pin/src"),
            (b"/w/agent1/src/a.rs:3:5", b"/pin/src/src/a.rs:3:5"),
            (
                b"--manifest-path=/w/agent1/Cargo.toml",
                b"--manifest-path=/pin/src/Cargo.toml",
            ),
            (b"\"/w/agent1\"", b"\"/pin/src\""),
            (b"/w/agent10/src/a.rs", b"/w/agent10/src/a.rs"),
            (b"/x/w/agent1-old/a.rs", b"/x/w/agent1-old/a.rs"),
        ];
        for (text, expected) in cases {
            let got = replace_path(text, from, to);
            assert_eq!(got, expected, "{}", String::from_utf8_lossy(text));
        }
    }

    #[test]
    fn reads_the_cargo_configuration_above_the_checkout_at_the_pin_too() {
        let s = Sandbox::new();
        s.write(
            "outer/.cargo/config.toml",
            "[env]\nAH_PROBE = \"from-outer\"\n",
        );
        s.crate_repo("outer/repo");
        s.write(
            "outer/repo/src/main.rs",
            "fn main() {\n    println!(\"{}\", option_env!(\"AH_PROBE\").unwrap_or(\"missing\"));\n}\n",
        );
        let repo = s.location("outer/repo");
        repo.create_dirs().unwrap();
        if let Some((pin, mounts)) = repo.pinned() {
            let mount = pin.join(".cargo/config.toml");
            assert_eq!(mounts, [(s.path("outer/.cargo/config.toml"), mount)]);
        }
        let target = s.path("target");
        std::fs::create_dir_all(&target).unwrap();
        let args = ["run".to_owned(), "--quiet".to_owned()];
        let mut out = Vec::new();
        let code = run_to(
            &repo,
            &target,
            &repo.worktree,
            &args,
            &[],
            &mut out,
            io::sink(),
        );
        assert_eq!(code.unwrap(), 0);
        assert_eq!(String::from_utf8(out).unwrap(), "from-outer\n");
    }

    /// A crate at `dir` whose `main` prints `dependency::value()`, a `path` dependency at
    /// `dependency` (relative to `dir`), committed on main.
    fn with_path_dependency(s: &Sandbox, dir: &str, dependency: &str) -> Location {
        let shared = std::path::Path::new(dir).join(dependency);
        let shared = shared.to_str().unwrap();
        s.write(
            &format!("{shared}/Cargo.toml"),
            "[package]\nname = \"dependency\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        s.write(
            &format!("{shared}/src/lib.rs"),
            "pub fn value() -> u32 {\n    7\n}\n",
        );
        s.crate_repo(dir);
        s.write(
            &format!("{dir}/Cargo.toml"),
            &format!(
                "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                 [dependencies]\ndependency = {{ path = \"{dependency}\" }}\n"
            ),
        );
        s.write(
            &format!("{dir}/src/main.rs"),
            "fn main() {\n    println!(\"{}\", dependency::value());\n}\n",
        );
        s.run(dir, "cargo", &["generate-lockfile", "--quiet"]);
        s.run(dir, "git", &["add", "."]);
        s.run(dir, "git", &["commit", "-q", "-m", "a path dependency"]);
        let location = s.location(dir);
        location.create_dirs().unwrap();
        location
    }

    fn run_prints(location: &Location) -> String {
        let target = crate::adopt::target_dir(location);
        let args = ["run".to_owned(), "--quiet".to_owned()];
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_to(
            location,
            &target,
            &location.worktree,
            &args,
            &[],
            &mut out,
            &mut err,
        );
        assert_eq!(code.unwrap(), 0, "{}", String::from_utf8_lossy(&err));
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn builds_a_path_dependency_outside_the_worktree_at_the_pin_and_in_the_golden() {
        let s = Sandbox::new();
        let repo = with_path_dependency(&s, "work/repo", "../shared");
        if let Some((pin, mounts)) = repo.pinned() {
            assert_eq!(mounts, [(s.path("work/shared"), pin.join("shared"))]);
        }
        golden::refresh(&repo, "main").unwrap();
        adopt::adopt(&repo).unwrap();
        assert_eq!(run_prints(&repo), "7\n");
    }

    #[test]
    fn a_path_dependency_too_far_out_for_the_pin_builds_at_the_worktree_s_own_path() {
        let s = Sandbox::new();
        let repo = with_path_dependency(&s, "a/b/repo", "../../../far");
        assert!(repo.pinned().is_none());
        // The golden builds at its own path too, its dependency linked in the state directory.
        golden::refresh(&repo, "main").unwrap();
        adopt::adopt(&repo).unwrap();
        assert_eq!(run_prints(&repo), "7\n");
    }

    #[test]
    fn only_clippy_ending_in_deny_warnings_moves_it_to_the_environment() {
        let cases = [
            (
                "clippy -p a --all-targets -- -D warnings",
                Some("clippy -p a --all-targets"),
            ),
            ("clippy -- -Dwarnings", Some("clippy")),
            ("clippy -- -D warnings -W clippy::pedantic", None),
            ("clippy --all-targets", None),
            ("test -- -D warnings", None),
        ];
        for (command, expected) in cases {
            let args: Vec<String> = command.split(' ').map(String::from).collect();
            let stripped = deny_warnings_args(&args).map(|args| args.join(" "));
            assert_eq!(stripped.as_deref(), expected, "{command}");
        }
    }

    #[test]
    fn deny_warnings_in_the_environment_fails_clippy_on_a_warning() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        let repo = s.location("repo");
        s.write("repo/src/lib.rs", "pub fn f() {\n    let unused = 1;\n}\n");
        let target = s.path("target");
        std::fs::create_dir_all(&target).unwrap();
        let args = ["clippy".to_owned()];
        let mut err = Vec::new();
        let lint = |envs: &[(&str, &str)], err: &mut Vec<u8>| {
            run_to(&repo, &target, &repo.worktree, &args, envs, io::sink(), err).unwrap()
        };
        assert_eq!(lint(&[], &mut err), 0, "{}", String::from_utf8_lossy(&err));
        let code = lint(&DENY_WARNINGS, &mut err);
        assert_ne!(code, 0, "{}", String::from_utf8_lossy(&err));
    }

    #[test]
    fn runs_pinned_from_a_subdirectory_with_real_paths_and_exit_codes() {
        let s = Sandbox::new();
        s.crate_repo("repo");
        golden::refresh(&s.location("repo"), "main").unwrap();
        s.run("repo", "git", &["worktree", "add", "-q", "../wt"]);
        let wt = s.location("wt");
        assert!(matches!(
            adopt::adopt(&wt).unwrap(),
            adopt::Outcome::Adopted(_)
        ));
        let target = adopt::target_dir(&wt);
        let cargo = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = run_to(
                &wt,
                &target,
                &wt.worktree.join("src"),
                &args,
                &[],
                &mut out,
                &mut err,
            );
            let text = |bytes| String::from_utf8(bytes).unwrap();
            (code.unwrap(), text(out), text(err))
        };

        let (code, out, err) = cargo(&["build", "--message-format=json"]);
        assert_eq!(code, 0, "{err}");
        let manifest = wt.worktree.join("Cargo.toml");
        assert!(out.contains(&*manifest.to_string_lossy()), "{out}");
        if let Some(pin) = &wt.pin {
            assert!(!out.contains(&*pin.to_string_lossy()), "{out}");
            assert!(!err.contains(&*pin.to_string_lossy()), "{err}");
            assert!(out.contains(&*target.to_string_lossy()), "{out}");
        }

        s.write(
            "wt/src/lib.rs",
            "#[test]\nfn fails() {\n    panic!(\"no\");\n}\n",
        );
        let (code, _, err) = cargo(&["test", "--quiet"]);
        assert_eq!(code, 101, "{err}");
    }
}
