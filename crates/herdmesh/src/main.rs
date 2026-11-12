//! herdmesh CLI.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod doctor;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "herdmesh",
    version,
    about = "Shared project rooms for coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Create, join, invite, leave or inspect a room (not yet implemented).
    Room {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run the per-machine daemon (not yet implemented).
    Daemon,
    /// Run the MCP stdio server for one agent (not yet implemented).
    Mcp,
    /// Check the local environment (herdr binary, data dir).
    Doctor,
}

/// Exit code for subcommands that are still stubs.
const EXIT_NOT_IMPLEMENTED: u8 = 2;

fn not_implemented(name: &str) -> ExitCode {
    eprintln!("herdmesh {name}: not yet implemented");
    ExitCode::from(EXIT_NOT_IMPLEMENTED)
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().command {
        Cmd::Room { .. } => not_implemented("room"),
        Cmd::Daemon => not_implemented("daemon"),
        Cmd::Mcp => not_implemented("mcp"),
        Cmd::Doctor => {
            let report = doctor::run(&doctor::DoctorConfig::from_env());
            print!("{}", report.render());
            if report.ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_all_subcommands() {
        for sub in ["room", "daemon", "mcp", "doctor"] {
            assert!(Cli::try_parse_from(["herdmesh", sub]).is_ok(), "{sub}");
        }
    }
}
