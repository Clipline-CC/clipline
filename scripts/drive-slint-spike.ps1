[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$ContextPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:SlintMarkerKinds = @(
    'trayReady', 'windowCreated', 'windowDropped', 'saveReplay', 'ready', 'error'
)

function ConvertTo-SlintJsonLine {
    param([Parameter(Mandatory = $true)][object]$Value)
    return ($Value | ConvertTo-Json -Depth 8 -Compress)
}

function Write-DriverMarker {
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
    Add-Content -LiteralPath $Path -Value (ConvertTo-SlintJsonLine $marker) -Encoding UTF8
}

function Get-SlintMarkerState {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [switch]$IncludeIncompleteFinalLine
    )
    $markers = @()
    if (Test-Path -LiteralPath $Path -PathType Leaf) {
        [string]$raw = Get-Content -LiteralPath $Path -Raw -ErrorAction Stop
        $lines = @($raw -split "`r?`n")
        $lineCount = $lines.Count
        if (-not $IncludeIncompleteFinalLine -and
            $raw.Length -gt 0 -and
            $raw -notmatch "(`r`n|`n)$") {
            # The app writes from another thread. Ignore only its unfinished
            # final line while the process is still alive; every completed
            # line remains fail-closed.
            $lineCount--
        }
        for ($index = 0; $index -lt $lineCount; $index++) {
            $line = $lines[$index]
            if ([string]::IsNullOrWhiteSpace($line)) { continue }
            try { $marker = $line | ConvertFrom-Json -ErrorAction Stop } catch {
                throw "malformed completed Slint marker line $($index + 1): $($_.Exception.Message)"
            }
            if ($marker.schemaVersion -ne 1) {
                throw "unsupported Slint marker schema version on line $($index + 1)"
            }
            if ([string]$marker.kind -notin $script:SlintMarkerKinds) {
                throw "unsupported Slint marker kind '$($marker.kind)' on line $($index + 1)"
            }
            $markers += ,$marker
        }
    }
    $errors = @($markers | Where-Object kind -eq 'error')
    if ($errors.Count -gt 0) {
        throw "Slint spike failed: $($errors[0].detail)"
    }
    return [pscustomobject]@{
        Markers = $markers
        TrayReady = @($markers | Where-Object kind -eq 'trayReady')
        WindowCreated = @($markers | Where-Object kind -eq 'windowCreated')
        WindowDropped = @($markers | Where-Object kind -eq 'windowDropped')
        Ready = @($markers | Where-Object kind -eq 'ready')
    }
}

function Test-RootIdentity {
    param([Parameter(Mandatory = $true)][object]$Context)
    $row = Get-CimInstance Win32_Process -Filter "ProcessId=$($Context.rootProcessId)"
    if (-not $row -or $row.Name -ne $Context.rootProcessName) { return $false }
    $expected = ([datetime]$Context.rootStartUtc).ToUniversalTime()
    $actual = ([datetime]$row.CreationDate).ToUniversalTime()
    return [math]::Abs(($actual - $expected).TotalSeconds) -le 2.0
}

function Request-SlintWindowClose {
    param([Parameter(Mandatory = $true)][object]$Context)
    $process = Get-Process -Id ([int]$Context.rootProcessId) -ErrorAction Stop
    $process.Refresh()
    if ($process.MainWindowHandle -eq [IntPtr]::Zero) { return $false }
    return $process.CloseMainWindow()
}

$context = Get-Content -LiteralPath $ContextPath -Raw | ConvertFrom-Json
if ($context.schemaVersion -ne 1 -or $context.frontend -ne 'slint') {
    throw 'driver context is not a Slint baseline context'
}
if ([string]::IsNullOrWhiteSpace([string]$context.frontendMarkerPath)) {
    throw 'Slint driver context is missing frontendMarkerPath'
}

$readyPublished = $false
$closeRequested = $false
$deadline = [datetime]::UtcNow.AddSeconds([int]$context.readinessTimeoutSeconds)
try {
    while ([datetime]::UtcNow -lt $deadline) {
        if (-not (Test-RootIdentity -Context $context)) {
            throw 'Slint root process exited or changed identity before readiness'
        }
        $state = Get-SlintMarkerState -Path $context.frontendMarkerPath
        switch ([string]$context.scenario) {
            'autostart-tray' {
                if ($state.WindowCreated.Count -ne 0) {
                    throw 'autostart tray created a Slint window before Open'
                }
                if ($state.TrayReady.Count -gt 0) {
                    Write-DriverMarker -Path $context.markerPath -Kind ready `
                        -Detail 'tray-first services ready with no Slint window or presentation resources'
                    $readyPublished = $true
                }
            }
            'close-to-tray' {
                if (-not $closeRequested -and $state.WindowCreated.Count -eq 1 -and
                    $state.Ready.Count -gt 0) {
                    $closeRequested = Request-SlintWindowClose -Context $context
                }
                if ($state.WindowCreated.Count -gt 1) {
                    throw 'close-to-tray created more than one Slint window'
                }
                if ($closeRequested -and $state.WindowDropped.Count -eq 1) {
                    Write-DriverMarker -Path $context.markerPath -Kind ready `
                        -Detail 'native close request dropped the Slint window and returned to tray'
                    $readyPublished = $true
                }
            }
            default {
                if ($state.Ready.Count -gt 0) {
                    Write-DriverMarker -Path $context.markerPath -Kind ready `
                        -Detail ([string]$state.Ready[-1].detail)
                    $readyPublished = $true
                }
            }
        }
        if ($readyPublished) { break }
        Start-Sleep -Milliseconds 50
    }
    if (-not $readyPublished) {
        throw 'Slint spike did not publish semantic readiness before the deadline'
    }

    while (-not (Test-Path -LiteralPath $context.stopPath -PathType Leaf)) {
        if (-not (Test-RootIdentity -Context $context)) {
            throw 'Slint root process exited during the measurement window'
        }
        Get-SlintMarkerState -Path $context.frontendMarkerPath | Out-Null
        Start-Sleep -Milliseconds 100
    }

    $shutdownDeadline = [datetime]::UtcNow.AddSeconds(10)
    while ((Test-RootIdentity -Context $context) -and [datetime]::UtcNow -lt $shutdownDeadline) {
        Get-SlintMarkerState -Path $context.frontendMarkerPath | Out-Null
        Start-Sleep -Milliseconds 50
    }
    if (Test-RootIdentity -Context $context) {
        throw 'Slint spike did not shut down cleanly after the harness stop signal'
    }
    # Once the producer is gone, its final line can no longer be partial due
    # to concurrent writing. Validate it too, including any terminal error.
    Get-SlintMarkerState -Path $context.frontendMarkerPath -IncludeIncompleteFinalLine | Out-Null
} catch {
    Write-DriverMarker -Path $context.markerPath -Kind error -Detail $_.Exception.Message
    throw
}
