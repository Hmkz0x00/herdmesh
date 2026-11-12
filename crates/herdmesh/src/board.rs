//! `herdmesh board`: a small live TUI (members, leases, open tasks, inbox
//! count) for a herdr plugin pane. Data comes from the local daemon over its
//! IPC socket; every peer-authored string goes through [`sanitize_display`]
//! in [`render`], which is a pure function so it is testable without a
//! terminal.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use hm_daemon::ipc::{Call, Client, Request, Response, Token, current_user, socket_name};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::data::DataDir;
use crate::display::{NAME_MAX, TEXT_MAX, sanitize_display};

/// How often the board asks the daemon for fresh data.
pub const REFRESH: Duration = Duration::from_secs(2);
/// Input poll slice; keeps `q` responsive between refreshes.
const POLL: Duration = Duration::from_millis(200);
/// Rows shown per section before "+n more".
const SECTION_MAX: usize = 8;

/// File name of the IPC secret inside the data dir. The daemon must write
/// its token here (see `hm_daemon::ipc::load_or_create_token`).
pub const TOKEN_FILE: &str = "ipc.token";

/// Everything the board shows. Strings are raw peer text, sanitized only
/// when rendered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoardState {
    pub room: String,
    /// `(agent label, status)`
    pub members: Vec<(String, String)>,
    /// `(holder label, paths)`
    pub leases: Vec<(String, String)>,
    /// `(task id, status, title)`; only open work.
    pub tasks: Vec<(String, String, String)>,
    pub inbox: usize,
    /// Set when the daemon could not be reached.
    pub error: Option<String>,
}

fn section(out: &mut Vec<String>, title: &str, rows: Vec<String>) {
    out.push(format!("{title} ({})", rows.len()));
    if rows.is_empty() {
        out.push("  -".to_owned());
    }
    let hidden = rows.len().saturating_sub(SECTION_MAX);
    out.extend(rows.into_iter().take(SECTION_MAX).map(|r| format!("  {r}")));
    if hidden > 0 {
        out.push(format!("  +{hidden} more"));
    }
}

/// Plain-text lines for `state`. No terminal escapes can appear: all peer
/// strings are sanitized and the rest is static.
pub fn render(state: &BoardState) -> Vec<String> {
    let mut out = vec![format!(
        "herdmesh board - room {}",
        sanitize_display(&state.room, NAME_MAX)
    )];
    if let Some(e) = &state.error {
        out.push(format!("! {}", sanitize_display(e, TEXT_MAX)));
    }
    out.push(String::new());
    section(
        &mut out,
        "members",
        state
            .members
            .iter()
            .map(|(who, st)| {
                format!(
                    "{} {}",
                    sanitize_display(who, NAME_MAX),
                    sanitize_display(st, NAME_MAX)
                )
            })
            .collect(),
    );
    section(
        &mut out,
        "leases",
        state
            .leases
            .iter()
            .map(|(who, paths)| {
                format!(
                    "{} {}",
                    sanitize_display(who, NAME_MAX),
                    sanitize_display(paths, TEXT_MAX)
                )
            })
            .collect(),
    );
    section(
        &mut out,
        "open tasks",
        state
            .tasks
            .iter()
            .map(|(id, st, title)| {
                format!(
                    "{} [{}] {}",
                    sanitize_display(id, NAME_MAX),
                    sanitize_display(st, NAME_MAX),
                    sanitize_display(title, TEXT_MAX)
                )
            })
            .collect(),
    );
    out.push(format!("inbox: {}", state.inbox));
    out.push(String::new());
    out.push("q quit".to_owned());
    out
}

/// Fill `state` from the daemon's room-card text (`room`, `members`,
/// `lease` lines). Lines the card dropped for budget are simply absent.
pub fn apply_card(state: &mut BoardState, card: &str) {
    for line in card.lines() {
        if let Some(name) = line.strip_prefix("room ") {
            state.room = name.to_owned();
        } else if let Some(list) = line.strip_prefix("members ") {
            state.members = list
                .split(", ")
                .filter_map(|m| m.rsplit_once(' '))
                .map(|(who, st)| (who.to_owned(), st.to_owned()))
                .collect();
        } else if let Some(rest) = line.strip_prefix("lease ")
            && let Some((who, paths)) = rest.split_once('«')
        {
            state
                .leases
                .push((who.to_owned(), paths.trim_end_matches('»').to_owned()));
        }
    }
}

/// Parse `task_list` items (`"<id> <status>: <title>"`), keeping open work.
pub fn apply_tasks(state: &mut BoardState, items: &[(String, String)]) {
    state.tasks = items
        .iter()
        .filter_map(|(_, text)| {
            let (id, rest) = text.split_once(' ')?;
            let (status, title) = rest.split_once(": ")?;
            matches!(status, "open" | "in_progress" | "blocked")
                .then(|| (id.to_owned(), status.to_owned(), title.to_owned()))
        })
        .collect();
}

async fn ask(client: &Client, agent: &str, call: Call) -> Result<Vec<(String, String)>, String> {
    let req = Request {
        agent: agent.to_owned(),
        room: None,
        call,
    };
    match client.request(&req).await {
        Ok(Response::Ok(items)) => Ok(items),
        Ok(Response::Err(code)) => Err(format!("daemon: {code:?}")),
        Err(e) => Err(format!("daemon unreachable: {e}")),
    }
}

/// One refresh. Never panics; failures land in `state.error`.
pub async fn fetch(client: &Client, agent: &str) -> BoardState {
    let mut state = BoardState::default();
    let result: Result<(), String> = async {
        let card = ask(client, agent, Call::RoomCard).await?;
        for (_, text) in &card {
            apply_card(&mut state, text);
        }
        let tasks = ask(client, agent, Call::TaskList { filter: None }).await?;
        apply_tasks(&mut state, &tasks);
        state.inbox = ask(client, agent, Call::Inbox { ack: false }).await?.len();
        Ok(())
    }
    .await;
    state.error = result.err();
    state
}

fn read_token(path: &PathBuf) -> Result<Token, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("no daemon token ({e})"))?;
    let arr: [u8; hm_daemon::ipc::TOKEN_LEN] = bytes
        .try_into()
        .map_err(|_| "daemon token has the wrong size".to_owned())?;
    Ok(Token::from_bytes(arr))
}

fn run_tui(client: &Client, agent: &str) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut terminal = ratatui::init();
    let result = (|| -> anyhow::Result<()> {
        let mut state = rt.block_on(fetch(client, agent));
        let mut last = Instant::now();
        loop {
            terminal.draw(|f| {
                let lines: Vec<Line> = render(&state).into_iter().map(Line::from).collect();
                f.render_widget(Paragraph::new(lines), f.area());
            })?;
            if event::poll(POLL)?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                let quit = matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
                    || (k.code == KeyCode::Char('c')
                        && k.modifiers.contains(KeyModifiers::CONTROL));
                if quit {
                    return Ok(());
                }
            }
            if last.elapsed() >= REFRESH {
                state = rt.block_on(fetch(client, agent));
                last = Instant::now();
            }
        }
    })();
    ratatui::restore();
    result
}

/// Entry point for `herdmesh board`.
pub fn run(data: Option<DataDir>, agent: &str) -> ExitCode {
    let Some(data) = data else {
        eprintln!("herdmesh board: no data dir; pass --data-dir");
        return ExitCode::FAILURE;
    };
    let token = match read_token(&data.root().join(TOKEN_FILE)) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("herdmesh board: {e}; is the daemon running?");
            return ExitCode::FAILURE;
        }
    };
    let client = Client::new(socket_name(&current_user()), token);
    match run_tui(&client, agent) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("herdmesh board: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BoardState {
        BoardState {
            room: "alpha".into(),
            members: vec![
                ("26b934/claude".into(), "working".into()),
                ("c0dce1/codex".into(), "idle".into()),
            ],
            leases: vec![("26b934/claude".into(), "crates/hm-brief/**".into())],
            tasks: vec![("t-brief".into(), "blocked".into(), "build L1 brief".into())],
            inbox: 2,
            error: None,
        }
    }

    #[test]
    fn render_snapshot() {
        let expected = [
            "herdmesh board - room alpha",
            "",
            "members (2)",
            "  26b934/claude working",
            "  c0dce1/codex idle",
            "leases (1)",
            "  26b934/claude crates/hm-brief/**",
            "open tasks (1)",
            "  t-brief [blocked] build L1 brief",
            "inbox: 2",
            "",
            "q quit",
        ];
        assert_eq!(render(&sample()), expected);
    }

    #[test]
    fn render_empty_and_error() {
        let s = BoardState {
            room: "r".into(),
            error: Some("daemon unreachable: refused".into()),
            ..BoardState::default()
        };
        let lines = render(&s);
        assert_eq!(lines[1], "! daemon unreachable: refused");
        assert!(lines.contains(&"members (0)".to_owned()));
        assert!(lines.contains(&"inbox: 0".to_owned()));
    }

    #[test]
    fn render_sanitizes_peer_text_and_caps_sections() {
        let mut s = sample();
        s.room = "al\x1b[2Jpha".into();
        s.tasks = vec![(
            "t-1".into(),
            "open".into(),
            "evil\r\n\x1b]0;x\x07title".into(),
        )];
        s.members = (0..12)
            .map(|i| (format!("n{i}/a"), "idle".into()))
            .collect();
        let lines = render(&s);
        for l in &lines {
            assert!(!l.chars().any(char::is_control), "{l:?}");
        }
        assert!(lines.iter().any(|l| l == "  +4 more"));
        assert_eq!(
            lines.iter().filter(|l| l.starts_with("  n")).count(),
            SECTION_MAX
        );
    }

    #[test]
    fn parses_card_and_tasks() {
        let card = "room alpha\nmembers 26b934/claude working, c0dce1/codex ?\n\
                    lease 26b934/claude«crates/hm-brief/** docs/**»\ntask t-1 blocked x\n(+2 less relevant omitted)";
        let mut s = BoardState::default();
        apply_card(&mut s, card);
        assert_eq!(s.room, "alpha");
        assert_eq!(
            s.members,
            vec![
                ("26b934/claude".into(), "working".into()),
                ("c0dce1/codex".into(), "?".into())
            ]
        );
        assert_eq!(
            s.leases,
            vec![("26b934/claude".into(), "crates/hm-brief/** docs/**".into())]
        );
        let items = vec![
            ("a/b".to_owned(), "t-1 blocked: fix it: now".to_owned()),
            ("a/b".to_owned(), "t-2 done: shipped".to_owned()),
            ("a/b".to_owned(), "garbage".to_owned()),
        ];
        apply_tasks(&mut s, &items);
        assert_eq!(
            s.tasks,
            vec![("t-1".into(), "blocked".into(), "fix it: now".into())]
        );
    }

    #[test]
    fn plugin_manifest_parses_with_required_keys() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugin/herdr-plugin.toml");
        let text = std::fs::read_to_string(path).unwrap();
        let m: toml::Table = text.parse().unwrap();
        for k in ["id", "name", "version", "min_herdr_version"] {
            assert!(m.get(k).and_then(|v| v.as_str()).is_some(), "{k}");
        }
        assert_eq!(m["id"].as_str(), Some("herdmesh"));
        let ids = |tbl: &str| -> Vec<String> {
            m.get(tbl)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.get("id")?.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        let actions = ids("actions");
        for want in [
            "room.create",
            "room.join",
            "room.invite",
            "room.leave",
            "board.open",
        ] {
            assert!(actions.iter().any(|a| a == want), "action {want}");
        }
        // Every action/pane/startup/build entry has a non-empty argv.
        for tbl in ["actions", "panes", "startup", "build"] {
            let arr = m[tbl].as_array().unwrap();
            assert!(!arr.is_empty(), "{tbl}");
            for t in arr {
                let argv = t["command"].as_array().unwrap();
                assert!(!argv.is_empty() && argv.iter().all(|a| a.is_str()), "{tbl}");
            }
        }
        let pane = &m["panes"].as_array().unwrap()[0];
        assert_eq!(pane["id"].as_str(), Some("board"));
        assert_eq!(pane["command"][1].as_str(), Some("board"));
        let startup = m["startup"][0]["command"].as_array().unwrap();
        assert!(startup.iter().any(|a| a.as_str() == Some("--ensure")));
        let build = m["build"][0]["command"].as_array().unwrap();
        assert_eq!(
            build.iter().filter_map(|a| a.as_str()).collect::<Vec<_>>(),
            ["cargo", "build", "--release", "-p", "herdmesh"]
        );
    }
}
