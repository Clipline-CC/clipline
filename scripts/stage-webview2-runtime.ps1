param(
    [string]$ArchivePath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0

$workspaceRoot = [System.IO.Path]::GetFullPath((Split-Path $PSScriptRoot -Parent))
$appRoot = Join-Path $workspaceRoot 'apps\clipline-app'
$manifestPath = Join-Path $appRoot 'webview2-fixed-runtime.json'
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
if ([int]$manifest.schema_version -ne 1) {
    throw 'WebView2 runtime manifest schema_version must be 1.'
}

$archiveUri = [uri]$manifest.archive_url
if ($archiveUri.Scheme -cne 'https' -or $archiveUri.Host -cne 'msedge.sf.dl.delivery.mp.microsoft.com') {
    throw "WebView2 archive must use Microsoft's reviewed HTTPS download host."
}

if ([string]::IsNullOrWhiteSpace($ArchivePath)) {
    $inputRoot = if ($env:RUNNER_TEMP) {
        Join-Path $env:RUNNER_TEMP 'clipline-webview2-inputs'
    } else {
        Join-Path $env:LOCALAPPDATA 'Clipline\release-inputs'
    }
    New-Item -ItemType Directory -Path $inputRoot -Force | Out-Null
    $ArchivePath = Join-Path $inputRoot $manifest.archive_name
    if (-not (Test-Path -LiteralPath $ArchivePath -PathType Leaf)) {
        Invoke-WebRequest -UseBasicParsing -Uri $manifest.archive_url -OutFile $ArchivePath
    }
}

$archive = Get-Item -LiteralPath ([System.IO.Path]::GetFullPath($ArchivePath)) -Force
if ($archive.PSIsContainer -or $archive.Name -cne $manifest.archive_name) {
    throw "WebView2 release input must be the exact archive $($manifest.archive_name)."
}
if ($archive.Length -ne [int64]$manifest.archive_size) {
    throw "WebView2 archive size mismatch: expected $($manifest.archive_size), got $($archive.Length)."
}
$archiveHash = (Get-FileHash -LiteralPath $archive.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
if ($archiveHash -cne $manifest.archive_sha256) {
    throw "WebView2 archive SHA-256 mismatch: expected $($manifest.archive_sha256), got $archiveHash."
}

$runtimeFolder = "Microsoft.WebView2.FixedVersionRuntime.$($manifest.version).$($manifest.architecture)"
$payloadRoot = [System.IO.Path]::GetFullPath((Join-Path $appRoot 'webview2-fixed'))
$destination = [System.IO.Path]::GetFullPath((Join-Path $payloadRoot $runtimeFolder))
if (-not $destination.StartsWith("$payloadRoot\", [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Refusing to stage WebView2 outside $payloadRoot."
}

$temporary = Join-Path $appRoot ('.webview2-stage-{0}-{1}' -f $PID, [guid]::NewGuid().ToString('N'))
$backup = Join-Path $appRoot ('.webview2-previous-{0}-{1}' -f $PID, [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $temporary | Out-Null
$published = $false

try {
    $expand = Join-Path $env:SystemRoot 'System32\expand.exe'
    & $expand '-F:*' $archive.FullName $temporary | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "expand.exe failed with exit code $LASTEXITCODE."
    }

    $extracted = Join-Path $temporary $runtimeFolder
    $entries = @(Get-ChildItem -LiteralPath $temporary -Force)
    if ($entries.Count -ne 1 -or $entries[0].Name -cne $runtimeFolder) {
        throw "WebView2 CAB did not contain the expected single root $runtimeFolder."
    }
    if (-not (Test-Path -LiteralPath (Join-Path $extracted 'msedgewebview2.exe') -PathType Leaf)) {
        throw 'WebView2 CAB is missing msedgewebview2.exe.'
    }

    New-Item -ItemType Directory -Path $payloadRoot -Force | Out-Null
    if (Test-Path -LiteralPath $destination) {
        Move-Item -LiteralPath $destination -Destination $backup
    }
    try {
        Move-Item -LiteralPath $extracted -Destination $destination
        & (Join-Path $PSScriptRoot 'verify-webview2-runtime.ps1') -RequirePayload
        $published = $true
    } catch {
        if (Test-Path -LiteralPath $destination) {
            Remove-Item -LiteralPath $destination -Recurse -Force
        }
        if (Test-Path -LiteralPath $backup) {
            Move-Item -LiteralPath $backup -Destination $destination
        }
        throw
    }
    if (Test-Path -LiteralPath $backup) {
        Remove-Item -LiteralPath $backup -Recurse -Force
    }
    Write-Host "Staged verified WebView2 Fixed Version runtime at $destination"
} finally {
    if (Test-Path -LiteralPath $temporary) {
        Remove-Item -LiteralPath $temporary -Recurse -Force
    }
    if (-not $published -and (Test-Path -LiteralPath $backup) -and -not (Test-Path -LiteralPath $destination)) {
        Move-Item -LiteralPath $backup -Destination $destination
    }
}
