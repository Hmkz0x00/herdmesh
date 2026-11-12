//! L1 brief for one agent: what it missed since its cursor, filtered by
//! relevance, within a token budget. Items are pointers (short ids, refs);
//! full content is one `get_event` away (L2).

use std::collections::{BTreeMap, BTreeSet};

use hm_proto::budget::estimate_tokens;
use hm_proto::{AgentRef, Event, EventBody, EventId, Lamport, TaskStatus};
use hm_store::{ClaimState, Store, settle::paths_overlap};

use crate::BriefError;
use crate::fit::{Item, fit};
use crate::text::{LEGEND, agent_label, clean, ref_str, short_id, task_status, wrap_max};

// Relevance scores: higher survives longer under a tight budget.
const S_DIRECT: u32 = 100; // ask to me, answer to my ask
const S_HANDOFF: u32 = 95;
const S_CONFLICT: u32 = 90;
const S_MY_TASK: u32 = 85;
const S_LATEST_DECISION: u32 = 80;
const S_DECISION: u32 = 60;
const S_TASK_NEW: u32 = 55;
const S_NOTE: u32 = 50;
const S_BROADCAST: u32 = 35;
const S_DIGEST: u32 = 30;
const S_OLD_DECISION: u32 = 20;
/// Decisions after the newest few are rolled to the bottom of the ranking.
const DECISIONS_SHOWN_FIRST: usize = 3;

/// A built brief.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brief {
    pub text: String,
    /// Estimated tokens of `text` (always ≤ the requested max).
    pub tokens: usize,
    /// Items shown / dropped for budget (after dedup).
    pub shown: usize,
    pub omitted: usize,
    /// Highest lamport covered; use as the agent's next cursor.
    pub upto: Lamport,
}

/// What agent `me` has that others may collide with.
struct Mine {
    paths: Vec<String>,
    tasks: BTreeSet<String>,
}

fn prefix_of(p: &str) -> &str {
    let end = p.find(['*', '?', '[', '{', '\\']).unwrap_or(p.len());
    p[..end].trim_end_matches('/')
}

impl Mine {
    fn overlaps(&self, paths: &[String]) -> bool {
        self.paths
            .iter()
            .any(|m| paths.iter().any(|p| paths_overlap(m, p)))
    }

    /// Whether a note scope (a directory-ish string) concerns my paths.
    fn scope_relates(&self, scope: &str) -> bool {
        let s = scope.trim_end_matches('/');
        !s.is_empty()
            && self.paths.iter().any(|m| {
                let pre = prefix_of(m);
                paths_overlap(m, scope) || pre.starts_with(s) || s.starts_with(pre)
            })
    }
}

fn mine(store: &Store, me: &AgentRef, now: u64) -> Mine {
    let views = store.views();
    let mut paths: Vec<String> = views
        .active_leases(now)
        .filter(|(_, l)| l.holder == *me)
        .map(|(p, _)| p)
        .collect();
    let mut tasks = BTreeSet::new();
    for (id, t) in &views.board {
        let live = !matches!(t.status, TaskStatus::Done | TaskStatus::Dropped);
        if live
            && (t.created_by == *me
                || t.paths
                    .iter()
                    .any(|p| paths.iter().any(|m| paths_overlap(m, p))))
        {
            tasks.insert(id.to_string());
            paths.extend(t.paths.iter().cloned());
        }
    }
    paths.sort();
    paths.dedup();
    Mine { paths, tasks }
}

fn refs(r: &[hm_proto::Ref]) -> String {
    r.iter().map(|r| format!(" {}", ref_str(r))).collect()
}

/// One deduplicated candidate line.
struct Cand {
    score: u32,
    count: usize,
    lamport: u64,
    line: String,
}

/// Build the brief for `me` from events with lamport > `cursor`.
pub fn brief(
    store: &Store,
    me: &AgentRef,
    cursor: Lamport,
    now: u64,
    max_tokens: usize,
) -> Result<Brief, BriefError> {
    let views = store.views();
    let events = store.events_since(Lamport(0))?;
    let upto = events.last().map_or(cursor, |e| e.lamport.max(cursor));
    let my = mine(store, me, now);
    let permitted = |e: &Event| views.role_of(&e.author.node).may(e.kind());

    let my_asks: BTreeSet<EventId> = events
        .iter()
        .filter(|e| e.author == *me && matches!(e.body, EventBody::Ask { .. }))
        .map(|e| e.id)
        .collect();
    // Newest decisions first: rank 0 is the latest.
    let decision_rank: BTreeMap<EventId, usize> = events
        .iter()
        .rev()
        .filter(|e| permitted(e) && matches!(e.body, EventBody::Decision { .. }))
        .enumerate()
        .map(|(i, e)| (e.id, i))
        .collect();

    // Dedup key = (kind, author, body text); the newest copy is shown with ×n.
    let mut cands: BTreeMap<String, Cand> = BTreeMap::new();
    for e in events
        .iter()
        .filter(|e| e.lamport > cursor && e.author != *me && permitted(e))
    {
        let who = agent_label(&e.author);
        let id = short_id(&e.id);
        let w = |t: &str| wrap_max(&e.author, t, crate::ITEM_TEXT_MAX_CHARS);
        let (score, key, line) = match &e.body {
            EventBody::Ask { to, q } => match to {
                Some(t) if t == me => (
                    S_DIRECT,
                    format!("ask|{who}|{q}"),
                    format!("ask {id} {}", w(q)),
                ),
                None => (
                    S_BROADCAST,
                    format!("askall|{who}|{q}"),
                    format!("ask(all) {id} {}", w(q)),
                ),
                _ => continue,
            },
            EventBody::Answer { ask_id, a } if my_asks.contains(ask_id) => (
                S_DIRECT,
                format!("ans|{who}|{ask_id}|{a}"),
                format!("answer {id} to {} {}", short_id(ask_id), w(a)),
            ),
            EventBody::Handoff {
                to,
                summary,
                refs: r,
                next_steps,
            } => {
                let score = match to {
                    Some(t) if t == me => S_HANDOFF,
                    None => S_BROADCAST,
                    _ => continue,
                };
                let next = if next_steps.is_empty() {
                    String::new()
                } else {
                    format!(" | next: {}", next_steps.join("; "))
                };
                (
                    score,
                    format!("hand|{who}|{summary}|{next}"),
                    format!("handoff {id} {}{}", w(&format!("{summary}{next}")), refs(r)),
                )
            }
            EventBody::LeaseClaim { paths, .. } if my.overlaps(paths) => {
                let state = match views.claim_state(&e.id, now) {
                    Some(ClaimState::Pending) => "pending",
                    Some(ClaimState::Granted) => "granted",
                    Some(ClaimState::Lost { .. }) => "lost",
                    Some(ClaimState::Expired) => "expired",
                    Some(ClaimState::Released) => "released",
                    Some(ClaimState::Renewed { .. }) => "renewed",
                    None => "?",
                };
                (
                    S_CONFLICT,
                    format!("lease|{who}|{}", paths.join(" ")),
                    format!("lease! {id} {} {state}", w(&paths.join(" "))),
                )
            }
            EventBody::TaskUpdate { task, status, note } if my.tasks.contains(task.as_str()) => {
                let tail = note
                    .as_deref()
                    .map_or(format!(" by {who}"), |n| format!(" {}", w(n)));
                (
                    S_MY_TASK,
                    format!("tu|{who}|{task}|{status:?}|{note:?}"),
                    format!("task {task} {}{tail}", task_status(*status)),
                )
            }
            EventBody::TaskAdd { task, title, paths } if my.overlaps(paths) => (
                S_TASK_NEW,
                format!("ta|{who}|{task}"),
                format!("task+ {task} {}", w(title)),
            ),
            EventBody::Decision { title, refs: r, .. } => {
                let score = match decision_rank.get(&e.id) {
                    Some(0) => S_LATEST_DECISION,
                    Some(&r) if r < DECISIONS_SHOWN_FIRST => S_DECISION,
                    _ => S_OLD_DECISION,
                };
                (
                    score,
                    format!("dec|{who}|{title}"),
                    format!("decision {id} {}{}", w(title), refs(r)),
                )
            }
            EventBody::Note { scope, text } if my.scope_relates(scope) => (
                S_NOTE,
                format!("note|{who}|{scope}|{text}"),
                format!("note {id} {} {}", clean(scope, 40), w(text)),
            ),
            _ => continue,
        };
        let c = cands.entry(key).or_insert(Cand {
            score,
            count: 0,
            lamport: 0,
            line: String::new(),
        });
        c.count += 1;
        c.score = c.score.max(score);
        if e.lamport.0 >= c.lamport {
            c.lamport = e.lamport.0;
            c.line = line;
        }
    }

    let mut items: Vec<Item> = cands
        .into_values()
        .map(|c| Item {
            score: c.score,
            recency: c.lamport,
            pos: c.lamport,
            line: if c.count > 1 {
                format!("{} ×{}", c.line, c.count)
            } else {
                c.line
            },
        })
        .collect();

    // Older material comes from the latest digest, only if there is unread news.
    if upto > cursor
        && let Some((e, since, text)) = events.iter().rev().find_map(|e| match &e.body {
            EventBody::Digest { since, text } if permitted(e) => Some((e, since, text)),
            _ => None,
        })
    {
        items.push(Item {
            score: S_DIGEST,
            recency: 0,
            pos: 0,
            line: format!(
                "digest L{} {}",
                since.0,
                wrap_max(&e.author, text, crate::ITEM_TEXT_MAX_CHARS)
            ),
        });
    }

    let head = vec![
        format!("brief {} L{}→L{}", agent_label(me), cursor.0, upto.0),
        LEGEND.to_owned(),
    ];
    let empty = items.is_empty();
    if empty {
        items.push(Item {
            score: 0,
            recency: 0,
            pos: 0,
            line: "nothing new".into(),
        });
    }
    let f = fit(&head, items, max_tokens);
    let shown = if empty { 0 } else { f.shown };
    Ok(Brief {
        tokens: estimate_tokens(&f.text),
        text: f.text,
        shown,
        omitted: f.omitted,
        upto,
    })
}
