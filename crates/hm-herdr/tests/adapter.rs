#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use hm_herdr::{
    FakeHerdrCli, Herdr, HerdrCli, HerdrError, HerdrStatus, parse_agent_list_json,
    resolve_herdr_bin, resolve_herdr_bin_with, strip_control_chars,
};
use hm_proto::AgentStatus;
use hm_proto::budget::{PING_MAX, estimate_tokens};

const FIXTURE_JSON: &str = include_str!("fixtures/agent_list.json");

#[test]
fn parse_fixture() {
    let agents = parse_agent_list_json(FIXTURE_JSON).expect("fixture should parse successfully");
    assert_eq!(agents.len(), 8);

    // Unnamed idle agent
    let a0 = &agents[0];
    assert_eq!(a0.pane_id, "w2:p1");
    assert_eq!(a0.name, None);
    assert_eq!(a0.kind, "claude");
    assert_eq!(a0.status, HerdrStatus::Idle);
    assert_eq!(a0.cwd, PathBuf::from("/workspace/repo"));

    // Named idle agent
    let a2 = &agents[2];
    assert_eq!(a2.pane_id, "w2:p6");
    assert_eq!(a2.name.as_deref(), Some("agy"));
    assert_eq!(a2.kind, "agy");
    assert_eq!(a2.status, HerdrStatus::Idle);
    assert_eq!(a2.cwd, PathBuf::from("/workspace/other-project"));

    // Done agent
    let a3 = &agents[3];
    assert_eq!(a3.pane_id, "w3:p1");
    assert_eq!(a3.name, None);
    assert_eq!(a3.kind, "claude");
    assert_eq!(a3.status, HerdrStatus::Done);
    assert_eq!(a3.cwd, PathBuf::from("/workspace/task-archive"));

    // Working agent targeted by name
    let a5 = &agents[5];
    assert_eq!(a5.pane_id, "w3:p2");
    assert_eq!(a5.name.as_deref(), Some("hm-agy"));
    assert_eq!(a5.kind, "agy");
    assert_eq!(a5.status, HerdrStatus::Working);
    assert_eq!(a5.cwd, PathBuf::from("/workspace/hm-wt-agy"));

    // Working agent targeted by pane
    let a6 = &agents[6];
    assert_eq!(a6.pane_id, "w3:p3");
    assert_eq!(a6.name.as_deref(), Some("hm-impl1"));
    assert_eq!(a6.kind, "claude");
    assert_eq!(a6.status, HerdrStatus::Working);

    // Idle agent
    let a7 = &agents[7];
    assert_eq!(a7.pane_id, "w3:p4");
    assert_eq!(a7.name.as_deref(), Some("hm-impl2"));
    assert_eq!(a7.kind, "claude");
    assert_eq!(a7.status, HerdrStatus::Idle);
    assert_eq!(a7.cwd, PathBuf::from("/workspace/hm-wt-impl2"));
}

#[test]
fn notify_refused_while_working_or_blocked() {
    let fake = FakeHerdrCli::new(FIXTURE_JSON);
    let herdr = Herdr::new(fake);

    // 1. Refuse while Working (targeted by name)
    let err = herdr.notify("hm-agy", "ping").unwrap_err();
    match err {
        HerdrError::AgentNotReady { target, status } => {
            assert_eq!(target, "hm-agy");
            assert_eq!(status, HerdrStatus::Working);
        }
        other => panic!("expected AgentNotReady, got {other:?}"),
    }

    // 2. Refuse while Working (targeted by pane_id)
    let err = herdr.notify("w3:p3", "ping").unwrap_err();
    match err {
        HerdrError::AgentNotReady { target, status } => {
            assert_eq!(target, "w3:p3");
            assert_eq!(status, HerdrStatus::Working);
        }
        other => panic!("expected AgentNotReady, got {other:?}"),
    }

    // 3. Refuse while Blocked
    let blocked_json =
        FIXTURE_JSON.replace(r#""agent_status": "idle""#, r#""agent_status": "blocked""#);
    herdr.cli().set_list_json(blocked_json);

    let err = herdr.notify("hm-impl2", "ping").unwrap_err();
    match err {
        HerdrError::AgentNotReady { target, status } => {
            assert_eq!(target, "hm-impl2");
            assert_eq!(status, HerdrStatus::Blocked);
        }
        other => panic!("expected AgentNotReady, got {other:?}"),
    }

    // Ensure prompt was NEVER called when refused
    for call in herdr.cli().calls() {
        assert_ne!(call.first().map(String::as_str), Some("agent_prompt"));
        if call.len() >= 2 {
            assert_ne!((call[0].as_str(), call[1].as_str()), ("agent", "prompt"));
        }
    }
}

#[test]
fn notify_allowed_while_idle_or_done() {
    let fake = FakeHerdrCli::new(FIXTURE_JSON);
    let herdr = Herdr::new(fake);

    // Idle agent (by name)
    herdr
        .notify("hm-impl2", "ping idle")
        .expect("idle agent should accept notification");

    // Done agent (by pane_id)
    herdr
        .notify("w3:p1", "ping done")
        .expect("done agent should accept notification");

    // Both succeeded and sent prompt commands
    let calls = herdr.cli().calls();
    let prompt_calls: Vec<_> = calls
        .into_iter()
        .filter(|c| c.len() >= 2 && c[0] == "agent" && c[1] == "prompt")
        .collect();
    assert_eq!(prompt_calls.len(), 2);
    assert_eq!(
        prompt_calls[0],
        vec!["agent", "prompt", "hm-impl2", "ping idle"]
    );
    assert_eq!(
        prompt_calls[1],
        vec!["agent", "prompt", "w3:p1", "ping done"]
    );
}

#[test]
fn oversized_ping_refused() {
    let fake = FakeHerdrCli::new(FIXTURE_JSON);
    let herdr = Herdr::new(fake);

    // Construct a message strictly exceeding PING_MAX tokens
    let mut big_msg = String::from("task update: ");
    while estimate_tokens(&big_msg) <= PING_MAX {
        big_msg.push_str("very long notification sentence with details; ");
    }
    let token_count = estimate_tokens(&big_msg);
    assert!(token_count > PING_MAX);

    let err = herdr.notify("hm-impl2", &big_msg).unwrap_err();
    match err {
        HerdrError::OversizedPing { tokens, max } => {
            assert_eq!(tokens, token_count);
            assert_eq!(max, PING_MAX);
        }
        other => panic!("expected OversizedPing, got {other:?}"),
    }

    // Verify prompt was never sent
    assert!(herdr.cli().calls().is_empty());
}

#[test]
fn control_chars_stripped() {
    let input = "hello\r\n\x00\x1b[31mworld\t!";
    let stripped = strip_control_chars(input);
    assert_eq!(stripped, "hello[31mworld!");

    let fake = FakeHerdrCli::new(FIXTURE_JSON);
    let herdr = Herdr::new(fake);

    herdr
        .notify("hm-impl2", input)
        .expect("notify should succeed after stripping");

    let last = herdr.cli().last_call().expect("should have recorded call");
    assert_eq!(last, vec!["agent", "prompt", "hm-impl2", "hello[31mworld!"]);
}

#[test]
fn command_args_exact() {
    let fake = FakeHerdrCli::new(FIXTURE_JSON);
    let herdr = Herdr::new(fake);

    herdr
        .notify("hm-impl2", "M1-2 done @ 8ca4588")
        .expect("notify should succeed");

    let calls = herdr.cli().calls();
    assert_eq!(calls.len(), 2);

    // Step 1: list agents
    assert_eq!(calls[0], vec!["agent", "list"]);

    // Step 2: prompt agent without --wait
    assert_eq!(
        calls[1],
        vec!["agent", "prompt", "hm-impl2", "M1-2 done @ 8ca4588"]
    );

    // Explicitly assert --wait is not in args
    for call in &calls {
        assert!(
            !call.iter().any(|arg| arg == "--wait"),
            "command must NOT contain --wait"
        );
    }
}

#[test]
fn agent_not_found() {
    let fake = FakeHerdrCli::new(FIXTURE_JSON);
    let herdr = Herdr::new(fake);

    let err = herdr.notify("unknown-agent", "ping").unwrap_err();
    match err {
        HerdrError::AgentNotFound(target) => assert_eq!(target, "unknown-agent"),
        other => panic!("expected AgentNotFound, got {other:?}"),
    }
}

#[test]
fn resolve_bin_path_prefers_env_var() {
    let custom_bin = PathBuf::from("/custom/bin/my-herdr.exe");
    let resolved = resolve_herdr_bin_with(
        Some(custom_bin.as_os_str().to_os_string()),
        Some("C:\\Windows;C:\\Tools".into()),
    );
    assert_eq!(resolved, custom_bin);

    // Fallback when env var is None
    let fallback = resolve_herdr_bin_with(None, None);
    assert_eq!(fallback, PathBuf::from(hm_herdr::HERDR_EXE));

    // Can run standard resolve_herdr_bin() without panic
    let _ = resolve_herdr_bin();
}

#[test]
fn status_mapping_and_display() {
    assert_eq!(HerdrStatus::from("idle"), HerdrStatus::Idle);
    assert_eq!(HerdrStatus::from("IDLE"), HerdrStatus::Idle);
    assert_eq!(HerdrStatus::from("working"), HerdrStatus::Working);
    assert_eq!(HerdrStatus::from("blocked"), HerdrStatus::Blocked);
    assert_eq!(HerdrStatus::from("done"), HerdrStatus::Done);
    assert_eq!(HerdrStatus::from("unknown"), HerdrStatus::Unknown);
    assert_eq!(HerdrStatus::from("something_else"), HerdrStatus::Unknown);

    assert_eq!(HerdrStatus::Idle.to_string(), "idle");
    assert_eq!(HerdrStatus::Working.to_string(), "working");
    assert_eq!(HerdrStatus::Blocked.to_string(), "blocked");
    assert_eq!(HerdrStatus::Done.to_string(), "done");
    assert_eq!(HerdrStatus::Unknown.to_string(), "unknown");

    // Interop with hm_proto::AgentStatus
    let proto_idle: AgentStatus = HerdrStatus::Idle.try_into().expect("convert idle");
    assert_eq!(proto_idle, AgentStatus::Idle);
    let back: HerdrStatus = proto_idle.into();
    assert_eq!(back, HerdrStatus::Idle);

    assert!(AgentStatus::try_from(HerdrStatus::Unknown).is_err());
}

#[test]
fn notify_via_cli_trait_directly() {
    let fake = FakeHerdrCli::new(FIXTURE_JSON);
    // Directly call trait method on fake without Herdr struct wrapper
    fake.notify("hm-impl2", "ping via trait")
        .expect("notify should work on &impl HerdrCli");
    let last = fake.last_call().expect("last call");
    assert_eq!(last, vec!["agent", "prompt", "hm-impl2", "ping via trait"]);
}

#[test]
fn notify_reports_command_failure() {
    let fake = FakeHerdrCli::new(FIXTURE_JSON);
    fake.set_prompt_error("agent prompt timed out");

    let err = fake.notify("hm-impl2", "ping").unwrap_err();
    match err {
        HerdrError::CommandFailed { status, stderr } => {
            assert_eq!(status, Some(1));
            assert_eq!(stderr, "agent prompt timed out");
        }
        other => panic!("expected CommandFailed, got {other:?}"),
    }
}

#[test]
fn real_herdr_agent_list_if_available() {
    let system = hm_herdr::SystemHerdrCli::from_env();
    if system.bin_path().is_file() {
        match system.list_agents() {
            Ok(agents) => {
                assert!(
                    !agents.is_empty(),
                    "real herdr should return running agents"
                );
            }
            Err(e) => {
                eprintln!("real herdr command returned error: {e}");
            }
        }
    }
}
