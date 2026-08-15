param(
    [Parameter(Mandatory = $true)]
    [string]$Tag,
    [Parameter(Mandatory = $true)]
    [string]$Commit,
    [ValidateSet('Nightly', 'Stable')]
    [string]$Channel = 'Nightly',
    [switch]$ValidateOnly,
    [string]$Repository = 'dain98/clipline',
    [string]$ReleaseDirectory = 'dist',
    [string]$NotesPath,
    [datetime]$PublishedAt = [datetime]::UtcNow
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0

$workspaceRoot = [System.IO.Path]::GetFullPath((Split-Path $PSScriptRoot -Parent))
$appRoot = Join-Path $workspaceRoot 'apps\clipline-app'
$tauri = Get-Content -LiteralPath (Join-Path $appRoot 'tauri.conf.json') -Raw | ConvertFrom-Json
$version = [string]$tauri.version
$cargoManifest = Get-Content -LiteralPath (Join-Path $appRoot 'Cargo.toml') -Raw
$cargoLock = Get-Content -LiteralPath (Join-Path $workspaceRoot 'Cargo.lock') -Raw
$cargoVersion = [regex]::Match(
    $cargoManifest,
    '(?ms)^\[package\]\s+name\s*=\s*"clipline-app"\s+version\s*=\s*"([^"]+)"'
).Groups[1].Value
$lockVersion = [regex]::Match(
    $cargoLock,
    '(?ms)^\[\[package\]\]\s+name\s*=\s*"clipline-app"\s+version\s*=\s*"([^"]+)"'
).Groups[1].Value

if ([string]::IsNullOrWhiteSpace($version) -or $cargoVersion -cne $version -or $lockVersion -cne $version) {
    throw "Clipline versions disagree: Tauri=$version Cargo=$cargoVersion Cargo.lock=$lockVersion."
}

$expectedTag = if ($Channel -eq 'Stable') { "v$version" } else { "nightly-v$version" }
if ($Tag -cne $expectedTag) {
    throw "$Channel tag must exactly match the application version: $expectedTag."
}
if ($Commit -notmatch '^[0-9a-fA-F]{40}$') {
    throw "$Channel commit must be a full 40-character Git SHA."
}
if ($ValidateOnly) {
    Write-Host "Validated $Channel $version at $Commit."
    return
}
if ($Repository -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') {
    throw "Invalid GitHub repository name: $Repository."
}
if ([string]::IsNullOrWhiteSpace($NotesPath) -or -not (Test-Path -LiteralPath $NotesPath -PathType Leaf)) {
    throw 'Generated release notes are required.'
}

$releaseRoot = [System.IO.Path]::GetFullPath((Join-Path $workspaceRoot $ReleaseDirectory))
if (-not $releaseRoot.StartsWith("$workspaceRoot\", [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Release directory must remain inside $workspaceRoot."
}
New-Item -ItemType Directory -Path $releaseRoot -Force | Out-Null

$regularName = "Clipline_${version}_x64-setup.exe"
$standaloneName = "Clipline_${version}_x64-standalone-setup.exe"
$regularPath = Join-Path $releaseRoot $regularName
$standalonePath = Join-Path $releaseRoot $standaloneName
foreach ($path in @($regularPath, "$regularPath.sig", $standalonePath, "$standalonePath.sig")) {
    $file = Get-Item -LiteralPath $path -Force -ErrorAction Stop
    if ($file.PSIsContainer -or $file.Length -eq 0) {
        throw "$Channel asset must be a non-empty file: $path."
    }
}

$generatedNotes = (Get-Content -LiteralPath $NotesPath -Raw).Trim()
$shortCommit = $Commit.Substring(0, 8).ToLowerInvariant()
$assetTag = if ($Channel -eq 'Stable') { "v$version" } else { 'nightly' }
$notesTitle = if ($Channel -eq 'Stable') { "Clipline $version" } else { "Clipline Nightly $version" }
$notesSource = if ($Channel -eq 'Stable') { 'main' } else { 'develop' }
$notes = @"
## $notesTitle

$generatedNotes

Built automatically from $notesSource commit $shortCommit after workspace tests, warning-denied
Clippy, and pinned release-runtime verification.
"@.Trim()
$notesName = "release-notes-$version.md"
[System.IO.File]::WriteAllText(
    (Join-Path $releaseRoot $notesName),
    "$notes`n",
    [System.Text.UTF8Encoding]::new($false)
)

$published = $PublishedAt.ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
function Write-UpdaterManifest {
    param(
        [string]$InstallerName,
        [string]$OutputName
    )

    $signature = (Get-Content -LiteralPath (Join-Path $releaseRoot "$InstallerName.sig") -Raw).Trim()
    $manifest = [ordered]@{
        version = $version
        notes = $notes
        pub_date = $published
        platforms = [ordered]@{
            'windows-x86_64' = [ordered]@{
                signature = $signature
                url = "https://github.com/$Repository/releases/download/$assetTag/$InstallerName"
            }
        }
    }
    $json = $manifest | ConvertTo-Json -Depth 6
    [System.IO.File]::WriteAllText(
        (Join-Path $releaseRoot $OutputName),
        "$json`n",
        [System.Text.UTF8Encoding]::new($false)
    )
}

Write-UpdaterManifest -InstallerName $regularName -OutputName 'latest.json'
Write-UpdaterManifest -InstallerName $standaloneName -OutputName 'latest-standalone.json'

$expected = @(
    $regularName,
    "$regularName.sig",
    $standaloneName,
    "$standaloneName.sig",
    'latest.json',
    'latest-standalone.json',
    $notesName
)
$actual = @(Get-ChildItem -LiteralPath $releaseRoot -File | ForEach-Object Name)
$difference = @(Compare-Object $expected $actual)
if ($difference.Count -ne 0) {
    throw "$Channel release directory must contain exactly seven expected assets: $($difference | Out-String)"
}
Write-Host "Prepared seven $Channel $version assets in $releaseRoot."
