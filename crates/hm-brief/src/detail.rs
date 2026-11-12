//! L2: full detail of one event or thread, fetched by id on demand.

use hm_proto::{Event, EventBody, EventId, Lamport, Ref};
use hm_store::Store;

use crate::BriefError;
use crate::text::{agent_label, clean, ref_str, task_status, wrap_max};

/// Per-field cap for L2 text (the reader asked for detail).
const DETAIL_CHARS: usize = 2000;

fn all(store: &Store) -> Result<Vec<Event>, BriefError> {
    Ok(store.events_since(Lamport(0))?)
}

/// Resolve a full or short (≥ 4 hex chars) event id. `None` if unknown or
/// ambiguous.
pub fn find_event(store: &Store, id_or_prefix: &str) -> Result<Option<Event>, BriefError> {
    let p = id_or_prefix.trim().to_ascii_lowercase();
    if p.len() < 4 || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let mut hits = all(store)?
        .into_iter()
        .filter(|e| e.id.to_string().starts_with(&p));
    let first = hits.next();
    Ok(if hits.next().is_some() { None } else { first })
}

fn refs_str(refs: &[Ref]) -> String {
    if refs.is_empty() {
        String::new()
    } else {
        let v: Vec<String> = refs.iter().map(ref_str).collect();
        format!(" refs {}", v.join(" "))
    }
}

fn body_text(e: &Event) -> String {
    let a = &e.author;
    let w = |t: &str| wrap_max(a, t, DETAIL_CHARS);
    let to_label =
        |to: &Option<hm_proto::AgentRef>| to.as_ref().map_or("all".to_owned(), agent_label);
    match &e.body {
        EventBody::Ask { to, q } => format!("to {}: {}", to_label(to), w(q)),
        EventBody::Answer { ask_id, a: ans } => format!("re {ask_id}: {}", w(ans)),
        EventBody::Note { scope, text } => format!("scope {}: {}", clean(scope, 80), w(text)),
        EventBody::Decision {
            title,
            rationale,
            refs,
        } => format!("{} why: {}{}", w(title), w(rationale), refs_str(refs)),
        EventBody::Handoff {
            to,
            summary,
            refs,
            next_steps,
        } => format!(
            "to {}: {}{} next: {}",
            to_label(to),
            w(summary),
            refs_str(refs),
            w(&next_steps.join("; "))
        ),
        EventBody::Digest { since, text } => format!("upto L{}: {}", since.0, w(text)),
        EventBody::TaskAdd { task, title, paths } => {
            format!("{task} {} paths {}", w(title), clean(&paths.join(" "), 200))
        }
        EventBody::TaskUpdate { task, status, note } => format!(
            "{task} {} {}",
            task_status(*status),
            note.as_deref().map_or(String::new(), w)
        ),
        EventBody::LeaseClaim { paths, ttl_secs } => {
            format!("{} ttl {ttl_secs}s", clean(&paths.join(" "), 200))
        }
        other => format!("{:?}", other.kind()),
    }
}

fn line(e: &Event) -> String {
    format!(
        "{:?} {} by {} L{} seq{} ts{} | {}",
        e.kind(),
        e.id,
        agent_label(&e.author),
        e.lamport.0,
        e.seq,
        e.ts,
        body_text(e)
    )
}

/// Full detail for one event (full id, author, position, whole text).
pub fn get_event(store: &Store, id: &EventId) -> Result<Option<String>, BriefError> {
    Ok(all(store)?.iter().find(|e| e.id == *id).map(line))
}

/// The ask/answer thread containing `id` (from either end); any other event
/// is its own thread.
pub fn get_thread(store: &Store, id: &EventId) -> Result<Option<String>, BriefError> {
    let events = all(store)?;
    let Some(e) = events.iter().find(|e| e.id == *id) else {
        return Ok(None);
    };
    let root = match &e.body {
        EventBody::Answer { ask_id, .. } => *ask_id,
        _ => e.id,
    };
    let mut lines: Vec<String> = events
        .iter()
        .filter(|x| {
            x.id == root || matches!(&x.body, EventBody::Answer { ask_id, .. } if *ask_id == root)
        })
        .map(line)
        .collect();
    if lines.is_empty() {
        lines.push(line(e));
    }
    Ok(Some(lines.join("\n")))
}
