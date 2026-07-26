<#
.SYNOPSIS
    Samples Clipline memory at high cadence across repeated Save Replay operations.

.DESCRIPTION
    Launches Clipline with WebView2 remote debugging, records a settled baseline,
    invokes the real `save_replay` command, and samples root/child private working
    set plus private commit every 50-100 ms. GPU local/non-local allocations are
    captured separately before and after each save when Windows exposes the GPU
    Process Memory counters.

.EXAMPLE
    pwsh -File scripts/measure-save-replay-memory.ps1 `
      -Exe target/release/clipline-app.exe -Saves 5 -SampleIntervalMs 75
#>
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [ValidateRange(1, 100)][int]$Saves = 5,
    [ValidateRange(50, 100)][int]$SampleIntervalMs = 75,
    [ValidateRange(1, 60)][int]$BaselineSeconds = 5,
    [ValidateRange(1, 60)][int]$SettleSeconds = 3,
    [string]$OutCsv = "$env:TEMP\clipline-save-memory.csv",
    [ValidateRange(1024, 65535)][int]$DebugPort = 9223
)

$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @"
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
public class ClipSaveMem {
  [StructLayout(LayoutKind.Sequential)]
  public struct C { public uint cb; public uint pf;
    public UIntPtr peakWs; public UIntPtr ws; public UIntPtr qpp; public UIntPtr qp;
    public UIntPtr qpnp; public UIntPtr qnp; public UIntPtr pf2; public UIntPtr peakPf;
    public UIntPtr PrivateUsage; public UIntPtr PrivateWorkingSetSize; public ulong shared; }
  [DllImport("kernel32.dll", SetLastError = true)]
  public static extern IntPtr OpenProcess(uint a, bool i, int p);
  [DllImport("psapi.dll", SetLastError = true)]
  public static extern bool GetProcessMemoryInfo(IntPtr h, out C c, uint cb);
  [DllImport("kernel32.dll", SetLastError = true)]
  public static extern bool CloseHandle(IntPtr h);
  static C Read(int pid) {
    IntPtr h = OpenProcess(0x1000, false, pid);
    C c = new C(); c.cb = (uint)Marshal.SizeOf(typeof(C));
    if (h == IntPtr.Zero)
      throw new Win32Exception(Marshal.GetLastWin32Error(), "OpenProcess failed for PID " + pid);
    try {
      if (!GetProcessMemoryInfo(h, out c, c.cb))
        throw new Win32Exception(
          Marshal.GetLastWin32Error(),
          "GetProcessMemoryInfo(PROCESS_MEMORY_COUNTERS_EX2) failed for PID " + pid);
      return c;
    } finally {
      CloseHandle(h);
    }
  }
  public static long Pws(int pid) { return (long)Read(pid).PrivateWorkingSetSize; }
  public static long Commit(int pid) { return (long)Read(pid).PrivateUsage; }
}
"@

function Resolve-ProcessTree {
    param([int]$RootPid, [datetime]$RootStart)

    $all = Get-CimInstance Win32_Process
    $byParent = @{}
    foreach ($process in $all) {
        $parent = [int]$process.ParentProcessId
        if (-not $byParent.ContainsKey($parent)) { $byParent[$parent] = @() }
        $byParent[$parent] += $process
    }
    $result = New-Object System.Collections.Generic.List[int]
    $result.Add($RootPid)
    $queue = New-Object System.Collections.Generic.Queue[int]
    $queue.Enqueue($RootPid)
    while ($queue.Count) {
        $parent = $queue.Dequeue()
        foreach ($child in @($byParent[$parent])) {
            if ($child.CreationDate -and $child.CreationDate -ge $RootStart) {
                $pidValue = [int]$child.ProcessId
                $result.Add($pidValue)
                $queue.Enqueue($pidValue)
            }
        }
    }
    return $result.ToArray()
}

function Get-MemorySample {
    param([int]$RootPid, [int[]]$ProcessIds)

    $rootPws = [ClipSaveMem]::Pws($RootPid)
    $rootCommit = [ClipSaveMem]::Commit($RootPid)
    $childPws = 0L
    $childCommit = 0L
    foreach ($processId in $ProcessIds) {
        if ($processId -eq $RootPid) { continue }
        try {
            # A WebView2 utility process can exit after Resolve-ProcessTree
            # snapshots it. Read both values before adding either so a
            # partially sampled child does not skew the aggregate.
            $processPws = [ClipSaveMem]::Pws($processId)
            $processCommit = [ClipSaveMem]::Commit($processId)
            $childPws += $processPws
            $childCommit += $processCommit
        } catch {
            # Child churn is expected during a save. Root reads above remain
            # deliberately uncaught so an invalid run or unsupported counter
            # layout still fails loudly.
        }
    }
    return [pscustomobject]@{
        RootPws = $rootPws
        RootCommit = $rootCommit
        ChildPws = $childPws
        ChildCommit = $childCommit
    }
}

function Get-GpuProcessMemory {
    param([int[]]$ProcessIds)

    $wanted = @{}
    foreach ($processId in $ProcessIds) { $wanted[$processId] = $true }
    $local = 0L
    $nonLocal = 0L
    try {
        $counters = Get-Counter @(
            '\GPU Process Memory(*)\Local Usage',
            '\GPU Process Memory(*)\Non Local Usage'
        ) -ErrorAction Stop
        foreach ($sample in $counters.CounterSamples) {
            if ($sample.InstanceName -notmatch '^pid_(\d+)_') { continue }
            if (-not $wanted[[int]$Matches[1]]) { continue }
            if ($sample.Path -like '*\Local Usage') { $local += [long]$sample.CookedValue }
            elseif ($sample.Path -like '*\Non Local Usage') { $nonLocal += [long]$sample.CookedValue }
        }
    } catch {
        # GPU counters are optional (for example under some remote sessions).
    }
    return [pscustomobject]@{ Local = $local; NonLocal = $nonLocal }
}

function Invoke-Cdp {
    param([string]$WsUrl, [string]$Expression)

    $socket = New-Object System.Net.WebSockets.ClientWebSocket
    $token = [System.Threading.CancellationToken]::None
    try {
        $socket.ConnectAsync([Uri]$WsUrl, $token).Wait(5000) | Out-Null
        $payload = @{
            id = 1
            method = 'Runtime.evaluate'
            params = @{ expression = $Expression; awaitPromise = $true; returnByValue = $true }
        } | ConvertTo-Json -Depth 5 -Compress
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($payload)
        $segment = New-Object System.ArraySegment[byte] -ArgumentList @(, $bytes)
        $socket.SendAsync(
            $segment,
            [System.Net.WebSockets.WebSocketMessageType]::Text,
            $true,
            $token
        ).Wait(5000) | Out-Null
        $buffer = New-Object byte[] 16384
        $received = $socket.ReceiveAsync(
            (New-Object System.ArraySegment[byte] -ArgumentList @(, $buffer)),
            $token
        )
        $received.Wait(8000) | Out-Null
        if ($received.IsCompleted) {
            return [System.Text.Encoding]::UTF8.GetString(
                $buffer,
                0,
                $received.Result.Count
            )
        }
    } finally {
        $socket.Dispose()
    }
    throw "CDP evaluate timed out: $Expression"
}

function Get-CdpValue {
    param([string]$Reply)
    if ($Reply -match '"value"\s*:\s*"([^"]*)"') { return $Matches[1] }
    if ($Reply -match '"value"\s*:\s*(true|false|null|-?\d+)') { return $Matches[1] }
    return ''
}

function Resolve-CliplinePage {
    param([int]$Port)

    $targets = Invoke-RestMethod "http://127.0.0.1:$Port/json/list" -TimeoutSec 5
    $page = $targets | Where-Object {
        $_.type -eq 'page' -and ($_.url -match 'clipline' -or $_.title -match 'Clipline')
    } | Select-Object -First 1
    if (-not $page) { throw "no Clipline page on remote-debugging port $Port" }
    return $page.webSocketDebuggerUrl
}

function Add-Sample {
    param(
        [System.Collections.Generic.List[object]]$Rows,
        [int]$Save,
        [string]$Phase,
        [System.Diagnostics.Stopwatch]$Clock,
        [int]$RootPid,
        [int[]]$ProcessIds
    )

    $sample = Get-MemorySample -RootPid $RootPid -ProcessIds $ProcessIds
    $Rows.Add([pscustomobject]@{
        Save = $Save
        Phase = $Phase
        ElapsedMs = $Clock.ElapsedMilliseconds
        RootPwsBytes = $sample.RootPws
        RootCommitBytes = $sample.RootCommit
        ChildPwsBytes = $sample.ChildPws
        ChildCommitBytes = $sample.ChildCommit
    })
}

$resolvedExe = (Resolve-Path -LiteralPath $Exe).Path
$existingClipline = @(Get-Process -Name 'clipline-app' -ErrorAction SilentlyContinue)
if ($existingClipline.Count -gt 0) {
    $existingIds = ($existingClipline.Id | Sort-Object) -join ', '
    throw "Stop existing Clipline processes before measuring (PID: $existingIds)."
}
$oldBrowserArgs = $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$DebugPort"
$rows = New-Object System.Collections.Generic.List[object]
$summaries = @()
$process = Start-Process -FilePath $resolvedExe -PassThru
try {
    Start-Sleep -Seconds 16
    $rootPid = $process.Id
    $rootStart = (Get-CimInstance Win32_Process -Filter "ProcessId=$rootPid").CreationDate
    $webSocket = Resolve-CliplinePage -Port $DebugPort
    $processIds = Resolve-ProcessTree -RootPid $rootPid -RootStart $rootStart

    for ($save = 1; $save -le $Saves; $save++) {
        # Refresh between saves so a recorder child started after initial UI
        # readiness is still included without putting CIM work in the 75 ms
        # sampling loop.
        $processIds = Resolve-ProcessTree -RootPid $rootPid -RootStart $rootStart
        $clock = [System.Diagnostics.Stopwatch]::StartNew()
        $baselineSamples = [math]::Ceiling(($BaselineSeconds * 1000) / $SampleIntervalMs)
        for ($index = 0; $index -lt $baselineSamples; $index++) {
            Add-Sample $rows $save 'baseline' $clock $rootPid $processIds
            Start-Sleep -Milliseconds $SampleIntervalMs
        }
        $baselineRoot = ($rows | Where-Object {
            $_.Save -eq $save -and $_.Phase -eq 'baseline'
        } | Measure-Object RootPwsBytes -Average).Average
        $baselineCommit = ($rows | Where-Object {
            $_.Save -eq $save -and $_.Phase -eq 'baseline'
        } | Measure-Object RootCommitBytes -Average).Average
        $gpuBefore = Get-GpuProcessMemory -ProcessIds $processIds

        $started = Get-CdpValue (Invoke-Cdp $webSocket @"
(() => {
  globalThis.__cliplineSaveMemory = { done: false, error: "" };
  window.__TAURI__.core.invoke("save_replay")
    .then(() => { globalThis.__cliplineSaveMemory.done = true; })
    .catch((error) => {
      globalThis.__cliplineSaveMemory.error = String(error);
      globalThis.__cliplineSaveMemory.done = true;
    });
  return "started";
})()
"@)
        if ($started -ne 'started') { throw "save $save did not start ($started)" }

        do {
            Add-Sample $rows $save 'save' $clock $rootPid $processIds
            Start-Sleep -Milliseconds $SampleIntervalMs
            $saveState = Get-CdpValue (Invoke-Cdp $webSocket @"
(() => {
  const state = globalThis.__cliplineSaveMemory;
  if (!state || !state.done) return "running";
  return state.error ? "error:" + state.error : "done";
})()
"@)
        } while ($saveState -eq 'running')
        if ($saveState -ne 'done') { throw "save $save failed: $saveState" }

        $settleSamples = [math]::Ceiling(($SettleSeconds * 1000) / $SampleIntervalMs)
        for ($index = 0; $index -lt $settleSamples; $index++) {
            Add-Sample $rows $save 'settle' $clock $rootPid $processIds
            Start-Sleep -Milliseconds $SampleIntervalMs
        }
        $saveRows = $rows | Where-Object { $_.Save -eq $save -and $_.Phase -eq 'save' }
        $peakRoot = ($saveRows | Measure-Object RootPwsBytes -Maximum).Maximum
        $peakCommit = ($saveRows | Measure-Object RootCommitBytes -Maximum).Maximum
        $gpuAfter = Get-GpuProcessMemory -ProcessIds $processIds
        $summaries += [pscustomobject]@{
            Save = $save
            Samples = $saveRows.Count
            RootPwsBaselineMiB = [math]::Round($baselineRoot / 1MB, 1)
            RootPwsPeakMiB = [math]::Round($peakRoot / 1MB, 1)
            RootPwsDeltaMiB = [math]::Round(($peakRoot - $baselineRoot) / 1MB, 1)
            RootCommitBaselineMiB = [math]::Round($baselineCommit / 1MB, 1)
            RootCommitPeakMiB = [math]::Round($peakCommit / 1MB, 1)
            RootCommitDeltaMiB = [math]::Round(($peakCommit - $baselineCommit) / 1MB, 1)
            GpuLocalBeforeMiB = [math]::Round($gpuBefore.Local / 1MB, 1)
            GpuLocalAfterMiB = [math]::Round($gpuAfter.Local / 1MB, 1)
            GpuNonLocalBeforeMiB = [math]::Round($gpuBefore.NonLocal / 1MB, 1)
            GpuNonLocalAfterMiB = [math]::Round($gpuAfter.NonLocal / 1MB, 1)
        }
    }
} finally {
    if ($process -and -not $process.HasExited) {
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
    }
    $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = $oldBrowserArgs
}

$rows | Export-Csv -LiteralPath $OutCsv -NoTypeInformation
$summaries | Format-Table -AutoSize
Write-Host "samples: $OutCsv"
