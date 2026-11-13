//! Read-only room snapshot for `herdmesh view` (ADR-003).
//!
//! [`Feed`] serializes to exactly the JSON of `GET /<token>/api/state`.
//! Free-text fields are redacted, escaped with `sanitize_display`, and
//! capped here before being served over authenticated local IPC.
//!
//! Cost: one pass over the room log per call (rooms are small in v1; the
//! page polls every few seconds). Nothing is written, nobody is made a
//! member.

use std::collections::BTreeMap;

use crate::display::sanitize_display;
use hm_brief::agent_label;
pub use hm_proto::feed::{Feed, FeedEvent, FeedLease, FeedMember, FeedTask, FullEvent};
use hm_proto::redact::redact;
use hm_proto::{AgentRef, AgentStatus, Event, EventBody, EventId, Role, TaskStatus};
use hm_store::{ClaimState, Store};

/// Most events per response.
pub const EVENTS_MAX: usize = 200;
/// Longest free-text field, in chars (ellipsis included).
pub const TEXT_MAX: usize = 200;
/// Most leases / open tasks listed.
const LIST_MAX: usize = 100;
/// Hex chars of an event id shown to the page.
const SHORT_ID: usize = 6;
/// Longest part of an activity line, in chars.
const PART_MAX: usize = 60;

/// Redacted, one-line, `TEXT_MAX`-capped copy of peer text.
fn text(s: &str) -> String {
    sanitize_display(&redact(s).text, TEXT_MAX)
}

/// Like [`text`], also saying whether nothing was cut.
fn text_full(s: &str) -> (String, bool) {
    let r = redact(s).text;
    let cut = sanitize_display(&r, TEXT_MAX);
    let whole = sanitize_display(&r, usize::MAX);
    let full = cut == whole;
    (cut, full)
}

fn short(id: &EventId) -> String {
    id.to_string().chars().take(SHORT_ID).collect()
}

fn who(a: &AgentRef) -> String {
    agent_label(&AgentRef::new(a.node, redact(&a.agent_name).text))
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::Owner => "owner",
        Role::Member => "member",
        Role::Observer => "observer",
    }
}

fn status_str(s: Option<AgentStatus>) -> &'static str {
    match s {
        None => "unknown",
        Some(AgentStatus::Idle) => "idle",
        Some(AgentStatus::Working) => "working",
        Some(AgentStatus::Blocked) => "blocked",
        Some(AgentStatus::Done) => "done",
    }
}

fn age(now: u64, then: u64) -> String {
    let s = now.saturating_sub(then) / 1000;
    match s {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

/// Everything the page and the CLI log need to know about one message.
struct Parts {
    kind: &'static str,
    /// Complete peer text, not yet redacted or cut.
    raw: String,
    /// Feed `ref` field (task id, short claim id, or full event id).
    reference: Option<String>,
    to: Option<String>,
    /// Full ids / refs this message points at.
    refs: Vec<String>,
}

fn parts(e: &Event) -> Option<Parts> {
    let p = |kind, raw: String, reference: Option<String>, to, refs| Parts {
        kind,
        raw,
        reference,
        to,
        refs,
    };
    Some(match &e.body {
        EventBody::Join { .. } => p("join", String::new(), None, None, vec![]),
        EventBody::Leave => p("leave", String::new(), None, None, vec![]),
        EventBody::Presence { status } => p(
            "presence",
            status_str(Some(*status)).to_owned(),
            None,
            None,
            vec![],
        ),
        EventBody::Ask { to, q } => p("ask", q.clone(), None, to.as_ref().map(who), vec![]),
        EventBody::Answer { ask_id, a } => p(
            "answer",
            a.clone(),
            Some(ask_id.to_string()),
            None,
            vec![ask_id.to_string()],
        ),
        EventBody::Handoff {
            to,
            summary,
            refs,
            next_steps,
        } => {
            let mut t = summary.clone();
            for s in next_steps {
                t.push_str("\nnext: ");
                t.push_str(s);
            }
            p(
                "handoff",
                t,
                None,
                to.as_ref().map(who),
                refs.iter().map(ToString::to_string).collect(),
            )
        }
        EventBody::LeaseClaim { paths, .. } => {
            p("claim", paths.join(" "), Some(short(&e.id)), None, vec![])
        }
        EventBody::LeaseRelease { claim } => p(
            "release",
            String::new(),
            Some(short(claim)),
            None,
            vec![claim.to_string()],
        ),
        EventBody::TaskAdd { task, title, .. } => p(
            "task_add",
            title.clone(),
            Some(task.to_string()),
            None,
            vec![task.to_string()],
        ),
        EventBody::TaskUpdate { task, status, note } => p(
            "task_update",
            note.clone()
                .unwrap_or_else(|| task_status(*status).to_owned()),
            Some(task.to_string()),
            None,
            vec![task.to_string()],
        ),
        EventBody::Decision {
            title,
            rationale,
            refs,
        } => p(
            "decide",
            format!("{title}\n{rationale}"),
            None,
            None,
            refs.iter().map(ToString::to_string).collect(),
        ),
        EventBody::Note { text: t, .. } => p("note", t.clone(), None, None, vec![]),
        EventBody::Digest { .. } | EventBody::Genesis { .. } | EventBody::RoleGrant { .. } => {
            return None;
        }
    })
}

/// Longest text served by [`full_event`], in bytes.
pub const FULL_MAX: usize = 8 * 1024;

fn cap_bytes(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut n = max.saturating_sub('…'.len_utf8());
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s.truncate(n);
        s.push('…');
    }
    s
}

/// Complete text of `e`; `None` for kinds the view does not show.
pub fn full_event(e: &Event) -> Option<FullEvent> {
    let p = parts(e)?;
    Some(FullEvent {
        id: e.id.to_string(),
        kind: p.kind.to_owned(),
        from: who(&e.author),
        to: p.to,
        ts_ms: e.ts,
        text: cap_bytes(sanitize_display(&redact(&p.raw).text, usize::MAX), FULL_MAX),
        refs: p.refs.iter().map(|r| text(r)).collect(),
    })
}

/// The event whose hex id is `id` (full, or a unique prefix of >= 6 chars),
/// as JSON. `None` if unknown, ambiguous or not shown.
pub fn event_json(store: &Store, id: &str) -> Result<Option<String>, hm_store::StoreError> {
    let id = id.to_ascii_lowercase();
    if id.len() < SHORT_ID || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let all = store.events_since(hm_proto::Lamport(0))?;
    let mut hits = all.iter().filter(|e| e.id.to_string().starts_with(&id));
    let found = match (hits.next(), hits.next()) {
        (Some(e), None) => full_event(e),
        _ => None,
    };
    Ok(found.and_then(|f| serde_json::to_string(&f).ok()))
}

fn task_status(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Open => "open",
        TaskStatus::InProgress => "doing",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Done => "done",
        TaskStatus::Dropped => "dropped",
    }
}

/// Short phrase for the last action of an agent, e.g. `answered bob`.
fn action(e: &Event, by_id: &BTreeMap<EventId, &Event>) -> Option<String> {
    let name = |a: &AgentRef| a.agent_name.clone();
    Some(match &e.body {
        EventBody::Ask { to, .. } => match to {
            Some(t) => format!("asked {}", name(t)),
            None => "asked".to_owned(),
        },
        EventBody::Answer { ask_id, .. } => match by_id.get(ask_id) {
            Some(q) => format!("answered {}", name(&q.author)),
            None => "answered".to_owned(),
        },
        EventBody::Handoff { to, .. } => match to {
            Some(t) => format!("handed off to {}", name(t)),
            None => "handed off".to_owned(),
        },
        EventBody::LeaseClaim { .. } => "claimed".to_owned(),
        EventBody::LeaseRelease { .. } => "released".to_owned(),
        EventBody::TaskAdd { task, .. } => format!("added {}", task.as_str()),
        EventBody::TaskUpdate { task, status, .. } => {
            format!("{} {}", task.as_str(), task_status(*status))
        }
        EventBody::Decision { .. } => "decided".to_owned(),
        EventBody::Note { .. } => "noted".to_owned(),
        _ => return None,
    })
}

fn cut(s: &str) -> String {
    let redacted = redact(s).text;
    if redacted.chars().count() > PART_MAX {
        format!(
            "{}\u{2026}",
            redacted.chars().take(PART_MAX - 1).collect::<String>()
        )
    } else {
        redacted
    }
}

/// Build the snapshot. `events_since` is read from the log; `you` is the
/// viewing agent (never added to the room).
pub fn build(
    name: &str,
    store: &Store,
    me: &AgentRef,
    now: u64,
    since: u64,
    limit: usize,
) -> Result<Feed, hm_store::StoreError> {
    let views = store.views();
    let all = store.events_since(hm_proto::Lamport(0))?;
    let by_id: BTreeMap<EventId, &Event> = all.iter().map(|e| (e.id, e)).collect();
    let limit = limit.clamp(1, EVENTS_MAX);

    // Leases still alive.
    let mut leases = Vec::new();
    let mut held: BTreeMap<AgentRef, Vec<String>> = BTreeMap::new();
    for c in views.leases.claims() {
        let state = match views.claim_state(&c.id, now) {
            Some(ClaimState::Pending) => "pending",
            Some(ClaimState::Granted) => "granted",
            _ => continue,
        };
        held.entry(c.holder.clone())
            .or_default()
            .push(c.paths.join(", "));
        if leases.len() < LIST_MAX {
            leases.push(FeedLease {
                id: short(&c.id),
                paths: c.paths.iter().map(|p| text(p)).collect(),
                holder: who(&c.holder),
                state: state.to_owned(),
                expires_ms: c.expires_at_ms(),
            });
        }
    }

    // Open tasks; the owner is whoever made the latest change.
    let mut tasks = Vec::new();
    let mut doing: BTreeMap<AgentRef, Vec<String>> = BTreeMap::new();
    for (id, t) in &views.board {
        if matches!(t.status, TaskStatus::Done | TaskStatus::Dropped) {
            continue;
        }
        let owner = by_id.get(&t.updated).map(|e| e.author.clone());
        if matches!(t.status, TaskStatus::InProgress | TaskStatus::Blocked)
            && let Some(o) = &owner
        {
            doing.entry(o.clone()).or_default().push(format!(
                "{} {}",
                id.as_str(),
                task_status(t.status)
            ));
        }
        if tasks.len() < LIST_MAX {
            tasks.push(FeedTask {
                id: text(id.as_str()),
                title: text(&t.title),
                status: task_status(t.status).to_owned(),
                owner: owner.as_ref().map(who),
            });
        }
    }

    // Last event and last real action per agent.
    let mut last_ms: BTreeMap<&AgentRef, u64> = BTreeMap::new();
    let mut last_act: BTreeMap<&AgentRef, &Event> = BTreeMap::new();
    for e in &all {
        let t = last_ms.entry(&e.author).or_default();
        *t = (*t).max(e.ts);
        if action(e, &by_id).is_some() {
            last_act.insert(&e.author, e);
        }
    }

    let members = views
        .members
        .iter()
        .map(|(a, m)| {
            let mut parts = Vec::new();
            if let Some(h) = held.get(a) {
                parts.push(cut(&format!("holds {}", h.join(", "))));
            }
            if let Some(d) = doing.get(a) {
                parts.push(cut(&d.join(", ")));
            }
            if let Some(e) = last_act.get(a)
                && let Some(act) = action(e, &by_id)
            {
                parts.push(format!("{act} {}", age(now, e.ts)));
            }
            let activity = if parts.is_empty() {
                "no activity yet".to_owned()
            } else {
                parts.join(" · ")
            };
            FeedMember {
                id: who(a),
                node: a.node.to_string().chars().take(6).collect(),
                name: text(&a.agent_name),
                role: role_str(m.role.unwrap_or_else(|| views.role_of(&a.node))).to_owned(),
                local: a.node == me.node,
                present: m.present,
                status: status_str(m.status).to_owned(),
                activity: text(&activity),
                last_ms: last_ms.get(a).copied().unwrap_or(0),
            }
        })
        .collect();

    // Events: the newest `limit` on first load, else the next `limit`
    // after the cursor. Lamport is the cursor, so an event synced late with
    // a lower lamport than the cursor is not re-sent (acceptable for a view).
    let shown: Vec<&Event> = all
        .iter()
        .filter(|e| e.lamport.0 > since && parts(e).is_some())
        .collect();
    let more = since != 0 && shown.len() > limit;
    let page: Vec<&Event> = if since == 0 {
        shown[shown.len().saturating_sub(limit)..].to_vec()
    } else {
        shown.into_iter().take(limit).collect()
    };
    let max_lamport = all.iter().map(|e| e.lamport.0).max().unwrap_or(since);
    let cursor = match page.last() {
        Some(l) if more => l.lamport.0,
        _ => max_lamport.max(since),
    };
    let events = page
        .iter()
        .filter_map(|e| {
            let p = parts(e)?;
            let (t, full) = text_full(&p.raw);
            Some(FeedEvent {
                id: e.id.to_string(),
                lamport: e.lamport.0,
                ts_ms: e.ts,
                kind: p.kind.to_owned(),
                from: who(&e.author),
                to: p.to,
                text: t,
                full,
                reference: p.reference.map(|r| text(&r)),
            })
        })
        .collect();

    Ok(Feed {
        room: text(name),
        you: who(me),
        now_ms: now,
        cursor,
        members,
        leases,
        tasks,
        events,
    })
}

/// Complete terminal-safe conversation entry; unlike HTTP detail, no text cap.
pub fn log_line(e: &Event) -> Option<String> {
    let p = parts(e)?;
    let ms = e.ts % 86_400_000;
    Some(format!(
        "{:02}:{:02}:{:02}.{:03}Z {} -> {} [{}] {}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1_000 % 60,
        ms % 1_000,
        who(&e.author),
        p.to.as_deref().unwrap_or("*"),
        p.kind,
        sanitize_display(&redact(&p.raw).text, usize::MAX)
    ))
}
