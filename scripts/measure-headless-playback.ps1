<#
.SYNOPSIS
    Measures Clipline's spawned headless native-playback process tree.

.DESCRIPTION
    Launches exactly one headless_playback executable, verifies its PID and
    creation time, and samples only that root plus creation-time-valid
    descendants through scripts/lib/Clipline.ProcessMetrics.psm1.

    The executable CLI contract is:

      headless_playback.exe --fixture <absolute-path> \
        --scenario <playback|seek-storm|cycle-100> \
        --run-seconds <warmup-plus-steady-seconds> \
        --telemetry <absolute-json-path>

    On a clean exit the runner must atomically create UTF-8 JSON with this
    stable envelope (the contents of metrics are runner-owned and preserved):

      {
        "schemaVersion": 1,
        "scenario": "playback",
        "status": "ok",
        "sourceFixture": "C:/absolute/source.mp4",
        "metrics": { "elapsedMs": 330000, ... }
      }

    Existing Clipline or headless playback processes are never attached to or
    terminated. They are excluded from the measured tree and recorded in the
    provenance JSON. A run is rejected only if measured background CPU noise,
    child-read failures, duration, runner telemetry, or another explicit
    protocol check fails.

    Generated evidence defaults to the system temporary directory. If an
    in-repository output directory is selected, do not commit its machine-
    specific CSV, JSON, or log files.

.EXAMPLE
    cargo build -p clipline-playback --example headless_playback --release
    ./scripts/measure-headless-playback.ps1 `
      -Exe target/release/examples/headless_playback.exe `
      -FixturePath C:/media/clipline-1080p60-two-opus.mp4 `
      -Scenario playback -WarmupSeconds 30 -SteadySeconds 300

.EXAMPLE
    ./scripts/measure-headless-playback.ps1 `
      -Exe target/release/examples/headless_playback.exe `
      -FixturePath fixtures/playback/hybrid-writer-h264-two-opus-5s.mp4 `
      -Scenario seek-storm -WarmupSeconds 0 -SteadySeconds 3 `
      -OutputDirectory artifacts/slint-playback-smoke
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [Parameter(Mandatory = $true)][string]$FixturePath,
    [Parameter(Mandatory = $true)]
    [ValidateSet('playback', 'seek-storm', 'cycle-100')]
    [string]$Scenario,
    [ValidateRange(0, 3600)][int]$WarmupSeconds = 30,
    [ValidateRange(1, 3600)][int]$SteadySeconds = 300,
    [ValidateRange(100, 60000)][int]$SampleIntervalMs = 1000,
    [ValidateRange(5, 600)][int]$ShutdownGraceSeconds = 30,
    [ValidateRange(0.0, 100.0)][double]$MaxBackgroundCpuPercent = 10.0,
    [ValidateRange(0.0, 1.0)][double]$MaxNoisySampleRatio = 0.05,
    [string]$Renderer = 'd3d11-headless',
    [string]$OutputDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:HarnessVersion = '1.0.0'
$script:SchemaVersion = 1
$script:MaximumSamples = 40000
$script:MaximumTreeProcesses = 64
$script:MaximumObservedIdentities = 1024
$script:MaximumTelemetryBytes = 1MB
$script:ScriptRoot = Split-Path -Parent $PSCommandPath
$script:RepoRoot = Split-Path -Parent $script:ScriptRoot
Import-Module (Join-Path $script:ScriptRoot 'lib/Clipline.ProcessMetrics.psm1') -Force

function Quote-HeadlessArgument {
    param([Parameter(Mandatory = $true)][string]$Value)
    if ($Value.Contains('"')) { throw "runner argument contains an unsupported quote: $Value" }
    return '"' + $Value + '"'
}

function Get-HeadlessMetricSummary {
    param(
        [Parameter(Mandatory = $true)][object[]]$Samples,
        [Parameter(Mandatory = $true)][string]$Property,
        [switch]$Optional
    )

    $values = @($Samples | ForEach-Object { $_.$Property } | Where-Object { $null -ne $_ })
    if ($values.Count -eq 0) {
        if ($Optional) { return $null }
        throw "steady sample metric is empty: $Property"
    }
    return [pscustomobject][ordered]@{
        p50 = Get-CliplineMedian -Values $values
        p95 = Get-CliplinePercentile -Values $values -Percentile 0.95
    }
}

function Get-HeadlessSystemCpuPercent {
    try {
        $row = Get-CimInstance Win32_PerfFormattedData_PerfOS_Processor `
            -Filter "Name='_Total'" -ErrorAction Stop
        if (-not $row) { throw 'the _Total processor row is missing' }
        $value = [double]$row.PercentProcessorTime
        if ($value -lt 0.0 -or $value -gt 100.0) {
            throw "system CPU was outside 0..100: $value"
        }
        return [pscustomobject][ordered]@{ Value = $value; Error = $null }
    } catch {
        return [pscustomobject][ordered]@{ Value = $null; Error = $_.Exception.Message }
    }
}

function Add-HeadlessConcurrentProcesses {
    param(
        [Parameter(Mandatory = $true)][object[]]$ProcessRows,
        [Parameter(Mandatory = $true)][hashtable]$TreeProcessIds,
        [Parameter(Mandatory = $true)][string]$RootProcessName,
        [Parameter(Mandatory = $true)][hashtable]$Observed
    )

    $interestingNames = @('clipline-app.exe', 'clipline.exe', 'headless_playback.exe', $RootProcessName)
    $observedUtc = [datetime]::UtcNow.ToString('o')
    foreach ($row in $ProcessRows) {
        $processId = [int]$row.ProcessId
        if ($TreeProcessIds.ContainsKey($processId) -or $row.Name -notin $interestingNames) { continue }
        if (-not $row.CreationDate) { continue }
        $creationUtc = ([datetime]$row.CreationDate).ToUniversalTime()
        $identity = "$processId|$($creationUtc.Ticks)"
        if ($Observed.ContainsKey($identity)) {
            $Observed[$identity].lastObservedUtc = $observedUtc
            continue
        }
        $Observed[$identity] = [pscustomobject][ordered]@{
            processId = $processId
            parentProcessId = [int]$row.ParentProcessId
            name = [string]$row.Name
            creationUtc = $creationUtc.ToString('o')
            firstObservedUtc = $observedUtc
            lastObservedUtc = $observedUtc
        }
        if ($Observed.Count -gt $script:MaximumObservedIdentities) {
            throw "concurrent process provenance exceeded the $($script:MaximumObservedIdentities)-identity bound"
        }
    }
}

function Get-HeadlessProcessSample {
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

    $allProcesses = @(Get-CimInstance Win32_Process)
    $rootRow = $allProcesses | Where-Object { [int]$_.ProcessId -eq $RootProcessId } |
        Select-Object -First 1
    if (-not $rootRow) { return $null }
    if ($rootRow.Name -ne $RootProcessName) {
        throw "root PID $RootProcessId changed identity from $RootProcessName to $($rootRow.Name)"
    }
    $observedRootStart = ([datetime]$rootRow.CreationDate).ToUniversalTime()
    if ([math]::Abs(($observedRootStart - $RootStartUtc).TotalSeconds) -gt 2.0) {
        throw "root PID $RootProcessId was reused during sampling"
    }

    $descendants = @(Get-CliplineDescendantProcesses -RootProcessId $RootProcessId `
        -RootStart $RootStartUtc.ToLocalTime() -ProcessRows $allProcesses)
    $treeRows = @($rootRow) + $descendants
    if ($treeRows.Count -gt $script:MaximumTreeProcesses) {
        throw "spawned process tree exceeded the $($script:MaximumTreeProcesses)-process evidence bound"
    }

    $treeProcessIds = @{}
    foreach ($row in $treeRows) {
        $processId = [int]$row.ProcessId
        $treeProcessIds[$processId] = $true
        $creationUtc = ([datetime]$row.CreationDate).ToUniversalTime()
        $ObservedTree["$processId|$($creationUtc.Ticks)"] = [pscustomobject][ordered]@{
            processId = $processId
            name = [string]$row.Name
            creationUtc = $creationUtc.ToString('o')
        }
        if ($ObservedTree.Count -gt $script:MaximumObservedIdentities) {
            throw "spawned process identity history exceeded the $($script:MaximumObservedIdentities)-identity bound"
        }
    }
    Add-HeadlessConcurrentProcesses -ProcessRows $allProcesses -TreeProcessIds $treeProcessIds `
        -RootProcessName $RootProcessName -Observed $ConcurrentProcesses

    $metrics = New-Object System.Collections.Generic.List[object]
    $childReadFailures = 0
    foreach ($processRow in $treeRows) {
        $processId = [int]$processRow.ProcessId
        $isRoot = $processId -eq $RootProcessId
        try {
            $live = Get-Process -Id $processId -ErrorAction Stop
            $expectedStart = ([datetime]$processRow.CreationDate).ToUniversalTime()
            if ([math]::Abs(($live.StartTime.ToUniversalTime() - $expectedStart).TotalSeconds) -gt 2.0) {
                throw "PID $processId was reused between tree discovery and metric read"
            }
            $snapshot = Get-CliplineNativeProcessSnapshot -ProcessId $processId
            $identity = "$processId|$($expectedStart.Ticks)"
            $cpuPercent = 0.0
            if ($PreviousCpu.ContainsKey($identity)) {
                $previous = $PreviousCpu[$identity]
                $cpuPercent = Get-CliplineCpuPercent `
                    -PreviousTime100ns ([long]$previous.CpuTime100ns) `
                    -CurrentTime100ns ([long]$snapshot.CpuTime100ns) `
                    -ElapsedMs ([math]::Max(1, $ElapsedMs - [long]$previous.ElapsedMs)) `
                    -LogicalProcessorCount ([Environment]::ProcessorCount)
            }
            $PreviousCpu[$identity] = [pscustomobject]@{
                CpuTime100ns = [long]$snapshot.CpuTime100ns
                ElapsedMs = $ElapsedMs
            }
            $metrics.Add([pscustomobject][ordered]@{
                Cim = $processRow
                Snapshot = $snapshot
                IsRoot = $isRoot
                CpuPercent = [double]$cpuPercent
            })
        } catch {
            if ($isRoot) { throw "strict root metric read failed: $($_.Exception.Message)" }
            $childReadFailures++
        }
    }
    if (-not ($metrics | Where-Object IsRoot)) { throw 'strict root metric row is missing' }

    $treePrivateWorkingSet = [long](($metrics | ForEach-Object {
        $_.Snapshot.PrivateWorkingSetBytes
    } | Measure-Object -Sum).Sum)
    $treePrivateCommit = [long](($metrics | ForEach-Object {
        $_.Snapshot.PrivateCommitBytes
    } | Measure-Object -Sum).Sum)
    $treeWorkingSet = [long](($metrics | ForEach-Object {
        $_.Snapshot.WorkingSetBytes
    } | Measure-Object -Sum).Sum)
    $treeCpuPercent = [double](($metrics | Measure-Object -Property CpuPercent -Sum).Sum)
    $treeHandles = [long](($metrics | ForEach-Object {
        $_.Snapshot.HandleCount
    } | Measure-Object -Sum).Sum)
    $treeThreads = [long](($metrics | ForEach-Object {
        $_.Snapshot.ThreadCount
    } | Measure-Object -Sum).Sum)
    $processIds = @($metrics | ForEach-Object { [int]$_.Cim.ProcessId })
    $gpu = Get-CliplineGpuProcessMemory -ProcessIds $processIds
    $systemCpu = Get-HeadlessSystemCpuPercent
    $backgroundCpu = if ($null -ne $systemCpu.Value) {
        [math]::Max(0.0, [double]$systemCpu.Value - $treeCpuPercent)
    } else {
        $null
    }

    $sampleUtc = [datetime]::UtcNow.ToString('o')
    $rawRows = New-Object System.Collections.Generic.List[object]
    foreach ($metric in $metrics) {
        $rawRows.Add([pscustomobject][ordered]@{
            RunId = $RunId
            Frontend = 'headless'
            Renderer = $RendererName
            Scenario = $ScenarioName
            Phase = $Phase
            SampleUtc = $sampleUtc
            ElapsedMs = $ElapsedMs
            ProcessId = [int]$metric.Cim.ProcessId
            ParentProcessId = [int]$metric.Cim.ParentProcessId
            ProcessName = [string]$metric.Cim.Name
            ProcessRole = Get-CliplineProcessRole -ProcessName $metric.Cim.Name `
                -CommandLine $metric.Cim.CommandLine -IsRoot $metric.IsRoot
            IsRoot = [bool]$metric.IsRoot
            PrivateWorkingSetBytes = [long]$metric.Snapshot.PrivateWorkingSetBytes
            PrivateCommitBytes = [long]$metric.Snapshot.PrivateCommitBytes
            WorkingSetBytes = [long]$metric.Snapshot.WorkingSetBytes
            CpuTime100ns = [long]$metric.Snapshot.CpuTime100ns
            CpuPercent = [math]::Round([double]$metric.CpuPercent, 4)
            HandleCount = [long]$metric.Snapshot.HandleCount
            ThreadCount = [long]$metric.Snapshot.ThreadCount
            TreePrivateWorkingSetBytes = $treePrivateWorkingSet
            TreePrivateCommitBytes = $treePrivateCommit
            TreeWorkingSetBytes = $treeWorkingSet
            TreeCpuPercent = [math]::Round($treeCpuPercent, 4)
            TreeHandleCount = $treeHandles
            TreeThreadCount = $treeThreads
            TreeProcessCount = $metrics.Count
            ChildReadFailures = $childReadFailures
            GpuCountersAvailable = [bool]$gpu.Available
            GpuLocalBytes = if ($gpu.Available) { [long]$gpu.LocalBytes } else { $null }
            GpuNonLocalBytes = if ($gpu.Available) { [long]$gpu.NonLocalBytes } else { $null }
        })
    }

    return [pscustomobject][ordered]@{
        Rows = @($rawRows.ToArray())
        Aggregate = [pscustomobject][ordered]@{
            SampleUtc = $sampleUtc
            Phase = $Phase
            TreePrivateWorkingSetBytes = $treePrivateWorkingSet
            TreePrivateCommitBytes = $treePrivateCommit
            TreeWorkingSetBytes = $treeWorkingSet
            TreeCpuPercent = $treeCpuPercent
            TreeHandleCount = $treeHandles
            TreeThreadCount = $treeThreads
            TreeProcessCount = $metrics.Count
            ChildReadFailures = $childReadFailures
            GpuCountersAvailable = [bool]$gpu.Available
            GpuLocalBytes = if ($gpu.Available) { [long]$gpu.LocalBytes } else { $null }
            GpuNonLocalBytes = if ($gpu.Available) { [long]$gpu.NonLocalBytes } else { $null }
            SystemCpuPercent = $systemCpu.Value
            BackgroundCpuPercent = $backgroundCpu
            SystemCpuReadError = $systemCpu.Error
        }
    }
}

function Stop-HeadlessObservedTree {
    param(
        [Parameter(Mandatory = $true)][hashtable]$ObservedTree,
        [Parameter(Mandatory = $true)][int]$RootProcessId
    )

    $records = @($ObservedTree.Values | Sort-Object {
        if ([int]$_.processId -eq $RootProcessId) { 1 } else { 0 }
    })
    foreach ($record in $records) {
        try {
            $live = Get-Process -Id ([int]$record.processId) -ErrorAction Stop
            $expected = ([datetime]$record.creationUtc).ToUniversalTime()
            if ([math]::Abs(($live.StartTime.ToUniversalTime() - $expected).TotalSeconds) -le 2.0) {
                Stop-Process -Id ([int]$record.processId) -Force -ErrorAction SilentlyContinue
            }
        } catch {
            # The exact launched process already exited; never widen cleanup scope.
        }
    }
}

function Get-HeadlessLiveObservedDescendants {
    param(
        [Parameter(Mandatory = $true)][hashtable]$ObservedTree,
        [Parameter(Mandatory = $true)][int]$RootProcessId
    )

    $liveRecords = New-Object System.Collections.Generic.List[object]
    foreach ($record in $ObservedTree.Values) {
        if ([int]$record.processId -eq $RootProcessId) { continue }
        try {
            $live = Get-Process -Id ([int]$record.processId) -ErrorAction Stop
            $expected = ([datetime]$record.creationUtc).ToUniversalTime()
            if ([math]::Abs(($live.StartTime.ToUniversalTime() - $expected).TotalSeconds) -le 2.0) {
                $liveRecords.Add($record)
            }
        } catch {
            # The observed descendant exited or its PID was reused.
        }
    }
    return @($liveRecords.ToArray())
}

if ($env:OS -ne 'Windows_NT') { throw 'headless playback process metrics require Windows' }

$resolvedExe = (Resolve-Path -LiteralPath $Exe -ErrorAction Stop).Path
$resolvedFixture = (Resolve-Path -LiteralPath $FixturePath -ErrorAction Stop).Path
if (-not (Test-Path -LiteralPath $resolvedExe -PathType Leaf)) { throw '-Exe must be a file' }
if (-not (Test-Path -LiteralPath $resolvedFixture -PathType Leaf)) {
    throw '-FixturePath must be a file'
}

$plannedSamples = [math]::Ceiling(
    (($WarmupSeconds + $SteadySeconds) * 1000.0) / $SampleIntervalMs
) + 2
if ($plannedSamples -gt $script:MaximumSamples) {
    throw "requested run exceeds the $($script:MaximumSamples)-sample evidence bound"
}

# Strict PROCESS_MEMORY_COUNTERS_EX2 preflight before the child or evidence exists.
$preflight = Get-CliplineNativeProcessSnapshot -ProcessId $PID
if ($preflight.PrivateWorkingSetBytes -le 0 -or $preflight.PrivateCommitBytes -le 0) {
    throw 'PROCESS_MEMORY_COUNTERS_EX2 preflight returned invalid values'
}

$runId = '{0}-headless-{1}-{2}' -f `
    ([datetime]::UtcNow.ToString('yyyyMMddTHHmmssZ')), $Scenario,
    ([guid]::NewGuid().ToString('N').Substring(0, 8))
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path ([System.IO.Path]::GetTempPath()) 'clipline-headless-playback'
}
$outputRoot = [System.IO.Path]::GetFullPath($OutputDirectory)
$runDirectory = Join-Path $outputRoot "runs/$runId"
New-Item -ItemType Directory -Path $runDirectory -Force | Out-Null
$rawCsvPath = Join-Path $outputRoot "$runId.raw.csv"
$provenancePath = Join-Path $outputRoot "$runId.provenance.json"
$telemetryPath = Join-Path $runDirectory 'runner.telemetry.json'
if ((Test-Path -LiteralPath $rawCsvPath) -or (Test-Path -LiteralPath $provenancePath)) {
    throw "run output already exists for $runId"
}

$gitCommit = (& git -C $script:RepoRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($gitCommit)) {
    throw 'could not resolve the source git commit'
}
$gitStatus = @(& git -C $script:RepoRoot status --porcelain --untracked-files=no)
$operatingSystem = Get-CimInstance Win32_OperatingSystem
$processors = @(Get-CimInstance Win32_Processor | ForEach-Object {
    [pscustomobject][ordered]@{
        name = $_.Name
        cores = $_.NumberOfCores
        logicalProcessors = $_.NumberOfLogicalProcessors
    }
})
$videoControllers = @(Get-CimInstance Win32_VideoController -ErrorAction SilentlyContinue |
    ForEach-Object {
        [pscustomobject][ordered]@{
            name = $_.Name
            driverVersion = $_.DriverVersion
            driverDate = $_.DriverDate
        }
    })

$rawRows = New-Object System.Collections.Generic.List[object]
$aggregateSamples = New-Object System.Collections.Generic.List[object]
$previousCpu = @{}
$observedTree = @{}
$concurrentProcesses = @{}
$rootProcess = $null
$rootStartUtc = $null
$rootProcessName = [System.IO.Path]::GetFileName($resolvedExe)
$clock = $null
$startedUtc = [datetime]::UtcNow
$endedUtc = $null
$runnerTelemetry = $null
$runnerExitCode = $null
$samplingOverruns = 0
$failure = $null
$totalRunSeconds = [int64]$WarmupSeconds + [int64]$SteadySeconds

try {
    $arguments = @(
        '--fixture', (Quote-HeadlessArgument $resolvedFixture),
        '--scenario', $Scenario,
        '--run-seconds', $totalRunSeconds.ToString([Globalization.CultureInfo]::InvariantCulture),
        '--telemetry', (Quote-HeadlessArgument $telemetryPath)
    )
    $rootProcess = Start-Process -FilePath $resolvedExe -ArgumentList $arguments `
        -WorkingDirectory (Split-Path -Parent $resolvedExe) -WindowStyle Hidden -PassThru

    $identityDeadline = [datetime]::UtcNow.AddSeconds(10)
    $rootCim = $null
    do {
        $rootCim = Get-CimInstance Win32_Process -Filter "ProcessId=$($rootProcess.Id)"
        if (-not $rootCim) { Start-Sleep -Milliseconds 25 }
    } while (-not $rootCim -and [datetime]::UtcNow -lt $identityDeadline)
    if (-not $rootCim -or $rootCim.Name -ne $rootProcessName) {
        throw 'launched headless process identity could not be established'
    }
    $rootStartUtc = ([datetime]$rootCim.CreationDate).ToUniversalTime()
    $liveRoot = Get-Process -Id $rootProcess.Id -ErrorAction Stop
    if ([math]::Abs(($liveRoot.StartTime.ToUniversalTime() - $rootStartUtc).TotalSeconds) -gt 2.0) {
        throw 'launched headless process creation time could not be verified'
    }
    $observedTree["$($rootProcess.Id)|$($rootStartUtc.Ticks)"] = [pscustomobject][ordered]@{
        processId = [int]$rootProcess.Id
        name = $rootProcessName
        creationUtc = $rootStartUtc.ToString('o')
    }

    $clock = [System.Diagnostics.Stopwatch]::StartNew()
    $measurementEndMs = $totalRunSeconds * 1000L
    $nextSampleMs = 0L
    while ($clock.ElapsedMilliseconds -lt $measurementEndMs) {
        if ($aggregateSamples.Count -ge $script:MaximumSamples) {
            throw "sample count exceeded the $($script:MaximumSamples)-sample evidence bound"
        }
        if ($rootProcess.HasExited) { break }

        $elapsedMs = $clock.ElapsedMilliseconds
        $phase = if ($elapsedMs -lt ($WarmupSeconds * 1000L)) { 'warmup' } else { 'steady' }
        $sample = Get-HeadlessProcessSample -RootProcessId $rootProcess.Id `
            -RootStartUtc $rootStartUtc -RootProcessName $rootProcessName -RunId $runId `
            -RendererName $Renderer -ScenarioName $Scenario -Phase $phase `
            -ElapsedMs $elapsedMs -PreviousCpu $previousCpu -ObservedTree $observedTree `
            -ConcurrentProcesses $concurrentProcesses
        if ($null -eq $sample) { break }
        foreach ($row in $sample.Rows) { $rawRows.Add($row) }
        $aggregateSamples.Add($sample.Aggregate)

        $nextSampleMs += $SampleIntervalMs
        $remainingMs = $nextSampleMs - $clock.ElapsedMilliseconds
        if ($remainingMs -gt 0) {
            Start-Sleep -Milliseconds ([int][math]::Min($remainingMs, $SampleIntervalMs))
        } else {
            $samplingOverruns++
            $nextSampleMs = $clock.ElapsedMilliseconds
        }
    }

    $minimumDurationMs = $measurementEndMs - $SampleIntervalMs
    if ($clock.ElapsedMilliseconds -lt $minimumDurationMs) {
        throw "runner exited before the requested measurement duration ($($clock.ElapsedMilliseconds) ms)"
    }
    if (-not $rootProcess.HasExited -and -not $rootProcess.WaitForExit($ShutdownGraceSeconds * 1000)) {
        throw "runner did not exit within the $ShutdownGraceSeconds-second shutdown grace"
    }
    $rootProcess.WaitForExit()
    $runnerExitCode = $rootProcess.ExitCode
    if ($runnerExitCode -ne 0) { throw "runner exited with code $runnerExitCode" }
    $lingeringDescendants = @(Get-HeadlessLiveObservedDescendants `
        -ObservedTree $observedTree -RootProcessId $rootProcess.Id)
    if ($lingeringDescendants.Count -gt 0) {
        throw "runner left $($lingeringDescendants.Count) creation-time-verified descendant process(es) alive"
    }
    if (-not (Test-Path -LiteralPath $telemetryPath -PathType Leaf)) {
        throw 'runner did not create its required telemetry JSON'
    }
    if ((Get-Item -LiteralPath $telemetryPath).Length -gt $script:MaximumTelemetryBytes) {
        throw "runner telemetry exceeded the $($script:MaximumTelemetryBytes)-byte bound"
    }
    $runnerTelemetry = Get-Content -LiteralPath $telemetryPath -Raw -Encoding UTF8 |
        ConvertFrom-Json
    if ([int]$runnerTelemetry.schemaVersion -ne 1) {
        throw "runner telemetry schemaVersion must be 1"
    }
    if ([string]$runnerTelemetry.scenario -ne $Scenario) {
        throw "runner telemetry scenario does not match $Scenario"
    }
    if ([string]$runnerTelemetry.status -ne 'ok') {
        throw "runner telemetry status is not ok"
    }
    if ($null -eq $runnerTelemetry.metrics) { throw 'runner telemetry metrics object is missing' }
    if ([string]::IsNullOrWhiteSpace([string]$runnerTelemetry.sourceFixture) -or
        -not [System.IO.Path]::GetFullPath([string]$runnerTelemetry.sourceFixture).Equals(
            $resolvedFixture,
            [StringComparison]::OrdinalIgnoreCase
        )) {
        throw 'runner telemetry sourceFixture does not match the measured fixture'
    }
    $runnerElapsedMs = 0L
    if ($null -eq $runnerTelemetry.metrics.elapsedMs -or
        -not [long]::TryParse(
            [string]$runnerTelemetry.metrics.elapsedMs,
            [ref]$runnerElapsedMs
        ) -or
        $runnerElapsedMs -lt $minimumDurationMs) {
        throw 'runner telemetry elapsedMs does not cover the requested measurement duration'
    }
} catch {
    $failure = $_.Exception.Message
} finally {
    $endedUtc = [datetime]::UtcNow
    if ($rootProcess) {
        Stop-HeadlessObservedTree -ObservedTree $observedTree -RootProcessId $rootProcess.Id
        if (-not $rootProcess.HasExited) { $null = $rootProcess.WaitForExit(5000) }
    }
}

$steady = @($aggregateSamples.ToArray() | Where-Object Phase -eq 'steady')
$summary = $null
$noiseAssessment = [pscustomobject][ordered]@{
    thresholdPercent = $MaxBackgroundCpuPercent
    maximumNoisySampleRatio = $MaxNoisySampleRatio
    samplesWithCpu = 0
    samplesWithoutCpu = 0
    noisySamples = 0
    noisySampleRatio = $null
    accepted = $false
    rawSamples = @()
}
if ($steady.Count -gt 0) {
    try {
        $backgroundValues = @($steady | Where-Object {
            $null -ne $_.BackgroundCpuPercent
        })
        $noiseAssessment.samplesWithCpu = $backgroundValues.Count
        $noiseAssessment.samplesWithoutCpu = $steady.Count - $backgroundValues.Count
        $noiseAssessment.rawSamples = @($steady | ForEach-Object {
            [pscustomobject][ordered]@{
                sampleUtc = $_.SampleUtc
                systemCpuPercent = $_.SystemCpuPercent
                measuredTreeCpuPercent = $_.TreeCpuPercent
                backgroundCpuPercent = $_.BackgroundCpuPercent
                readError = $_.SystemCpuReadError
            }
        })
        if ($backgroundValues.Count -gt 0) {
            $noiseAssessment.noisySamples = @($backgroundValues | Where-Object {
                [double]$_.BackgroundCpuPercent -gt $MaxBackgroundCpuPercent
            }).Count
            $noiseAssessment.noisySampleRatio =
                $noiseAssessment.noisySamples / [double]$backgroundValues.Count
            $noiseAssessment.accepted = `
                $noiseAssessment.samplesWithoutCpu -eq 0 -and `
                $noiseAssessment.noisySampleRatio -le $MaxNoisySampleRatio
        }
        $summary = [pscustomobject][ordered]@{
            steadySampleCount = $steady.Count
            treePrivateWorkingSetBytes = Get-HeadlessMetricSummary $steady 'TreePrivateWorkingSetBytes'
            treePrivateCommitBytes = Get-HeadlessMetricSummary $steady 'TreePrivateCommitBytes'
            treeWorkingSetBytes = Get-HeadlessMetricSummary $steady 'TreeWorkingSetBytes'
            treeCpuPercent = Get-HeadlessMetricSummary $steady 'TreeCpuPercent'
            treeHandleCount = Get-HeadlessMetricSummary $steady 'TreeHandleCount'
            treeThreadCount = Get-HeadlessMetricSummary $steady 'TreeThreadCount'
            treeProcessCount = Get-HeadlessMetricSummary $steady 'TreeProcessCount'
            gpuLocalBytes = Get-HeadlessMetricSummary $steady 'GpuLocalBytes' -Optional
            gpuNonLocalBytes = Get-HeadlessMetricSummary $steady 'GpuNonLocalBytes' -Optional
            systemCpuPercent = Get-HeadlessMetricSummary $steady 'SystemCpuPercent' -Optional
            backgroundCpuPercent = Get-HeadlessMetricSummary $steady 'BackgroundCpuPercent' -Optional
            childReadFailuresTotal = [long](($steady |
                Measure-Object -Property ChildReadFailures -Sum).Sum)
        }
    } catch {
        if (-not $failure) { $failure = $_.Exception.Message }
    }
}

if (-not $failure -and $steady.Count -eq 0) { $failure = 'no steady samples were captured' }
if (-not $failure -and $summary.childReadFailuresTotal -ne 0) {
    $failure = 'one or more spawned child metric reads failed during the steady window'
}
if (-not $failure -and -not $noiseAssessment.accepted) {
    $failure = 'background CPU noise exceeded the configured system-idle protocol'
}

if ($rawRows.Count -gt 0) {
    $rawRows.ToArray() | Select-Object -Property (Get-CliplineSampleColumns) |
        Export-Csv -LiteralPath $rawCsvPath -NoTypeInformation
}

$provenance = [pscustomobject][ordered]@{
    schemaVersion = $script:SchemaVersion
    harnessVersion = $script:HarnessVersion
    runId = $runId
    status = if ($failure) { 'rejected' } else { 'accepted' }
    failure = $failure
    git = [pscustomobject][ordered]@{
        commit = $gitCommit
        trackedWorktreeDirty = $gitStatus.Count -gt 0
    }
    scenario = $Scenario
    renderer = $Renderer
    executable = [pscustomobject][ordered]@{
        path = $resolvedExe
        sha256 = (Get-FileHash -LiteralPath $resolvedExe -Algorithm SHA256).Hash.ToLowerInvariant()
        bytes = [long](Get-Item -LiteralPath $resolvedExe).Length
    }
    fixture = [pscustomobject][ordered]@{
        path = $resolvedFixture
        sha256 = (Get-FileHash -LiteralPath $resolvedFixture -Algorithm SHA256).Hash.ToLowerInvariant()
        bytes = [long](Get-Item -LiteralPath $resolvedFixture).Length
    }
    runner = [pscustomobject][ordered]@{
        processId = if ($rootProcess) { [int]$rootProcess.Id } else { $null }
        processCreatedUtc = if ($rootStartUtc) { $rootStartUtc.ToString('o') } else { $null }
        exitCode = $runnerExitCode
        telemetryPath = $telemetryPath
        telemetry = $runnerTelemetry
    }
    timing = [pscustomobject][ordered]@{
        harnessStartedUtc = $startedUtc.ToString('o')
        harnessEndedUtc = $endedUtc.ToString('o')
        warmupSeconds = $WarmupSeconds
        requestedSteadySeconds = $SteadySeconds
        sampleIntervalMs = $SampleIntervalMs
        samplingOverruns = $samplingOverruns
    }
    machine = [pscustomobject][ordered]@{
        computerName = $env:COMPUTERNAME
        operatingSystem = [pscustomobject][ordered]@{
            caption = $operatingSystem.Caption
            version = $operatingSystem.Version
            buildNumber = $operatingSystem.BuildNumber
        }
        processors = $processors
        physicalMemoryBytes = [long]$operatingSystem.TotalVisibleMemorySize * 1KB
        videoControllers = $videoControllers
        sessionName = $env:SESSIONNAME
        remoteSession = [Environment]::UserInteractive -and $env:SESSIONNAME -like 'RDP-*'
    }
    processScope = [pscustomobject][ordered]@{
        policy = 'spawned-root-plus-creation-time-valid-descendants'
        maximumTreeProcesses = $script:MaximumTreeProcesses
        observedTree = @($observedTree.Values | Sort-Object processId)
        excludedConcurrentCliplineOrHeadlessProcesses = @(
            $concurrentProcesses.Values | Sort-Object processId, creationUtc
        )
    }
    systemNoise = $noiseAssessment
    rawSamples = [pscustomobject][ordered]@{
        path = if ($rawRows.Count -gt 0) { $rawCsvPath } else { $null }
        rows = $rawRows.Count
        columns = @(Get-CliplineSampleColumns)
    }
    summary = $summary
}
$provenance | ConvertTo-Json -Depth 20 |
    Set-Content -LiteralPath $provenancePath -Encoding UTF8

Write-Host "raw samples: $rawCsvPath"
Write-Host "provenance:  $provenancePath"
Write-Host "runner data: $runDirectory"
if ($summary) {
    Write-Host ("steady tree private working set p50/p95: {0:N1}/{1:N1} MiB" -f `
        ($summary.treePrivateWorkingSetBytes.p50 / 1MB),
        ($summary.treePrivateWorkingSetBytes.p95 / 1MB))
}
if ($failure) { throw "headless playback run rejected: $failure (see $provenancePath)" }
