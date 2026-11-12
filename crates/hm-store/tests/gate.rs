//! ADR-002 core: seq, pure views, single lease model, checked Lamport, skew
//! guard, local first-seen safety, anchored roles, per-author seq sync,
//! equivocation.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use ed25519_dalek::SigningKey;
use hm_proto::limits::{MAX_FUTURE_SKEW_MS, MAX_LAMPORT_JUMP};
use hm_proto::{Event, EventBody, EventId, Lamport, NodeId, Role, RoomId, TaskId};
use hm_store::settle::{self, ClaimState, SETTLE_MS};
use hm_store::{
    AppendOutcome, AuthorSeqs, PENDING_CAP, SUMMARY_TAIL_ROWS, Store, StoreError, Views,
};
use proptest::prelude::*;

const NOW: u64 = 10_000_000;

fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(blake3::hash(&[b'g', n]).as_bytes())
}

fn node(n: u8) -> NodeId {
    NodeId::from_verifying_key(&key(n).verifying_key())
}

fn room() -> RoomId {
    RoomId::from_bytes([3; 32])
}

fn sign(k: u8, seq: u64, lamport: u64, ts: u64, body: EventBody) -> Event {
    Event::sign(&key(k), room(), "ag", seq, Lamport(lamport), ts, body).unwrap()
}

fn note(k: u8, seq: u64, lamport: u64, text: &str) -> Event {
    sign(
        k,
        seq,
        lamport,
        NOW,
        EventBody::Note {
            scope: "s".into(),
            text: text.into(),
        },
    )
}

fn claim(k: u8, seq: u64, lamport: u64, ts: u64, paths: &[&str]) -> Event {
    sign(
        k,
        seq,
        lamport,
        ts,
        EventBody::LeaseClaim {
            paths: paths.iter().map(|p| p.to_string()).collect(),
            ttl_secs: 600,
        },
    )
}

fn store() -> Store {
    Store::open_in_memory(room()).unwrap()
}

fn snapshot(v: &Views) -> Vec<u8> {
    postcard::to_allocvec(v).unwrap()
}

// ---- (a) seq ----

#[test]
fn next_seq_counts_per_author() {
    let mut s = store();
    assert_eq!(s.next_seq(&node(1)).unwrap(), 1);
    s.append_at(&note(1, 1, 1, "a"), NOW).unwrap();
    s.append_at(&note(1, 2, 2, "b"), NOW).unwrap();
    s.append_at(&note(2, 1, 3, "c"), NOW).unwrap();
    assert_eq!(s.next_seq(&node(1)).unwrap(), 3);
    assert_eq!(s.next_seq(&node(2)).unwrap(), 2);
}

#[test]
fn genesis_and_role_grant_roundtrip_through_store() {
    let g = sign(1, 1, 1, NOW, EventBody::Genesis { owner: node(1) });
    let r = sign(
        1,
        2,
        2,
        NOW,
        EventBody::RoleGrant {
            node: node(2),
            role: Role::Observer,
        },
    );
    let mut s = store();
    s.append_at(&g, NOW).unwrap();
    s.append_at(&r, NOW).unwrap();
    let back = s.events_for(&node(1), 0, &[], 10).unwrap();
    assert_eq!(back, vec![g, r]);
}

// ---- (b) views are pure over signed data ----

#[test]
fn views_do_not_depend_on_local_arrival_time() {
    let evs = [
        claim(1, 1, 1, NOW, &["src/**"]),
        claim(2, 1, 2, NOW, &["src/a.rs"]),
        note(1, 2, 3, "x"),
    ];
    let mut early = store();
    let mut late = store();
    for e in &evs {
        early.append_at(e, NOW).unwrap();
        late.append_at(e, NOW + 3_600_000).unwrap();
    }
    assert_eq!(snapshot(early.views()), snapshot(late.views()));
    // Time-based state is computed at read time.
    let id = evs[0].id;
    assert_eq!(
        early.claim_state(&id, NOW).unwrap(),
        Some(ClaimState::Pending)
    );
    assert_eq!(
        early.claim_state(&id, NOW + SETTLE_MS).unwrap(),
        Some(ClaimState::Granted)
    );
    assert_eq!(
        early.claim_state(&id, NOW + 600_000).unwrap(),
        Some(ClaimState::Expired)
    );
}

// ---- (c) views use the settle model ----

#[test]
fn views_leases_follow_settle_atomic_sets() {
    let a = claim(1, 1, 1, NOW, &["src/x.rs"]);
    let b = claim(2, 1, 2, NOW, &["src/x.rs", "docs/y.md"]);
    let c = claim(3, 1, 3, NOW, &["docs/y.md"]);
    let mut s = store();
    for e in [&c, &b, &a] {
        s.append_at(e, NOW).unwrap();
    }
    let active: BTreeMap<_, _> = s.views().active_leases(NOW).collect();
    assert_eq!(active["src/x.rs"].claim, a.id);
    assert_eq!(active["docs/y.md"].claim, c.id);
    let all = [a.clone(), b.clone(), c.clone()];
    for e in &all {
        assert_eq!(
            s.claim_state(&e.id, NOW + 5_000).unwrap(),
            settle::claim_state(&e.id, &all, NOW + 5_000)
        );
    }
}

// ---- (d) Lamport ----

#[test]
fn u64_max_lamport_is_rejected() {
    let mut s = store();
    let e = note(1, 1, u64::MAX, "x");
    assert!(matches!(s.append_at(&e, NOW), Err(StoreError::LamportJump)));
    assert_eq!(s.event_count().unwrap(), 0);
    // Even after a large but legal history.
    s.append_at(&note(2, 1, MAX_LAMPORT_JUMP, "a"), NOW)
        .unwrap();
    let r = s
        .append_batch_at(&[note(1, 1, u64::MAX, "x")], NOW)
        .unwrap();
    assert_eq!((r.inserted, r.rejected), (0, 1));
}

#[test]
fn lamport_jump_limit_is_relative_to_local_max() {
    let mut s = store();
    s.append_at(&note(1, 1, 10, "a"), NOW).unwrap();
    assert!(matches!(
        s.append_at(&note(2, 1, 10 + MAX_LAMPORT_JUMP + 1, "b"), NOW),
        Err(StoreError::LamportJump)
    ));
    assert_eq!(
        s.append_at(&note(2, 1, 10 + MAX_LAMPORT_JUMP, "b"), NOW)
            .unwrap(),
        AppendOutcome::Inserted
    );
    // Within one batch the max advances event by event (in order).
    let r = s
        .append_batch_at(
            &[
                note(3, 2, 10 + 3 * MAX_LAMPORT_JUMP, "d"),
                note(3, 1, 10 + 2 * MAX_LAMPORT_JUMP, "c"),
            ],
            NOW,
        )
        .unwrap();
    assert_eq!(r.inserted, 2);
}

#[test]
fn next_lamport_errors_instead_of_wrapping() {
    // Chain up to u64::MAX in legal steps is impractical; check the helper.
    assert_eq!(hm_store::next_lamport([Lamport(u64::MAX)]), None);
    assert_eq!(store().next_lamport().unwrap(), Lamport(1));
}

// ---- (e) skew guard ----

#[test]
fn future_event_is_deferred_then_stored_when_time_comes() {
    let mut s = store();
    let future = sign(1, 1, 1, NOW + MAX_FUTURE_SKEW_MS + 1, EventBody::Leave);
    assert_eq!(s.append_at(&future, NOW).unwrap(), AppendOutcome::Deferred);
    assert_eq!(s.event_count().unwrap(), 0);
    assert_eq!(s.pending_len(), 1);
    // Re-offering does not duplicate the queue.
    s.append_at(&future, NOW).unwrap();
    assert_eq!(s.pending_len(), 1);
    // Exactly at the edge is fine.
    let edge = sign(2, 1, 1, NOW + MAX_FUTURE_SKEW_MS, EventBody::Leave);
    assert_eq!(s.append_at(&edge, NOW).unwrap(), AppendOutcome::Inserted);
    // A later append retries the queue.
    s.append_at(&note(3, 1, 2, "later"), NOW + 1).unwrap();
    assert_eq!(s.event_count().unwrap(), 3);
    assert_eq!(s.pending_len(), 0);
}

#[test]
fn pending_queue_is_bounded_and_keeps_nearest() {
    let mut s = store();
    let base = NOW + MAX_FUTURE_SKEW_MS + 1;
    let evs: Vec<Event> = (0..=PENDING_CAP as u64)
        .map(|i| {
            sign(
                1,
                i + 1,
                i + 1,
                base + (PENDING_CAP as u64 - i),
                EventBody::Leave,
            )
        })
        .collect();
    let r = s.append_batch_at(&evs, NOW).unwrap();
    assert_eq!(r.deferred, PENDING_CAP + 1);
    assert_eq!(s.pending_len(), PENDING_CAP);
    // The farthest one (i = 0) was evicted; time passes, the rest land.
    let r = s.retry_pending(base + PENDING_CAP as u64).unwrap();
    assert_eq!(r.inserted, PENDING_CAP);
    assert_eq!(s.event_count().unwrap(), PENDING_CAP as u64);
    assert!(s.events_for(&node(1), 0, &[], 1).unwrap()[0].seq == 2);
}

// ---- (f) local first-seen settle safety ----

#[test]
fn claim_safe_needs_settled_win_and_no_recent_conflict() {
    let mut s = store();
    let mine = claim(1, 1, 1, NOW, &["src/**"]);
    s.append_at(&mine, NOW).unwrap();
    assert!(
        !s.claim_safe(&mine.id, NOW).unwrap(),
        "inside settle window"
    );
    assert!(s.claim_safe(&mine.id, NOW + SETTLE_MS).unwrap());

    // A conflicting (losing) claim arrives late: unsafe until it is old news.
    let theirs = claim(2, 1, 5, NOW + 100, &["src/a.rs"]);
    let t = NOW + 10_000;
    s.append_at(&theirs, t).unwrap();
    assert_eq!(
        s.claim_state(&theirs.id, t).unwrap(),
        Some(ClaimState::Lost { winner: mine.id })
    );
    assert_eq!(s.first_seen(&theirs.id).unwrap(), Some(t));
    assert!(!s.claim_safe(&mine.id, t + SETTLE_MS - 1).unwrap());
    assert!(s.claim_safe(&mine.id, t + SETTLE_MS).unwrap());
    // The loser is never safe.
    assert!(!s.claim_safe(&theirs.id, t + 60_000).unwrap());
}

#[test]
fn backdated_claim_still_waits_out_local_settle() {
    let mut s = store();
    let old = claim(1, 1, 1, NOW - 60_000, &["src/**"]);
    s.append_at(&old, NOW).unwrap();
    assert_eq!(
        s.claim_state(&old.id, NOW).unwrap(),
        Some(ClaimState::Granted)
    );
    assert!(!s.claim_safe(&old.id, NOW).unwrap());
    assert!(s.claim_safe(&old.id, NOW + SETTLE_MS).unwrap());
}

// ---- (g) roles anchored to genesis ----

fn genesis() -> Event {
    sign(1, 1, 1, NOW, EventBody::Genesis { owner: node(1) })
}

fn grant(by: u8, seq: u64, lamport: u64, to: u8, role: Role) -> Event {
    sign(
        by,
        seq,
        lamport,
        NOW,
        EventBody::RoleGrant {
            node: node(to),
            role,
        },
    )
}

fn decision(k: u8, seq: u64, lamport: u64) -> Event {
    sign(
        k,
        seq,
        lamport,
        NOW,
        EventBody::Decision {
            title: "t".into(),
            rationale: String::new(),
            refs: vec![],
        },
    )
}

#[test]
fn observer_events_are_stored_but_ignored() {
    let mut s = store();
    s.pin_genesis(genesis().id).unwrap();
    for e in [
        genesis(),
        grant(1, 2, 2, 2, Role::Observer),
        claim(2, 1, 3, NOW, &["src/**"]),
        decision(2, 2, 4),
        note(2, 3, 5, "observers may talk"),
    ] {
        s.append_at(&e, NOW).unwrap();
    }
    assert_eq!(s.event_count().unwrap(), 5);
    let v = s.views();
    assert_eq!(v.owner, Some(node(1)));
    assert_eq!(v.role_of(&node(2)), Role::Observer);
    assert_eq!(v.active_leases(NOW).count(), 0);
    assert!(v.decisions.is_empty());
    assert_eq!(v.ignored, 2);
}

#[test]
fn only_owner_grants_and_owner_cannot_be_demoted() {
    let mut s = store();
    s.pin_genesis(genesis().id).unwrap();
    for e in [
        genesis(),
        grant(2, 1, 2, 3, Role::Owner),    // member → ignored
        grant(1, 2, 3, 2, Role::Owner),    // owner grants owner
        grant(2, 2, 4, 1, Role::Observer), // granted owner can't demote genesis owner
        grant(2, 3, 5, 3, Role::Observer), // but can grant others
    ] {
        s.append_at(&e, NOW).unwrap();
    }
    let v = s.views();
    assert_eq!(v.role_of(&node(1)), Role::Owner);
    assert_eq!(v.role_of(&node(2)), Role::Owner);
    assert_eq!(v.role_of(&node(3)), Role::Observer);
    assert_eq!(v.role_of(&node(4)), Role::Member);
}

#[test]
fn unpinned_or_other_genesis_and_self_declared_roles_are_ignored() {
    let fake = sign(9, 1, 1, NOW, EventBody::Genesis { owner: node(9) });
    let join = sign(9, 2, 2, NOW, EventBody::Join { role: Role::Owner });
    let g9 = grant(9, 3, 3, 2, Role::Observer);
    // No pin: nobody owns the room.
    let mut s = store();
    for e in [&genesis(), &fake, &join, &g9] {
        s.append_at(e, NOW).unwrap();
    }
    assert_eq!(s.views().owner, None);
    assert_eq!(s.views().role_of(&node(2)), Role::Member);
    // Pinned to the real genesis: the fake one is ignored too.
    s.pin_genesis(genesis().id).unwrap();
    assert_eq!(s.views().owner, Some(node(1)));
    assert_eq!(s.views().role_of(&node(9)), Role::Member);
    assert_eq!(s.views().role_of(&node(2)), Role::Member);
    let m = &s.views().members[&join.author];
    assert_eq!(m.role, Some(Role::Member));
    assert!(matches!(
        s.pin_genesis(fake.id),
        Err(StoreError::GenesisMismatch)
    ));
    s.pin_genesis(genesis().id).unwrap(); // same id: no-op
}

#[test]
fn genesis_pin_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.sqlite");
    {
        let mut s = Store::open(&path, room()).unwrap();
        s.pin_genesis(genesis().id).unwrap();
        s.append_at(&genesis(), NOW).unwrap();
    }
    let s = Store::open(&path, room()).unwrap();
    assert_eq!(s.genesis(), Some(genesis().id));
    assert_eq!(s.views().owner, Some(node(1)));
}

// ---- (h) per-author seq sync ----

#[test]
fn seq_summary_tracks_prefix_and_holes() {
    let mut s = store();
    for seq in [1, 2, 4, 7] {
        s.append_at(&note(1, seq, seq, "x"), NOW).unwrap();
    }
    s.append_at(&note(2, 1, 1, "y"), NOW).unwrap();
    let sum = s.seq_summary(16).unwrap();
    assert_eq!(
        sum[&node(1)],
        AuthorSeqs {
            hi: 2,
            max: 7,
            holes: vec![3, 5, 6]
        }
    );
    assert_eq!(
        sum[&node(2)],
        AuthorSeqs {
            hi: 1,
            max: 1,
            holes: vec![]
        }
    );
    assert_eq!(s.seq_summary(2).unwrap()[&node(1)].holes, vec![3, 5]);
    // Filling the hole advances the prefix.
    s.append_at(&note(1, 3, 3, "x"), NOW).unwrap();
    assert_eq!(s.seq_summary(16).unwrap()[&node(1)].hi, 4);
}

#[test]
fn seq_summary_reads_a_bounded_tail_per_author() {
    // GATE-2 N4: an author that never publishes seq 1 keeps hi = 0; its
    // whole sparse tail must not be read on every summary.
    let mut s = store();
    let n = 2 * SUMMARY_TAIL_ROWS as u64;
    let evs: Vec<Event> = (1..=n).map(|i| note(1, 2 * i, i, "x")).collect();
    s.append_batch_at(&evs, NOW).unwrap();
    let a = s.seq_summary(usize::MAX).unwrap()[&node(1)].clone();
    assert_eq!((a.hi, a.max), (0, 2 * n));
    assert!(a.holes.len() <= SUMMARY_TAIL_ROWS, "{}", a.holes.len());
    assert_eq!(a.holes[..3], [1, 3, 5]);
}

#[test]
fn seq_summary_page_lists_authors_after_cursor_with_limit() {
    let mut s = store();
    for k in 1..=5 {
        s.append_at(&note(k, 1, 1, "x"), NOW).unwrap();
    }
    let mut all: Vec<NodeId> = (1..=5).map(node).collect();
    all.sort();
    let keys = |m: BTreeMap<NodeId, AuthorSeqs>| m.into_keys().collect::<Vec<_>>();
    assert_eq!(keys(s.seq_summary_page(None, 2, 0).unwrap()), all[..2]);
    assert_eq!(
        keys(s.seq_summary_page(Some(all[1]), 10, 0).unwrap()),
        all[2..]
    );
    let p = s.seq_summary_page(Some(all[0]), 1, 0).unwrap();
    assert_eq!(
        p[&all[1]],
        AuthorSeqs {
            hi: 1,
            max: 1,
            holes: vec![]
        }
    );
}

#[test]
fn events_for_serves_holes_then_range_with_limit() {
    let mut s = store();
    for seq in 1..=10 {
        s.append_at(&note(1, seq, seq, "x"), NOW).unwrap();
    }
    s.append_at(&note(2, 1, 1, "other"), NOW).unwrap();
    let seqs = |v: Vec<Event>| v.iter().map(|e| e.seq).collect::<Vec<_>>();
    assert_eq!(
        seqs(s.events_for(&node(1), 7, &[], 100).unwrap()),
        [8, 9, 10]
    );
    assert_eq!(
        seqs(s.events_for(&node(1), 7, &[5, 2, 2, 9, 0], 100).unwrap()),
        [2, 5, 8, 9, 10]
    );
    assert_eq!(
        seqs(s.events_for(&node(1), 7, &[5, 2], 3).unwrap()),
        [2, 5, 8]
    );
    assert_eq!(seqs(s.events_for(&node(1), 1, &[], 2).unwrap()), [2, 3]);
    assert!(s.events_for(&node(3), 0, &[], 10).unwrap().is_empty());
}

#[test]
fn equivocation_keeps_lowest_id_and_is_marked() {
    let a = note(1, 1, 1, "a");
    let b = note(1, 1, 1, "b");
    let (lo, hi) = if a.id < b.id { (a, b) } else { (b, a) };
    for order in [[&lo, &hi], [&hi, &lo]] {
        let mut s = store();
        s.append_at(order[0], NOW).unwrap();
        let r = s.append_batch_at(&[order[1].clone()], NOW).unwrap();
        assert_eq!(r.equivocations, 1);
        assert_eq!(s.event_count().unwrap(), 1);
        assert_eq!(
            s.events_for(&node(1), 0, &[], 10).unwrap(),
            vec![lo.clone()]
        );
        let marks = s.equivocations().unwrap();
        assert_eq!(marks.len(), 1);
        assert_eq!((marks[0].node, marks[0].seq), (node(1), 1));
        assert_eq!((marks[0].kept, marks[0].dropped), (lo.id, hi.id));
        assert_eq!(s.first_seen(&hi.id).unwrap(), None);
    }
}

// ---- properties ----

const PATHS: [&str; 3] = ["a.rs", "b.rs", "dir/**"];

fn arb_body() -> impl Strategy<Value = EventBody> {
    let path = prop::sample::select(PATHS.to_vec()).prop_map(String::from);
    let paths = prop::collection::vec(path, 1..3);
    let who = (0u8..4).prop_map(node);
    let role = prop::sample::select(vec![Role::Owner, Role::Member, Role::Observer]);
    prop_oneof![
        role.clone().prop_map(|role| EventBody::Join { role }),
        Just(EventBody::Leave),
        (who.clone(), role).prop_map(|(node, role)| EventBody::RoleGrant { node, role }),
        (paths, 1u32..30).prop_map(|(paths, ttl_secs)| EventBody::LeaseClaim { paths, ttl_secs }),
        (0u8..4).prop_map(|n| EventBody::LeaseRelease {
            claim: EventId::from_bytes([n; 32])
        }),
        "[a-z]{1,4}".prop_map(|title| EventBody::Decision {
            title,
            rationale: String::new(),
            refs: vec![],
        }),
        "[a-z]{1,4}".prop_map(|t| EventBody::TaskAdd {
            task: TaskId::new("t1").unwrap(),
            title: t,
            paths: vec![],
        }),
    ]
}

/// (author, seq, lamport, ts, body). Small seq range forces equivocations;
/// the first event is the genesis.
fn arb_events() -> impl Strategy<Value = Vec<Event>> {
    prop::collection::vec((0u8..4, 1u64..5, 1u64..8, 0u64..40_000, arb_body()), 1..24).prop_map(
        |specs| {
            let mut evs = vec![genesis()];
            evs.extend(
                specs
                    .into_iter()
                    // Seq 1 of node 1 is the genesis; keep it unique.
                    .filter(|(k, seq, ..)| !(*k == 1 && *seq == 1))
                    .map(|(k, seq, l, ts, body)| sign(k, seq, l, NOW - 40_000 + ts, body)),
            );
            evs
        },
    )
}

fn apply_all(events: &[Event], batch: bool) -> Store {
    let mut s = store();
    s.pin_genesis(genesis().id).unwrap();
    if batch {
        s.append_batch_at(events, NOW).unwrap();
    } else {
        for e in events {
            let _ = s.append_at(e, NOW).unwrap();
        }
    }
    s
}

fn shuffled() -> impl Strategy<Value = (Vec<Event>, Vec<Event>)> {
    arb_events().prop_flat_map(|evs| (Just(evs.clone()), Just(evs).prop_shuffle()))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Same event set, any order (one by one or batched) → identical views,
    /// identical stored set, identical equivocation marks.
    #[test]
    fn same_event_set_any_order_identical_views((events, perm) in shuffled(), batch in any::<bool>()) {
        let a = apply_all(&events, false);
        let b = apply_all(&perm, batch);
        prop_assert_eq!(snapshot(a.views()), snapshot(b.views()));
        let all = |s: &Store| s.events_since(Lamport(0)).unwrap();
        prop_assert_eq!(all(&a), all(&b));
        prop_assert_eq!(a.equivocations().unwrap(), b.equivocations().unwrap());
        for now in [NOW, NOW + 20_000] {
            let la: Vec<_> = a.views().active_leases(now).collect();
            let lb: Vec<_> = b.views().active_leases(now).collect();
            prop_assert_eq!(la, lb);
        }
    }

    /// Equivocation: for every (author, seq) the stored event is the one
    /// with the lowest id among those offered.
    #[test]
    fn equivocation_is_deterministic((events, perm) in shuffled()) {
        let s = apply_all(&perm, false);
        let mut lowest: BTreeMap<(NodeId, u64), EventId> = BTreeMap::new();
        for e in &events {
            let k = (e.author.node, e.seq);
            let cur = lowest.entry(k).or_insert(e.id);
            if e.id < *cur { *cur = e.id; }
        }
        let stored: BTreeMap<(NodeId, u64), EventId> = s
            .events_since(Lamport(0))
            .unwrap()
            .into_iter()
            .map(|e| ((e.author.node, e.seq), e.id))
            .collect();
        prop_assert_eq!(stored, lowest);
    }

    #[test]
    fn incremental_equals_rebuild(events in arb_events()) {
        let mut s = apply_all(&events, false);
        let inc = snapshot(s.views());
        s.rebuild_views().unwrap();
        prop_assert_eq!(inc, snapshot(s.views()));
    }
}
