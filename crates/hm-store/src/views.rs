//! Materialized views: a pure, deterministic reducer over events applied in
//! [`OrderKey`] order. Same event set → same views, whatever the arrival order.

use std::collections::BTreeMap;

use hm_proto::{
    AgentRef, AgentStatus, Event, EventBody, EventId, Lamport, NodeId, Ref, Role, TaskId,
    TaskStatus,
};
use serde::{Deserialize, Serialize};

/// Total order over events: (lamport, author node, id). ADR-001 note 6.
pub type OrderKey = (Lamport, NodeId, EventId);

pub fn order_key(e: &Event) -> OrderKey {
    (e.lamport, e.author.node, e.id)
}

/// Next Lamport value after everything seen: `max(seen) + 1`, or 1 if none.
pub fn next_lamport(seen: impl IntoIterator<Item = Lamport>) -> Lamport {
    let max = seen.into_iter().map(|l| l.0).max().unwrap_or(0);
    Lamport(max.saturating_add(1))
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
pub struct Lease {
    pub holder: AgentRef,
    pub claim: EventId,
    pub lamport: Lamport,
    /// Claim `ts` + ttl. Informational wall clock, compared only against claim `ts`
    /// during reduction so views stay deterministic.
    pub expires_at_ms: u64,
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
    /// `None` until a `Join` is seen.
    pub role: Option<Role>,
    pub present: bool,
    pub status: Option<AgentStatus>,
    /// Lamport of this agent's latest event: it has seen everything below it.
    pub last_lamport: Lamport,
}

/// Outcome of a lease claim against the current holder of one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    Granted,
    Lost,
}

/// Lease conflict policy. M1 uses [`order_only`]; M1-4 plugs settle logic in here.
pub type ClaimPolicy = fn(current: Option<&Lease>, claim: &Lease, claim_ts: u64) -> ClaimOutcome;

/// First claim in event order wins while it is unexpired at the new claim's `ts`.
/// The same holder may renew.
pub fn order_only(current: Option<&Lease>, claim: &Lease, claim_ts: u64) -> ClaimOutcome {
    match current {
        Some(cur) if cur.holder != claim.holder && cur.expires_at_ms > claim_ts => {
            ClaimOutcome::Lost
        }
        _ => ClaimOutcome::Granted,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Views {
    pub board: BTreeMap<TaskId, Task>,
    /// Current holder per path. Expired entries stay until replaced; filter with
    /// [`Views::active_leases`].
    pub leases: BTreeMap<String, Lease>,
    pub decisions: Vec<DecisionRec>,
    pub members: BTreeMap<AgentRef, Member>,
}

impl Views {
    /// Leases still valid at `now_ms`.
    pub fn active_leases(&self, now_ms: u64) -> impl Iterator<Item = (&String, &Lease)> {
        self.leases
            .iter()
            .filter(move |(_, l)| l.expires_at_ms > now_ms)
    }

    /// Apply one event. Callers must apply events in ascending [`OrderKey`].
    pub fn apply(&mut self, e: &Event, policy: ClaimPolicy) {
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
            EventBody::Join { role } => {
                member.role = Some(*role);
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
            EventBody::LeaseClaim { paths, ttl_secs } => {
                let claim = Lease {
                    holder: e.author.clone(),
                    claim: e.id,
                    lamport: e.lamport,
                    expires_at_ms: e.ts.saturating_add(u64::from(*ttl_secs) * 1000),
                };
                for p in paths {
                    if policy(self.leases.get(p), &claim, e.ts) == ClaimOutcome::Granted {
                        self.leases.insert(p.clone(), claim.clone());
                    }
                }
            }
            EventBody::LeaseRelease { paths } => {
                for p in paths {
                    if self.leases.get(p).is_some_and(|l| l.holder == e.author) {
                        self.leases.remove(p);
                    }
                }
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
            // Read from the log directly; no materialized view yet.
            EventBody::Handoff { .. }
            | EventBody::Ask { .. }
            | EventBody::Answer { .. }
            | EventBody::Note { .. }
            | EventBody::Digest { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(holder: u8, exp: u64) -> Lease {
        Lease {
            holder: AgentRef::new(NodeId::from_bytes([holder; 32]), "x"),
            claim: EventId::from_bytes([0; 32]),
            lamport: Lamport(1),
            expires_at_ms: exp,
        }
    }

    #[test]
    fn order_only_policy() {
        assert_eq!(order_only(None, &lease(1, 5), 0), ClaimOutcome::Granted);
        assert_eq!(
            order_only(Some(&lease(1, 5)), &lease(2, 9), 4),
            ClaimOutcome::Lost
        );
        assert_eq!(
            order_only(Some(&lease(1, 5)), &lease(2, 9), 5),
            ClaimOutcome::Granted
        );
        assert_eq!(
            order_only(Some(&lease(1, 5)), &lease(1, 9), 0),
            ClaimOutcome::Granted
        );
    }
}
