# herdmesh end-to-end smoke test script
[CmdletBinding()]
param(
    [string]$HerdmeshBin
)

$ErrorActionPreference = "Stop"

Write-Host "=========================================" -ForegroundColor Cyan
Write-Host " herdmesh End-to-End Smoke Test (MCP)   " -ForegroundColor Cyan
Write-Host "=========================================" -ForegroundColor Cyan

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Resolve-Path (Join-Path $ScriptDir "..")

# 1. Locate binary
$Bin = $HerdmeshBin
if (-not $Bin) {
    $Candidates = @(
        (Join-Path $RepoRoot "target\debug\herdmesh.exe"),
        (Join-Path $RepoRoot "..\hm-target\debug\herdmesh.exe"),
        (Join-Path $RepoRoot "target\release\herdmesh.exe"),
        (Join-Path $RepoRoot "..\hm-target\release\herdmesh.exe")
    )
    foreach ($c in $Candidates) {
        if (Test-Path $c) {
            $Bin = (Resolve-Path $c).Path
            break
        }
    }
    if (-not $Bin) {
        $cmd = Get-Command herdmesh -ErrorAction SilentlyContinue
        if ($cmd) {
            $Bin = $cmd.Source
        }
    }
}

if (-not $Bin -or -not (Test-Path $Bin)) {
    Write-Error "Could not find herdmesh binary. Build with 'cargo build -p herdmesh --bin herdmesh' or pass -HerdmeshBin."
    exit 1
}

Write-Host "Using binary: $Bin"

# 2. Temporary directory
$DataDir = Join-Path ([System.IO.Path]::GetTempPath()) ("herdmesh-smoke-" + [System.Guid]::NewGuid().ToString().Substring(0, 8))
New-Item -ItemType Directory -Path $DataDir -Force | Out-Null
Write-Host "Using temporary data dir: $DataDir"

$TestUser = "smoke-" + $PID
$env:USERNAME = $TestUser
$env:USER = $TestUser

$DaemonProc = $null

try {
    # Step 1: Create room
    Write-Host "`n[Step 1/6] Creating room 'alpha' with agent 'a'..." -ForegroundColor Yellow
    & $Bin --data-dir $DataDir room create alpha --as a
    if ($LASTEXITCODE -ne 0) {
        throw "room create failed with exit code $LASTEXITCODE"
    }

    # Step 2: Start daemon
    Write-Host "`n[Step 2/6] Starting herdmesh daemon in background..." -ForegroundColor Yellow
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Bin
    $psi.Arguments = "--data-dir `"$DataDir`" daemon"
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.EnvironmentVariables["USERNAME"] = $TestUser
    $psi.EnvironmentVariables["USER"] = $TestUser
    $DaemonProc = [System.Diagnostics.Process]::Start($psi)
    Start-Sleep -Milliseconds 800

    if ($DaemonProc.HasExited) {
        $err = $DaemonProc.StandardError.ReadToEnd()
        throw "Daemon failed to start: $err"
    }
    Write-Host "Daemon running (PID $($DaemonProc.Id))"

    # Step 3: Run MCP session for agent 'a'
    Write-Host "`n[Step 3/6] Running MCP session for agent 'a'..." -ForegroundColor Yellow
    $mcpAInput = @(
        '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke-ps1","version":"0.1.0"}}}'
        '{"jsonrpc":"2.0","method":"notifications/initialized"}'
        '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
        '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"room_card","arguments":{}}}'
        '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"ask","arguments":{"to":"b","question":"Smoke test question from a to b"}}}'
        '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"claim","arguments":{"paths":["src/x/**"],"ttl_min":10}}}'
    ) -join "`n"

    $mcpAOutput = $mcpAInput | & $Bin --data-dir $DataDir mcp --agent a
    $mcpAJoined = $mcpAOutput -join "`n"

    if ($mcpAJoined -notmatch '"room_card"') {
        throw "tools/list did not contain room_card: $mcpAJoined"
    }
    Write-Host "  ✔ tools/list verified" -ForegroundColor Green

    if ($mcpAJoined -notmatch 'alpha') {
        throw "room_card output did not contain 'alpha': $mcpAJoined"
    }
    Write-Host "  ✔ room_card verified" -ForegroundColor Green

    if ($mcpAJoined -notmatch '"id":4') {
        throw "ask call failed: $mcpAJoined"
    }
    Write-Host "  ✔ ask sent to agent b" -ForegroundColor Green

    if ($mcpAJoined -notmatch '(pending|granted)') {
        throw "claim for src/x/** did not return pending/granted: $mcpAJoined"
    }
    Write-Host "  ✔ claim for src/x/** registered" -ForegroundColor Green

    # Step 4: Run MCP session for agent 'b'
    Write-Host "`n[Step 4/6] Running MCP session for agent 'b'..." -ForegroundColor Yellow
    $mcpBInput = @(
        '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke-ps1","version":"0.1.0"}}}'
        '{"jsonrpc":"2.0","method":"notifications/initialized"}'
        '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"inbox","arguments":{}}}'
        '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"claim","arguments":{"paths":["src/x/y.rs"],"ttl_min":10}}}'
    ) -join "`n"

    $mcpBOutput = $mcpBInput | & $Bin --data-dir $DataDir mcp --agent b
    $mcpBJoined = $mcpBOutput -join "`n"

    if ($mcpBJoined -notmatch '<<peer-untrusted') {
        throw "inbox did not wrap peer message with <<peer-untrusted>>: $mcpBJoined"
    }
    if ($mcpBJoined -notmatch 'Smoke test question from a to b') {
        throw "inbox did not receive ask from agent a: $mcpBJoined"
    }
    Write-Host "  ✔ inbox received peer ask wrapped as untrusted" -ForegroundColor Green

    if ($mcpBJoined -notmatch '(lost|pending)') {
        throw "overlapping claim for src/x/y.rs was not lost or pending: $mcpBJoined"
    }
    Write-Host "  ✔ overlapping claim correctly resolved to lost/pending" -ForegroundColor Green

    Write-Host "`n=========================================" -ForegroundColor Green
    Write-Host " ✔ All smoke test checks PASSED!         " -ForegroundColor Green
    Write-Host "=========================================" -ForegroundColor Green
}
finally {
    if ($DaemonProc -and -not $DaemonProc.HasExited) {
        Write-Host "Stopping daemon..."
        $DaemonProc.Kill()
        $DaemonProc.WaitForExit()
    }
    if (Test-Path $DataDir) {
        Remove-Item -Recurse -Force $DataDir -ErrorAction SilentlyContinue
    }
}
