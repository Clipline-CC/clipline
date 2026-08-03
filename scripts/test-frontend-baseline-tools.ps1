param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$module = Join-Path $PSScriptRoot 'lib/Clipline.ProcessMetrics.psm1'
Import-Module $module -Force

function Assert-Equal {
    param([object]$Expected, [object]$Actual, [string]$Message)
    if ($Expected -ne $Actual) {
        throw "$Message (expected '$Expected', got '$Actual')"
    }
}

function Assert-SequenceEqual {
    param([object[]]$Expected, [object[]]$Actual, [string]$Message)
    Assert-Equal $Expected.Count $Actual.Count "$Message count"
    for ($index = 0; $index -lt $Expected.Count; $index++) {
        Assert-Equal $Expected[$index] $Actual[$index] "$Message at index $index"
    }
}

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

function Import-TestedFunction {
    param(
        [Parameter(Mandatory = $true)][System.Management.Automation.Language.Ast]$Ast,
        [Parameter(Mandatory = $true)][string]$Name
    )
    $definition = @($Ast.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
            $node.Name -eq $Name
    }, $true)) | Select-Object -First 1
    if (-not $definition) { throw "function $Name was not found in the tested script" }
    $scriptDefinition = $definition.Extent.Text -replace '^function\s+([^\s{]+)', 'function script:$1'
    Invoke-Expression $scriptDefinition
}

$rootStart = [datetime]'2026-08-01T00:00:00Z'
$rows = @(
    [pscustomobject]@{ ProcessId = 11; ParentProcessId = 10; CreationDate = $rootStart.AddSeconds(1) },
    [pscustomobject]@{ ProcessId = 12; ParentProcessId = 11; CreationDate = $rootStart.AddSeconds(2) },
    [pscustomobject]@{ ProcessId = 13; ParentProcessId = 10; CreationDate = $rootStart.AddSeconds(-1) },
    [pscustomobject]@{ ProcessId = 14; ParentProcessId = 13; CreationDate = $rootStart.AddSeconds(3) },
    [pscustomobject]@{ ProcessId = 10; ParentProcessId = 12; CreationDate = $rootStart.AddSeconds(4) },
    [pscustomobject]@{ ProcessId = 15; ParentProcessId = 10; CreationDate = $null }
)
$descendants = @(Get-CliplineDescendantProcesses `
    -RootProcessId 10 `
    -RootStart $rootStart `
    -ProcessRows $rows)
Assert-SequenceEqual @(11, 12) @($descendants.ProcessId) 'descendant traversal must reject PID reuse and cycles'

Assert-Equal 2.5 (Get-CliplineMedian @(4, 1, 3, 2)) 'median must interpolate even sample sets'
Assert-Equal 3 (Get-CliplineMedian @(3)) 'median must support one sample'
Assert-Equal 3.7 (Get-CliplinePercentile @(1, 2, 3, 4) 0.9) 'percentile interpolation'
$emptyFailed = $false
try { Get-CliplineMedian @() | Out-Null } catch { $emptyFailed = $true }
Assert-Equal $true $emptyFailed 'empty median must fail closed'

Assert-Equal 25.0 (Get-CliplineCpuPercent 1000000 3000000 100 8) 'CPU must normalize by wall time and cores'
Assert-Equal 0.0 (Get-CliplineCpuPercent 3000000 1000000 100 8) 'CPU counter reset must not go negative'

$expectedColumns = @(
    'RunId', 'Frontend', 'Renderer', 'Scenario', 'Phase', 'SampleUtc', 'ElapsedMs',
    'ProcessId', 'ParentProcessId', 'ProcessName', 'ProcessRole', 'IsRoot',
    'PrivateWorkingSetBytes', 'PrivateCommitBytes', 'WorkingSetBytes', 'CpuTime100ns',
    'CpuPercent', 'HandleCount', 'ThreadCount', 'TreePrivateWorkingSetBytes',
    'TreePrivateCommitBytes', 'TreeWorkingSetBytes', 'TreeCpuPercent', 'TreeHandleCount',
    'TreeThreadCount', 'TreeProcessCount', 'ChildReadFailures', 'GpuCountersAvailable',
    'GpuLocalBytes', 'GpuNonLocalBytes'
)
Assert-SequenceEqual $expectedColumns @(Get-CliplineSampleColumns) 'raw CSV schema must remain stable'

$gpu = Get-CliplineGpuProcessMemory -ProcessIds @(10) -CounterReader { throw 'counter unavailable' }
Assert-Equal $false $gpu.Available 'missing GPU counters must be explicit'
Assert-Equal $null $gpu.LocalBytes 'missing GPU local memory must not be reported as zero'
Assert-Equal $null $gpu.NonLocalBytes 'missing GPU non-local memory must not be reported as zero'

Assert-Equal 'root' (Get-CliplineProcessRole 'clipline-app.exe' '' $true) 'root process role'
Assert-Equal 'webview-gpu' (Get-CliplineProcessRole 'msedgewebview2.exe' '--type=gpu-process' $false) 'GPU role'
Assert-Equal 'webview-renderer' (Get-CliplineProcessRole 'msedgewebview2.exe' '--type=renderer' $false) 'renderer role'
Assert-Equal 'ffmpeg' (Get-CliplineProcessRole 'ffmpeg.exe' '' $false) 'FFmpeg role'

if ($env:OS -eq 'Windows_NT') {
    $live = Get-CliplineNativeProcessSnapshot -ProcessId $PID
    Assert-True ($live.PrivateWorkingSetBytes -gt 0) 'live private working set must be readable'
    Assert-True ($live.PrivateCommitBytes -gt 0) 'live private commit must be readable'
    Assert-True ($live.WorkingSetBytes -gt 0) 'live ordinary working set must be readable'
    Assert-True ($live.HandleCount -gt 0) 'live handle count must be readable'
    Assert-True ($live.ThreadCount -gt 0) 'live thread count must be readable'
}

foreach ($scriptName in @(
    'measure-frontend-baseline.ps1',
    'drive-slint-spike.ps1',
    'measure-slint-library.ps1'
)) {
    $scriptPath = Join-Path $PSScriptRoot $scriptName
    $tokens = $null
    $parseErrors = $null
    [System.Management.Automation.Language.Parser]::ParseFile(
        $scriptPath,
        [ref]$tokens,
        [ref]$parseErrors
    ) | Out-Null
    Assert-Equal 0 $parseErrors.Count "$scriptName must parse without PowerShell errors"
}

$libraryMeasurePath = Join-Path $PSScriptRoot 'measure-slint-library.ps1'
$libraryTokens = $null
$libraryErrors = $null
$libraryAst = [System.Management.Automation.Language.Parser]::ParseFile(
    $libraryMeasurePath,
    [ref]$libraryTokens,
    [ref]$libraryErrors
)
foreach ($name in @(
    'Write-CliplineCreateNewText',
    'Publish-CliplineCreateNewSignal',
    'Resolve-LibraryFixture',
    'Initialize-LibraryFixtureRoot',
    'Assert-LibraryTelemetry'
)) {
    Import-TestedFunction -Ast $libraryAst -Name $name
}
$librarySource = Get-Content -LiteralPath $libraryMeasurePath -Raw
foreach ($contract in @(
    "'local-cold', 'local-warm', 'cloud-pages', 'selection-page-churn', 'reveal-close-100'",
    "'ready', 'pageSettled', 'postersSettled', 'exerciseSettled', 'error', 'stop'",
    "'create-new-atomic-rename'",
    'excludedConcurrentCliplineProcesses',
    'pwsGrowthMeasuredExternally',
    'productionCredentialsLoaded',
    'publishableAbsoluteEvidence',
    'fullLifecyclePending',
    'executedDuringMeasuredWindow',
    "realGpuGate = 'pending-winit-software-does-not-exercise-the-native-video-path'",
    "throw 'publishable Library evidence requires a clean tracked worktree'",
    'Stop-LibraryOwnedTree -ObservedTree'
)) {
    Assert-True ($librarySource.Contains($contract)) "Slint Library sampler contract missing: $contract"
}

$libraryTestRoot = Join-Path ([System.IO.Path]::GetTempPath()) `
    ("clipline-library-harness-test-{0}" -f [guid]::NewGuid())
New-Item -ItemType Directory -Path $libraryTestRoot | Out-Null
try {
    $source = Join-Path $libraryTestRoot 'source.mp4'
    [System.IO.File]::WriteAllBytes($source, [byte[]](1, 2, 3, 4, 5))
    $hash = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash.ToLowerInvariant()
    $manifest = [pscustomobject]@{
        schema_version = 1
        suite = 'clipline-native-playback-v1'
        fixtures = @([pscustomobject]@{
            file = 'source.mp4'
            artifact = [pscustomobject]@{ sha256 = $hash }
        })
        production_mux_oracles = @()
    }
    $manifest | ConvertTo-Json -Depth 8 |
        Set-Content -LiteralPath (Join-Path $libraryTestRoot 'manifest.json') -Encoding UTF8
    $fixture = Resolve-LibraryFixture -Directory $libraryTestRoot -RequestedPath 'source.mp4'
    Assert-Equal $hash $fixture.sha256 'Library source fixture must be manifest hash-verified'
    $links = Join-Path $libraryTestRoot 'links'
    $corpus = Initialize-LibraryFixtureRoot -Root $links -Fixture $fixture -Count 50
    Assert-Equal 50 $corpus.count 'Library fixture builder must create the requested exact count'
    Assert-Equal 50 @(Get-ChildItem -LiteralPath $links -File).Count `
        'Library fixture builder must create only exact hard-linked clips'
    Assert-Equal $true $corpus.allSeedsHashVerified `
        'Library fixture builder must hash-verify every bounded seed copy'
    Assert-Equal 1 $corpus.seedCount '50 clips must use one bounded hard-link seed'

    $createNew = Join-Path $libraryTestRoot 'create-new.txt'
    Write-CliplineCreateNewText -Path $createNew -Text 'first'
    $overwriteFailed = $false
    try { Write-CliplineCreateNewText -Path $createNew -Text 'second' } catch {
        $overwriteFailed = $true
    }
    Assert-True $overwriteFailed 'Library evidence writes must refuse to overwrite an existing path'
    Assert-Equal 'first' (Get-Content -LiteralPath $createNew -Raw) `
        'failed create-new publication must preserve existing evidence'

    $signal = Join-Path $libraryTestRoot 'owned.stop'
    Publish-CliplineCreateNewSignal -Path $signal
    Assert-True (Test-Path -LiteralPath $signal -PathType Leaf) `
        'owned stop signal must appear only as a closed regular file'
    $signalOverwriteFailed = $false
    try { Publish-CliplineCreateNewSignal -Path $signal } catch {
        $signalOverwriteFailed = $true
    }
    Assert-True $signalOverwriteFailed 'owned stop signal publication must not overwrite'

    $telemetryContract = [pscustomobject]@{
        processId = 42; buildSha = 'test'; adapter = 'test'; scale = 1.0
        fixtureSeedRoot = $corpus.seedRoot
    }
    $telemetry = [pscustomobject]@{
        schemaVersion = 1; status = 'completed'; publication = 'create-new-atomic-rename'
        scenario = 'reveal-close-100'; clipCount = 50
        provenance = [pscustomobject]@{
            processId = 42; processName = 'catalog_harness'; buildSha = 'test'
            renderer = 'winit-software'; adapter = 'test'; scale = 1.0
            fixtureSeedRoot = $corpus.seedRoot
        }
        sourceFixture = [pscustomobject]@{ path = $fixture.path; sha256 = $fixture.sha256 }
        metrics = [pscustomobject]@{
            firstUsablePageMs = 100; pageChangeP95Ms = 20; filterGroupP95Ms = 20
            retainedRows = 50; retainedDecodedImages = 32; posterLruEntries = 32
            posterCacheSize = 32; ffmpegChildPeak = 1; duplicateSameKeyExtractions = 0
            posterExtractionStarts = 32; singleFlightFollowers = 1
            offPageDecodedImagesAfterSettle = 0; offPageModelImagesAfterSettle = 0
            stalePublications = 0; activeLeasesAfterClose = 0
            pwsGrowthBytes = $null; pwsGrowthMeasuredExternally = $true
        }
        lifecycle = [pscustomobject]@{
            attachmentsCreated = 101; attachmentsDropped = 101
            imagesAccepted = 64; imagesReleased = 64
            posterHandlesAccepted = 32; posterHandlesReleased = 32
            modelImagesPublished = 32; modelImagesReplaced = 32
            leasesAcquired = 0; leasesReleased = 0
        }
        safety = [pscustomobject]@{ productionCredentialsLoaded = $false; cloudNetworkRequests = 0 }
        churn = [pscustomobject]@{}
        reveal = [pscustomobject]@{
            windowRevealCloseCycles = 100; windowRevealCloseCyclesPending = $false
            windowCyclesExecutedDuringMeasuredWindow = $true
            cloudMediaCycles = 0; cloudMediaCyclesPending = $true
        }
    }
    Assert-LibraryTelemetry -Telemetry $telemetry -Fixture $fixture -Count 50 `
        -ScenarioName 'reveal-close-100' -RendererName 'winit-software' `
        -Contract $telemetryContract
    $telemetry.reveal.windowRevealCloseCyclesPending = $true
    $pendingWindowAccepted = $true
    try {
        Assert-LibraryTelemetry -Telemetry $telemetry -Fixture $fixture -Count 50 `
            -ScenarioName 'reveal-close-100' -RendererName 'winit-software' `
            -Contract $telemetryContract
    } catch { $pendingWindowAccepted = $false }
    Assert-Equal $false $pendingWindowAccepted `
        'real window lifecycle may not use the Cloud-media pending escape hatch'
} finally {
    Remove-Item -LiteralPath $libraryTestRoot -Recurse -Force -ErrorAction SilentlyContinue
}

$measureSource = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'measure-frontend-baseline.ps1') -Raw
foreach ($contract in @(
    "`$frontendMarkerPath = Join-Path `$profileRoot 'slint-markers.jsonl'",
    'frontendMarkerPath = $frontendMarkerPath',
    "'--autostart'",
    'same truly lazy tray state',
    "'autostart-tray', 'review-idle', 'review-playing', 'scrub-storm'",
    "'close-to-tray', 'reveal-close-100'",
    'Assert-SlintLifecycleTelemetry -Telemetry $frontendTelemetry',
    '[long]$lifecycle.openAccepted -ne 100',
    '[long]$lifecycle.windowDropped -ne 100',
    "`$Scenario -eq 'autostart-tray'",
    "'none'"
)) {
    Assert-True ($measureSource.Contains($contract)) "Slint baseline contract missing: $contract"
}

$adapterSource = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'drive-slint-spike.ps1') -Raw
foreach ($contract in @(
    "'trayReady', 'windowCreated', 'windowDropped', 'saveReplay', 'ready', 'error'",
    'Get-SlintMarkerState -Path $context.frontendMarkerPath',
    'throw "Slint spike failed: $($errors[0].detail)"',
    'Request-SlintWindowClose -Context $context',
    '$state.Ready.Count -gt 0',
    "'autostart tray created a Slint window before Open'",
    "'native close request dropped the Slint window and returned to tray'"
)) {
    Assert-True ($adapterSource.Contains($contract)) "Slint adapter contract missing: $contract"
}

$measurePath = Join-Path $PSScriptRoot 'measure-frontend-baseline.ps1'
$measureTokens = $null
$measureErrors = $null
$measureAst = [System.Management.Automation.Language.Parser]::ParseFile(
    $measurePath,
    [ref]$measureTokens,
    [ref]$measureErrors
)
Import-TestedFunction -Ast $measureAst -Name 'Get-CliplineDriverMarker'
Import-TestedFunction -Ast $measureAst -Name 'Assert-SlintLifecycleTelemetry'

$testRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("clipline-harness-test-{0}" -f [guid]::NewGuid())
New-Item -ItemType Directory -Path $testRoot | Out-Null
try {
    $markerPath = Join-Path $testRoot 'driver.jsonl'
    @(
        [pscustomobject]@{ schemaVersion = 1; kind = 'ready'; detail = 'early' },
        [pscustomobject]@{ schemaVersion = 1; kind = 'error'; detail = 'must win' },
        [pscustomobject]@{ schemaVersion = 1; kind = 'ready'; detail = 'late' }
    ) | ForEach-Object { $_ | ConvertTo-Json -Compress } |
        Set-Content -LiteralPath $markerPath -Encoding UTF8
    Assert-Equal 'error' (Get-CliplineDriverMarker -Path $markerPath).kind `
        'a later ready marker must never mask any completed error marker'

    $lifecycle = [pscustomobject]@{
        mode = 'tray'; windowActive = $false; quitting = $true
        openAccepted = 100; closeAccepted = 100
        windowCreated = 100; windowDropped = 100; maxLiveWindows = 1
        desktopAttached = 100; desktopDetached = 100
        playbackStarted = 100; playbackStopped = 100
        videoHostCreated = 100; videoHostDropped = 100
        modelSetsCreated = 100; modelSetsDropped = 100
        liveDesktopAttachments = 0; livePlaybackSessions = 0
        liveVideoHosts = 0; liveModelSets = 0; presentationResourcesLive = 0
        trayReady = $false; desktopConsumerAlive = $false
        hotkeyServiceAlive = $false; activationServiceAlive = $false
        quitAccepted = 1
    }
    $telemetry = [pscustomobject]@{
        schemaVersion = 1
        lifecycle = $lifecycle
        presentation = [pscustomobject]@{ path = 'd3d11-child-window' }
    }
    Assert-SlintLifecycleTelemetry -Telemetry $telemetry -ScenarioName 'reveal-close-100'
    $lifecycle.windowDropped = 99
    $unbalancedFailed = $false
    try {
        Assert-SlintLifecycleTelemetry -Telemetry $telemetry -ScenarioName 'reveal-close-100'
    } catch { $unbalancedFailed = $true }
    Assert-True $unbalancedFailed 'unbalanced Slint lifecycle telemetry must fail closed'
} finally {
    Remove-Item -LiteralPath $testRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host 'frontend baseline helper self-tests passed'
