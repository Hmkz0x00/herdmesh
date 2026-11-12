# herdmesh installer (build from local source)
[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"

Write-Host "== herdmesh installer" -ForegroundColor Cyan

# Check for rustup
if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) {
    Write-Error "'rustup' was not found in PATH. Please install Rust using rustup from https://rustup.rs and retry."
    exit 1
}

# Check for cargo
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "'cargo' was not found in PATH. Please install Rust and cargo via https://rustup.rs and retry."
    exit 1
}

# Locate repository root and target crate
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Resolve-Path (Join-Path $ScriptDir "..")
$CratePath = Join-Path $RepoRoot "crates\herdmesh"

if (-not (Test-Path $CratePath)) {
    Write-Error "Directory '$CratePath' not found."
    exit 1
}

Write-Host "Building and installing herdmesh CLI from source..." -ForegroundColor Cyan
cargo install --path "$CratePath" --locked
if ($LASTEXITCODE -ne 0) {
    Write-Error "cargo install failed with exit code $LASTEXITCODE"
    exit $LASTEXITCODE
}

Write-Host ""
Write-Host "✔ herdmesh installed successfully!" -ForegroundColor Green
Write-Host ""
Write-Host "Next steps:"
Write-Host "  1. Verify CLI environment:   herdmesh doctor"
Write-Host "  2. Configure agent MCP:       see docs/agents-setup.md"
Write-Host "  3. Create or join a room:     herdmesh room create <room-name>"
Write-Host "  4. (Optional) herdr plugin:   herdr plugin install herdmesh"
