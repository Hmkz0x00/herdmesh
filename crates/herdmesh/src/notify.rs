//! Presence diffing and the idle-only notify queue. Pure over [`HerdrCli`]
//! so it is tested with `FakeHerdrCli`; the daemon loop only calls it.

use std::collections::{BTreeMap, BTreeSet};

use hm_daemon::Ping;
use hm_herdr::{HerdrCli, HerdrStatus, list_agents, notify};
use hm_proto::{AgentStatus, EventId, RoomId};

/// Remembers the last announced status per (room, agent) and yields only
/// the changes.
#[derive(Debug, Default)]
pub struct PresenceTracker {
    last: BTreeMap<(RoomId, String), AgentStatus>,
}

impl PresenceTracker {
    /// `current`: herdr status per agent name. Agents that vanished are
    /// forgotten (so coming back announces again). Returns
    /// `(agent, status)` pairs to announce in `room`, members only.
    pub fn diff(
        &mut self,
        room: RoomId,
        members: &[String],
        current: &BTreeMap<String, AgentStatus>,
    ) -> Vec<(String, AgentStatus)> {
        self.last
            .retain(|(r, a), _| *r != room || current.contains_key(a));
        let mut out = Vec::new();
        for name in members {
            let Some(&status) = current.get(name) else {
                continue;
            };
            if self.last.get(&(room, name.clone())) != Some(&status) {
                self.last.insert((room, name.clone()), status);
                out.push((name.clone(), status));
            }
        }
        out
    }

    /// Forget a status that could not be announced, so it is retried.
    pub fn forget(&mut self, room: RoomId, agent: &str) {
        self.last.remove(&(room, agent.to_owned()));
    }
}

/// herdr statuses by agent name; unnamed agents and unknown statuses dropped.
pub fn status_by_name(cli: &(impl HerdrCli + ?Sized)) -> BTreeMap<String, AgentStatus> {
    list_agents(cli)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|a| Some((a.name?, AgentStatus::try_from(a.status).ok()?)))
        .collect()
}

/// `[herdmesh] Ask from ab12/bob re task#12 - call herdmesh inbox`
pub fn ping_text(p: &Ping) -> String {
    let re = p.re.as_deref().map_or_else(String::new, |r| {
        format!(" re {}", r.chars().take(24).collect::<String>())
    });
    format!(
        "[herdmesh] {} from {}{re} - call herdmesh inbox",
        p.kind, p.from
    )
}

/// Pings waiting for their agent to be idle/done. Each event is offered
/// once; it leaves the queue when delivered (one prompt covers all queued
/// events of that agent: the inbox returns them together).
#[derive(Debug, Default)]
pub struct NotifyQueue {
    queued: BTreeMap<(RoomId, EventId), Ping>,
    handled: BTreeSet<(RoomId, EventId)>,
}

impl NotifyQueue {
    pub fn offer(&mut self, pings: impl IntoIterator<Item = Ping>) {
        for p in pings {
            let key = (p.room, p.event);
            if !self.handled.contains(&key) {
                self.queued.entry(key).or_insert(p);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.queued.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queued.is_empty()
    }

    /// Try to deliver. Only agents herdr reports idle/done get a prompt;
    /// the rest stay queued for the next tick. Returns prompts sent.
    pub fn flush(&mut self, cli: &(impl HerdrCli + ?Sized)) -> usize {
        if self.queued.is_empty() {
            return 0;
        }
        let Ok(agents) = list_agents(cli) else {
            return 0;
        };
        let ready: BTreeSet<&str> = agents
            .iter()
            .filter(|a| matches!(a.status, HerdrStatus::Idle | HerdrStatus::Done))
            .filter_map(|a| a.name.as_deref())
            .collect();
        let mut sent = 0;
        let mut delivered: BTreeSet<String> = BTreeSet::new();
        for p in self.queued.values() {
            if delivered.contains(&p.agent) || !ready.contains(p.agent.as_str()) {
                continue;
            }
            if notify(cli, &p.agent, &ping_text(p)).is_ok() {
                delivered.insert(p.agent.clone());
                sent += 1;
            }
        }
        let done: Vec<_> = self
            .queued
            .iter()
            .filter(|(_, p)| delivered.contains(&p.agent))
            .map(|(k, _)| *k)
            .collect();
        for k in done {
            self.queued.remove(&k);
            self.handled.insert(k);
        }
        sent
    }
}

#[cfg(test)]
mod tests {
    use hm_herdr::FakeHerdrCli;
    use hm_proto::budget::{PING_MAX, estimate_tokens};

    use super::*;

    fn list(status: &str) -> String {
        format!(
            r#"{{"result":{{"agents":[{{"name":"bob","agent":"claude","agent_status":"{status}","pane_id":"w1:p1","cwd":"/x"}}]}}}}"#
        )
    }

    fn ping(n: u8) -> Ping {
        Ping {
            room: RoomId::from_bytes([1; 32]),
            event: EventId::from_hex(&format!("{n:02x}").repeat(32)).unwrap(),
            agent: "bob".into(),
            kind: "Ask",
            from: "ab12cd/alice".into(),
            re: Some("task#12".into()),
        }
    }

    fn prompts(f: &FakeHerdrCli) -> usize {
        f.calls().iter().filter(|c| c[1] == "prompt").count()
    }

    #[test]
    fn queue_fires_only_when_herdr_reports_idle() {
        let cli = FakeHerdrCli::new(list("working"));
        let mut q = NotifyQueue::default();
        q.offer([ping(1)]);
        assert_eq!(q.flush(&cli), 0);
        assert_eq!(prompts(&cli), 0);
        cli.set_list_json(list("blocked"));
        assert_eq!(q.flush(&cli), 0);
        assert_eq!(q.len(), 1, "still queued");
        cli.set_list_json(list("idle"));
        assert_eq!(q.flush(&cli), 1);
        assert_eq!(prompts(&cli), 1);
        assert!(q.is_empty());
        // Delivered events are never re-queued.
        q.offer([ping(1)]);
        assert!(q.is_empty());
    }

    #[test]
    fn done_counts_and_one_prompt_covers_all_queued_events() {
        let cli = FakeHerdrCli::new(list("done"));
        let mut q = NotifyQueue::default();
        q.offer([ping(1), ping(2)]);
        assert_eq!(q.flush(&cli), 1);
        assert_eq!(prompts(&cli), 1);
        assert!(q.is_empty());
    }

    #[test]
    fn failed_prompt_stays_queued_and_unknown_agent_is_skipped() {
        let cli = FakeHerdrCli::new(list("idle"));
        cli.set_prompt_error("boom");
        let mut q = NotifyQueue::default();
        q.offer([ping(1)]);
        assert_eq!(q.flush(&cli), 0);
        assert_eq!(q.len(), 1);
        let none = FakeHerdrCli::new(r#"{"result":{"agents":[]}}"#);
        assert_eq!(q.flush(&none), 0);
        assert_eq!(prompts(&none), 0);
    }

    #[test]
    fn ping_text_fits_the_ping_budget() {
        let t = ping_text(&ping(1));
        assert!(t.starts_with("[herdmesh] Ask from ab12cd/alice re task#12"));
        assert!(estimate_tokens(&t) <= PING_MAX, "{t}");
        let mut long = ping(1);
        long.from = "a".repeat(24) + "@abcdef";
        long.re = Some("r".repeat(100));
        assert!(estimate_tokens(&ping_text(&long)) <= PING_MAX);
    }

    #[test]
    fn presence_diff_emits_only_on_change() {
        let room = RoomId::from_bytes([1; 32]);
        let members = vec!["bob".to_owned(), "carol".to_owned()];
        let mut t = PresenceTracker::default();
        let mut cur = BTreeMap::from([("bob".to_owned(), AgentStatus::Idle)]);
        assert_eq!(
            t.diff(room, &members, &cur),
            [("bob".to_owned(), AgentStatus::Idle)]
        );
        assert!(t.diff(room, &members, &cur).is_empty(), "unchanged");
        cur.insert("bob".into(), AgentStatus::Working);
        cur.insert("stranger".into(), AgentStatus::Idle);
        assert_eq!(
            t.diff(room, &members, &cur),
            [("bob".to_owned(), AgentStatus::Working)],
            "non-members ignored"
        );
        // Vanishing then returning re-announces.
        cur.remove("bob");
        assert!(t.diff(room, &members, &cur).is_empty());
        cur.insert("bob".into(), AgentStatus::Working);
        assert_eq!(t.diff(room, &members, &cur).len(), 1);
    }

    #[test]
    fn status_by_name_skips_unknown() {
        let cli = FakeHerdrCli::new(list("weird"));
        assert!(status_by_name(&cli).is_empty());
        let cli = FakeHerdrCli::new(list("idle"));
        assert_eq!(status_by_name(&cli)["bob"], AgentStatus::Idle);
    }
}
