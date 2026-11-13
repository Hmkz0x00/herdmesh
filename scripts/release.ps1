param(
    [Parameter(Mandatory = $true)]
    [string]$Version
)

$ErrorActionPreference = 'Stop'

if ($Version.StartsWith('v')) {
    $Version = $Version.Substring(1)
}
if ([string]::IsNullOrWhiteSpace($Version)) {
    throw 'Version must not be empty.'
}

$metadata = cargo metadata --format-version 1 --no-deps | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) {
    throw 'cargo metadata failed.'
}
$targetDirectory = $metadata.target_directory
$exePath = Join-Path $targetDirectory 'release\herdmesh.exe'

cargo build --release -p herdmesh --locked
if ($LASTEXITCODE -ne 0) {
    throw 'cargo build failed.'
}
if (-not (Test-Path -LiteralPath $exePath -PathType Leaf)) {
    throw "Expected release executable was not produced: $exePath"
}

$dist = Join-Path (Split-Path -Parent $PSScriptRoot) 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null
$releaseExe = Join-Path $dist 'herdmesh-windows-x64.exe'
$releaseScript = Join-Path $dist 'easy-join.ps1'
Copy-Item -LiteralPath $exePath -Destination $releaseExe -Force
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'easy-join.ps1') -Destination $releaseScript -Force

$sumLines = foreach ($path in @($releaseExe, $releaseScript)) {
    $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    '{0}  {1}' -f $hash, (Split-Path -Leaf $path)
}
Set-Content -LiteralPath (Join-Path $dist 'SHA256SUMS.txt') -Value $sumLines -Encoding Ascii

Write-Host "Run this command to publish the release:"
Write-Host "gh release create v$Version dist/* --title v$Version --notes 'herdmesh v$Version'"
