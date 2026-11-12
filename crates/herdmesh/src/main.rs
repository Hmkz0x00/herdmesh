//! herdmesh CLI.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod doctor;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use herdmesh::data::DataDir;
use herdmesh::room::{self, DEFAULT_AGENT};

#[derive(Debug, Parser)]
#[command(
    name = "herdmesh",
    version,
    about = "Shared project rooms for coding agents"
)]
struct Cli {
    /// Data dir (default: the platform data dir + /herdmesh, as in `doctor`).
    #[arg(long, global = true, value_name = "DIR")]
    data_dir: Option<PathBuf>,
    /// Use node/room secret files that others can read (warn, don't refuse).
    #[arg(long, global = true)]
    allow_insecure_key: bool,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Create, join, invite, leave, inspect or run a room.
    Room {
        #[command(subcommand)]
        cmd: RoomCmd,
    },
    /// Run the per-machine daemon (not yet implemented).
    Daemon,
    /// Run the MCP stdio server for one agent (not yet implemented).
    Mcp,
    /// Live board of members, leases, tasks and inbox (herdr plugin pane).
    Board {
        /// Agent name used to read the inbox.
        #[arg(long = "as", default_value = DEFAULT_AGENT)]
        agent: String,
    },
    /// Check the local environment (herdr binary, data dir).
    Doctor,
}

#[derive(Debug, Subcommand)]
enum RoomCmd {
    /// Create a room and print an invite ticket.
    Create {
        name: String,
        /// Agent name for events you author.
        #[arg(long = "as", default_value = DEFAULT_AGENT)]
        agent: String,
    },
    /// Print an invite ticket (room secret + your address).
    Invite { room: String },
    /// Join a room from a ticket (local setup; then `room run`).
    Join {
        ticket: String,
        #[arg(long = "as", default_value = DEFAULT_AGENT)]
        agent: String,
        /// Local name for the room (default: room-<id prefix>).
        #[arg(long)]
        name: Option<String>,
    },
    /// Announce Leave; with --yes also delete local room state.
    Leave {
        room: String,
        #[arg(long)]
        yes: bool,
    },
    /// Show members, leases and open tasks from the local store.
    Status { room: String },
    /// Go online in the foreground until Ctrl+C.
    Run { room: String },
}

fn data_dir(flag: Option<PathBuf>) -> Option<DataDir> {
    flag.or_else(DataDir::default_root).map(DataDir::new)
}

fn room_main(data: Option<DataDir>, cmd: RoomCmd) -> ExitCode {
    let Some(data) = data else {
        eprintln!("herdmesh room: no data dir; pass --data-dir");
        return ExitCode::FAILURE;
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("herdmesh room: tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let res = rt.block_on(async {
        match cmd {
            RoomCmd::Create { name, agent } => room::create(&data, &name, &agent).await,
            RoomCmd::Invite { room } => room::invite(&data, &room).await,
            RoomCmd::Join {
                ticket,
                agent,
                name,
            } => room::join(&data, &ticket, &agent, name.as_deref()),
            RoomCmd::Leave { room, yes } => room::leave(&data, &room, yes).await,
            RoomCmd::Status { room } => room::status(&data, &room),
            RoomCmd::Run { room } => {
                let ctrl_c = async {
                    // If the handler cannot be installed, run until killed.
                    if tokio::signal::ctrl_c().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                };
                room::run(&data, &room, ctrl_c, |l| println!("{l}"))
                    .await
                    .map(|()| String::new())
            }
        }
    });
    match res {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("herdmesh room: {e}");
            ExitCode::FAILURE
        }
    }
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

    let cli = Cli::parse();
    match cli.command {
        Cmd::Room { cmd } => room_main(
            data_dir(cli.data_dir).map(|d| d.allow_insecure_key(cli.allow_insecure_key)),
            cmd,
        ),
        Cmd::Daemon => not_implemented("daemon"),
        Cmd::Mcp => not_implemented("mcp"),
        Cmd::Board { agent } => herdmesh::board::run(data_dir(cli.data_dir), &agent),
        Cmd::Doctor => {
            let mut cfg = doctor::DoctorConfig::from_env();
            if cli.data_dir.is_some() {
                cfg.data_dir = cli.data_dir;
            }
            let report = doctor::run(&cfg);
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
        for sub in ["daemon", "mcp", "doctor"] {
            assert!(Cli::try_parse_from(["herdmesh", sub]).is_ok(), "{sub}");
        }
    }

    #[test]
    fn parses_room_subcommands() {
        let ok = |args: &[&str]| {
            let mut v = vec!["herdmesh", "room"];
            v.extend_from_slice(args);
            Cli::try_parse_from(v).map(|c| c.command)
        };
        assert!(matches!(
            ok(&["create", "alpha"]).unwrap(),
            Cmd::Room { cmd: RoomCmd::Create { agent, .. } } if agent == DEFAULT_AGENT
        ));
        assert!(matches!(
            ok(&["join", "hmroom1abc", "--as", "codex"]).unwrap(),
            Cmd::Room { cmd: RoomCmd::Join { agent, name: None, .. } } if agent == "codex"
        ));
        assert!(matches!(
            ok(&["leave", "alpha"]).unwrap(),
            Cmd::Room {
                cmd: RoomCmd::Leave { yes: false, .. }
            }
        ));
        assert!(matches!(
            ok(&["leave", "alpha", "--yes"]).unwrap(),
            Cmd::Room {
                cmd: RoomCmd::Leave { yes: true, .. }
            }
        ));
        for args in [&["invite", "a"][..], &["status", "a"], &["run", "a"]] {
            assert!(ok(args).is_ok(), "{args:?}");
        }
        assert!(ok(&[]).is_err(), "room needs a subcommand");
        assert!(ok(&["create"]).is_err(), "create needs a name");
    }

    #[test]
    fn data_dir_flag_is_global() {
        let c = Cli::try_parse_from(["herdmesh", "room", "status", "a", "--data-dir", "/tmp/x"])
            .unwrap();
        assert_eq!(c.data_dir, Some(PathBuf::from("/tmp/x")));
    }
}
