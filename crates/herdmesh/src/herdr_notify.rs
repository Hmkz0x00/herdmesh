//! Human-facing herdr notifications; independent of idle-only agent pings.

use crate::display::sanitize_display;
use hm_daemon::ipc::{Call, Request, Response};
use hm_daemon::{Daemon, Ping};
use hm_herdr::HerdrCli;
use hm_proto::budget::{PING_MAX, estimate_tokens};
use hm_proto::feed::FullEvent;
use hm_proto::redact::redact;
use std::collections::{BTreeMap, VecDeque};

const RATE_MS: u64 = 10_000;
const TRACKED_MAX: usize = 1024;
const PREVIEW_CHARS: usize = 80;

struct Message<'a> {
    from: &'a str,
    to: &'a str,
    kind: &'a str,
    text: &'a str,
}

/// Desktop notices do not depend on agent idle state. Memory and per-agent
/// delivery are bounded; suppressed messages still remain in the room inbox.
#[derive(Default)]
pub struct Notifications {
    last: BTreeMap<String, u64>,
    seen: VecDeque<String>,
}

impl Notifications {
    fn show(
        &mut self,
        cli: &(impl HerdrCli + ?Sized),
        key: &str,
        message: Message<'_>,
        now: u64,
    ) -> bool {
        if !matches!(message.kind, "Ask" | "Handoff") || self.seen.iter().any(|id| id == key) {
            return false;
        }
        if self.seen.len() == TRACKED_MAX {
            self.seen.pop_front();
        }
        self.seen.push_back(key.to_owned());
        self.last
            .retain(|_, then| now.saturating_sub(*then) < RATE_MS);
        if self.last.contains_key(message.to) || self.last.len() == TRACKED_MAX {
            return false;
        }
        let title = format!(
            "{} -> {}",
            sanitize_display(&redact(message.from).text, 48),
            sanitize_display(&redact(message.to).text, 32)
        );
        let preview = sanitize_display(&redact(message.text).text, PREVIEW_CHARS);
        let mut body = format!("{}: {preview}", message.kind);
        while estimate_tokens(&body) > PING_MAX {
            body.pop();
        }
        // SystemHerdrCli applies CREATE_NO_WINDOW on Windows to every invocation.
        let sent = cli
            .run(&[
                "notification",
                "show",
                &title,
                "--body",
                &body,
                "--sound",
                "request",
            ])
            .is_ok();
        // Rate-limit attempts as well as successes when herdr is unavailable/busy.
        self.last.insert(message.to.to_owned(), now);
        sent
    }

    pub fn dispatch(
        &mut self,
        cli: &(impl HerdrCli + ?Sized),
        daemon: &Daemon,
        pings: &[Ping],
        now: u64,
    ) -> usize {
        if pings.is_empty() || cli.list_agents().is_err() {
            return 0;
        }
        let mut sent = 0;
        for ping in pings.iter().take(TRACKED_MAX) {
            let Response::Ok(items) = daemon.handle_request(Request {
                agent: ping.agent.clone(),
                room: Some(ping.room),
                call: Call::FeedEvent {
                    id: ping.event.to_string(),
                },
            }) else {
                continue;
            };
            let [(_, json)] = items.as_slice() else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<FullEvent>(json) else {
                continue;
            };
            let key = format!("{}/{}", ping.room, ping.event);
            sent += usize::from(self.show(
                cli,
                &key,
                Message {
                    from: &event.from,
                    to: &ping.agent,
                    kind: ping.kind,
                    text: &event.text,
                },
                now,
            ));
        }
        sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hm_herdr::FakeHerdrCli;

    #[test]
    fn notification_args_are_separate_sanitized_redacted_and_capped() {
        let cli = FakeHerdrCli::default();
        let mut notifications = Notifications::default();
        let text = format!(
            "hello\n\x1b[2J ghp_a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8 {}",
            "next ".repeat(100)
        );
        assert!(notifications.show(
            &cli,
            "room/event-one",
            Message {
                from: "node/alice",
                to: "bob",
                kind: "Ask",
                text: &text
            },
            0
        ));
        let args = cli.last_call().unwrap();
        assert_eq!(&args[..3], ["notification", "show", "node/alice -> bob"]);
        assert_eq!(args[3], "--body");
        assert!(args[4].starts_with("Ask: hello"));
        assert!(!args[4].contains("ghp_") && !args[4].chars().any(char::is_control));
        assert!(args[4].chars().count() <= 85);
        assert!(estimate_tokens(&args[4]) <= PING_MAX);
        assert_eq!(
            args[4],
            concat!(
                r"Ask: hello\n\x1b[2J [REDACTED:github_token] ",
                "next next next next next next next next \u{2026}"
            )
        );
        assert_eq!(&args[5..], ["--sound", "request"]);
    }

    #[test]
    fn rate_limit_is_per_agent_and_repeated_events_are_not_replayed() {
        let cli = FakeHerdrCli::default();
        let mut notifications = Notifications::default();
        assert!(notifications.show(
            &cli,
            "one",
            Message {
                from: "alice",
                to: "bob",
                kind: "Ask",
                text: "hello"
            },
            0
        ));
        assert!(!notifications.show(
            &cli,
            "two",
            Message {
                from: "alice",
                to: "bob",
                kind: "Ask",
                text: "hello"
            },
            9999
        ));
        assert!(notifications.show(
            &cli,
            "three",
            Message {
                from: "alice",
                to: "carol",
                kind: "Handoff",
                text: "hello"
            },
            9999
        ));
        assert!(notifications.show(
            &cli,
            "four",
            Message {
                from: "alice",
                to: "bob",
                kind: "Ask",
                text: "hello"
            },
            10000
        ));
        assert!(!notifications.show(
            &cli,
            "four",
            Message {
                from: "alice",
                to: "bob",
                kind: "Ask",
                text: "hello"
            },
            20000
        ));
        assert!(!notifications.show(
            &cli,
            "one",
            Message {
                from: "alice",
                to: "bob",
                kind: "Ask",
                text: "hello"
            },
            20000
        ));
        assert!(!notifications.show(
            &cli,
            "two",
            Message {
                from: "alice",
                to: "bob",
                kind: "Ask",
                text: "hello"
            },
            20000
        ));
        assert_eq!(cli.calls().len(), 3);
    }

    #[test]
    fn ticker_shows_ask_and_handoff_while_agents_are_working() {
        use hm_proto::RoomId;
        use std::sync::Arc;
        let daemon = Arc::new(Daemon::new(
            ed25519_dalek::SigningKey::from_bytes(&[5; 32]),
            Arc::new(hm_daemon::NullNet),
        ));
        let room = daemon
            .create_room(
                "test",
                hm_store::Store::open_in_memory(RoomId::from_bytes([1; 32])).unwrap(),
            )
            .unwrap();
        let call = |agent: &str, call| {
            assert!(matches!(
                daemon.handle_request(Request {
                    agent: agent.into(),
                    room: Some(room),
                    call
                }),
                Response::Ok(_)
            ));
        };
        for agent in ["alice", "bob", "carol"] {
            call(agent, Call::RoomCard);
        }
        call(
            "alice",
            Call::Ask {
                to: "bob".into(),
                question: "ready?".into(),
            },
        );
        call(
            "alice",
            Call::Handoff {
                to: Some("carol".into()),
                summary: "take over".into(),
                refs: vec![],
                next_steps: vec![],
            },
        );
        let cli = std::sync::Arc::new(FakeHerdrCli::new(
            r#"[{"pane_id":"w:p1","name":"bob","status":"working"},{"pane_id":"w:p2","name":"carol","status":"working"}]"#,
        ));
        let mut ticker = crate::daemon::Ticker::new(daemon, cli.clone());
        ticker.tick();
        ticker.tick();
        let calls = cli.calls();
        let notices: Vec<_> = calls.iter().filter(|c| c[0] == "notification").collect();
        assert_eq!(notices.len(), 2, "{calls:?}");
        assert!(notices.iter().any(|c| c[4] == "Ask: ready?"));
        assert!(notices.iter().any(|c| c[4] == "Handoff: take over"));
        assert!(
            !calls
                .iter()
                .any(|c| c.starts_with(&["agent".into(), "prompt".into()]))
        );
    }

    #[test]
    fn missing_herdr_never_attempts_a_notification() {
        struct Missing(std::sync::Mutex<Vec<Vec<String>>>);
        impl HerdrCli for Missing {
            fn run(&self, args: &[&str]) -> hm_herdr::Result<String> {
                self.0
                    .lock()
                    .unwrap()
                    .push(args.iter().map(|a| (*a).to_owned()).collect());
                Err(hm_herdr::HerdrError::NotFound)
            }
        }
        let cli = Missing(std::sync::Mutex::new(vec![]));
        let daemon = Daemon::new(
            ed25519_dalek::SigningKey::from_bytes(&[5; 32]),
            std::sync::Arc::new(hm_daemon::NullNet),
        );
        let ping = Ping {
            room: hm_proto::RoomId::from_bytes([1; 32]),
            event: hm_proto::EventId::from_bytes([2; 32]),
            agent: "bob".into(),
            kind: "Ask",
            from: "alice".into(),
            re: None,
        };
        assert_eq!(
            Notifications::default().dispatch(&cli, &daemon, &[ping], 0),
            0
        );
        assert!(cli.0.lock().unwrap().iter().all(|c| c[0] != "notification"));
    }
}
