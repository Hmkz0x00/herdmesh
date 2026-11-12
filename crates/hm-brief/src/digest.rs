//! Deterministic digest of the log up to a lamport, within a token budget.
//! Built from signed events only (not local state), so every node that holds
//! the same events produces the same text.

use std::collections::BTreeMap;

use hm_proto::{EventBody, Lamport, TaskId, TaskStatus};
use hm_store::Store;

use crate::BriefError;
use crate::fit::{Item, fit};
use crate::text::{clean, task_status};

const PART_CHARS: usize = 80;

/// Result of [`build_digest`]; `text` is the body of an `EventBody::Digest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    pub text: String,
    /// Covers events with lamport ≤ `upto`.
    pub upto: Lamport,
}

/// Summarize everything with lamport ≤ `upto`: newest decisions, unfinished
/// tasks, recent handoffs. Peer text is flattened by [`clean`]; the digest
/// event itself is wrapped as untrusted when read.
pub fn build_digest(store: &Store, upto: Lamport, max_tokens: usize) -> Result<Digest, BriefError> {
    let views = store.views();
    let mut tasks: BTreeMap<TaskId, (String, TaskStatus)> = BTreeMap::new();
    let mut items: Vec<Item> = Vec::new();
    for e in store.events_since(Lamport(0))? {
        if e.lamport > upto {
            break;
        }
        if !views.role_of(&e.author.node).may(e.kind()) {
            continue;
        }
        let pos = e.lamport.0;
        match &e.body {
            EventBody::Decision { title, .. } => items.push(Item {
                score: 90,
                recency: pos,
                pos,
                line: format!("decided: {}", clean(title, PART_CHARS)),
            }),
            EventBody::Handoff { summary, .. } => items.push(Item {
                score: 60,
                recency: pos,
                pos,
                line: format!("handoff: {}", clean(summary, PART_CHARS)),
            }),
            EventBody::TaskAdd { task, title, .. } => {
                tasks
                    .entry(task.clone())
                    .or_insert_with(|| (title.clone(), TaskStatus::Open));
            }
            EventBody::TaskUpdate { task, status, .. } => {
                if let Some(t) = tasks.get_mut(task) {
                    t.1 = *status;
                }
            }
            _ => {}
        }
    }
    for (id, (title, status)) in tasks {
        if matches!(status, TaskStatus::Done | TaskStatus::Dropped) {
            continue;
        }
        let score = if status == TaskStatus::Blocked {
            85
        } else {
            75
        };
        items.push(Item {
            score,
            recency: 0,
            pos: 0,
            line: format!(
                "task {id} {}: {}",
                task_status(status),
                clean(&title, PART_CHARS)
            ),
        });
    }
    let head = vec![format!("digest to L{}", upto.0)];
    Ok(Digest {
        text: fit(&head, items, max_tokens).text,
        upto,
    })
}
