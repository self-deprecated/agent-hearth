//! An optional toolchain for every build Agent Hearth runs, which `setup --nightly` chooses: a
//! pinned nightly, with the parallel front end (`-Zthreads`) where it paid. On Linux with 32
//! cores they made builds after an edit 16–21% faster than stable on atuin and ruff
//! (`bench/settings-*-linux2.txt`). The repository's own toolchain is left alone: humans and CI
//! keep it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs, io};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::golden;

const FILE: &str = "toolchain.json";
/// Set for cargo when `-Zthreads` is on: Agent Hearth is then also cargo's `RUSTC_WRAPPER`, and
/// adds `-Zthreads=<this>` to every compiler run. A flag in `RUSTFLAGS` would replace the
/// repository's own rustflags, as cargo reads them from one place only.
pub const THREADS_VAR: &str = "AGENT_HEARTH_RUSTC_THREADS";
/// A `RUSTC_WRAPPER` that was set before Agent Hearth's, which it runs in turn.
const WRAPPED_VAR: &str = "AGENT_HEARTH_RUSTC_WRAPPER";
/// Where `-Zthreads` was measured to pay: 32 cores on Linux, not 8 on an M2
/// (`bench/settings-atuin-mac2.txt`). Between them it is unmeasured.
const THREADS_FROM_CORES: usize = 16;
const THREADS: u32 = 8;
/// How many days back from today `setup --nightly` looks for a nightly with clippy and rustfmt.
const NIGHTLY_DAYS: u64 = 14;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Toolchain {
    /// A rustup toolchain name, or `rustc -V` for a directory given by path.
    pub name: String,
    /// The directory holding its `cargo`, `rustc`, `cargo-clippy` and `rustfmt`.
    pub bin: PathBuf,
    /// `-Zthreads` for every compiler run, when it is on.
    pub threads: Option<u32>,
}

/// The toolchain this repository's builds use, when it is not the repository's own.
pub fn load(state_dir: &Path) -> Result<Option<Toolchain>> {
    let path = state_dir.join(FILE);
    match fs::read(&path) {
        Ok(json) => Ok(Some(
            serde_json::from_slice(&json).with_context(|| format!("reading {}", path.display()))?,
        )),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Makes `toolchain` the one every build uses; `None` goes back to the repository's own.
pub fn store(state_dir: &Path, toolchain: Option<&Toolchain>) -> Result<()> {
    let path = state_dir.join(FILE);
    match toolchain {
        Some(toolchain) => {
            fs::create_dir_all(state_dir)
                .with_context(|| format!("creating {}", state_dir.display()))?;
            golden::write_atomically(&path, &serde_json::to_vec_pretty(toolchain)?)
        }
        None => match fs::remove_file(&path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => {
                Err(err).with_context(|| format!("removing {}", path.display()))
            }
            _ => Ok(()),
        },
    }
}

impl Toolchain {
    /// Points `command`, and every cargo and compiler it runs, at this toolchain: its directory
    /// first on `PATH`, and `RUSTC` and `RUSTDOC` set, which cargo would otherwise prefer.
    pub fn apply(&self, command: &mut Command) {
        let rest = env::var_os("PATH").unwrap_or_default();
        let path =
            env::join_paths(std::iter::once(self.bin.clone()).chain(env::split_paths(&rest)))
                .unwrap_or(rest);
        command
            .env("PATH", path)
            .env("RUSTC", self.bin.join(exe("rustc")))
            .env("RUSTDOC", self.bin.join(exe("rustdoc")));
        if let (Some(threads), Ok(agent_hearth)) = (self.threads, env::current_exe()) {
            if let Some(wrapper) = env::var_os("RUSTC_WRAPPER").filter(|w| !w.is_empty()) {
                command.env(WRAPPED_VAR, wrapper);
            }
            command
                .env("RUSTC_WRAPPER", agent_hearth)
                .env(THREADS_VAR, threads.to_string());
        }
    }

    /// Fails unless `bin` has every program Agent Hearth runs.
    fn check(bin: &Path) -> Result<()> {
        for program in ["cargo", "rustc", "cargo-clippy", "rustfmt"] {
            if !bin.join(exe(program)).is_file() {
                bail!("{} has no {program}", bin.display());
            }
        }
        Ok(())
    }
}

/// What a gate's verdicts depend on besides the code: the toolchain, when it is not the
/// repository's own (whose verdicts keep their keys).
pub fn gate_key(toolchain: Option<&Toolchain>) -> String {
    toolchain.map_or_else(String::new, |toolchain| {
        format!("; toolchain {}", toolchain.name)
    })
}

/// `program` with the toolchain applied, when there is one.
pub fn command(program: &str, toolchain: Option<&Toolchain>) -> Command {
    let mut command = Command::new(program);
    if let Some(toolchain) = toolchain {
        toolchain.apply(&mut command);
    }
    command
}

fn exe(name: &str) -> String {
    format!("{name}{}", env::consts::EXE_SUFFIX)
}

/// When cargo runs Agent Hearth as its `RUSTC_WRAPPER` (`args`: the compiler, then its
/// arguments), runs the compiler with `-Zthreads` and returns its exit code.
pub fn wrap_rustc(args: &[OsString]) -> Option<ExitCode> {
    let threads = env::var(THREADS_VAR).ok()?;
    let (compiler, rest) = args.split_first()?;
    let (program, args) = wrapped(compiler, rest, &threads, env::var_os(WRAPPED_VAR));
    let mut command = Command::new(program);
    command
        .args(args)
        .env_remove(THREADS_VAR)
        .env_remove(WRAPPED_VAR);
    #[cfg(unix)]
    {
        let err = std::os::unix::process::CommandExt::exec(&mut command);
        eprintln!(
            "agent-hearth: running {}: {err}",
            compiler.to_string_lossy()
        );
        Some(ExitCode::from(2))
    }
    #[cfg(not(unix))]
    Some(match command.status() {
        Ok(status) => ExitCode::from(status.code().map_or(2, |code| code as u8)),
        Err(err) => {
            eprintln!(
                "agent-hearth: running {}: {err}",
                compiler.to_string_lossy()
            );
            ExitCode::from(2)
        }
    })
}

/// The program and arguments the wrapper runs: an earlier wrapper, if there was one, then the
/// compiler (rustc, or clippy's driver, which cargo puts first), then its arguments and
/// `-Zthreads`.
fn wrapped(
    compiler: &OsString,
    rest: &[OsString],
    threads: &str,
    earlier: Option<OsString>,
) -> (OsString, Vec<OsString>) {
    let mut args = Vec::new();
    let program = match earlier {
        Some(earlier) => {
            args.push(compiler.clone());
            earlier
        }
        None => compiler.clone(),
    };
    args.extend(rest.iter().cloned());
    args.push(format!("-Zthreads={threads}").into());
    (program, args)
}

/// `-Zthreads` for a machine with `cores`: on where it was measured to pay.
pub fn threads_for(cores: usize) -> Option<u32> {
    (cores >= THREADS_FROM_CORES).then_some(THREADS)
}

/// The toolchain `setup --nightly [spec]` means: a directory holding `cargo`, a rustup toolchain
/// name, or, with none, the newest dated nightly from the last two weeks that has clippy and
/// rustfmt, installed with rustup.
pub fn resolve(spec: Option<&str>) -> Result<Toolchain> {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    if let Some(dir) = spec.map(Path::new).filter(|path| path.is_dir()) {
        let bin = if dir.join(exe("cargo")).is_file() {
            dir.to_path_buf()
        } else {
            dir.join("bin")
        };
        Toolchain::check(&bin)?;
        let version = capture(Command::new(bin.join(exe("rustc"))).arg("-V"))?;
        let nightly = version.contains("-nightly") || version.contains("-dev");
        return Ok(Toolchain {
            name: version.trim().to_owned(),
            bin,
            threads: threads_for(cores).filter(|_| nightly),
        });
    }
    let names = match spec {
        Some(name) => vec![name.to_owned()],
        None => {
            let today = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("reading the clock")?
                .as_secs()
                / 86_400;
            (0..NIGHTLY_DAYS)
                .map(|back| format!("nightly-{}", date(today - back)))
                .collect()
        }
    };
    let mut last = None;
    for name in &names {
        let installed = Command::new("rustup")
            .args(["toolchain", "install", name, "--profile", "minimal"])
            .args(["--component", "clippy", "--component", "rustfmt"])
            .output();
        match installed {
            Err(err) if err.kind() == io::ErrorKind::NotFound => bail!(
                "rustup not found: pass the toolchain's directory instead, as `agent-hearth \
                 setup --nightly <dir with bin/cargo>`"
            ),
            Err(err) => return Err(err).context("running rustup"),
            Ok(output) if output.status.success() => {
                let sysroot = capture(
                    Command::new("rustup")
                        .args(["run", name, "rustc"])
                        .args(["--print", "sysroot"]),
                )?;
                let bin = PathBuf::from(sysroot.trim()).join("bin");
                Toolchain::check(&bin)?;
                return Ok(Toolchain {
                    name: name.clone(),
                    bin,
                    threads: threads_for(cores).filter(|_| name.starts_with("nightly")),
                });
            }
            Ok(output) => last = Some(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
        }
    }
    bail!(
        "no toolchain {} with clippy and rustfmt: {}",
        if spec.is_some() {
            &names[0]
        } else {
            "nightly of the last two weeks"
        },
        last.unwrap_or_default()
    )
}

/// `YYYY-MM-DD` of the day `days` after 1970-01-01 (Howard Hinnant's `civil_from_days`).
fn date(days: u64) -> String {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn capture(command: &mut Command) -> Result<String> {
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

    #[test]
    fn a_stored_toolchain_loads_back_and_none_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap(), None);
        let toolchain = Toolchain {
            name: "nightly-2026-11-01".to_owned(),
            bin: PathBuf::from("/toolchains/nightly/bin"),
            threads: Some(8),
        };
        store(dir.path(), Some(&toolchain)).unwrap();
        assert_eq!(load(dir.path()).unwrap(), Some(toolchain));
        store(dir.path(), None).unwrap();
        store(dir.path(), None).unwrap();
        assert_eq!(load(dir.path()).unwrap(), None);
    }

    #[test]
    fn applying_a_toolchain_puts_it_first_and_wraps_rustc_only_with_threads() {
        let mut toolchain = Toolchain {
            name: "nightly".to_owned(),
            bin: PathBuf::from("/toolchains/nightly/bin"),
            threads: None,
        };
        let envs = |toolchain: &Toolchain| {
            let mut command = Command::new("cargo");
            toolchain.apply(&mut command);
            command
                .get_envs()
                .map(|(key, value)| (key.to_owned(), value.map(OsString::from)))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let plain = envs(&toolchain);
        let path = plain[&OsString::from("PATH")].clone().unwrap();
        assert_eq!(
            env::split_paths(&path).next(),
            Some(PathBuf::from("/toolchains/nightly/bin"))
        );
        assert_eq!(
            plain[&OsString::from("RUSTC")],
            Some(OsString::from(format!(
                "/toolchains/nightly/bin/{}",
                exe("rustc")
            )))
        );
        assert!(!plain.contains_key(&OsString::from(THREADS_VAR)));
        assert!(!plain.contains_key(&OsString::from("RUSTC_WRAPPER")));

        toolchain.threads = Some(8);
        let threaded = envs(&toolchain);
        assert_eq!(
            threaded[&OsString::from(THREADS_VAR)],
            Some(OsString::from("8"))
        );
        assert_eq!(
            threaded[&OsString::from("RUSTC_WRAPPER")],
            Some(env::current_exe().unwrap().into_os_string())
        );
    }

    #[test]
    fn the_wrapper_adds_threads_and_runs_an_earlier_wrapper_first() {
        let os = |s: &str| OsString::from(s);
        let rest = [os("--crate-name"), os("tiny")];
        assert_eq!(
            wrapped(&os("/bin/rustc"), &rest, "8", None),
            (
                os("/bin/rustc"),
                vec![os("--crate-name"), os("tiny"), os("-Zthreads=8")]
            )
        );
        assert_eq!(
            wrapped(&os("clippy-driver"), &rest, "4", Some(os("sccache"))),
            (
                os("sccache"),
                vec![
                    os("clippy-driver"),
                    os("--crate-name"),
                    os("tiny"),
                    os("-Zthreads=4")
                ]
            )
        );
    }

    #[test]
    fn threads_are_on_only_where_they_were_measured_to_pay() {
        assert_eq!(threads_for(8), None);
        assert_eq!(threads_for(32), Some(8));
    }

    #[test]
    fn days_since_the_epoch_are_dates() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(20_395), "2025-11-03");
        assert_eq!(date(20_760), "2026-11-03");
        assert_eq!(date(11_016), "2000-02-29");
    }
}
