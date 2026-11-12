# Self-Hosted iroh-relay Operations Guide

Guide for deploying and operating a self-hosted `iroh-relay` (v1.4.1) for herdmesh clusters and pointing clients to custom relays.

## 1. Installation

Install the standalone relay binary from crates.io:
```bash
cargo install iroh-relay --version 1.4.1 --locked
```
Verified against [iroh-relay docs.rs](https://docs.rs/iroh-relay/1.4.1).

---

## 2. Running the Relay

### A. Development Mode (Plain HTTP)
For local testing or closed LANs without TLS:
```bash
iroh-relay --dev
```
- Listens on `[::]:3340` over plain HTTP.
- Bypasses TLS requirements.
- Client relay URL: `http://localhost:3340` (or `http://<lan-ip>:3340`).

### B. Production Mode (TLS with Automated ACME / Let's Encrypt)
Run with a configuration file:
```bash
iroh-relay --config /etc/iroh-relay/relay.toml
```

Example configuration (`/etc/iroh-relay/relay.toml`):
```toml
http_bind_addr = "0.0.0.0:80"
enable_quic_addr_discovery = true

[tls]
https_bind_addr = "0.0.0.0:443"
quic_bind_addr = "0.0.0.0:3340"
hostname = ["relay.example.com"]
cert_mode = "LetsEncrypt" # or "Manual"
cert_dir = "/var/lib/iroh-relay/certs"
prod_tls = true
contact = "ops@example.com"

[access]
shared_token = ["your-secure-bearer-token"] # or "everyone"
```

---

## 3. Required Ports & Firewall

Configure ingress firewall rules on the relay host:

| Port | Protocol | Purpose | Exposure |
|---|---|---|---|
| **80** | TCP | HTTP captive portal & ACME challenge validation | Public |
| **443** | TCP | HTTPS relay traffic (HTTP/1.1 transport upgrade) | Public |
| **3340** | UDP | QUIC address discovery (QAD/STUN holepunch assist) | Public |
| **3340** | TCP | Development mode only (`--dev`) | LAN / Localhost |
| **9090** | TCP | Prometheus metrics (`metrics_bind_addr`) | Internal only |

---

## 4. Client Configuration in herdmesh

To point herdmesh nodes to a self-hosted relay:

### Option A: Configuration File (Planned, M2-4)
In `~/.herdmesh/config.toml`:
```toml
# Custom relay URL for iroh endpoint
relay_url = "https://relay.example.com"
# For local dev: relay_url = "http://127.0.0.1:3340"
```

### Option B: CLI Flag (Planned, M2-4)
```bash
herdmesh room create my-room --relay-url https://relay.example.com
```

### Internal Implementation
In `crates/hm-net/src/net.rs`, `relay_url` maps to `RelayChoice::Custom(url)`:
```rust
Endpoint::builder(presets::N0)
    .relay_mode(RelayMode::custom([url]))
```

---

## 5. Security & Access Control Notes

- **End-to-End Encryption:** iroh connections are end-to-end QUIC encrypted using node ed25519 keys (`EndpointId`). The relay server routes encrypted frames by destination `EndpointId` only and cannot decrypt message content.
- **Relay Access Control:** Restrict relay usage using `[access]` config:
  - `shared_token = ["..."]`: Requires clients to present `Authorization: Bearer <token>` or `?token=`. (Can be set via `IROH_RELAY_ACCESS_TOKEN` env var).
  - `allowlist = ["<endpoint-id>", ...]`: Admits only specified Node/Endpoint IDs.
- **Metrics Hardening:** Never expose port 9090 publicly; bind to loopback (`metrics_bind_addr = "127.0.0.1:9090"`).
