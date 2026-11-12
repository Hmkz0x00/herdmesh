//! Acceptance tests for `Ref` parse/print.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use hm_proto::{EventId, GitRef, LineRange, Ref, TaskId};
use proptest::prelude::*;

#[test]
fn parses_examples() {
    let r: Ref = "git:abc1234:src/lib.rs#L10-40".parse().unwrap();
    let Ref::Git(g) = &r else { panic!("{r:?}") };
    assert_eq!(g.sha(), "abc1234");
    assert_eq!(g.path(), "src/lib.rs");
    assert_eq!(g.lines(), Some(LineRange::new(10, 40).unwrap()));
    assert_eq!(r.to_string(), "git:abc1234:src/lib.rs#L10-40");

    let r: Ref = "git:abc1234:C:/x.rs#L7".parse().unwrap();
    assert_eq!(r.to_string(), "git:abc1234:C:/x.rs#L7");

    let r: Ref = "git:abc1234:README.md".parse().unwrap();
    assert_eq!(r.to_string(), "git:abc1234:README.md");

    let r: Ref = "task:t-12".parse().unwrap();
    assert_eq!(r, Ref::Task(TaskId::new("t-12").unwrap()));

    let hex = "ab".repeat(32);
    let r: Ref = format!("event:{hex}").parse().unwrap();
    assert_eq!(r, Ref::Event(EventId::from_bytes([0xab; 32])));
    assert_eq!(r.to_string(), format!("event:{hex}"));
}

#[test]
fn rejects_bad_refs() {
    for bad in [
        "",
        "git:",
        "git:abc:x",          // sha too short
        "git:ABC1234:x",      // uppercase hex
        "git:abc1234:",       // empty path
        "git:abc1234:a#b",    // '#' in path
        "git:abc1234:a#L0",   // line 0
        "git:abc1234:a#L9-3", // reversed range
        "git:abc1234:a#L",    // missing number
        "git:abc1234:a\nb",   // control char
        "task:",
        "task:a b",
        "event:abcd",
        "event:zz",
        "bogus:1",
    ] {
        assert!(bad.parse::<Ref>().is_err(), "accepted {bad:?}");
    }
}

#[test]
fn serde_uses_string_form() {
    let r: Ref = "task:t-1".parse().unwrap();
    let bytes = postcard::to_allocvec(&r).unwrap();
    let back: Ref = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(back, r);
}

fn arb_ref() -> impl Strategy<Value = Ref> {
    let git = (
        "[0-9a-f]{7,64}",
        "[A-Za-z0-9_./:-]{1,40}",
        proptest::option::of((1u32..10_000, 0u32..10_000)),
    )
        .prop_map(|(sha, path, lines)| {
            let lines = lines.map(|(s, len)| LineRange::new(s, s.saturating_add(len)).unwrap());
            Ref::Git(GitRef::new(sha, path, lines).unwrap())
        });
    let task = "[A-Za-z0-9_.-]{1,32}".prop_map(|s| Ref::Task(TaskId::new(s).unwrap()));
    let event = any::<[u8; 32]>().prop_map(|b| Ref::Event(EventId::from_bytes(b)));
    prop_oneof![git, task, event]
}

proptest! {
    #[test]
    fn print_parse_roundtrip(r in arb_ref()) {
        let s = r.to_string();
        let back: Ref = s.parse().unwrap();
        prop_assert_eq!(back, r);
    }

    #[test]
    fn parse_never_panics(s in "\\PC{0,60}") {
        let _ = s.parse::<Ref>();
    }
}
