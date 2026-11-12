#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct ChildGuard(Option<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct McpClient {
    _guard: ChildGuard,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl McpClient {
    fn spawn(bin: &str, data_dir: &std::path::Path, user: &str, agent: &str) -> Self {
        let mut cmd = Command::new(bin);
        cmd.args([
            "--data-dir",
            data_dir.to_str().unwrap(),
            "mcp",
            "--agent",
            agent,
        ])
        .env("USERNAME", user)
        .env("USER", user)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
        let mut child = cmd.spawn().expect("failed to spawn herdmesh mcp");
        let stdin = child.stdin.take().expect("failed to take stdin");
        let stdout = BufReader::new(child.stdout.take().expect("failed to take stdout"));
        Self {
            _guard: ChildGuard(Some(child)),
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn send(&mut self, req: &Value) {
        let s = serde_json::to_string(req).unwrap();
        writeln!(self.stdin, "{s}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn read_message(&mut self) -> Value {
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .expect("failed to read line from mcp stdout");
        assert!(
            !line.trim().is_empty(),
            "unexpected EOF or empty line from mcp server"
        );
        serde_json::from_str(&line).expect("failed to parse json-rpc message")
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send(&req);
        loop {
            let resp = self.read_message();
            if resp.get("id").and_then(Value::as_u64) == Some(id) {
                return resp;
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        let notif = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.send(&notif);
    }

    fn call_tool(&mut self, name: &str, args: Value) -> String {
        let resp = self.request(
            "tools/call",
            json!({
                "name": name,
                "arguments": args,
            }),
        );
        let content = resp["result"]["content"]
            .as_array()
            .expect("expected content array");
        assert!(!content.is_empty(), "content array is empty");
        content[0]["text"]
            .as_str()
            .expect("expected text block")
            .to_owned()
    }
}

#[test]
fn smoke_mcp_end_to_end() {
    let bin = env!("CARGO_BIN_EXE_herdmesh");
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let test_user = format!("smoke-{}", std::process::id());

    // 1. Create room 'alpha' with agent 'a'
    let status = Command::new(bin)
        .args([
            "--data-dir",
            data_dir.to_str().unwrap(),
            "room",
            "create",
            "alpha",
            "--as",
            "a",
        ])
        .env("USERNAME", &test_user)
        .env("USER", &test_user)
        .status()
        .expect("failed to run room create");
    assert!(status.success(), "room create failed");

    // 2. Start daemon as a child process
    let mut daemon_cmd = Command::new(bin);
    daemon_cmd
        .args(["--data-dir", data_dir.to_str().unwrap(), "daemon"])
        .env("USERNAME", &test_user)
        .env("USER", &test_user)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    let daemon_child = daemon_cmd.spawn().expect("failed to spawn daemon");
    let _daemon_guard = ChildGuard(Some(daemon_child));

    // Allow daemon a moment to bind and load rooms
    std::thread::sleep(Duration::from_millis(600));

    // 3. Spawn 'herdmesh mcp --agent a'
    let mut mcp_a = McpClient::spawn(bin, &data_dir, &test_user, "a");

    // Initialize MCP session for 'a'
    let init_a = mcp_a.request(
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "smoke-a", "version": "0.1.0" }
        }),
    );
    assert!(init_a["result"]["capabilities"]["tools"].is_object());
    mcp_a.notify("notifications/initialized", json!({}));

    // List tools and assert all 14 tools
    let list_resp = mcp_a.request("tools/list", json!({}));
    let tools = list_resp["result"]["tools"]
        .as_array()
        .expect("tools array");
    let mut tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    tool_names.sort();

    let mut expected = vec![
        "room_card",
        "brief",
        "inbox",
        "ask",
        "answer",
        "handoff",
        "claim",
        "release",
        "task_add",
        "task_update",
        "task_list",
        "decide",
        "get_event",
        "search",
    ];
    expected.sort();
    assert_eq!(tool_names, expected, "expected 14 MCP tools");

    // 'a' calls room_card
    let card = mcp_a.call_tool("room_card", json!({}));
    assert!(
        card.contains("alpha"),
        "room_card should mention room alpha: {card}"
    );

    // 'a' asks 'b'
    let _ask = mcp_a.call_tool(
        "ask",
        json!({
            "to": "b",
            "question": "smoke test question from a to b"
        }),
    );

    // 4. Spawn 'herdmesh mcp --agent b'
    let mut mcp_b = McpClient::spawn(bin, &data_dir, &test_user, "b");

    // Initialize MCP session for 'b'
    let init_b = mcp_b.request(
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "smoke-b", "version": "0.1.0" }
        }),
    );
    assert!(init_b["result"]["capabilities"]["tools"].is_object());
    mcp_b.notify("notifications/initialized", json!({}));

    // 'b' checks inbox: should see the ask from 'a' wrapped in <<peer-untrusted>>
    let inbox = mcp_b.call_tool("inbox", json!({}));
    assert!(
        inbox.contains("<<peer-untrusted"),
        "inbox must be wrapped untrusted: {inbox}"
    );
    assert!(
        inbox.contains("smoke test question from a to b"),
        "inbox missing question: {inbox}"
    );
    assert!(
        inbox.contains("<</peer-untrusted>>"),
        "inbox missing closing tag: {inbox}"
    );

    // 5. 'a' claims 'src/x/**'
    let claim_a = mcp_a.call_tool(
        "claim",
        json!({
            "paths": ["src/x/**"],
            "ttl_min": 10
        }),
    );
    assert!(
        claim_a.contains("pending") || claim_a.contains("granted"),
        "claim_a should be pending or granted: {claim_a}"
    );

    // 6. 'b' claims overlapping path 'src/x/y.rs' -> lost/pending
    let claim_b = mcp_b.call_tool(
        "claim",
        json!({
            "paths": ["src/x/y.rs"],
            "ttl_min": 10
        }),
    );
    assert!(
        claim_b.contains("lost") || claim_b.contains("pending"),
        "claim_b should be lost or pending: {claim_b}"
    );
}
