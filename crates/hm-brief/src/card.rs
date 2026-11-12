//! L0 room card: the cheapest possible "where are we" (≤ `ROOM_CARD_MAX`).

use std::collections::BTreeMap;

use hm_proto::budget::ROOM_CARD_MAX;
use hm_proto::{AgentStatus, EventId, TaskStatus};
use hm_store::{Lease, Views};

use crate::fit::{Item, fit};
use crate::text::{LEGEND, agent_label, clean, task_status, wrap_max};

const TITLE_CHARS: usize = 60;
const TOP_TASKS: usize = 3;

fn status_word(s: Option<AgentStatus>) -> &'static str {
    match s {
        Some(AgentStatus::Idle) => "idle",
        Some(AgentStatus::Working) => "working",
        Some(AgentStatus::Blocked) => "blocked",
        Some(AgentStatus::Done) => "done",
        None => "?",
    }
}

fn task_rank(s: TaskStatus) -> u8 {
    match s {
        TaskStatus::Blocked => 0,
        TaskStatus::InProgress => 1,
        _ => 2,
    }
}

/// Room card: room, members + status, active leases, top 3 open tasks, last
/// decision title. Whole lines are dropped (least relevant first) to stay
/// within [`ROOM_CARD_MAX`].
pub fn room_card(room: &str, views: &Views, now: u64) -> String {
    let head = vec![format!("room {}", clean(room, 40)), LEGEND.to_owned()];
    let mut items = Vec::new();

    if let Some(d) = views.decisions.last() {
        items.push(Item {
            score: 90,
            recency: 0,
            pos: 40,
            line: format!("decision {}", wrap_max(&d.author, &d.title, TITLE_CHARS)),
        });
    }

    let mut tasks: Vec<_> = views
        .board
        .iter()
        .filter(|(_, t)| {
            matches!(
                t.status,
                TaskStatus::Open | TaskStatus::InProgress | TaskStatus::Blocked
            )
        })
        .collect();
    tasks.sort_by(|a, b| (task_rank(a.1.status), a.0).cmp(&(task_rank(b.1.status), b.0)));
    for (i, (id, t)) in tasks.into_iter().take(TOP_TASKS).enumerate() {
        items.push(Item {
            score: 80 - i as u32,
            recency: 0,
            pos: 30 + i as u64,
            line: format!(
                "task {id} {} {}",
                task_status(t.status),
                wrap_max(&t.created_by, &t.title, TITLE_CHARS)
            ),
        });
    }

    // One line per claim; longest-lived first (they matter most to others).
    let mut by_claim: BTreeMap<EventId, (Lease, Vec<String>)> = BTreeMap::new();
    for (path, lease) in views.active_leases(now) {
        by_claim
            .entry(lease.claim)
            .or_insert_with(|| (lease, Vec::new()))
            .1
            .push(path);
    }
    for (_, (lease, paths)) in by_claim {
        items.push(Item {
            score: 70,
            recency: lease.expires_at_ms,
            pos: 20,
            line: format!(
                "lease {}",
                wrap_max(&lease.holder, &paths.join(" "), TITLE_CHARS)
            ),
        });
    }

    if !views.members.is_empty() {
        let list: Vec<String> = views
            .members
            .iter()
            .map(|(a, m)| format!("{} {}", agent_label(a), status_word(m.status)))
            .collect();
        items.push(Item {
            score: 75,
            recency: 0,
            pos: 10,
            line: format!("members {}", list.join(", ")),
        });
    }

    fit(&head, items, ROOM_CARD_MAX).text
}
