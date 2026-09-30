param([string]$SignatureVerifier)

$ErrorActionPreference = 'Stop'
$workspace = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$workflow = Get-Content -LiteralPath "$workspace/.github/workflows/_sign-release.yml" -Raw
$scripts = @([regex]::Matches($workflow, '(?m)^        run: \|\r?\n((?:          .*\r?\n|\r?\n)+)') | ForEach-Object {
    [scriptblock]::Create([regex]::Replace($_.Groups[1].Value, '(?m)^          ', ''))
})
if ($scripts.Count -ne 2) { throw 'Expected validation and signing scripts.' }
$version = (Get-Content "$workspace/apps/clipline-app/tauri.conf.json" -Raw | ConvertFrom-Json).version
$commit = git -C $workspace rev-parse HEAD
$root = Join-Path $workspace "target/signing-test-$([Guid]::NewGuid().ToString('N'))"

foreach ($channel in @('nightly', 'stable')) {
    $directory = Join-Path $root $channel
    New-Item -ItemType Directory -Path "$directory/dist", "$directory/temp" -Force | Out-Null
    Set-Content "$directory/notes.md" 'Throwaway signing regression.'
    Set-Content "$directory/dist/Clipline_${version}_x64-setup.exe" 'regular payload'
    Set-Content "$directory/dist/Clipline_${version}_x64-standalone-setup.exe" 'different standalone payload'
    $env:RELEASE_CHANNEL = $channel
    $env:GITHUB_REF_NAME = if ($channel -eq 'nightly') { "nightly-v$version" } else { "v$version" }
    $env:GITHUB_REPOSITORY = 'Clipline-CC/clipline'
    $env:RUNNER_TEMP = "$directory/temp"
    $env:GITHUB_ENV = "$directory/environment.txt"
    & "$workspace/scripts/prepare-nightly-assets.ps1" -Tag $env:GITHUB_REF_NAME -Commit $commit `
        -Channel $channel -Unsigned -ReleaseDirectory ([IO.Path]::GetRelativePath($workspace, "$directory/dist")) `
        -NotesPath "$directory/notes.md"
    Push-Location $directory
    try {
        & $scripts[0]
        $env:RELEASE_SIGNER = (Get-Content $env:GITHUB_ENV | Where-Object { $_.StartsWith('RELEASE_SIGNER=') }).Substring(15)
        $key = "$directory/throwaway.key"
        & $env:RELEASE_SIGNER signer generate --ci -p '' -w $key *> $null
        if ($LASTEXITCODE -ne 0) { throw 'Throwaway key generation failed.' }
        $env:TAURI_SIGNING_PRIVATE_KEY = (Get-Content -LiteralPath $key -Raw).Trim()
        $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ''
        $manifestPath = 'dist/latest.json'
        $original = Get-Content -LiteralPath $manifestPath -Raw
        $invalid = $original | ConvertFrom-Json
        $invalid.platforms.'windows-x86_64'.signature = 'unexpected'
        $invalid | ConvertTo-Json -Depth 6 | Set-Content $manifestPath
        $rejected = $false
        try { & $scripts[0] } catch { $rejected = $true }
        if (-not $rejected) { throw 'Signing accepted an already-signed manifest.' }
        Set-Content $manifestPath $original
        & $scripts[1] *> $null
        $publicText = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String((Get-Content "$key.pub" -Raw).Trim()))
        $publicKey = @($publicText -split '\r?\n' | Where-Object { $_ -and -not $_.StartsWith('untrusted comment:') })[0]
        foreach ($manifestName in @('latest.json', 'latest-standalone.json')) {
            $manifest = Get-Content "dist/$manifestName" -Raw | ConvertFrom-Json
            $platform = $manifest.platforms.'windows-x86_64'
            $installer = "dist/$(([Uri]$platform.url).Segments[-1])"
            if ($platform.signature -cne (Get-Content "$installer.sig" -Raw).Trim()) { throw 'Manifest signature differs from sidecar.' }
            if ($SignatureVerifier) {
                & $SignatureVerifier $publicKey $platform.signature $installer
                if ($LASTEXITCODE -ne 0) { throw 'Manifest does not verify its installer.' }
            }
        }
    } finally {
        Pop-Location
        Remove-Item Env:TAURI_SIGNING_PRIVATE_KEY, Env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD -ErrorAction SilentlyContinue
    }
}
Write-Host 'Nightly and Stable artifact-only signing checks passed with throwaway keys.'
