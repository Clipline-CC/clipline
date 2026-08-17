[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('Initialize', 'InstallTauriCli', 'Measure', 'Snapshot', 'Summary', 'SelfTest')]
    [string]$Action,

    [string]$Name,
    [string]$Executable,
    [string[]]$CommandArguments = @(),
    [string]$WorkingDirectory = (Get-Location).Path,
    [string]$MonitorProcess,
    [string]$OutputDirectory = $(
        if ($env:BENCHMARK_DIR) { $env:BENCHMARK_DIR }
        else { Join-Path $env:TEMP 'clipline-nightly-benchmark' }
    )
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$tauriVersion = '2.11.2'
$tauriArchiveName = 'cargo-tauri-x86_64-pc-windows-msvc.zip'
$tauriArchiveSize = 7414116
$tauriArchiveSha256 = 'b6844470bcbf1da6e5dbf01990ae317d4d7969171628bb8badbdbff2e3d06d23'
$tauriArchiveUrl = "https://github.com/tauri-apps/tauri/releases/download/tauri-cli-v$tauriVersion/$tauriArchiveName"

function Write-JsonFile {
    param([Parameter(Mandatory)]$Value, [Parameter(Mandatory)][string]$Path)

    $Value | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $Path -Encoding utf8NoBOM
}

function Get-PathStats {
    param([Parameter(Mandatory)][string]$Path)

    if (-not (Test-Path -LiteralPath $Path)) {
        return [ordered]@{ path = $Path; exists = $false; files = 0; bytes = 0 }
    }

    $item = Get-Item -LiteralPath $Path
    if ($item.PSIsContainer) {
        $files = @(Get-ChildItem -LiteralPath $Path -Recurse -File)
    } else {
        $files = @($item)
    }
    [ordered]@{
        path = $Path
        exists = $true
        files = $files.Count
        bytes = [long](($files | Measure-Object -Property Length -Sum).Sum ?? 0)
    }
}

function Write-Timing {
    param([Parameter(Mandatory)]$Value)

    New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
    $safeName = $Value.name -replace '[^a-zA-Z0-9_.-]', '-'
    Write-JsonFile $Value (Join-Path $OutputDirectory "timing-$safeName.json")
}

function Invoke-MeasuredProcess {
    param(
        [Parameter(Mandatory)][string]$StepName,
        [Parameter(Mandatory)][string]$FilePath,
        [string[]]$Arguments = @(),
        [Parameter(Mandatory)][string]$Directory,
        [string]$ProcessToMonitor
    )

    $started = [DateTimeOffset]::UtcNow
    $watched = @{}
    $process = Start-Process -FilePath $FilePath -ArgumentList $Arguments `
        -WorkingDirectory $Directory -NoNewWindow -PassThru

    while (-not $process.HasExited) {
        if ($ProcessToMonitor) {
            foreach ($child in @(Get-Process -Name $ProcessToMonitor -ErrorAction SilentlyContinue)) {
                $sampledAt = [DateTimeOffset]::UtcNow
                if (-not $watched.ContainsKey($child.Id)) {
                    $watched[$child.Id] = [ordered]@{
                        pid = $child.Id
                        first_seen_utc = $sampledAt.ToString('O')
                        last_seen_utc = $sampledAt.ToString('O')
                        cpu_seconds = 0.0
                        peak_working_set_bytes = 0L
                    }
                }
                try {
                    $entry = $watched[$child.Id]
                    $entry.last_seen_utc = $sampledAt.ToString('O')
                    $entry.cpu_seconds = $child.TotalProcessorTime.TotalSeconds
                    $entry.peak_working_set_bytes = [long][Math]::Max(
                        $entry.peak_working_set_bytes,
                        $child.WorkingSet64
                    )
                } catch {
                    # The short-lived process exited between discovery and sampling.
                }
            }
        }
        Start-Sleep -Milliseconds 200
        $process.Refresh()
    }
    $process.WaitForExit()
    $completed = [DateTimeOffset]::UtcNow
    $timing = [ordered]@{
        type = 'command'
        name = $StepName
        command = (@($FilePath) + $Arguments) -join ' '
        working_directory = $Directory
        started_utc = $started.ToString('O')
        completed_utc = $completed.ToString('O')
        duration_seconds = [Math]::Round(($completed - $started).TotalSeconds, 3)
        exit_code = $process.ExitCode
        monitored_processes = @($watched.Values)
    }
    Write-Timing $timing
    if ($process.ExitCode -ne 0) {
        throw "$StepName failed with exit code $($process.ExitCode)."
    }
}

function Initialize-Benchmark {
    New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
    $workspacePath = (Resolve-Path -LiteralPath $WorkingDirectory).Path
    $drive = [System.IO.Path]::GetPathRoot($workspacePath).TrimEnd('\')
    $logicalDisk = Get-CimInstance Win32_LogicalDisk -Filter "DeviceID='$drive'"
    $processors = @(Get-CimInstance Win32_Processor)
    $computer = Get-CimInstance Win32_ComputerSystem
    $operatingSystem = Get-CimInstance Win32_OperatingSystem
    $physicalDisks = try {
        @(Get-PhysicalDisk | ForEach-Object {
            [ordered]@{
                name = $_.FriendlyName
                media_type = [string]$_.MediaType
                bus_type = [string]$_.BusType
                size_bytes = [long]$_.Size
            }
        })
    } catch {
        @()
    }
    $runnerReadyPath = Join-Path $OutputDirectory 'runner-ready.txt'

    $metadata = [ordered]@{
        schema_version = 1
        captured_utc = [DateTimeOffset]::UtcNow.ToString('O')
        runner_ready_utc = if (Test-Path -LiteralPath $runnerReadyPath) {
            (Get-Content -Raw -LiteralPath $runnerReadyPath).Trim()
        } else { $null }
        commit = $env:GITHUB_SHA
        provider = $env:BENCHMARK_PROVIDER
        runner_label = $env:BENCHMARK_RUNNER_LABEL
        cache_strategy = $env:BENCHMARK_CACHE_STRATEGY
        cache_epoch = $env:BENCHMARK_CACHE_EPOCH
        expected_cache = $env:BENCHMARK_EXPECTED_CACHE
        repetition = $env:BENCHMARK_REPETITION
        runner = [ordered]@{
            name = $env:RUNNER_NAME
            arch = $env:RUNNER_ARCH
            image_os = $env:ImageOS
            image_version = $env:ImageVersion
        }
        operating_system = [ordered]@{
            caption = $operatingSystem.Caption
            version = $operatingSystem.Version
            build = $operatingSystem.BuildNumber
        }
        cpu = [ordered]@{
            models = @($processors | ForEach-Object Name)
            logical_processors = [int](($processors | Measure-Object -Property NumberOfLogicalProcessors -Sum).Sum)
            max_clock_mhz = [int](($processors | Measure-Object -Property MaxClockSpeed -Maximum).Maximum)
        }
        memory_bytes = [long]$computer.TotalPhysicalMemory
        workspace_disk = [ordered]@{
            drive = $drive
            filesystem = $logicalDisk.FileSystem
            size_bytes = [long]$logicalDisk.Size
            free_bytes = [long]$logicalDisk.FreeSpace
        }
        physical_disks = $physicalDisks
    }
    Write-JsonFile $metadata (Join-Path $OutputDirectory 'system.json')
}

function Install-TauriCli {
    New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
    $started = [DateTimeOffset]::UtcNow
    $archive = Join-Path $OutputDirectory $tauriArchiveName
    $destination = Join-Path $OutputDirectory 'tauri-cli'
    Invoke-WebRequest -UseBasicParsing -Uri $tauriArchiveUrl -OutFile $archive
    $download = Get-Item -LiteralPath $archive
    if ($download.Length -ne $tauriArchiveSize) {
        throw "Tauri CLI archive size mismatch: expected $tauriArchiveSize, got $($download.Length)."
    }
    $actualHash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -cne $tauriArchiveSha256) {
        throw "Tauri CLI archive SHA-256 mismatch: expected $tauriArchiveSha256, got $actualHash."
    }
    Expand-Archive -LiteralPath $archive -DestinationPath $destination -Force
    $binaries = @(Get-ChildItem -LiteralPath $destination -Recurse -Filter cargo-tauri.exe -File)
    if ($binaries.Count -ne 1) {
        throw "Expected exactly one cargo-tauri.exe in the pinned Tauri CLI archive."
    }
    $versionOutput = (& $binaries[0].FullName --version | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $versionOutput -cne "tauri-cli $tauriVersion") {
        throw "Expected tauri-cli $tauriVersion, got '$versionOutput'."
    }
    $binaries[0].DirectoryName | Add-Content -LiteralPath $env:GITHUB_PATH
    $completed = [DateTimeOffset]::UtcNow
    Write-Timing ([ordered]@{
        type = 'command'
        name = 'tauri-cli-prebuilt'
        command = $tauriArchiveUrl
        working_directory = $WorkingDirectory
        started_utc = $started.ToString('O')
        completed_utc = $completed.ToString('O')
        duration_seconds = [Math]::Round(($completed - $started).TotalSeconds, 3)
        exit_code = 0
        archive_bytes = $download.Length
        archive_sha256 = $actualHash
        version_output = $versionOutput
        monitored_processes = @()
    })
}

function Write-Snapshot {
    $root = (Resolve-Path -LiteralPath $WorkingDirectory).Path
    $paths = @(
        Get-PathStats (Join-Path $root 'apps/clipline-app/ffmpeg')
        Get-PathStats (Join-Path $root 'apps/clipline-app/webview2-fixed')
        Get-PathStats (Join-Path $root 'target/release/clipline-app.exe')
        Get-PathStats (Join-Path $root 'target/release/ffmpeg')
        Get-PathStats (Join-Path $root 'target/release/webview2-fixed')
        Get-PathStats (Join-Path $root 'target/release/bundle/nsis')
        Get-PathStats (Join-Path $root 'dist')
    )
    if ($Name -eq 'packages') {
        $cargoRoot = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
        $paths += Get-PathStats (Join-Path $root 'target')
        $paths += Get-PathStats (Join-Path $cargoRoot 'registry')
        $paths += Get-PathStats (Join-Path $cargoRoot 'git')
    }
    $snapshot = [ordered]@{
        type = 'snapshot'
        name = $Name
        captured_utc = [DateTimeOffset]::UtcNow.ToString('O')
        paths = $paths
    }
    Write-JsonFile $snapshot (Join-Path $OutputDirectory "snapshot-$Name.json")
}

function Complete-Benchmark {
    $systemPath = Join-Path $OutputDirectory 'system.json'
    if (-not (Test-Path -LiteralPath $systemPath)) {
        Write-Warning 'Benchmark initialization did not complete; no partial summary is available.'
        return
    }
    $metadata = Get-Content -Raw -LiteralPath $systemPath | ConvertFrom-Json
    $timings = @(Get-ChildItem -LiteralPath $OutputDirectory -Filter 'timing-*.json' -File |
        ForEach-Object { Get-Content -Raw -LiteralPath $_.FullName | ConvertFrom-Json } |
        Sort-Object started_utc)
    $snapshots = @(Get-ChildItem -LiteralPath $OutputDirectory -Filter 'snapshot-*.json' -File |
        ForEach-Object { Get-Content -Raw -LiteralPath $_.FullName | ConvertFrom-Json })
    $cache = if (Test-Path -LiteralPath (Join-Path $OutputDirectory 'cache.json')) {
        Get-Content -Raw -LiteralPath (Join-Path $OutputDirectory 'cache.json') | ConvertFrom-Json
    } else { $null }
    $sccache = if (Test-Path -LiteralPath (Join-Path $OutputDirectory 'sccache.json')) {
        Get-Content -Raw -LiteralPath (Join-Path $OutputDirectory 'sccache.json') | ConvertFrom-Json
    } else { $null }
    $report = [ordered]@{
        schema_version = 1
        system = $metadata
        cache = $cache
        commands = $timings
        snapshots = $snapshots
        sccache = $sccache
        note = 'GitHub action and post-job timings are added by the aggregate report.'
    }
    Write-JsonFile $report (Join-Path $OutputDirectory 'benchmark.json')

    if ($env:GITHUB_STEP_SUMMARY) {
        @(
            "## Windows Nightly benchmark — $($metadata.provider)"
            ''
            "Commit: ``$($metadata.commit)`` · runner: ``$($metadata.runner_label)`` · cache: ``$($metadata.cache_strategy)`` ($($cache.hit))"
            ''
            "CPU: $($metadata.cpu.models -join ', ') ($($metadata.cpu.logical_processors) logical) · RAM: $([Math]::Round($metadata.memory_bytes / 1GB, 1)) GiB · disk: $([Math]::Round($metadata.workspace_disk.size_bytes / 1GB, 1)) GiB"
            ''
            '| Measured command | Wall time | makensis CPU | Core equivalent | Host CPU |'
            '|---|---:|---:|---:|---:|'
        ) | Add-Content -LiteralPath $env:GITHUB_STEP_SUMMARY
        foreach ($timing in $timings) {
            $nsisCpu = [Math]::Round((@($timing.monitored_processes | ForEach-Object cpu_seconds) | Measure-Object -Sum).Sum, 1)
            $nsisWall = (@($timing.monitored_processes | ForEach-Object {
                ([DateTimeOffset]$_.last_seen_utc - [DateTimeOffset]$_.first_seen_utc).TotalSeconds
            }) | Measure-Object -Maximum).Maximum
            $coreEquivalent = if ($nsisWall) { [Math]::Round($nsisCpu / $nsisWall, 2) } else { 0 }
            $hostCpu = if ($metadata.cpu.logical_processors) {
                [Math]::Round(100 * $coreEquivalent / $metadata.cpu.logical_processors, 1)
            } else { 0 }
            "| $($timing.name) | $([Math]::Round($timing.duration_seconds, 1))s | ${nsisCpu}s | $coreEquivalent | $hostCpu% |" |
                Add-Content -LiteralPath $env:GITHUB_STEP_SUMMARY
        }
        @(
            ''
            'Action setup/restore/upload/save and total job timings appear in the aggregate summary after all providers finish.'
        ) | Add-Content -LiteralPath $env:GITHUB_STEP_SUMMARY
    }
}

switch ($Action) {
    'Initialize' { Initialize-Benchmark }
    'InstallTauriCli' { Install-TauriCli }
    'Measure' {
        if (-not $Name -or -not $Executable) { throw 'Measure requires -Name and -Executable.' }
        Invoke-MeasuredProcess -StepName $Name -FilePath $Executable -Arguments $CommandArguments `
            -Directory $WorkingDirectory -ProcessToMonitor $MonitorProcess
    }
    'Snapshot' {
        if (-not $Name) { throw 'Snapshot requires -Name.' }
        New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
        Write-Snapshot
    }
    'Summary' { Complete-Benchmark }
    'SelfTest' {
        $testRoot = Join-Path ([System.IO.Path]::GetTempPath()) "clipline-benchmark-$([Guid]::NewGuid())"
        New-Item -ItemType Directory -Path $testRoot | Out-Null
        Set-Content -LiteralPath (Join-Path $testRoot 'one') -Value '123' -NoNewline
        Set-Content -LiteralPath (Join-Path $testRoot 'two') -Value '45' -NoNewline
        $stats = Get-PathStats $testRoot
        if ($stats.files -ne 2 -or $stats.bytes -ne 5) { throw 'Path statistics self-test failed.' }
        $fileStats = Get-PathStats (Join-Path $testRoot 'one')
        if ($fileStats.files -ne 1 -or $fileStats.bytes -ne 3) {
            throw 'Single-file statistics self-test failed.'
        }
        $originalOutput = $OutputDirectory
        $OutputDirectory = $testRoot
        Invoke-MeasuredProcess -StepName self-test -FilePath $env:ComSpec `
            -Arguments @('/d', '/c', 'exit', '0') -Directory $testRoot
        $timing = Get-Content -Raw -LiteralPath (Join-Path $testRoot 'timing-self-test.json') | ConvertFrom-Json
        if ($timing.exit_code -ne 0 -or $timing.duration_seconds -lt 0) {
            throw 'Process timing self-test failed.'
        }
        $OutputDirectory = $originalOutput
        Write-Host 'benchmark-windows-nightly.ps1 self-test passed.'
    }
}
