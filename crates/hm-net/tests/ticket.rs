#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{Ipv4Addr, SocketAddr};

use hm_net::{RoomSecret, RoomTicket, TICKET_PREFIX, TICKET_VERSION, TicketError};
use hm_proto::EventId;
use iroh::{EndpointAddr, SecretKey};

fn addr(seed: u8, port: u16) -> EndpointAddr {
    EndpointAddr::new(SecretKey::from_bytes(&[seed; 32]).public())
        .with_ip_addr(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
        .with_relay_url("https://relay.example.invalid".parse().unwrap())
}

fn ticket() -> RoomTicket {
    RoomTicket::new(
        RoomSecret::from_bytes([9; 32]),
        EventId::from_bytes([3; 32]),
        vec![addr(1, 4001), addr(2, 4002)],
    )
}

#[test]
fn roundtrip() {
    let t = ticket();
    let s = t.to_string();
    assert!(s.starts_with(TICKET_PREFIX), "{s}");
    assert_eq!(TICKET_PREFIX, "hmroom1");
    let back: RoomTicket = s.parse().unwrap();
    assert_eq!(back, t);
    assert_eq!(back.room_id, t.secret.room_id());
    assert_eq!(back.genesis, EventId::from_bytes([3; 32]));
}

#[test]
fn roundtrip_empty_bootstrap() {
    let t = RoomTicket::new(
        RoomSecret::generate().unwrap(),
        EventId::from_bytes([4; 32]),
        vec![],
    );
    assert_eq!(t.to_string().parse::<RoomTicket>().unwrap(), t);
}

#[test]
fn text_is_lowercase_and_case_insensitive() {
    let s = ticket().to_string();
    assert_eq!(s, s.to_ascii_lowercase());
    assert_eq!(
        s.to_ascii_uppercase()
            .replacen("HMROOM1", "hmroom1", 1)
            .parse::<RoomTicket>()
            .unwrap(),
        ticket()
    );
}

#[test]
fn rejects_missing_prefix() {
    let s = ticket().to_string();
    let body = s.strip_prefix(TICKET_PREFIX).unwrap();
    assert!(matches!(
        body.parse::<RoomTicket>(),
        Err(TicketError::Prefix)
    ));
    assert!(matches!(
        format!("hmroom2{body}").parse::<RoomTicket>(),
        Err(TicketError::Prefix)
    ));
}

#[test]
fn rejects_any_single_char_tamper() {
    let s = ticket().to_string();
    let n = TICKET_PREFIX.len();
    for i in n..s.len() {
        let mut b = s.clone().into_bytes();
        b[i] = if b[i] == b'a' { b'b' } else { b'a' };
        let t = String::from_utf8(b).unwrap();
        assert!(t.parse::<RoomTicket>().is_err(), "tamper at {i} accepted");
    }
}

#[test]
fn rejects_truncation() {
    let s = ticket().to_string();
    assert!(s[..s.len() - 2].parse::<RoomTicket>().is_err());
    assert!(TICKET_PREFIX.parse::<RoomTicket>().is_err());
}

#[test]
fn rejects_unknown_version() {
    // v1 tickets (no genesis id) are no longer accepted.
    assert_eq!(TICKET_VERSION, 2);
    for v in [1, 3] {
        let s = RoomTicket::encode_with_version(&ticket(), v);
        assert!(
            matches!(s.parse::<RoomTicket>(), Err(TicketError::Version { found }) if found == v),
            "v{v}"
        );
    }
}

#[test]
fn rejects_room_id_not_derived_from_secret() {
    let mut t = ticket();
    t.room_id = RoomSecret::from_bytes([8; 32]).room_id();
    assert!(matches!(
        t.to_string().parse::<RoomTicket>(),
        Err(TicketError::RoomIdMismatch)
    ));
}

#[test]
fn room_id_and_topic_are_derived_and_distinct() {
    let s = RoomSecret::from_bytes([9; 32]);
    assert_eq!(s.room_id().as_bytes(), blake3::hash(&[9; 32]).as_bytes());
    let mut h = blake3::Hasher::new();
    h.update(b"herdmesh/topic");
    h.update(&[9; 32]);
    assert_eq!(s.topic_id().as_bytes(), h.finalize().as_bytes());
    assert_ne!(s.room_id().as_bytes(), s.topic_id().as_bytes());
}

#[test]
fn generated_secrets_differ() {
    assert_ne!(
        RoomSecret::generate().unwrap(),
        RoomSecret::generate().unwrap()
    );
}

#[test]
fn debug_never_shows_secret() {
    let t = ticket();
    let hex: String = [9u8; 32].iter().map(|b| format!("{b:02x}")).collect();
    for dbg in [
        format!("{:?}", t.secret),
        format!("{t:?}"),
        format!("{t:#?}"),
    ] {
        assert!(!dbg.contains(&hex), "{dbg}");
        assert!(!dbg.contains("9, 9, 9"), "{dbg}");
    }
}
