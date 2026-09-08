# SPDX-License-Identifier: MIT OR Apache-2.0
# Isolated window-only diagnostic. Never used by Clipline's recorder.
param(
    [Parameter(Mandatory=$true)][ValidateRange(1,[long]::MaxValue)][long]$Hwnd,
    [Parameter(Mandatory=$true)][ValidateRange(1,[int]::MaxValue)][int]$ExpectedProcessId,
    [Parameter(Mandatory=$true)][string]$OutputDirectory,
    [ValidateRange(1,600)][int]$Seconds=10,
    [ValidateRange(1,60)][int]$Fps=30,
    [ValidateSet(0,2)][int]$Flags=2,
    [switch]$ExpectMockMotion
)
$ErrorActionPreference='Stop'
if (Test-Path -LiteralPath $OutputDirectory) { throw 'OutputDirectory must be new.' }
$null=New-Item -ItemType Directory -Path $OutputDirectory
$output=(Resolve-Path -LiteralPath $OutputDirectory).Path
$worker=Join-Path $PSScriptRoot 'windows\print-window-worker.ps1'
$arguments=@('-NoProfile','-ExecutionPolicy','Bypass','-File',('"'+$worker+'"'),
    '-Hwnd',$Hwnd,'-ExpectedProcessId',$ExpectedProcessId,'-OutputDirectory',('"'+$output+'"'),
    '-Seconds',$Seconds,'-Fps',$Fps,'-Flags',$Flags)
if ($ExpectMockMotion) { $arguments+='-ExpectMockMotion' }
$process=New-Object Diagnostics.Process
$process.StartInfo.FileName='powershell.exe'
$process.StartInfo.Arguments=$arguments -join ' '
$process.StartInfo.UseShellExecute=$false
$process.StartInfo.CreateNoWindow=$true
$process.StartInfo.RedirectStandardOutput=$true
$process.StartInfo.RedirectStandardError=$true
try {
    # Own the process handle directly. Start-Process -PassThru can lose ExitCode
    # after a timed wait in Windows PowerShell, even when the worker succeeded.
    $null=$process.Start()
    $stdout=$process.StandardOutput.ReadToEndAsync()
    $stderr=$process.StandardError.ReadToEndAsync()
    # PrintWindow is synchronous. Terminate only this helper if the call hangs;
    # never abort a thread inside Clipline or terminate the selected application.
    $timedOut=-not $process.WaitForExit(($Seconds+15)*1000)
    if ($timedOut) {
        $process.Kill()
        if (-not $process.WaitForExit(2000)) { throw 'Timed-out helper did not terminate.' }
    }
    $stdout.Result | Set-Content (Join-Path $output 'worker.stdout.txt')
    $stderr.Result | Set-Content (Join-Path $output 'worker.stderr.txt')
    $code=$process.ExitCode
    "worker_exit=$code`nworker_timeout=$timedOut" | Set-Content (Join-Path $output 'exit.txt')
    if ($timedOut) { throw 'PrintWindow helper timed out; evidence preserved.' }
    if (Test-Path (Join-Path $output 'summary.json')) { Get-Content (Join-Path $output 'summary.json') }
    if ($code -ne 0) { throw "PrintWindow diagnostic failed ($code); inspect worker.stderr.txt." }
} finally { $process.Dispose() }
