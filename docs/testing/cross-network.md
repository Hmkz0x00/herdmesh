# Cross-Network Manual Testing Guide

Step-by-step procedure to test herdmesh peer connectivity across different networks (e.g. one host behind home NAT, another on cellular hotspot / CGNAT).

## 1. Prerequisites & Installation

On both machines:
- Rust toolchain (`cargo`, `rustc` 1.85+)
- Clone repo and build herdmesh binary:
  ```bash
  cargo build --release -p herdmesh
  # executable is at target/release/herdmesh
  ```

---

## 2. Test Procedure & CLI Commands

> **Note:** Room management CLI commands below are **planned CLI, M2-4** (currently stubbed in `crates/herdmesh/src/main.rs`).

### Step 1: Host A (Home Wi-Fi / NAT) — Create Room
```bash
RUST_LOG=iroh=debug,hm_net=debug ./target/release/herdmesh room create alpha-room
```
Output yields a shareable room ticket:
```text
Created room 'alpha-room' (id: 4f8b9...)
Ticket: hmroom1a2b3c4...
Listening on iroh node: 7k9m2...
```

### Step 2: Host B (Cellular Hotspot / CGNAT) — Join Room
```bash
RUST_LOG=iroh=debug,hm_net=debug ./target/release/herdmesh room join hmroom1a2b3c4...
```

### Step 3: Verify Room Status
```bash
./target/release/herdmesh room status
```
Shows connected peers, latency, and transport mode (`direct` vs `relay`).

---

## 3. Confirming Direct vs Relay Connection

### Programmatic Verification (iroh 1.4.1 API)
In iroh 1.4.1, path observability is queried via [`Connection::paths`](https://docs.rs/iroh/1.4.1/iroh/endpoint/struct.Connection.html#method.paths) or [`Connection::paths_stream`](https://docs.rs/iroh/1.4.1/iroh/endpoint/struct.Connection.html#method.paths_stream):

```rust
// Inspect current active path
for path in conn.paths().iter().filter(|p| p.is_selected()) {
    if path.is_ip() {
        println!("Direct IP path: {} (RTT: {:?})", path.remote_addr(), path.rtt());
    } else if path.is_relay() {
        println!("Relayed path via: {} (RTT: {:?})", path.remote_addr(), path.rtt());
    }
}
```
Relevant API types:
- [`Connection::paths() -> PathList`](https://docs.rs/iroh/1.4.1/iroh/endpoint/struct.Connection.html#method.paths): snapshot of open paths.
- [`Path::is_ip() -> bool`](https://docs.rs/iroh/1.4.1/iroh/endpoint/struct.Path.html#method.is_ip): true for hole-punched or direct socket paths.
- [`Path::is_relay() -> bool`](https://docs.rs/iroh/1.4.1/iroh/endpoint/struct.Path.html#method.is_relay): true if traffic traverses relay.
- [`Path::remote_addr() -> &TransportAddr`](https://docs.rs/iroh/1.4.1/iroh/endpoint/struct.Path.html#method.remote_addr): yields `TransportAddr::Ip(SocketAddr)` or `TransportAddr::Relay(RelayUrl)`.
- [`Connection::path_events() -> PathEventStream`](https://docs.rs/iroh/1.4.1/iroh/endpoint/struct.Connection.html#method.path_events): emits [`PathEvent::Selected`](https://docs.rs/iroh/1.4.1/iroh/endpoint/enum.PathEvent.html#variant.Selected).

### Tracing Log Inspection
When running with `RUST_LOG=iroh=debug`:
1. **Initial Relay Connection:**
   `iroh::_events::path::opened` & `iroh::_events::path::selected` with `remote_addr = Relay("https://...")`.
2. **Holepunch Initiation:**
   `debug!("connections are not good enough, triggering holepunching")`
3. **Direct Path Upgrade:**
   `iroh::_events::path::opened` with `remote_addr = Ip(198.51.100.23:51820)`
   followed by `iroh::_events::path::selected` with `remote_addr = Ip(...)`.
   *(Traffic has successfully migrated off relay to direct UDP).*

---

## 4. Test Results Template

Record execution results across network configurations:

| Run ID | Host A Network | Host B Network | Initial Path | Final Path (after ~5s) | Direct RTT | Status |
|---|---|---|---|---|---|---|
| #1 | LAN (192.168.1.x) | LAN (192.168.1.y) | Direct IP | Direct IP | < 5 ms | PASS |
| #2 | Home Wi-Fi (NAT) | Phone Hotspot (CGNAT) | Relay | Direct IP (Hole-punched) | ~45 ms | PASS |
| #3 | Office Wi-Fi (Symmetric NAT) | Cellular (CGNAT) | Relay | Relay (Fallback) | ~85 ms | PASS |
| #4 | Strict UDP Firewall | Phone Hotspot | Relay | Relay | ~90 ms | PASS |
