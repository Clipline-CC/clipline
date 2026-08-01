Set-StrictMode -Version Latest

$script:SampleColumns = @(
    'RunId',
    'Frontend',
    'Renderer',
    'Scenario',
    'Phase',
    'SampleUtc',
    'ElapsedMs',
    'ProcessId',
    'ParentProcessId',
    'ProcessName',
    'ProcessRole',
    'IsRoot',
    'PrivateWorkingSetBytes',
    'PrivateCommitBytes',
    'WorkingSetBytes',
    'CpuTime100ns',
    'CpuPercent',
    'HandleCount',
    'ThreadCount',
    'TreePrivateWorkingSetBytes',
    'TreePrivateCommitBytes',
    'TreeWorkingSetBytes',
    'TreeCpuPercent',
    'TreeHandleCount',
    'TreeThreadCount',
    'TreeProcessCount',
    'ChildReadFailures',
    'GpuCountersAvailable',
    'GpuLocalBytes',
    'GpuNonLocalBytes'
)

function Get-CliplineSampleColumns {
    return @($script:SampleColumns)
}

function Get-CliplinePercentile {
    param(
        [Parameter(Mandatory = $true)][object[]]$Values,
        [Parameter(Mandatory = $true)][ValidateRange(0.0, 1.0)][double]$Percentile
    )

    $numbers = @($Values | ForEach-Object { [double]$_ } | Sort-Object)
    if ($numbers.Count -eq 0) {
        throw 'cannot calculate a percentile from an empty sample set'
    }
    if ($numbers.Count -eq 1) { return $numbers[0] }

    $position = $Percentile * ($numbers.Count - 1)
    $lower = [math]::Floor($position)
    $upper = [math]::Ceiling($position)
    if ($lower -eq $upper) { return $numbers[$lower] }
    $weight = $position - $lower
    return $numbers[$lower] + (($numbers[$upper] - $numbers[$lower]) * $weight)
}

function Get-CliplineMedian {
    param([Parameter(Mandatory = $true)][object[]]$Values)
    return Get-CliplinePercentile -Values $Values -Percentile 0.5
}

function Get-CliplineCpuPercent {
    param(
        [Parameter(Mandatory = $true)][long]$PreviousTime100ns,
        [Parameter(Mandatory = $true)][long]$CurrentTime100ns,
        [Parameter(Mandatory = $true)][ValidateRange(1, [int]::MaxValue)][long]$ElapsedMs,
        [Parameter(Mandatory = $true)][ValidateRange(1, 4096)][int]$LogicalProcessorCount
    )

    $delta = $CurrentTime100ns - $PreviousTime100ns
    if ($delta -lt 0) { return 0.0 }
    $wallTime100ns = $ElapsedMs * 10000.0
    return ($delta / $wallTime100ns) * (100.0 / $LogicalProcessorCount)
}

function Get-CliplineDescendantProcesses {
    param(
        [Parameter(Mandatory = $true)][int]$RootProcessId,
        [Parameter(Mandatory = $true)][datetime]$RootStart,
        [Parameter(Mandatory = $true)][object[]]$ProcessRows
    )

    $byParent = @{}
    foreach ($process in $ProcessRows) {
        $parent = [int]$process.ParentProcessId
        if (-not $byParent.ContainsKey($parent)) {
            $byParent[$parent] = New-Object System.Collections.Generic.List[object]
        }
        $byParent[$parent].Add($process)
    }

    $result = New-Object System.Collections.Generic.List[object]
    $queue = New-Object System.Collections.Generic.Queue[int]
    $seen = @{}
    $queue.Enqueue($RootProcessId)
    while ($queue.Count -gt 0) {
        $parent = $queue.Dequeue()
        if ($seen.ContainsKey($parent)) { continue }
        $seen[$parent] = $true
        if (-not $byParent.ContainsKey($parent)) { continue }

        foreach ($child in $byParent[$parent]) {
            if (-not $child.CreationDate) { continue }
            if ([datetime]$child.CreationDate -lt $RootStart) { continue }
            $childId = [int]$child.ProcessId
            if ($childId -eq $RootProcessId -or $seen.ContainsKey($childId)) { continue }
            $result.Add($child)
            $queue.Enqueue($childId)
        }
    }
    return @($result.ToArray())
}

function Initialize-CliplineProcessMetrics {
    if ('CliplineProcessMetricsNative' -as [type]) { return }
    if ($PSVersionTable.PSVersion.Major -ge 6 -and -not $IsWindows) {
        throw 'Clipline native process metrics require Windows'
    }

    Add-Type -TypeDefinition @"
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;

public static class CliplineProcessMetricsNative {
    [StructLayout(LayoutKind.Sequential)]
    private struct ProcessMemoryCountersEx2 {
        public uint cb;
        public uint PageFaultCount;
        public UIntPtr PeakWorkingSetSize;
        public UIntPtr WorkingSetSize;
        public UIntPtr QuotaPeakPagedPoolUsage;
        public UIntPtr QuotaPagedPoolUsage;
        public UIntPtr QuotaPeakNonPagedPoolUsage;
        public UIntPtr QuotaNonPagedPoolUsage;
        public UIntPtr PagefileUsage;
        public UIntPtr PeakPagefileUsage;
        public UIntPtr PrivateUsage;
        public UIntPtr PrivateWorkingSetSize;
        public ulong SharedCommitUsage;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct FileTime {
        public uint Low;
        public uint High;
    }

    public sealed class Snapshot {
        public long PrivateWorkingSetBytes;
        public long PrivateCommitBytes;
        public long WorkingSetBytes;
        public long CpuTime100ns;
        public uint HandleCount;
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr OpenProcess(uint access, bool inherit, int processId);
    [DllImport("psapi.dll", SetLastError = true)]
    private static extern bool GetProcessMemoryInfo(
        IntPtr process,
        out ProcessMemoryCountersEx2 counters,
        uint size);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool GetProcessHandleCount(IntPtr process, out uint count);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool GetProcessTimes(
        IntPtr process,
        out FileTime creation,
        out FileTime exit,
        out FileTime kernel,
        out FileTime user);
    [DllImport("kernel32.dll")]
    private static extern bool CloseHandle(IntPtr process);

    private static long ToLong(FileTime value) {
        return ((long)value.High << 32) | value.Low;
    }

    public static Snapshot Read(int processId) {
        const uint ProcessQueryLimitedInformation = 0x1000;
        IntPtr process = OpenProcess(ProcessQueryLimitedInformation, false, processId);
        if (process == IntPtr.Zero) {
            throw new Win32Exception(
                Marshal.GetLastWin32Error(),
                "OpenProcess failed for PID " + processId);
        }
        try {
            ProcessMemoryCountersEx2 counters = new ProcessMemoryCountersEx2();
            counters.cb = (uint)Marshal.SizeOf(typeof(ProcessMemoryCountersEx2));
            if (!GetProcessMemoryInfo(process, out counters, counters.cb)) {
                throw new Win32Exception(
                    Marshal.GetLastWin32Error(),
                    "GetProcessMemoryInfo(PROCESS_MEMORY_COUNTERS_EX2) failed for PID " + processId);
            }
            uint handles;
            if (!GetProcessHandleCount(process, out handles)) {
                throw new Win32Exception(
                    Marshal.GetLastWin32Error(),
                    "GetProcessHandleCount failed for PID " + processId);
            }
            FileTime creation;
            FileTime exit;
            FileTime kernel;
            FileTime user;
            if (!GetProcessTimes(process, out creation, out exit, out kernel, out user)) {
                throw new Win32Exception(
                    Marshal.GetLastWin32Error(),
                    "GetProcessTimes failed for PID " + processId);
            }
            return new Snapshot {
                PrivateWorkingSetBytes = (long)counters.PrivateWorkingSetSize,
                PrivateCommitBytes = (long)counters.PrivateUsage,
                WorkingSetBytes = (long)counters.WorkingSetSize,
                CpuTime100ns = ToLong(kernel) + ToLong(user),
                HandleCount = handles
            };
        } finally {
            CloseHandle(process);
        }
    }
}
"@
}

function Get-CliplineNativeProcessSnapshot {
    param([Parameter(Mandatory = $true)][int]$ProcessId)

    Initialize-CliplineProcessMetrics
    $native = [CliplineProcessMetricsNative]::Read($ProcessId)
    $process = Get-Process -Id $ProcessId -ErrorAction Stop
    return [pscustomobject][ordered]@{
        PrivateWorkingSetBytes = [long]$native.PrivateWorkingSetBytes
        PrivateCommitBytes = [long]$native.PrivateCommitBytes
        WorkingSetBytes = [long]$native.WorkingSetBytes
        CpuTime100ns = [long]$native.CpuTime100ns
        HandleCount = [long]$native.HandleCount
        ThreadCount = [long]$process.Threads.Count
    }
}

function Get-CliplineGpuProcessMemory {
    param(
        [Parameter(Mandatory = $true)][int[]]$ProcessIds,
        [scriptblock]$CounterReader = { param($paths) Get-Counter $paths -ErrorAction Stop }
    )

    $wanted = @{}
    foreach ($processId in $ProcessIds) { $wanted[[int]$processId] = $true }
    try {
        $counters = & $CounterReader @(
            '\GPU Process Memory(*)\Local Usage',
            '\GPU Process Memory(*)\Non Local Usage'
        )
        $local = 0L
        $nonLocal = 0L
        foreach ($sample in @($counters.CounterSamples)) {
            if ($sample.InstanceName -notmatch '^pid_(\d+)_') { continue }
            if (-not $wanted.ContainsKey([int]$Matches[1])) { continue }
            if ($sample.Path -like '*\Local Usage') {
                $local += [long]$sample.CookedValue
            } elseif ($sample.Path -like '*\Non Local Usage') {
                $nonLocal += [long]$sample.CookedValue
            }
        }
        return [pscustomobject][ordered]@{
            Available = $true
            LocalBytes = $local
            NonLocalBytes = $nonLocal
            Error = $null
        }
    } catch {
        return [pscustomobject][ordered]@{
            Available = $false
            LocalBytes = $null
            NonLocalBytes = $null
            Error = $_.Exception.Message
        }
    }
}

function Get-CliplineProcessRole {
    param([string]$ProcessName, [string]$CommandLine, [bool]$IsRoot)
    if ($IsRoot) { return 'root' }
    if ($ProcessName -eq 'msedgewebview2.exe') {
        if ($CommandLine -match '--type=gpu-process') { return 'webview-gpu' }
        if ($CommandLine -match '--type=renderer') { return 'webview-renderer' }
        if ($CommandLine -match '--type=utility') { return 'webview-utility' }
        return 'webview-other'
    }
    if ($ProcessName -eq 'ffmpeg.exe') { return 'ffmpeg' }
    return 'child'
}

Export-ModuleMember -Function @(
    'Get-CliplineSampleColumns',
    'Get-CliplinePercentile',
    'Get-CliplineMedian',
    'Get-CliplineCpuPercent',
    'Get-CliplineDescendantProcesses',
    'Initialize-CliplineProcessMetrics',
    'Get-CliplineNativeProcessSnapshot',
    'Get-CliplineGpuProcessMemory',
    'Get-CliplineProcessRole'
)
