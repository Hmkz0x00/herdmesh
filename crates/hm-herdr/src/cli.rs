//! CLI invocation and adapter traits.

use std::path::{Path, PathBuf};
use std::process::Command;

use hm_proto::budget::{PING_MAX, estimate_tokens};

use crate::agent::{HerdrAgent, HerdrStatus, parse_agent_list_json, strip_control_chars};
use crate::error::{HerdrError, Result};

/// Name of the herdr executable on this platform.
pub const HERDR_EXE: &str = if cfg!(windows) { "herdr.exe" } else { "herdr" };

/// Locate the herdr binary: `HERDR_BIN_PATH` if set, else search `PATH`.
pub fn resolve_herdr_bin() -> PathBuf {
    resolve_herdr_bin_with(std::env::var_os("HERDR_BIN_PATH"), std::env::var_os("PATH"))
}

/// Locate the herdr binary given explicit environment variable inputs.
pub fn resolve_herdr_bin_with(
    env_bin: Option<std::ffi::OsString>,
    path_var: Option<std::ffi::OsString>,
) -> PathBuf {
    if let Some(val) = env_bin {
        if !val.is_empty() {
            return PathBuf::from(val);
        }
    }
    if let Some(paths) = path_var {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(HERDR_EXE);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from(HERDR_EXE)
}

/// Abstract interface for executing herdr CLI commands.
pub trait HerdrCli: Send + Sync {
    /// Execute a command with the given argument list.
    fn run(&self, args: &[&str]) -> Result<String>;

    /// List all agents currently tracked by herdr.
    fn list_agents(&self) -> Result<Vec<HerdrAgent>> {
        list_agents(self)
    }

    /// Notify an agent with prompt text without waiting.
    fn notify(&self, target: &str, text: &str) -> Result<()> {
        notify(self, target, text)
    }
}

/// Real herdr CLI runner executing child processes via [`std::process::Command`].
#[derive(Debug, Clone)]
pub struct SystemHerdrCli {
    bin_path: PathBuf,
}

impl SystemHerdrCli {
    /// Create a runner using `HERDR_BIN_PATH` if set, otherwise finding `herdr` on `PATH`.
    pub fn from_env() -> Self {
        Self {
            bin_path: resolve_herdr_bin(),
        }
    }

    /// Create a runner using an explicit executable path.
    pub fn new(bin_path: impl Into<PathBuf>) -> Self {
        Self {
            bin_path: bin_path.into(),
        }
    }

    /// Path to the configured executable.
    pub fn bin_path(&self) -> &Path {
        &self.bin_path
    }
}

impl Default for SystemHerdrCli {
    fn default() -> Self {
        Self::from_env()
    }
}

impl HerdrCli for SystemHerdrCli {
    fn run(&self, args: &[&str]) -> Result<String> {
        tracing::debug!(bin = %self.bin_path.display(), ?args, "executing herdr command");
        let mut cmd = Command::new(&self.bin_path);
        cmd.args(args);
        #[cfg(windows)]
        {
            // Never flash a console window for the periodic herdr polls.
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let output = cmd.output().map_err(HerdrError::Io)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            return Err(HerdrError::CommandFailed {
                status: output.status.code(),
                stderr,
            });
        }

        let stdout = std::str::from_utf8(&output.stdout)?.to_owned();
        Ok(stdout)
    }
}

/// List all agents currently tracked by herdr.
pub fn list_agents(cli: &(impl HerdrCli + ?Sized)) -> Result<Vec<HerdrAgent>> {
    let stdout = cli.run(&["agent", "list"])?;
    parse_agent_list_json(&stdout)
}

/// Send a prompt notification to an agent.
///
/// Refuses if the agent is not in status [`HerdrStatus::Idle`] or [`HerdrStatus::Done`].
/// Refuses if `text` exceeds [`PING_MAX`] tokens.
/// Strips control characters and newlines before submission.
/// Runs without `--wait`.
pub fn notify(cli: &(impl HerdrCli + ?Sized), target: &str, text: &str) -> Result<()> {
    // 1. Check token budget of raw input
    let tokens = estimate_tokens(text);
    if tokens > PING_MAX {
        return Err(HerdrError::OversizedPing {
            tokens,
            max: PING_MAX,
        });
    }

    // 2. Strip control characters and newlines
    let cleaned = strip_control_chars(text);

    // 3. Re-check token budget on cleaned text
    let cleaned_tokens = estimate_tokens(&cleaned);
    if cleaned_tokens > PING_MAX {
        return Err(HerdrError::OversizedPing {
            tokens: cleaned_tokens,
            max: PING_MAX,
        });
    }

    // 4. Re-check agent status right before sending
    let agents = cli.list_agents()?;
    let agent = agents
        .iter()
        .find(|a| a.name.as_deref() == Some(target) || a.pane_id == target)
        .ok_or_else(|| HerdrError::AgentNotFound(target.to_string()))?;

    match agent.status {
        HerdrStatus::Idle | HerdrStatus::Done => {}
        other => {
            return Err(HerdrError::AgentNotReady {
                target: target.to_string(),
                status: other,
            });
        }
    }

    // 5. Submit prompt without --wait
    tracing::debug!(target, cleaned_tokens, "dispatching herdr prompt");
    cli.run(&["agent", "prompt", target, &cleaned])?;
    Ok(())
}

/// High-level adapter for interacting with herdr.
#[derive(Debug, Clone)]
pub struct Herdr<C: HerdrCli = SystemHerdrCli> {
    cli: C,
}

impl Herdr<SystemHerdrCli> {
    /// Create an adapter using the environment / PATH herdr executable.
    pub fn from_env() -> Self {
        Self {
            cli: SystemHerdrCli::from_env(),
        }
    }
}

impl Default for Herdr<SystemHerdrCli> {
    fn default() -> Self {
        Self::from_env()
    }
}

impl<C: HerdrCli> Herdr<C> {
    /// Create an adapter wrapping the given CLI runner.
    pub fn new(cli: C) -> Self {
        Self { cli }
    }

    /// Access the underlying CLI runner.
    pub fn cli(&self) -> &C {
        &self.cli
    }

    /// Mutably access the underlying CLI runner.
    pub fn cli_mut(&mut self) -> &mut C {
        &mut self.cli
    }

    /// List all agents currently tracked by herdr.
    pub fn list_agents(&self) -> Result<Vec<HerdrAgent>> {
        self.cli.list_agents()
    }

    /// Notify an agent with prompt text without waiting.
    pub fn notify(&self, target: &str, text: &str) -> Result<()> {
        self.cli.notify(target, text)
    }
}
