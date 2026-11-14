//! herdmesh CLI.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod doctor;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use herdmesh::daemon;
use herdmesh::data::DataDir;
use herdmesh::naming;
use herdmesh::room::{self, DEFAULT_AGENT};
use herdmesh::{project, setup};

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
    Daemon {
        /// Start a detached daemon only if none is running, then exit.
        #[arg(long)]
        ensure: bool,
    },
    /// Run the MCP stdio server for one agent; starts the daemon if needed.
    Mcp {
        /// Agent CLI this server runs for (claude, codex, agy, ...). The agent
        /// name becomes `<person>-<kind>` (`-2`, `-3` for a second one).
        #[arg(long, conflicts_with = "agent")]
        kind: Option<String>,
        /// Exact agent name (overrides the automatic one).
        #[arg(long)]
        agent: Option<String>,
        /// Room (local name or id); default: this folder's room, else the default room.
        #[arg(long)]
        room: Option<String>,
    },
    /// Set this project folder up: room, agent MCP configs, instructions.
    Setup {
        /// Join with this invite ticket; with no value, read it from the clipboard.
        #[arg(long, num_args = 0..=1, default_missing_value = "", value_name = "TICKET")]
        join: Option<String>,
        /// Your name in rooms (saved; default: your OS user name).
        #[arg(long)]
        name: Option<String>,
        /// Don't ask before writing files.
        #[arg(long)]
        yes: bool,
        /// Show which agent CLIs are detected and which file each would get, then exit.
        #[arg(long)]
        list_clients: bool,
    },
    /// Live board of members, leases, tasks and inbox (herdr plugin pane).
    Board {
        /// Agent name used to read the inbox.
        #[arg(long = "as", default_value = DEFAULT_AGENT)]
        agent: String,
    },
    /// Open a read-only local web view of room activity.
    View {
        #[arg(long)]
        room: Option<String>,
        #[arg(long)]
        no_open: bool,
    },
    /// Check the local environment (herdr binary, data dir).
    Doctor,
}

#[derive(Debug, Subcommand)]
enum RoomCmd {
    /// Create a room and print an invite ticket.
    Create {
        name: String,
        /// Agent name for events you author (default: <person>-cli).
        #[arg(long = "as")]
        agent: Option<String>,
    },
    /// Print an invite ticket (room secret + your address).
    Invite {
        /// Room (default: this folder's room).
        room: Option<String>,
        /// Also copy the ticket to the clipboard.
        #[arg(long)]
        copy: bool,
    },
    /// Join a room from a ticket (local setup; then `room run`).
    Join {
        ticket: String,
        /// Agent name for events you author (default: <person>-cli).
        #[arg(long = "as")]
        agent: Option<String>,
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
    /// Owner only: set a member role (owner|member|observer).
    Grant {
        room: String,
        /// Node id prefix (4+ hex chars, see `room status`).
        node: String,
        role: String,
    },
    /// Show members, leases and open tasks from the local store.
    Status {
        /// Room (default: this folder's room).
        room: Option<String>,
    },
    /// Print full conversation entries; optionally follow new events.
    Log {
        /// Room (default: this folder's room).
        room: Option<String>,
        #[arg(long)]
        follow: bool,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Go online in the foreground until Ctrl+C.
    Run {
        /// Room (default: this folder's room).
        room: Option<String>,
    },
}

fn data_dir(flag: Option<PathBuf>) -> Option<DataDir> {
    flag.or_else(DataDir::default_root).map(DataDir::new)
}

/// Check every name given on the command line. clap would echo a rejected
/// value in its own error, and a rejected value may be a pasted secret, so we
/// validate after parsing with messages that never repeat it.
fn check_names(cmd: &Cmd) -> Result<(), String> {
    let mut checks: Vec<(&'static str, &str)> = Vec::new();
    match cmd {
        Cmd::Mcp { agent, kind, .. } => {
            checks.extend(agent.as_deref().map(|a| ("--agent", a)));
            checks.extend(kind.as_deref().map(|k| ("--kind", k)));
        }
        Cmd::Setup { name, .. } => checks.extend(name.as_deref().map(|n| ("--name", n))),
        Cmd::Board { agent } => checks.push(("--as", agent)),
        Cmd::Room {
            cmd: RoomCmd::Create { agent, .. } | RoomCmd::Join { agent, .. },
        } => checks.extend(agent.as_deref().map(|a| ("--as", a))),
        _ => {}
    }
    for (flag, value) in checks {
        naming::validate(flag, value).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Agent label for events the CLI authors: `--as`, else `<person>-cli`.
fn cli_agent(data: &DataDir, given: Option<String>) -> Result<String, String> {
    if let Some(a) = given {
        return Ok(a);
    }
    let cfg = data.load_config().map_err(|e| e.to_string())?;
    let person = naming::person(&cfg).map_err(|e| e.to_string())?;
    naming::candidate(&person, "cli", 1).map_err(|e| e.to_string())
}

/// Explicit room, else the room `herdmesh setup` made for this folder.
fn project_room(data: &DataDir, given: Option<String>) -> Result<String, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    project::room_arg(data.root(), given.as_deref(), &cwd)
}

async fn room_cmd(data: &DataDir, cmd: RoomCmd) -> Result<String, String> {
    let e = |x: room::RoomError| x.to_string();
    match cmd {
        RoomCmd::Create { name, agent } => {
            let agent = cli_agent(data, agent)?;
            room::create(data, &name, &agent).await.map_err(e)
        }
        RoomCmd::Invite { room, copy } => {
            let room = project_room(data, room)?;
            room::invite(data, &room, copy).await.map_err(e)
        }
        RoomCmd::Join {
            ticket,
            agent,
            name,
        } => {
            let agent = cli_agent(data, agent)?;
            room::join(data, &ticket, &agent, name.as_deref()).map_err(e)
        }
        RoomCmd::Leave { room, yes } => room::leave(data, &room, yes).await.map_err(e),
        RoomCmd::Grant { room, node, role } => {
            room::grant(data, &room, &node, &role).await.map_err(e)
        }
        RoomCmd::Status { room } => room::status(data, &project_room(data, room)?).map_err(e),
        RoomCmd::Log {
            room,
            follow,
            limit,
        } => {
            let room = project_room(data, room)?;
            // Like `view`: bring the daemon up so --follow sees synced
            // events. The log itself reads the local store, so a daemon
            // that cannot start is a warning, not a failure.
            if let Err(err) = daemon::ensure_for_cli(
                data,
                &daemon::default_socket(),
                || daemon::spawn_detached(data),
                |m| eprintln!("herdmesh room log: {m}"),
            )
            .await
            {
                eprintln!("herdmesh room log: daemon not available ({err}); showing local state");
            }
            room::log(
                data,
                &room,
                follow,
                limit,
                async {
                    let _ = tokio::signal::ctrl_c().await;
                },
                |line| println!("{line}"),
            )
            .await
            .map(|()| String::new())
            .map_err(e)
        }
        RoomCmd::Run { room } => {
            let room = project_room(data, room)?;
            let ctrl_c = async {
                // If the handler cannot be installed, run until killed.
                if tokio::signal::ctrl_c().await.is_err() {
                    std::future::pending::<()>().await;
                }
            };
            room::run(data, &room, ctrl_c, |l| println!("{l}"))
                .await
                .map(|()| String::new())
                .map_err(e)
        }
    }
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
    let res = rt.block_on(room_cmd(&data, cmd));
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

fn ensure_main(data: Option<DataDir>) -> ExitCode {
    let Some(data) = data else {
        eprintln!("herdmesh daemon --ensure: no data dir; pass --data-dir");
        return ExitCode::FAILURE;
    };
    block_on("daemon --ensure", async {
        daemon::ensure_running(&daemon::default_socket(), || daemon::spawn_detached(&data))
            .await
            .map_err(|e| e.to_string())
    })
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

fn mcp_main(
    data: Option<DataDir>,
    agent: Option<&str>,
    kind: Option<&str>,
    room: Option<&str>,
) -> ExitCode {
    let Some(data) = data else {
        eprintln!("herdmesh mcp: no data dir; pass --data-dir");
        return ExitCode::FAILURE;
    };
    // The lease keeps an automatic name reserved while this server runs.
    let (name, _lease) = match agent {
        Some(a) => (a.to_owned(), None),
        None => {
            let claimed = data
                .load_config()
                .map_err(|e| e.to_string())
                .and_then(|cfg| naming::person(&cfg).map_err(|e| e.to_string()))
                .and_then(|person| {
                    naming::claim_agent_name(&data.agents_dir(), &person, kind.unwrap_or("agent"))
                        .map_err(|e| e.to_string())
                });
            match claimed {
                Ok(c) => (c.name.clone(), Some(c)),
                Err(e) => {
                    eprintln!("herdmesh mcp: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    };
    // Without --room, prefer this folder's room if this machine knows it.
    let project_room = if room.is_none() {
        std::env::current_dir()
            .ok()
            .and_then(|d| project::find_room(data.root(), &d))
            .filter(|r| data.resolve_room(r).is_ok())
    } else {
        None
    };
    let room = room.or(project_room.as_deref());
    tracing::info!(agent = %name, "herdmesh mcp starting");
    block_on("mcp", async {
        let backend = daemon::mcp_backend(&data, &daemon::default_socket(), &name, room, || {
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

fn setup_main(
    data: Option<DataDir>,
    join: Option<String>,
    name: Option<String>,
    yes: bool,
) -> ExitCode {
    let Some(data) = data else {
        eprintln!("herdmesh setup: no data dir; pass --data-dir");
        return ExitCode::FAILURE;
    };
    let project = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("herdmesh setup: current folder: {e}");
            return ExitCode::FAILURE;
        }
    };
    block_on(
        "setup",
        setup::run(setup::Opts {
            data: &data,
            project: &project,
            join,
            name,
            yes,
        }),
    )
}

fn stderr_supports_ansi() -> bool {
    std::io::stderr().is_terminal()
        && !anstyle_query::no_color()
        && anstyle_query::windows::enable_ansi_colors()
            .unwrap_or_else(anstyle_query::term_supports_ansi_color)
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(stderr_supports_ansi())
        .init();

    let cli = Cli::parse();
    if let Err(e) = check_names(&cli.command) {
        eprintln!("herdmesh: {e}");
        return ExitCode::FAILURE;
    }
    match cli.command {
        Cmd::Room { cmd } => room_main(
            data_dir(cli.data_dir).map(|d| d.allow_insecure_key(cli.allow_insecure_key)),
            cmd,
        ),
        Cmd::Daemon { ensure: true } => ensure_main(
            data_dir(cli.data_dir).map(|d| d.allow_insecure_key(cli.allow_insecure_key)),
        ),
        Cmd::Daemon { ensure: false } => daemon_main(
            data_dir(cli.data_dir).map(|d| d.allow_insecure_key(cli.allow_insecure_key)),
        ),
        Cmd::Mcp { agent, kind, room } => mcp_main(
            data_dir(cli.data_dir).map(|d| d.allow_insecure_key(cli.allow_insecure_key)),
            agent.as_deref(),
            kind.as_deref(),
            room.as_deref(),
        ),
        Cmd::Setup {
            list_clients: true, ..
        } => {
            print!("{}", setup::list_clients());
            ExitCode::SUCCESS
        }
        Cmd::Setup {
            join, name, yes, ..
        } => setup_main(
            data_dir(cli.data_dir).map(|d| d.allow_insecure_key(cli.allow_insecure_key)),
            join,
            name,
            yes,
        ),
        Cmd::View { room, no_open } => {
            let data = data_dir(cli.data_dir);
            let room = room.or_else(|| {
                let data = data.as_ref()?;
                std::env::current_dir()
                    .ok()
                    .and_then(|d| project::find_room(data.root(), &d))
            });
            herdmesh::view::run(data, room.as_deref(), no_open)
        }
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
    fn parses_view_and_room_log() {
        let c = Cli::try_parse_from(["herdmesh", "view", "--room", "alpha", "--no-open"]).unwrap();
        assert!(matches!(c.command, Cmd::View { room: Some(r), no_open: true } if r == "alpha"));
        let c = Cli::try_parse_from([
            "herdmesh", "room", "log", "alpha", "--follow", "--limit", "7",
        ])
        .unwrap();
        assert!(
            matches!(c.command, Cmd::Room { cmd: RoomCmd::Log { room: Some(room), follow: true, limit: 7 } } if room == "alpha")
        );
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_all_subcommands() {
        for sub in ["daemon", "doctor"] {
            assert!(Cli::try_parse_from(["herdmesh", sub]).is_ok(), "{sub}");
        }
        let c =
            Cli::try_parse_from(["herdmesh", "mcp", "--agent", "bob", "--room", "alpha"]).unwrap();
        assert!(matches!(
            c.command,
            Cmd::Mcp { agent: Some(a), kind: None, room: Some(r) } if a == "bob" && r == "alpha"
        ));
        let c = Cli::try_parse_from(["herdmesh", "mcp", "--kind", "claude"]).unwrap();
        assert!(matches!(
            c.command,
            Cmd::Mcp { agent: None, kind: Some(k), room: None } if k == "claude"
        ));
        assert!(
            Cli::try_parse_from(["herdmesh", "mcp"]).is_ok(),
            "kind defaults"
        );
        assert!(
            Cli::try_parse_from(["herdmesh", "mcp", "--kind", "a", "--agent", "b"]).is_err(),
            "--kind and --agent conflict"
        );
    }

    #[test]
    fn secret_looking_or_malformed_names_are_rejected_by_every_flag() {
        let token = "ghp_0123456789abcdef0123456789abcdef0123";
        for args in [
            vec!["mcp", "--agent", token],
            vec!["mcp", "--kind", "Bad Kind"],
            vec!["board", "--as", token],
            vec!["room", "create", "x", "--as", token],
            vec!["room", "join", "t", "--as", "UPPER"],
            vec!["setup", "--name", "has space"],
        ] {
            let mut v = vec!["herdmesh"];
            v.extend(args.iter());
            let c = Cli::try_parse_from(v).unwrap();
            let e = check_names(&c.command).unwrap_err();
            assert!(!e.contains("ghp_"), "error must not echo the value: {e}");
        }
        for args in [
            vec!["mcp", "--agent", "bob-codex"],
            vec!["mcp", "--kind", "claude"],
            vec!["mcp"],
            vec!["board"],
            vec!["room", "create", "x"],
            vec!["setup", "--name", "alice"],
        ] {
            let mut v = vec!["herdmesh"];
            v.extend(args.iter());
            let c = Cli::try_parse_from(v).unwrap();
            assert!(check_names(&c.command).is_ok(), "{args:?}");
        }
    }

    #[test]
    fn parses_setup() {
        let c = Cli::try_parse_from(["herdmesh", "setup"]).unwrap();
        assert!(matches!(
            c.command,
            Cmd::Setup {
                join: None,
                name: None,
                yes: false,
                list_clients: false
            }
        ));
        let c = Cli::try_parse_from(["herdmesh", "setup", "--join", "--yes"]).unwrap();
        assert!(matches!(c.command, Cmd::Setup { join: Some(j), yes: true, .. } if j.is_empty()));
        let c = Cli::try_parse_from(["herdmesh", "setup", "--join", "hmroom1abc", "--name", "bob"])
            .unwrap();
        assert!(
            matches!(c.command, Cmd::Setup { join: Some(j), name: Some(n), .. } if j == "hmroom1abc" && n == "bob")
        );
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
            Cmd::Room {
                cmd: RoomCmd::Create { agent: None, .. }
            }
        ));
        assert!(matches!(
            ok(&["join", "hmroom1abc", "--as", "codex"]).unwrap(),
            Cmd::Room { cmd: RoomCmd::Join { agent: Some(agent), name: None, .. } } if agent == "codex"
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
        // Room commands fall back to the project room when the room is omitted.
        for args in [&["invite"][..], &["status"], &["run"], &["log"]] {
            assert!(ok(args).is_ok(), "{args:?}");
        }
        assert!(matches!(
            ok(&["invite", "--copy"]).unwrap(),
            Cmd::Room {
                cmd: RoomCmd::Invite {
                    room: None,
                    copy: true
                }
            }
        ));
        assert!(ok(&["leave"]).is_err(), "leave always names the room");
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
