//! Materialized views: a pure, deterministic reducer over signed events
//! applied in [`OrderKey`] order (ADR-002 §1). Same event set (and pinned
//! genesis) → same views, whatever the arrival order. No local times enter a
//! view; time-based answers ([`Views::active_leases`], [`Views::claim_state`])
//! take `now` at read time.

use std::collections::{BTreeMap, btree_map};

use hm_proto::{
    AgentRef, AgentStatus, Event, EventBody, EventId, Lamport, NodeId, Ref, Role, TaskId,
    TaskStatus,
};
use serde::{Deserialize, Serialize};

use crate::settle::{ClaimRec, ClaimState, Lease, LeaseBook};

/// Total order over events: (lamport, author node, id). ADR-001 note 6.
pub type OrderKey = (Lamport, NodeId, EventId);

pub fn order_key(e: &Event) -> OrderKey {
    (e.lamport, e.author.node, e.id)
}

/// Next Lamport value after everything seen: `max(seen) + 1`, or 1 if none.
/// `None` when the clock is exhausted (never wraps or saturates).
pub fn next_lamport(seen: impl IntoIterator<Item = Lamport>) -> Option<Lamport> {
    let max = seen.into_iter().map(|l| l.0).max().unwrap_or(0);
    max.checked_add(1).map(Lamport)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub title: String,
    pub paths: Vec<String>,
    pub status: TaskStatus,
    pub note: Option<String>,
    pub created_by: AgentRef,
    pub created: EventId,
    pub updated: EventId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionRec {
    pub id: EventId,
    pub author: AgentRef,
    pub lamport: Lamport,
    pub title: String,
    pub rationale: String,
    pub refs: Vec<Ref>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// The node's anchored role; `None` until a `Join` is seen. A `Join`'s
    /// self-declared role is ignored.
    pub role: Option<Role>,
    pub present: bool,
    pub status: Option<AgentStatus>,
    /// Lamport of this agent's latest event: it has seen everything below it.
    pub last_lamport: Lamport,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Views {
    pub board: BTreeMap<TaskId, Task>,
    /// All lease claims and releases; query with [`Views::active_leases`]
    /// and [`Views::claim_state`].
    pub leases: LeaseBook,
    pub decisions: Vec<DecisionRec>,
    pub members: BTreeMap<AgentRef, Member>,
    /// Pinned Genesis event id (from the ticket). Only that Genesis counts.
    pub genesis: Option<EventId>,
    /// Owner named by the pinned Genesis, once it has been applied.
    pub owner: Option<NodeId>,
    /// Explicit grants. Nodes without one are members.
    pub roles: BTreeMap<NodeId, Role>,
    /// Events stored but ignored because their author lacked permission.
    pub ignored: u64,
}

impl Views {
    /// Empty views anchored to `genesis` (if known).
    pub fn new(genesis: Option<EventId>) -> Self {
        Self {
            genesis,
            ..Self::default()
        }
    }

    /// Role of `node`: the genesis owner is always `Owner`; otherwise its
    /// latest grant, else `Member`.
    pub fn role_of(&self, node: &NodeId) -> Role {
        if self.owner.as_ref() == Some(node) {
            return Role::Owner;
        }
        self.roles.get(node).copied().unwrap_or(Role::Member)
    }

    /// Path → lease for every winning claim at `now_ms`.
    pub fn active_leases(&self, now_ms: u64) -> btree_map::IntoIter<String, Lease> {
        self.leases.active(now_ms).into_iter()
    }

    /// Lease state of claim `id` at `now_ms` (see [`crate::settle`]).
    pub fn claim_state(&self, id: &EventId, now_ms: u64) -> Option<ClaimState> {
        self.leases.state(id, now_ms)
    }

    /// Apply one event. Callers must apply events in ascending [`OrderKey`].
    pub fn apply(&mut self, e: &Event) {
        if let EventBody::Genesis { owner } = &e.body {
            if self.genesis == Some(e.id) && self.owner.is_none() {
                self.owner = Some(*owner);
            } else {
                self.ignored += 1;
            }
            return;
        }
        let role = self.role_of(&e.author.node);
        if !role.may(e.kind()) {
            self.ignored += 1;
            return;
        }
        let member = self
            .members
            .entry(e.author.clone())
            .or_insert_with(|| Member {
                role: None,
                present: false,
                status: None,
                last_lamport: e.lamport,
            });
        member.last_lamport = member.last_lamport.max(e.lamport);

        match &e.body {
            EventBody::Join { .. } => {
                member.role = Some(role);
                member.present = true;
            }
            EventBody::Leave => member.present = false,
            EventBody::Presence { status } => member.status = Some(*status),
            EventBody::TaskAdd { task, title, paths } => {
                // First add wins; a later add with the same id is ignored.
                self.board.entry(task.clone()).or_insert_with(|| Task {
                    title: title.clone(),
                    paths: paths.clone(),
                    status: TaskStatus::Open,
                    note: None,
                    created_by: e.author.clone(),
                    created: e.id,
                    updated: e.id,
                });
            }
            EventBody::TaskUpdate { task, status, note } => {
                if let Some(t) = self.board.get_mut(task) {
                    t.status = *status;
                    if note.is_some() {
                        t.note.clone_from(note);
                    }
                    t.updated = e.id;
                }
            }
            EventBody::LeaseClaim { .. } => {
                if let Some(c) = ClaimRec::from_event(e) {
                    self.leases.add_claim(c);
                }
            }
            EventBody::LeaseRelease { claim } => {
                self.leases.add_release(e.author.node, order_key(e), *claim);
            }
            EventBody::Decision {
                title,
                rationale,
                refs,
            } => self.decisions.push(DecisionRec {
                id: e.id,
                author: e.author.clone(),
                lamport: e.lamport,
                title: title.clone(),
                rationale: rationale.clone(),
                refs: refs.clone(),
            }),
            EventBody::RoleGrant { node, role } => {
                // The genesis owner can't be demoted.
                if self.owner.as_ref() != Some(node) {
                    self.roles.insert(*node, *role);
                    for (who, m) in &mut self.members {
                        if who.node == *node && m.role.is_some() {
                            m.role = Some(*role);
                        }
                    }
                }
            }
            // Handled above.
            EventBody::Genesis { .. } => {}
            // Read from the log directly; no materialized view yet.
            EventBody::Handoff { .. }
            | EventBody::Ask { .. }
            | EventBody::Answer { .. }
            | EventBody::Note { .. }
            | EventBody::Digest { .. } => {}
        }
    }
}
