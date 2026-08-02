$ErrorActionPreference = "Stop"
Set-StrictMode -Version 2.0

$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$builder = Join-Path $repoRoot "scripts\build-slint-installer.ps1"
$installer = Get-Content -LiteralPath (Join-Path $repoRoot "packaging\slint\installer.nsi") -Raw
$shared = Get-Content -LiteralPath (Join-Path $repoRoot "packaging\slint\installer-shared.nsh") -Raw
$fixtureRoots = [System.Collections.Generic.List[string]]::new()
$powershell = (Get-Process -Id $PID).Path

function Write-Utf8 {
  param([string]$Path, [string]$Text)
  $parent = Split-Path -Parent $Path
  if (-not [string]::IsNullOrWhiteSpace($parent) -and -not (Test-Path -LiteralPath $parent)) {
    New-Item -ItemType Directory -Path $parent -Force | Out-Null
  }
  [System.IO.File]::WriteAllText($Path, $Text, [System.Text.UTF8Encoding]::new($false))
}

function New-PackageFixture {
  param(
    [string]$Variant = "regular",
    [string]$ProbeVariant = $Variant,
    [string]$ProbeVersion = "0.1.43"
  )

  $root = Join-Path ([System.IO.Path]::GetTempPath()) ("clipline-slint-package-test-{0}-{1}" -f $PID, [guid]::NewGuid().ToString("N"))
  $fixtureRoots.Add($root)
  foreach ($directory in @(
    "apps\clipline-app\ffmpeg",
    "apps\clipline-app\icons",
    "packaging\slint",
    "scripts",
    "out"
  )) {
    New-Item -ItemType Directory -Path (Join-Path $root $directory) -Force | Out-Null
  }

  Copy-Item -LiteralPath (Join-Path $repoRoot "packaging\slint\installer.nsi") -Destination (Join-Path $root "packaging\slint\installer.nsi")
  Copy-Item -LiteralPath (Join-Path $repoRoot "packaging\slint\installer-shared.nsh") -Destination (Join-Path $root "packaging\slint\installer-shared.nsh")
  Write-Utf8 (Join-Path $root "scripts\verify-ffmpeg-resource.ps1") "param([string]`$ResourceDirectory)`nthrow 'fixture verifier must not run'`n"
  Write-Utf8 (Join-Path $root "apps\clipline-app\Cargo.toml") "[package]`nname = `"clipline-app`"`nversion = `"0.1.43`"`n"
  Write-Utf8 (Join-Path $root "apps\clipline-app\tauri.conf.json") (@{
      productName = "Clipline"
      version = "0.1.43"
      identifier = "io.clipline.app"
      bundle = @{ publisher = "Clipline" }
    } | ConvertTo-Json -Depth 4)
  Write-Utf8 (Join-Path $root "THIRD-PARTY-NOTICES.md") "FFmpeg is LGPL software; source and replacement rights remain available.`n"
  Write-Utf8 (Join-Path $root "apps\clipline-app\icons\icon.ico") "fixture-icon"

  $ffmpegRoot = Join-Path $root "apps\clipline-app\ffmpeg"
  Write-Utf8 (Join-Path $ffmpegRoot "README.md") "fixture FFmpeg notice"
  Write-Utf8 (Join-Path $ffmpegRoot "PROVENANCE.json") '{"schema_version":1}'
  Write-Utf8 (Join-Path $ffmpegRoot "LICENSE.txt") "fixture LGPL license"
  Write-Utf8 (Join-Path $ffmpegRoot "ffmpeg.exe") "fixture executable bytes"
  $allowed = foreach ($name in @("LICENSE.txt", "ffmpeg.exe")) {
    $path = Join-Path $ffmpegRoot $name
    [ordered]@{
      archive_path = $name
      staged_name = $name
      size = (Get-Item -LiteralPath $path).Length
      sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    }
  }
  Write-Utf8 (Join-Path $root "apps\clipline-app\ffmpeg-runtime.json") (([ordered]@{
      schema_version = 1
      allowed_files = @($allowed)
    } | ConvertTo-Json -Depth 5) + "`n")

  $sourcePath = Join-Path $root "probe.rs"
  $candidatePath = Join-Path $root "clipline-slint-spike.exe"
  $probeJson = ([ordered]@{
      schemaVersion = 1
      kind = "clipline-slint-internal-candidate"
      productName = "Clipline"
      publisher = "Clipline"
      identifier = "io.clipline.app"
      version = $ProbeVersion
      variant = $ProbeVariant
      applicationStateStarted = $false
      autostartRegistryMutation = $false
    } | ConvertTo-Json -Compress)
  Write-Utf8 $sourcePath @"
fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.len() == 2 && args[1] == "--clipline-package-probe" {
        println!("{}", r#"$probeJson"#);
    } else {
        std::process::exit(2);
    }
}
"@
  & rustc --edition 2021 -O $sourcePath -o $candidatePath
  if ($LASTEXITCODE -ne 0) {
    throw "failed to compile package-probe fixture"
  }

  return [pscustomobject]@{
    Root = $root
    Variant = $Variant
    Candidate = $candidatePath
    Hash = (Get-FileHash -LiteralPath $candidatePath -Algorithm SHA256).Hash.ToLowerInvariant()
    Output = (Join-Path $root "out")
    Ffmpeg = $ffmpegRoot
  }
}

function Invoke-Builder {
  param([object]$Fixture, [string]$ExpectedHash = $Fixture.Hash)

  $oldSelfTest = $env:CLIPLINE_PACKAGE_SELF_TEST
  $env:CLIPLINE_PACKAGE_SELF_TEST = "1"
  try {
    $arguments = @(
      "-NoProfile",
      "-ExecutionPolicy", "Bypass",
      "-File", $builder,
      "-Variant", $Fixture.Variant,
      "-CandidateExecutable", $Fixture.Candidate,
      "-ExpectedExecutableSha256", $ExpectedHash,
      "-OutputDirectory", $Fixture.Output,
      "-RepositoryRoot", $Fixture.Root,
      "-FixtureMode",
      "-ValidateOnly"
    )
    $quotedArguments = $arguments | ForEach-Object { '"' + $_.Replace('"', '\"') + '"' }
    $start = [System.Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $powershell
    $start.Arguments = $quotedArguments -join " "
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $start
    try {
      if (-not $process.Start()) {
        throw "package builder test process did not start"
      }
      $stdout = $process.StandardOutput.ReadToEndAsync()
      $stderr = $process.StandardError.ReadToEndAsync()
      if (-not $process.WaitForExit(30000)) {
        $process.Kill()
        throw "package builder test process timed out"
      }
      $output = $stdout.GetAwaiter().GetResult() + $stderr.GetAwaiter().GetResult()
      return [pscustomobject]@{ ExitCode = $process.ExitCode; Output = $output }
    } finally {
      $process.Dispose()
    }
  } finally {
    $env:CLIPLINE_PACKAGE_SELF_TEST = $oldSelfTest
  }
}

function Assert-Pass {
  param([string]$Name, [object]$Result)
  if ($Result.ExitCode -ne 0) {
    throw "$Name should pass but failed: $($Result.Output)"
  }
}

function Assert-Fail {
  param([string]$Name, [object]$Result, [string]$Pattern)
  if ($Result.ExitCode -eq 0) {
    throw "$Name should fail closed"
  }
  if ($Result.Output -notmatch $Pattern) {
    throw "$Name failed for the wrong reason. Expected /$Pattern/, got: $($Result.Output)"
  }
}

try {
  foreach ($token in @(
    "RequestExecutionLevel user",
    "SetShellVarContext current",
    "SetRegView 64",
    '"/P"',
    '"/R"',
    '"/UPDATE"',
    '"/ARGS"',
    "VersionCompare",
    "CLIPLINE_CANDIDATE_STATE_KEY",
    "UninstallString",
    "QuietUninstallString",
    "THIRD-PARTY-NOTICES.md",
    "ffmpeg\LICENSE.txt"
  )) {
    if (-not ($installer.Contains($token) -or $shared.Contains($token))) {
      throw "NSIS contract is missing $token"
    }
  }
  foreach ($exactFlag in @("/P", "/R", "/REINSTALL", "/UPDATE", "/ARGS")) {
    if (-not $installer.Contains("CLIPLINE_READ_EXACT_FLAG `"$exactFlag`"")) {
      throw "NSIS must parse $exactFlag as an exact token rather than a prefix"
    }
  }
  if (
    -not $installer.Contains("Function CliplineHasExactFlag") -or
    -not $shared.Contains("Prefix") -or
    -not $shared.Contains("shadow a later exact /R")
  ) {
    throw "the exact-flag scanner must continue past prefixes to a later exact token"
  }
  function Test-ExactFlagVector([string]$CommandLine, [string]$Flag) {
    return @($CommandLine -split '\s+' | Where-Object { $_ -ceq $Flag }).Count -gt 0
  }
  foreach ($vector in @(
    @("/REINSTALL", "/R", $false),
    @("/REINSTALL /R", "/R", $true),
    @("/Rjunk /R", "/R", $true),
    @("/PASSIVE", "/P", $false),
    @("/P /R /UPDATE /ARGS", "/ARGS", $true)
  )) {
    if ((Test-ExactFlagVector $vector[0] $vector[1]) -ne $vector[2]) {
      throw "exact flag vector failed: $($vector -join ' | ')"
    }
  }
  foreach ($token in @(
    "io.clipline.app",
    "Clipline",
    "regular",
    "standalone",
    "Clipline-Slint-Internal-Candidate"
  )) {
    if (-not $shared.Contains($token)) {
      throw "shared NSIS contract is missing $token"
    }
  }
  if (($installer + $shared).ToLowerInvariant().Contains("webview2")) {
    throw "native NSIS source must not stage WebView2"
  }
  if ($installer.Contains("MUI_PAGE_DIRECTORY") -or $installer.Contains("InstallDirRegKey")) {
    throw "the internal candidate install directory must not be user- or registry-overridable"
  }
  foreach ($token in @(
    'StrCmp $INSTDIR "${CLIPLINE_INSTALL_DIRECTORY}"',
    "CLIPLINE_PACKAGE_FENCE_NAME",
    "WaitForSingleObject",
    "un.CliplineDeleteRequired",
    'Function un.onInit'
  )) {
    if (-not ($installer.Contains($token) -or $shared.Contains($token))) {
      throw "NSIS isolation/process-fence contract is missing $token"
    }
  }
  if (@([regex]::Matches($installer, 'StrCmp \$INSTDIR "\$\{CLIPLINE_INSTALL_DIRECTORY\}"')).Count -ne 2) {
    throw "both installer and uninstaller must reject an overridden candidate directory"
  }
  foreach ($productionName in @(
    "Clipline_0.1.43_x64-setup.exe",
    "Clipline_0.1.43_x64-standalone-setup.exe"
  )) {
    if (($installer + $shared).Contains($productionName)) {
      throw "internal candidate must not use production updater name $productionName"
    }
  }

  $regular = New-PackageFixture
  Assert-Pass "valid regular fixture" (Invoke-Builder $regular)
  $standalone = New-PackageFixture -Variant "standalone" -ProbeVariant "standalone"
  Assert-Pass "valid standalone fixture" (Invoke-Builder $standalone)

  $tampered = New-PackageFixture
  Write-Utf8 (Join-Path $tampered.Ffmpeg "ffmpeg.exe") "fixture executable byteS"
  Assert-Fail "tampered FFmpeg" (Invoke-Builder $tampered) "substituted|wrong size"

  $dirty = New-PackageFixture
  Write-Utf8 (Join-Path $dirty.Ffmpeg "unexpected.dll") "dirty"
  Assert-Fail "dirty FFmpeg" (Invoke-Builder $dirty) "dirty"

  foreach ($missingName in @("LICENSE.txt", "PROVENANCE.json", "README.md")) {
    $missing = New-PackageFixture
    Remove-Item -LiteralPath (Join-Path $missing.Ffmpeg $missingName) -Force
    Assert-Fail "missing $missingName" (Invoke-Builder $missing) "dirty|cannot find|missing"
  }

  $missingNotices = New-PackageFixture
  Remove-Item -LiteralPath (Join-Path $missingNotices.Root "THIRD-PARTY-NOTICES.md") -Force
  Assert-Fail "missing attribution" (Invoke-Builder $missingNotices) "cannot find|attribution"

  $wrongHash = New-PackageFixture
  Assert-Fail "wrong executable hash" (Invoke-Builder $wrongHash ("0" * 64)) "independently supplied expected hash"

  $wrongVersion = New-PackageFixture -ProbeVersion "0.1.44"
  Assert-Fail "wrong package version" (Invoke-Builder $wrongVersion) "probe product, version, variant"

  $crossInstall = New-PackageFixture -Variant "standalone" -ProbeVariant "regular"
  Assert-Fail "cross-install variant receipt" (Invoke-Builder $crossInstall) "probe product, version, variant"

  $cargoMismatch = New-PackageFixture
  Write-Utf8 (Join-Path $cargoMismatch.Root "apps\clipline-app\Cargo.toml") "[package]`nname = `"clipline-app`"`nversion = `"0.1.42`"`n"
  Assert-Fail "application version mismatch" (Invoke-Builder $cargoMismatch) "versions must match"

  $preexisting = New-PackageFixture
  $preexistingPath = Join-Path $preexisting.Output "Clipline-Slint-Internal-Candidate_0.1.43_x64-setup.exe"
  Write-Utf8 $preexistingPath "owned"
  Assert-Fail "pre-existing output" (Invoke-Builder $preexisting) "pre-existing package output"
  if ((Get-Content -LiteralPath $preexistingPath -Raw) -cne "owned") {
    throw "pre-existing output must remain byte-for-byte owned by its caller"
  }

  $webview = New-PackageFixture
  $webviewPath = Join-Path $webview.Ffmpeg "WebView2Loader.dll"
  Write-Utf8 $webviewPath "webview fixture"
  $manifestPath = Join-Path $webview.Root "apps\clipline-app\ffmpeg-runtime.json"
  $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
  $manifest.allowed_files += [pscustomobject]@{
    archive_path = "WebView2Loader.dll"
    staged_name = "WebView2Loader.dll"
    size = (Get-Item -LiteralPath $webviewPath).Length
    sha256 = (Get-FileHash -LiteralPath $webviewPath -Algorithm SHA256).Hash.ToLowerInvariant()
  }
  Write-Utf8 $manifestPath (($manifest | ConvertTo-Json -Depth 5) + "`n")
  Assert-Fail "webview payload" (Invoke-Builder $webview) "forbidden WebView/Tauri asset"

  $oversized = New-PackageFixture
  $stream = [System.IO.File]::Open($oversized.Candidate, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
  try {
    $stream.SetLength(257MB)
  } finally {
    $stream.Dispose()
  }
  Assert-Fail "oversized executable" (Invoke-Builder $oversized) "size must be between"

  $oversizedFfmpeg = New-PackageFixture
  $stream = [System.IO.File]::Open(
    (Join-Path $oversizedFfmpeg.Ffmpeg "ffmpeg.exe"),
    [System.IO.FileMode]::Open,
    [System.IO.FileAccess]::Write,
    [System.IO.FileShare]::None
  )
  try {
    $stream.SetLength(513MB)
  } finally {
    $stream.Dispose()
  }
  Assert-Fail "oversized FFmpeg source" (Invoke-Builder $oversizedFfmpeg) "outside the 512 MiB bound before staging"

  $knownMakensis = Join-Path $env:LOCALAPPDATA "tauri\NSIS\Bin\makensis.exe"
  if (Test-Path -LiteralPath $knownMakensis -PathType Leaf) {
    $versionOutput = @(& $knownMakensis /VERSION 2>&1 | ForEach-Object { $_.ToString().Trim() })
    if ($LASTEXITCODE -ne 0 -or $versionOutput.Count -ne 1 -or $versionOutput[0] -cne "v3.11") {
      throw "discovered makensis is not reviewed v3.11"
    }
  } else {
    Write-Host "SKIP: reviewed makensis v3.11 was not discovered; real installer build remains pending"
  }

  Write-Host "Slint installer helper self-tests passed (tampered, oversized, webview, and cross-install boundaries)."
} finally {
  foreach ($root in $fixtureRoots) {
    $resolved = [System.IO.Path]::GetFullPath($root)
    $temporaryRoot = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
    if ($resolved.StartsWith($temporaryRoot, [System.StringComparison]::OrdinalIgnoreCase) -and (Test-Path -LiteralPath $resolved)) {
      Remove-Item -LiteralPath $resolved -Recurse -Force
    }
  }
}
