//! Pure receive-path checks: no network.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use ed25519_dalek::SigningKey;
use hm_net::{DropReason, MAX_EVENT_BYTES, SeenSet, screen_event};
use hm_proto::{Event, EventBody, EventId, Lamport, RoomId};

fn key() -> SigningKey {
    SigningKey::from_bytes(&[3; 32])
}

fn note(room: RoomId, text: &str) -> Event {
    Event::sign(
        &key(),
        room,
        "a",
        Lamport(1),
        1,
        EventBody::Note {
            scope: "t".into(),
            text: text.into(),
        },
    )
    .unwrap()
}

const ROOM: RoomId = RoomId::from_bytes([1; 32]);

#[test]
fn valid_event_passes() {
    let ev = note(ROOM, "hi");
    assert_eq!(screen_event(&ev.encode().unwrap(), &ROOM).unwrap(), ev);
}

#[test]
fn foreign_room_dropped() {
    let ev = note(RoomId::from_bytes([2; 32]), "hi");
    assert_eq!(
        screen_event(&ev.encode().unwrap(), &ROOM),
        Err(DropReason::WrongRoom)
    );
}

#[test]
fn tampered_dropped() {
    let mut ev = note(ROOM, "hi");
    ev.ts += 1;
    assert_eq!(
        screen_event(&ev.encode().unwrap(), &ROOM),
        Err(DropReason::BadSignature)
    );
}

#[test]
fn garbage_dropped() {
    assert_eq!(
        screen_event(b"not an event", &ROOM),
        Err(DropReason::Malformed)
    );
}

#[test]
fn oversize_dropped_before_decode() {
    let big = vec![0u8; MAX_EVENT_BYTES + 1];
    assert_eq!(screen_event(&big, &ROOM), Err(DropReason::Oversize));
    assert_eq!(MAX_EVENT_BYTES, 64 * 1024);
}

#[test]
fn seen_set_dedups_and_is_bounded_lru() {
    let id = |b: u8| EventId::from_bytes([b; 32]);
    let mut s = SeenSet::new(3);
    assert!(s.insert(id(1)));
    assert!(!s.insert(id(1)), "duplicate");
    assert!(s.insert(id(2)));
    assert!(s.insert(id(3)));
    // Touch 1 so 2 becomes least recently used.
    assert!(!s.insert(id(1)));
    assert!(s.insert(id(4)));
    assert_eq!(s.len(), 3);
    assert!(s.insert(id(2)), "2 was evicted");
    assert!(!s.insert(id(1)), "1 survived as recently used");
}
