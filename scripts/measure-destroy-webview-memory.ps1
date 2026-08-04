<#
.SYNOPSIS
    Acceptance harness for destroyable-WebView idle RAM (slim-core Task A5).

.DESCRIPTION
    Measures Clipline process-tree private working set and private commit after:
      1) cold --autostart settle (no WebView expected; telemetry only)
      2) recorder-stopped, no-WebView control (telemetry only)
      3) destroy-to-tray after a visible library/review session
      4) recreate -> destroy cycles (same-process rebound check)
      5) immediate close -> open race

    Hard gates (docs/superpowers/plans/2026-08-04-slim-core-webview-ffmpeg.md):
      - zero Clipline-owned msedgewebview2.exe children after autostart/destroy
      - settled tree private working set <= GateMiB (product budget; default 120)
      - third-cycle AND final destroy PWS/commit <= first-destroy + ReboundSlackMiB
      - close->open race and recreate cycles succeed

    Stretch (non-blocking): PWS <= StretchMiB (default 90).
    Absolute commit, cold/warm cross-process deltas, and recorder-stopped control are telemetry.
    Do NOT compare destroy commit to a killed cold --autostart process.

.EXAMPLE
    pwsh -File scripts/measure-destroy-webview-memory.ps1 -Exe target/release/clipline-app.exe
#>
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [int]$Runs = 3,
    [int]$GateMiB = 120,
    [int]$StretchMiB = 90,
    [int]$ReboundSlackMiB = 15,
    [int]$AutostartSettleSeconds = 100,
    [int]$DestroySettleSeconds = 90,
    [int]$ControlSettleSeconds = 60,
    [int]$DebugPort = 9333,
    [string]$OutCsv = "$env:TEMP\clipline-destroy-memory.csv"
)

$ErrorActionPreference = 'Stop'
$Exe = (Resolve-Path $Exe).Path

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class ClipDestroyMem {
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
            if ($child.CreationDate -and $child.CreationDate -ge $RootStart) {
                $tree.Add($child)
                $queue.Enqueue([int]$child.ProcessId)
            }
        }
    }

    $wvPws = 0L; $wvCommit = 0L; $count = 0
    $treePws = [ClipDestroyMem]::Pws($RootPid)
    $treeCommit = [ClipDestroyMem]::Commit($RootPid)
    foreach ($proc in $tree) {
        $pid_ = [int]$proc.ProcessId
        $pws = [ClipDestroyMem]::Pws($pid_)
        if ($pws -le 0) { continue }
        $treePws += $pws
        $treeCommit += [ClipDestroyMem]::Commit($pid_)
        if ($proc.Name -eq 'msedgewebview2.exe') {
            $wvPws += $pws
            $wvCommit += [ClipDestroyMem]::Commit($pid_)
            $count++
        }
    }
    [pscustomobject]@{
        TreePws = $treePws
        TreeCommit = $treeCommit
        WebViewPws = $wvPws
        WebViewCommit = $wvCommit
        WebViewProcesses = $count
    }
}

function Get-SettledSample {
    param([int]$RootPid, [datetime]$RootStart, [int]$Seconds)
    $samples = @()
    $iterations = [math]::Max(6, [math]::Ceiling($Seconds / 5))
    for ($i = 0; $i -lt $iterations; $i++) {
        $samples += Get-Sample -RootPid $RootPid -RootStart $RootStart
        Start-Sleep -Seconds 5
    }
    $tail = $samples | Select-Object -Last 6
    $medianPws = ($tail | ForEach-Object { $_.TreePws } | Sort-Object)[[math]::Floor($tail.Count / 2)]
    $medianCommit = ($tail | ForEach-Object { $_.TreeCommit } | Sort-Object)[[math]::Floor($tail.Count / 2)]
    $last = $samples[-1]
    [pscustomobject]@{
        MedianTreePws = $medianPws
        MedianTreeCommit = $medianCommit
        WebViewProcesses = $last.WebViewProcesses
        Samples = $samples
    }
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
        $socket.SendAsync((New-Object System.ArraySegment[byte] -ArgumentList @(, $bytes)),
            [System.Net.WebSockets.WebSocketMessageType]::Text, $true, $token).Wait(5000) | Out-Null
        $buffer = New-Object byte[] 32768
        $recv = $socket.ReceiveAsync((New-Object System.ArraySegment[byte] -ArgumentList @(, $buffer)), $token)
        $recv.Wait(10000) | Out-Null
        if ($recv.IsCompleted) {
            return [System.Text.Encoding]::UTF8.GetString($buffer, 0, $recv.Result.Count)
        }
    } finally { $socket.Dispose() }
    throw "CDP evaluate timed out: $Expression"
}

function Get-CdpValue {
    param([string]$Reply)
    if ($Reply -match '"value"\s*:\s*"((?:\\.|[^"\\])*)"') {
        return [System.Text.RegularExpressions.Regex]::Unescape($Matches[1])
    }
    if ($Reply -match '"value"\s*:\s*(true|false|null|-?\d+)') { return $Matches[1] }
    return ''
}

function Resolve-CliplinePage {
    param([int]$Port)
    $deadline = (Get-Date).AddSeconds(45)
    do {
        try {
            $targets = Invoke-RestMethod "http://127.0.0.1:$Port/json/list" -TimeoutSec 3
            $page = $targets | Where-Object {
                $_.type -eq 'page' -and ($_.url -match 'clipline|tauri|asset.localhost' -or $_.title -match 'Clipline')
            } | Select-Object -First 1
            if ($page) { return $page.webSocketDebuggerUrl }
        } catch {}
        Start-Sleep -Seconds 1
    } while ((Get-Date) -lt $deadline)
    throw "no Clipline page on port $Port"
}

function Stop-CliplineTree {
    param([System.Diagnostics.Process]$Proc)
    if ($null -eq $Proc) { return }
    try {
        Stop-Process -Id $Proc.Id -Force -ErrorAction SilentlyContinue
    } catch {}
    Get-Process clipline-app -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 3
}

function Ensure-MinimizeUsesDestroy {
    param([string]$WsUrl)
    $expr = @"
(async () => {
  const settings = await window.__TAURI__.core.invoke('get_settings');
  settings.minimize_to_tray = true;
  settings.close_to_tray = true;
  await window.__TAURI__.core.invoke('save_settings', { settings });
  return 'ok';
})()
"@
    $value = Get-CdpValue (Invoke-Cdp $WsUrl $expr)
    if ($value -ne 'ok') { throw "failed to enable minimize_to_tray ($value)" }
}

function Stop-Recorder {
    param([string]$WsUrl)
    $value = Get-CdpValue (Invoke-Cdp $WsUrl @"
(async () => {
  await window.__TAURI__.core.invoke('set_recording', { recording: false });
  return 'stopped';
})()
"@)
    if ($value -ne 'stopped') { throw "failed to stop recorder ($value)" }
}

function Open-LibraryAndClip {
    param([string]$WsUrl)
    $clicked = Get-CdpValue (Invoke-Cdp $WsUrl @"
(() => {
  const c = document.querySelector('.card-thumb');
  if (!c) return 'no-card';
  (c.closest('.card') || c).click();
  return 'clicked';
})()
"@)
    if ($clicked -eq 'clicked') {
        Start-Sleep -Seconds 3
        $played = Get-CdpValue (Invoke-Cdp $WsUrl @"
(async () => {
  const v = document.getElementById('video');
  if (!v) return 'no-video';
  try { await v.play(); } catch (e) { return 'play-failed:' + String(e); }
  return v.currentSrc ? 'playing' : 'no-src';
})()
"@)
        return $played
    }
    return $clicked
}

function Destroy-ToTray {
    param([string]$WsUrl, [int]$RootPid, [datetime]$RootStart)
    Invoke-Cdp $WsUrl "window.__TAURI__.core.invoke('minimize_main_window')" | Out-Null
    Start-Sleep -Seconds 4
    $deadline = (Get-Date).AddSeconds(20)
    do {
        $sample = Get-Sample -RootPid $RootPid -RootStart $RootStart
        if ($sample.WebViewProcesses -eq 0) { return }
        Start-Sleep -Seconds 1
    } while ((Get-Date) -lt $deadline)
}

function Open-FromDestroyed {
    param([string]$ExePath)
    $secondary = Start-Process -FilePath $ExePath -PassThru
    Start-Sleep -Seconds 2
    try { Stop-Process -Id $secondary.Id -Force -ErrorAction SilentlyContinue } catch {}
    Start-Sleep -Seconds 6
}

$results = @()
for ($run = 1; $run -le $Runs; $run++) {
    Write-Host "=== run $run/$Runs ==="

    Write-Host "cold --autostart (commit baseline)..."
    Remove-Item Env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS -ErrorAction SilentlyContinue
    $auto = Start-Process -FilePath $Exe -ArgumentList '--autostart' -PassThru
    try {
        Start-Sleep -Seconds 8
        if ($auto.HasExited) { throw "autostart exited early code=$($auto.ExitCode)" }
        $rootStart = (Get-CimInstance Win32_Process -Filter "ProcessId=$($auto.Id)").CreationDate
        $autoSettled = Get-SettledSample -RootPid $auto.Id -RootStart $rootStart -Seconds $AutostartSettleSeconds
        $autoPwsMiB = [math]::Round($autoSettled.MedianTreePws / 1MB, 1)
        $autoCommitMiB = [math]::Round($autoSettled.MedianTreeCommit / 1MB, 1)
        $autoStretch = $autoPwsMiB -le $StretchMiB
        $autoGate = ($autoSettled.WebViewProcesses -eq 0) -and ($autoPwsMiB -le $GateMiB)
        Write-Host ("  autostart tree PWS={0} MiB commit={1} MiB webviews={2} gate={3} stretch90={4}" -f $autoPwsMiB, $autoCommitMiB, $autoSettled.WebViewProcesses, $autoGate, $autoStretch)
    } finally {
        Stop-CliplineTree -Proc $auto
    }

    Write-Host "recorder-stopped no-WebView control (telemetry)..."
    $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$DebugPort"
    $control = Start-Process -FilePath $Exe -PassThru
    try {
        Start-Sleep -Seconds 12
        if ($control.HasExited) { throw "control launch exited early code=$($control.ExitCode)" }
        $controlPid = $control.Id
        $controlStart = (Get-CimInstance Win32_Process -Filter "ProcessId=$controlPid").CreationDate
        $ws = Resolve-CliplinePage -Port $DebugPort
        Ensure-MinimizeUsesDestroy -WsUrl $ws
        Stop-Recorder -WsUrl $ws
        Start-Sleep -Seconds 3
        Destroy-ToTray -WsUrl $ws -RootPid $controlPid -RootStart $controlStart
        $controlSettled = Get-SettledSample -RootPid $controlPid -RootStart $controlStart -Seconds $ControlSettleSeconds
        $controlPwsMiB = [math]::Round($controlSettled.MedianTreePws / 1MB, 1)
        $controlCommitMiB = [math]::Round($controlSettled.MedianTreeCommit / 1MB, 1)
        Write-Host ("  control tree PWS={0} MiB commit={1} MiB webviews={2} (telemetry)" -f $controlPwsMiB, $controlCommitMiB, $controlSettled.WebViewProcesses)
    } finally {
        Stop-CliplineTree -Proc $control
        Remove-Item Env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS -ErrorAction SilentlyContinue
    }

    Write-Host "visible -> destroy cycles..."
    $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$DebugPort"
    $proc = Start-Process -FilePath $Exe -PassThru
    try {
        Start-Sleep -Seconds 12
        if ($proc.HasExited) { throw "normal launch exited early code=$($proc.ExitCode)" }
        $rootPid = $proc.Id
        $rootStart = (Get-CimInstance Win32_Process -Filter "ProcessId=$rootPid").CreationDate
        $ws = Resolve-CliplinePage -Port $DebugPort
        Ensure-MinimizeUsesDestroy -WsUrl $ws
        $play = Open-LibraryAndClip -WsUrl $ws
        Write-Host "  library/clip probe: $play"
        Start-Sleep -Seconds 20
        $visible = Get-Sample -RootPid $rootPid -RootStart $rootStart

        Destroy-ToTray -WsUrl $ws -RootPid $rootPid -RootStart $rootStart
        $d1 = Get-SettledSample -RootPid $rootPid -RootStart $rootStart -Seconds $DestroySettleSeconds
        $d1Pws = [math]::Round($d1.MedianTreePws / 1MB, 1)
        $d1Commit = [math]::Round($d1.MedianTreeCommit / 1MB, 1)
        $d1Stretch = $d1Pws -le $StretchMiB
        $d1Gate = ($d1.WebViewProcesses -eq 0) -and ($d1Pws -le $GateMiB)
        Write-Host ("  destroy#1 tree PWS={0} MiB commit={1} MiB webviews={2} gate={3} stretch90={4}" -f $d1Pws, $d1Commit, $d1.WebViewProcesses, $d1Gate, $d1Stretch)

        Open-FromDestroyed -ExePath $Exe
        $ws = Resolve-CliplinePage -Port $DebugPort
        Ensure-MinimizeUsesDestroy -WsUrl $ws
        Destroy-ToTray -WsUrl $ws -RootPid $rootPid -RootStart $rootStart
        $d2 = Get-SettledSample -RootPid $rootPid -RootStart $rootStart -Seconds ([math]::Max(45, [math]::Floor($DestroySettleSeconds / 2)))

        Open-FromDestroyed -ExePath $Exe
        $ws = Resolve-CliplinePage -Port $DebugPort
        Ensure-MinimizeUsesDestroy -WsUrl $ws
        Destroy-ToTray -WsUrl $ws -RootPid $rootPid -RootStart $rootStart
        $d3 = Get-SettledSample -RootPid $rootPid -RootStart $rootStart -Seconds ([math]::Max(45, [math]::Floor($DestroySettleSeconds / 2)))
        $d3Pws = [math]::Round($d3.MedianTreePws / 1MB, 1)
        $d3Commit = [math]::Round($d3.MedianTreeCommit / 1MB, 1)
        $cycleGate = ($d2.WebViewProcesses -eq 0) -and ($d3.WebViewProcesses -eq 0) -and ($d3Pws -le ($d1Pws + $ReboundSlackMiB)) -and ($d3Commit -le ($d1Commit + $ReboundSlackMiB))
        Write-Host ("  destroy#3 tree PWS={0} MiB commit={1} MiB webviews={2} vsD1+{3} reboundOk={4}" -f $d3Pws, $d3Commit, $d3.WebViewProcesses, $ReboundSlackMiB, $cycleGate)

        Open-FromDestroyed -ExePath $Exe
        $ws = Resolve-CliplinePage -Port $DebugPort
        Ensure-MinimizeUsesDestroy -WsUrl $ws
        Invoke-Cdp $ws "window.__TAURI__.core.invoke('minimize_main_window')" | Out-Null
        Start-Sleep -Milliseconds 100
        Open-FromDestroyed -ExePath $Exe
        $ws = Resolve-CliplinePage -Port $DebugPort
        $raceVisible = Get-CdpValue (Invoke-Cdp $ws "String(document.visibilityState || 'unknown')")
        Start-Sleep -Seconds 3
        $raceSample = Get-Sample -RootPid $rootPid -RootStart $rootStart
        $raceOk = ($raceVisible -ne '') -and ($raceSample.WebViewProcesses -gt 0)
        Write-Host ("  race reopen visibility={0} webviews={1} ok={2}" -f $raceVisible, $raceSample.WebViewProcesses, $raceOk)

        Ensure-MinimizeUsesDestroy -WsUrl $ws
        Destroy-ToTray -WsUrl $ws -RootPid $rootPid -RootStart $rootStart
        $final = Get-SettledSample -RootPid $rootPid -RootStart $rootStart -Seconds ([math]::Max(60, [math]::Floor($DestroySettleSeconds * 2 / 3)))
        $finalPws = [math]::Round($final.MedianTreePws / 1MB, 1)
        $finalCommit = [math]::Round($final.MedianTreeCommit / 1MB, 1)
        $finalReboundOk = ($finalPws -le ($d1Pws + $ReboundSlackMiB)) -and ($finalCommit -le ($d1Commit + $ReboundSlackMiB))
        $finalStretch = $finalPws -le $StretchMiB
        $finalGate = ($final.WebViewProcesses -eq 0) -and ($finalPws -le $GateMiB) -and $finalReboundOk
        Write-Host ("  final destroy PWS={0} MiB commit={1} MiB webviews={2} vsD1 reboundOk={3} gate={4} stretch90={5}" -f $finalPws, $finalCommit, $final.WebViewProcesses, $finalReboundOk, $finalGate, $finalStretch)

        $results += [pscustomobject]@{
            Run = $run
            AutostartPwsMiB = $autoPwsMiB
            AutostartCommitMiB = $autoCommitMiB
            AutostartWebViews = $autoSettled.WebViewProcesses
            AutostartGate = $autoGate
            ControlPwsMiB = $controlPwsMiB
            ControlCommitMiB = $controlCommitMiB
            ControlWebViews = $controlSettled.WebViewProcesses
            VisibleTreePwsMiB = [math]::Round($visible.TreePws / 1MB, 1)
            Destroy1PwsMiB = $d1Pws
            Destroy1CommitMiB = $d1Commit
            Destroy1WebViews = $d1.WebViewProcesses
            Destroy1Gate = $d1Gate
            Destroy3PwsMiB = $d3Pws
            Destroy3CommitMiB = $d3Commit
            CycleNoRebound = $cycleGate
            FinalNoRebound = $finalReboundOk
            RaceOk = $raceOk
            FinalPwsMiB = $finalPws
            FinalCommitMiB = $finalCommit
            FinalWebViews = $final.WebViewProcesses
            FinalGate = $finalGate
            Stretch90 = ($autoStretch -and $d1Stretch -and $finalStretch)
            Gate = ($autoGate -and $d1Gate -and $cycleGate -and $raceOk -and $finalGate)
        }
    } finally {
        Stop-CliplineTree -Proc $proc
        Remove-Item Env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS -ErrorAction SilentlyContinue
    }
}

$results | Export-Csv $OutCsv -NoTypeInformation
$results | Format-Table -AutoSize
$passed = ($results | Where-Object Gate).Count
Write-Host ""
Write-Host "hard gates: WS<=${GateMiB} MiB, zero WebViews, cycle+final rebound <= first destroy +${ReboundSlackMiB} MiB; stretch WS<=${StretchMiB} MiB non-blocking: $passed/$($results.Count) runs passed"
Write-Host "csv: $OutCsv"
if ($passed -lt $results.Count) { exit 1 }
