# SPDX-License-Identifier: MIT OR Apache-2.0
# Run through ../test-print-window.ps1 for an external process timeout.
param([long]$Hwnd,[int]$ExpectedProcessId,[string]$OutputDirectory,
      [int]$Seconds,[int]$Fps,[int]$Flags,[switch]$ExpectMockMotion)
$ErrorActionPreference='Stop'
if ($Seconds -lt 1 -or $Seconds -gt 600 -or $Fps -lt 1 -or $Fps -gt 60 -or $Flags -notin @(0,2)) { throw 'Invalid worker limits.' }
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System; using System.Runtime.InteropServices;
public static class ReadbackNative {
 [StructLayout(LayoutKind.Sequential)] public struct Rect {public int L,T,R,B;}
 [StructLayout(LayoutKind.Sequential)] public struct Point {public int X,Y;}
 [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
 [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h,IntPtr dc,uint flags);
 [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h,out Rect r);
 [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h,ref Point p);
 [DllImport("user32.dll")] public static extern bool GetWindowDisplayAffinity(IntPtr h,out uint a);
 [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h,out uint p);
 [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
 [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
}
'@
$null=[ReadbackNative]::SetProcessDPIAware()
$target=Get-Process -Id $ExpectedProcessId
$started=$target.StartTime
if ($ExpectMockMotion -and $target.ProcessName -notin @('CliplineMockBlt','CliplineMockFlip')) { throw 'Mock motion mode requires a Clipline mock executable.' }
$timer=[Diagnostics.Stopwatch]::StartNew()
$reads=0; $errors=0; $changes=0; $validCounters=0; $previous=$null
$totalMs=0.0; $maxMs=0.0; $peakPrivate=0L; $last=$null; $midpoint=$false
$lastProgress=0.0; $stalled=$false; $invalidCounters=0
$log=New-Object IO.StreamWriter((Join-Path $OutputDirectory 'samples.jsonl'),$false)
$log.AutoFlush=$true
try {
    while ($timer.Elapsed.TotalSeconds -lt $Seconds) {
        $tick=[Diagnostics.Stopwatch]::StartNew()
        $bitmap=$null; $graphics=$null
        try {
            [uint32]$owner=0; [uint32]$affinity=0
            $null=[ReadbackNative]::GetWindowThreadProcessId([IntPtr]$Hwnd,[ref]$owner)
            if ($owner -ne $ExpectedProcessId -or (Get-Process -Id $ExpectedProcessId).StartTime -ne $started) { throw 'Target identity changed.' }
            if (-not [ReadbackNative]::GetWindowDisplayAffinity([IntPtr]$Hwnd,[ref]$affinity) -or $affinity -ne 0) { throw 'Target capture affinity unavailable or protected.' }
            if (-not [ReadbackNative]::IsWindowVisible([IntPtr]$Hwnd) -or [ReadbackNative]::IsIconic([IntPtr]$Hwnd)) { throw 'Target hidden or minimized.' }
            $r=New-Object ReadbackNative+Rect
            $origin=New-Object ReadbackNative+Point
            if (-not [ReadbackNative]::GetWindowRect([IntPtr]$Hwnd,[ref]$r) -or -not [ReadbackNative]::ClientToScreen([IntPtr]$Hwnd,[ref]$origin)) { throw 'Target geometry unavailable.' }
            $width=$r.R-$r.L; $height=$r.B-$r.T
            if ($width -lt 1 -or $height -lt 1 -or [long]$width*$height -gt 16777216) { throw 'Invalid target dimensions.' }
            $bitmap=New-Object Drawing.Bitmap($width,$height)
            $graphics=[Drawing.Graphics]::FromImage($bitmap)
            $graphics.Clear([Drawing.Color]::Magenta)
            $dc=$graphics.GetHdc()
            try {
                $readClock=[Diagnostics.Stopwatch]::StartNew()
                $ok=[ReadbackNative]::PrintWindow([IntPtr]$Hwnd,$dc,[uint32]$Flags)
                $readClock.Stop()
            } finally { $graphics.ReleaseHdc($dc) }
            if (-not $ok) { throw 'PrintWindow returned false.' }
            # Recheck protection/identity after the blocking call before saving.
            $null=[ReadbackNative]::GetWindowThreadProcessId([IntPtr]$Hwnd,[ref]$owner)
            if ($owner -ne $ExpectedProcessId -or (Get-Process -Id $ExpectedProcessId).StartTime -ne $started -or -not [ReadbackNative]::GetWindowDisplayAffinity([IntPtr]$Hwnd,[ref]$affinity) -or $affinity -ne 0) { throw 'Target identity or protection changed during read.' }
            $counter=$null
            if ($ExpectMockMotion) {
                # Check before a newer frame can reset the progress timestamp:
                # one long blocking read must not erase a capture gap.
                if ($timer.Elapsed.TotalSeconds-$lastProgress -gt 2) { $stalled=$true }
                $counter=0
                # Reject blank white/blue surfaces before interpreting binary bits.
                foreach ($anchor in @(@(40,40,255,0,0),@(120,40,0,0,255))) {
                    $x=$origin.X-$r.L+$anchor[0];$y=$origin.Y-$r.T+$anchor[1]
                    if ($x -lt 0 -or $x -ge $width -or $y -lt 0 -or $y -ge $height) { $counter=$null;break }
                    $pixel=$bitmap.GetPixel($x,$y)
                    if ($pixel.R -ne $anchor[2] -or $pixel.G -ne $anchor[3] -or $pixel.B -ne $anchor[4]) { $counter=$null;break }
                }
                for ($bit=0;$bit -lt 16;$bit++) {
                    if ($null -eq $counter) { break }
                    $x=$origin.X-$r.L+19+22*$bit; $y=$origin.Y-$r.T+110
                    if ($x -lt 0 -or $x -ge $width -or $y -lt 0 -or $y -ge $height) { $counter=$null; break }
                    $pixel=$bitmap.GetPixel($x,$y)
                    if ($pixel.R -eq 255 -and $pixel.G -eq 255 -and $pixel.B -eq 255) { $counter=$counter -bor (1 -shl $bit) }
                    elseif ($pixel.R -ne 0 -or $pixel.G -ne 0 -or $pixel.B -ne 255) { $counter=$null; break }
                }
                if ($null -ne $counter) {
                    $validCounters++
                    $delta=if ($null -ne $previous) { ($counter-$previous+65536)%65536 } else { 0 }
                    # Large backward/torn jumps do not count as evidence of motion.
                    if ($delta -gt 0 -and $delta -le 4096) { $changes++;$lastProgress=$timer.Elapsed.TotalSeconds }
                    elseif ($delta -gt 4096) { $invalidCounters++ }
                    $previous=$counter
                } else { $invalidCounters++ }
            }
            $readMs=$readClock.Elapsed.TotalMilliseconds
            $totalMs+=$readMs; $maxMs=[Math]::Max($maxMs,$readMs)
            $peakPrivate=[Math]::Max($peakPrivate,(Get-Process -Id $PID).PrivateMemorySize64)
            $log.WriteLine(([pscustomobject]@{elapsed_s=$timer.Elapsed.TotalSeconds;read_ms=$readMs;width=$width;height=$height;mock_counter=$counter;private_bytes=(Get-Process -Id $PID).PrivateMemorySize64} | ConvertTo-Json -Compress))
            if ($reads -eq 0) { $bitmap.Save((Join-Path $OutputDirectory 'first.bmp'),[Drawing.Imaging.ImageFormat]::Bmp) }
            if (-not $midpoint -and $timer.Elapsed.TotalSeconds -ge $Seconds/2) { $bitmap.Save((Join-Path $OutputDirectory 'middle.bmp'),[Drawing.Imaging.ImageFormat]::Bmp); $midpoint=$true }
            if ($null -ne $last) { $last.Dispose() }
            $last=$bitmap; $bitmap=$null; $reads++
        } catch {
            $errors++
            $log.WriteLine(([pscustomobject]@{elapsed_s=$timer.Elapsed.TotalSeconds;error=$_.Exception.Message} | ConvertTo-Json -Compress))
        } finally {
            if ($null -ne $graphics) { $graphics.Dispose() }
            if ($null -ne $bitmap) { $bitmap.Dispose() }
        }
        if ($ExpectMockMotion -and $timer.Elapsed.TotalSeconds-$lastProgress -gt 2) { $stalled=$true }
        $wait=[int][Math]::Max(0,(1000.0/$Fps)-$tick.Elapsed.TotalMilliseconds)
        if ($wait -gt 0) { Start-Sleep -Milliseconds $wait }
    }
    if ($null -ne $last) { $last.Save((Join-Path $OutputDirectory 'last.bmp'),[Drawing.Imaging.ImageFormat]::Bmp) }
    [pscustomobject]@{method='PrintWindow';flags=$Flags;reads=$reads;errors=$errors;valid_mock_counters=$validCounters;invalid_mock_counters=$invalidCounters;mock_counter_changes=$changes;mock_stalled_over_2s=$stalled;elapsed_s=$timer.Elapsed.TotalSeconds;mean_read_ms=$totalMs/[Math]::Max(1,$reads);max_read_ms=$maxMs;peak_private_bytes=$peakPrivate} | ConvertTo-Json | Set-Content (Join-Path $OutputDirectory 'summary.json')
    if ($reads -eq 0 -or ($ExpectMockMotion -and ($changes -eq 0 -or $stalled -or $invalidCounters -gt 0))) { throw 'No readable frames, invalid mock pixels, or expected mock motion stalled; inspect evidence.' }
} finally { $log.Dispose(); if ($null -ne $last) { $last.Dispose() } }
