# herdmesh easy test: FRIEND side. No Rust needed: downloads herdmesh.exe from
# the latest GitHub release, joins with the ticket, and goes online.
# Usage:  powershell -ExecutionPolicy Bypass -File easy-join.ps1 "<ticket>"
param([Parameter(Mandatory = $true)][string]$Ticket)
# Windows PowerShell 5.1 turns native stderr into errors under 'Stop';
# check $LASTEXITCODE instead.
$ErrorActionPreference = 'Continue'
$dir = Join-Path $env:LOCALAPPDATA 'herdmesh-test'
$exe = Join-Path $dir 'herdmesh.exe'
New-Item -ItemType Directory -Force -Path $dir | Out-Null
if (-not (Test-Path $exe)) {
    Write-Host 'Downloading herdmesh.exe ...'
    $url = 'https://github.com/Hmkz0x00/herdmesh/releases/latest/download/herdmesh-windows-x64.exe'
    try { Invoke-WebRequest -Uri $url -OutFile $exe -UseBasicParsing -ErrorAction Stop }
    catch { Write-Host "download failed: $_" -ForegroundColor Red; exit 1 }
}
& $exe room join $Ticket.Trim() --as friend --name test
if ($LASTEXITCODE -ne 0) { Write-Host 'join failed (see message above)' -ForegroundColor Red; exit 1 }
Write-Host ''
Write-Host '=== Joined. Going online; you should see the host below. Ctrl+C to stop. ===' -ForegroundColor Green
$env:RUST_LOG = 'iroh=info,hm_net=info'
& $exe room run test
