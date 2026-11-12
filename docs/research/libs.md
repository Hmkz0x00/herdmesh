# Crate API Research & Verification for herdmesh

Verified on `x86_64-pc-windows-msvc` (Rust 1.99.1) against crates.io / docs.rs.

## 1. Compatibility & Windows MSVC Support

- **iroh & iroh-gossip compatibility:** **Compatible**. `iroh-gossip 0.102.0` declares `iroh = "1.4.1"` and `iroh-base = "1.4.1"` in its dependencies. Types (`Endpoint`, `EndpointId`, `Router`) unify seamlessly across both crates.
- **Windows MSVC compilation:** **Verified**. `iroh 1.4.1` compiles and links cleanly on `x86_64-pc-windows-msvc`.

---

## 2. iroh

- **Version:** `1.4.1`
- **Docs:** <https://docs.rs/iroh/1.4.1>
- **Key Types & Functions:**
  - Builder: `iroh::Endpoint::builder(presets::N0)` -> `iroh::endpoint::Builder`
  - Builder methods: `.secret_key(SecretKey)`, `.alpns(Vec<Vec<u8>>)`, `.relay_mode(RelayMode)`, `.bind().await`
  - Identity / Addr: `iroh::EndpointId` (type alias for `iroh_base::PublicKey`, previously `NodeId`), `iroh::EndpointAddr` (contains `.id: EndpointId`, previously `NodeAddr`). `ep.id() -> EndpointId`, `ep.addr() -> EndpointAddr`. Key generation: `SecretKey::generate()`.
  - Connect / Accept: `ep.connect(addr: EndpointAddr, alpn: &[u8]).await` -> `Connection`; `ep.accept().await` -> `Option<Incoming>`, `incoming.await` -> `Connection`. Stream methods: `conn.open_uni()`, `conn.open_bi()`, `conn.accept_uni()`, `conn.accept_bi()`.
  - Relay Mode: `iroh::RelayMode` (`Disabled`, `Default`, `Staging`, `Custom(RelayMap)`). `RelayMode::custom([RelayUrl])`.
  - Self-hosting Relay: Standalone CLI binary via `cargo install iroh-relay` (`iroh-relay --dev` for HTTP:3340; `--config` for production TLS/ACME); or library API `iroh_relay::server::Server::spawn(ServerConfig)` via `iroh-relay = { version = "1.4.1", features = ["server"] }`.

```rust
use iroh::{endpoint::presets, Endpoint, EndpointAddr, RelayMode, SecretKey};
async fn iroh_example(peer: EndpointAddr) -> Result<(), Box<dyn std::error::Error>> {
    let secret = SecretKey::generate();
    let ep = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .alpns(vec![b"hm/0.1".to_vec()])
        .relay_mode(RelayMode::Default)
        .bind().await?;
    let _my_addr: EndpointAddr = ep.addr();
    let conn = ep.connect(peer, b"hm/0.1").await?;
    let mut send = conn.open_uni().await?;
    send.write_all(b"ping").await?;
    Ok(())
}
```

---

## 3. iroh-gossip

- **Version:** `0.102.0`
- **Docs:** <https://docs.rs/iroh-gossip/0.102.0>
- **Key Types & Functions:**
  - Setup with Router: `iroh_gossip::net::Gossip::builder().spawn(ep.clone())`, `iroh::protocol::Router::builder(ep).accept(iroh_gossip::ALPN, gossip.clone()).spawn()`
  - Subscription: `TopicId::from_bytes([u8; 32])`, `gossip.subscribe(topic, bootstrap_peers: Vec<EndpointId>).await` -> `GossipTopic`
  - Channels: `topic.split()` -> `(GossipSender, GossipReceiver)`. Wait for peer mesh: `receiver.joined().await`.
  - Broadcast: `sender.broadcast(bytes: Bytes).await`
  - Receive: Stream of `Result<iroh_gossip::api::Event, ApiError>` via `futures_util::StreamExt::next()`. Events: `Event::Received(Message)` with `msg.content` and `msg.delivered_from`, `Event::NeighborUp(EndpointId)`, `Event::NeighborDown(EndpointId)`.

```rust
use futures_util::StreamExt;
use iroh::{endpoint::presets, protocol::Router, Endpoint, EndpointId};
use iroh_gossip::{api::Event, Gossip, TopicId, ALPN};
async fn gossip_ex(peers: Vec<EndpointId>) -> Result<(), Box<dyn std::error::Error>> {
    let ep = Endpoint::bind(presets::N0).await?;
    let gossip = Gossip::builder().spawn(ep.clone());
    let _r = Router::builder(ep).accept(ALPN, gossip.clone()).spawn();
    let (tx, mut rx) = gossip.subscribe(TopicId::from_bytes([0u8; 32]), peers).await?.split();
    tx.broadcast(b"msg".to_vec().into()).await?;
    if let Some(Ok(Event::Received(m))) = rx.next().await {
        println!("got: {:?}", m.content);
    }
    Ok(())
}
```

---

## 4. rmcp

- **Version:** `3.9.0`
- **Docs:** <https://docs.rs/rmcp/3.9.0>
- **Required Cargo Features:** `features = ["transport-io"]` (default features `server` and `macros` provide `ServerHandler`, `ServiceExt`, macros, and `schemars`; `transport-io` provides `rmcp::transport::stdio`).
- **Key Types & Functions:**
  - Traits & Router: `rmcp::ServerHandler`, `rmcp::ServiceExt`, `rmcp::handler::server::router::tool::ToolRouter<Self>`, `rmcp::handler::server::wrapper::Parameters<T>`
  - Macros: `#[tool_router(router = <field>)]`, `#[tool(name = "...", description = "...")]`, `#[tool_handler(router = self.<field>)]`
  - Transport & Run: `rmcp::transport::stdio()`, `server.serve(transport).await` -> `RunningService`, `running_service.waiting().await`

```rust
use rmcp::{handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    tool, tool_handler, tool_router, ServerHandler, ServiceExt};
#[derive(Clone)]
struct Srv { tool_router: ToolRouter<Self> }
#[tool_router(router = tool_router)]
impl Srv {
    #[tool(name = "ping", description = "health check")]
    async fn ping(&self) -> String { "pong".into() }
}
#[tool_handler(router = self.tool_router)]
impl ServerHandler for Srv {}
async fn run_mcp() -> Result<(), Box<dyn std::error::Error>> {
    let s = Srv { tool_router: Srv::tool_router() };
    s.serve(rmcp::transport::stdio()).await?.waiting().await?;
    Ok(())
}
```

---

## 5. interprocess

- **Version:** `2.4.8`
- **Docs:** <https://docs.rs/interprocess/2.4.8>
- **Required Cargo Features:** `features = ["tokio"]`
- **Key Types & Functions:**
  - Listener: `interprocess::local_socket::ListenerOptions::new().name(name).create_tokio()`
  - Stream: `interprocess::local_socket::tokio::Stream::connect(name).await`
  - Prelude / Traits: `interprocess::local_socket::tokio::prelude::*` (imports `Listener::accept`, `Stream`, `ToFsName`, `ToNsName`)
  - Cross-Platform Naming:
    - `GenericNamespaced`: Maps to Windows Named Pipes (`\\.\pipe\<name>`) on Windows, and Abstract Domain Sockets on Linux.
    - Fallback: On macOS/BSD where `GenericNamespaced::is_supported()` is false, use filesystem paths via `ToFsName` with `GenericFilePath` (Unix Domain Socket, e.g. `/tmp/<name>.sock`).

```rust
use interprocess::local_socket::{tokio::{prelude::*, Stream},
    GenericFilePath, GenericNamespaced, ListenerOptions, ToFsName, ToNsName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
async fn ipc_ex() -> Result<(), Box<dyn std::error::Error>> {
    let nm = if GenericNamespaced::is_supported() { "hm.sock".to_ns_name::<GenericNamespaced>()? }
             else { "/tmp/hm.sock".to_fs_name::<GenericFilePath>()? };
    let l = ListenerOptions::new().name(nm.clone()).create_tokio()?;
    tokio::spawn(async move { Stream::connect(nm).await.unwrap().write_all(b"hi").await.unwrap(); });
    let mut conn = l.accept().await?;
    let mut buf = [0u8; 2];
    conn.read_exact(&mut buf).await?;
    Ok(())
}
```

---

## 6. tiktoken-rs

- **Version:** `0.12.1`
- **Docs:** <https://docs.rs/tiktoken-rs/0.12.1>
- **Key Types & Functions:**
  - Tokenizer Singleton: `tiktoken_rs::o200k_base_singleton() -> &'static CoreBPE` (or fresh instance via `tiktoken_rs::o200k_base() -> Result<CoreBPE, ...>`)
  - Count Tokens: `bpe.encode_ordinary(text: &str) -> Vec<Rank>`, token count is `.len()`
  - Special Tokens: `bpe.encode_with_special_tokens(text: &str)`
  - Chat Token Counting: `tiktoken_rs::num_tokens_from_messages("gpt-4o", &messages)` (resolves internally to `Tokenizer::O200kBase`)

```rust
use tiktoken_rs::o200k_base_singleton;
fn count_tokens(text: &str) -> usize {
    let bpe = o200k_base_singleton();
    bpe.encode_ordinary(text).len()
}
#[test]
fn test_count() {
    assert_eq!(count_tokens("hello world"), 2);
}
```
