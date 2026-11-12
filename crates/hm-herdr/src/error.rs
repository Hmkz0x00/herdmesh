//! Errors returned by the herdr adapter.

use std::str::Utf8Error;

use crate::agent::HerdrStatus;

/// Result alias for herdr operations.
pub type Result<T, E = HerdrError> = std::result::Result<T, E>;

/// Errors returned by the herdr adapter.
#[derive(Debug, thiserror::Error)]
pub enum HerdrError {
    /// Failed to execute the herdr binary or process I/O.
    #[error("herdr I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// herdr process exited with a non-zero exit status.
    #[error("herdr command failed (exit code: {status:?}): {stderr}")]
    CommandFailed {
        /// Process exit code, if available.
        status: Option<i32>,
        /// Stderr output captured from the process.
        stderr: String,
    },

    /// herdr command stdout was not valid UTF-8.
    #[error("herdr output is not valid UTF-8: {0}")]
    Utf8(#[from] Utf8Error),

    /// Failed to parse JSON from herdr command output.
    #[error("failed to parse herdr JSON: {0}")]
    Json(#[from] serde_json::Error),

    /// The target agent name or pane ID was not found in the agent list.
    #[error("agent '{0}' not found in herdr agent list")]
    AgentNotFound(String),

    /// The target agent is not ready to receive a prompt (not Idle or Done).
    #[error("agent '{target}' is not ready to receive prompt (status: {status})")]
    AgentNotReady {
        /// Target name or pane ID.
        target: String,
        /// Current agent status.
        status: HerdrStatus,
    },

    /// The notification message exceeds the token budget cap (`PING_MAX`).
    #[error("notification message exceeds token budget ({tokens} tokens > max {max})")]
    OversizedPing {
        /// Estimated token count.
        tokens: usize,
        /// Budget cap.
        max: usize,
    },

    /// An agent status string from herdr could not be converted to a known status.
    #[error("unknown agent status")]
    UnknownStatus,

    /// No herdr binary could be located via `HERDR_BIN_PATH` or `PATH`.
    #[error("herdr binary not found on HERDR_BIN_PATH or PATH")]
    NotFound,
}
