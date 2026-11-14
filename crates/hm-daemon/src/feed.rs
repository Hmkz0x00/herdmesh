//! Read-only room snapshot for `herdmesh view` (ADR-003).
//!
//! [`Feed`] serializes to exactly the JSON of `GET /<token>/api/state`.
//! Free-text fields are redacted, escaped with `sanitize_display`, and
//! capped here before being served over authenticated local IPC.
//!
//! Polling decodes only bounded, indexed history windows. Nothing is written,
//! nobody is made a member; collections and total IPC JSON bytes are capped.

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
/// Nested paths and references per object.
const PATHS_MAX: usize = 16;
const REFS_MAX: usize = 16;
/// JSON byte ceiling, leaving ample room for the IPC envelope (1 MiB).
pub const FEED_JSON_MAX: usize = 128 * 1024;
/// Recent arrivals inspected for member activity and initial state.
const HISTORY_MAX: usize = 256;
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
    agent_label(&AgentRef::new(
        a.node,
        sanitize_display(&redact(&a.agent_name).text, 32),
    ))
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
    truncated: bool,
}

fn parts(e: &Event) -> Option<Parts> {
    let truncated = match &e.body {
        EventBody::Handoff {
            refs, next_steps, ..
        } => refs.len() > REFS_MAX || next_steps.len() > REFS_MAX,
        EventBody::Decision { refs, .. } => refs.len() > REFS_MAX,
        EventBody::LeaseClaim { paths, .. } => paths.len() > PATHS_MAX,
        _ => false,
    };
    let p = |kind, raw: String, reference: Option<String>, to, refs| Parts {
        kind,
        raw,
        reference,
        to,
        refs,
        truncated,
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
            for s in next_steps.iter().take(REFS_MAX) {
                t.push_str("\nnext: ");
                t.push_str(s);
            }
            p(
                "handoff",
                t,
                refs.iter().take(REFS_MAX).find_map(|r| match r {
                    hm_proto::Ref::Event(id) => Some(id.to_string()),
                    _ => None,
                }),
                to.as_ref().map(who),
                refs.iter()
                    .take(REFS_MAX)
                    .map(ToString::to_string)
                    .collect(),
            )
        }
        EventBody::LeaseClaim { paths, .. } => p(
            "claim",
            paths
                .iter()
                .take(PATHS_MAX)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" "),
            Some(short(&e.id)),
            None,
            vec![],
        ),
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
            refs.iter()
                .take(REFS_MAX)
                .map(ToString::to_string)
                .collect(),
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
        refs: p.refs.iter().map(|r| reference(r)).collect(),
        truncated: p.truncated
            || sanitize_display(&redact(&p.raw).text, usize::MAX).len() > FULL_MAX,
        role: None,
    })
}

/// The event whose hex id is `id` (full, or a unique prefix of >= 6 chars),
/// as JSON. `None` if unknown, ambiguous or not shown.
pub fn event_json(store: &Store, id: &str) -> Result<Option<String>, hm_store::StoreError> {
    let id = id.to_ascii_lowercase();
    if id.len() < SHORT_ID || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let Some(e) = store.event_by_prefix(&id)? else {
        return Ok(None);
    };
    // Same rule as the store: an event its author's final role may not write
    // is not shown (the raw audit log is separate).
    if !visible(store, &e) {
        return Ok(None);
    }
    let Some(mut f) = full_event(&e) else {
        return Ok(None);
    };
    f.role = role_mark(store, &e);
    f.to = recipient(store, &e)?;
    Ok(serde_json::to_string(&f).ok())
}

/// The store's rule: views ignore events their author's FINAL role may not
/// write, so the feed must not show them as ordinary events either.
fn visible(store: &Store, e: &Event) -> bool {
    store.views().role_of(&e.author.node).may(e.kind())
}

/// `Some("observer")` for events of an observer (removed) author.
fn role_mark(store: &Store, e: &Event) -> Option<String> {
    (store.views().role_of(&e.author.node) == Role::Observer).then(|| "observer".to_owned())
}

// Structured event identifiers are references, never secret-looking free text.
fn reference(s: &str) -> String {
    let hex = s.strip_prefix("event:").unwrap_or(s);
    if EventId::from_hex(hex).is_some() {
        s.to_owned()
    } else {
        text(s)
    }
}

fn recipient(store: &Store, e: &Event) -> Result<Option<String>, hm_store::StoreError> {
    match &e.body {
        EventBody::Answer { ask_id, .. } => Ok(store
            .event_by_prefix(&ask_id.to_string())?
            .filter(|q| matches!(q.body, EventBody::Ask { .. }))
            .map(|q| who(&q.author))),
        EventBody::Ask { to, .. } | EventBody::Handoff { to, .. } => Ok(to.as_ref().map(who)),
        _ => Ok(None),
    }
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

/// Build a bounded snapshot; `you` is never added to the room.
pub fn build(
    name: &str,
    store: &Store,
    me: &AgentRef,
    now: u64,
    since: u64,
    limit: usize,
) -> Result<Feed, hm_store::StoreError> {
    let views = store.views();
    let mut recent = store.recent_events(HISTORY_MAX + 1)?;
    let mut truncated = recent.len() > HISTORY_MAX || views.members.len() > LIST_MAX;
    if recent.len() > HISTORY_MAX {
        recent.remove(0);
    }
    let all: Vec<&Event> = recent
        .iter()
        .map(|(_, e)| e)
        .filter(|e| visible(store, e))
        .collect();
    let by_id: BTreeMap<EventId, &Event> = all.iter().map(|&e| (e.id, e)).collect();
    let limit = limit.clamp(1, EVENTS_MAX);

    // Leases still alive.
    let mut leases = Vec::new();
    let mut held: BTreeMap<AgentRef, Vec<String>> = BTreeMap::new();
    truncated |= views.leases.claims().nth(HISTORY_MAX).is_some();
    for c in views.leases.claims().take(HISTORY_MAX) {
        if leases.len() == LIST_MAX {
            truncated = true;
            break;
        }
        let state = match views.claim_state(&c.id, now) {
            Some(ClaimState::Pending) => "pending",
            Some(ClaimState::Granted) => "granted",
            _ => continue,
        };
        truncated |= c.paths.len() > PATHS_MAX;
        let paths: Vec<String> = c.paths.iter().take(PATHS_MAX).map(|p| text(p)).collect();
        let held_paths = held.entry(c.holder.clone()).or_default();
        if held_paths.len() < PATHS_MAX {
            held_paths.push(cut(&paths.join(", ")));
        } else {
            truncated = true;
        }
        if leases.len() < LIST_MAX {
            leases.push(FeedLease {
                id: short(&c.id),
                paths,
                holder: who(&c.holder),
                state: state.to_owned(),
                expires_ms: c.expires_at_ms(),
            });
        }
    }

    // Open tasks; the owner is whoever made the latest change.
    let mut tasks = Vec::new();
    let mut doing: BTreeMap<AgentRef, Vec<String>> = BTreeMap::new();
    truncated |= views.board.len() > HISTORY_MAX;
    for (id, t) in views.board.iter().take(HISTORY_MAX) {
        if matches!(t.status, TaskStatus::Done | TaskStatus::Dropped) {
            continue;
        }
        if tasks.len() == LIST_MAX {
            truncated = true;
            break;
        }
        let owner = store
            .event_by_prefix(&t.updated.to_string())?
            .map(|e| e.author);
        if matches!(t.status, TaskStatus::InProgress | TaskStatus::Blocked)
            && let Some(o) = &owner
        {
            let actions = doing.entry(o.clone()).or_default();
            if actions.len() < PATHS_MAX {
                actions.push(format!("{} {}", text(id.as_str()), task_status(t.status)));
            } else {
                truncated = true;
            }
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
        .take(LIST_MAX)
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
            let removed = views.role_of(&a.node) == Role::Observer;
            let activity = if removed {
                "removed".to_owned()
            } else if parts.is_empty() {
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
                present: m.present && !removed,
                status: status_str(m.status).to_owned(),
                activity: text(&activity),
                last_ms: last_ms.get(a).copied().unwrap_or(0),
            }
        })
        .collect();

    // Initial load shows the newest arrivals; subsequent pages use local
    // insertion positions, including late and equal-Lamport peer events.
    let mut arrivals = if since == 0 {
        store.recent_events(EVENTS_MAX + 1)?
    } else {
        store.events_after(since, HISTORY_MAX + 1)?
    };
    if arrivals.len() > HISTORY_MAX {
        arrivals.pop();
        truncated = true;
    }
    let head = arrivals.last().map_or(since, |(position, _)| *position);
    let shown: Vec<_> = arrivals
        .iter()
        .filter(|(_, e)| visible(store, e) && parts(e).is_some())
        .collect();
    let more = shown.len() > limit;
    truncated |= more;
    let page: Vec<_> = if since == 0 {
        shown[shown.len().saturating_sub(limit)..].to_vec()
    } else {
        shown.into_iter().take(limit).collect()
    };
    let cursor = match page.last() {
        Some((position, _)) if since != 0 && more => *position,
        _ => head,
    };
    let mut events = Vec::new();
    for (_, e) in &page {
        let Some(p) = parts(e) else { continue };
        let (t, full) = text_full(&p.raw);
        truncated |= p.truncated;
        events.push(FeedEvent {
            id: e.id.to_string(),
            lamport: e.lamport.0,
            ts_ms: e.ts,
            kind: p.kind.to_owned(),
            from: who(&e.author),
            to: recipient(store, e)?,
            text: t,
            full: full && !p.truncated,
            reference: p.reference.map(|r| reference(&r)),
            role: role_mark(store, e),
        });
    }
    let mut feed = Feed {
        room: text(name),
        you: who(me),
        now_ms: now,
        cursor,
        truncated,
        members,
        leases,
        tasks,
        events,
    };
    let before = feed.events.len();
    fit_bytes(&mut feed);
    if since != 0 && feed.events.len() < before {
        // A byte-limited incremental page must not skip the dropped tail.
        feed.cursor = feed
            .events
            .last()
            .and_then(|e| {
                arrivals
                    .iter()
                    .find(|(_, raw)| raw.id.to_string() == e.id)
                    .map(|(p, _)| *p)
            })
            .unwrap_or(since);
    }
    Ok(feed)
}

// Count serialized bytes, including JSON escaping, rather than Unicode chars.
// Removing whole tail items keeps the result parseable and references intact.
fn fit_bytes(feed: &mut Feed) {
    let Ok(bytes) = serde_json::to_vec(feed) else {
        return;
    };
    let mut size = bytes.len();
    fn trim<T: serde::Serialize>(items: &mut Vec<T>, size: &mut usize) -> bool {
        let mut changed = false;
        while *size > FEED_JSON_MAX {
            let Some(item) = items.pop() else { break };
            if let Ok(bytes) = serde_json::to_vec(&item) {
                *size = size.saturating_sub(bytes.len());
            }
            changed = true;
        }
        changed
    }
    feed.truncated |= trim(&mut feed.leases, &mut size);
    feed.truncated |= trim(&mut feed.tasks, &mut size);
    feed.truncated |= trim(&mut feed.members, &mut size);
    feed.truncated |= trim(&mut feed.events, &mut size);
}

/// Complete terminal-safe conversation entry; unlike HTTP detail, no text cap.
pub fn log_line(e: &Event) -> Option<String> {
    let mut p = parts(e)?;
    // The terminal log is an explicit full-message reader, not a polling IPC
    // snapshot: retain the complete handoff/claim within the event's wire cap.
    match &e.body {
        EventBody::Handoff {
            summary,
            next_steps,
            ..
        } => {
            p.raw.clone_from(summary);
            for step in next_steps {
                p.raw.push_str("\nnext: ");
                p.raw.push_str(step);
            }
        }
        EventBody::LeaseClaim { paths, .. } => p.raw = paths.join(" "),
        _ => {}
    }
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

#[cfg(test)]
mod budget_tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use hm_proto::{NodeId, Ref, RoomId, TaskId};

    fn append(store: &mut Store, name: &str, body: EventBody) -> Event {
        let key = SigningKey::from_bytes(&[8; 32]);
        let node = NodeId::from_verifying_key(&key.verifying_key());
        let e = Event::sign(
            &key,
            store.room(),
            name,
            store.next_seq(&node).unwrap(),
            store.next_lamport().unwrap(),
            0,
            body,
        )
        .unwrap();
        store.append_at(&e, 0).unwrap();
        e
    }

    fn snapshot(store: &Store, since: u64, limit: usize) -> Feed {
        build(
            "test",
            store,
            &AgentRef::new(NodeId::from_bytes([9; 32]), "viewer"),
            1000,
            since,
            limit,
        )
        .unwrap()
    }

    #[test]
    fn caps_peer_collections_and_serialized_ipc_bytes() {
        let mut store = Store::open_in_memory(RoomId::from_bytes([1; 32])).unwrap();
        for n in 0..110 {
            let name = format!("agent-{n}");
            append(&mut store, &name, EventBody::Join { role: Role::Member });
            if n == 0 {
                append(
                    &mut store,
                    &name,
                    EventBody::LeaseClaim {
                        paths: (0..20)
                            .map(|i| format!("src/{n}/{i}/{}", "\u{754c}".repeat(190)))
                            .collect(),
                        ttl_secs: 600,
                    },
                );
            }
            append(
                &mut store,
                &name,
                EventBody::TaskAdd {
                    task: TaskId::new(format!("t-{n}")).unwrap(),
                    title: "\u{754c}".repeat(200),
                    paths: vec![],
                },
            );
        }
        let feed = snapshot(&store, 0, usize::MAX);
        assert!(feed.truncated);
        assert!(feed.members.len() <= LIST_MAX);
        assert!(feed.leases.len() <= LIST_MAX);
        assert!(feed.tasks.len() <= LIST_MAX);
        assert!(feed.events.len() <= EVENTS_MAX);
        assert!(feed.leases.iter().all(|l| l.paths.len() <= PATHS_MAX));
        let json = serde_json::to_string(&feed).unwrap();
        assert!(json.len() <= FEED_JSON_MAX, "{}", json.len());
        let response = crate::ipc::Response::Ok(vec![("feed".into(), json)]);
        assert!(postcard::to_allocvec(&response).unwrap().len() < crate::ipc::MAX_FRAME);
    }

    #[test]
    fn answer_and_handoff_keep_event_links_but_scrub_secret_text() {
        let mut store = Store::open_in_memory(RoomId::from_bytes([1; 32])).unwrap();
        let key = SigningKey::from_bytes(&[8; 32]);
        let bob = AgentRef::new(NodeId::from_verifying_key(&key.verifying_key()), "bob");
        let ask = append(
            &mut store,
            "alice",
            EventBody::Ask {
                to: Some(bob),
                q: "what OS?".into(),
            },
        );
        let answer = append(
            &mut store,
            "bob",
            EventBody::Answer {
                ask_id: ask.id,
                a: "Windows".into(),
            },
        );
        let handoff = append(
            &mut store,
            "bob",
            EventBody::Handoff {
                to: Some(ask.author.clone()),
                summary: "ghp_a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8".into(),
                refs: vec![Ref::Event(ask.id)],
                next_steps: vec![],
            },
        );
        let feed = snapshot(&store, 0, 200);
        let a = feed
            .events
            .iter()
            .find(|e| e.id == answer.id.to_string())
            .unwrap();
        assert_eq!(a.reference.as_deref(), Some(ask.id.to_string().as_str()));
        assert_eq!(a.to.as_deref(), Some(who(&ask.author).as_str()));
        let h = feed
            .events
            .iter()
            .find(|e| e.id == handoff.id.to_string())
            .unwrap();
        assert_eq!(h.reference.as_deref(), Some(ask.id.to_string().as_str()));
        assert!(!h.text.contains("ghp_"));
        for e in [answer, handoff] {
            let detail: FullEvent =
                serde_json::from_str(&event_json(&store, &e.id.to_string()).unwrap().unwrap())
                    .unwrap();
            assert!(detail.refs.iter().any(|r| r.contains(&ask.id.to_string())));
        }
    }

    #[test]
    fn byte_budget_counts_escaped_json_and_keeps_messages() {
        let store = Store::open_in_memory(RoomId::from_bytes([1; 32])).unwrap();
        let mut feed = snapshot(&store, 0, 200);
        feed.leases = (0..LIST_MAX)
            .map(|i| FeedLease {
                id: i.to_string(),
                holder: "a".into(),
                state: "pending".into(),
                expires_ms: 0,
                paths: vec!["\"\u{754c}".repeat(100); PATHS_MAX],
            })
            .collect();
        assert!(serde_json::to_vec(&feed).unwrap().len() > FEED_JSON_MAX);
        fit_bytes(&mut feed);
        assert!(feed.truncated);
        assert!(serde_json::to_vec(&feed).unwrap().len() <= FEED_JSON_MAX);
    }

    #[test]
    fn detail_caps_nested_refs_and_reports_truncation() {
        let mut store = Store::open_in_memory(RoomId::from_bytes([1; 32])).unwrap();
        let event = append(
            &mut store,
            "alice",
            EventBody::Handoff {
                to: None,
                summary: "message ".repeat(2000),
                refs: vec![Ref::Event(EventId::from_bytes([0xab; 32])); REFS_MAX + 10],
                next_steps: vec!["next".into(); REFS_MAX + 10],
            },
        );
        let detail = full_event(&event).unwrap();
        assert!(detail.truncated);
        assert_eq!(detail.refs.len(), REFS_MAX);
        assert!(detail.text.len() <= FULL_MAX);
        assert!(serde_json::to_vec(&detail).unwrap().len() < FEED_JSON_MAX);
    }

    #[test]
    fn incremental_byte_limited_pages_never_skip_messages() {
        let mut store = Store::open_in_memory(RoomId::from_bytes([1; 32])).unwrap();
        append(&mut store, "alice", EventBody::Join { role: Role::Member });
        for _ in 0..210 {
            append(
                &mut store,
                "alice",
                EventBody::Note {
                    scope: "test".into(),
                    text: "\"\u{754c}".repeat(100),
                },
            );
        }
        let mut cursor = 1;
        let mut seen = std::collections::BTreeSet::new();
        let mut byte_limited = false;
        loop {
            let feed = snapshot(&store, cursor, EVENTS_MAX);
            assert!(serde_json::to_vec(&feed).unwrap().len() <= FEED_JSON_MAX);
            byte_limited |= feed.truncated && feed.events.len() < EVENTS_MAX;
            for event in &feed.events {
                assert!(seen.insert(event.id.clone()), "no duplicate event");
            }
            if feed.cursor == cursor {
                break;
            }
            cursor = feed.cursor;
        }
        assert!(byte_limited);
        assert_eq!(seen.len(), 210);
    }

    #[test]
    fn conversation_log_keeps_all_handoff_steps() {
        let mut store = Store::open_in_memory(RoomId::from_bytes([1; 32])).unwrap();
        let event = append(
            &mut store,
            "alice",
            EventBody::Handoff {
                to: None,
                summary: "take over".into(),
                refs: vec![],
                next_steps: (0..20).map(|i| format!("step {i}")).collect(),
            },
        );
        assert!(log_line(&event).unwrap().contains("step 19"));
    }
}

#[cfg(test)]
mod removal_tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use hm_proto::{Lamport, NodeId, RoomId, TaskId};

    const T: u64 = 1_700_000_000_000;

    fn key(n: u8) -> SigningKey {
        SigningKey::from_bytes(&[n; 32])
    }

    fn node(n: u8) -> NodeId {
        NodeId::from_bytes(key(n).verifying_key().to_bytes())
    }

    fn sign(k: u8, name: &str, seq: u64, l: u64, body: EventBody) -> Event {
        Event::sign(
            &key(k),
            RoomId::from_bytes([2; 32]),
            name,
            seq,
            Lamport(l),
            T,
            body,
        )
        .unwrap()
    }

    fn task(seq: u64, l: u64, id: &str) -> Event {
        sign(
            2,
            "mallory",
            seq,
            l,
            EventBody::TaskAdd {
                task: TaskId::new(id).unwrap(),
                title: format!("title-{id}"),
                paths: vec![],
            },
        )
    }

    fn decision(seq: u64, l: u64, title: &str) -> Event {
        sign(
            2,
            "mallory",
            seq,
            l,
            EventBody::Decision {
                title: title.into(),
                rationale: String::new(),
                refs: vec![],
            },
        )
    }

    fn claim(seq: u64, l: u64, path: &str) -> Event {
        sign(
            2,
            "mallory",
            seq,
            l,
            EventBody::LeaseClaim {
                paths: vec![path.into()],
                ttl_secs: 600,
            },
        )
    }

    fn note(seq: u64, l: u64, text: &str) -> Event {
        sign(
            2,
            "mallory",
            seq,
            l,
            EventBody::Note {
                scope: "x".into(),
                text: text.into(),
            },
        )
    }

    fn build_store(events: &[Event]) -> Store {
        let genesis = sign(1, "owner", 1, 1, EventBody::Genesis { owner: node(1) });
        let mut s = Store::open_in_memory(RoomId::from_bytes([2; 32])).unwrap();
        s.pin_genesis(genesis.id).unwrap();
        s.append_at(&genesis, T).unwrap();
        for e in events {
            let _ = s.append_at(e, T);
        }
        s
    }

    fn feed(store: &Store, since: u64, limit: usize) -> Feed {
        let me = AgentRef::new(node(1), "owner");
        build("r", store, &me, T, since, limit).unwrap()
    }

    fn removed_scenario() -> Store {
        build_store(&[
            sign(2, "mallory", 1, 2, EventBody::Join { role: Role::Member }),
            task(2, 3, "early"),
            decision(3, 4, "early-decision"),
            claim(4, 5, "src/early/**"),
            note(5, 6, "hello from before"),
            // Owner demotes mallory at L10.
            sign(
                1,
                "owner",
                2,
                10,
                EventBody::RoleGrant {
                    node: node(2),
                    role: Role::Observer,
                },
            ),
            // Backdated writes signed after removal.
            task(6, 9, "late"),
            decision(7, 9, "late-decision"),
            claim(8, 9, "src/late/**"),
            note(9, 9, "hello from after"),
        ])
    }

    fn kinds_from_mallory(f: &Feed) -> Vec<&str> {
        f.events
            .iter()
            .filter(|e| e.from.contains("mallory"))
            .map(|e| e.kind.as_str())
            .collect()
    }

    #[test]
    fn removed_members_operational_events_never_appear_before_or_after() {
        let s = removed_scenario();
        let f = feed(&s, 0, 200);
        let kinds = kinds_from_mallory(&f);
        for bad in ["task", "decision", "claim", "task_add", "lease", "release"] {
            assert!(!kinds.contains(&bad), "{bad} leaked: {kinds:?}");
        }
        let json = serde_json::to_string(&f).unwrap();
        for bad in [
            "title-early",
            "title-late",
            "early-decision",
            "late-decision",
            "src/early",
            "src/late",
        ] {
            assert!(!json.contains(bad), "{bad} leaked in {json}");
        }
        assert!(f.tasks.is_empty() && f.leases.is_empty());
    }

    #[test]
    fn removed_members_messages_are_marked_observer() {
        let s = removed_scenario();
        let f = feed(&s, 0, 200);
        let notes: Vec<_> = f.events.iter().filter(|e| e.kind == "note").collect();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes.iter().all(|e| e.role.as_deref() == Some("observer")));
        // Ordinary members' events carry no role mark.
        let join = f
            .events
            .iter()
            .find(|e| e.kind == "genesis" || e.from.contains("owner"));
        assert!(join.is_none_or(|e| e.role.is_none()));
    }

    #[test]
    fn member_list_shows_removed_not_active() {
        let s = removed_scenario();
        let f = feed(&s, 0, 200);
        let m = f.members.iter().find(|m| m.name == "mallory").unwrap();
        assert_eq!(m.role, "observer");
        assert!(!m.present);
        assert_eq!(m.activity, "removed");
    }

    #[test]
    fn event_detail_follows_the_same_rule() {
        let s = removed_scenario();
        let ev = |e: &Event| event_json(&s, &e.id.to_string()).unwrap();
        assert!(ev(&task(6, 9, "late")).is_none());
        assert!(ev(&decision(3, 4, "early-decision")).is_none());
        assert!(ev(&claim(8, 9, "src/late/**")).is_none());
        let shown = ev(&note(9, 9, "hello from after")).unwrap();
        assert!(shown.contains(r#""role":"observer""#), "{shown}");
    }

    #[test]
    fn pagination_advances_over_skipped_arrivals() {
        let s = removed_scenario();
        let first = feed(&s, 0, 200);
        // Nothing new: polling from the cursor yields no events and the same head.
        let next = feed(&s, first.cursor, 200);
        assert!(next.events.is_empty());
        assert_eq!(next.cursor, first.cursor);
        // Page through from the start one event at a time: no hidden event
        // appears and the cursor strictly advances to the end.
        let mut since = 1;
        let mut seen = Vec::new();
        for _ in 0..40 {
            let p = feed(&s, since, 1);
            seen.extend(p.events.iter().map(|e| e.kind.clone()));
            if p.cursor == since {
                break;
            }
            since = p.cursor;
        }
        assert!(
            !seen
                .iter()
                .any(|k| k.contains("task") || k == "decision" || k.contains("claim")),
            "{seen:?}"
        );
        assert_eq!(since, first.cursor);
    }
}
