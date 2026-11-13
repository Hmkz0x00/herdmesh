# Cross-Network Testing Guide

Step-by-step procedure to test peer connectivity and agent coordination across different networks (e.g., home NAT to cellular hotspot / CGNAT).

## 1. Prerequisites & Installation

On both machines:
- **Rust Toolchain**: `rustup`, `cargo`, `rustc` (1.85+).
- **Git** and C++ Linker: On Windows, install **Visual Studio Build Tools** with the "Desktop development with C++" workload.
- **herdr** (optional): Terminal multiplexer for coding agents ([herdr.dev](https://herdr.dev)).
- Clone and install from `https://github.com/Hmkz0x00/herdmesh`:
  ```bash
  # Windows (PowerShell)
  git clone https://github.com/Hmkz0x00/herdmesh.git && cd herdmesh && .\scripts\install.ps1
  # Linux / macOS
  git clone https://github.com/Hmkz0x00/herdmesh.git && cd herdmesh && bash scripts/install.sh
  ```

---

## 2. Phase 1: Network Connectivity

### Step 1: Host Machine (Home Wi-Fi / NAT)
Create room and generate an invite ticket (ticket is a **SECRET** containing room key + node address; send privately):
```bash
herdmesh room create alpha-room
herdmesh room invite alpha-room
```

### Step 2: Peer Machine (Cellular Hotspot / CGNAT)
Join using the secret invite ticket, naming the local room and specifying agent alias:
```bash
herdmesh room join "<ticket>" --as bob --name alpha-room
```

### Step 3: Run Foreground Network Sync
Both peers run the network loop with tracing enabled to inspect path selection:
```bash
# Linux / macOS
RUST_LOG=iroh=info,hm_net=debug herdmesh room run alpha-room
# Windows (PowerShell)
$env:RUST_LOG="iroh=info,hm_net=debug"; herdmesh room run alpha-room
```
> **Windows Firewall Note:** On initial UDP listen, Windows Firewall will prompt to allow network access. Select **Private networks** and click *Allow access*.

### Observing Direct vs. Relay Connections in Logs
- **Relayed Connection (initial):** Log emits `path::opened` and `path::selected` with `remote_addr = Relay("https://...")`.
- **Direct IP Connection (upgrade):** iroh holepunches NAT; log emits `path::opened` followed by `path::selected` with `remote_addr = Ip(<ip>:<port>)`.

---

## 3. Phase 2: Multi-Agent Collaboration

Once network connectivity is established, stop foreground `room run` (`Ctrl+C`) on both nodes and switch to daemon + MCP coordination:

1. **Ensure Background Daemon Running**:
   ```bash
   herdmesh daemon --ensure
   ```
2. **Configure Agent MCP Clients**:
   Set up Claude Code, Codex, or Antigravity per [docs/agents-setup.md](../agents-setup.md) (`command: "herdmesh"`, `args: ["mcp", "--agent", "<agent>"]`).
3. **Agent Interaction Test**:
   - **Ask & Inbox**: Agent A calls `ask(to="bob", question="Status check?")`. Peer Bob runs `inbox()`, receiving the question tagged inside `<<peer-untrusted>>` wrappers.
   - **Claim Conflict**: Agent A calls `claim(paths=["src/core/**"])` (granted). Peer Bob calls `claim(paths=["src/core/mod.rs"])` (reports conflict/lost due to A's active lease).
4. **Live Board Inspection**:
   Run `herdmesh board --as <agent>` to inspect real-time presence, leases, and event sync.

---

## 4. Test Results Template

| Run ID | Host Network | Peer Network | Transport (Relay / Direct) | RTT | Agent Sync (Ask / Claim) | Status |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| #1 | LAN (192.168.1.x) | LAN (192.168.1.y) | Direct IP | < 5 ms | Ask delivered, Claim conflicted | PASS |
| #2 | Home Wi-Fi (NAT) | Phone Hotspot (CGNAT) | Direct IP (Hole-punched) | ~45 ms | Ask delivered, Claim conflicted | PASS |
| #3 | Strict Firewall | Cellular (Symmetric NAT) | Relay Fallback | ~90 ms | Ask delivered, Claim conflicted | PASS |
