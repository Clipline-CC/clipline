# SPDX-License-Identifier: MIT OR Apache-2.0
# Build two independently selectable game identities. Requires MSVC C++ Build Tools.
param([Parameter(Mandatory = $true)][string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
if (Test-Path -LiteralPath $OutputDirectory) { throw 'OutputDirectory must be new.' }
$null = New-Item -ItemType Directory -Path $OutputDirectory
$output = (Resolve-Path -LiteralPath $OutputDirectory).Path
$source = Join-Path $PSScriptRoot 'windows\mock_game.cpp'
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vs = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if ($LASTEXITCODE -ne 0 -or -not $vs) { throw 'MSVC C++ Build Tools not found.' }
$environment = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'
# Reject batch metacharacters before writing literal, quoted compiler paths.
foreach ($path in @($environment, $source, $output, (Split-Path $vswhere))) {
    if ($path -match '[^\x20-\x7E]') { throw 'The batch build requires ASCII paths.' }
    if ($path -match '[%!?&|<>^"\r\n]') { throw 'Build paths contain unsupported batch characters.' }
}
$batch = Join-Path $output 'build.cmd'
$lines = @(
    '@echo off',
    ('set "PATH=' + (Split-Path $vswhere) + ';%PATH%"'),
    ('call "' + $environment + '" >nul'),
    'if errorlevel 1 exit /b %errorlevel%',
    ('cl /nologo /std:c++17 /EHsc /W4 /WX /O2 /MT /DUNICODE /D_UNICODE "' + $source + '" /Fo"' + (Join-Path $output 'mock_game.obj') + '" /Fe"' + (Join-Path $output 'CliplineMockBlt.exe') + '" /link /SUBSYSTEM:WINDOWS d3d11.lib dxgi.lib winmm.lib user32.lib'),
    'exit /b %errorlevel%'
)
$lines | Set-Content -LiteralPath $batch -Encoding ASCII
& $env:ComSpec /d /c ('"' + $batch + '"')
if ($LASTEXITCODE -ne 0) { throw "Mock game build failed: $LASTEXITCODE" }
Copy-Item -LiteralPath (Join-Path $output 'CliplineMockBlt.exe') -Destination (Join-Path $output 'CliplineMockFlip.exe')
Get-FileHash -LiteralPath (Join-Path $output 'CliplineMockBlt.exe'), (Join-Path $output 'CliplineMockFlip.exe')
