//! Acceptance tests for the event model: roundtrip, stable id, tamper rejection.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use ed25519_dalek::SigningKey;
use hm_proto::{
    AgentRef, AgentStatus, Event, EventBody, EventId, EventKind, Lamport, NodeId, ProtoError, Ref,
    Role, RoomId, TaskId, TaskStatus, WIRE_VERSION,
};
use proptest::prelude::*;

/// Fresh key from the OS RNG; never a hard-coded key.
fn key() -> SigningKey {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).unwrap();
    SigningKey::from_bytes(&seed)
}

fn room() -> RoomId {
    RoomId::from_bytes([7u8; 32])
}

fn agent(k: &SigningKey) -> AgentRef {
    AgentRef::new(NodeId::from_verifying_key(&k.verifying_key()), "claude")
}

/// One body of every kind, so every variant is covered by roundtrip tests.
fn all_bodies(k: &SigningKey) -> Vec<EventBody> {
    let ask_id = EventId::from_bytes([3u8; 32]);
    let refs = vec![
        "git:abc1234:crates/hm-proto/src/lib.rs#L10-40"
            .parse::<Ref>()
            .unwrap(),
        "task:t-12".parse().unwrap(),
        Ref::Event(ask_id),
    ];
    vec![
        EventBody::Join { role: Role::Member },
        EventBody::Leave,
        EventBody::Presence {
            status: AgentStatus::Working,
        },
        EventBody::TaskAdd {
            task: TaskId::new("t-12").unwrap(),
            title: "event model".into(),
            paths: vec!["crates/hm-proto/**".into()],
        },
        EventBody::TaskUpdate {
            task: TaskId::new("t-12").unwrap(),
            status: TaskStatus::Done,
            note: Some("merged".into()),
        },
        EventBody::LeaseClaim {
            paths: vec!["crates/hm-proto/**".into()],
            ttl_secs: 3600,
        },
        EventBody::LeaseRelease { claim: ask_id },
        EventBody::Decision {
            title: "postcard wire".into(),
            rationale: "compact, deterministic".into(),
            refs: refs.clone(),
        },
        EventBody::Handoff {
            to: Some(agent(k)),
            summary: "proto done".into(),
            refs: refs.clone(),
            next_steps: vec!["store".into()],
        },
        EventBody::Ask {
            to: None,
            q: "which tokenizer?".into(),
        },
        EventBody::Answer {
            ask_id,
            a: "o200k".into(),
        },
        EventBody::Note {
            scope: "crates/hm-net".into(),
            text: "iroh 1.x".into(),
        },
        EventBody::Digest {
            since: Lamport(10),
            text: "summary".into(),
        },
        EventBody::Genesis {
            owner: agent(k).node,
        },
        EventBody::RoleGrant {
            node: NodeId::from_bytes([9u8; 32]),
            role: Role::Observer,
        },
    ]
}

fn sample(k: &SigningKey, body: EventBody) -> Event {
    Event::sign(k, room(), "claude", 1, Lamport(42), 1_700_000_000_000, body).unwrap()
}

#[test]
fn every_kind_is_covered() {
    let k = key();
    let kinds: Vec<EventKind> = all_bodies(&k).iter().map(EventBody::kind).collect();
    assert_eq!(kinds, EventKind::ALL.to_vec());
}

#[test]
fn roundtrip_encode_decode_all_kinds() {
    let k = key();
    for body in all_bodies(&k) {
        let ev = sample(&k, body);
        ev.verify().unwrap();
        let bytes = ev.encode().unwrap();
        assert_eq!(bytes[0], WIRE_VERSION);
        let back = Event::decode(&bytes).unwrap();
        assert_eq!(back, ev);
        back.verify().unwrap();
    }
}

#[test]
fn id_is_stable_for_equal_events() {
    let k = key();
    let a = sample(&k, EventBody::Leave);
    let b = sample(&k, EventBody::Leave);
    assert_eq!(a.id, b.id);
    assert_eq!(a.id, a.compute_id().unwrap());
    let c = Event::sign(&k, room(), "claude", 1, Lamport(43), a.ts, EventBody::Leave).unwrap();
    assert_ne!(a.id, c.id);
}

#[test]
fn wire_version_is_2() {
    assert_eq!(WIRE_VERSION, 2);
}

#[test]
fn seq_is_signed() {
    let k = key();
    let a = sample(&k, EventBody::Leave);
    let b = Event::sign(&k, room(), "claude", 2, a.lamport, a.ts, EventBody::Leave).unwrap();
    assert_eq!(a.seq, 1);
    assert_ne!(a.id, b.id);
    let mut c = a.clone();
    c.seq = 2;
    assert!(c.verify().is_err());
}

#[test]
fn zero_seq_is_rejected() {
    let k = key();
    assert!(matches!(
        Event::sign(&k, room(), "claude", 0, Lamport(1), 1, EventBody::Leave),
        Err(ProtoError::ZeroSeq)
    ));
    // A peer-built event with seq 0 fails verification even when well signed.
    let mut ev = sample(&k, EventBody::Leave);
    ev.seq = 0;
    assert!(matches!(ev.verify(), Err(ProtoError::ZeroSeq)));
}

#[test]
fn role_permissions() {
    use EventKind as K;
    for kind in EventKind::ALL {
        assert!(Role::Owner.may(kind), "{kind:?}");
    }
    assert!(!Role::Member.may(K::RoleGrant));
    assert!(!Role::Member.may(K::Genesis));
    assert!(Role::Member.may(K::LeaseClaim));
    assert!(Role::Member.may(K::Decision));
    for k in [K::Join, K::Leave, K::Presence, K::Ask, K::Answer, K::Note] {
        assert!(Role::Observer.may(k), "{k:?}");
    }
    for k in [
        K::TaskAdd,
        K::TaskUpdate,
        K::LeaseClaim,
        K::LeaseRelease,
        K::Decision,
        K::Handoff,
        K::Digest,
        K::RoleGrant,
        K::Genesis,
    ] {
        assert!(!Role::Observer.may(k), "{k:?}");
    }
}

#[test]
fn author_node_is_the_signing_key() {
    let k = key();
    let ev = sample(&k, EventBody::Leave);
    assert_eq!(ev.author, agent(&k));
}

#[test]
fn rejects_unknown_wire_version() {
    let k = key();
    let mut bytes = sample(&k, EventBody::Leave).encode().unwrap();
    bytes[0] = WIRE_VERSION.wrapping_add(1);
    assert!(matches!(
        Event::decode(&bytes),
        Err(ProtoError::UnsupportedVersion(_))
    ));
}

#[test]
fn rejects_trailing_bytes_and_empty_input() {
    let k = key();
    let mut bytes = sample(&k, EventBody::Leave).encode().unwrap();
    bytes.push(0);
    assert!(matches!(
        Event::decode(&bytes),
        Err(ProtoError::TrailingBytes)
    ));
    assert!(Event::decode(&[]).is_err());
}

#[test]
fn signature_from_other_key_fails() {
    let (k1, k2) = (key(), key());
    let mut ev = sample(&k1, EventBody::Leave);
    let other = sample(&k2, EventBody::Leave);
    ev.sig = other.sig;
    assert!(matches!(ev.verify(), Err(ProtoError::BadSignature)));
}

#[test]
fn reauthored_event_fails_even_with_recomputed_id() {
    let (k1, k2) = (key(), key());
    let mut ev = sample(&k1, EventBody::Leave);
    ev.author = agent(&k2);
    ev.id = ev.compute_id().unwrap();
    assert!(matches!(ev.verify(), Err(ProtoError::BadSignature)));
}

#[test]
fn wrong_id_fails() {
    let k = key();
    let mut ev = sample(&k, EventBody::Leave);
    ev.id = EventId::from_bytes([0u8; 32]);
    assert!(matches!(ev.verify(), Err(ProtoError::IdMismatch)));
}

fn note_event(k: &SigningKey, text: String, lamport: u64) -> Event {
    Event::sign(
        k,
        room(),
        "claude",
        1,
        Lamport(lamport),
        1,
        EventBody::Note {
            scope: "s".into(),
            text,
        },
    )
    .unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Flip any single byte of the wire form: decode fails or verify fails.
    #[test]
    fn any_wire_byte_tamper_is_rejected(
        text in ".{0,40}",
        lamport in any::<u64>(),
        pos in any::<prop::sample::Index>(),
        xor in 1u8..=255,
    ) {
        let k = key();
        let bytes = note_event(&k, text, lamport).encode().unwrap();
        let mut bad = bytes.clone();
        let i = pos.index(bad.len());
        bad[i] ^= xor;
        if let Ok(ev) = Event::decode(&bad) {
            prop_assert!(ev.verify().is_err(), "tamper at byte {} accepted", i);
        }
    }

    /// Flip one byte of the body text, the author (node or name) or the lamport.
    #[test]
    fn field_tamper_is_rejected(
        text in ".{1,40}",
        lamport in any::<u64>(),
        which in 0usize..5,
        pos in any::<prop::sample::Index>(),
        xor in 1u8..=255,
    ) {
        let k = key();
        let mut ev = note_event(&k, text, lamport);
        match which {
            0 => {
                let EventBody::Note { text, .. } = &mut ev.body else { unreachable!() };
                let mut b = std::mem::take(text).into_bytes();
                let i = pos.index(b.len());
                b[i] ^= xor;
                *text = String::from_utf8_lossy(&b).into_owned();
            }
            1 => {
                let mut node = *ev.author.node.as_bytes();
                node[pos.index(32)] ^= xor;
                ev.author.node = NodeId::from_bytes(node);
            }
            2 => {
                let mut b = ev.author.agent_name.clone().into_bytes();
                let i = pos.index(b.len());
                b[i] ^= xor;
                ev.author.agent_name = String::from_utf8_lossy(&b).into_owned();
            }
            3 => ev.seq ^= u64::from(xor) << (8 * pos.index(8)),
            _ => ev.lamport = Lamport(ev.lamport.0 ^ (u64::from(xor) << (8 * pos.index(8)))),
        }
        prop_assert!(ev.verify().is_err());
    }
}
