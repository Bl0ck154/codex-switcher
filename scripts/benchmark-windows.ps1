<#
Interactive, read-only benchmark: launch with -Executable for cold startup,
or attach with -ProcessId (startup unavailable). Windows PowerShell 5.1+.
#>
[CmdletBinding(DefaultParameterSetName='Launch')]
param(
    [Parameter(Mandatory=$true,ParameterSetName='Launch')][string]$Executable,
    [Parameter(Mandatory=$true,ParameterSetName='Attach')][ValidateRange(1,2147483647)][int]$ProcessId,
    [string]$AppArguments = '',
    [ValidateRange(10,30)][int]$IdleMinutes = 10,
    [ValidateRange(1,30)][int]$SampleSeconds = 3,
    [ValidateRange(1,20)][int]$TrayCycles = 3,
    [ValidateRange(1,100)][int]$SwitchCount = 10,
    [ValidateRange(1,100)][int]$StatsCount = 5,
    [ValidateNotNullOrEmpty()][string]$Label = 'before',
    [string]$OutputDirectory = (Join-Path $env:TEMP 'CodexSwitcherBenchmarks'),
    [string]$CompareTo
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($env:OS -ne 'Windows_NT') { throw 'This benchmark must run on Windows.' }
. (Join-Path $PSScriptRoot 'windows-perf-lib.ps1')

function Confirm-Action { param([string]$Message) [void](Read-Host $Message) }
function Invoke-Phase {
    param([string]$Name, [int]$Seconds, [int]$Iteration = 0)
    Write-Host ("Capturing {0} [{1}] for {2}s..." -f $Name,$Iteration,$Seconds) -ForegroundColor Cyan
    $cpuHistory.Clear()
    $previousTime.Value = $null
    $until = (Get-Date).AddSeconds($Seconds)
    do {
        $snapshot = Get-PerfSnapshot -RootPid $rootPid -RootCreationDate $rootCreated -Seen $seen -CpuHistory $cpuHistory -PreviousTime $previousTime -Stage $Name -Iteration $Iteration
        [void]$samples.Add($snapshot)
        $left = ($until - (Get-Date)).TotalSeconds
        if ($left -gt 0) { Start-Sleep -Milliseconds ([int](1000 * [math]::Min($SampleSeconds,$left))) }
    } while ((Get-Date) -lt $until)
}
function Format-MiB {
    param($Value)
    if ($null -eq $Value) { return 'n/a' }
    return ('{0:N2}' -f [double]$Value)
}

if ($PSCmdlet.ParameterSetName -eq 'Launch') {
    if (-not (Test-Path -LiteralPath $Executable -PathType Leaf)) { throw "Executable does not exist: $Executable" }
    Write-Host 'Close all other Codex Switcher instances first (avoid single-instance handoff).' -ForegroundColor Yellow
    Confirm-Action 'Press ENTER to launch and sample cold startup'
    if ([string]::IsNullOrWhiteSpace($AppArguments)) {
        $launched = Start-Process -FilePath $Executable -PassThru
    } else {
        $launched = Start-Process -FilePath $Executable -ArgumentList $AppArguments -PassThru
    }
    $rootPid = [int]$launched.Id
} else {
    $rootPid = $ProcessId
    Write-Warning 'Attached to an existing process: cold startup cannot be measured.'
}
$rootNode = $null
for ($attempt=0; $attempt -lt 40; $attempt++) {
    $rootNode = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $rootPid" -ErrorAction SilentlyContinue
    if ($null -ne $rootNode) { break }
    Start-Sleep -Milliseconds 250
}
if ($null -eq $rootNode) { throw "PID $rootPid was not found. Possibly a handoff to an existing instance." }
$rootCreated = ([datetime]$rootNode.CreationDate).ToUniversalTime()
$startedAt = (Get-Date).ToUniversalTime()
$seen = @{}
$cpuHistory = @{}
$previousTime = [ref]$null
$samples = New-Object 'System.Collections.Generic.List[object]'
Write-Host ("Target: {0} PID={1}" -f $rootNode.Name,$rootPid)
Write-Host 'Unique proven descendants only. External matched WebView2 candidates are excluded.' -ForegroundColor Yellow
Write-Host 'UI actions are manual. The script never switches accounts or accesses credentials.' -ForegroundColor Yellow

if ($PSCmdlet.ParameterSetName -eq 'Launch') { Invoke-Phase -Name 'startup' -Seconds 20 }
Confirm-Action 'Show MAIN WINDOW, wait for it to settle, then press ENTER'
Invoke-Phase -Name 'main_visible' -Seconds 25
Confirm-Action 'HIDE MAIN WINDOW (close-to-tray), then press ENTER'
Invoke-Phase -Name 'main_hidden' -Seconds 25
for ($cycle=1; $cycle -le $TrayCycles; $cycle++) {
    Confirm-Action ("Tray cycle {0}/{1}: ENTER, then IMMEDIATELY OPEN the LEFT-CLICK popup" -f $cycle,$TrayCycles)
    Invoke-Phase -Name 'tray_open' -Seconds 9 -Iteration $cycle
    Confirm-Action ("Tray cycle {0}/{1}: ENTER, then IMMEDIATELY CLOSE the popup" -f $cycle,$TrayCycles)
    Invoke-Phase -Name 'tray_closed' -Seconds 9 -Iteration $cycle
}
Confirm-Action ("HIDE all windows/popup and avoid interaction for {0} minutes. ENTER to start" -f $IdleMinutes)
Invoke-Phase -Name 'idle' -Seconds ($IdleMinutes * 60)
Confirm-Action 'OPEN main window; prepare test accounts. You will switch accounts during each capture'
for ($n=1; $n -le $SwitchCount; $n++) {
    Confirm-Action ("Switch {0}/{1}: ENTER, then immediately switch account" -f $n,$SwitchCount)
    Invoke-Phase -Name 'account_switch' -Seconds 10 -Iteration $n
}
for ($n=1; $n -le $StatsCount; $n++) {
    Confirm-Action ("Stats {0}/{1}: ENTER, then immediately open/close Usage Stats" -f $n,$StatsCount)
    Invoke-Phase -Name 'usage_stats' -Seconds 10 -Iteration $n
}

$finishedAt = (Get-Date).ToUniversalTime()
$stageOrder = @('startup','main_visible','main_hidden','tray_open','tray_closed','idle','account_switch','usage_stats')
$stageSummary = @($stageOrder | ForEach-Object {
    $summary = Get-PerfStageSummary -Samples $samples.ToArray() -Stage $_
    if ($null -ne $summary) { $summary }
})
$captureMode = if ($PSCmdlet.ParameterSetName -eq 'Launch') { 'launched' } else { 'attached' }
$payload = [pscustomobject]@{
    schemaVersion = 1
    label = $Label
    startedUtc = $startedAt.ToString('o')
    finishedUtc = $finishedAt.ToString('o')
    osVersion = [environment]::OSVersion.VersionString
    logicalProcessors = [environment]::ProcessorCount
    rootPid = $rootPid
    rootStartedUtc = $rootCreated.ToString('o')
    rootName = [string]$rootNode.Name
    captureMode = $captureMode
    settings = [pscustomobject]@{
        idleMinutes = $IdleMinutes; sampleSeconds = $SampleSeconds
        trayCycles = $TrayCycles; switchCount = $SwitchCount; statsCount = $StatsCount
    }
    cautions = @(
        'Private Bytes: committed private virtual memory, not physical RAM.',
        'Working Set sum may count shared resident pages across different PIDs more than once.',
        'Only PID+creation-time verified descendants are counted; external WebView2 candidates excluded.',
        'CPU % normalized to all logical processors; first sample of each phase lacks CPU delta.',
        'Missing process metrics UNDERCOUNT totals; interactions are manual.'
    )
    stages = $stageSummary
    samples = $samples.ToArray()
}
$tag = ($Label -replace '[^a-zA-Z0-9_-]','_')
$folder = Join-Path $OutputDirectory ((Get-Date).ToString('yyyyMMdd-HHmmss') + '-' + $tag)
New-Item -ItemType Directory -Force -Path $folder | Out-Null
$jsonPath = Join-Path $folder 'run.json'
$payload | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $jsonPath -Encoding UTF8
$samples | Select-Object stage,iteration,timestampUtc,processCount,privateBytes,workingSetBytes,mainPrivateBytes,webview2PrivateBytes,helperPrivateBytes,cpuPercent,ambiguousWebView2Candidates,missingMetrics |
    Export-Csv -Path (Join-Path $folder 'samples.csv') -NoTypeInformation -Encoding UTF8
$rows = foreach ($sample in $samples) {
    foreach ($process in $sample.processes) {
        [pscustomobject]@{
            stage=$sample.stage; iteration=$sample.iteration; timestampUtc=$sample.timestampUtc
            pid=$process.pid; parentPid=$process.parentPid; startUtc=$process.startUtc
            name=$process.name; role=$process.role; privateBytes=$process.privateBytes
            workingSetBytes=$process.workingSetBytes; cpuPercent=$process.cpuPercent
            handleCount=$process.handleCount
        }
    }
}
@($rows) | Export-Csv -Path (Join-Path $folder 'processes.csv') -NoTypeInformation -Encoding UTF8
$lines = New-Object 'System.Collections.Generic.List[string]'
[void]$lines.Add("# Codex Switcher Windows benchmark: $Label")
[void]$lines.Add('')
[void]$lines.Add("Target: $($rootNode.Name) PID $rootPid; $($stageSummary.Count) measured stages; $($samples.Count) samples.")
[void]$lines.Add('')
[void]$lines.Add('| Stage | Median private MiB | Peak private MiB | Median WebView2 MiB | Median WS-sum MiB | Median CPU % | Stage growth MiB |')
[void]$lines.Add('|---|---:|---:|---:|---:|---:|---:|')
foreach ($stage in $stageSummary) {
    [void]$lines.Add(('| {0} | {1} | {2} | {3} | {4} | {5} | {6} |' -f
        $stage.stage,(Format-MiB $stage.medianPrivateMiB),(Format-MiB $stage.peakPrivateMiB),
        (Format-MiB $stage.medianWebView2PrivateMiB),(Format-MiB $stage.medianWorkingSetSumMiB),
        (Format-MiB $stage.medianCpuPercent),(Format-MiB $stage.growthMiB)))
}
[void]$lines.Add('')
$idle = @($stageSummary | Where-Object { $_.stage -eq 'idle' })
if ($idle.Count -eq 1 -and $idle[0].investigateIdleGrowth) {
    [void]$lines.Add("**Investigate:** idle private bytes grew $($idle[0].growthMiB) MiB. Not proof of a leak.")
}
[void]$lines.Add('Largest processes at the end of each stage:')
foreach ($stage in $stageSummary) {
    $topText = @($stage.topProcessesLastSample | ForEach-Object { "$($_.name) [$($_.role), PID $($_.pid)]: $($_.privateMiB) MiB" }) -join '; '
    [void]$lines.Add("- **$($stage.stage):** $topText")
}
[void]$lines.Add('')
[void]$lines.Add('Private Bytes are not physical RAM; WS sums can include shared pages; external WebView2 candidates excluded; missing metrics undercount. Interactions manual.')
$lines | Set-Content -Path (Join-Path $folder 'report.md') -Encoding UTF8
Write-Host "JSON: $jsonPath" -ForegroundColor Green
Write-Host "CSV and report: $folder" -ForegroundColor Green
if ($CompareTo) {
    if (-not (Test-Path -LiteralPath $CompareTo -PathType Leaf)) { throw "Baseline not found: $CompareTo" }
    & (Join-Path $PSScriptRoot 'compare-windows-benchmarks.ps1') -Before $CompareTo -After $jsonPath -OutputDirectory $folder
}
