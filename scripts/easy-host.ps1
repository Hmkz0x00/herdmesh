# herdmesh easy test: HOST side. Creates room "test", copies the invite ticket
# to your clipboard, then goes online and shows the connection log.
# Usage:  powershell -ExecutionPolicy Bypass -File scripts\easy-host.ps1
# Windows PowerShell 5.1 turns native stderr into errors under 'Stop';
# check $LASTEXITCODE instead.
$ErrorActionPreference = 'Continue'
$exe = (Get-Command herdmesh -ErrorAction SilentlyContinue).Source
if (-not $exe) { $exe = Join-Path $env:LOCALAPPDATA 'herdmesh-test\herdmesh.exe' }
if (-not (Test-Path $exe)) { Write-Host "herdmesh not found. Install it first (scripts\install.ps1)." -ForegroundColor Red; exit 1 }

# A background daemon would hold the room; stop it for this foreground test.
Get-Process herdmesh -ErrorAction SilentlyContinue | Stop-Process -Force

$ticket = cmd /c "`"$exe`" room invite test 2>nul"
if ($LASTEXITCODE -ne 0 -or -not $ticket) {
    cmd /c "`"$exe`" room create test >nul 2>nul"
    if ($LASTEXITCODE -ne 0) { Write-Host 'room create failed' -ForegroundColor Red; exit 1 }
    $ticket = cmd /c "`"$exe`" room invite test 2>nul"
}
if (-not $ticket) { Write-Host 'could not get an invite ticket' -ForegroundColor Red; exit 1 }
$ticket = ($ticket | Select-Object -Last 1).Trim()
Set-Clipboard -Value $ticket

Write-Host ''
Write-Host '=== Invite ticket copied to your clipboard ===' -ForegroundColor Green
Write-Host 'Paste it to your friend in a PRIVATE message (it contains the room secret).'
Write-Host 'Leave this window open. When your friend joins you will see them below.'
Write-Host 'Press Ctrl+C to stop.' ; Write-Host ''
$env:RUST_LOG = 'iroh=info,hm_net=info'
& $exe room run test
