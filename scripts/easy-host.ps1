# herdmesh easy host: HOST side.
# Downloads the latest herdmesh release, installs/updates it in %LOCALAPPDATA%\herdmesh,
# ensures it is in PATH, and sets up a room for the current project (copies invite ticket).
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts\easy-host.ps1
param()

# Windows PowerShell 5.1 turns native stderr into errors under 'Stop';
# check $LASTEXITCODE instead.
$ErrorActionPreference = 'Continue'

$installDir = Join-Path $env:LOCALAPPDATA 'herdmesh'
$installedExe = Join-Path $installDir 'herdmesh.exe'

# ALWAYS download the latest release herdmesh-windows-x64.exe + SHA256SUMS.txt to a temp file.
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ('herdmesh-install-' + [System.Guid]::NewGuid().ToString('n'))
New-Item -ItemType Directory -Force -Path $tempDir | Out-Null
$tempExe = Join-Path $tempDir 'herdmesh-windows-x64.exe'
$tempSums = Join-Path $tempDir 'SHA256SUMS.txt'

$url = 'https://github.com/Hmkz0x00/herdmesh/releases/latest/download'
Write-Host 'Downloading latest herdmesh release ...' -ForegroundColor Cyan

# Enable TLS 1.2 on older PowerShell 5.1 environments
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

try {
    Invoke-WebRequest -Uri "$url/herdmesh-windows-x64.exe" -OutFile $tempExe -UseBasicParsing -ErrorAction Stop
    Invoke-WebRequest -Uri "$url/SHA256SUMS.txt" -OutFile $tempSums -UseBasicParsing -ErrorAction Stop
}
catch {
    Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "Download failed: $_" -ForegroundColor Red
    exit 1
}

# Verify SHA256 (fail closed: delete + exit 1 on mismatch).
$expectedLine = Get-Content -LiteralPath $tempSums | Where-Object { $_ -match '^([0-9a-fA-F]{64})\s+herdmesh-windows-x64\.exe$' } | Select-Object -First 1
if (-not $expectedLine) {
    Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host 'Could not find checksum for herdmesh-windows-x64.exe in SHA256SUMS.txt; installation aborted.' -ForegroundColor Red
    exit 1
}

$expectedHash = $Matches[1].ToLowerInvariant()
$actualHash = (Get-FileHash -LiteralPath $tempExe -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actualHash -ne $expectedHash) {
    Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host 'SHA256 checksum mismatch for herdmesh-windows-x64.exe; downloaded files deleted.' -ForegroundColor Red
    exit 1
}

# Stop running herdmesh processes before replacing the executable.
$running = Get-Process herdmesh -ErrorAction SilentlyContinue
if ($running) {
    Write-Host 'Stopping running herdmesh processes...'
    $running | Stop-Process -Force -ErrorAction SilentlyContinue
    $running | ForEach-Object {
        try { $_.WaitForExit(3000) } catch {}
    }
}

# Replace the installed exe.
New-Item -ItemType Directory -Force -Path $installDir | Out-Null
try {
    Copy-Item -LiteralPath $tempExe -Destination $installedExe -Force -ErrorAction Stop
}
catch {
    Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "Failed to install herdmesh.exe into ${installDir}: $_" -ForegroundColor Red
    exit 1
}
finally {
    Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
}

# Add the install dir to the USER Path if missing (no admin), and also to the current session $env:PATH.
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($null -eq $userPath) { $userPath = '' }
$normalizedInstallDir = $installDir.TrimEnd('\')
$hasInstallDir = $false
foreach ($part in ($userPath -split ';')) {
    if ($part.Trim().TrimEnd('\') -ieq $normalizedInstallDir) {
        $hasInstallDir = $true
        break
    }
}
if (-not $hasInstallDir) {
    $newUserPath = if ([string]::IsNullOrWhiteSpace($userPath)) { $installDir } else { "$userPath;$installDir" }
    [Environment]::SetEnvironmentVariable('Path', $newUserPath, 'User')
}

$hasSessionDir = $false
foreach ($part in ($env:PATH -split ';')) {
    if ($part.Trim().TrimEnd('\') -ieq $normalizedInstallDir) {
        $hasSessionDir = $true
        break
    }
}
if (-not $hasSessionDir) {
    $env:PATH = "$installDir;$env:PATH"
}

# In the CURRENT folder (the user's project), run 'herdmesh setup' (it creates/configures room and copies ticket).
Write-Host 'Running herdmesh setup in current folder...' -ForegroundColor Cyan
& $installedExe setup

if ($LASTEXITCODE -ne 0) {
    Write-Host "herdmesh setup failed with exit code $LASTEXITCODE." -ForegroundColor Red
    exit $LASTEXITCODE
}

# Print 3 next steps: open a new terminal; run 'herdmesh view'; start your agents.
Write-Host ''
Write-Host '=== Setup complete ===' -ForegroundColor Green
Write-Host 'Next steps:'
Write-Host '  1. Open a new terminal'
Write-Host "  2. Run 'herdmesh view'"
Write-Host '  3. Start your agents'
