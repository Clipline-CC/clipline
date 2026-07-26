<#
.SYNOPSIS
    Measures how much memory Clipline's WebView2 gives back while hidden in the tray.

.DESCRIPTION
    The acceptance harness for the tray-hide memory work (see
    docs/superpowers/plans/2026-07-24-memory-footprint-reduction.md, Tasks 4 and 4b).
    The in-app RAM meter cannot measure this: ui/main.js only polls while
    `!document.hidden`, so by the time it samples, the window is visible again.

    Per run it launches a release build with remote debugging, inflates WebView2 to
    the state where the cost exists (library rendered + a clip decoding), takes a
    stable visible baseline, hides via the app's real tray path, then samples the
    hidden state and compares the final-30s median.

    Two measurement traps this deliberately avoids:

    * Metric. Committed private bytes do not move when Windows trims a hidden
      process, so they cannot distinguish "decommitted" from "paged out". Private
      working set is the primary figure; commit is recorded alongside it so the
      two can be told apart.
    * PID reuse. A bare parent/child walk can sweep in unrelated processes -- dev
      machines routinely run many msedgewebview2.exe belonging to other apps. Every
      candidate must be a descendant AND have started at or after the app root.

.PARAMETER Exe
    Path to the release clipline-app.exe.

.PARAMETER Runs
    Clean launches to perform. The plan requires at least 3.

.PARAMETER GateMiB
    Required drop in aggregate WebView2 private working set.

.EXAMPLE
    pwsh -File scripts/measure-hidden-webview-memory.ps1 -Exe target/release/clipline-app.exe
#>
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [int]$Runs = 3,
    [int]$GateMiB = 40,
    [int]$HiddenSeconds = 120,
    [string]$OutCsv = "$env:TEMP\clipline-hidden-memory.csv",
    [int]$DebugPort = 9222
)

$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class ClipMem {
  [StructLayout(LayoutKind.Sequential)]
  public struct C { public uint cb; public uint pf;
    public UIntPtr peakWs; public UIntPtr ws; public UIntPtr qpp; public UIntPtr qp;
    public UIntPtr qpnp; public UIntPtr qnp; public UIntPtr pf2; public UIntPtr peakPf;
    public UIntPtr PrivateUsage; public UIntPtr PrivateWorkingSetSize; public ulong shared; }
  [DllImport("kernel32.dll")] public static extern IntPtr OpenProcess(uint a, bool i, int p);
  [DllImport("psapi.dll")] public static extern bool GetProcessMemoryInfo(IntPtr h, out C c, uint cb);
  [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
  static C Read(int pid) {
    IntPtr h = OpenProcess(0x1000, false, pid);
    C c = new C(); c.cb = (uint)Marshal.SizeOf(typeof(C));
    if (h == IntPtr.Zero) return c;
    GetProcessMemoryInfo(h, out c, c.cb); CloseHandle(h); return c; }
  public static long Pws(int pid) { return (long)Read(pid).PrivateWorkingSetSize; }
  public static long Commit(int pid) { return (long)Read(pid).PrivateUsage; }
}
"@

function Get-Sample {
    param([int]$RootPid, [datetime]$RootStart)

    $all = Get-CimInstance Win32_Process
    $tree = New-Object System.Collections.Generic.List[object]
    $queue = New-Object System.Collections.Generic.Queue[int]
    $queue.Enqueue($RootPid)
    $seen = @{}
    while ($queue.Count) {
        $current = $queue.Dequeue()
        if ($seen[$current]) { continue }
        $seen[$current] = $true
        foreach ($child in ($all | Where-Object { [int]$_.ParentProcessId -eq $current })) {
            # Descendant AND started no earlier than the root: without this, PID
            # reuse pulls in other applications' processes.
            if ($child.CreationDate -and $child.CreationDate -ge $RootStart) {
                $tree.Add($child)
                $queue.Enqueue([int]$child.ProcessId)
            }
        }
    }

    $wvPws = 0; $wvCommit = 0; $gpu = 0; $renderer = 0; $count = 0
    $treePws = [ClipMem]::Pws($RootPid)
    $treeCommit = [ClipMem]::Commit($RootPid)
    foreach ($proc in $tree) {
        $pid_ = [int]$proc.ProcessId
        $pws = [ClipMem]::Pws($pid_)
        if ($pws -le 0) { continue }
        $treePws += $pws
        $treeCommit += [ClipMem]::Commit($pid_)
        if ($proc.Name -eq 'msedgewebview2.exe') {
            $wvPws += $pws
            $wvCommit += [ClipMem]::Commit($pid_)
            $count++
            if ($proc.CommandLine -match '--type=gpu-process') { $gpu += $pws }
            elseif ($proc.CommandLine -match '--type=renderer') { $renderer += $pws }
        }
    }
    [pscustomobject]@{
        WebViewPws = $wvPws; WebViewCommit = $wvCommit; Gpu = $gpu; Renderer = $renderer
        TreePws = $treePws; TreeCommit = $treeCommit; WebViewProcesses = $count
    }
}

function Invoke-Cdp {
    param([string]$WsUrl, [string]$Expression)

    $socket = New-Object System.Net.WebSockets.ClientWebSocket
    $token = [System.Threading.CancellationToken]::None
    try {
        $socket.ConnectAsync([Uri]$WsUrl, $token).Wait(5000) | Out-Null
        $payload = @{
            id     = 1
            method = 'Runtime.evaluate'
            params = @{ expression = $Expression; awaitPromise = $true; returnByValue = $true }
        } | ConvertTo-Json -Depth 5 -Compress
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($payload)
        $socket.SendAsync((New-Object System.ArraySegment[byte] -ArgumentList @(, $bytes)),
            [System.Net.WebSockets.WebSocketMessageType]::Text, $true, $token).Wait(5000) | Out-Null
        $buffer = New-Object byte[] 16384
        $recv = $socket.ReceiveAsync((New-Object System.ArraySegment[byte] -ArgumentList @(, $buffer)), $token)
        $recv.Wait(8000) | Out-Null
        if ($recv.IsCompleted) {
            return [System.Text.Encoding]::UTF8.GetString($buffer, 0, $recv.Result.Count)
        }
    } finally { $socket.Dispose() }
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
    # Select by Clipline identity: the port can host other WebView2 pages, and
    # "first page wins" would silently drive the wrong application.
    $targets = Invoke-RestMethod "http://127.0.0.1:$Port/json/list" -TimeoutSec 5
    $page = $targets | Where-Object {
        $_.type -eq 'page' -and ($_.url -match 'clipline' -or $_.title -match 'Clipline')
    } | Select-Object -First 1
    if (-not $page) {
        throw "no Clipline page on port $Port (found: $(($targets | ForEach-Object { $_.url }) -join ', '))"
    }
    return $page.webSocketDebuggerUrl
}

$results = @()
for ($run = 1; $run -le $Runs; $run++) {
    Write-Host "run $run/$Runs..."
    $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$DebugPort"
    $proc = Start-Process -FilePath $Exe -PassThru
    try {
        Start-Sleep -Seconds 16
        $rootPid = $proc.Id
        $rootStart = (Get-CimInstance Win32_Process -Filter "ProcessId=$rootPid").CreationDate
        $ws = Resolve-CliplinePage -Port $DebugPort

        $cold = Get-Sample -RootPid $rootPid -RootStart $rootStart

        # Inflate: a cold app has almost no compositor or decode state, so
        # measuring there understates the fix by an order of magnitude.
        $clicked = Get-CdpValue (Invoke-Cdp $ws @"
(() => { const c = document.querySelector('.card-thumb');
  if (!c) return 'no-card'; (c.closest('.card') || c).click(); return 'clicked'; })()
"@)
        if ($clicked -ne 'clicked') { throw "run ${run}: could not open a clip ($clicked)" }
        Start-Sleep -Seconds 4
        $played = Get-CdpValue (Invoke-Cdp $ws @"
(async () => { const v = document.getElementById('video');
  if (!v) return 'no-video';
  try { await v.play(); } catch (e) { return 'play-failed'; }
  return v.currentSrc ? 'playing' : 'no-src'; })()
"@)
        if ($played -ne 'playing') { throw "run ${run}: playback did not start ($played)" }
        Start-Sleep -Seconds 25

        $loaded = Get-Sample -RootPid $rootPid -RootStart $rootStart

        $baseline = @()
        for ($i = 0; $i -lt 6; $i++) {
            $baseline += Get-Sample -RootPid $rootPid -RootStart $rootStart
            Start-Sleep -Seconds 5
        }
        $baseWv = ($baseline[3..5] | ForEach-Object { $_.WebViewPws } | Measure-Object -Average).Average
        $baseCommit = ($baseline[3..5] | ForEach-Object { $_.WebViewCommit } | Measure-Object -Average).Average

        # Hide through the app's real tray path. Calling ShowWindow from outside
        # would hide only the native window and bypass what is under test.
        Invoke-Cdp $ws "window.__TAURI__.core.invoke('minimize_main_window')" | Out-Null
        Start-Sleep -Seconds 3
        # Assert the tray path was actually taken, not a taskbar minimise.
        $visible = Get-CdpValue (Invoke-Cdp $ws "String(document.visibilityState)")
        $trayConfirmed = (Get-Process -Id $rootPid).MainWindowHandle -eq 0
        if (-not $trayConfirmed) {
            throw "run ${run}: window still has a handle -- minimize_to_tray may be off (visibilityState=$visible)"
        }

        $samples = @()
        $iterations = [math]::Ceiling($HiddenSeconds / 5)
        for ($i = 0; $i -lt $iterations; $i++) {
            $samples += Get-Sample -RootPid $rootPid -RootStart $rootStart
            Start-Sleep -Seconds 5
        }
        $tail = $samples | Select-Object -Last 6
        $medianWv = ($tail | ForEach-Object { $_.WebViewPws } | Sort-Object)[[math]::Floor($tail.Count / 2)]
        $medianCommit = ($tail | ForEach-Object { $_.WebViewCommit } | Sort-Object)[[math]::Floor($tail.Count / 2)]
        $last = $samples[-1]

        $deltaPws = ($baseWv - $medianWv) / 1MB
        $results += [pscustomobject]@{
            Run                 = $run
            WebViewProcesses    = $last.WebViewProcesses
            ColdWvMiB           = [math]::Round($cold.WebViewPws / 1MB, 1)
            LoadedWvMiB         = [math]::Round($loaded.WebViewPws / 1MB, 1)
            BaseWvMiB           = [math]::Round($baseWv / 1MB, 1)
            HiddenWvMedianMiB   = [math]::Round($medianWv / 1MB, 1)
            DeltaPwsMiB         = [math]::Round($deltaPws, 1)
            # Commit alongside working set: if commit barely moves while working
            # set collapses, the memory was trimmed rather than decommitted.
            BaseWvCommitMiB     = [math]::Round($baseCommit / 1MB, 1)
            HiddenWvCommitMiB   = [math]::Round($medianCommit / 1MB, 1)
            DeltaCommitMiB      = [math]::Round(($baseCommit - $medianCommit) / 1MB, 1)
            GpuBaseMiB          = [math]::Round($baseline[5].Gpu / 1MB, 1)
            GpuHiddenMiB        = [math]::Round($last.Gpu / 1MB, 1)
            TreePwsBaseMiB      = [math]::Round($baseline[5].TreePws / 1MB, 1)
            TreePwsHiddenMiB    = [math]::Round($last.TreePws / 1MB, 1)
            Gate                = ($deltaPws -ge $GateMiB)
        }
    } finally {
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 5
    }
}

$results | Export-Csv $OutCsv -NoTypeInformation
$results | Format-Table -AutoSize

$passed = ($results | Where-Object Gate).Count
$median = ($results | ForEach-Object { $_.DeltaPwsMiB } | Sort-Object)[[math]::Floor($results.Count / 2)]
Write-Host ""
Write-Host "gate ${GateMiB} MiB: $passed/$($results.Count) runs passed, median $median MiB"
Write-Host "csv: $OutCsv"
if ($passed -lt $results.Count) { exit 1 }
