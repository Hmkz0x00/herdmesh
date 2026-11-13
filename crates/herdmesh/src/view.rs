//! `herdmesh view`: a local, read-only web page of the room (ADR-003).
//!
//! A foreground HTTP/1.1 server on `127.0.0.1:<random port>`, hand-written
//! (GET only, one request per connection) so no web framework is pulled in.
//! Rules, all mandatory:
//! - every URL starts with a random 128-bit token segment, else 404;
//! - `Host` must be `127.0.0.1:<port>` or `localhost:<port>`, else 403
//!   (DNS rebinding); checked before anything else;
//! - GET only, else 405; no CORS headers;
//! - every response carries the CSP, `nosniff` and `no-store` headers;
//! - assets are embedded; room state comes from the daemon's read-only
//!   `Feed` IPC call and every string in it passes `sanitize_display` again.

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::time::Duration;

use hm_daemon::ipc::{Call, Client, Request, Response, Token, current_user, socket_name};
use hm_proto::RoomId;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::timeout;

use crate::board::TOKEN_FILE;
use crate::data::DataDir;
use crate::display::sanitize_display;
use crate::room::DEFAULT_AGENT;

const INDEX_HTML: &str = include_str!("../assets/view/index.html");
const VIEW_CSS: &str = include_str!("../assets/view/view.css");
const VIEW_JS: &str = include_str!("../assets/view/view.js");

/// Longest request head accepted.
const HEAD_MAX: usize = 16 * 1024;
/// Whole-request read deadline (slow-loris guard).
const READ_DEADLINE: Duration = Duration::from_secs(5);
/// Connections served at once.
const MAX_CONNS: usize = 32;
/// Events per poll (the daemon caps at 200 too).
const FEED_LIMIT: u32 = 200;
/// Cap for every string served (names, paths, event text).
const STRING_MAX: usize = 200;

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
                   connect-src 'self'; base-uri 'none'; frame-ancestors 'none'";

/// Why the daemon could not give a state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateError;

/// Source of the room-state JSON for a `since` cursor.
pub type StateFn =
    Arc<dyn Fn(u64) -> Pin<Box<dyn Future<Output = Result<String, StateError>> + Send>> + Send + Sync>;

/// A 128-bit random URL token, lowercase hex.
pub fn random_token() -> std::io::Result<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// What to do with one request.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Static(&'static str, &'static str),
    State { since: u64 },
    Deny(u16),
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Decide on a request head (no body). Order: Host (403), method (405),
/// token and path (404).
fn decide(head: &str, token: &str, port: u16) -> Action {
    let mut lines = head.split("\r\n");
    let Some(line) = lines.next() else {
        return Action::Deny(400);
    };
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Action::Deny(400);
    };
    if !version.starts_with("HTTP/1.") || !target.starts_with('/') {
        return Action::Deny(400);
    }
    let mut hosts = Vec::new();
    for h in lines {
        if let Some((k, v)) = h.split_once(':')
            && k.trim().eq_ignore_ascii_case("host")
        {
            hosts.push(v.trim());
        }
    }
    let host_ok = match hosts.as_slice() {
        [h] => {
            let h = h.to_ascii_lowercase();
            h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}")
        }
        _ => false,
    };
    if !host_ok {
        return Action::Deny(403);
    }
    if method != "GET" {
        return Action::Deny(405);
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let Some((tok, rest)) = path[1..].split_once('/') else {
        return Action::Deny(404);
    };
    if !ct_eq(tok, token) {
        return Action::Deny(404);
    }
    match rest {
        "" | "index.html" => Action::Static("text/html; charset=utf-8", INDEX_HTML),
        "view.css" => Action::Static("text/css; charset=utf-8", VIEW_CSS),
        "view.js" => Action::Static("text/javascript; charset=utf-8", VIEW_JS),
        "api/state" => {
            let since = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("since="))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            Action::State { since }
        }
        _ => Action::Deny(404),
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Bad Gateway",
    }
}

/// One complete response; the three security headers are on every one.
fn render(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let allow = if status == 405 { "Allow: GET\r\n" } else { "" };
    let mut out = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Content-Security-Policy: {CSP}\r\nX-Content-Type-Options: nosniff\r\n\
         Cache-Control: no-store\r\n{allow}Connection: close\r\n\r\n",
        reason(status),
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// Terminal-safe, length-capped copy of every string in `v` (defence in
/// depth on top of the daemon's own redaction and truncation).
fn sanitize_json(v: &mut Value) {
    match v {
        Value::String(s) => *s = sanitize_display(s, STRING_MAX),
        Value::Array(a) => a.iter_mut().for_each(sanitize_json),
        Value::Object(o) => o.values_mut().for_each(sanitize_json),
        _ => {}
    }
}

fn sanitized_state(json: &str) -> Option<Vec<u8>> {
    let mut v: Value = serde_json::from_str(json).ok()?;
    sanitize_json(&mut v);
    serde_json::to_vec(&v).ok()
}

async fn read_head(conn: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            buf.truncate(i);
            return String::from_utf8(buf).ok();
        }
        if buf.len() > HEAD_MAX {
            return None;
        }
        let n = conn.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

async fn handle(mut conn: TcpStream, token: &str, port: u16, state: &StateFn) {
    let reply = match timeout(READ_DEADLINE, read_head(&mut conn)).await {
        Ok(Some(head)) => match decide(&head, token, port) {
            Action::Static(ct, body) => render(200, ct, body.as_bytes()),
            Action::State { since } => match state(since).await {
                Ok(json) => match sanitized_state(&json) {
                    Some(body) => render(200, "application/json; charset=utf-8", &body),
                    None => render(502, "text/plain; charset=utf-8", b"bad state"),
                },
                Err(StateError) => render(502, "text/plain; charset=utf-8", b"daemon unavailable"),
            },
            Action::Deny(code) => render(code, "text/plain; charset=utf-8", reason(code).as_bytes()),
        },
        Ok(None) | Err(_) => render(400, "text/plain; charset=utf-8", b"Bad Request"),
    };
    // The peer may already be gone; nothing useful to do about it.
    let _ = conn.write_all(&reply).await;
    let _ = conn.shutdown().await;
}

/// Bind `127.0.0.1` on a random port.
pub async fn bind() -> std::io::Result<(TcpListener, u16)> {
    let l = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
    let port = l.local_addr()?.port();
    Ok((l, port))
}

/// Serve until `shutdown` completes.
pub async fn serve(
    listener: TcpListener,
    token: String,
    state: StateFn,
    shutdown: impl Future<Output = ()>,
) {
    let port = listener.local_addr().map_or(0, |a| a.port());
    let token: Arc<str> = Arc::from(token);
    let slots = Arc::new(Semaphore::new(MAX_CONNS));
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => return,
            acc = listener.accept() => {
                let Ok((conn, _)) = acc else { continue };
                let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else {
                    continue; // over capacity: drop the connection
                };
                let (token, state) = (Arc::clone(&token), Arc::clone(&state));
                tokio::spawn(async move {
                    handle(conn, &token, port, &state).await;
                    drop(permit);
                });
            }
        }
    }
}

/// Map a daemon answer to the state JSON.
pub fn state_from_response(resp: Result<Response, impl Sized>) -> Result<String, StateError> {
    match resp {
        Ok(Response::Ok(mut items)) if items.len() == 1 => {
            items.pop().map(|(_, json)| json).ok_or(StateError)
        }
        _ => Err(StateError),
    }
}

/// State source backed by the daemon over local IPC.
pub fn daemon_state(client: Client, room: Option<RoomId>) -> StateFn {
    let client = Arc::new(client);
    Arc::new(move |since| {
        let client = Arc::clone(&client);
        Box::pin(async move {
            let req = Request {
                agent: DEFAULT_AGENT.to_owned(),
                room,
                call: Call::Feed {
                    since,
                    limit: FEED_LIMIT,
                },
            };
            state_from_response(client.request(&req).await)
        })
    })
}

fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    let mut cmd = {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut c = Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url])
            .creation_flags(CREATE_NO_WINDOW);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

fn read_token(path: &PathBuf) -> Result<Token, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("no daemon token ({e})"))?;
    let arr: [u8; hm_daemon::ipc::TOKEN_LEN] = bytes
        .try_into()
        .map_err(|_| "daemon token has the wrong size".to_owned())?;
    Ok(Token::from_bytes(arr))
}

async fn run_async(data: &DataDir, room: Option<&str>, open: bool) -> Result<(), String> {
    let room = room
        .map(|r| data.resolve_room(r).map(|(id, _)| id))
        .transpose()
        .map_err(|e| e.to_string())?;
    let token = read_token(&data.root().join(TOKEN_FILE))
        .map_err(|e| format!("{e}; is the daemon running?"))?;
    let client = Client::new(socket_name(&current_user()), token);
    let (listener, port) = bind().await.map_err(|e| format!("bind: {e}"))?;
    let url_token = random_token().map_err(|e| format!("random token: {e}"))?;
    let url = format!("http://127.0.0.1:{port}/{url_token}/");
    println!("herdmesh view: {url}");
    println!("read-only, local only; Ctrl+C to stop");
    if open && let Err(e) = open_browser(&url) {
        eprintln!("herdmesh view: could not open a browser ({e}); open the URL above");
    }
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    serve(listener, url_token, daemon_state(client, room), ctrl_c).await;
    Ok(())
}

/// Entry point for `herdmesh view`.
pub fn run(data: Option<DataDir>, room: Option<&str>, no_open: bool) -> ExitCode {
    let Some(data) = data else {
        eprintln!("herdmesh view: no data dir; pass --data-dir");
        return ExitCode::FAILURE;
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("herdmesh view: tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run_async(&data, room, !no_open)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("herdmesh view: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ed25519_dalek::SigningKey;
    use hm_daemon::{Daemon, NullNet};
    use hm_proto::{Event, EventBody, NodeId};
    use hm_store::Store;
    use serde_json::json;

    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn daemon() -> Arc<Daemon> {
        let d = Arc::new(Daemon::new(
            SigningKey::from_bytes(&[7; 32]),
            Arc::new(NullNet),
        ));
        d.create_room(
            "alpha",
            Store::open_in_memory(RoomId::from_bytes([1; 32])).unwrap(),
        )
        .unwrap();
        d
    }

    fn feed_req(since: u64) -> Request {
        Request {
            agent: DEFAULT_AGENT.into(),
            room: None,
            call: Call::Feed {
                since,
                limit: FEED_LIMIT,
            },
        }
    }

    /// State source straight from a daemon (no IPC).
    fn state_of(d: Arc<Daemon>) -> StateFn {
        Arc::new(move |since| {
            let r = d.handle_request(feed_req(since));
            Box::pin(async move { state_from_response(Ok::<_, ()>(r)) })
        })
    }

    struct Reply {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl Reply {
        fn header(&self, k: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        }
    }

    async fn raw(port: u16, request: &str) -> Reply {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(request.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8(buf).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let headers = lines
            .map(|l| {
                let (k, v) = l.split_once(": ").unwrap();
                (k.to_owned(), v.to_owned())
            })
            .collect();
        Reply {
            status,
            headers,
            body: body.to_owned(),
        }
    }

    async fn get(port: u16, path: &str) -> Reply {
        raw(
            port,
            &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        )
        .await
    }

    async fn spawn(state: StateFn) -> (u16, tokio::sync::oneshot::Sender<()>) {
        let (l, port) = bind().await.unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(serve(l, TOKEN.to_owned(), state, async {
            let _ = rx.await;
        }));
        (port, tx)
    }

    fn assert_secure_headers(r: &Reply) {
        assert_eq!(r.header("content-security-policy"), Some(CSP));
        assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
        assert_eq!(r.header("cache-control"), Some("no-store"));
        assert!(r.header("access-control-allow-origin").is_none());
    }

    #[tokio::test]
    async fn wrong_or_missing_token_is_404() {
        let (port, _stop) = spawn(state_of(daemon())).await;
        for path in [
            "/",
            "/index.html",
            "/wrong/",
            "/wrong/api/state",
            "/0123456789abcdef0123456789abcdee/api/state",
            &format!("/{TOKEN}x/api/state"),
            &format!("/{}/api/state", &TOKEN[..31]),
            &format!("/{TOKEN}"),
            &format!("/{TOKEN}/nope"),
            &format!("/{TOKEN}/../x"),
        ] {
            let r = get(port, path).await;
            assert_eq!(r.status, 404, "{path}");
            assert_secure_headers(&r);
        }
    }

    #[tokio::test]
    async fn bad_host_is_403_even_with_the_right_token() {
        let (port, _stop) = spawn(state_of(daemon())).await;
        for host in [
            "evil.example",
            "evil.example:80",
            &format!("evil.example:{port}"),
            &format!("127.0.0.1:{}", port.wrapping_add(1)),
            "127.0.0.1",
            &format!("[::1]:{port}"),
        ] {
            let r = raw(
                port,
                &format!("GET /{TOKEN}/api/state HTTP/1.1\r\nHost: {host}\r\n\r\n"),
            )
            .await;
            assert_eq!(r.status, 403, "{host}");
            assert_secure_headers(&r);
        }
        // no Host at all
        let r = raw(port, &format!("GET /{TOKEN}/ HTTP/1.1\r\n\r\n")).await;
        assert_eq!(r.status, 403);
        // two Host headers
        let r = raw(
            port,
            &format!(
                "GET /{TOKEN}/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nHost: evil.example\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(r.status, 403);
        // localhost is fine, any case
        let r = raw(
            port,
            &format!("GET /{TOKEN}/ HTTP/1.1\r\nHost: LocalHost:{port}\r\n\r\n"),
        )
        .await;
        assert_eq!(r.status, 200);
    }

    #[tokio::test]
    async fn only_get_is_allowed() {
        let (port, _stop) = spawn(state_of(daemon())).await;
        for m in ["POST", "PUT", "DELETE", "HEAD", "OPTIONS", "PATCH"] {
            let r = raw(
                port,
                &format!(
                    "{m} /{TOKEN}/api/state HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
                     Content-Length: 0\r\n\r\n"
                ),
            )
            .await;
            assert_eq!(r.status, 405, "{m}");
            assert_eq!(r.header("allow"), Some("GET"));
            assert_secure_headers(&r);
        }
    }

    #[tokio::test]
    async fn garbage_is_400_with_headers() {
        let (port, _stop) = spawn(state_of(daemon())).await;
        let r = raw(port, "nonsense\r\n\r\n").await;
        assert_eq!(r.status, 400);
        assert_secure_headers(&r);
    }

    #[tokio::test]
    async fn serves_the_embedded_assets_with_the_headers() {
        let (port, _stop) = spawn(state_of(daemon())).await;
        for (p, ct, body) in [
            ("", "text/html; charset=utf-8", INDEX_HTML),
            ("index.html", "text/html; charset=utf-8", INDEX_HTML),
            ("view.css", "text/css; charset=utf-8", VIEW_CSS),
            ("view.js", "text/javascript; charset=utf-8", VIEW_JS),
        ] {
            let r = get(port, &format!("/{TOKEN}/{p}")).await;
            assert_eq!(r.status, 200, "{p}");
            assert_eq!(r.header("content-type"), Some(ct));
            assert_eq!(r.body, body);
            assert_secure_headers(&r);
        }
    }

    #[tokio::test]
    async fn state_json_matches_the_adr_contract() {
        let d = daemon();
        d.handle_request(Request {
            agent: "alice".into(),
            room: None,
            call: Call::Claim {
                paths: vec!["src/net/**".into()],
                ttl_min: 10,
            },
        });
        let (port, _stop) = spawn(state_of(d)).await;
        let r = get(port, &format!("/{TOKEN}/api/state?since=0")).await;
        assert_eq!(r.status, 200);
        assert_eq!(
            r.header("content-type"),
            Some("application/json; charset=utf-8")
        );
        assert_secure_headers(&r);
        let v: Value = serde_json::from_str(&r.body).unwrap();
        let keys = |x: &Value| {
            let mut k: Vec<_> = x.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        assert_eq!(
            keys(&v),
            [
                "cursor", "events", "leases", "members", "now_ms", "room", "tasks", "you"
            ]
        );
        assert_eq!(
            keys(&v["members"][0]),
            [
                "activity", "id", "last_ms", "local", "name", "node", "present", "role", "status"
            ]
        );
        assert_eq!(
            keys(&v["leases"][0]),
            ["expires_ms", "holder", "id", "paths", "state"]
        );
        assert_eq!(
            keys(&v["events"][0]),
            ["from", "id", "kind", "lamport", "ref", "text", "to", "ts_ms"]
        );
        assert_eq!(v["room"], "alpha");
        assert_eq!(v["members"][0]["role"], "owner");
        assert_eq!(v["leases"][0]["paths"], json!(["src/net/**"]));
        // cursor round trip: nothing after it
        let cur = v["cursor"].as_u64().unwrap();
        let r = get(port, &format!("/{TOKEN}/api/state?since={cur}")).await;
        let v2: Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v2["events"], json!([]));
        // junk since is treated as 0, not an error
        let r = get(port, &format!("/{TOKEN}/api/state?since=abc")).await;
        assert_eq!(r.status, 200);
    }

    #[tokio::test]
    async fn peer_script_text_stays_a_short_plain_json_string() {
        let d = daemon();
        d.handle_request(Request {
            agent: "alice".into(),
            room: None,
            call: Call::RoomCard,
        });
        let room = d.room_ids()[0];
        let peer = SigningKey::from_bytes(&[9; 32]);
        let store = d.store_for_test(room);
        let (seq, lam) = {
            let s = store.lock().unwrap();
            let n = NodeId::from_verifying_key(&peer.verifying_key());
            (s.next_seq(&n).unwrap(), s.next_lamport().unwrap())
        };
        let evil = format!(
            "<script>alert(1)</script></script>\u{1b}[2J\u{202e}\"}}]{}",
            "y".repeat(400)
        );
        let ev = Event::sign(
            &peer,
            room,
            "mallory",
            seq,
            lam,
            1,
            EventBody::Note {
                scope: "s".into(),
                text: evil,
            },
        )
        .unwrap();
        store.lock().unwrap().append(&ev).unwrap();
        let (port, _stop) = spawn(state_of(d)).await;
        let r = get(port, &format!("/{TOKEN}/api/state")).await;
        assert_eq!(r.status, 200);
        let v: Value = serde_json::from_str(&r.body).unwrap();
        let t = v["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["kind"] == "note")
            .unwrap()["text"]
            .as_str()
            .unwrap();
        assert!(t.starts_with("<script>alert(1)</script>"), "{t}");
        assert!(t.chars().count() <= 200, "{}", t.chars().count());
        assert!(!r.body.contains('\u{1b}') && !r.body.contains('\u{202e}'));
    }

    #[tokio::test]
    async fn daemon_down_is_502_with_headers() {
        let down: StateFn = Arc::new(|_| Box::pin(async { Err(StateError) }));
        let (port, _stop) = spawn(down).await;
        let r = get(port, &format!("/{TOKEN}/api/state")).await;
        assert_eq!(r.status, 502);
        assert_secure_headers(&r);
    }

    #[test]
    fn tokens_are_random_128_bit_hex() {
        let (a, b) = (random_token().unwrap(), random_token().unwrap());
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn assets_follow_the_no_inline_rule() {
        // CSP forbids inline script/style and external requests, so the
        // page must not rely on them.
        let html = INDEX_HTML.to_ascii_lowercase();
        assert!(!html.contains("<script>") && !html.contains("style="));
        for a in [INDEX_HTML, VIEW_CSS, VIEW_JS] {
            assert!(!a.contains("https://") && !a.contains("http://"), "external");
        }
    }
}
