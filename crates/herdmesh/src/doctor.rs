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
    pub fix: Option<String>,
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
                CheckStatus::Ok => "ok",
                CheckStatus::Fail => "fail",
                CheckStatus::Warn => "warn",
                CheckStatus::Unknown => "unknown",
            };
            let _ = writeln!(
                out,
                "{state:<4} {:<width$}  {}{}",
                c.name,
                c.detail,
                c.fix
                    .as_deref()
                    .map(|f| format!(" -> {f}"))
                    .unwrap_or_default()
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
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| {
                handle.block_on(async { ipc::probe_owner(socket).await == Probe::Ours })
            })
        } else {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map(|rt| rt.block_on(async { ipc::probe_owner(socket).await == Probe::Ours }))
                .unwrap_or(false)
        }
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
    fix: Option<impl Into<String>>,
) -> Check {
    Check {
        name,
        status,
        detail: detail.into(),
        fix: fix.map(Into::into),
    }
}

fn cli_on_path(name: &str, path_var: Option<&OsString>) -> bool {
    let Some(path_var) = path_var else {
        return false;
    };
    std::env::split_paths(path_var).any(|dir| {
        #[cfg(windows)]
        {
            let exts = [".exe", ".cmd", ".bat", ""];
            exts.iter()
                .any(|ext| dir.join(format!("{name}{ext}")).is_file())
        }
        #[cfg(not(windows))]
        {
            dir.join(name).is_file()
        }
    })
}

fn plugin_check(runner: &dyn Runner, herdr: Option<&Path>) -> Check {
    let Some(herdr) = herdr else {
        return check(
            CheckStatus::Unknown,
            "herdr plugin",
            "unknown (herdr unavailable)",
            None::<&str>,
        );
    };
    let Ok(output) = runner.output(herdr.as_os_str(), &["plugin", "list", "--json"]) else {
        return check(
            CheckStatus::Unknown,
            "herdr plugin",
            "unknown",
            None::<&str>,
        );
    };
    if !output.status.success() {
        return check(
            CheckStatus::Unknown,
            "herdr plugin",
            "unknown",
            None::<&str>,
        );
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return check(
            CheckStatus::Unknown,
            "herdr plugin",
            "unknown",
            None::<&str>,
        );
    };
    let entries = value
        .as_array()
        .or_else(|| value.get("plugins").and_then(|v| v.as_array()))
        .or_else(|| {
            value
                .get("result")
                .and_then(|r| r.get("plugins"))
                .and_then(|v| v.as_array())
        });
    let Some(entries) = entries else {
        return check(
            CheckStatus::Unknown,
            "herdr plugin",
            "unknown",
            None::<&str>,
        );
    };
    let found = entries.iter().find(|entry| {
        let is_herdmesh = |v: Option<&serde_json::Value>| {
            v.and_then(|s| s.as_str())
                .map(|s| s.eq_ignore_ascii_case("herdmesh"))
                .unwrap_or(false)
        };
        is_herdmesh(entry.get("plugin_id"))
            || is_herdmesh(entry.get("id"))
            || is_herdmesh(entry.get("name"))
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
                None::<&str>,
            )
        }
        Some(_) => check(
            CheckStatus::Fail,
            "herdr plugin",
            "herdmesh not linked or disabled",
            Some("run: herdr plugin link herdmesh"),
        ),
        None => check(
            CheckStatus::Fail,
            "herdr plugin",
            "herdmesh not linked",
            Some("run: herdr plugin link herdmesh"),
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
                None::<&str>,
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
                        None::<&str>,
                    ));
                }
                _ => checks.push(check(
                    CheckStatus::Warn,
                    "version",
                    "could not read version",
                    None::<&str>,
                )),
            }
        }
        None => checks.push(check(CheckStatus::Warn, "herdr", "not found", None::<&str>)),
    }
    checks.push(plugin_check(runner, herdr.as_deref()));
    let (status, detail, fix) = if let Some(dir) = &cfg.data_dir {
        match check_data_dir(dir) {
            Ok(()) => (CheckStatus::Ok, "writable".to_string(), None),
            Err(e) => (
                CheckStatus::Fail,
                e,
                Some("check data directory permissions or pass --data-dir".to_string()),
            ),
        }
    } else {
        (
            CheckStatus::Fail,
            "data directory unavailable".to_string(),
            Some("set data directory or pass --data-dir".to_string()),
        )
    };
    checks.push(check(status, "data dir", detail, fix));
    let daemon = runner.daemon_running(&cfg.daemon_socket);
    checks.push(check(
        if daemon {
            CheckStatus::Ok
        } else {
            CheckStatus::Fail
        },
        "daemon",
        if daemon { "running" } else { "not running" },
        (!daemon).then_some("run: herdmesh daemon --ensure"),
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
            (!found).then(|| format!("install {agent} and add it to PATH")),
        ));
    }
    let count = cfg.data_dir.as_ref().and_then(|d| {
        let rooms_dir = d.join("rooms");
        if !rooms_dir.exists() {
            Some(0)
        } else {
            std::fs::read_dir(&rooms_dir).ok().map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|e| e.path().is_dir())
                    .count()
            })
        }
    });
    checks.push(match count {
        Some(n) => check(
            CheckStatus::Ok,
            "rooms",
            format!("{n} present"),
            None::<&str>,
        ),
        None => check(
            CheckStatus::Unknown,
            "rooms",
            "unknown (rooms directory unavailable)",
            None::<&str>,
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

    #[test]
    fn handles_real_herdr_plugin_result_envelope() {
        let tmp = TempDir::new().unwrap();
        let herdr = tmp.path().join("herdr");
        std::fs::write(&herdr, b"fake").unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        for agent in ["claude", "codex", "agy"] {
            std::fs::write(
                bin.join(if cfg!(windows) {
                    format!("{agent}.cmd")
                } else {
                    agent.to_string()
                }),
                b"",
            )
            .unwrap();
        }
        let path = std::env::join_paths([bin]).unwrap();
        let runner = FakeRunner {
            responses: vec![
                (
                    herdr.to_string_lossy().into_owned(),
                    vec!["--version".into()],
                    b"herdr 0.9.5\n".to_vec(),
                    true,
                ),
                (
                    herdr.to_string_lossy().into_owned(),
                    vec!["plugin".into(), "list".into(), "--json".into()],
                    br#"{"id":"cli:plugin","result":{"plugins":[{"plugin_id":"herdmesh","name":"Herdmesh","enabled":true}]}}"#.to_vec(),
                    true,
                ),
            ],
            daemon: true,
        };
        let cfg = DoctorConfig {
            herdr_preferred: Some(herdr),
            path_var: Some(path),
            data_dir: Some(tmp.path().to_path_buf()),
            daemon_socket: "test-socket".into(),
        };
        let report = run_with(&cfg, &runner);
        assert!(report.ok(), "{}", report.render());
        assert_eq!(report.checks[2].status, CheckStatus::Ok);
        assert_eq!(report.checks[2].detail, "herdmesh linked and enabled");
        // Verify 0 rooms when rooms subdir doesn't exist yet
        assert!(report.render().contains("0 present"));
    }

    #[test]
    fn plugin_missing_or_disabled_fails_with_fix_hint() {
        let tmp = TempDir::new().unwrap();
        let herdr = tmp.path().join("herdr");
        std::fs::write(&herdr, b"fake").unwrap();
        let runner = FakeRunner {
            responses: vec![(
                herdr.to_string_lossy().into_owned(),
                vec!["plugin".into(), "list".into(), "--json".into()],
                br#"{"plugins":[{"name":"other","enabled":true}]}"#.to_vec(),
                true,
            )],
            daemon: true,
        };
        let check = plugin_check(&runner, Some(&herdr));
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(check.detail, "herdmesh not linked");
        assert_eq!(
            check.fix.as_deref(),
            Some("run: herdr plugin link herdmesh")
        );

        let disabled_runner = FakeRunner {
            responses: vec![(
                herdr.to_string_lossy().into_owned(),
                vec!["plugin".into(), "list".into(), "--json".into()],
                br#"{"plugins":[{"plugin_id":"herdmesh","enabled":false}]}"#.to_vec(),
                true,
            )],
            daemon: true,
        };
        let check_disabled = plugin_check(&disabled_runner, Some(&herdr));
        assert_eq!(check_disabled.status, CheckStatus::Fail);
        assert_eq!(check_disabled.detail, "herdmesh not linked or disabled");
        assert_eq!(
            check_disabled.fix.as_deref(),
            Some("run: herdr plugin link herdmesh")
        );
    }

    #[test]
    fn missing_agent_cli_fails_with_fix_hint() {
        let tmp = TempDir::new().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        // only claude exists, codex and agy missing
        std::fs::write(
            bin.join(if cfg!(windows) {
                "claude.exe"
            } else {
                "claude"
            }),
            b"",
        )
        .unwrap();
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
        assert!(!report.ok());
        let rendered = report.render();
        assert!(rendered.contains("install codex and add it to PATH"));
        assert!(rendered.contains("install agy and add it to PATH"));
    }
}
