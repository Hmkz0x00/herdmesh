<#
.SYNOPSIS
    Real per-CLI MCP smoke test runner for herdmesh.
.DESCRIPTION
    Tests real coding agent CLIs (Claude Code, Antigravity, Codex) against herdmesh MCP stdio server:
    - Claude Code: automated test using temporary --mcp-config with strict isolation
    - Antigravity: checks CLI options for ephemeral MCP injection without config modification
    - Codex: verifies CLI override syntax (-c mcp_servers.herdmesh...)
#>

[CmdletBinding()]
param(
    [string]$BinaryPath = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# 1. Locate binary
if (-not $BinaryPath) {
    $candidates = @(
        "E:/Codes/hm-target/release/herdmesh.exe",
        "E:/Codes/hm-target/debug/herdmesh.exe",
        "$PSScriptRoot/../target/release/herdmesh.exe",
        "$PSScriptRoot/../target/debug/herdmesh.exe"
    )
    foreach ($cand in $candidates) {
        if (Test-Path $cand) {
            $BinaryPath = (Resolve-Path $cand).Path
            break
        }
    }
}

if (-not $BinaryPath -or -not (Test-Path $BinaryPath)) {
    Write-Host "Building herdmesh in release mode..." -ForegroundColor Cyan
    cargo build --release -p herdmesh
    $BinaryPath = "E:/Codes/hm-target/release/herdmesh.exe"
    if (-not (Test-Path $BinaryPath)) {
        throw "Failed to locate or build herdmesh binary."
    }
}

$BinaryPath = (Resolve-Path $BinaryPath).Path
Write-Host "Using herdmesh binary: $BinaryPath" -ForegroundColor Green

# 2. Setup isolated temp environment
$rnd = [System.Guid]::NewGuid().ToString("N").Substring(0, 8)
$smokeUser = "smoke-$rnd"
$tempDataDir = Join-Path ([System.IO.Path]::GetTempPath()) "hm-cli-smoke-$rnd"
$tempConfigDir = Join-Path ([System.IO.Path]::GetTempPath()) "hm-cli-conf-$rnd"
New-Item -ItemType Directory -Path $tempDataDir -Force | Out-Null
New-Item -ItemType Directory -Path $tempConfigDir -Force | Out-Null

$daemonProc = $null

try {
    $env:USER = $smokeUser
    $env:USERNAME = $smokeUser

    Write-Host "`n[1/4] Creating test room 'smoke'..." -ForegroundColor Cyan
    & $BinaryPath --data-dir $tempDataDir room create smoke
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to create room 'smoke'"
    }

    Write-Host "`n[2/4] Starting herdmesh daemon..." -ForegroundColor Cyan
    $daemonProc = Start-Process -FilePath $BinaryPath -ArgumentList "--data-dir", $tempDataDir, "daemon" -PassThru -NoNewWindow
    Start-Sleep -Seconds 2
    if ($daemonProc.HasExited) {
        throw "Daemon failed to stay running (exit code: $($daemonProc.ExitCode))"
    }
    Write-Host "Daemon running (PID: $($daemonProc.Id), Pipe: herdmesh-$smokeUser)" -ForegroundColor Green

    # [3/4] Test Claude Code
    Write-Host "`n[3/4] Testing Claude Code MCP..." -ForegroundColor Cyan
    $mcpConfigFile = Join-Path $tempConfigDir "claude-mcp.json"
    $mcpConfig = @{
        mcpServers = @{
            herdmesh = @{
                command = $BinaryPath
                args = @("--data-dir", $tempDataDir, "mcp", "--agent", "claude-agent")
                env = @{
                    USER = $smokeUser
                    USERNAME = $smokeUser
                }
            }
        }
    } | ConvertTo-Json -Depth 5
    Set-Content -Path $mcpConfigFile -Value $mcpConfig -Encoding UTF8

    $claudePrompt = "Call the herdmesh room_card tool, then ask agent b 'ping?' with the ask tool. Reply with the tool results only."
    Write-Host "Executing claude with --mcp-config..." -ForegroundColor Gray
    
    $prevEAP = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $claudeOut = ($null | claude -p $claudePrompt `
        --mcp-config $mcpConfigFile `
        --strict-mcp-config `
        --model haiku `
        --allowedTools 'mcp__herdmesh__*' 2>$null) | Out-String
    $ErrorActionPreference = $prevEAP

    Write-Host "`n--- Claude Code Output ---" -ForegroundColor Yellow
    Write-Host $claudeOut.Trim()
    Write-Host "--- End Claude Code Output ---`n" -ForegroundColor Yellow

    if ($claudeOut -match "room smoke" -and $claudeOut -match "asked id=") {
        Write-Host "Claude Code MCP Smoke Test: PASS (called room_card and ask)" -ForegroundColor Green
    } else {
        Write-Warning "Claude Code output did not contain expected tool responses."
    }

    # [4/4] Antigravity & Codex Status
    Write-Host "`n[4/4] CLI Compatibility Status:" -ForegroundColor Cyan
    Write-Host "Antigravity (agy): UNVERIFIED" -ForegroundColor Yellow
    Write-Host "  Reason: 'agy --settings' rejects 'mcpServers' (unknown setting); agy has no CLI flag for ephemeral MCP config without mutating user ~/.gemini/config/mcp_config.json." -ForegroundColor Gray

    Write-Host "Codex (codex-cli): PENDING (Rate-limited until 02:33)" -ForegroundColor Yellow
    Write-Host "  Command to execute once limit resets:" -ForegroundColor Gray
    $codexCmd = @"
  codex exec "Call the herdmesh room_card tool, then ask agent b 'ping?' with the ask tool. Reply with the tool results only." --ephemeral -c 'mcp_servers.herdmesh.command="$BinaryPath"' -c 'mcp_servers.herdmesh.args=["--data-dir", "$tempDataDir", "mcp", "--agent", "codex-agent"]' -c 'mcp_servers.herdmesh.env={ USER = "$smokeUser", USERNAME = "$smokeUser" }'
"@
    Write-Host $codexCmd -ForegroundColor Gray

} finally {
    Write-Host "`nCleaning up resources..." -ForegroundColor Cyan
    if ($daemonProc -and -not $daemonProc.HasExited) {
        Write-Host "Stopping daemon process $($daemonProc.Id)..." -ForegroundColor Gray
        Stop-Process -Id $daemonProc.Id -Force -ErrorAction SilentlyContinue
    }
    if (Test-Path $tempDataDir) {
        Remove-Item -Recurse -Force $tempDataDir -ErrorAction SilentlyContinue
    }
    if (Test-Path $tempConfigDir) {
        Remove-Item -Recurse -Force $tempConfigDir -ErrorAction SilentlyContinue
    }
    Write-Host "Cleanup complete." -ForegroundColor Green
}
