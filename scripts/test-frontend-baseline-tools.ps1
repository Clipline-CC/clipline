param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$module = Join-Path $PSScriptRoot 'lib/Clipline.ProcessMetrics.psm1'
Import-Module $module -Force

function Assert-Equal {
    param([object]$Expected, [object]$Actual, [string]$Message)
    if ($Expected -ne $Actual) {
        throw "$Message (expected '$Expected', got '$Actual')"
    }
}

function Assert-SequenceEqual {
    param([object[]]$Expected, [object[]]$Actual, [string]$Message)
    Assert-Equal $Expected.Count $Actual.Count "$Message count"
    for ($index = 0; $index -lt $Expected.Count; $index++) {
        Assert-Equal $Expected[$index] $Actual[$index] "$Message at index $index"
    }
}

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

$rootStart = [datetime]'2026-08-01T00:00:00Z'
$rows = @(
    [pscustomobject]@{ ProcessId = 11; ParentProcessId = 10; CreationDate = $rootStart.AddSeconds(1) },
    [pscustomobject]@{ ProcessId = 12; ParentProcessId = 11; CreationDate = $rootStart.AddSeconds(2) },
    [pscustomobject]@{ ProcessId = 13; ParentProcessId = 10; CreationDate = $rootStart.AddSeconds(-1) },
    [pscustomobject]@{ ProcessId = 14; ParentProcessId = 13; CreationDate = $rootStart.AddSeconds(3) },
    [pscustomobject]@{ ProcessId = 10; ParentProcessId = 12; CreationDate = $rootStart.AddSeconds(4) },
    [pscustomobject]@{ ProcessId = 15; ParentProcessId = 10; CreationDate = $null }
)
$descendants = @(Get-CliplineDescendantProcesses `
    -RootProcessId 10 `
    -RootStart $rootStart `
    -ProcessRows $rows)
Assert-SequenceEqual @(11, 12) @($descendants.ProcessId) 'descendant traversal must reject PID reuse and cycles'

Assert-Equal 2.5 (Get-CliplineMedian @(4, 1, 3, 2)) 'median must interpolate even sample sets'
Assert-Equal 3 (Get-CliplineMedian @(3)) 'median must support one sample'
Assert-Equal 3.7 (Get-CliplinePercentile @(1, 2, 3, 4) 0.9) 'percentile interpolation'
$emptyFailed = $false
try { Get-CliplineMedian @() | Out-Null } catch { $emptyFailed = $true }
Assert-Equal $true $emptyFailed 'empty median must fail closed'

Assert-Equal 25.0 (Get-CliplineCpuPercent 1000000 3000000 100 8) 'CPU must normalize by wall time and cores'
Assert-Equal 0.0 (Get-CliplineCpuPercent 3000000 1000000 100 8) 'CPU counter reset must not go negative'

$expectedColumns = @(
    'RunId', 'Frontend', 'Renderer', 'Scenario', 'Phase', 'SampleUtc', 'ElapsedMs',
    'ProcessId', 'ParentProcessId', 'ProcessName', 'ProcessRole', 'IsRoot',
    'PrivateWorkingSetBytes', 'PrivateCommitBytes', 'WorkingSetBytes', 'CpuTime100ns',
    'CpuPercent', 'HandleCount', 'ThreadCount', 'TreePrivateWorkingSetBytes',
    'TreePrivateCommitBytes', 'TreeWorkingSetBytes', 'TreeCpuPercent', 'TreeHandleCount',
    'TreeThreadCount', 'TreeProcessCount', 'ChildReadFailures', 'GpuCountersAvailable',
    'GpuLocalBytes', 'GpuNonLocalBytes'
)
Assert-SequenceEqual $expectedColumns @(Get-CliplineSampleColumns) 'raw CSV schema must remain stable'

$gpu = Get-CliplineGpuProcessMemory -ProcessIds @(10) -CounterReader { throw 'counter unavailable' }
Assert-Equal $false $gpu.Available 'missing GPU counters must be explicit'
Assert-Equal $null $gpu.LocalBytes 'missing GPU local memory must not be reported as zero'
Assert-Equal $null $gpu.NonLocalBytes 'missing GPU non-local memory must not be reported as zero'

Assert-Equal 'root' (Get-CliplineProcessRole 'clipline-app.exe' '' $true) 'root process role'
Assert-Equal 'webview-gpu' (Get-CliplineProcessRole 'msedgewebview2.exe' '--type=gpu-process' $false) 'GPU role'
Assert-Equal 'webview-renderer' (Get-CliplineProcessRole 'msedgewebview2.exe' '--type=renderer' $false) 'renderer role'
Assert-Equal 'ffmpeg' (Get-CliplineProcessRole 'ffmpeg.exe' '' $false) 'FFmpeg role'

if ($env:OS -eq 'Windows_NT') {
    $live = Get-CliplineNativeProcessSnapshot -ProcessId $PID
    Assert-True ($live.PrivateWorkingSetBytes -gt 0) 'live private working set must be readable'
    Assert-True ($live.PrivateCommitBytes -gt 0) 'live private commit must be readable'
    Assert-True ($live.WorkingSetBytes -gt 0) 'live ordinary working set must be readable'
    Assert-True ($live.HandleCount -gt 0) 'live handle count must be readable'
    Assert-True ($live.ThreadCount -gt 0) 'live thread count must be readable'
}

Write-Host 'frontend baseline helper self-tests passed'
