//! `herdmesh doctor`: a friendly checklist of local setup checks.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use hm_daemon::ipc::{self, Probe};

pub const HERDR_PINNED_RELEASE: &str = "0.9.5-x86_64-pc-windows-msvc";
pub const HERDR_EXE: &str = if cfg!(windows) { "herdr.exe" } else { "herdr" };

#[derive(Debug, Clone)]
pub struct DoctorConfig {
    pub herdr_preferred: Option<PathBuf>,
    pub path_var: Option<OsString>,
    pub data_dir: Option<PathBuf>,
    pub daemon_socket: String,
}

impl DoctorConfig {
    pub fn from_env() -> Self {
        Self {
            herdr_preferred: dirs::home_dir().map(|h| {
                h.join(".herdr")
                    .join("packages")
                    .join("standalone")
                    .join("releases")
                    .join(HERDR_PINNED_RELEASE)
                    .join(HERDR_EXE)
            }),
            path_var: std::env::var_os("PATH"),
            data_dir: dirs::data_dir().map(|d| d.join("herdmesh")),
            daemon_socket: crate::daemon::default_socket(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
    pub fix: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Ok,
    Fail,
    Warn,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    /// Only operational failures make doctor return a nonzero exit code.
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|c| c.status != CheckStatus::Fail)
    }

    pub fn render(&self) -> String {
        let width = self.checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
        let mut out = String::new();
        for c in &self.checks {
            let state = match c.status {
                CheckStatus::Ok => "ok  ",
                CheckStatus::Fail => "fail",
                CheckStatus::Warn => "warn",
                CheckStatus::Unknown => "unknown",
            };
            let _ = writeln!(
                out,
                "{state:<7}{:<width$}  {}{}",
                c.name,
                c.detail,
                c.fix.map(|f| format!(" -> run: {f}")).unwrap_or_default()
            );
        }
        out
    }
}

use std::fmt::Write as _;

pub trait Runner {
    fn output(&self, program: &OsStr, args: &[&str]) -> Result<Output, String>;
    fn daemon_running(&self, socket: &str) -> bool;
}

use std::ffi::OsStr;

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn output(&self, program: &OsStr, args: &[&str]) -> Result<Output, String> {
        let mut command = Command::new(program);
        command.args(args);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        command.output().map_err(|e| e.to_string())
    }

    fn daemon_running(&self, socket: &str) -> bool {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map(|rt| rt.block_on(async { ipc::probe_owner(socket).await == Probe::Ours }))
            .unwrap_or(false)
    }
}

pub fn find_herdr(preferred: Option<&Path>, path_var: Option<&OsString>) -> Option<PathBuf> {
    if let Some(p) = preferred.filter(|p| p.is_file()) {
        return Some(p.to_path_buf());
    }
    std::env::split_paths(path_var?)
        .map(|dir| dir.join(HERDR_EXE))
        .find(|p| p.is_file())
}

fn check(
    status: CheckStatus,
    name: &'static str,
    detail: impl Into<String>,
    fix: Option<&'static str>,
) -> Check {
    Check {
        name,
        status,
        detail: detail.into(),
        fix,
    }
}

fn cli_on_path(name: &str, path_var: Option<&OsString>) -> bool {
    let Some(path_var) = path_var else {
        return false;
    };
    std::env::split_paths(path_var).any(|dir| {
        let candidate = dir.join(if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_owned()
        });
        candidate.is_file()
    })
}

fn plugin_check(runner: &dyn Runner, herdr: Option<&Path>) -> Check {
    let Some(herdr) = herdr else {
        return check(
            CheckStatus::Unknown,
            "herdr plugin",
            "unknown (herdr unavailable)",
            None,
        );
    };
    let Ok(output) = runner.output(herdr.as_os_str(), &["plugin", "list", "--json"]) else {
        return check(CheckStatus::Unknown, "herdr plugin", "unknown", None);
    };
    if !output.status.success() {
        return check(CheckStatus::Unknown, "herdr plugin", "unknown", None);
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return check(CheckStatus::Unknown, "herdr plugin", "unknown", None);
    };
    let entries = value
        .as_array()
        .or_else(|| value.get("plugins").and_then(|v| v.as_array()));
    let Some(entries) = entries else {
        return check(CheckStatus::Unknown, "herdr plugin", "unknown", None);
    };
    let found = entries.iter().find(|entry| {
        entry.get("name").and_then(|v| v.as_str()) == Some("herdmesh")
            || entry.get("id").and_then(|v| v.as_str()) == Some("herdmesh")
    });
    match found {
        Some(entry)
            if entry.get("enabled").and_then(|v| v.as_bool()) == Some(true)
                && entry
                    .get("linked")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true) =>
        {
            check(
                CheckStatus::Ok,
                "herdr plugin",
                "herdmesh linked and enabled",
                None,
            )
        }
        Some(_) => check(
            CheckStatus::Fail,
            "herdr plugin",
            "herdmesh not linked or disabled",
            Some("herdr plugin link herdmesh"),
        ),
        None => check(
            CheckStatus::Fail,
            "herdr plugin",
            "herdmesh not linked",
            Some("herdr plugin link herdmesh"),
        ),
    }
}

pub fn run(cfg: &DoctorConfig) -> Report {
    run_with(cfg, &SystemRunner)
}

pub fn run_with(cfg: &DoctorConfig, runner: &dyn Runner) -> Report {
    let mut checks = Vec::new();
    let herdr = find_herdr(cfg.herdr_preferred.as_deref(), cfg.path_var.as_ref());
    match herdr.as_deref() {
        Some(bin) => {
            checks.push(check(
                CheckStatus::Ok,
                "herdr",
                bin.display().to_string(),
                None,
            ));
            match runner.output(bin.as_os_str(), &["--version"]) {
                Ok(out) if out.status.success() => {
                    let version = String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    checks.push(check(
                        if version.is_empty() {
                            CheckStatus::Warn
                        } else {
                            CheckStatus::Ok
                        },
                        "version",
                        if version.is_empty() {
                            "unknown version".to_string()
                        } else {
                            version
                        },
                        None,
                    ));
                }
                _ => checks.push(check(
                    CheckStatus::Warn,
                    "version",
                    "could not read version",
                    None,
                )),
            }
        }
        None => checks.push(check(CheckStatus::Warn, "herdr", "not found", None)),
    }
    checks.push(plugin_check(runner, herdr.as_deref()));
    let (status, detail) = if let Some(dir) = &cfg.data_dir {
        match check_data_dir(dir) {
            Ok(()) => (CheckStatus::Ok, "writable".to_string()),
            Err(e) => (CheckStatus::Fail, e),
        }
    } else {
        (CheckStatus::Fail, "data directory unavailable".to_string())
    };
    checks.push(check(
        status,
        "data dir",
        detail,
        (status == CheckStatus::Fail).then_some("check the data directory permissions"),
    ));
    let daemon = runner.daemon_running(&cfg.daemon_socket);
    checks.push(check(
        if daemon {
            CheckStatus::Ok
        } else {
            CheckStatus::Fail
        },
        "daemon",
        if daemon { "running" } else { "not running" },
        (!daemon).then_some("herdmesh daemon --ensure"),
    ));
    for agent in ["claude", "codex", "agy"] {
        let found = cli_on_path(agent, cfg.path_var.as_ref());
        checks.push(check(
            if found {
                CheckStatus::Ok
            } else {
                CheckStatus::Fail
            },
            agent,
            if found {
                "found on PATH"
            } else {
                "not found on PATH"
            },
            (!found).then_some("install the CLI and add it to PATH"),
        ));
    }
    let count = cfg
        .data_dir
        .as_ref()
        .and_then(|d| std::fs::read_dir(d.join("rooms")).ok())
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| e.path().is_dir())
                .count()
        });
    checks.push(match count {
        Some(n) => check(CheckStatus::Ok, "rooms", format!("{n} present"), None),
        None => check(
            CheckStatus::Unknown,
            "rooms",
            "unknown (rooms directory unavailable)",
            None,
        ),
    });
    Report { checks }
}

pub fn check_data_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create: {e}"))?;
    let probe = dir.join(".herdmesh-doctor-probe");
    std::fs::write(&probe, b"ok").map_err(|e| format!("not writable: {e}"))?;
    std::fs::remove_file(&probe).map_err(|e| format!("cannot remove probe: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;

    struct FakeRunner {
        responses: Vec<(String, Vec<String>, Vec<u8>, bool)>,
        daemon: bool,
    }
    impl Runner for FakeRunner {
        fn output(&self, program: &OsStr, args: &[&str]) -> Result<Output, String> {
            let args = args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            let found = self
                .responses
                .iter()
                .find(|(p, a, _, _)| p == &program.to_string_lossy() && a == &args);
            let Some((_, _, stdout, success)) = found else {
                return Err("not configured".into());
            };
            #[cfg(not(windows))]
            let status = std::process::ExitStatus::from_raw(if *success { 0 } else { 256 });
            #[cfg(windows)]
            let status = std::process::ExitStatus::from_raw(if *success { 0 } else { 1 });
            Ok(Output {
                status,
                stdout: stdout.clone(),
                stderr: Vec::new(),
            })
        }
        fn daemon_running(&self, _socket: &str) -> bool {
            self.daemon
        }
    }

    #[test]
    fn checklist_checks_each_item_and_renders_fix_hints() {
        let tmp = TempDir::new().unwrap();
        let herdr = tmp.path().join("herdr");
        std::fs::write(&herdr, b"fake").unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(data.join("rooms").join("one")).unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let path = std::env::join_paths([&bin]).unwrap();
        let runner = FakeRunner {
            responses: vec![
                (
                    herdr.to_string_lossy().into_owned(),
                    vec!["--version".into()],
                    b"herdr 1.2\n".to_vec(),
                    true,
                ),
                (
                    herdr.to_string_lossy().into_owned(),
                    vec!["plugin".into(), "list".into(), "--json".into()],
                    br#"[{"name":"herdmesh","enabled":true,"linked":true}]"#.to_vec(),
                    true,
                ),
            ],
            daemon: false,
        };
        let cfg = DoctorConfig {
            herdr_preferred: Some(herdr),
            path_var: Some(path),
            data_dir: Some(data),
            daemon_socket: "test-socket".into(),
        };
        let report = run_with(&cfg, &runner);
        assert!(!report.ok());
        assert_eq!(report.checks.len(), 9);
        assert_eq!(report.checks[0].status, CheckStatus::Ok);
        assert_eq!(report.checks[1].detail, "herdr 1.2");
        assert_eq!(report.checks[2].status, CheckStatus::Ok);
        assert!(report.render().contains("-> run: herdmesh daemon --ensure"));
        assert!(report.render().contains("1 present"));
    }

    #[test]
    fn herdr_missing_is_warning_and_plugin_unknown_is_nonfatal() {
        let tmp = TempDir::new().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        for agent in ["claude", "codex", "agy"] {
            std::fs::write(
                bin.join(if cfg!(windows) {
                    format!("{agent}.exe")
                } else {
                    agent.to_string()
                }),
                b"",
            )
            .unwrap();
        }
        let path = std::env::join_paths([bin]).unwrap();
        let cfg = DoctorConfig {
            herdr_preferred: None,
            path_var: Some(path),
            data_dir: Some(tmp.path().to_path_buf()),
            daemon_socket: "missing".into(),
        };
        let report = run_with(
            &cfg,
            &FakeRunner {
                responses: vec![],
                daemon: true,
            },
        );
        assert_eq!(report.checks[0].status, CheckStatus::Warn);
        assert_eq!(report.checks[1].status, CheckStatus::Unknown);
        assert!(report.ok());
    }
}
