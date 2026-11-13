# herdmesh easy test: HOST side. Creates room "test", copies the invite ticket
# to your clipboard, then goes online and shows the connection log.
# Usage:  powershell -ExecutionPolicy Bypass -File scripts\easy-host.ps1
$ErrorActionPreference = 'Stop'
$exe = (Get-Command herdmesh -ErrorAction SilentlyContinue).Source
if (-not $exe) { $exe = Join-Path $env:LOCALAPPDATA 'herdmesh-test\herdmesh.exe' }
if (-not (Test-Path $exe)) { throw "herdmesh not found. Install it first (scripts\install.ps1)." }

# A background daemon would hold the room; stop it for this foreground test.
Get-Process herdmesh -ErrorAction SilentlyContinue | Stop-Process -Force

$ticket = & $exe room invite test 2>$null
if ($LASTEXITCODE -ne 0 -or -not $ticket) {
    & $exe room create test | Out-Null
    $ticket = & $exe room invite test
}
$ticket = ($ticket | Select-Object -Last 1).Trim()
Set-Clipboard -Value $ticket

Write-Host ''
Write-Host '=== Invite ticket copied to your clipboard ===' -ForegroundColor Green
Write-Host 'Paste it to your friend in a PRIVATE message (it contains the room secret).'
Write-Host 'Leave this window open. When your friend joins you will see them below.'
Write-Host 'Press Ctrl+C to stop.' ; Write-Host ''
$env:RUST_LOG = 'iroh=info,hm_net=info'
& $exe room run test
