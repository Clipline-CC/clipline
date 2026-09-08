# Standalone probe preflight and controlled fixture. Never starts Clipline recording.
param(
    [string]$ProbePath,
    [string]$TargetPath,
    [Parameter(Mandatory = $true)][string]$OutputDirectory,
    [switch]$PreflightOnly
)
$ErrorActionPreference = 'Stop'
if (-not $ProbePath) { $ProbePath = Join-Path $PSScriptRoot 'dwm_probe.exe' }
if (-not $TargetPath) { $TargetPath = Join-Path $PSScriptRoot 'dwm-probe-target.ps1' }
$ProbePath = (Resolve-Path -LiteralPath $ProbePath).Path
if (Test-Path -LiteralPath $OutputDirectory) { throw 'OutputDirectory must be new.' }
$null = New-Item -ItemType Directory -Path $OutputDirectory
$OutputDirectory = (Resolve-Path -LiteralPath $OutputDirectory).Path

function Invoke-Probe([string]$Arguments, [string]$LogName, [int]$TimeoutSeconds = 15) {
    $process = New-Object Diagnostics.Process
    $process.StartInfo.FileName = $ProbePath
    $process.StartInfo.Arguments = $Arguments
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.CreateNoWindow = $true
    $process.StartInfo.RedirectStandardOutput = $true
    $process.StartInfo.RedirectStandardError = $true
    try {
        $null = $process.Start()
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $timedOut = -not $process.WaitForExit($TimeoutSeconds * 1000)
        if ($timedOut) {
            $process.Kill()
            $process.WaitForExit()
        }
        $stdout.Result | Set-Content -LiteralPath (Join-Path $OutputDirectory "$LogName.stdout.txt")
        $stderr.Result | Set-Content -LiteralPath (Join-Path $OutputDirectory "$LogName.stderr.txt")
        $code = $process.ExitCode
        $hex = '0x{0:X8}' -f [BitConverter]::ToUInt32([BitConverter]::GetBytes([int]$code), 0)
        "exit_code=$code ($hex)" | Set-Content -LiteralPath (Join-Path $OutputDirectory "$LogName.exit.txt")
        if ($timedOut) {
            "timeout_seconds=$TimeoutSeconds" | Add-Content -LiteralPath (Join-Path $OutputDirectory "$LogName.exit.txt")
            throw "Probe $LogName timed out after $TimeoutSeconds seconds; stdout/stderr were preserved."
        }
        if ($code -ne 0) {
            $hint = if ($hex -eq '0xC0000135') {
                ' A required DLL is missing. Use the complete package with the official Microsoft x64 runtime; this is not a DWM capture result.'
            } else { ' Inspect the stdout/stderr logs before diagnosing the target.' }
            throw "Probe $LogName failed: $code ($hex).$hint"
        }
        return $stdout.Result
    } finally {
        $process.Dispose()
    }
}

# A loader failure must stop the run BEFORE launching or waiting for the fixture.
$null = Invoke-Probe '--help' 'help'
$null = Invoke-Probe '--list' 'list'
if ($PreflightOnly) { Write-Output "Probe preflight passed: $OutputDirectory"; return }

$TargetPath = (Resolve-Path -LiteralPath $TargetPath).Path
$machine = [ordered]@{
    utc = [DateTime]::UtcNow.ToString('o')
    windows = Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, BuildNumber
    cpu = Get-CimInstance Win32_Processor | Select-Object Name
    gpu = @(Get-CimInstance Win32_VideoController | Select-Object Name, DriverVersion, VideoModeDescription)
    executable_sha256 = (Get-FileHash -LiteralPath $ProbePath).Hash
}
$machine | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'machine.json')
$fixture = Start-Process powershell.exe -WindowStyle Hidden -PassThru -ArgumentList (
    '-NoProfile -ExecutionPolicy Bypass -File "' + $TargetPath + '"'
) -RedirectStandardError (Join-Path $OutputDirectory 'target.stderr.txt')
try {
    $ready = $false
    for ($i = 0; $i -lt 40; $i++) {
        $windows = Invoke-Probe '--list' 'target-list'
        if ($windows -match '(?m)^\d+\t[^\t]*\tClipline DWM Probe Target\r?$') { $ready = $true; break }
        if ($fixture.HasExited) { throw 'Controlled target exited; inspect target.stderr.txt.' }
        Start-Sleep -Milliseconds 200
    }
    if (-not $ready) { throw 'Controlled target did not appear after probe preflight passed; inspect target.stderr.txt.' }
    $captureDirectory = Join-Path $OutputDirectory 'capture'
    $result = Invoke-Probe ('--window "Clipline DWM Probe Target" --seconds 22 --fps 60 --out "' + $captureDirectory + '"') 'capture' 40
    Write-Output $result
    Write-Output "Evidence: $OutputDirectory"
} finally {
    # Only stop the fixture process started by this script, never a game or other shell.
    if (-not $fixture.HasExited) { Stop-Process -Id $fixture.Id -ErrorAction SilentlyContinue }
    $fixture.Dispose()
}
