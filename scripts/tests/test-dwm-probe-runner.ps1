# End-to-end runner regressions. Uses .NET test executables, no GPU or Rust linker.
param([string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
if (-not $OutputDirectory) { $OutputDirectory = Join-Path ([IO.Path]::GetTempPath()) ('dwm runner tests ' + [Guid]::NewGuid().ToString('N')) }
if (Test-Path -LiteralPath $OutputDirectory) { throw 'OutputDirectory must be new.' }
$null = New-Item -ItemType Directory -Path $OutputDirectory
$OutputDirectory = (Resolve-Path -LiteralPath $OutputDirectory).Path
$runner = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\test-dwm-probe.ps1')).Path
function Assert([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function New-TestExe([string]$Name, [string]$Body) {
    $path = Join-Path $OutputDirectory "$Name.exe"
    Add-Type -TypeDefinition ('public static class ' + $Name + ' { public static int Main(string[] args) { ' + $Body + ' } }') -OutputAssembly $path -OutputType ConsoleApplication
    return $path
}
function Run-Check([string]$Exe, [string]$Name, [switch]$PreflightOnly) {
    $resultDirectory = Join-Path $OutputDirectory $Name
    # Invalid TargetPath proves preflight failures occur before fixture startup.
    $extraArguments = @()
    if ($PreflightOnly) { $extraArguments += '-PreflightOnly' }
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $runner -ProbePath $Exe -TargetPath (Join-Path $OutputDirectory 'not-a-target.ps1') -OutputDirectory $resultDirectory @extraArguments 1> (Join-Path $OutputDirectory "$Name.console.txt") 2> (Join-Path $OutputDirectory "$Name.error.txt")
    return $LASTEXITCODE
}
$missing = New-TestExe 'MissingRuntime' 'return unchecked((int)0xC0000135);'
$listFailure = New-TestExe 'ListFailure' 'System.Console.WriteLine(args[0]); return args[0] == "--list" ? 7 : 0;'
$success = New-TestExe 'Success' 'System.Console.WriteLine(args[0]); return 0;'
$timeout = New-TestExe 'Timeout' 'System.Console.WriteLine("before stall"); System.Console.Error.WriteLine("stall diagnostic"); System.Threading.Thread.Sleep(30000); return 0;'

# Native stderr is an ErrorRecord in Windows PowerShell; inspect the child exit code.
$ErrorActionPreference = 'Continue'
$missingExit = Run-Check $missing 'missing'
$listExit = Run-Check $listFailure 'list-failure'
$successExit = Run-Check $success 'success' -PreflightOnly
$timeoutExit = Run-Check $timeout 'timeout'
$ErrorActionPreference = 'Stop'
Assert ($missingExit -ne 0) 'Missing-runtime exit was ignored.'
Assert ((Get-Content -Raw (Join-Path $OutputDirectory 'missing\help.exit.txt')) -match '0xC0000135') 'Original native loader code was lost.'
Assert (-not (Test-Path (Join-Path $OutputDirectory 'missing\list.exit.txt'))) 'Runner continued to enumeration after loader failure.'
Assert ($listExit -ne 0) 'Enumeration exit was ignored.'
Assert ((Get-Content -Raw (Join-Path $OutputDirectory 'list-failure\list.exit.txt')) -match '0x00000007') 'Enumeration exit evidence missing.'
Assert ($successExit -eq 0) 'Working preflight failed.'
Assert ((Get-Content -Raw (Join-Path $OutputDirectory 'success\list.stdout.txt')) -match '--list') 'Working enumeration evidence missing.'
Assert ($timeoutExit -ne 0) 'Stalled probe passed.'
Assert ((Get-Content -Raw (Join-Path $OutputDirectory 'timeout\help.stdout.txt')) -match 'before stall') 'Timeout stdout lost.'
Assert ((Get-Content -Raw (Join-Path $OutputDirectory 'timeout\help.stderr.txt')) -match 'stall diagnostic') 'Timeout stderr lost.'
Assert ((Get-Content -Raw (Join-Path $OutputDirectory 'timeout\help.exit.txt')) -match 'timeout_seconds=15') 'Timeout marker missing.'
Write-Output "PASS: loader failure, enumeration failure, working preflight, timeout evidence, paths with spaces. Evidence: $OutputDirectory"
# CI shells otherwise inherit the last intentionally failing native child exit code.
exit 0
