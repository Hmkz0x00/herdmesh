#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
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
    stdout: Receiver<String>,
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
        let stdout = output_lines(child.stdout.take().expect("failed to take stdout"));
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
        let line = self
            .stdout
            .recv_timeout(Duration::from_secs(20))
            .expect("timed out waiting for MCP response or server exited");
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
        assert!(resp.get("error").is_none(), "JSON-RPC error: {resp}");
        assert_ne!(
            resp["result"]["isError"], true,
            "tool {name} failed: {resp}"
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

// Drain stdout on a separate thread so a broken child cannot hang the test.
fn output_lines(stdout: std::process::ChildStdout) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

fn command(data_dir: &std::path::Path, user: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_herdmesh"));
    cmd.arg("--data-dir")
        .arg(data_dir)
        .env("USERNAME", user)
        .env("USER", user);
    cmd
}

fn initialize(client: &mut McpClient) {
    let init = client.request(
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "view-e2e", "version": "0.1.0"}
        }),
    );
    assert!(
        init["result"]["capabilities"]["tools"].is_object(),
        "{init}"
    );
    client.notify("notifications/initialized", json!({}));
}

fn event_id(reply: &str) -> &str {
    let id = reply
        .split_once("id=")
        .expect("tool response includes event id")
        .1
        .split(|c: char| !c.is_ascii_hexdigit())
        .next()
        .unwrap();
    assert_eq!(id.len(), 64, "{reply}");
    id
}

fn http(authority: &str, method: &str, path: &str, host: &str, status: u16) -> String {
    let addr = authority.parse().expect("loopback socket address");
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").expect("HTTP response");
    assert!(
        headers.starts_with(&format!("HTTP/1.1 {status} ")),
        "{response}"
    );
    let headers = headers.to_ascii_lowercase();
    assert!(
        headers.contains("\r\ncontent-security-policy: default-src 'none';"),
        "{headers}"
    );
    body.to_owned()
}

#[test]
fn live_view_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let user = format!(
        "view-e2e-{}-{}",
        std::process::id(),
        tmp.path().file_name().unwrap().to_string_lossy()
    );
    let room = "view-e2e";
    let created = command(&data_dir, &user)
        .args(["room", "create", room, "--as", "a"])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let daemon = command(&data_dir, &user)
        .arg("daemon")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let _daemon = ChildGuard(Some(daemon));
    // Match smoke_mcp's daemon startup allowance; requests also have bounded waits.
    std::thread::sleep(Duration::from_millis(600));

    let bin = env!("CARGO_BIN_EXE_herdmesh");
    let mut a = McpClient::spawn(bin, &data_dir, &user, "a");
    let mut b = McpClient::spawn(bin, &data_dir, &user, "b");
    initialize(&mut a);
    initialize(&mut b);
    assert!(a.call_tool("room_card", json!({})).contains(room));
    assert!(b.call_tool("room_card", json!({})).contains(room));
    let asked = a.call_tool("ask", json!({"to": "b", "question": "what OS?"}));
    let ask_id = event_id(&asked);
    let answered = b.call_tool("answer", json!({"ask_id": ask_id, "text": "Windows"}));
    assert!(answered.contains("answered id="), "{answered}");
    let answer_id = event_id(&answered);
    let claim_a = a.call_tool("claim", json!({"paths": ["src/x/**"], "ttl_min": 10}));
    assert!(
        claim_a.contains("pending") || claim_a.contains("granted"),
        "{claim_a}"
    );
    let claim_b = b.call_tool("claim", json!({"paths": ["src/x/y.rs"], "ttl_min": 10}));
    assert!(
        claim_b.contains("lost") || claim_b.contains("pending"),
        "{claim_b}"
    );

    let mut view = command(&data_dir, &user)
        .args(["view", "--room", room, "--no-open"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let lines = output_lines(view.stdout.take().unwrap());
    let _view = ChildGuard(Some(view));
    let line = lines
        .recv_timeout(Duration::from_secs(20))
        .expect("view URL or server exited");
    let url = line
        .strip_prefix("herdmesh view: http://")
        .expect("printed view URL");
    let (authority, token_path) = url.split_once('/').expect("URL token path");
    assert!(authority.starts_with("127.0.0.1:"), "{url}");
    let base = format!("/{token_path}");
    let state: Value = serde_json::from_str(&http(
        authority,
        "GET",
        &format!("{base}api/state"),
        authority,
        200,
    ))
    .unwrap();
    assert_eq!(state["room"], room);
    let members = state["members"].as_array().unwrap();
    assert_eq!(members.len(), 2, "{state}");
    let member_id = |name: &str| {
        members.iter().find(|m| m["name"] == name).unwrap()["id"]
            .as_str()
            .unwrap()
    };
    let a_id = member_id("a");
    let b_id = member_id("b");
    let events = state["events"].as_array().unwrap();
    let ask = events
        .iter()
        .find(|e| e["id"] == ask_id)
        .expect("ask in feed");
    assert_eq!(ask["kind"], "ask");
    assert_eq!(ask["from"], a_id);
    assert_eq!(ask["to"], b_id);
    assert_eq!(ask["text"], "what OS?");
    // Match the exact answer returned when b answered a's ask via MCP.
    // The feed currently redacts answer refs and has no direct answer recipient.
    let answer = events
        .iter()
        .find(|e| e["id"] == answer_id)
        .unwrap_or_else(|| panic!("MCP answer missing from feed: {state}"));
    assert_eq!(answer["kind"], "answer");
    assert_eq!(answer["from"], b_id);
    assert_eq!(answer["text"], "Windows");
    assert!(
        state["leases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["holder"] == a_id
                && l["paths"] == json!(["src/x/**"])
                && matches!(l["state"].as_str(), Some("pending" | "granted"))),
        "{state}"
    );
    let detail: Value = serde_json::from_str(&http(
        authority,
        "GET",
        &format!("{base}api/event?id={ask_id}"),
        authority,
        200,
    ))
    .unwrap();
    assert_eq!(detail["id"], ask_id);
    assert_eq!(detail["kind"], "ask");
    assert_eq!(detail["from"], a_id);
    assert_eq!(detail["to"], b_id);
    assert_eq!(detail["text"], "what OS?");
    http(authority, "GET", "/wrong-token/api/state", authority, 404);
    http(
        authority,
        "GET",
        &format!("{base}api/state"),
        "evil.example",
        403,
    );
    http(
        authority,
        "POST",
        &format!("{base}api/state"),
        authority,
        405,
    );

    let log = command(&data_dir, &user)
        .args(["room", "log", room])
        .output()
        .unwrap();
    assert!(
        log.status.success(),
        "{}",
        String::from_utf8_lossy(&log.stderr)
    );
    let log = String::from_utf8(log.stdout).unwrap();
    assert!(
        log.lines()
            .any(|l| l.contains("[ask]") && l.contains("what OS?")),
        "{log}"
    );
    assert!(
        log.lines()
            .any(|l| l.contains("[answer]") && l.contains("Windows")),
        "{log}"
    );
}
