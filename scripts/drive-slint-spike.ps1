[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$ContextPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-LatestMarker {
    param([Parameter(Mandatory = $true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $null }
    $latest = $null
    foreach ($line in Get-Content -LiteralPath $Path) {
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        $marker = $line | ConvertFrom-Json
        if ($marker.schemaVersion -ne 1) { throw 'unsupported Slint marker schema version' }
        if ($marker.kind -in @('ready', 'error')) { $latest = $marker }
    }
    return $latest
}

function Test-RootIdentity {
    param([Parameter(Mandatory = $true)][object]$Context)
    $row = Get-CimInstance Win32_Process -Filter "ProcessId=$($Context.rootProcessId)"
    if (-not $row -or $row.Name -ne $Context.rootProcessName) { return $false }
    $expected = ([datetime]$Context.rootStartUtc).ToUniversalTime()
    $actual = ([datetime]$row.CreationDate).ToUniversalTime()
    return [math]::Abs(($actual - $expected).TotalSeconds) -le 2.0
}

$context = Get-Content -LiteralPath $ContextPath -Raw | ConvertFrom-Json
if ($context.schemaVersion -ne 1 -or $context.frontend -ne 'slint') {
    throw 'driver context is not a Slint baseline context'
}
$deadline = [datetime]::UtcNow.AddSeconds([int]$context.readinessTimeoutSeconds)
while ([datetime]::UtcNow -lt $deadline) {
    if (-not (Test-RootIdentity -Context $context)) {
        throw 'Slint root process exited or changed identity before readiness'
    }
    $marker = Get-LatestMarker -Path $context.markerPath
    if ($marker -and $marker.kind -eq 'error') { throw "Slint spike failed: $($marker.detail)" }
    if ($marker -and $marker.kind -eq 'ready') { break }
    Start-Sleep -Milliseconds 50
}
if (-not $marker -or $marker.kind -ne 'ready') {
    throw 'Slint spike did not publish semantic readiness before the deadline'
}

while (-not (Test-Path -LiteralPath $context.stopPath -PathType Leaf)) {
    if (-not (Test-RootIdentity -Context $context)) {
        throw 'Slint root process exited during the measurement window'
    }
    $marker = Get-LatestMarker -Path $context.markerPath
    if ($marker -and $marker.kind -eq 'error') { throw "Slint spike failed: $($marker.detail)" }
    Start-Sleep -Milliseconds 100
}

$shutdownDeadline = [datetime]::UtcNow.AddSeconds(10)
while ((Test-RootIdentity -Context $context) -and [datetime]::UtcNow -lt $shutdownDeadline) {
    Start-Sleep -Milliseconds 50
}
if (Test-RootIdentity -Context $context) {
    throw 'Slint spike did not shut down cleanly after the harness stop signal'
}
