<#
.SYNOPSIS
  Compare two run.json files from benchmark-windows.ps1.
.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\compare-windows-benchmarks.ps1 -Before .\before\run.json -After .\after\run.json
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$Before,
    [Parameter(Mandatory=$true)][string]$After,
    [string]$OutputDirectory
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (-not (Test-Path -LiteralPath $Before -PathType Leaf)) { throw "Not found: $Before" }
if (-not (Test-Path -LiteralPath $After -PathType Leaf)) { throw "Not found: $After" }
$old = Get-Content -LiteralPath $Before -Raw | ConvertFrom-Json
$new = Get-Content -LiteralPath $After -Raw | ConvertFrom-Json
if ($old.schemaVersion -ne 1 -or $new.schemaVersion -ne 1) { throw 'Unsupported JSON benchmark schema; expected version 1.' }
if ($old.rootName -ne $new.rootName) { Write-Warning 'Executable names differ between runs; verify comparability.' }
if ($old.logicalProcessors -ne $new.logicalProcessors) { Write-Warning 'Logical CPU count differs; CPU percentages are not directly comparable.' }
if ($old.settings.sampleSeconds -ne $new.settings.sampleSeconds) { Write-Warning 'Sample intervals differ.' }
if ($old.settings.idleMinutes -ne $new.settings.idleMinutes) { Write-Warning 'Idle durations differ.' }
$names = @('startup','main_visible','main_hidden','tray_open','tray_closed','idle','account_switch','usage_stats')
$rows = foreach ($name in $names) {
    $b = @($old.stages | Where-Object { $_.stage -eq $name })
    $a = @($new.stages | Where-Object { $_.stage -eq $name })
    if ($b.Count -ne 1 -or $a.Count -ne 1) { continue }
    $baseline = $b[0]; $current = $a[0]
    [pscustomobject]@{
        stage = $name
        beforePrivateMiB = $baseline.medianPrivateMiB
        afterPrivateMiB = $current.medianPrivateMiB
        deltaPrivateMiB = [math]::Round($current.medianPrivateMiB - $baseline.medianPrivateMiB,2)
        reductionPercent = if ($baseline.medianPrivateMiB -gt 0) {
            [math]::Round(100 * ($baseline.medianPrivateMiB - $current.medianPrivateMiB) / $baseline.medianPrivateMiB,2)
        } else { $null }
        beforePeakMiB = $baseline.peakPrivateMiB
        afterPeakMiB = $current.peakPrivateMiB
        beforeWebView2MiB = $baseline.medianWebView2PrivateMiB
        afterWebView2MiB = $current.medianWebView2PrivateMiB
        deltaWebView2MiB = [math]::Round($current.medianWebView2PrivateMiB - $baseline.medianWebView2PrivateMiB,2)
        beforeWorkingSetSumMiB = $baseline.medianWorkingSetSumMiB
        afterWorkingSetSumMiB = $current.medianWorkingSetSumMiB
        beforeCpuPercent = $baseline.medianCpuPercent
        afterCpuPercent = $current.medianCpuPercent
        beforeStageGrowthMiB = $baseline.growthMiB
        afterStageGrowthMiB = $current.growthMiB
        beforeSamplesWithMissingMetrics = $baseline.samplesWithMissingMetrics
        afterSamplesWithMissingMetrics = $current.samplesWithMissingMetrics
    }
}
$rows = @($rows)
if ($rows.Count -eq 0) { throw 'No matching measured stages found between the two runs.' }
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Split-Path -Path (Resolve-Path -LiteralPath $After) -Parent
}
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
$csv = Join-Path $OutputDirectory 'before-after.csv'
$json = Join-Path $OutputDirectory 'before-after.json'
$md = Join-Path $OutputDirectory 'before-after.md'
$rows | Export-Csv -Path $csv -Encoding UTF8 -NoTypeInformation
$comparison = [pscustomobject]@{
    schemaVersion = 1
    generatedUtc = (Get-Date).ToUniversalTime().ToString('o')
    beforeLabel = $old.label
    afterLabel = $new.label
    beforeStartedUtc = $old.startedUtc
    afterStartedUtc = $new.startedUtc
    beforeOS = $old.osVersion
    afterOS = $new.osVersion
    warnings = @(
        'Relative changes compare process-tree Private Bytes, not system RAM consumed.',
        'Working Set sums can double-count pages shared between processes.',
        'A matching WebView2 user-data directory outside the proven tree is excluded.',
        'Sampled memory growth alone does not prove a leak; repeat identical scenarios.'
    )
    stages = $rows
}
$comparison | ConvertTo-Json -Depth 8 | Set-Content -Path $json -Encoding UTF8

$lines = New-Object 'System.Collections.Generic.List[string]'
[void]$lines.Add('# Windows memory benchmark: before vs after')
[void]$lines.Add('')
[void]$lines.Add("**Before:** $($old.label) ($($old.startedUtc)); **After:** $($new.label) ($($new.startedUtc))")
[void]$lines.Add('')
[void]$lines.Add('| Stage | Before private MiB | After private MiB | Difference MiB | Reduction % | WebView2 difference MiB | CPU % before/after |')
[void]$lines.Add('|---|---:|---:|---:|---:|---:|---:|')
foreach ($r in $rows) {
    $bCpu = if ($null -eq $r.beforeCpuPercent) { 'n/a' } else { "$($r.beforeCpuPercent)" }
    $aCpu = if ($null -eq $r.afterCpuPercent) { 'n/a' } else { "$($r.afterCpuPercent)" }
    [void]$lines.Add(('| {0} | {1} | {2} | {3} | {4} | {5} | {6}/{7} |' -f
        $r.stage,$r.beforePrivateMiB,$r.afterPrivateMiB,$r.deltaPrivateMiB,$r.reductionPercent,
        $r.deltaWebView2MiB,$bCpu,$aCpu))
}
[void]$lines.Add('')
[void]$lines.Add('**Interpretation:** negative delta means less committed private memory after; positive reduction % means improvement.')
[void]$lines.Add('')
[void]$lines.Add('For per-process attribution, inspect processes.csv sorted by privateBytes for the matching stage and timestamp. The full process tree (unique PID + creation time) is in run.json.')
[void]$lines.Add('')
[void]$lines.Add('**Limits:** Private Bytes are NOT physical RAM. Working Set sums include shared physical pages; do not interpret as unique RAM. WebView2 outside verified ancestry is excluded even when its user-data directory matches. Missing/inaccessible processes can undercount. Repeat runs with identical account data, app build mode, Windows version, warm/cold cache and schedule.')
$lines | Set-Content -Path $md -Encoding UTF8
Write-Host "Comparison: $md" -ForegroundColor Green
Write-Host "Comparison JSON: $json" -ForegroundColor Green
Write-Host "Comparison CSV: $csv" -ForegroundColor Green
