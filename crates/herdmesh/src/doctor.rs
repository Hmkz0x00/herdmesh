//! `herdmesh doctor`: environment checks (herdr binary, data dir).
//!
//! All inputs are injected through [`DoctorConfig`] so tests can point at a
//! fake herdr and a temp data dir.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// herdr release the team pins for now (see task card M0-2).
pub const HERDR_PINNED_RELEASE: &str = "0.9.5-x86_64-pc-windows-msvc";

/// Name of the herdr executable on this platform.
pub const HERDR_EXE: &str = if cfg!(windows) { "herdr.exe" } else { "herdr" };

/// Inputs to a doctor run.
#[derive(Debug, Clone)]
pub struct DoctorConfig {
    /// Preferred herdr location, checked before `PATH`.
    pub herdr_preferred: Option<PathBuf>,
    /// Value of `PATH` used for the fallback search.
    pub path_var: Option<OsString>,
    /// herdmesh data dir; created if missing.
    pub data_dir: Option<PathBuf>,
}

impl DoctorConfig {
    /// Real-environment config: standalone herdr install, process `PATH`,
    /// and `<platform data dir>/herdmesh`.
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
        }
    }
}

/// One row of the doctor table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// Result of a doctor run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    /// True when every check passed.
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }

    /// Short fixed-width table, one line per check.
    pub fn render(&self) -> String {
        let width = self.checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
        let mut out = String::new();
        for c in &self.checks {
            let status = if c.ok { "ok  " } else { "FAIL" };
            let _ = writeln!(out, "{status}  {:<width$}  {}", c.name, c.detail);
        }
        out
    }
}

/// Locate herdr: the preferred path if it is a file, else the first match on `PATH`.
pub fn find_herdr(preferred: Option<&Path>, path_var: Option<&OsString>) -> Option<PathBuf> {
    if let Some(p) = preferred.filter(|p| p.is_file()) {
        return Some(p.to_path_buf());
    }
    std::env::split_paths(path_var?)
        .map(|dir| dir.join(HERDR_EXE))
        .find(|p| p.is_file())
}

/// Run `<herdr> --version`; returns the trimmed first stdout line.
pub fn herdr_version(bin: &Path) -> Result<String, String> {
    let out = Command::new(bin)
        .arg("--version")
        .output()
        .map_err(|e| format!("cannot run {}: {e}", bin.display()))?;
    if !out.status.success() {
        return Err(format!("--version exited with {}", out.status));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    match stdout.lines().map(str::trim).find(|l| !l.is_empty()) {
        Some(line) => Ok(line.to_string()),
        None => Err("--version printed nothing".to_string()),
    }
}

/// Create `dir` if missing and prove it is writable with a probe file.
pub fn check_data_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create: {e}"))?;
    let probe = dir.join(".herdmesh-doctor-probe");
    std::fs::write(&probe, b"ok").map_err(|e| format!("not writable: {e}"))?;
    std::fs::remove_file(&probe).map_err(|e| format!("cannot remove probe: {e}"))?;
    Ok(())
}

/// Run every check.
pub fn run(cfg: &DoctorConfig) -> Report {
    let mut checks = Vec::new();

    match find_herdr(cfg.herdr_preferred.as_deref(), cfg.path_var.as_ref()) {
        Some(bin) => {
            checks.push(Check {
                name: "herdr binary",
                ok: true,
                detail: bin.display().to_string(),
            });
            let (ok, detail) = match herdr_version(&bin) {
                Ok(v) => (true, v),
                Err(e) => (false, e),
            };
            checks.push(Check {
                name: "herdr version",
                ok,
                detail,
            });
        }
        None => checks.push(Check {
            name: "herdr binary",
            ok: false,
            detail: "not found (standalone install or PATH)".to_string(),
        }),
    }

    let (ok, detail) = match &cfg.data_dir {
        None => (false, "no platform data dir".to_string()),
        Some(dir) => match check_data_dir(dir) {
            Ok(()) => (true, dir.display().to_string()),
            Err(e) => (false, format!("{}: {e}", dir.display())),
        },
    };
    checks.push(Check {
        name: "data dir",
        ok,
        detail,
    });

    Report { checks }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Write a fake herdr into `dir` that prints `version_line` (or fails).
    fn fake_herdr(dir: &Path, version_line: Option<&str>) -> PathBuf {
        #[cfg(windows)]
        {
            let path = dir.join("herdr.cmd");
            let body = match version_line {
                Some(v) => format!("@echo off\r\necho {v}\r\n"),
                None => "@echo off\r\nexit /b 3\r\n".to_string(),
            };
            std::fs::write(&path, body).unwrap();
            path
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = dir.join("herdr");
            let body = match version_line {
                Some(v) => format!("#!/bin/sh\necho '{v}'\n"),
                None => "#!/bin/sh\nexit 3\n".to_string(),
            };
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
    }

    fn cfg(
        preferred: Option<PathBuf>,
        path_var: Option<OsString>,
        data: Option<PathBuf>,
    ) -> DoctorConfig {
        DoctorConfig {
            herdr_preferred: preferred,
            path_var,
            data_dir: data,
        }
    }

    #[test]
    fn find_herdr_prefers_preferred_path() {
        let tmp = TempDir::new().unwrap();
        let fake = fake_herdr(tmp.path(), Some("herdr 9.9.9"));
        assert_eq!(find_herdr(Some(&fake), None), Some(fake));
    }

    #[test]
    fn find_herdr_falls_back_to_path() {
        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir(&bin_dir).unwrap();
        let on_path = bin_dir.join(HERDR_EXE);
        std::fs::write(&on_path, b"").unwrap();
        let path_var = std::env::join_paths([tmp.path().join("empty"), bin_dir]).unwrap();
        let missing = tmp.path().join("nope").join(HERDR_EXE);
        assert_eq!(find_herdr(Some(&missing), Some(&path_var)), Some(on_path));
    }

    #[test]
    fn find_herdr_none_when_absent() {
        let tmp = TempDir::new().unwrap();
        let path_var = std::env::join_paths([tmp.path()]).unwrap();
        assert_eq!(find_herdr(None, Some(&path_var)), None);
        assert_eq!(find_herdr(None, None), None);
    }

    #[test]
    fn herdr_version_reads_first_line() {
        let tmp = TempDir::new().unwrap();
        let fake = fake_herdr(tmp.path(), Some("herdr 9.9.9"));
        assert_eq!(herdr_version(&fake).unwrap(), "herdr 9.9.9");
    }

    #[test]
    fn herdr_version_fails_on_nonzero_exit() {
        let tmp = TempDir::new().unwrap();
        let fake = fake_herdr(tmp.path(), None);
        assert!(herdr_version(&fake).is_err());
    }

    #[test]
    fn data_dir_is_created_and_probe_removed() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("a").join("herdmesh");
        check_data_dir(&dir).unwrap();
        assert!(dir.is_dir());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn data_dir_fails_when_path_is_a_file() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(check_data_dir(&file).is_err());
    }

    #[test]
    fn run_all_ok_with_fake_herdr() {
        let tmp = TempDir::new().unwrap();
        let fake = fake_herdr(tmp.path(), Some("herdr 9.9.9"));
        let report = run(&cfg(Some(fake), None, Some(tmp.path().join("data"))));
        assert!(report.ok(), "{}", report.render());
        assert_eq!(report.checks.len(), 3);
        assert_eq!(report.checks[1].detail, "herdr 9.9.9");
    }

    #[test]
    fn run_fails_when_herdr_missing() {
        let tmp = TempDir::new().unwrap();
        let report = run(&cfg(None, None, Some(tmp.path().join("data"))));
        assert!(!report.ok());
        assert_eq!(report.checks[0].name, "herdr binary");
        assert!(!report.checks[0].ok);
        assert!(report.checks.iter().any(|c| c.name == "data dir" && c.ok));
    }

    #[test]
    fn run_fails_when_version_fails() {
        let tmp = TempDir::new().unwrap();
        let fake = fake_herdr(tmp.path(), None);
        let report = run(&cfg(Some(fake), None, Some(tmp.path().join("data"))));
        assert!(!report.ok());
        assert!(!report.checks[1].ok);
    }

    #[test]
    fn run_fails_without_data_dir() {
        let tmp = TempDir::new().unwrap();
        let fake = fake_herdr(tmp.path(), Some("herdr 9.9.9"));
        let report = run(&cfg(Some(fake), None, None));
        assert!(!report.ok());
    }

    #[test]
    fn render_marks_failures() {
        let report = Report {
            checks: vec![
                Check {
                    name: "a",
                    ok: true,
                    detail: "fine".into(),
                },
                Check {
                    name: "longer",
                    ok: false,
                    detail: "bad".into(),
                },
            ],
        };
        let table = report.render();
        assert!(table.contains("ok    a       fine"), "{table}");
        assert!(table.contains("FAIL  longer  bad"), "{table}");
    }
}
