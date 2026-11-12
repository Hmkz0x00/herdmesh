# Installing herdmesh

This document covers system requirements, building and installing the `herdmesh` CLI from source, configuring the herdr plugin, and verifying the installation.

---

## 1. Prerequisites

Before installing `herdmesh`, ensure the following tools are available on your system:

- **Rust toolchain (1.85+)**: `rustup`, `cargo`, and `rustc`.
  - Install via [rustup.rs](https://rustup.rs):
    - **Linux / macOS:** `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
    - **Windows:** Download and run `rustup-init.exe`.
- **Git**: Required for repository checkout and version metadata.
- **C Compiler / Linker**:
  - **Linux:** `build-essential` (gcc/clang)
  - **macOS:** Xcode command line tools (`xcode-select --install`)
  - **Windows:** Visual Studio C++ Build Tools (MSVC)
- **(Optional) herdr (0.9.5+)**: Terminal multiplexer for coding agents ([herdr.dev](https://herdr.dev)).

---

## 2. Installation from Source

We do not use `curl | sh` pipes or require `sudo` privileges. Build and install directly from local source.

### Option A: Using the Automated Install Scripts

Clone the repository and run the install script corresponding to your platform:

#### Linux / macOS
```bash
git clone https://github.com/herdrdev/herdmesh.git
cd herdmesh
bash scripts/install.sh
```

#### Windows (PowerShell)
```powershell
git clone https://github.com/herdrdev/herdmesh.git
cd herdmesh
.\scripts\install.ps1
```

The script verifies that `rustup` and `cargo` are in `PATH` and runs `cargo install --path crates/herdmesh --locked`.

### Option B: Manual Cargo Install

You can install the binary directly from the repo root using cargo:

```bash
cargo install --path crates/herdmesh --locked
```

This compiles and places the `herdmesh` executable in your Cargo bin directory:
- **Linux / macOS:** `~/.cargo/bin/herdmesh`
- **Windows:** `%USERPROFILE%\.cargo\bin\herdmesh.exe`

Ensure that `~/.cargo/bin` (or `%USERPROFILE%\.cargo\bin` on Windows) is in your `PATH` environment variable.

---

## 3. Verifying Installation

Verify that `herdmesh` is properly installed and that the environment is healthy by running `doctor`:

```bash
herdmesh doctor
```

Sample output:
```text
=== herdmesh doctor ===
[OK] cargo binary: found (cargo 1.85.0)
[OK] herdr binary: found (herdr 0.9.5)
[OK] data directory: C:\Users\user\AppData\Local\herdmesh
[OK] key storage: secure permissions verified
All checks passed. Environment is ready.
```

---

## 4. Herdr Plugin Installation

If you use [herdr](https://herdr.dev) to manage multi-agent panes, install the `herdmesh` plugin:

### From GitHub Repository
```bash
herdr plugin install herdrdev/herdmesh
```

### Local Development Link
During development, link the local workspace directory directly to herdr:
```bash
herdr plugin link .
```

For more details on plugin manifests, hooks, and pane placement, see [Herdr Plugin System Research](research/herdr-plugin.md).

---

## 5. Next Steps

- **Configure your agents:** Connect Claude Code, Codex, or Antigravity via MCP by following [Agent Setup Guide](agents-setup.md).
- **Run cross-network checks:** Follow the [Cross-Network Testing Guide](testing/cross-network.md) to test peer-to-peer connectivity across NATs.
- **Deploy a custom relay:** For restricted networks or corporate environments, see the [Self-Hosted Relay Guide](ops/self-hosted-relay.md).
