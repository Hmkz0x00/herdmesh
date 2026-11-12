//! In-memory fake herdr CLI runner for tests and offline usage.

use std::sync::Mutex;

use crate::cli::HerdrCli;
use crate::error::{HerdrError, Result};

/// Fake herdr CLI runner for testing and offline simulations.
#[derive(Debug, Default)]
pub struct FakeHerdrCli {
    list_json: Mutex<String>,
    prompt_error: Mutex<Option<String>>,
    calls: Mutex<Vec<Vec<String>>>,
}

impl FakeHerdrCli {
    /// Create a fake runner returning `canned_list_json` on `agent list`.
    pub fn new(canned_list_json: impl Into<String>) -> Self {
        Self {
            list_json: Mutex::new(canned_list_json.into()),
            prompt_error: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Update the canned JSON returned by `agent list`.
    pub fn set_list_json(&self, json: impl Into<String>) {
        if let Ok(mut lock) = self.list_json.lock() {
            *lock = json.into();
        }
    }

    /// Set an error for prompt calls.
    pub fn set_prompt_error(&self, stderr: impl Into<String>) {
        if let Ok(mut lock) = self.prompt_error.lock() {
            *lock = Some(stderr.into());
        }
    }

    /// Recorded command arguments passed to `run`.
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Last command arguments passed to `run`.
    pub fn last_call(&self) -> Option<Vec<String>> {
        self.calls.lock().ok().and_then(|c| c.last().cloned())
    }

    /// Clear recorded calls.
    pub fn clear_calls(&self) {
        if let Ok(mut c) = self.calls.lock() {
            c.clear();
        }
    }
}

impl HerdrCli for FakeHerdrCli {
    fn run(&self, args: &[&str]) -> Result<String> {
        let arg_vec: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        if let Ok(mut c) = self.calls.lock() {
            c.push(arg_vec);
        }

        if args.starts_with(&["agent", "list"]) {
            let json = self.list_json.lock().map(|j| j.clone()).unwrap_or_default();
            Ok(json)
        } else if args.starts_with(&["agent", "prompt"]) {
            if let Ok(guard) = self.prompt_error.lock() {
                if let Some(ref err) = *guard {
                    return Err(HerdrError::CommandFailed {
                        status: Some(1),
                        stderr: err.clone(),
                    });
                }
            }
            Ok("{}".to_string())
        } else {
            Ok("{}".to_string())
        }
    }
}
