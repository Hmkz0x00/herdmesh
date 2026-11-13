# herdmesh easy test: FRIEND side. No Rust needed: downloads herdmesh.exe from
# the latest GitHub release, joins with the ticket, and goes online.
# Usage:  powershell -ExecutionPolicy Bypass -File easy-join.ps1 "<ticket>"
param([Parameter(Mandatory = $true)][string]$Ticket)
# Windows PowerShell 5.1 turns native stderr into errors under 'Stop';
# check $LASTEXITCODE instead.
$ErrorActionPreference = 'Continue'
$dir = Join-Path $env:LOCALAPPDATA 'herdmesh-test'
$exe = Join-Path $dir 'herdmesh.exe'
$sums = Join-Path $dir 'SHA256SUMS.txt'
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$url = 'https://github.com/Hmkz0x00/herdmesh/releases/latest/download'
Write-Host 'Downloading herdmesh.exe ...'
try {
    Invoke-WebRequest -Uri "$url/herdmesh-windows-x64.exe" -OutFile $exe -UseBasicParsing -ErrorAction Stop
    Invoke-WebRequest -Uri "$url/SHA256SUMS.txt" -OutFile $sums -UseBasicParsing -ErrorAction Stop
}
catch {
    Remove-Item -LiteralPath $exe -Force -ErrorAction SilentlyContinue
    Write-Host "download failed: $_" -ForegroundColor Red
    exit 1
}
$expectedLine = Get-Content -LiteralPath $sums | Where-Object { $_ -match '^([0-9a-f]{64})  herdmesh-windows-x64\.exe$' } | Select-Object -First 1
$actualHash = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()
if (-not $expectedLine -or $actualHash -ne $Matches[1]) {
    Remove-Item -LiteralPath $exe -Force -ErrorAction SilentlyContinue
    Write-Host 'SHA256 verification failed for herdmesh.exe; the downloaded executable was deleted.' -ForegroundColor Red
    exit 1
}
& $exe room join $Ticket.Trim() --as friend --name test
if ($LASTEXITCODE -ne 0) { Write-Host 'join failed (see message above)' -ForegroundColor Red; exit 1 }
Write-Host ''
Write-Host '=== Joined. Going online; you should see the host below. Ctrl+C to stop. ===' -ForegroundColor Green
$env:RUST_LOG = 'iroh=info,hm_net=info'
& $exe room run test
