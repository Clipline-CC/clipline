<#
Compare the working CPU converter with released 1.0.5. Requires git and rustc.
Uses standalone exact-source copies with only error metadata/module paths adapted;
no Windows capture, encoder, GPU readback or installer is exercised.
Example: powershell -ExecutionPolicy Bypass -File scripts/benchmark-cpu-conversion.ps1
#>
[CmdletBinding()]
param(
    [string]$OutputDirectory = (Join-Path ([IO.Path]::GetTempPath()) ('clipline-cpu-' + [guid]::NewGuid())),
    [ValidateRange(1, 100)][int]$Rounds = 12,
    [ValidateRange(1, 1000)][int]$Frames = 16,
    [ValidateRange(1, 1000000)][int]$VerifyCases = 50000,
    [switch]$VerifyOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
$baseline = 'a9a214eeb15f3597920fc4657d98e5cf62f56fd1'
$output = [IO.Path]::GetFullPath($OutputDirectory)
[IO.Directory]::CreateDirectory($output) | Out-Null
$utf8 = New-Object System.Text.UTF8Encoding($false)

function Read-Baseline([string]$Path) {
    $lines = & git -C $repo show "${baseline}:$Path"
    if ($LASTEXITCODE -ne 0) { throw "Cannot read $Path at $baseline; fetch repository history first." }
    return ($lines -join "`n") + "`n"
}

function Stage-Converter([string]$Name, [string]$Cpu, [string]$Layout) {
    [IO.File]::WriteAllText((Join-Path $output "$Name-original.rs"), $Cpu, $utf8)
    # Keep the algorithm intact; standalone rustc has no thiserror dependency.
    $adapted = $Cpu.Replace(', thiserror::Error', '')
    $adapted = $adapted -replace '(?m)^\s*#\[error\([^\r\n]*\)\]\r?\n', ''
    $adapted = $adapted.Replace('crate::video_layout::', "crate::${Name}_layout::")
    [IO.File]::WriteAllText((Join-Path $output "$Name.rs"), $adapted, $utf8)
    [IO.File]::WriteAllText((Join-Path $output "${Name}_layout.rs"), $Layout, $utf8)
}

$cpuPath = 'crates/clipline-capture/src/cpu_video.rs'
$layoutPath = 'crates/clipline-capture/src/video_layout.rs'
Stage-Converter 'baseline' (Read-Baseline $cpuPath) (Read-Baseline $layoutPath)
Stage-Converter 'candidate' ([IO.File]::ReadAllText((Join-Path $repo $cpuPath))) `
    ([IO.File]::ReadAllText((Join-Path $repo $layoutPath)))
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'cpu-conversion-bench.rs') -Destination (Join-Path $output 'main.rs')
$compiler = & rustc -vV
if ($LASTEXITCODE -ne 0) { throw 'rustc is unavailable' }
$metadata = [ordered]@{
    baseline_commit = $baseline
    working_commit = (& git -C $repo rev-parse HEAD)
    working_diff = (& git -C $repo diff -- $cpuPath $layoutPath) -join "`n"
    candidate_sha256 = (Get-FileHash -LiteralPath (Join-Path $output 'candidate-original.rs') -Algorithm SHA256).Hash
    candidate_layout_sha256 = (Get-FileHash -LiteralPath (Join-Path $output 'candidate_layout.rs') -Algorithm SHA256).Hash
    compiler = $compiler -join "`n"
    compiler_flags = '--edition 2021 -C opt-level=3 -C codegen-units=1 -D warnings'
    rounds = $Rounds
    frames = $Frames
    verification_cases = $VerifyCases
    machine = [Environment]::OSVersion.VersionString
    processor = $env:PROCESSOR_IDENTIFIER
}
[IO.File]::WriteAllText((Join-Path $output 'metadata.json'), ($metadata | ConvertTo-Json -Depth 4), $utf8)
$executable = Join-Path $output 'cpu-bench.exe'
& rustc --edition 2021 -C opt-level=3 -C codegen-units=1 -D warnings (Join-Path $output 'main.rs') -o $executable
if ($LASTEXITCODE -ne 0) { throw 'Benchmark compilation failed' }
$mode = if ($VerifyOnly) { 'verify' } else { 'measure' }
$ErrorActionPreference = 'Continue'
& $executable $mode $VerifyCases $Rounds $Frames 2> (Join-Path $output 'verification.log') |
    Tee-Object -FilePath (Join-Path $output 'results.csv')
$runExit = $LASTEXITCODE
$ErrorActionPreference = 'Stop'
if ($runExit -ne 0) { throw 'Byte equivalence or benchmark execution failed; see verification.log.' }
Write-Host "Evidence: $output"
