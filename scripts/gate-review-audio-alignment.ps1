<#
.SYNOPSIS
    Gate: no review sidecar may be audible while its alignment is outstanding.

.DESCRIPTION
    Asserts and exits non-zero. Written for the rebuild in
    docs/superpowers/plans/2026-07-26-review-audio-alignment.md, and deliberately
    designed to fail on the state of `develop` that preceded it.

    The violation is a *sample*, not a snapshot: an element observed with
    `muted === false` while `seeking === true` was audible during a seek, and can
    emit already-decoded pre-seek audio. A later snapshot cannot see this, which is
    why an earlier harness reported clean while the leak was present.

    Silence fails. A CDP error, an empty sidecar set, a missing clip, or a page that
    is not Clipline are all failures rather than vacuous passes — two previous
    harnesses reported success while measuring nothing at all.

.PARAMETER Exe
    Path to a release clipline-app.exe.
#>
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [int]$DebugPort = 9222,
    [int]$SampleMs = 4
)

$ErrorActionPreference = 'Stop'
$script:failures = New-Object System.Collections.Generic.List[string]
function Fail([string]$m) { $script:failures.Add($m); Write-Host "  FAIL  $m" }
function Pass([string]$m) { Write-Host "  ok    $m" }

$script:ws = $null
function Cdp([string]$expr) {
    if (-not $script:ws) {
        try { $t = Invoke-RestMethod "http://127.0.0.1:$DebugPort/json/list" -TimeoutSec 5 }
        catch { throw "CDP unreachable on port $DebugPort : $_" }
        $p = $t | Where-Object {
            $_.type -eq 'page' -and ($_.url -match 'clipline' -or $_.title -match 'Clipline')
        } | Select-Object -First 1
        if (-not $p) { throw "no Clipline page on port $DebugPort" }
        $script:ws = $p.webSocketDebuggerUrl
    }
    $s = New-Object System.Net.WebSockets.ClientWebSocket
    $ct = [System.Threading.CancellationToken]::None
    try {
        $s.ConnectAsync([Uri]$script:ws, $ct).Wait(5000) | Out-Null
        $msg = @{ id = 1; method = 'Runtime.evaluate'
            params = @{ expression = $expr; awaitPromise = $true; returnByValue = $true } } |
            ConvertTo-Json -Depth 6 -Compress
        $b = [System.Text.Encoding]::UTF8.GetBytes($msg)
        $s.SendAsync((New-Object System.ArraySegment[byte] -ArgumentList @(, $b)), 'Text', $true, $ct).Wait(5000) | Out-Null
        $sb = New-Object System.Text.StringBuilder
        $buf = New-Object byte[] 262144
        do {
            $r = $s.ReceiveAsync((New-Object System.ArraySegment[byte] -ArgumentList @(, $buf)), $ct)
            $r.Wait(20000) | Out-Null
            if (-not $r.IsCompleted) { throw "CDP receive timed out for: $expr" }
            [void]$sb.Append([System.Text.Encoding]::UTF8.GetString($buf, 0, $r.Result.Count))
        } while (-not $r.Result.EndOfMessage)
        $raw = $sb.ToString()
        # An exception inside the page is a gate failure, never a silent empty result.
        if ($raw -match '"exceptionDetails"') { throw "page threw evaluating: $expr`n$raw" }
        $parsed = $raw | ConvertFrom-Json
        return $parsed.result.result.value
    } finally { $s.Dispose() }
}

# --- in-page sampler -------------------------------------------------------
# Polls every sidecar plus the video, recording each `muted` transition and every
# sample taken while an element is seeking. Sidecars are created with `new Audio()`
# and never appended to the DOM, so they are reachable only through the module
# state -- which works because the UI loads as classic scripts.
$INSTALL_SAMPLER = @"
(() => {
  if (window.__alignmentGate) { window.__alignmentGate.stop(); }
  const state = {
    samples: 0, violations: [], transitions: [], errors: [],
    started: performance.now(), timer: 0, last: new Map(),
  };
  const read = () => {
    const set = (typeof activeReviewAudioSidecars !== 'undefined') ? activeReviewAudioSidecars : [];
    const mode = (typeof reviewAudioMode !== 'undefined') ? reviewAudioMode : null;
    return { set, mode };
  };
  const tick = () => {
    try {
      const { set, mode } = read();
      state.samples++;
      for (const s of set) {
        const a = s.element;
        const key = s.audioTrackId;
        const now = { muted: a.muted, seeking: a.seeking, t: a.currentTime };
        const prev = state.last.get(key);
        if (!prev || prev.muted !== now.muted) {
          state.transitions.push({
            ms: Math.round(performance.now() - state.started),
            id: key, muted: now.muted, seeking: now.seeking,
            t: Math.round(now.t * 1000) / 1000, mode,
          });
        }
        // The violation: audible while its own seek is still in flight.
        if (now.muted === false && now.seeking === true) {
          state.violations.push({
            ms: Math.round(performance.now() - state.started),
            id: key, t: Math.round(now.t * 1000) / 1000, mode,
          });
        }
        state.last.set(key, now);
      }
    } catch (e) { state.errors.push(String(e)); }
  };
  state.timer = window.setInterval(tick, $SampleMs);
  window.__alignmentGate = {
    stop: () => window.clearInterval(state.timer),
    report: () => ({
      samples: state.samples,
      violations: state.violations,
      transitions: state.transitions.length,
      errors: state.errors,
    }),
  };
  return 'installed';
})()
"@

Get-Process clipline-app -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 700
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$DebugPort"
$proc = Start-Process -FilePath $Exe -PassThru
try {
    Start-Sleep -Seconds 16
    Write-Host "gate: review audio alignment"

    # --- open a clip through the real UI ---------------------------------------
    $opened = Cdp "(() => { const c = document.querySelector('.card-thumb'); if (!c) return 'no-card'; (c.closest('.card')||c).click(); return 'clicked'; })()"
    if ($opened -ne 'clicked') { Fail "could not open a clip through the library grid ($opened)"; throw "setup failed" }
    Start-Sleep -Seconds 5
    # Warm path: reopen so the handover is fast, which is the ordering that bites.
    Cdp "(() => { const b = document.getElementById('review-back'); if (b) b.click(); return 'back'; })()" | Out-Null
    Start-Sleep -Seconds 2
    Cdp "(() => { const c = document.querySelector('.card-thumb'); (c.closest('.card')||c).click(); return 'ok'; })()" | Out-Null
    Start-Sleep -Seconds 6

    # Silence must fail: an empty sidecar set means the gate measured nothing.
    $count = [int](Cdp "(typeof activeReviewAudioSidecars !== 'undefined') ? activeReviewAudioSidecars.length : 0")
    if ($count -lt 1) {
        Fail "no active sidecars: the gate would measure nothing (clip may have no audio tracks)"
        throw "setup failed"
    }
    Pass "active sidecars: $count"

    if ((Cdp $INSTALL_SAMPLER) -ne 'installed') { Fail "sampler did not install"; throw "setup failed" }
    Pass "sampler installed at ${SampleMs}ms"

    # --- exercise: scrub while playing, through the seek bar -------------------
    Cdp "(async () => { const v = document.getElementById('video'); try { await v.play(); } catch (e) {} return 'playing'; })()" | Out-Null
    Start-Sleep -Seconds 2
    # Real UI path: drive the timeline element the user drags, not video.currentTime.
    Cdp @"
(async () => {
  const bar = document.getElementById('timeline') || document.querySelector('.timeline');
  for (let i = 0; i < 10; i++) {
    const frac = 0.1 + i * 0.06;
    if (bar && bar.getBoundingClientRect) {
      const r = bar.getBoundingClientRect();
      const x = r.left + r.width * frac;
      const y = r.top + r.height / 2;
      for (const type of ['pointerdown', 'pointermove', 'pointerup']) {
        bar.dispatchEvent(new PointerEvent(type, { clientX: x, clientY: y, bubbles: true, pointerId: 1 }));
      }
    } else if (typeof seekTo === 'function') {
      seekTo(frac * ((typeof clipDuration === 'function') ? clipDuration() : 30));
    } else {
      return 'no-seek-path';
    }
    await new Promise(r => setTimeout(r, 90));
  }
  return 'scrubbed';
})()
"@ | Out-Null
    Start-Sleep -Seconds 3

    # --- exercise: track switch mid-playback ----------------------------------
    Cdp @"
(() => {
  const boxes = [...document.querySelectorAll('#audio-track-list input[type=checkbox]')];
  if (boxes.length < 2) return 'single-track';
  boxes[boxes.length - 1].click();
  return 'toggled';
})()
"@ | Out-Null
    Start-Sleep -Seconds 5

    # --- collect ---------------------------------------------------------------
    Cdp "window.__alignmentGate.stop()" | Out-Null
    $report = Cdp "JSON.stringify(window.__alignmentGate.report())" | ConvertFrom-Json

    Write-Host ""
    Write-Host "samples: $($report.samples)   muted transitions: $($report.transitions)   violations: $($report.violations.Count)"

    if ($report.errors.Count -gt 0) { Fail "sampler recorded page errors: $($report.errors -join '; ')" }
    if ($report.samples -lt 100) { Fail "only $($report.samples) samples taken; the sampler did not run" }
    else { Pass "sampler took $($report.samples) samples" }

    if ($report.violations.Count -gt 0) {
        Fail "$($report.violations.Count) sample(s) with an audible sidecar mid-seek"
        $report.violations | Select-Object -First 8 | ForEach-Object {
            Write-Host "          +$($_.ms)ms  $($_.id)  t=$($_.t)  mode=$($_.mode)"
        }
    } else {
        Pass "no sidecar was audible while seeking"
    }

    # --- final state ----------------------------------------------------------
    $final = Cdp @"
(() => {
  const v = document.getElementById('video');
  const set = (typeof activeReviewAudioSidecars !== 'undefined') ? activeReviewAudioSidecars : [];
  return JSON.stringify({
    mode: (typeof reviewAudioMode !== 'undefined') ? reviewAudioMode : null,
    videoMuted: v.muted, videoPaused: v.paused,
    active: set.map(s => s.audioTrackId),
    selected: (typeof currentReviewAudioTrackIds !== 'undefined') ? currentReviewAudioTrackIds : null,
    stuckMuted: set.filter(s => s.element.muted).map(s => s.audioTrackId),
  });
})()
"@ | ConvertFrom-Json

    if ($final.mode -eq 'sidecars' -and $final.stuckMuted.Count -gt 0) {
        Fail "sidecars audible mode but stranded muted: $($final.stuckMuted -join ', ')"
    } else { Pass "no stranded mute (mode=$($final.mode))" }

    # Only meaningful in `sidecars` mode: `direct` legitimately has no active set,
    # because the clip's own audio track is playing instead.
    $activeSorted = ($final.active | Sort-Object) -join ','
    $selectedSorted = ($final.selected | Sort-Object) -join ','
    if ($final.mode -eq 'sidecars') {
        if ($activeSorted -ne $selectedSorted) {
            Fail "active sidecars [$activeSorted] do not match selection [$selectedSorted]"
        } else { Pass "active set matches selection [$activeSorted]" }
    } else {
        Pass "mode=$($final.mode); active-set comparison not applicable"
    }
    # Whatever the mode, something must be audible: a selection that ends with
    # neither the video nor any sidecar audible is silence.
    $audible = ($final.mode -eq 'sidecars' -and $final.stuckMuted.Count -lt $final.active.Count) `
        -or ($final.mode -eq 'direct' -and -not $final.videoMuted)
    if ($final.mode -eq 'muted') { Pass "mode=muted: silence is the requested state" }
    elseif (-not $audible) { Fail "nothing is audible in mode=$($final.mode): selection produced silence" }
    else { Pass "audio is audible in mode=$($final.mode)" }
} catch {
    Fail "gate aborted: $_"
} finally {
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
}

Write-Host ""
if ($script:failures.Count -gt 0) {
    Write-Host "GATE FAILED ($($script:failures.Count) assertion(s))" -ForegroundColor Red
    exit 1
}
Write-Host "GATE PASSED" -ForegroundColor Green
exit 0
