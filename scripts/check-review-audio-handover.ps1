# Runtime check for the review audio handover (plan 2026-07-25-review-audio-handover).
# Drives a release build over CDP and reports the audible state of every media
# element. The races this guards are invisible to the pure/static tests: a stuck
# muted sidecar shows as `audible: 0`, and an unsettled claim as tok != settled.

param([string]$Exe)
$ErrorActionPreference = 'Stop'

$script:ws = $null
function Cdp([string]$expr) {
    if (-not $script:ws) {
        $t = Invoke-RestMethod "http://127.0.0.1:9222/json/list" -TimeoutSec 5
        $p = $t | Where-Object { $_.type -eq 'page' -and ($_.url -match 'clipline' -or $_.title -match 'Clipline') } | Select-Object -First 1
        if (-not $p) { throw "no Clipline page" }
        $script:ws = $p.webSocketDebuggerUrl
    }
    $s = New-Object System.Net.WebSockets.ClientWebSocket
    $ct = [System.Threading.CancellationToken]::None
    try {
        $s.ConnectAsync([Uri]$script:ws, $ct).Wait(5000) | Out-Null
        $msg = @{ id = 1; method = 'Runtime.evaluate'
            params = @{ expression = $expr; awaitPromise = $true; returnByValue = $true } } | ConvertTo-Json -Depth 6 -Compress
        $b = [System.Text.Encoding]::UTF8.GetBytes($msg)
        $s.SendAsync((New-Object System.ArraySegment[byte] -ArgumentList @(, $b)), 'Text', $true, $ct).Wait(5000) | Out-Null
        $sb = New-Object System.Text.StringBuilder
        $buf = New-Object byte[] 65536
        do {
            $r = $s.ReceiveAsync((New-Object System.ArraySegment[byte] -ArgumentList @(, $buf)), $ct)
            $r.Wait(12000) | Out-Null
            if (-not $r.IsCompleted) { break }
            [void]$sb.Append([System.Text.Encoding]::UTF8.GetString($buf, 0, $r.Result.Count))
        } while (-not $r.Result.EndOfMessage)
        return $sb.ToString()
    } finally { $s.Dispose() }
}

# Reports the audible state of every media element, which is what the races broke.
$PROBE = @'
(() => {
  const v = document.getElementById('video');
  const set = (typeof activeReviewAudioSidecars !== 'undefined') ? activeReviewAudioSidecars : [];
  const sc = set.map(s => ({ id: s.audioTrackId, muted: s.element.muted, paused: s.element.paused, seeking: s.element.seeking, t: Math.round(s.element.currentTime*1000)/1000, tok: s.seekToken, settled: s.settledToken }));
  return JSON.stringify({ mode: (typeof reviewAudioMode !== 'undefined') ? reviewAudioMode : '?', videoMuted: v.muted, videoPaused: v.paused, videoT: Math.round(v.currentTime*1000)/1000, sidecars: sc, audible: sc.filter(s => !s.muted).length });
})()
'@

Get-Process clipline-app -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 700
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=9222"
$proc = Start-Process -FilePath $Exe -PassThru
try {
    Start-Sleep -Seconds 16
    Cdp "(() => { const c = document.querySelector('.card-thumb'); (c.closest('.card')||c).click(); return 'ok'; })()" | Out-Null
    Start-Sleep -Seconds 5
    # Warm path: reopen so the handover wins the race against the initial seek.
    Cdp "document.getElementById('review-back')?.click()" | Out-Null
    Start-Sleep -Seconds 2
    Cdp "(() => { const c = document.querySelector('.card-thumb'); (c.closest('.card')||c).click(); return 'ok'; })()" | Out-Null
    Start-Sleep -Seconds 6

    Write-Host "`n--- after warm handover (expect video muted, >=1 audible sidecar) ---"
    ((Cdp $PROBE) -replace '.*"value":"', '' -replace '"\}\}\}$','') -replace '\\"','"'

    # Hammer overlapping seeks: this is the permanent-mute race.
    Write-Host "`n--- hammering 12 overlapping seeks ---"
    Cdp "(async () => { const v = document.getElementById('video'); for (let i = 0; i < 12; i++) { v.currentTime = 1 + i * 0.12; await new Promise(r => setTimeout(r, 25)); } return 'done'; })()" | Out-Null
    Start-Sleep -Seconds 4
    Write-Host "--- after seek storm (a stuck-muted sidecar would show audibleSidecars: 0) ---"
    ((Cdp $PROBE) -replace '.*"value":"', '' -replace '"\}\}\}$','') -replace '\\"','"'

    # And again while playing, which is where an audible seek would leak.
    Cdp "(async () => { const v = document.getElementById('video'); try { await v.play(); } catch(e) {} return 'playing'; })()" | Out-Null
    Start-Sleep -Seconds 2
    Cdp "(async () => { const v = document.getElementById('video'); for (let i = 0; i < 8; i++) { v.currentTime = 3 + i * 0.2; await new Promise(r => setTimeout(r, 30)); } return 'done'; })()" | Out-Null
    Start-Sleep -Seconds 4
    Write-Host "`n--- after seek storm while playing ---"
    ((Cdp $PROBE) -replace '.*"value":"', '' -replace '"\}\}\}$','') -replace '\\"','"'
} finally {
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
}

