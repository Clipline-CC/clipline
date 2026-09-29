# Controlled target for dwm_probe. This creates test windows, never controls a game.
# At 4s an opaque magenta window covers the target; 12s resize; 15s minimize;
# 18s restore. The target closes at 28s. Start the probe immediately after this.
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$form = New-Object System.Windows.Forms.Form
$form.Text = 'Clipline DWM Probe Target'
$form.ClientSize = New-Object System.Drawing.Size(640, 360)
$form.StartPosition = 'Manual'
$form.Location = New-Object System.Drawing.Point(180, 180)
$form.BackColor = [System.Drawing.Color]::Navy
$form.GetType().GetProperty('DoubleBuffered', [Reflection.BindingFlags]'NonPublic,Instance').SetValue($form, $true, $null)
$script:probeClock = [Diagnostics.Stopwatch]::StartNew()
$form.Add_Shown({
    # A hidden PowerShell host can pass its initial show state to the first form.
    $form.WindowState = 'Minimized'
    $form.WindowState = 'Normal'
    $script:probeClock.Restart()
})
$form.Add_Paint({
    param($sender, $event)
    $x = [int](($script:probeClock.ElapsedMilliseconds / 4) % [Math]::Max(1, $sender.ClientSize.Width - 90))
    $event.Graphics.FillRectangle([Drawing.Brushes]::Lime, $x, 90, 80, 80)
    $event.Graphics.FillRectangle([Drawing.Brushes]::Red, 0, 0, 60, 60)
    $event.Graphics.FillRectangle([Drawing.Brushes]::Blue, 60, 0, 60, 60)
    $event.Graphics.DrawString('DWM TEST: red / blue blocks, moving green square', $sender.Font, [Drawing.Brushes]::White, 10, 220)
})

$cover = New-Object System.Windows.Forms.Form
$cover.Text = 'Clipline DWM Probe Occluder'
$cover.BackColor = [Drawing.Color]::Magenta
$cover.StartPosition = 'Manual'
$cover.TopMost = $true
$cover.ShowInTaskbar = $false
$cover.ClientSize = New-Object Drawing.Size(700, 430)
$cover.Location = New-Object Drawing.Point(160, 160)
$script:probePhase = 0
$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 16
$timer.Add_Tick({
    $seconds = $script:probeClock.Elapsed.TotalSeconds
    $form.Invalidate()
    if ($seconds -ge 4 -and $script:probePhase -eq 0) {
        $cover.Show()
        $script:probePhase = 1
    }
    if ($seconds -ge 12 -and $script:probePhase -eq 1) {
        $cover.Hide()
        $form.ClientSize = New-Object Drawing.Size(800, 450)
        $script:probePhase = 2
    }
    if ($seconds -ge 15 -and $script:probePhase -eq 2) {
        $form.WindowState = 'Minimized'
        $script:probePhase = 3
    }
    if ($seconds -ge 18 -and $script:probePhase -eq 3) {
        $form.WindowState = 'Normal'
        $script:probePhase = 4
    }
    if ($seconds -ge 28) { $form.Close() }
})
try {
    $timer.Start()
    $null = $form.ShowDialog()
} finally {
    $timer.Stop()
    $timer.Dispose()
    $cover.Dispose()
    $form.Dispose()
}
