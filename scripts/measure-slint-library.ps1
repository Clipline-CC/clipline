<#
.SYNOPSIS
    Captures bounded, process-tree measurements for the native Slint Library.

.DESCRIPTION
    Builds an exact 50, 500, or 2,000-clip hard-linked corpus in a disposable
    profile before launching catalog_harness. The sampler owns only the
    launched PID plus creation-time-verified descendants. Other Clipline
    processes are excluded and recorded; they are never attached to, stopped,
    or used as a reason to abort.

    The catalog harness contract is:

      catalog_harness.exe --fixture-root <directory> --fixture-seed-root <directory> `
        --source-fixture <file> `
        --clip-count <50|500|2000> --scenario <scenario> `
        --marker-path <jsonl> --stop-path <file> --exercise-path <file> `
        --telemetry-path <json>

    It publishes ready, pageSettled, postersSettled, exerciseSettled, error, and stop markers.
    Final telemetry is published only by create-new atomic rename and carries
    publication="create-new-atomic-rename".

    A publishable absolute gate uses at least three accepted five-minute
    samples. Rejected attempts remain as evidence and never count toward that
    total. Set -AcceptedSamples 1 and a shorter -SteadySeconds only for smoke
    diagnostics; the resulting series is explicitly non-publishable.

.EXAMPLE
    cargo build --manifest-path apps/clipline-slint-spike/Cargo.toml `
      --example catalog_harness --profile benchmark
    ./scripts/measure-slint-library.ps1 `
      -Exe apps/clipline-slint-spike/target/benchmark/examples/catalog_harness.exe `
      -FixturesDir fixtures/playback -ClipCount 2000 -Scenario local-cold `
      -AcceptedSamples 3 -SteadySeconds 300
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [Parameter(Mandatory = $true)][string]$FixturesDir,
    [string]$FixturePath = 'h264-one-opus-3s.mp4',
    [string]$FfmpegPath,
    [Parameter(Mandatory = $true)][ValidateSet(50, 500, 2000)][int]$ClipCount,
    [Parameter(Mandatory = $true)]
    [ValidateSet('local-cold', 'local-warm', 'cloud-pages', 'selection-page-churn', 'reveal-close-100')]
    [string]$Scenario,
    [ValidateRange(1, 10)][int]$AcceptedSamples = 3,
    [ValidateRange(1, 30)][int]$MaximumAttempts = 9,
    [ValidateRange(0, 3600)][int]$WarmupSeconds = 30,
    [ValidateRange(1, 3600)][int]$SteadySeconds = 300,
    [ValidateRange(100, 60000)][int]$SampleIntervalMs = 1000,
    [ValidateRange(5, 3600)][int]$ReadinessTimeoutSeconds = 600,
    [ValidateRange(5, 600)][int]$ShutdownGraceSeconds = 30,
    [ValidateRange(0.0, 100.0)][double]$MaxBackgroundCpuPercent = 10.0,
    [ValidateRange(0.0, 1.0)][double]$MaxNoisySampleRatio = 0.05,
    [string]$Renderer = 'winit-software',
    [switch]$AllowNonBenchmarkBuild,
    [string]$OutputDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:HarnessVersion = '1.0.0'
$script:SchemaVersion = 1
$script:MaximumSamples = 40000
$script:MaximumRawRows = 500000
$script:MaximumTreeProcesses = 64
$script:MaximumObservedIdentities = 4096
$script:MaximumTelemetryBytes = 1MB
$script:MaximumMarkerBytes = 4MB
$script:MaximumMarkers = 4096
$script:MaximumMarkerLineBytes = 64KB
$script:ScriptRoot = Split-Path -Parent $PSCommandPath
$script:RepoRoot = Split-Path -Parent $script:ScriptRoot
Import-Module (Join-Path $script:ScriptRoot 'lib/Clipline.ProcessMetrics.psm1') -Force

function Quote-LibraryArgument {
    param([Parameter(Mandatory = $true)][string]$Value)
    if ($Value.Contains('"')) { throw "runner argument contains an unsupported quote: $Value" }
    return '"' + $Value + '"'
}

function Write-CliplineCreateNewText {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Text
    )
    $stream = [System.IO.File]::Open(
        $Path,
        [System.IO.FileMode]::CreateNew,
        [System.IO.FileAccess]::Write,
        [System.IO.FileShare]::None
    )
    try {
        $bytes = [System.Text.UTF8Encoding]::new($false).GetBytes($Text)
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush($true)
    } finally {
        $stream.Dispose()
    }
}

function Publish-CliplineCreateNewSignal {
    param([Parameter(Mandatory = $true)][string]$Path)
    if (Test-Path -LiteralPath $Path) { throw "signal target already exists: $Path" }
    $temporary = "$Path.tmp.$([guid]::NewGuid().ToString('N'))"
    try {
        Write-CliplineCreateNewText -Path $temporary -Text ''
        [System.IO.File]::Move($temporary, $Path)
    } finally {
        if (Test-Path -LiteralPath $temporary) {
            Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
        }
    }
}

function Test-LibraryGitStatusEntryAllowed {
    param([Parameter(Mandatory = $true)][string]$Entry)
    if (-not $Entry.StartsWith('?? ', [StringComparison]::Ordinal)) { return $false }
    $path = $Entry.Substring(3).Trim().Trim('"').Replace('\', '/')
    return $path.Equals('paseo.json', [StringComparison]::OrdinalIgnoreCase) -or
        $path.StartsWith('artifacts/', [StringComparison]::OrdinalIgnoreCase)
}

function Resolve-LibraryFixture {
    param(
        [Parameter(Mandatory = $true)][string]$Directory,
        [Parameter(Mandatory = $true)][string]$RequestedPath
    )
    $resolvedDirectory = (Resolve-Path -LiteralPath $Directory -ErrorAction Stop).Path
    $manifestPath = Join-Path $resolvedDirectory 'manifest.json'
    if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
        throw "fixture manifest is missing: $manifestPath"
    }
    $candidate = if ([System.IO.Path]::IsPathRooted($RequestedPath)) {
        $RequestedPath
    } else {
        Join-Path $resolvedDirectory $RequestedPath
    }
    $resolved = (Resolve-Path -LiteralPath $candidate -ErrorAction Stop).Path
    if (-not (Test-Path -LiteralPath $resolved -PathType Leaf)) {
        throw "fixture is not a file: $resolved"
    }
    $manifest = Get-Content -LiteralPath $manifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
    if ([int]$manifest.schema_version -ne 1 -or
        [string]$manifest.suite -ne 'clipline-native-playback-v1') {
        throw 'fixture manifest has an unsupported schema or suite'
    }
    $entries = @($manifest.fixtures) + @($manifest.production_mux_oracles)
    $entry = @($entries | Where-Object {
        [string]$_.file -eq [System.IO.Path]::GetFileName($resolved)
    }) | Select-Object -First 1
    if (-not $entry -or -not $entry.artifact -or
        [string]::IsNullOrWhiteSpace([string]$entry.artifact.sha256)) {
        throw "fixture is not hash-covered by manifest.json: $resolved"
    }
    $actual = (Get-FileHash -LiteralPath $resolved -Algorithm SHA256).Hash.ToLowerInvariant()
    $expected = ([string]$entry.artifact.sha256).ToLowerInvariant()
    if ($actual -ne $expected) {
        throw "fixture hash does not match manifest.json: $resolved"
    }
    return [pscustomobject][ordered]@{
        directory = $resolvedDirectory
        manifest = $manifestPath
        path = $resolved
        sha256 = $actual
        bytes = [long](Get-Item -LiteralPath $resolved).Length
    }
}

function Initialize-LibraryFixtureRoot {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][object]$Fixture,
        [Parameter(Mandatory = $true)][int]$Count
    )
    if (Test-Path -LiteralPath $Root) { throw "fixture root already exists: $Root" }
    $linksPerSeed = 500
    $seedCount = [int][math]::Ceiling($Count / [double]$linksPerSeed)
    $seedRoot = Join-Path (Split-Path -Parent $Root) 'FixtureSeeds'
    if (Test-Path -LiteralPath $seedRoot) { throw "fixture seed root already exists: $seedRoot" }
    New-Item -ItemType Directory -Path $seedRoot | Out-Null
    $seeds = New-Object System.Collections.Generic.List[string]
    for ($seedIndex = 0; $seedIndex -lt $seedCount; $seedIndex++) {
        $seed = Join-Path $seedRoot ('seed-{0:D2}.mp4' -f $seedIndex)
        [System.IO.File]::Copy($Fixture.path, $seed, $false)
        if ((Get-FileHash -LiteralPath $seed -Algorithm SHA256).Hash.ToLowerInvariant() -ne
            $Fixture.sha256) {
            throw "fixture seed $($seedIndex + 1) failed SHA-256 verification"
        }
        $seeds.Add($seed)
    }
    New-Item -ItemType Directory -Path $Root | Out-Null
    for ($index = 0; $index -lt $Count; $index++) {
        $destination = Join-Path $Root ('clip-{0:D5}.mp4' -f $index)
        $seedIndex = [int][math]::Floor($index / [double]$linksPerSeed)
        try {
            New-Item -ItemType HardLink -Path $destination -Target $seeds[$seedIndex] `
                -ErrorAction Stop |
                Out-Null
        } catch {
            throw "could not hard-link fixture $($index + 1) of ${Count}: $($_.Exception.Message)"
        }
        if ([long](Get-Item -LiteralPath $destination).Length -ne [long]$Fixture.bytes) {
            throw "hard-linked fixture $($index + 1) has the wrong size"
        }
    }
    $files = @(Get-ChildItem -LiteralPath $Root -File -Filter '*.mp4')
    if ($files.Count -ne $Count) {
        throw "fixture root contains $($files.Count) MP4 files; expected exactly $Count"
    }
    return [pscustomobject][ordered]@{
        count = $Count
        sourcePath = $Fixture.path
        sourceSha256 = $Fixture.sha256
        sourceBytes = $Fixture.bytes
        root = $Root
        seedRoot = $seedRoot
        seedCount = $seedCount
        maximumLinksPerSeed = $linksPerSeed
        constructionCompletedUtc = [datetime]::UtcNow.ToString('o')
        allSeedsHashVerified = $true
        allLinksIdentityVerifiedByHarness = $true
    }
}

function Initialize-LibraryWarmPosterCache {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][object]$Fixture,
        [Parameter(Mandatory = $true)][string]$Ffmpeg
    )
    $seed = Join-Path $Root '.warm-poster-seed.jpg'
    $arguments = @(
        '-hide_banner', '-loglevel', 'error', '-nostdin', '-ss', '1.000',
        '-i', $Fixture.path, '-frames:v', '1', '-vf', 'scale=480:-2:flags=bicubic',
        '-q:v', '4', '-c:v', 'mjpeg', '-f', 'image2', $seed
    )
    & $Ffmpeg @arguments
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $seed -PathType Leaf) -or
        (Get-Item -LiteralPath $seed).Length -le 4) {
        throw 'ffmpeg could not pre-generate the owned warm poster cache'
    }
    try {
        for ($index = 0; $index -lt 32; $index++) {
            Copy-Item -LiteralPath $seed -Destination `
                (Join-Path $Root ('clip-{0:D5}.poster.jpg' -f $index))
        }
    } finally {
        Remove-Item -LiteralPath $seed -Force -ErrorAction SilentlyContinue
    }
    return 32
}

function Get-LibrarySystemCpuPercent {
    try {
        $row = Get-CimInstance Win32_PerfFormattedData_PerfOS_Processor `
            -Filter "Name='_Total'" -ErrorAction Stop
        if (-not $row) { throw 'the _Total processor row is missing' }
        $value = [double]$row.PercentProcessorTime
        if ($value -lt 0.0 -or $value -gt 100.0) { throw "invalid system CPU value: $value" }
        return [pscustomobject]@{ Value = $value; Error = $null }
    } catch {
        return [pscustomobject]@{ Value = $null; Error = $_.Exception.Message }
    }
}

function Add-LibraryConcurrentProcesses {
    param(
        [Parameter(Mandatory = $true)][object[]]$Rows,
        [Parameter(Mandatory = $true)][hashtable]$TreeIds,
        [Parameter(Mandatory = $true)][string]$RootName,
        [Parameter(Mandatory = $true)][hashtable]$Observed
    )
    $names = @('clipline-app.exe', 'clipline.exe', 'clipline-slint-spike.exe',
        'catalog_harness.exe', $RootName)
    $now = [datetime]::UtcNow.ToString('o')
    foreach ($row in $Rows) {
        $processId = [int]$row.ProcessId
        if ($TreeIds.ContainsKey($processId) -or [string]$row.Name -notin $names -or
            -not $row.CreationDate) { continue }
        $creation = ([datetime]$row.CreationDate).ToUniversalTime()
        $identity = "$processId|$($creation.Ticks)"
        if ($Observed.ContainsKey($identity)) {
            $Observed[$identity].lastObservedUtc = $now
            continue
        }
        $Observed[$identity] = [pscustomobject][ordered]@{
            processId = $processId
            parentProcessId = [int]$row.ParentProcessId
            name = [string]$row.Name
            creationUtc = $creation.ToString('o')
            firstObservedUtc = $now
            lastObservedUtc = $now
        }
        if ($Observed.Count -gt $script:MaximumObservedIdentities) {
            throw 'concurrent process provenance exceeded its identity bound'
        }
    }
}

function Get-LibraryProcessSample {
    param(
        [Parameter(Mandatory = $true)][int]$RootProcessId,
        [Parameter(Mandatory = $true)][datetime]$RootStartUtc,
        [Parameter(Mandatory = $true)][string]$RootProcessName,
        [Parameter(Mandatory = $true)][string]$RunId,
        [Parameter(Mandatory = $true)][string]$RendererName,
        [Parameter(Mandatory = $true)][string]$ScenarioName,
        [Parameter(Mandatory = $true)][string]$Phase,
        [Parameter(Mandatory = $true)][long]$ElapsedMs,
        [Parameter(Mandatory = $true)][hashtable]$PreviousCpu,
        [Parameter(Mandatory = $true)][hashtable]$ObservedTree,
        [Parameter(Mandatory = $true)][hashtable]$ConcurrentProcesses
    )
    $all = @(Get-CimInstance Win32_Process)
    $root = $all | Where-Object { [int]$_.ProcessId -eq $RootProcessId } | Select-Object -First 1
    if (-not $root) { return $null }
    if ([string]$root.Name -ne $RootProcessName) { throw 'measured root PID changed process name' }
    $actualStart = ([datetime]$root.CreationDate).ToUniversalTime()
    if ([math]::Abs(($actualStart - $RootStartUtc).TotalSeconds) -gt 2.0) {
        throw 'measured root PID was reused'
    }
    $descendants = @(Get-CliplineDescendantProcesses -RootProcessId $RootProcessId `
        -RootStart $RootStartUtc.ToLocalTime() -ProcessRows $all)
    $tree = @($root) + $descendants
    if ($tree.Count -gt $script:MaximumTreeProcesses) { throw 'measured process tree exceeded 64' }
    $treeIds = @{}
    foreach ($row in $tree) {
        $id = [int]$row.ProcessId
        $created = ([datetime]$row.CreationDate).ToUniversalTime()
        $treeIds[$id] = $true
        $ObservedTree["$id|$($created.Ticks)"] = [pscustomobject][ordered]@{
            processId = $id; name = [string]$row.Name; creationUtc = $created.ToString('o')
        }
        if ($ObservedTree.Count -gt $script:MaximumObservedIdentities) {
            throw 'owned process identity history exceeded its bound'
        }
    }
    Add-LibraryConcurrentProcesses -Rows $all -TreeIds $treeIds -RootName $RootProcessName `
        -Observed $ConcurrentProcesses

    $metrics = New-Object System.Collections.Generic.List[object]
    $childFailures = 0
    foreach ($row in $tree) {
        $id = [int]$row.ProcessId
        $isRoot = $id -eq $RootProcessId
        try {
            $created = ([datetime]$row.CreationDate).ToUniversalTime()
            $live = Get-Process -Id $id -ErrorAction Stop
            if ([math]::Abs(($live.StartTime.ToUniversalTime() - $created).TotalSeconds) -gt 2.0) {
                throw "PID $id was reused during metric read"
            }
            $snapshot = Get-CliplineNativeProcessSnapshot -ProcessId $id
            $identity = "$id|$($created.Ticks)"
            $cpu = 0.0
            if ($PreviousCpu.ContainsKey($identity)) {
                $previous = $PreviousCpu[$identity]
                $cpu = Get-CliplineCpuPercent -PreviousTime100ns $previous.CpuTime100ns `
                    -CurrentTime100ns $snapshot.CpuTime100ns `
                    -ElapsedMs ([math]::Max(1, $ElapsedMs - $previous.ElapsedMs)) `
                    -LogicalProcessorCount ([Environment]::ProcessorCount)
            }
            $PreviousCpu[$identity] = [pscustomobject]@{
                CpuTime100ns = [long]$snapshot.CpuTime100ns; ElapsedMs = $ElapsedMs
            }
            $metrics.Add([pscustomobject]@{
                Cim = $row; Snapshot = $snapshot; IsRoot = $isRoot; CpuPercent = [double]$cpu
            })
        } catch {
            if ($isRoot) { throw "strict root metric read failed: $($_.Exception.Message)" }
            $childFailures++
        }
    }
    if (-not ($metrics | Where-Object IsRoot)) { throw 'strict root metric row is missing' }
    $pws = [long](($metrics | ForEach-Object Snapshot | Measure-Object PrivateWorkingSetBytes -Sum).Sum)
    $commit = [long](($metrics | ForEach-Object Snapshot | Measure-Object PrivateCommitBytes -Sum).Sum)
    $working = [long](($metrics | ForEach-Object Snapshot | Measure-Object WorkingSetBytes -Sum).Sum)
    $cpuTotal = [double](($metrics | Measure-Object CpuPercent -Sum).Sum)
    $handles = [long](($metrics | ForEach-Object Snapshot | Measure-Object HandleCount -Sum).Sum)
    $threads = [long](($metrics | ForEach-Object Snapshot | Measure-Object ThreadCount -Sum).Sum)
    $gpu = Get-CliplineGpuProcessMemory -ProcessIds @($metrics | ForEach-Object { [int]$_.Cim.ProcessId })
    $systemCpu = Get-LibrarySystemCpuPercent
    $background = if ($null -eq $systemCpu.Value) { $null } else {
        [math]::Max(0.0, [double]$systemCpu.Value - $cpuTotal)
    }
    $sampleUtc = [datetime]::UtcNow.ToString('o')
    $rows = @($metrics | ForEach-Object {
        [pscustomobject][ordered]@{
            RunId = $RunId; Frontend = 'slint'; Renderer = $RendererName; Scenario = $ScenarioName
            Phase = $Phase; SampleUtc = $sampleUtc; ElapsedMs = $ElapsedMs
            ProcessId = [int]$_.Cim.ProcessId; ParentProcessId = [int]$_.Cim.ParentProcessId
            ProcessName = [string]$_.Cim.Name
            ProcessRole = Get-CliplineProcessRole -ProcessName $_.Cim.Name `
                -CommandLine $_.Cim.CommandLine -IsRoot $_.IsRoot
            IsRoot = [bool]$_.IsRoot
            PrivateWorkingSetBytes = [long]$_.Snapshot.PrivateWorkingSetBytes
            PrivateCommitBytes = [long]$_.Snapshot.PrivateCommitBytes
            WorkingSetBytes = [long]$_.Snapshot.WorkingSetBytes
            CpuTime100ns = [long]$_.Snapshot.CpuTime100ns
            CpuPercent = [math]::Round([double]$_.CpuPercent, 4)
            HandleCount = [long]$_.Snapshot.HandleCount; ThreadCount = [long]$_.Snapshot.ThreadCount
            TreePrivateWorkingSetBytes = $pws; TreePrivateCommitBytes = $commit
            TreeWorkingSetBytes = $working; TreeCpuPercent = [math]::Round($cpuTotal, 4)
            TreeHandleCount = $handles; TreeThreadCount = $threads; TreeProcessCount = $metrics.Count
            ChildReadFailures = $childFailures; GpuCountersAvailable = [bool]$gpu.Available
            GpuLocalBytes = if ($gpu.Available) { [long]$gpu.LocalBytes } else { $null }
            GpuNonLocalBytes = if ($gpu.Available) { [long]$gpu.NonLocalBytes } else { $null }
        }
    })
    return [pscustomobject][ordered]@{
        Rows = $rows
        Aggregate = [pscustomobject][ordered]@{
            SampleUtc = $sampleUtc; Phase = $Phase; TreePrivateWorkingSetBytes = $pws
            TreePrivateCommitBytes = $commit; TreeWorkingSetBytes = $working
            TreeCpuPercent = $cpuTotal; TreeHandleCount = $handles; TreeThreadCount = $threads
            TreeProcessCount = $metrics.Count; ChildReadFailures = $childFailures
            GpuLocalBytes = if ($gpu.Available) { [long]$gpu.LocalBytes } else { $null }
            GpuNonLocalBytes = if ($gpu.Available) { [long]$gpu.NonLocalBytes } else { $null }
            SystemCpuPercent = $systemCpu.Value; BackgroundCpuPercent = $background
            SystemCpuReadError = $systemCpu.Error
            FfmpegProcessCount = @($metrics | Where-Object { $_.Cim.Name -eq 'ffmpeg.exe' }).Count
        }
    }
}

function Get-LibraryMarkerState {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][datetime]$RootStartUtc,
        [Parameter(Mandatory = $true)][object]$Contract,
        [switch]$ProducerExited
    )
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return @() }
    $item = Get-Item -LiteralPath $Path
    if ($item.Length -gt $script:MaximumMarkerBytes) { throw 'marker file exceeded 4 MiB' }
    [string]$raw = Get-Content -LiteralPath $Path -Raw -Encoding UTF8
    if ([string]::IsNullOrEmpty($raw)) { return @() }
    $lines = @($raw -split "`r?`n")
    if (-not $ProducerExited -and $raw.Length -gt 0 -and $raw -notmatch "(`r`n|`n)$") {
        $lines = @($lines | Select-Object -First ([math]::Max(0, $lines.Count - 1)))
    }
    $markers = New-Object System.Collections.Generic.List[object]
    foreach ($line in $lines) {
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        if ([System.Text.Encoding]::UTF8.GetByteCount($line) -gt $script:MaximumMarkerLineBytes) {
            throw 'marker line exceeded 64 KiB'
        }
        try { $marker = $line | ConvertFrom-Json -ErrorAction Stop } catch {
            throw "malformed completed marker: $($_.Exception.Message)"
        }
        if ([int]$marker.schemaVersion -ne 1 -or
            [string]$marker.kind -notin @(
                'ready', 'pageSettled', 'postersSettled', 'exerciseSettled', 'error', 'stop')) {
            throw 'marker has unsupported schema or kind'
        }
        $timestamp = ([datetime]$marker.timestampUtc).ToUniversalTime()
        if ($timestamp -lt $RootStartUtc -or $timestamp -gt [datetime]::UtcNow.AddSeconds(5)) {
            throw 'marker timestamp is outside the owned process lifetime'
        }
        $expectedStartUnixMs = ([datetimeoffset]$RootStartUtc).ToUnixTimeMilliseconds()
        if (-not $marker.provenance -or
            [int]$marker.provenance.processId -ne [int]$Contract.processId -or
            [string]$marker.provenance.processName -ne 'catalog_harness' -or
            [math]::Abs([double]$marker.provenance.processStartUnixMs - $expectedStartUnixMs) -gt 2000 -or
            [string]$marker.provenance.buildSha -ne [string]$Contract.buildSha -or
            [string]$marker.provenance.renderer -ne [string]$Contract.renderer -or
            [string]$marker.provenance.adapter -ne [string]$Contract.adapter -or
            [math]::Abs([double]$marker.provenance.scale - [double]$Contract.scale) -gt 0.0001 -or
            ([string]$marker.provenance.sourceSha256).ToLowerInvariant() -ne
                [string]$Contract.sourceSha256 -or
            -not [System.IO.Path]::GetFullPath([string]$marker.provenance.fixtureRoot).Equals(
                [string]$Contract.fixtureRoot, [StringComparison]::OrdinalIgnoreCase) -or
            -not [System.IO.Path]::GetFullPath([string]$marker.provenance.fixtureSeedRoot).Equals(
                [string]$Contract.fixtureSeedRoot, [StringComparison]::OrdinalIgnoreCase)) {
            throw 'marker provenance does not match the owned process/build/fixture contract'
        }
        $markers.Add($marker)
        if ($markers.Count -gt $script:MaximumMarkers) { throw 'marker count exceeded 4096' }
    }
    $errorMarker = @($markers | Where-Object kind -eq 'error') | Select-Object -First 1
    if ($errorMarker) { throw "catalog harness failed: $($errorMarker.detail)" }
    return @($markers.ToArray())
}

function Get-LibraryMetricSummary {
    param([object[]]$Samples, [string]$Property, [switch]$Optional)
    $values = @($Samples | ForEach-Object { $_.$Property } | Where-Object { $null -ne $_ })
    if ($values.Count -eq 0) {
        if ($Optional) { return $null }
        throw "steady metric is empty: $Property"
    }
    return [pscustomobject][ordered]@{
        p50 = Get-CliplineMedian $values
        p95 = Get-CliplinePercentile $values 0.95
        max = [double](($values | Measure-Object -Maximum).Maximum)
    }
}

function Stop-LibraryOwnedTree {
    param([hashtable]$ObservedTree, [int]$RootProcessId)
    foreach ($record in @($ObservedTree.Values | Sort-Object {
        if ([int]$_.processId -eq $RootProcessId) { 1 } else { 0 }
    })) {
        try {
            $live = Get-Process -Id ([int]$record.processId) -ErrorAction Stop
            $expected = ([datetime]$record.creationUtc).ToUniversalTime()
            if ([math]::Abs(($live.StartTime.ToUniversalTime() - $expected).TotalSeconds) -le 2.0) {
                Stop-Process -Id ([int]$record.processId) -Force -ErrorAction SilentlyContinue
            }
        } catch { }
    }
}

function Get-LibraryLiveOwnedDescendants {
    param([hashtable]$ObservedTree, [int]$RootProcessId)
    $result = New-Object System.Collections.Generic.List[object]
    foreach ($record in $ObservedTree.Values) {
        if ([int]$record.processId -eq $RootProcessId) { continue }
        try {
            $live = Get-Process -Id ([int]$record.processId) -ErrorAction Stop
            $expected = ([datetime]$record.creationUtc).ToUniversalTime()
            if ([math]::Abs(($live.StartTime.ToUniversalTime() - $expected).TotalSeconds) -le 2.0) {
                $result.Add($record)
            }
        } catch { }
    }
    return @($result.ToArray())
}

function Update-LibraryObservedDescendants {
    param(
        [Parameter(Mandatory = $true)][hashtable]$ObservedTree,
        [Parameter(Mandatory = $true)][int]$RootProcessId,
        [Parameter(Mandatory = $true)][datetime]$RootStartUtc
    )
    $all = @(Get-CimInstance Win32_Process)
    foreach ($row in @(Get-CliplineDescendantProcesses -RootProcessId $RootProcessId `
        -RootStart $RootStartUtc.ToLocalTime() -ProcessRows $all)) {
        if (-not $row.CreationDate) { continue }
        $created = ([datetime]$row.CreationDate).ToUniversalTime()
        $id = [int]$row.ProcessId
        $ObservedTree["$id|$($created.Ticks)"] = [pscustomobject][ordered]@{
            processId = $id; name = [string]$row.Name; creationUtc = $created.ToString('o')
        }
    }
}

function Assert-LibraryTelemetry {
    param(
        [Parameter(Mandatory = $true)][object]$Telemetry,
        [Parameter(Mandatory = $true)][object]$Fixture,
        [Parameter(Mandatory = $true)][int]$Count,
        [Parameter(Mandatory = $true)][string]$ScenarioName,
        [Parameter(Mandatory = $true)][string]$RendererName,
        [Parameter(Mandatory = $true)][object]$Contract
    )
    if ([int]$Telemetry.schemaVersion -ne 1 -or [string]$Telemetry.status -ne 'completed' -or
        [string]$Telemetry.scenario -ne $ScenarioName -or [int]$Telemetry.clipCount -ne $Count) {
        throw 'telemetry envelope does not match this run'
    }
    if ([string]$Telemetry.publication -ne 'create-new-atomic-rename') {
        throw 'telemetry did not attest create-new atomic-rename publication'
    }
    if (-not $Telemetry.provenance -or
        [int]$Telemetry.provenance.processId -ne [int]$Contract.processId -or
        [string]$Telemetry.provenance.processName -ne 'catalog_harness' -or
        [string]$Telemetry.provenance.buildSha -ne [string]$Contract.buildSha -or
        [string]$Telemetry.provenance.renderer -ne $RendererName -or
        [string]$Telemetry.provenance.adapter -ne [string]$Contract.adapter -or
        [math]::Abs([double]$Telemetry.provenance.scale - [double]$Contract.scale) -gt 0.0001 -or
        -not [System.IO.Path]::GetFullPath([string]$Telemetry.provenance.fixtureSeedRoot).Equals(
            [string]$Contract.fixtureSeedRoot, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'telemetry provenance does not match the measured process/build/environment'
    }
    if (-not $Telemetry.sourceFixture -or
        ([string]$Telemetry.sourceFixture.sha256).ToLowerInvariant() -ne $Fixture.sha256 -or
        -not [System.IO.Path]::GetFullPath([string]$Telemetry.sourceFixture.path).Equals(
            $Fixture.path, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'telemetry source fixture does not match the hash-verified oracle'
    }
    $m = $Telemetry.metrics
    $l = $Telemetry.lifecycle
    $safety = $Telemetry.safety
    if (-not $m -or -not $l -or -not $safety) {
        throw 'telemetry metrics/lifecycle/safety object is missing'
    }
    if ([bool]$safety.productionCredentialsLoaded -or [long]$safety.cloudNetworkRequests -ne 0) {
        throw 'catalog harness touched credentials or a real Cloud network endpoint'
    }
    if ($m.windowShownModelPublished -ne $true) {
        throw 'initial bounded model was not published into a shown Slint window'
    }
    foreach ($field in @('firstUsablePageMs', 'pageChangeP95Ms', 'filterGroupP95Ms',
        'posterSettleMs',
        'retainedRows', 'retainedDecodedImages', 'posterLruEntries', 'posterCacheSize',
        'ffmpegChildPeak',
        'duplicateSameKeyExtractions', 'posterExtractionStarts', 'singleFlightFollowers',
        'offPageDecodedImagesAfterSettle', 'stalePublications',
        'activeLeasesAfterClose', 'offPageModelImagesAfterSettle')) {
        if ($null -eq $m.$field) { throw "telemetry metric is missing: $field" }
        if ([double]$m.$field -lt 0.0) { throw "telemetry metric is negative: $field" }
    }
    if ([long]$m.retainedRows -gt 60 -or [long]$m.retainedDecodedImages -gt 32 -or
        [long]$m.posterLruEntries -gt 120 -or [long]$m.ffmpegChildPeak -gt 2 -or
        [long]$m.duplicateSameKeyExtractions -ne 0 -or
        [long]$m.offPageDecodedImagesAfterSettle -ne 0 -or
        [long]$m.offPageModelImagesAfterSettle -ne 0 -or [long]$m.stalePublications -ne 0 -or
        [long]$m.activeLeasesAfterClose -ne 0) {
        throw 'one or more absolute Library telemetry bounds failed'
    }
    $expectedPageImages = [math]::Min($Count, 32)
    if ([long]$l.posterHandlesAccepted -ne $expectedPageImages -or
        [long]$m.posterCacheSize -lt $expectedPageImages) {
        throw 'poster extraction/decode did not populate the bounded active image window'
    }
    if ($ScenarioName -eq 'local-warm') {
        if ([long]$m.ffmpegChildPeak -ne 0 -or [long]$m.posterExtractionStarts -ne 0 -or
            [long]$m.singleFlightFollowers -ne 0) {
            throw 'local-warm unexpectedly spawned an FFmpeg poster extraction'
        }
    } elseif ([long]$m.ffmpegChildPeak -lt 1 -or
        [long]$m.posterExtractionStarts -ne $expectedPageImages -or
        [long]$m.singleFlightFollowers -lt 1) {
        throw 'cold poster scenario did not prove bounded same-key extraction work'
    }
    foreach ($pair in @(
        @('attachmentsCreated', 'attachmentsDropped'), @('imagesAccepted', 'imagesReleased'),
        @('posterHandlesAccepted', 'posterHandlesReleased'),
        @('modelImagesPublished', 'modelImagesReplaced'),
        @('leasesAcquired', 'leasesReleased')
    )) {
        if ($null -eq $l.($pair[0]) -or $null -eq $l.($pair[1]) -or
            [long]$l.($pair[0]) -ne [long]$l.($pair[1])) {
            throw "lifecycle counter is missing or unbalanced: $($pair -join '/')"
        }
    }
    if ([long]$l.imagesAccepted -ne
            ([long]$l.posterHandlesAccepted + [long]$l.modelImagesPublished) -or
        [long]$l.imagesReleased -ne
            ([long]$l.posterHandlesReleased + [long]$l.modelImagesReplaced)) {
        throw 'aggregate image lifecycle counters do not equal their owned subcategories'
    }
    if ($Count -eq 2000 -and [double]$m.firstUsablePageMs -gt 1500.0) {
        throw 'first usable 2,000-clip page exceeded 1.5 seconds'
    }
    if ($m.pwsGrowthMeasuredExternally -ne $true -or $null -ne $m.pwsGrowthBytes) {
        throw 'harness must leave pwsGrowthBytes null and mark pwsGrowthMeasuredExternally=true'
    }
    if ($ScenarioName -eq 'selection-page-churn') {
        foreach ($field in @('localCloudPageSwitches', 'posterCancellations', 'uploadProgressBursts')) {
            if ([long]$Telemetry.churn.$field -ne 100) {
                throw "$field must equal 100 in selection-page-churn"
            }
        }
        if ($Telemetry.churn.executedDuringMeasuredWindow -ne $true) {
            throw 'selection/page churn was not executed inside the sampled steady window'
        }
    }
    if ($ScenarioName -eq 'reveal-close-100') {
        $windowPendingProperty = `
            $Telemetry.reveal.PSObject.Properties['windowRevealCloseCyclesPending']
        $windowPending = $null -ne $windowPendingProperty -and [bool]$windowPendingProperty.Value
        if ($windowPending -or [long]$Telemetry.reveal.windowRevealCloseCycles -ne 100) {
            throw 'window reveal/close cycles must equal 100'
        }
        if ($Telemetry.reveal.windowCyclesExecutedDuringMeasuredWindow -ne $true) {
            throw 'window reveal/close cycles did not run inside the sampled steady window'
        }
        if ([bool]$Telemetry.reveal.cloudMediaCyclesPending -or
            [long]$Telemetry.reveal.cloudMediaCycles -ne 100 -or
            [long]$Telemetry.reveal.cloudMediaOpens -ne 100 -or
            [long]$Telemetry.reveal.cloudMediaReplacements -ne 100 -or
            [long]$Telemetry.reveal.cloudMediaCloses -ne 100 -or
            [long]$Telemetry.reveal.cloudMediaCacheFills -ne 2 -or
            $Telemetry.reveal.cloudMediaCyclesExecutedDuringMeasuredWindow -ne $true -or
            [long]$l.leasesAcquired -ne 200 -or [long]$l.leasesReleased -ne 200) {
            throw 'cloud media lifecycle must prove 100 exact measured open/replace/close cycles'
        }
    }
}

function Invoke-LibrarySample {
    param([int]$Attempt, [string]$OutputRoot, [object]$Fixture)
    $runId = '{0}-slint-library-{1}-{2}-{3}' -f ([datetime]::UtcNow.ToString('yyyyMMddTHHmmssZ')),
        $ClipCount, $Scenario, ([guid]::NewGuid().ToString('N').Substring(0, 8))
    $profileRoot = Join-Path $OutputRoot "profiles/$runId"
    $appData = Join-Path $profileRoot 'AppData/Roaming'
    $localAppData = Join-Path $profileRoot 'AppData/Local'
    $tempRoot = Join-Path $profileRoot 'Temp'
    foreach ($directory in @($appData, $localAppData, $tempRoot)) {
        New-Item -ItemType Directory -Path $directory -Force | Out-Null
    }
    $fixtureRoot = Join-Path $profileRoot 'Videos/Clipline'
    $corpus = Initialize-LibraryFixtureRoot -Root $fixtureRoot -Fixture $Fixture -Count $ClipCount
    $corpus | Add-Member -NotePropertyName posterFfmpegPath -NotePropertyValue $resolvedFfmpeg
    $corpus | Add-Member -NotePropertyName posterFfmpegSha256 -NotePropertyValue $ffmpegHash
    $warmPosterCount = 0
    if ($Scenario -eq 'local-warm') {
        $warmPosterCount = Initialize-LibraryWarmPosterCache -Root $fixtureRoot -Fixture $Fixture `
            -Ffmpeg $resolvedFfmpeg
        $corpus | Add-Member -NotePropertyName warmPosterCount -NotePropertyValue $warmPosterCount
    }
    $markerPath = Join-Path $profileRoot 'catalog-markers.jsonl'
    $stopPath = Join-Path $profileRoot 'catalog.stop'
    $exercisePath = Join-Path $profileRoot 'catalog.exercise'
    $telemetryPath = Join-Path $profileRoot 'catalog.telemetry.json'
    $rawPath = Join-Path $OutputRoot "$runId.raw.csv"
    $provenancePath = Join-Path $OutputRoot "$runId.provenance.json"
    foreach ($path in @(
        $markerPath, $stopPath, $exercisePath, $telemetryPath, $rawPath, $provenancePath)) {
        if (Test-Path -LiteralPath $path) { throw "create-new evidence path already exists: $path" }
    }

    $rows = New-Object System.Collections.Generic.List[object]
    $samples = New-Object System.Collections.Generic.List[object]
    $previousCpu = @{}
    $observedTree = @{}
    $concurrent = @{}
    $root = $null
    $rootStart = $null
    $rootName = [System.IO.Path]::GetFileName($resolvedExe)
    $started = [datetime]::UtcNow
    $ended = $null
    $failure = $null
    $runnerTelemetry = $null
    $exitCode = $null
    $clock = $null
    $readyMarkers = @()
    $maxObservedFfmpeg = 0
    $launchError = $null
    $cloudMediaPending = $false
    $windowLifecyclePending = $false
    $exerciseRequired = $Scenario -in @('selection-page-churn', 'reveal-close-100')
    $exerciseSignaled = $false
    $exerciseSignalUtc = $null
    $exerciseSettledUtc = $null

    $oldAppData = $env:APPDATA; $oldLocalAppData = $env:LOCALAPPDATA
    $oldUserProfile = $env:USERPROFILE; $oldTemp = $env:TEMP; $oldTmp = $env:TMP
    $oldSlintBackend = $env:SLINT_BACKEND
    $oldCliplineFfmpeg = $env:CLIPLINE_FFMPEG
    try {
        $env:APPDATA = $appData; $env:LOCALAPPDATA = $localAppData
        $env:USERPROFILE = $profileRoot; $env:TEMP = $tempRoot; $env:TMP = $tempRoot
        $env:SLINT_BACKEND = $Renderer
        $env:CLIPLINE_FFMPEG = $resolvedFfmpeg
        $arguments = @(
            '--fixture-root', (Quote-LibraryArgument $fixtureRoot),
            '--fixture-seed-root', (Quote-LibraryArgument $corpus.seedRoot),
            '--source-fixture', (Quote-LibraryArgument $Fixture.path),
            '--source-sha256', $Fixture.sha256,
            '--clip-count', [string]$ClipCount,
            '--scenario', $Scenario,
            '--marker-path', (Quote-LibraryArgument $markerPath),
            '--stop-path', (Quote-LibraryArgument $stopPath),
            '--exercise-path', (Quote-LibraryArgument $exercisePath),
            '--telemetry-path', (Quote-LibraryArgument $telemetryPath),
            '--renderer', $Renderer,
            '--build-sha', $gitCommit,
            '--adapter', (Quote-LibraryArgument $gateAdapter),
            '--scale', $gateScale.ToString([Globalization.CultureInfo]::InvariantCulture)
        )
        try {
            $root = Start-Process -FilePath $resolvedExe -ArgumentList $arguments `
                -WorkingDirectory (Split-Path -Parent $resolvedExe) -PassThru
        } catch {
            $launchError = $_.Exception.Message
        }
    } finally {
        $env:APPDATA = $oldAppData; $env:LOCALAPPDATA = $oldLocalAppData
        $env:USERPROFILE = $oldUserProfile; $env:TEMP = $oldTemp; $env:TMP = $oldTmp
        $env:SLINT_BACKEND = $oldSlintBackend
        $env:CLIPLINE_FFMPEG = $oldCliplineFfmpeg
    }

    try {
        if ($launchError) { throw "start catalog harness: $launchError" }
        $deadline = [datetime]::UtcNow.AddSeconds(10)
        $rootCim = $null
        do {
            $rootCim = Get-CimInstance Win32_Process -Filter "ProcessId=$($root.Id)"
            if (-not $rootCim) { Start-Sleep -Milliseconds 25 }
        } while (-not $rootCim -and [datetime]::UtcNow -lt $deadline)
        if (-not $rootCim -or [string]$rootCim.Name -ne $rootName) {
            throw 'launched catalog harness identity could not be established'
        }
        $rootStart = ([datetime]$rootCim.CreationDate).ToUniversalTime()
        $observedTree["$($root.Id)|$($rootStart.Ticks)"] = [pscustomobject][ordered]@{
            processId = [int]$root.Id; name = $rootName; creationUtc = $rootStart.ToString('o')
        }
        $markerContract = [pscustomobject]@{
            processId = [int]$root.Id; buildSha = $gitCommit; renderer = $Renderer
            adapter = $gateAdapter; scale = $gateScale; sourceSha256 = $Fixture.sha256
            fixtureRoot = $fixtureRoot; fixtureSeedRoot = $corpus.seedRoot
        }
        $clock = [System.Diagnostics.Stopwatch]::StartNew()
        $readinessDeadline = [datetime]::UtcNow.AddSeconds($ReadinessTimeoutSeconds)
        do {
            if ($root.HasExited) { throw 'catalog harness exited before semantic readiness' }
            $sample = Get-LibraryProcessSample -RootProcessId $root.Id -RootStartUtc $rootStart `
                -RootProcessName $rootName -RunId $runId -RendererName $Renderer `
                -ScenarioName $Scenario -Phase 'startup' -ElapsedMs $clock.ElapsedMilliseconds `
                -PreviousCpu $previousCpu -ObservedTree $observedTree -ConcurrentProcesses $concurrent
            if ($rows.Count + $sample.Rows.Count -gt $script:MaximumRawRows) {
                throw 'raw process rows exceeded the 500000-row evidence bound'
            }
            foreach ($row in $sample.Rows) { $rows.Add($row) }; $samples.Add($sample.Aggregate)
            $maxObservedFfmpeg = [math]::Max($maxObservedFfmpeg, $sample.Aggregate.FfmpegProcessCount)
            $readyMarkers = Get-LibraryMarkerState -Path $markerPath -RootStartUtc $rootStart `
                -Contract $markerContract
            $ready = @($readyMarkers | Where-Object kind -eq 'ready').Count -eq 1 -and
                @($readyMarkers | Where-Object kind -eq 'pageSettled').Count -ge 1 -and
                @($readyMarkers | Where-Object kind -eq 'postersSettled').Count -ge 1
            if ($ready) { break }
            Start-Sleep -Milliseconds $SampleIntervalMs
        } while ([datetime]::UtcNow -lt $readinessDeadline)
        if (-not $ready) { throw 'ready/pageSettled/postersSettled markers were not all observed' }

        $warmupEnd = $clock.ElapsedMilliseconds + ($WarmupSeconds * 1000L)
        while ($clock.ElapsedMilliseconds -lt $warmupEnd) {
            if ($root.HasExited) { throw 'catalog harness exited during warmup' }
            $sample = Get-LibraryProcessSample -RootProcessId $root.Id -RootStartUtc $rootStart `
                -RootProcessName $rootName -RunId $runId -RendererName $Renderer `
                -ScenarioName $Scenario -Phase 'warmup' -ElapsedMs $clock.ElapsedMilliseconds `
                -PreviousCpu $previousCpu -ObservedTree $observedTree -ConcurrentProcesses $concurrent
            if ($rows.Count + $sample.Rows.Count -gt $script:MaximumRawRows) {
                throw 'raw process rows exceeded the 500000-row evidence bound'
            }
            foreach ($row in $sample.Rows) { $rows.Add($row) }; $samples.Add($sample.Aggregate)
            $maxObservedFfmpeg = [math]::Max($maxObservedFfmpeg, $sample.Aggregate.FfmpegProcessCount)
            Get-LibraryMarkerState -Path $markerPath -RootStartUtc $rootStart `
                -Contract $markerContract | Out-Null
            Start-Sleep -Milliseconds $SampleIntervalMs
        }
        $steadyStartMs = $clock.ElapsedMilliseconds
        $steadyEndMs = $steadyStartMs + ($SteadySeconds * 1000L)
        while ($clock.ElapsedMilliseconds -lt $steadyEndMs) {
            if ($samples.Count -ge $script:MaximumSamples) { throw 'sample count exceeded 40000' }
            if ($root.HasExited) { throw 'catalog harness exited during steady sampling' }
            $sample = Get-LibraryProcessSample -RootProcessId $root.Id -RootStartUtc $rootStart `
                -RootProcessName $rootName -RunId $runId -RendererName $Renderer `
                -ScenarioName $Scenario -Phase 'steady' -ElapsedMs $clock.ElapsedMilliseconds `
                -PreviousCpu $previousCpu -ObservedTree $observedTree -ConcurrentProcesses $concurrent
            if ($rows.Count + $sample.Rows.Count -gt $script:MaximumRawRows) {
                throw 'raw process rows exceeded the 500000-row evidence bound'
            }
            foreach ($row in $sample.Rows) { $rows.Add($row) }; $samples.Add($sample.Aggregate)
            $maxObservedFfmpeg = [math]::Max($maxObservedFfmpeg, $sample.Aggregate.FfmpegProcessCount)
            if ($exerciseRequired -and -not $exerciseSignaled) {
                Publish-CliplineCreateNewSignal -Path $exercisePath
                $exerciseSignalUtc = [datetime]::UtcNow
                $exerciseSignaled = $true
            }
            $duringMarkers = @(Get-LibraryMarkerState -Path $markerPath -RootStartUtc $rootStart `
                -Contract $markerContract)
            $settled = @($duringMarkers | Where-Object kind -eq 'exerciseSettled')
            if ($settled.Count -gt 1) { throw 'exerciseSettled marker was published more than once' }
            if ($settled.Count -eq 1) {
                $exerciseSettledUtc = ([datetime]$settled[0].timestampUtc).ToUniversalTime()
            }
            Start-Sleep -Milliseconds $SampleIntervalMs
        }
        if ($exerciseRequired) {
            if (-not $exerciseSignaled -or $null -eq $exerciseSignalUtc) {
                throw 'measured exercise signal was not published after the first steady sample'
            }
            $exerciseMarkers = @(Get-LibraryMarkerState -Path $markerPath -RootStartUtc $rootStart `
                -Contract $markerContract | Where-Object kind -eq 'exerciseSettled')
            if ($exerciseMarkers.Count -ne 1) {
                throw 'measured 100-cycle exercise did not settle inside the steady window'
            }
            $exerciseSettledUtc = ([datetime]$exerciseMarkers[0].timestampUtc).ToUniversalTime()
            if ($exerciseSettledUtc -lt $exerciseSignalUtc) {
                throw 'exercise settled before its owned sampler signal'
            }
            $steadySoFar = @($samples.ToArray() | Where-Object Phase -eq 'steady')
            $samplesBeforeExercise = @($steadySoFar | Where-Object {
                ([datetime]$_.SampleUtc).ToUniversalTime() -lt $exerciseSignalUtc
            }).Count
            $samplesAfterExercise = @($steadySoFar | Where-Object {
                ([datetime]$_.SampleUtc).ToUniversalTime() -gt $exerciseSettledUtc
            }).Count
            if ($samplesBeforeExercise -lt 1 -or $samplesAfterExercise -lt 1) {
                throw 'steady samples did not span both sides of the measured 100-cycle exercise'
            }
        }
        if (Test-Path -LiteralPath $telemetryPath) {
            throw 'final telemetry appeared before the sampler requested shutdown'
        }
        Publish-CliplineCreateNewSignal -Path $stopPath
        if (-not $root.WaitForExit($ShutdownGraceSeconds * 1000)) {
            throw 'catalog harness did not stop within the shutdown grace'
        }
        $root.WaitForExit(); $exitCode = $root.ExitCode
        if ($exitCode -ne 0) { throw "catalog harness exited with code $exitCode" }
        Update-LibraryObservedDescendants -ObservedTree $observedTree `
            -RootProcessId $root.Id -RootStartUtc $rootStart
        $lingering = @(Get-LibraryLiveOwnedDescendants -ObservedTree $observedTree `
            -RootProcessId $root.Id)
        if ($lingering.Count -gt 0) {
            throw "catalog harness left $($lingering.Count) owned descendant process(es) alive"
        }
        $readyMarkers = Get-LibraryMarkerState -Path $markerPath -RootStartUtc $rootStart `
            -Contract $markerContract -ProducerExited
        if (@($readyMarkers | Where-Object kind -eq 'stop').Count -ne 1) {
            throw 'catalog harness did not publish exactly one stop marker'
        }
        if (-not (Test-Path -LiteralPath $telemetryPath -PathType Leaf)) {
            throw 'catalog harness did not publish final telemetry'
        }
        if ((Get-Item -LiteralPath $telemetryPath).Length -gt $script:MaximumTelemetryBytes) {
            throw 'catalog harness telemetry exceeded 1 MiB'
        }
        $hashBefore = (Get-FileHash -LiteralPath $telemetryPath -Algorithm SHA256).Hash
        Start-Sleep -Milliseconds 100
        $hashAfter = (Get-FileHash -LiteralPath $telemetryPath -Algorithm SHA256).Hash
        if ($hashBefore -ne $hashAfter) { throw 'telemetry changed after the owned root exited' }
        $runnerTelemetry = Get-Content -LiteralPath $telemetryPath -Raw -Encoding UTF8 |
            ConvertFrom-Json
        Assert-LibraryTelemetry -Telemetry $runnerTelemetry -Fixture $Fixture -Count $ClipCount `
            -ScenarioName $Scenario -RendererName $Renderer -Contract $markerContract
        if ($Scenario -eq 'reveal-close-100') {
            $cloudMediaPending = [bool]$runnerTelemetry.reveal.cloudMediaCyclesPending
            $windowPendingProperty = `
                $runnerTelemetry.reveal.PSObject.Properties['windowRevealCloseCyclesPending']
            $windowLifecyclePending = $null -ne $windowPendingProperty -and
                [bool]$windowPendingProperty.Value
        }
        $pageSettledTimestamp = ([datetime](@($readyMarkers |
            Where-Object kind -eq 'pageSettled')[0].timestampUtc)).ToUniversalTime()
        $markerFirstUsableMs = ($pageSettledTimestamp - $rootStart).TotalMilliseconds
        if ([math]::Abs([double]$runnerTelemetry.metrics.firstUsablePageMs - $markerFirstUsableMs) -gt 250.0) {
            throw 'telemetry firstUsablePageMs does not match the semantic ready marker'
        }
        if ($maxObservedFfmpeg -gt 2) { throw 'observed FFmpeg child peak exceeded 2' }
    } catch {
        $failure = $_.Exception.Message
    } finally {
        $ended = [datetime]::UtcNow
        if (-not (Test-Path -LiteralPath $stopPath)) {
            try { Publish-CliplineCreateNewSignal -Path $stopPath } catch { }
        }
        if ($root) {
            if ($rootStart) {
                try {
                    Update-LibraryObservedDescendants -ObservedTree $observedTree `
                        -RootProcessId $root.Id -RootStartUtc $rootStart
                } catch { }
            }
            Stop-LibraryOwnedTree -ObservedTree $observedTree -RootProcessId $root.Id
        }
    }

    $steady = @($samples.ToArray() | Where-Object Phase -eq 'steady')
    $summary = $null
    $noise = [pscustomobject][ordered]@{
        thresholdPercent = $MaxBackgroundCpuPercent; maximumNoisySampleRatio = $MaxNoisySampleRatio
        samplesWithCpu = 0; samplesWithoutCpu = 0; noisySamples = 0
        noisySampleRatio = $null; accepted = $false; rawSamples = @()
    }
    if ($steady.Count -gt 0) {
        try {
            $withCpu = @($steady | Where-Object { $null -ne $_.BackgroundCpuPercent })
            $noise.samplesWithCpu = $withCpu.Count; $noise.samplesWithoutCpu = $steady.Count - $withCpu.Count
            $noise.rawSamples = @($steady | ForEach-Object {
                [pscustomobject]@{
                    sampleUtc = $_.SampleUtc; systemCpuPercent = $_.SystemCpuPercent
                    measuredTreeCpuPercent = $_.TreeCpuPercent
                    backgroundCpuPercent = $_.BackgroundCpuPercent
                    readError = $_.SystemCpuReadError
                }
            })
            if ($withCpu.Count -gt 0) {
                $noise.noisySamples = @($withCpu | Where-Object {
                    [double]$_.BackgroundCpuPercent -gt $MaxBackgroundCpuPercent
                }).Count
                $noise.noisySampleRatio = $noise.noisySamples / [double]$withCpu.Count
                $noise.accepted = $noise.samplesWithoutCpu -eq 0 -and
                    $noise.noisySampleRatio -le $MaxNoisySampleRatio
            }
            $summary = [pscustomobject][ordered]@{
                steadySampleCount = $steady.Count
                treePrivateWorkingSetBytes = Get-LibraryMetricSummary $steady TreePrivateWorkingSetBytes
                treePrivateCommitBytes = Get-LibraryMetricSummary $steady TreePrivateCommitBytes
                treeWorkingSetBytes = Get-LibraryMetricSummary $steady TreeWorkingSetBytes
                treeCpuPercent = Get-LibraryMetricSummary $steady TreeCpuPercent
                treeHandleCount = Get-LibraryMetricSummary $steady TreeHandleCount
                treeThreadCount = Get-LibraryMetricSummary $steady TreeThreadCount
                treeProcessCount = Get-LibraryMetricSummary $steady TreeProcessCount
                gpuLocalBytes = Get-LibraryMetricSummary $steady GpuLocalBytes -Optional
                gpuNonLocalBytes = Get-LibraryMetricSummary $steady GpuNonLocalBytes -Optional
                childReadFailuresTotal = [long](($steady | Measure-Object ChildReadFailures -Sum).Sum)
                pwsGrowthBytes = [long]$steady[-1].TreePrivateWorkingSetBytes -
                    [long]$steady[0].TreePrivateWorkingSetBytes
                handleGrowth = [long]$steady[-1].TreeHandleCount - [long]$steady[0].TreeHandleCount
                threadGrowth = [long]$steady[-1].TreeThreadCount - [long]$steady[0].TreeThreadCount
                observedFfmpegPeak = $maxObservedFfmpeg
            }
            if (-not $failure -and $summary.childReadFailuresTotal -ne 0) {
                $failure = 'one or more child metric reads failed during steady sampling'
            }
            if (-not $failure -and -not $noise.accepted) {
                $failure = 'background CPU noise exceeded the configured protocol'
            }
            if (-not $failure -and $ClipCount -eq 2000 -and $SteadySeconds -ge 300 -and
                [double]$summary.treePrivateWorkingSetBytes.p50 -gt 140MB) {
                $failure = '2,000-clip five-minute PWS p50 exceeded 140 MiB'
            }
            if (-not $failure -and $Scenario -in @('selection-page-churn', 'reveal-close-100') -and
                [long]$summary.pwsGrowthBytes -gt 10MB) {
                $failure = 'sampled process-tree PWS growth exceeded 10 MiB'
            }
            if (-not $failure -and $Scenario -in @('selection-page-churn', 'reveal-close-100') -and
                ([long]$summary.handleGrowth -gt 4 -or [long]$summary.threadGrowth -gt 2)) {
                $failure = '100-cycle process-tree handle/thread growth exceeded the bounded settle allowance'
            }
        } catch { if (-not $failure) { $failure = $_.Exception.Message } }
    } elseif (-not $failure) { $failure = 'no steady samples were captured' }

    $columns = @(Get-CliplineSampleColumns)
    $csv = if ($rows.Count -gt 0) {
        (@($rows.ToArray() | Select-Object -Property $columns | ConvertTo-Csv -NoTypeInformation) -join "`r`n") + "`r`n"
    } else { (($columns | ForEach-Object { '"' + $_ + '"' }) -join ',') + "`r`n" }
    Write-CliplineCreateNewText -Path $rawPath -Text $csv
    $provenance = [pscustomobject][ordered]@{
        schemaVersion = $script:SchemaVersion; harnessVersion = $script:HarnessVersion
        runId = $runId; attempt = $Attempt; status = if ($failure) { 'rejected' } else { 'accepted' }
        failure = $failure; scenario = $Scenario; clipCount = $ClipCount; renderer = $Renderer
        publishableDuration = $SteadySeconds -ge 300 -and -not $AllowNonBenchmarkBuild
        gitCommit = $gitCommit; trackedWorktreeDirty = $gitDirty
        executable = [pscustomobject]@{ path = $resolvedExe; sha256 = $exeHash; bytes = $exeBytes }
        corpus = $corpus
        runner = [pscustomobject]@{
            processId = if ($root) { [int]$root.Id } else { $null }
            processCreatedUtc = if ($rootStart) { $rootStart.ToString('o') } else { $null }
            exitCode = $exitCode; markerPath = $markerPath; telemetryPath = $telemetryPath
            telemetrySha256 = if (Test-Path $telemetryPath) {
                (Get-FileHash $telemetryPath -Algorithm SHA256).Hash.ToLowerInvariant()
            } else { $null }
            telemetry = $runnerTelemetry
        }
        timing = [pscustomobject]@{
            fixtureConstructionCompletedUtc = $corpus.constructionCompletedUtc
            harnessStartedUtc = $started.ToString('o'); harnessEndedUtc = $ended.ToString('o')
            warmupSeconds = $WarmupSeconds; steadySeconds = $SteadySeconds
            sampleIntervalMs = $SampleIntervalMs
            exerciseRequired = $exerciseRequired
            exerciseSignalUtc = if ($exerciseSignalUtc) { $exerciseSignalUtc.ToString('o') } else { $null }
            exerciseSettledUtc = if ($exerciseSettledUtc) { $exerciseSettledUtc.ToString('o') } else { $null }
        }
        machine = $machine
        processScope = [pscustomobject]@{
            policy = 'owned-root-plus-creation-time-valid-descendants'
            observedTree = @($observedTree.Values | Sort-Object processId)
            excludedConcurrentCliplineProcesses = @($concurrent.Values | Sort-Object processId, creationUtc)
        }
        systemNoise = $noise; rawSamples = [pscustomobject]@{ path = $rawPath; rows = $rows.Count; columns = $columns }
        summary = $summary
        gates = [pscustomobject]@{
            absoluteTelemetryBoundsPassed = -not $failure
            fiveMinuteDuration = $SteadySeconds -ge 300
            matchedTauri = 'pending'
            cloudMediaCycles = if ($Scenario -ne 'reveal-close-100') {
                'not-applicable'
            } elseif ($runnerTelemetry -and $runnerTelemetry.reveal.cloudMediaCyclesPending) {
                'pending'
            } else { 'measured' }
            windowLifecycle = if ($Scenario -ne 'reveal-close-100') {
                'not-applicable'
            } elseif ($windowLifecyclePending) { 'pending' } else { 'measured' }
        }
    }
    Write-CliplineCreateNewText -Path $provenancePath `
        -Text (($provenance | ConvertTo-Json -Depth 24) + "`r`n")
    return [pscustomobject]@{
        runId = $runId; accepted = -not $failure; provenancePath = $provenancePath
        rawPath = $rawPath; failure = $failure; summary = $summary
        fullLifecyclePending = $cloudMediaPending -or $windowLifecyclePending
    }
}

if ($env:OS -ne 'Windows_NT') { throw 'Slint Library metrics require Windows' }
if ($MaximumAttempts -lt $AcceptedSamples) { throw '-MaximumAttempts must be >= -AcceptedSamples' }
if ($Renderer -ne 'winit-software') { throw 'Task 11 currently pins renderer to winit-software' }
$planned = [math]::Ceiling(
    (($ReadinessTimeoutSeconds + $WarmupSeconds + $SteadySeconds) * 1000.0) / $SampleIntervalMs
) + 4
if ($planned -gt $script:MaximumSamples) { throw 'requested run exceeds the sample evidence bound' }
$preflight = Get-CliplineNativeProcessSnapshot -ProcessId $PID
if ($preflight.PrivateWorkingSetBytes -le 0 -or $preflight.PrivateCommitBytes -le 0) {
    throw 'PROCESS_MEMORY_COUNTERS_EX2 preflight returned invalid values'
}
$resolvedExe = (Resolve-Path -LiteralPath $Exe -ErrorAction Stop).Path
if (-not (Test-Path -LiteralPath $resolvedExe -PathType Leaf)) { throw '-Exe must be a file' }
if (-not $AllowNonBenchmarkBuild) {
    $profileDirectory = Split-Path -Leaf (Split-Path -Parent (Split-Path -Parent $resolvedExe))
    if ($profileDirectory -ne 'benchmark') {
        throw 'publishable Library gates require catalog_harness from the benchmark profile'
    }
    $standaloneCargo = Get-Content -LiteralPath `
        (Join-Path $script:RepoRoot 'apps/clipline-slint-spike/Cargo.toml') -Raw
    if ($standaloneCargo -notmatch
        '(?ms)^\[profile\.benchmark\].*?^inherits\s*=\s*"release".*?^debug-assertions\s*=\s*true\s*$') {
        throw 'standalone benchmark profile must inherit release with debug assertions'
    }
}
$helpText = (& $resolvedExe '--help' 2>&1 | Out-String)
if ($LASTEXITCODE -ne 0 -or
    $helpText -notmatch '--fixture-root' -or $helpText -notmatch '--exercise-path' -or
    $helpText -notmatch '--telemetry-path') {
    throw 'catalog_harness executable did not expose the Task 11 CLI contract'
}
$fixture = Resolve-LibraryFixture -Directory $FixturesDir -RequestedPath $FixturePath
$resolvedFfmpeg = $null
$ffmpegHash = $null
if ([string]::IsNullOrWhiteSpace($FfmpegPath)) {
    $FfmpegPath = Join-Path $script:RepoRoot 'apps/clipline-app/ffmpeg/ffmpeg.exe'
}
$resolvedFfmpeg = (Resolve-Path -LiteralPath $FfmpegPath -ErrorAction Stop).Path
if (-not (Test-Path -LiteralPath $resolvedFfmpeg -PathType Leaf)) {
    throw '-FfmpegPath must resolve to a file'
}
$ffmpegHash = (Get-FileHash -LiteralPath $resolvedFfmpeg -Algorithm SHA256).Hash.ToLowerInvariant()
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path ([System.IO.Path]::GetTempPath()) 'clipline-slint-library'
}
$outputRoot = [System.IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Path $outputRoot -Force | Out-Null
$gitCommit = (& git -C $script:RepoRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($gitCommit)) { throw 'cannot resolve git commit' }
$gitStatus = @(& git -C $script:RepoRoot status --porcelain --untracked-files=all)
if ($LASTEXITCODE -ne 0) { throw 'cannot inspect git worktree status' }
$gitDirty = @($gitStatus | Where-Object {
    -not (Test-LibraryGitStatusEntryAllowed -Entry ([string]$_))
}).Count -gt 0
if ($gitDirty -and -not $AllowNonBenchmarkBuild) {
    throw 'publishable Library evidence requires a clean source worktree'
}
$exeHash = (Get-FileHash -LiteralPath $resolvedExe -Algorithm SHA256).Hash.ToLowerInvariant()
$exeBytes = [long](Get-Item -LiteralPath $resolvedExe).Length
$os = Get-CimInstance Win32_OperatingSystem
$dpi = 96
try {
    if (-not ('CliplineLibraryDpi' -as [type])) {
        Add-Type -TypeDefinition 'using System.Runtime.InteropServices; public static class CliplineLibraryDpi { [DllImport("user32.dll")] public static extern uint GetDpiForSystem(); }' -ErrorAction Stop
    }
    $dpi = [int][CliplineLibraryDpi]::GetDpiForSystem()
} catch { }
$machine = [pscustomobject][ordered]@{
    computerName = $env:COMPUTERNAME
    operatingSystem = [pscustomobject]@{ caption = $os.Caption; version = $os.Version; build = $os.BuildNumber }
    processors = @(Get-CimInstance Win32_Processor | ForEach-Object {
        [pscustomobject]@{ name = $_.Name; cores = $_.NumberOfCores; logicalProcessors = $_.NumberOfLogicalProcessors }
    })
    physicalMemoryBytes = [long]$os.TotalVisibleMemorySize * 1KB
    videoControllers = @(Get-CimInstance Win32_VideoController -ErrorAction SilentlyContinue | ForEach-Object {
        [pscustomobject]@{ name = $_.Name; driverVersion = $_.DriverVersion; driverDate = $_.DriverDate }
    })
    displayScale = [pscustomobject]@{ dpi = $dpi; percent = [math]::Round(($dpi / 96.0) * 100.0, 1) }
    sessionName = $env:SESSIONNAME; remoteSession = $env:SESSIONNAME -like 'RDP-*'
}
$gateAdapter = if ($machine.videoControllers.Count -gt 0) {
    [string]$machine.videoControllers[0].name
} else { 'unavailable' }
$gateScale = [double]$machine.displayScale.percent / 100.0

$results = New-Object System.Collections.Generic.List[object]
$accepted = 0
for ($attempt = 1; $attempt -le $MaximumAttempts -and $accepted -lt $AcceptedSamples; $attempt++) {
    $result = Invoke-LibrarySample -Attempt $attempt -OutputRoot $outputRoot -Fixture $fixture
    $results.Add($result)
    if ($result.accepted) { $accepted++ }
    Write-Host ("attempt {0}: {1} ({2})" -f $attempt,
        $(if ($result.accepted) { 'accepted' } else { 'rejected' }), $result.provenancePath)
}
$seriesId = '{0}-slint-library-{1}-{2}-series-{3}' -f `
    ([datetime]::UtcNow.ToString('yyyyMMddTHHmmssZ')), $ClipCount, $Scenario,
    ([guid]::NewGuid().ToString('N').Substring(0, 8))
$seriesPath = Join-Path $outputRoot "$seriesId.json"
$hasPendingLifecycle = @($results | Where-Object {
    $_.accepted -and $_.fullLifecyclePending
}).Count -gt 0
$publishable = $accepted -ge 3 -and $SteadySeconds -ge 300 -and
    -not $AllowNonBenchmarkBuild -and -not $hasPendingLifecycle
$series = [pscustomobject][ordered]@{
    schemaVersion = 1; harnessVersion = $script:HarnessVersion; seriesId = $seriesId
    scenario = $Scenario; clipCount = $ClipCount; renderer = $Renderer
    requestedAcceptedSamples = $AcceptedSamples; acceptedSamples = $accepted
    rejectedSamples = $results.Count - $accepted; maximumAttempts = $MaximumAttempts
    publishableAbsoluteEvidence = $publishable
    reasonNotPublishable = if ($publishable) { $null } elseif ($accepted -lt 3) {
        'fewer than three accepted samples'
    } elseif ($SteadySeconds -lt 300) { 'steady window is shorter than five minutes' }
    elseif ($hasPendingLifecycle) { 'Cloud media or real window lifecycle cycles remain explicitly pending' }
    else { 'executable was not required to use the benchmark profile' }
    matchedTauriGate = 'pending'
    realGpuGate = 'pending-winit-software-does-not-exercise-the-native-video-path'
    runs = @($results | ForEach-Object {
        [pscustomobject]@{
            runId = $_.runId; accepted = $_.accepted; provenancePath = $_.provenancePath
            failure = $_.failure; fullLifecyclePending = $_.fullLifecyclePending
        }
    })
}
Write-CliplineCreateNewText -Path $seriesPath -Text (($series | ConvertTo-Json -Depth 10) + "`r`n")
Write-Host "series: $seriesPath"
if ($accepted -lt $AcceptedSamples) {
    throw "only $accepted of $AcceptedSamples required samples were accepted after $($results.Count) attempts"
}
