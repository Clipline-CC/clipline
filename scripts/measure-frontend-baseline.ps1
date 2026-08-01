<#
.SYNOPSIS
    Captures matched, process-tree frontend baseline samples for Clipline.

.DESCRIPTION
    Launches Clipline in a disposable Windows profile, delegates UI driving to
    a marker-producing adapter, and writes frontend-neutral raw samples plus a
    metadata/summary JSON sidecar. The built-in Tauri adapter uses CDP. A Slint
    adapter can later use UI Automation or a benchmark control endpoint while
    emitting the same ready/error marker protocol.

    Tauri runs intentionally accept only target/benchmark/clipline-app.exe.
    That optimized Cargo profile keeps debug assertions enabled, which makes
    Clipline skip mutation of the user's global autostart registry entry.

.EXAMPLE
    cargo build -p clipline-app --profile benchmark
    pwsh -File scripts/measure-frontend-baseline.ps1 `
      -Exe target/benchmark/clipline-app.exe `
      -Frontend tauri -Renderer webview2 -Scenario library-500 `
      -FixturesDir fixtures/playback
#>
[CmdletBinding()]
param(
    [string]$Exe,
    [ValidateSet(
        'autostart-tray',
        'library-50',
        'library-500',
        'library-2000',
        'settings',
        'review-idle',
        'review-playing',
        'scrub-storm',
        'close-to-tray',
        'reveal-close-100'
    )]
    [string]$Scenario,
    [ValidateSet('tauri', 'slint')][string]$Frontend = 'tauri',
    [string]$Renderer,
    [string]$FixturesDir,
    [string]$FixturePath,
    [string]$AdapterScript,
    [ValidateRange(1, 86400)][int]$SteadySeconds = 300,
    [ValidateRange(0, 3600)][int]$WarmupSeconds = 30,
    [ValidateRange(100, 60000)][int]$SampleIntervalMs = 1000,
    [ValidateRange(5, 3600)][int]$ReadinessTimeoutSeconds = 600,
    [ValidateRange(1, 65535)][int]$DebugPort = 9222,
    [string]$OutputDirectory,
    [switch]$DriverWorker,
    [string]$DriverContextPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:HarnessVersion = '1.0.0'
$script:ScenarioNames = @(
    'autostart-tray', 'library-50', 'library-500', 'library-2000', 'settings',
    'review-idle', 'review-playing', 'scrub-storm', 'close-to-tray',
    'reveal-close-100'
)
$script:ScriptRoot = Split-Path -Parent $PSCommandPath
$script:RepoRoot = Split-Path -Parent $script:ScriptRoot
Import-Module (Join-Path $script:ScriptRoot 'lib/Clipline.ProcessMetrics.psm1') -Force

function ConvertTo-CliplineJsonLine {
    param([Parameter(Mandatory = $true)][object]$Value)
    return ($Value | ConvertTo-Json -Depth 12 -Compress)
}

function Write-CliplineDriverMarker {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][ValidateSet('ready', 'error')][string]$Kind,
        [Parameter(Mandatory = $true)][string]$Detail
    )
    $marker = [pscustomobject][ordered]@{
        schemaVersion = 1
        kind = $Kind
        timestampUtc = [datetime]::UtcNow.ToString('o')
        detail = $Detail
    }
    Add-Content -LiteralPath $Path -Value (ConvertTo-CliplineJsonLine $marker) -Encoding UTF8
}

function Test-CliplineRootIdentity {
    param([Parameter(Mandatory = $true)][object]$Context)

    $row = Get-CimInstance Win32_Process -Filter "ProcessId=$($Context.rootProcessId)"
    if (-not $row) { return $false }
    if ($row.Name -ne $Context.rootProcessName) { return $false }
    $expected = [datetime]$Context.rootStartUtc
    $actual = ([datetime]$row.CreationDate).ToUniversalTime()
    return [math]::Abs(($actual - $expected).TotalSeconds) -le 2.0
}

function Invoke-CliplineCdp {
    param(
        [Parameter(Mandatory = $true)][string]$WebSocketUrl,
        [Parameter(Mandatory = $true)][string]$Expression
    )

    $socket = New-Object System.Net.WebSockets.ClientWebSocket
    $token = [System.Threading.CancellationToken]::None
    try {
        $connect = $socket.ConnectAsync([uri]$WebSocketUrl, $token)
        if (-not $connect.Wait(5000)) { throw 'CDP websocket connect timed out' }
        $payload = @{
            id = 1
            method = 'Runtime.evaluate'
            params = @{
                expression = $Expression
                awaitPromise = $true
                returnByValue = $true
            }
        } | ConvertTo-Json -Depth 8 -Compress
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($payload)
        $segment = New-Object System.ArraySegment[byte] -ArgumentList @(, $bytes)
        $send = $socket.SendAsync(
            $segment,
            [System.Net.WebSockets.WebSocketMessageType]::Text,
            $true,
            $token
        )
        if (-not $send.Wait(5000)) { throw 'CDP websocket send timed out' }

        $message = New-Object System.Collections.Generic.List[byte]
        do {
            $buffer = New-Object byte[] 65536
            $receiveSegment = New-Object System.ArraySegment[byte] -ArgumentList @(, $buffer)
            $receive = $socket.ReceiveAsync($receiveSegment, $token)
            if (-not $receive.Wait(10000)) { throw 'CDP websocket receive timed out' }
            $result = $receive.Result
            if ($result.MessageType -eq [System.Net.WebSockets.WebSocketMessageType]::Close) {
                throw 'CDP websocket closed before returning an evaluation result'
            }
            for ($index = 0; $index -lt $result.Count; $index++) {
                $message.Add($buffer[$index])
            }
        } until ($result.EndOfMessage)

        $json = [System.Text.Encoding]::UTF8.GetString($message.ToArray()) | ConvertFrom-Json
        if ($json.error) { throw "CDP protocol error: $($json.error.message)" }
        if ($json.result.exceptionDetails) {
            throw "CDP evaluation failed: $($json.result.exceptionDetails.text)"
        }
        return $json.result.result.value
    } finally {
        $socket.Dispose()
    }
}

function Resolve-CliplineCdpPage {
    param(
        [Parameter(Mandatory = $true)][int]$Port,
        [Parameter(Mandatory = $true)][datetime]$DeadlineUtc
    )

    do {
        try {
            $targets = Invoke-RestMethod "http://127.0.0.1:$Port/json/list" -TimeoutSec 2
            $page = $targets | Where-Object {
                $_.type -eq 'page' -and ($_.url -match 'clipline' -or $_.title -match 'Clipline')
            } | Select-Object -First 1
            if ($page -and $page.webSocketDebuggerUrl) { return [string]$page.webSocketDebuggerUrl }
        } catch {
            # CDP appears only after the Tauri webview is constructed.
        }
        Start-Sleep -Milliseconds 100
    } while ([datetime]::UtcNow -lt $DeadlineUtc)
    throw "no semantic Clipline CDP page appeared on port $Port"
}

function Wait-CliplineCdpCondition {
    param(
        [Parameter(Mandatory = $true)][int]$Port,
        [Parameter(Mandatory = $true)][string]$Expression,
        [Parameter(Mandatory = $true)][datetime]$DeadlineUtc
    )

    do {
        $webSocketUrl = Resolve-CliplineCdpPage -Port $Port -DeadlineUtc $DeadlineUtc
        try {
            if ([bool](Invoke-CliplineCdp -WebSocketUrl $webSocketUrl -Expression $Expression)) {
                return $webSocketUrl
            }
        } catch {
            # A close/reveal cycle intentionally destroys and rebuilds WebView2.
        }
        Start-Sleep -Milliseconds 100
    } while ([datetime]::UtcNow -lt $DeadlineUtc)
    throw "semantic frontend condition timed out: $Expression"
}

function Wait-CliplineWindowHidden {
    param(
        [Parameter(Mandatory = $true)][object]$Context,
        [Parameter(Mandatory = $true)][datetime]$DeadlineUtc
    )
    do {
        if (-not (Test-CliplineRootIdentity -Context $Context)) {
            throw 'Clipline root process exited or changed identity while waiting for tray state'
        }
        $process = Get-Process -Id ([int]$Context.rootProcessId) -ErrorAction Stop
        if ($process.MainWindowHandle -eq 0) { return }
        Start-Sleep -Milliseconds 100
    } while ([datetime]::UtcNow -lt $DeadlineUtc)
    throw 'Clipline did not reach the tray-hidden state'
}

function Wait-CliplineDiagnosticMessage {
    param(
        [Parameter(Mandatory = $true)][string]$LogPath,
        [Parameter(Mandatory = $true)][string]$Pattern,
        [Parameter(Mandatory = $true)][datetime]$DeadlineUtc
    )
    do {
        if (Test-Path -LiteralPath $LogPath -PathType Leaf) {
            if (Select-String -LiteralPath $LogPath -Pattern $Pattern -Quiet) { return }
        }
        Start-Sleep -Milliseconds 100
    } while ([datetime]::UtcNow -lt $DeadlineUtc)
    throw "diagnostic readiness marker did not appear: $Pattern"
}

function Invoke-TauriDriverWorker {
    param([Parameter(Mandatory = $true)][string]$ContextPath)

    $context = Get-Content -LiteralPath $ContextPath -Raw | ConvertFrom-Json
    $deadline = [datetime]::UtcNow.AddSeconds([int]$context.readinessTimeoutSeconds)
    $baseReady = @'
(() => typeof foregroundBootCompleted !== 'undefined'
  && foregroundBootCompleted
  && typeof windowLifecycleState !== 'undefined'
  && windowLifecycleState.known
  && !windowLifecycleState.backgrounded)()
'@

    try {
        if (-not (Test-CliplineRootIdentity -Context $context)) {
            throw 'Clipline root identity was missing before driver startup'
        }

        if ($context.scenario -eq 'autostart-tray') {
            Wait-CliplineDiagnosticMessage -LogPath $context.diagnosticLogPath `
                -Pattern 'tray build complete' -DeadlineUtc $deadline
            Wait-CliplineDiagnosticMessage -LogPath $context.diagnosticLogPath `
                -Pattern 'autostart launch hiding webview' -DeadlineUtc $deadline
            Wait-CliplineWindowHidden -Context $context -DeadlineUtc $deadline
            Write-CliplineDriverMarker -Path $context.markerPath -Kind ready `
                -Detail 'tray built; autostart hide completed; native window absent'
            return
        }

        $webSocketUrl = Wait-CliplineCdpCondition -Port ([int]$context.debugPort) `
            -Expression $baseReady -DeadlineUtc $deadline

        if ($context.scenario -like 'library-*') {
            $expected = [int]($context.scenario -replace '^library-', '')
            $visibleCards = [math]::Min($expected, 60)
            $libraryReady = @"
(() => $baseReady
  && Array.isArray(clipsCache)
  && clipsCache.length === $expected
  && !document.getElementById('gallery-view').hidden
  && document.querySelectorAll('#gallery-grid .card[data-clip-path]').length >= $visibleCards
  && typeof posterWorkActive !== 'undefined'
  && posterWorkActive === 0
  && Array.isArray(posterWorkQueue)
  && posterWorkQueue.length === 0)()
"@
            Wait-CliplineCdpCondition -Port ([int]$context.debugPort) `
                -Expression $libraryReady -DeadlineUtc $deadline | Out-Null
            Write-CliplineDriverMarker -Path $context.markerPath -Kind ready `
                -Detail "library rendered with exactly $expected indexed clips"
            return
        }

        if ($context.scenario -eq 'settings') {
            Invoke-CliplineCdp -WebSocketUrl $webSocketUrl `
                -Expression "document.getElementById('rail-settings').click(); true" | Out-Null
            $settingsReady = @"
(() => $baseReady
  && typeof settingsOpen !== 'undefined'
  && settingsOpen
  && currentSettings !== null
  && !document.getElementById('settings-page').hidden)()
"@
            Wait-CliplineCdpCondition -Port ([int]$context.debugPort) `
                -Expression $settingsReady -DeadlineUtc $deadline | Out-Null
            Write-CliplineDriverMarker -Path $context.markerPath -Kind ready `
                -Detail 'settings model loaded and settings overlay visible'
            return
        }

        $libraryHasClip = @"
(() => $baseReady
  && Array.isArray(clipsCache)
  && clipsCache.length === 1
  && document.querySelector('#gallery-grid .card[data-clip-path]') !== null)()
"@
        if ($context.scenario -in @('review-idle', 'review-playing', 'scrub-storm')) {
            $webSocketUrl = Wait-CliplineCdpCondition -Port ([int]$context.debugPort) `
                -Expression $libraryHasClip -DeadlineUtc $deadline
            Invoke-CliplineCdp -WebSocketUrl $webSocketUrl -Expression @'
(() => { document.querySelector('#gallery-grid .card[data-clip-path]').click(); return true; })()
'@ | Out-Null
            $reviewLoaded = @"
(() => $baseReady
  && currentClip !== null
  && !document.getElementById('review-viewer').hidden
  && video.readyState >= 3
  && Number.isFinite(video.duration)
  && video.duration > 0
  && typeof audioPreviewQueue !== 'undefined'
  && audioPreviewQueue.active === null
  && audioPreviewQueue.desired === null)()
"@
            $webSocketUrl = Wait-CliplineCdpCondition -Port ([int]$context.debugPort) `
                -Expression $reviewLoaded -DeadlineUtc $deadline
        }

        if ($context.scenario -eq 'review-idle') {
            Invoke-CliplineCdp -WebSocketUrl $webSocketUrl `
                -Expression "video.pause(); video.paused && video.readyState >= 3" | Out-Null
            Wait-CliplineCdpCondition -Port ([int]$context.debugPort) `
                -Expression "video.paused && video.readyState >= 3" -DeadlineUtc $deadline | Out-Null
            Write-CliplineDriverMarker -Path $context.markerPath -Kind ready `
                -Detail 'review media decoded to current data and transport paused'
            return
        }

        if ($context.scenario -eq 'review-playing') {
            Invoke-CliplineCdp -WebSocketUrl $webSocketUrl `
                -Expression "video.play().then(() => true).catch(() => false)" | Out-Null
            Wait-CliplineCdpCondition -Port ([int]$context.debugPort) `
                -Expression "!video.paused && video.currentTime > 0.05 && video.readyState >= 3" `
                -DeadlineUtc $deadline | Out-Null
            Write-CliplineDriverMarker -Path $context.markerPath -Kind ready `
                -Detail 'review transport playing with decoded current data'
            return
        }

        if ($context.scenario -eq 'scrub-storm') {
            Invoke-CliplineCdp -WebSocketUrl $webSocketUrl -Expression 'video.pause(); true' | Out-Null
            $successfulSeeks = 0
            $seekIndex = 0
            while ($successfulSeeks -lt 10 -and [datetime]::UtcNow -lt $deadline) {
                $fraction = (($seekIndex * 37) % 97) / 100.0
                $expression = "(() => { if (!(video.duration > 0)) return false; video.currentTime = video.duration * $fraction; return true; })()"
                if ([bool](Invoke-CliplineCdp -WebSocketUrl $webSocketUrl -Expression $expression)) {
                    $successfulSeeks++
                }
                $seekIndex++
                Start-Sleep -Milliseconds 100
            }
            if ($successfulSeeks -lt 10) { throw 'scrub storm could not complete ten semantic seeks' }
            Write-CliplineDriverMarker -Path $context.markerPath -Kind ready `
                -Detail 'review loaded and ten timeline seeks were accepted'
            while (-not (Test-Path -LiteralPath $context.stopPath)) {
                $fraction = (($seekIndex * 37) % 97) / 100.0
                $expression = "(() => { if (!(video.duration > 0)) return false; video.currentTime = video.duration * $fraction; return true; })()"
                Invoke-CliplineCdp -WebSocketUrl $webSocketUrl -Expression $expression | Out-Null
                $seekIndex++
                Start-Sleep -Milliseconds 100
            }
            return
        }

        if ($context.scenario -eq 'close-to-tray') {
            Invoke-CliplineCdp -WebSocketUrl $webSocketUrl `
                -Expression "window.__TAURI__.window.getCurrentWindow().close().then(() => true)" | Out-Null
            Wait-CliplineWindowHidden -Context $context -DeadlineUtc $deadline
            Wait-CliplineDiagnosticMessage -LogPath $context.diagnosticLogPath `
                -Pattern 'close request action: tray' -DeadlineUtc $deadline
            Write-CliplineDriverMarker -Path $context.markerPath -Kind ready `
                -Detail 'native close request completed through close-to-tray policy'
            return
        }

        if ($context.scenario -eq 'reveal-close-100') {
            for ($cycle = 1; $cycle -le 100; $cycle++) {
                Invoke-CliplineCdp -WebSocketUrl $webSocketUrl `
                    -Expression "window.__TAURI__.window.getCurrentWindow().close().then(() => true)" | Out-Null
                Wait-CliplineWindowHidden -Context $context -DeadlineUtc $deadline

                $oldAppData = $env:APPDATA
                $oldLocalAppData = $env:LOCALAPPDATA
                $oldUserProfile = $env:USERPROFILE
                $oldTemp = $env:TEMP
                $oldTmp = $env:TMP
                $oldBrowserArguments = $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS
                $oldWebViewUserData = $env:WEBVIEW2_USER_DATA_FOLDER
                try {
                    $env:APPDATA = $context.appData
                    $env:LOCALAPPDATA = $context.localAppData
                    $env:USERPROFILE = $context.userProfile
                    $env:TEMP = $context.tempPath
                    $env:TMP = $context.tempPath
                    $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$($context.debugPort)"
                    $env:WEBVIEW2_USER_DATA_FOLDER = $context.webViewUserData
                    $secondary = Start-Process -FilePath $context.exe -WorkingDirectory $context.exeDirectory -PassThru
                    if (-not $secondary.WaitForExit(15000)) {
                        throw "secondary reveal launch $cycle did not hand off to the primary instance"
                    }
                    if ($secondary.ExitCode -ne 0) {
                        throw "secondary reveal launch $cycle exited $($secondary.ExitCode)"
                    }
                } finally {
                    $env:APPDATA = $oldAppData
                    $env:LOCALAPPDATA = $oldLocalAppData
                    $env:USERPROFILE = $oldUserProfile
                    $env:TEMP = $oldTemp
                    $env:TMP = $oldTmp
                    $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = $oldBrowserArguments
                    $env:WEBVIEW2_USER_DATA_FOLDER = $oldWebViewUserData
                }
                $webSocketUrl = Wait-CliplineCdpCondition -Port ([int]$context.debugPort) `
                    -Expression $baseReady -DeadlineUtc $deadline
            }
            Invoke-CliplineCdp -WebSocketUrl $webSocketUrl `
                -Expression "window.__TAURI__.window.getCurrentWindow().close().then(() => true)" | Out-Null
            Wait-CliplineWindowHidden -Context $context -DeadlineUtc $deadline
            Write-CliplineDriverMarker -Path $context.markerPath -Kind ready `
                -Detail '100 reveal/close cycles completed and final close reached tray state'
            return
        }

        throw "Tauri driver does not implement scenario $($context.scenario)"
    } catch {
        Write-CliplineDriverMarker -Path $context.markerPath -Kind error -Detail $_.Exception.Message
        throw
    }
}

function Get-CliplineDriverMarker {
    param([Parameter(Mandatory = $true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $null }
    $lines = @(Get-Content -LiteralPath $Path -ErrorAction Stop)
    $latest = $null
    foreach ($line in $lines) {
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        try {
            $marker = $line | ConvertFrom-Json
            if ($marker.schemaVersion -ne 1) { throw 'unsupported marker schema version' }
            if ($marker.kind -in @('ready', 'error')) { $latest = $marker }
        } catch {
            # A concurrent writer may not have flushed the final line yet.
        }
    }
    return $latest
}

function Assert-CliplineDriverHealthy {
    param(
        [Parameter(Mandatory = $true)][string]$MarkerPath,
        [Parameter(Mandatory = $true)][System.Diagnostics.Process]$DriverProcess
    )
    $marker = Get-CliplineDriverMarker -Path $MarkerPath
    if ($marker -and $marker.kind -eq 'error') {
        throw "frontend driver failed after readiness: $($marker.detail)"
    }
    if ($DriverProcess.HasExited -and $DriverProcess.ExitCode -ne 0) {
        throw "frontend driver exited $($DriverProcess.ExitCode) after readiness"
    }
}

function Assert-CliplineBenchmarkExecutable {
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][string]$FrontendName
    )
    if (-not (Test-Path -LiteralPath $Executable -PathType Leaf)) {
        throw "executable does not exist: $Executable"
    }
    if ($FrontendName -ne 'tauri') { return $null }

    $profileDirectory = Split-Path -Leaf (Split-Path -Parent $Executable)
    if ($profileDirectory -ne 'benchmark') {
        throw 'Tauri baselines require target/benchmark/clipline-app.exe; release builds can mutate the global autostart registry'
    }
    $cargo = Get-Content -LiteralPath (Join-Path $script:RepoRoot 'Cargo.toml') -Raw
    if ($cargo -notmatch '(?ms)^\[profile\.benchmark\].*?^inherits\s*=\s*"release".*?^debug-assertions\s*=\s*true\s*$') {
        throw 'Cargo.toml benchmark profile must inherit release and enable debug assertions'
    }
    $probeText = (& $Executable '--clipline-benchmark-probe' 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($probeText)) {
        throw 'benchmark executable did not return its safety probe'
    }
    try { $probe = $probeText | ConvertFrom-Json } catch {
        throw "benchmark executable returned an invalid safety probe: $probeText"
    }
    if ($probe.schema -ne 1 -or
        -not [bool]$probe.benchmark_shell_safe -or
        -not [bool]$probe.debug_assertions -or
        [bool]$probe.autostart_registry_mutation -or
        [string]$probe.opt_level -in @('', '0', 'unknown')) {
        throw "benchmark executable is not optimized and shell-safe: $probeText"
    }
    return $probe
}

function Resolve-CliplineFixture {
    param(
        [Parameter(Mandatory = $true)][string]$ScenarioName,
        [string]$FixtureDirectory,
        [string]$RequestedPath
    )

    $requiresFixture = $ScenarioName -like 'library-*' -or
        $ScenarioName -in @('review-idle', 'review-playing', 'scrub-storm')
    if (-not $requiresFixture) { return $null }
    if ([string]::IsNullOrWhiteSpace($FixtureDirectory) -or
        -not (Test-Path -LiteralPath $FixtureDirectory -PathType Container)) {
        throw "scenario $ScenarioName requires an existing -FixturesDir"
    }
    $manifest = Join-Path $FixtureDirectory 'manifest.json'
    if (-not (Test-Path -LiteralPath $manifest -PathType Leaf)) {
        throw "fixture manifest is missing: $manifest"
    }
    if (-not [string]::IsNullOrWhiteSpace($RequestedPath)) {
        $candidate = $RequestedPath
        if (-not [System.IO.Path]::IsPathRooted($candidate)) {
            $candidate = Join-Path $FixtureDirectory $candidate
        }
    } elseif ($ScenarioName -like 'library-*') {
        $candidate = Join-Path $FixtureDirectory 'h264-one-opus-3s.mp4'
    } else {
        $candidate = Join-Path $FixtureDirectory 'h264-two-opus-markers-5s.mp4'
    }
    if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
        throw "requested fixture is missing: $candidate"
    }
    $resolvedCandidate = (Resolve-Path -LiteralPath $candidate).Path
    $manifestValue = Get-Content -LiteralPath $manifest -Raw | ConvertFrom-Json
    if ($manifestValue.schema_version -ne 1 -or
        $manifestValue.suite -ne 'clipline-native-playback-v1') {
        throw 'fixture manifest has an unsupported schema or suite'
    }
    $entry = @($manifestValue.fixtures | Where-Object { $_.file -eq [System.IO.Path]::GetFileName($resolvedCandidate) }) |
        Select-Object -First 1
    if (-not $entry -or -not $entry.artifact -or
        [string]::IsNullOrWhiteSpace([string]$entry.artifact.sha256)) {
        throw "requested fixture is not hash-covered by manifest.json: $resolvedCandidate"
    }
    $actualHash = (Get-FileHash -LiteralPath $resolvedCandidate -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -ne ([string]$entry.artifact.sha256).ToLowerInvariant()) {
        throw "requested fixture hash does not match manifest.json: $resolvedCandidate"
    }
    return $resolvedCandidate
}

function Add-CliplineFixtureLink {
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Destination
    )
    try {
        New-Item -ItemType HardLink -Path $Destination -Target $Source -ErrorAction Stop | Out-Null
    } catch {
        throw "could not hard-link fixture into disposable profile (keep fixtures and TEMP on the same volume): $($_.Exception.Message)"
    }
}

function Assert-CliplineDebugPortAvailable {
    param([Parameter(Mandatory = $true)][int]$Port)
    $client = New-Object System.Net.Sockets.TcpClient
    try {
        $connect = $client.BeginConnect('127.0.0.1', $Port, $null, $null)
        if ($connect.AsyncWaitHandle.WaitOne(250) -and $client.Connected) {
            throw "debug port $Port is already in use"
        }
    } finally {
        $client.Dispose()
    }
}

function Initialize-CliplineFixtureProfile {
    param(
        [Parameter(Mandatory = $true)][string]$ScenarioName,
        [Parameter(Mandatory = $true)][string]$MediaPath,
        [string]$SourceFixture
    )
    if ([string]::IsNullOrWhiteSpace($SourceFixture)) { return }

    $count = 1
    if ($ScenarioName -like 'library-*') {
        $count = [int]($ScenarioName -replace '^library-', '')
    }
    for ($index = 1; $index -le $count; $index++) {
        $name = if ($count -eq 1) { '000-review.mp4' } else { 'clip-{0:D4}.mp4' -f $index }
        $destination = Join-Path $MediaPath $name
        Add-CliplineFixtureLink -Source $SourceFixture -Destination $destination
        if ($count -eq 1) {
            foreach ($extension in @('markers.json', 'clipline.json', 'osu-enrichment.json')) {
                $sourceSidecar = [System.IO.Path]::ChangeExtension($SourceFixture, $extension)
                if (Test-Path -LiteralPath $sourceSidecar -PathType Leaf) {
                    $destinationSidecar = [System.IO.Path]::ChangeExtension($destination, $extension)
                    # Sidecars are copied, not linked: enrichment may rewrite a
                    # sidecar, and a hard link would mutate the frozen fixture.
                    Copy-Item -LiteralPath $sourceSidecar -Destination $destinationSidecar
                }
            }
        }
    }
}

function Get-CliplineFixtureHashes {
    param([string]$FixtureDirectory)
    if ([string]::IsNullOrWhiteSpace($FixtureDirectory) -or
        -not (Test-Path -LiteralPath $FixtureDirectory -PathType Container)) {
        return @()
    }
    return @(
        Get-ChildItem -LiteralPath $FixtureDirectory -File -Recurse | Sort-Object FullName |
            ForEach-Object {
                $relative = $_.FullName.Substring((Resolve-Path $FixtureDirectory).Path.Length).TrimStart('\', '/')
                [pscustomobject][ordered]@{
                    path = $relative
                    bytes = [long]$_.Length
                    sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
                }
            }
    )
}

function Get-CliplineSystemDisplayScale {
    if (-not ('CliplineDpiNative' -as [type])) {
        Add-Type -TypeDefinition @"
using System.Runtime.InteropServices;
public static class CliplineDpiNative {
    [DllImport("user32.dll")]
    public static extern uint GetDpiForSystem();
}
"@
    }
    $dpi = [CliplineDpiNative]::GetDpiForSystem()
    if ($dpi -le 0) { throw 'GetDpiForSystem returned no display DPI' }
    return [pscustomobject][ordered]@{
        dpi = [int]$dpi
        percent = [math]::Round(($dpi / 96.0) * 100.0, 1)
    }
}

function Get-CliplineProcessSample {
    param(
        [Parameter(Mandatory = $true)][int]$RootProcessId,
        [Parameter(Mandatory = $true)][datetime]$RootStart,
        [Parameter(Mandatory = $true)][string]$RootProcessName,
        [Parameter(Mandatory = $true)][string]$RunId,
        [Parameter(Mandatory = $true)][string]$FrontendName,
        [Parameter(Mandatory = $true)][string]$RendererName,
        [Parameter(Mandatory = $true)][string]$ScenarioName,
        [Parameter(Mandatory = $true)][string]$Phase,
        [Parameter(Mandatory = $true)][long]$ElapsedMs,
        [Parameter(Mandatory = $true)][hashtable]$PreviousCpu
    )

    $allProcesses = @(Get-CimInstance Win32_Process)
    $rootRow = $allProcesses | Where-Object { [int]$_.ProcessId -eq $RootProcessId } | Select-Object -First 1
    if (-not $rootRow) { throw "root process $RootProcessId exited during sampling" }
    if ($rootRow.Name -ne $RootProcessName) {
        throw "root PID $RootProcessId changed identity from $RootProcessName to $($rootRow.Name)"
    }
    $observedRootStart = ([datetime]$rootRow.CreationDate).ToUniversalTime()
    if ([math]::Abs(($observedRootStart - $RootStart).TotalSeconds) -gt 2.0) {
        throw "root PID $RootProcessId was reused during sampling"
    }

    $descendants = @(Get-CliplineDescendantProcesses -RootProcessId $RootProcessId `
        -RootStart $RootStart.ToLocalTime() -ProcessRows $allProcesses)
    $treeRows = @($rootRow) + $descendants
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

    $treePrivateWorkingSet = [long](($metrics | ForEach-Object { $_.Snapshot.PrivateWorkingSetBytes } | Measure-Object -Sum).Sum)
    $treePrivateCommit = [long](($metrics | ForEach-Object { $_.Snapshot.PrivateCommitBytes } | Measure-Object -Sum).Sum)
    $treeWorkingSet = [long](($metrics | ForEach-Object { $_.Snapshot.WorkingSetBytes } | Measure-Object -Sum).Sum)
    $treeCpuPercent = [double](($metrics | Measure-Object -Property CpuPercent -Sum).Sum)
    $treeHandles = [long](($metrics | ForEach-Object { $_.Snapshot.HandleCount } | Measure-Object -Sum).Sum)
    $treeThreads = [long](($metrics | ForEach-Object { $_.Snapshot.ThreadCount } | Measure-Object -Sum).Sum)
    $processIds = @($metrics | ForEach-Object { [int]$_.Cim.ProcessId })
    $gpu = Get-CliplineGpuProcessMemory -ProcessIds $processIds
    $sampleUtc = [datetime]::UtcNow.ToString('o')
    $rawRows = New-Object System.Collections.Generic.List[object]
    foreach ($metric in $metrics) {
        $rawRows.Add([pscustomobject][ordered]@{
            RunId = $RunId
            Frontend = $FrontendName
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
        }
    }
}

function Get-CliplineMetricSummary {
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

if ($DriverWorker) {
    if ([string]::IsNullOrWhiteSpace($DriverContextPath) -or
        -not (Test-Path -LiteralPath $DriverContextPath -PathType Leaf)) {
        throw '-DriverWorker requires an existing -DriverContextPath'
    }
    Invoke-TauriDriverWorker -ContextPath $DriverContextPath
    exit 0
}

if ([string]::IsNullOrWhiteSpace($Exe)) { throw '-Exe is required' }
if ([string]::IsNullOrWhiteSpace($Scenario) -or $Scenario -notin $script:ScenarioNames) {
    throw '-Scenario must name one supported scenario'
}
if ($Frontend -eq 'slint' -and
    ([string]::IsNullOrWhiteSpace($AdapterScript) -or
     -not (Test-Path -LiteralPath $AdapterScript -PathType Leaf))) {
    throw 'Slint baselines require an existing -AdapterScript that implements the marker protocol'
}
if ([string]::IsNullOrWhiteSpace($Renderer)) {
    $Renderer = if ($Frontend -eq 'tauri') { 'webview2' } else { 'native' }
}

$resolvedExe = (Resolve-Path -LiteralPath $Exe).Path
$benchmarkSafetyProbe = Assert-CliplineBenchmarkExecutable -Executable $resolvedExe -FrontendName $Frontend
$resolvedFixtures = $null
if (-not [string]::IsNullOrWhiteSpace($FixturesDir)) {
    if (-not (Test-Path -LiteralPath $FixturesDir -PathType Container)) {
        throw "fixtures directory does not exist: $FixturesDir"
    }
    $resolvedFixtures = (Resolve-Path -LiteralPath $FixturesDir).Path
}
$resolvedFixture = Resolve-CliplineFixture -ScenarioName $Scenario `
    -FixtureDirectory $resolvedFixtures -RequestedPath $FixturePath

$processName = [System.IO.Path]::GetFileNameWithoutExtension($resolvedExe)
$existing = @(Get-Process -Name $processName -ErrorAction SilentlyContinue)
if ($existing.Count -gt 0) {
    throw "refusing to benchmark while $processName is already running (PIDs: $(($existing.Id) -join ', '))"
}
if ($Frontend -eq 'tauri') { Assert-CliplineDebugPortAvailable -Port $DebugPort }
# PROCESS_MEMORY_COUNTERS_EX2 is the metric contract, not an optional detail.
# Reading this harness process proves the host has the required Windows update
# before Clipline is launched or any evidence is written.
$metricPreflight = Get-CliplineNativeProcessSnapshot -ProcessId $PID
if ($metricPreflight.PrivateWorkingSetBytes -le 0 -or
    $metricPreflight.PrivateCommitBytes -le 0 -or
    $metricPreflight.WorkingSetBytes -le 0) {
    throw 'PROCESS_MEMORY_COUNTERS_EX2 preflight returned invalid values'
}

$runId = '{0}-{1}-{2}' -f ([datetime]::UtcNow.ToString('yyyyMMddTHHmmssZ')), $Frontend, $Scenario
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path ([System.IO.Path]::GetTempPath()) 'clipline-frontend-baselines'
}
$outputRoot = [System.IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Path $outputRoot -Force | Out-Null
$rawCsvPath = Join-Path $outputRoot "$runId.raw.csv"
$metadataPath = Join-Path $outputRoot "$runId.metadata.json"
if ((Test-Path -LiteralPath $rawCsvPath) -or (Test-Path -LiteralPath $metadataPath)) {
    throw "run output already exists for $runId"
}

$profileRoot = Join-Path $outputRoot "profiles/$runId"
if (Test-Path -LiteralPath $profileRoot) { throw "disposable profile already exists: $profileRoot" }
$appData = Join-Path $profileRoot 'AppData/Roaming'
$localAppData = Join-Path $profileRoot 'AppData/Local'
$tempPath = Join-Path $profileRoot 'Temp'
$mediaPath = Join-Path $profileRoot 'Videos/Clipline'
$webViewUserData = Join-Path $profileRoot 'WebView2UserData'
foreach ($directory in @($appData, $localAppData, $tempPath, $mediaPath, $webViewUserData)) {
    New-Item -ItemType Directory -Path $directory -Force | Out-Null
}
Initialize-CliplineFixtureProfile -ScenarioName $Scenario -MediaPath $mediaPath `
    -SourceFixture $resolvedFixture

$markerPath = Join-Path $profileRoot 'driver-markers.jsonl'
$stopPath = Join-Path $profileRoot 'driver.stop'
$contextPath = Join-Path $profileRoot 'driver-context.json'
$diagnosticLogPath = Join-Path $appData 'Clipline/logs/clipline.log'
$fixtureHashes = Get-CliplineFixtureHashes -FixtureDirectory $resolvedFixtures
$exeHash = (Get-FileHash -LiteralPath $resolvedExe -Algorithm SHA256).Hash.ToLowerInvariant()
$gitCommit = (& git -C $script:RepoRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($gitCommit)) {
    throw 'could not resolve the benchmark source git commit'
}
$operatingSystem = Get-CimInstance Win32_OperatingSystem
$processors = @(Get-CimInstance Win32_Processor | ForEach-Object {
    [pscustomobject][ordered]@{ name = $_.Name; cores = $_.NumberOfCores; logicalProcessors = $_.NumberOfLogicalProcessors }
})
if ($processors.Count -eq 0) { throw 'could not record CPU metadata' }
$videoControllers = @(Get-CimInstance Win32_VideoController -ErrorAction SilentlyContinue | ForEach-Object {
    [pscustomobject][ordered]@{ name = $_.Name; driverVersion = $_.DriverVersion; driverDate = $_.DriverDate }
})
$displayScale = Get-CliplineSystemDisplayScale

$oldAppData = $env:APPDATA
$oldLocalAppData = $env:LOCALAPPDATA
$oldUserProfile = $env:USERPROFILE
$oldTemp = $env:TEMP
$oldTmp = $env:TMP
$oldBrowserArguments = $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS
$oldWebViewUserData = $env:WEBVIEW2_USER_DATA_FOLDER
$rootProcess = $null
$driverProcess = $null
$rawRows = New-Object System.Collections.Generic.List[object]
$aggregateSamples = New-Object System.Collections.Generic.List[object]
$previousCpu = @{}
$startUtc = $null
$readyMarker = $null

try {
    $env:APPDATA = $appData
    $env:LOCALAPPDATA = $localAppData
    $env:USERPROFILE = $profileRoot
    $env:TEMP = $tempPath
    $env:TMP = $tempPath
    if ($Frontend -eq 'tauri') {
        $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$DebugPort"
        $env:WEBVIEW2_USER_DATA_FOLDER = $webViewUserData
    }
    $arguments = @()
    if ($Scenario -eq 'autostart-tray') { $arguments += '--autostart' }
    if ($arguments.Count -gt 0) {
        $rootProcess = Start-Process -FilePath $resolvedExe -ArgumentList $arguments `
            -WorkingDirectory (Split-Path -Parent $resolvedExe) -PassThru
    } else {
        $rootProcess = Start-Process -FilePath $resolvedExe `
            -WorkingDirectory (Split-Path -Parent $resolvedExe) -PassThru
    }
} finally {
    $env:APPDATA = $oldAppData
    $env:LOCALAPPDATA = $oldLocalAppData
    $env:USERPROFILE = $oldUserProfile
    $env:TEMP = $oldTemp
    $env:TMP = $oldTmp
    $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = $oldBrowserArguments
    $env:WEBVIEW2_USER_DATA_FOLDER = $oldWebViewUserData
}

try {
    $rootCim = $null
    $identityDeadline = [datetime]::UtcNow.AddSeconds(10)
    do {
        $rootCim = Get-CimInstance Win32_Process -Filter "ProcessId=$($rootProcess.Id)"
        if (-not $rootCim) { Start-Sleep -Milliseconds 50 }
    } while (-not $rootCim -and [datetime]::UtcNow -lt $identityDeadline)
    if (-not $rootCim -or $rootCim.Name -ne ([System.IO.Path]::GetFileName($resolvedExe))) {
        throw 'launched process identity could not be established'
    }
    $startUtc = ([datetime]$rootCim.CreationDate).ToUniversalTime()
    $context = [pscustomobject][ordered]@{
        schemaVersion = 1
        harnessVersion = $script:HarnessVersion
        scenario = $Scenario
        frontend = $Frontend
        renderer = $Renderer
        exe = $resolvedExe
        exeDirectory = Split-Path -Parent $resolvedExe
        rootProcessId = [int]$rootProcess.Id
        rootProcessName = [string]$rootCim.Name
        rootStartUtc = $startUtc.ToString('o')
        readinessTimeoutSeconds = $ReadinessTimeoutSeconds
        debugPort = $DebugPort
        profileRoot = $profileRoot
        appData = $appData
        localAppData = $localAppData
        userProfile = $profileRoot
        tempPath = $tempPath
        webViewUserData = $webViewUserData
        mediaPath = $mediaPath
        fixturePath = $resolvedFixture
        markerPath = $markerPath
        stopPath = $stopPath
        diagnosticLogPath = $diagnosticLogPath
    }
    $context | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $contextPath -Encoding UTF8

    $driverHost = (Get-Process -Id $PID).Path
    if ($Frontend -eq 'tauri') {
        $driverArguments = @(
            '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"",
            '-DriverWorker', '-DriverContextPath', "`"$contextPath`""
        )
        $driverProcess = Start-Process -FilePath $driverHost -ArgumentList $driverArguments `
            -WorkingDirectory $script:RepoRoot -WindowStyle Hidden -PassThru
    } else {
        $resolvedAdapter = (Resolve-Path -LiteralPath $AdapterScript).Path
        $driverArguments = @(
            '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$resolvedAdapter`"",
            '-ContextPath', "`"$contextPath`""
        )
        $driverProcess = Start-Process -FilePath $driverHost -ArgumentList $driverArguments `
            -WorkingDirectory $script:RepoRoot -WindowStyle Hidden -PassThru
    }

    $clock = [System.Diagnostics.Stopwatch]::StartNew()
    $readyDeadline = [datetime]::UtcNow.AddSeconds($ReadinessTimeoutSeconds)
    while (-not $readyMarker) {
        $sample = Get-CliplineProcessSample -RootProcessId $rootProcess.Id -RootStart $startUtc `
            -RootProcessName $rootCim.Name -RunId $runId -FrontendName $Frontend `
            -RendererName $Renderer -ScenarioName $Scenario -Phase 'startup' `
            -ElapsedMs $clock.ElapsedMilliseconds -PreviousCpu $previousCpu
        foreach ($row in $sample.Rows) { $rawRows.Add($row) }
        $aggregateSamples.Add($sample.Aggregate)

        $marker = Get-CliplineDriverMarker -Path $markerPath
        if ($marker -and $marker.kind -eq 'error') { throw "frontend driver failed: $($marker.detail)" }
        if ($marker -and $marker.kind -eq 'ready') {
            $markerUtc = ([datetime]$marker.timestampUtc).ToUniversalTime()
            if ($markerUtc -lt $startUtc -or $markerUtc -gt [datetime]::UtcNow.AddSeconds(5)) {
                throw 'frontend readiness marker timestamp is outside this process lifetime'
            }
            $readyMarker = $marker
            break
        }
        if ($driverProcess.HasExited) {
            throw "frontend driver exited $($driverProcess.ExitCode) without the required readiness marker"
        }
        if ([datetime]::UtcNow -ge $readyDeadline) {
            throw "required frontend readiness marker was missing after $ReadinessTimeoutSeconds seconds"
        }
        Start-Sleep -Milliseconds $SampleIntervalMs
    }

    $warmupEnd = [datetime]::UtcNow.AddSeconds($WarmupSeconds)
    while ([datetime]::UtcNow -lt $warmupEnd) {
        $sample = Get-CliplineProcessSample -RootProcessId $rootProcess.Id -RootStart $startUtc `
            -RootProcessName $rootCim.Name -RunId $runId -FrontendName $Frontend `
            -RendererName $Renderer -ScenarioName $Scenario -Phase 'warmup' `
            -ElapsedMs $clock.ElapsedMilliseconds -PreviousCpu $previousCpu
        foreach ($row in $sample.Rows) { $rawRows.Add($row) }
        $aggregateSamples.Add($sample.Aggregate)
        Assert-CliplineDriverHealthy -MarkerPath $markerPath -DriverProcess $driverProcess
        Start-Sleep -Milliseconds $SampleIntervalMs
    }

    $steadyStartUtc = [datetime]::UtcNow
    $steadyEnd = $steadyStartUtc.AddSeconds($SteadySeconds)
    while ([datetime]::UtcNow -lt $steadyEnd) {
        $sample = Get-CliplineProcessSample -RootProcessId $rootProcess.Id -RootStart $startUtc `
            -RootProcessName $rootCim.Name -RunId $runId -FrontendName $Frontend `
            -RendererName $Renderer -ScenarioName $Scenario -Phase 'steady' `
            -ElapsedMs $clock.ElapsedMilliseconds -PreviousCpu $previousCpu
        foreach ($row in $sample.Rows) { $rawRows.Add($row) }
        $aggregateSamples.Add($sample.Aggregate)
        Assert-CliplineDriverHealthy -MarkerPath $markerPath -DriverProcess $driverProcess
        Start-Sleep -Milliseconds $SampleIntervalMs
    }
    $endUtc = [datetime]::UtcNow
    New-Item -ItemType File -Path $stopPath -Force | Out-Null
    Assert-CliplineDriverHealthy -MarkerPath $markerPath -DriverProcess $driverProcess

    if ($Frontend -eq 'tauri' -and
        @(Get-ChildItem -LiteralPath $webViewUserData -Force -ErrorAction SilentlyContinue).Count -eq 0) {
        throw 'WebView2 did not materialize its user-data folder inside the disposable profile'
    }

    $steady = @($aggregateSamples.ToArray() | Where-Object Phase -eq 'steady')
    if ($steady.Count -eq 0) { throw 'no steady samples were captured' }
    $summary = [pscustomobject][ordered]@{
        steadySampleCount = $steady.Count
        treePrivateWorkingSetBytes = Get-CliplineMetricSummary -Samples $steady -Property TreePrivateWorkingSetBytes
        treePrivateCommitBytes = Get-CliplineMetricSummary -Samples $steady -Property TreePrivateCommitBytes
        treeWorkingSetBytes = Get-CliplineMetricSummary -Samples $steady -Property TreeWorkingSetBytes
        treeCpuPercent = Get-CliplineMetricSummary -Samples $steady -Property TreeCpuPercent
        treeHandleCount = Get-CliplineMetricSummary -Samples $steady -Property TreeHandleCount
        treeThreadCount = Get-CliplineMetricSummary -Samples $steady -Property TreeThreadCount
        treeProcessCount = Get-CliplineMetricSummary -Samples $steady -Property TreeProcessCount
        gpuLocalBytes = Get-CliplineMetricSummary -Samples $steady -Property GpuLocalBytes -Optional
        gpuNonLocalBytes = Get-CliplineMetricSummary -Samples $steady -Property GpuNonLocalBytes -Optional
        childReadFailuresTotal = [long](($steady | Measure-Object -Property ChildReadFailures -Sum).Sum)
    }

    $columns = Get-CliplineSampleColumns
    $rawRows.ToArray() | Select-Object -Property $columns | Export-Csv -LiteralPath $rawCsvPath -NoTypeInformation
    $metadata = [pscustomobject][ordered]@{
        schemaVersion = 1
        harnessVersion = $script:HarnessVersion
        runId = $runId
        gitCommit = $gitCommit
        frontend = $Frontend
        renderer = $Renderer
        scenario = $Scenario
        executable = [pscustomobject][ordered]@{
            path = $resolvedExe
            sha256 = $exeHash
            bytes = [long](Get-Item -LiteralPath $resolvedExe).Length
            benchmarkSafetyProbe = $benchmarkSafetyProbe
        }
        fixtures = [pscustomobject][ordered]@{
            directory = $resolvedFixtures
            selected = $resolvedFixture
            files = $fixtureHashes
        }
        machine = [pscustomobject][ordered]@{
            computerName = $env:COMPUTERNAME
            operatingSystem = [pscustomobject][ordered]@{
                caption = $operatingSystem.Caption
                version = $operatingSystem.Version
                buildNumber = $operatingSystem.BuildNumber
            }
            processors = $processors
            videoControllers = $videoControllers
            displayScale = $displayScale
        }
        timing = [pscustomobject][ordered]@{
            processCreatedUtc = $startUtc.ToString('o')
            frontendReadyUtc = ([datetime]$readyMarker.timestampUtc).ToUniversalTime().ToString('o')
            firstUsableMs = [math]::Round((([datetime]$readyMarker.timestampUtc).ToUniversalTime() - $startUtc).TotalMilliseconds, 3)
            steadyStartedUtc = $steadyStartUtc.ToString('o')
            endedUtc = $endUtc.ToString('o')
            warmupSeconds = $WarmupSeconds
            requestedSteadySeconds = $SteadySeconds
            sampleIntervalMs = $SampleIntervalMs
        }
        readiness = [pscustomobject][ordered]@{
            protocol = 'clipline-baseline-marker-v1'
            detail = $readyMarker.detail
        }
        profile = [pscustomobject][ordered]@{
            disposable = $true
            root = $profileRoot
            appData = $appData
            localAppData = $localAppData
            userProfile = $profileRoot
            mediaPath = $mediaPath
            webViewUserData = $webViewUserData
            coldWebViewUserData = ($Frontend -eq 'tauri')
        }
        rawSamples = [pscustomobject][ordered]@{
            path = $rawCsvPath
            rows = $rawRows.Count
            columns = $columns
        }
        summary = $summary
    }
    $metadata | ConvertTo-Json -Depth 16 | Set-Content -LiteralPath $metadataPath -Encoding UTF8

    Write-Host "raw samples: $rawCsvPath"
    Write-Host "metadata:    $metadataPath"
    Write-Host "profile:     $profileRoot"
    Write-Host ("steady tree private working set p50/p95: {0:N1}/{1:N1} MiB" -f `
        ($summary.treePrivateWorkingSetBytes.p50 / 1MB),
        ($summary.treePrivateWorkingSetBytes.p95 / 1MB))
} finally {
    if (-not (Test-Path -LiteralPath $stopPath)) {
        New-Item -ItemType File -Path $stopPath -Force -ErrorAction SilentlyContinue | Out-Null
    }
    if ($driverProcess -and -not $driverProcess.HasExited) {
        if (-not $driverProcess.WaitForExit(5000)) {
            Stop-Process -Id $driverProcess.Id -Force -ErrorAction SilentlyContinue
        }
    }
    if ($rootProcess -and -not $rootProcess.HasExited) {
        Stop-Process -Id $rootProcess.Id -Force -ErrorAction SilentlyContinue
    }
}
