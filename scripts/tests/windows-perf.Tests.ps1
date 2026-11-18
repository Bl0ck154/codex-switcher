# Dependency-free Windows PowerShell 5.1 / PowerShell 7 regression tests.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$scriptDir = Join-Path $repo 'scripts'
. (Join-Path $scriptDir 'windows-perf-lib.ps1')

function Assert-True { param([bool]$Condition,[string]$Message) if (-not $Condition) { throw $Message } }
function Assert-Equal { param($Actual,$Expected,[string]$Message) if ($Actual -ne $Expected) { throw ("{0} (actual={1} expected={2})" -f $Message,$Actual,$Expected) } }
function New-Node {
    param([int]$Id,[int]$Parent,[string]$Name,[datetime]$Created,[string]$CommandLine='')
    [pscustomobject]@{ ProcessId=$Id;ParentProcessId=$Parent;Name=$Name;CreationDate=$Created;CommandLine=$CommandLine }
}

# Parse all scripts even when no Windows GUI / WebView2 runtime is present.
foreach ($path in @('windows-perf-lib.ps1','benchmark-windows.ps1','compare-windows-benchmarks.ps1')) {
    $tokens = $null; $parseErrors = $null
    [void][System.Management.Automation.Language.Parser]::ParseFile((Join-Path $scriptDir $path),[ref]$tokens,[ref]$parseErrors)
    Assert-Equal $parseErrors.Count 0 "PowerShell syntax: $path"
}
$time = [datetime]'2026-01-01T12:00:00Z'
$root = New-Node 100 4 'Codex Switcher.exe' $time
$browser = New-Node 200 100 'msedgewebview2.exe' $time.AddSeconds(1) '--user-data-dir="C:\Temp\Codex Switcher\EBWebView"'
$renderer = New-Node 300 200 'msedgewebview2.exe' $time.AddSeconds(2)
$foreign = New-Node 400 900 'msedgewebview2.exe' $time.AddSeconds(3)
$oldChild = New-Node 500 200 'other.exe' $time.AddSeconds(-2)
$nodes = @($root,$browser,$renderer,$foreign,$oldChild)

$tree = @(Select-PerfOwnedProcesses -Nodes $nodes -RootProcessId 100 -RootCreationDate $time)
Assert-Equal $tree.Count 3 'Exactly three unique descendant processes'
$ids = @($tree | ForEach-Object { $_.ProcessId } | Sort-Object)
Assert-Equal ($ids -join ',') '100,200,300' 'Exclude unrelated WebView2 and PID parent born later than child'
Assert-Equal (@($tree | Select-Object -ExpandProperty ProcessId -Unique).Count) 3 'No duplicated PIDs'
Assert-True ((Get-PerfIdentity $browser) -ne (Get-PerfIdentity (New-Node 200 100 'msedgewebview2.exe' $time.AddHours(1)))) 'PID reuse must produce different identity'

$seen = @{}
foreach ($p in $tree) { $seen[(Get-PerfIdentity $p)] = $true }
$survivor = @(Select-PerfOwnedProcesses -Nodes @($root,$renderer,$foreign) -RootProcessId 100 -RootCreationDate $time -PreviouslyOwned $seen)
Assert-Equal $survivor.Count 2 'Already witnessed orphan should remain attributed'
$failed = $false
try { [void](Select-PerfOwnedProcesses -Nodes @((New-Node 100 4 'Codex Switcher.exe' $time.AddDays(1))) -RootProcessId 100 -RootCreationDate $time) } catch { $failed = $true }
Assert-True $failed 'PID reuse of target must stop the benchmark'

Assert-Equal (Get-PerfRole 'Codex Switcher.exe' 100 100) 'main' 'Root classification'
Assert-Equal (Get-PerfRole 'msedgewebview2.exe' 200 100) 'webview2' 'WebView2 classification'
Assert-Equal (Get-PerfRole 'helper.exe' 600 100) 'helper' 'Helper classification'
Assert-Equal (Get-PerfUserDataDir $browser.CommandLine) 'c:\temp\codex switcher\ebwebview' 'Quoted WebView2 user-data-dir parsing'
Assert-Equal (Get-PerfUserDataDir '--user-data-dir=C:\foo\bar') 'c:\foo\bar' 'Unquoted WebView2 user-data-dir parsing'
Assert-True ($null -eq (Get-PerfUserDataDir '--no-sandbox')) 'Missing user-data-dir is null'
Assert-Equal (Get-PerfPercentile -Values @(1,3,5,7) -Fraction 0.5) 4 'Median interpolation'
Assert-True ($null -eq (Get-PerfPercentile -Values @() -Fraction 0.5)) 'Empty percentile'

$samples = @()
for ($i=0; $i -lt 25; $i++) {
    $mb = (100 + 3*$i) * 1MB
    $samples += [pscustomobject]@{
        stage='idle'; iteration=0; privateBytes=$mb; workingSetBytes=$mb
        webview2PrivateBytes=([long]($mb*0.7)); mainPrivateBytes=([long]($mb*0.3))
        cpuPercent=0.2; ambiguousWebView2Candidates=1; missingMetrics=0
        processes=@([pscustomobject]@{pid=100;name='Codex Switcher.exe';role='main';privateBytes=$mb})
    }
}
$summary = Get-PerfStageSummary -Samples $samples -Stage idle
Assert-True $summary.investigateIdleGrowth '25 samples of sustained 72 MiB growth should flag for investigation'
Assert-Equal $summary.sampleCount 25 'Stage sample count'
Assert-Equal $summary.peakAmbiguousWebView2Candidates 1 'Ambiguous candidate is NOT attributed'
Assert-True ($null -eq (Get-PerfStageSummary -Samples $samples -Stage startup)) 'Absent phase returns null'
Assert-True ($summary.medianWebView2PrivateMiB -lt $summary.medianPrivateMiB) 'Role subtotal less than total'

# Compare report creation without touching live processes.
$temp = Join-Path ([IO.Path]::GetTempPath()) ('cs-perf-test-' + [guid]::NewGuid().ToString('N'))
[void](New-Item -ItemType Directory -Path $temp)
try {
    $stage = [pscustomobject]@{
        stage='idle'; medianPrivateMiB=100; peakPrivateMiB=120
        medianWebView2PrivateMiB=70; medianWorkingSetSumMiB=90
        medianCpuPercent=1; growthMiB=5; samplesWithMissingMetrics=0
    }
    $old = [pscustomobject]@{
        schemaVersion=1; label='before'; startedUtc='2026-01-01T00:00:00Z'
        rootName='Codex Switcher.exe'; logicalProcessors=8; osVersion='Windows'
        settings=[pscustomobject]@{sampleSeconds=3;idleMinutes=10}; stages=@($stage)
    }
    $newStage = $stage | Select-Object *
    $newStage.medianPrivateMiB = 80
    $newStage.medianWebView2PrivateMiB = 50
    $new = [pscustomobject]@{
        schemaVersion=1; label='after'; startedUtc='2026-01-02T00:00:00Z'
        rootName='Codex Switcher.exe'; logicalProcessors=8; osVersion='Windows'
        settings=[pscustomobject]@{sampleSeconds=3;idleMinutes=10}; stages=@($newStage)
    }
    $beforeFile = Join-Path $temp 'before.json'
    $afterFile = Join-Path $temp 'after.json'
    $old | ConvertTo-Json -Depth 8 | Set-Content -Path $beforeFile
    $new | ConvertTo-Json -Depth 8 | Set-Content -Path $afterFile
    & (Join-Path $scriptDir 'compare-windows-benchmarks.ps1') -Before $beforeFile -After $afterFile -OutputDirectory $temp | Out-Null
    $comparison = Get-Content -Path (Join-Path $temp 'before-after.json') -Raw | ConvertFrom-Json
    Assert-Equal $comparison.stages[0].deltaPrivateMiB -20 'Before/after delta'
    Assert-Equal $comparison.stages[0].reductionPercent 20 'Before/after percent'
    Assert-True (Test-Path (Join-Path $temp 'before-after.csv')) 'CSV comparison exported'
    Assert-True (Test-Path (Join-Path $temp 'before-after.md')) 'Markdown comparison exported'
} finally {
    Remove-Item -LiteralPath $temp -Recurse -Force -ErrorAction SilentlyContinue
}
Write-Host 'Windows benchmark script tests: PASSED' -ForegroundColor Green
