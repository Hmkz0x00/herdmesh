//! herdmesh CLI.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod doctor;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use herdmesh::daemon;
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
    /// Run the per-machine daemon (one per user) until Ctrl+C.
    Daemon,
    /// Run the MCP stdio server for one agent; starts the daemon if needed.
    Mcp {
        /// Agent name this server acts as.
        #[arg(long)]
        agent: String,
        /// Room (local name or id) to use instead of the default room.
        #[arg(long)]
        room: Option<String>,
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

/// Run `fut` on a fresh runtime; print errors as `herdmesh <name>: ...`.
fn block_on<E: std::fmt::Display>(
    name: &str,
    fut: impl std::future::Future<Output = Result<(), E>>,
) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("herdmesh {name}: tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(fut) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("herdmesh {name}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn daemon_main(data: Option<DataDir>) -> ExitCode {
    let Some(data) = data else {
        eprintln!("herdmesh daemon: no data dir; pass --data-dir");
        return ExitCode::FAILURE;
    };
    block_on("daemon", async {
        let opts = daemon::DaemonOptions::from_data(&data)?;
        let ctrl_c = async {
            if tokio::signal::ctrl_c().await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        daemon::run(&data, opts, ctrl_c, |l| eprintln!("{l}")).await
    })
}

fn mcp_main(data: Option<DataDir>, agent: &str, room: Option<&str>) -> ExitCode {
    let Some(data) = data else {
        eprintln!("herdmesh mcp: no data dir; pass --data-dir");
        return ExitCode::FAILURE;
    };
    block_on("mcp", async {
        let backend = daemon::mcp_backend(&data, &daemon::default_socket(), agent, room, || {
            daemon::spawn_detached(&data)
        })
        .await
        .map_err(|e| e.to_string())?;
        // stdout belongs to the MCP transport; logs stay on stderr.
        hm_mcp::serve_stdio(std::sync::Arc::new(backend))
            .await
            .map_err(|e| e.to_string())
    })
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
        Cmd::Daemon => daemon_main(
            data_dir(cli.data_dir).map(|d| d.allow_insecure_key(cli.allow_insecure_key)),
        ),
        Cmd::Mcp { agent, room } => mcp_main(
            data_dir(cli.data_dir).map(|d| d.allow_insecure_key(cli.allow_insecure_key)),
            &agent,
            room.as_deref(),
        ),
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
        for sub in ["daemon", "doctor"] {
            assert!(Cli::try_parse_from(["herdmesh", sub]).is_ok(), "{sub}");
        }
        assert!(
            Cli::try_parse_from(["herdmesh", "mcp"]).is_err(),
            "needs --agent"
        );
        let c =
            Cli::try_parse_from(["herdmesh", "mcp", "--agent", "bob", "--room", "alpha"]).unwrap();
        assert!(matches!(
            c.command,
            Cmd::Mcp { agent, room: Some(r) } if agent == "bob" && r == "alpha"
        ));
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
