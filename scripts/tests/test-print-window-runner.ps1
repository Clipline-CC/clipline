# SPDX-License-Identifier: MIT OR Apache-2.0
# Test process supervision with fake workers: no window or capture API is called.
param([string]$OutputDirectory)
$ErrorActionPreference='Stop'
if (-not $OutputDirectory) { $OutputDirectory=Join-Path $env:TEMP ('print window tests '+[Guid]::NewGuid().ToString('N')) }
if (Test-Path -LiteralPath $OutputDirectory) { throw 'OutputDirectory must be new.' }
$null=New-Item -ItemType Directory -Path (Join-Path $OutputDirectory 'windows')
$runner=Join-Path $OutputDirectory 'test-print-window.ps1'
Copy-Item -LiteralPath (Join-Path $PSScriptRoot '..\test-print-window.ps1') -Destination $runner
$worker=Join-Path $OutputDirectory 'windows\print-window-worker.ps1'
$cases=@(
    @{Name='success';Body='Write-Output "worker succeeded"; exit 0';Code=0},
    @{Name='failure';Body='[Console]::Error.WriteLine("worker failure"); exit 7';Code=1},
    @{Name='timeout';Body='Write-Output "before stall"; Start-Sleep -Seconds 35';Code=1}
)
foreach ($case in $cases) {
    $case.Body | Set-Content -LiteralPath $worker
    $destination=Join-Path $OutputDirectory $case.Name
    $ErrorActionPreference='Continue'
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $runner -Hwnd 42 -ExpectedProcessId 42 -Seconds 1 -OutputDirectory $destination 1> (Join-Path $OutputDirectory ($case.Name+'.stdout.txt')) 2> (Join-Path $OutputDirectory ($case.Name+'.stderr.txt'))
    $code=$LASTEXITCODE
    $ErrorActionPreference='Stop'
    if ($code -ne $case.Code) { throw "Unexpected wrapper exit for $($case.Name): $code" }
    $exit=Get-Content (Join-Path $destination 'exit.txt') -Raw
    switch ($case.Name) {
        'success' { if ($exit -notmatch 'worker_exit=0') { throw 'Successful worker exit was lost.' } }
        'failure' {
            if ($exit -notmatch 'worker_exit=7') { throw 'Failure exit was lost.' }
            if ((Get-Content (Join-Path $destination 'worker.stderr.txt') -Raw) -notmatch 'worker failure') { throw 'Worker stderr was lost.' }
        }
        'timeout' {
            if ($exit -notmatch 'worker_timeout=True') { throw 'Worker timeout was not recorded.' }
            if ((Get-Content (Join-Path $destination 'worker.stdout.txt') -Raw) -notmatch 'before stall') { throw 'Pre-timeout output was lost.' }
        }
    }
}
Write-Output 'PASS: success/failure exit codes, timeout, diagnostic output, paths with spaces.'
exit 0
