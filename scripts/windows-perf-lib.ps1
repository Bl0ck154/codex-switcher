# Shared, read-only Windows process metrics for Codex Switcher benchmarks.
# Compatible with Windows PowerShell 5.1 and PowerShell 7+.
Set-StrictMode -Version Latest

function Get-PerfIdentity {
    param([Parameter(Mandatory)]$Node)
    $created = ([datetime]$Node.CreationDate).ToUniversalTime()
    '{0}|{1}' -f [int]$Node.ProcessId, $created.Ticks
}

function Select-PerfOwnedProcesses {
    param(
        [Parameter(Mandatory)][object[]]$Nodes,
        [Parameter(Mandatory)][int]$RootProcessId,
        [Parameter(Mandatory)][datetime]$RootCreationDate,
        [hashtable]$PreviouslyOwned = @{}
    )
    $byPid = @{}
    foreach ($node in $Nodes) { $byPid[[int]$node.ProcessId] = $node }
    $root = $byPid[$RootProcessId]
    if ($null -eq $root -or
        ([datetime]$root.CreationDate).ToUniversalTime().Ticks -ne $RootCreationDate.ToUniversalTime().Ticks) {
        throw "The original application process exited or its PID was reused ($RootProcessId)."
    }

    $owned = @{}
    $owned[(Get-PerfIdentity $root)] = $root
    $changed = $true
    while ($changed) {
        $changed = $false
        foreach ($node in $Nodes) {
            $key = Get-PerfIdentity $node
            if ($owned.ContainsKey($key)) { continue }
            $parent = $byPid[[int]$node.ParentProcessId]
            $isChild = $false
            if ($null -ne $parent -and $owned.ContainsKey((Get-PerfIdentity $parent))) {
                $isChild = ([datetime]$node.CreationDate) -ge ([datetime]$parent.CreationDate)
            }
            # A previously witnessed child can survive its parent. Never reassign by PID alone.
            if ($isChild -or $PreviouslyOwned.ContainsKey($key)) {
                $owned[$key] = $node
                $changed = $true
            }
        }
    }
    @($owned.Values)
}

function Get-PerfUserDataDir {
    param([AllowNull()][string]$CommandLine)
    if ([string]::IsNullOrWhiteSpace($CommandLine)) { return $null }
    if ($CommandLine -match '--user-data-dir=(?:"([^"]+)"|(\S+))') {
        $value = if ($Matches.ContainsKey('1') -and $Matches['1']) { $Matches['1'] } else { $Matches['2'].Trim('"') }
        return $value.TrimEnd('\', '/').ToLowerInvariant()
    }
    return $null
}

function Get-PerfRole {
    param([string]$Name, [int]$ProcessId, [int]$RootPid)
    if ($ProcessId -eq $RootPid) { return 'main' }
    if ($Name -match '^msedgewebview2(\.exe)?$') { return 'webview2' }
    return 'helper'
}

function Get-PerfPercentile {
    param([object[]]$Values, [ValidateRange(0,1)][double]$Fraction)
    $sorted = @($Values | Where-Object { $null -ne $_ } | ForEach-Object { [double]$_ } | Sort-Object)
    if ($sorted.Count -eq 0) { return $null }
    $position = ($sorted.Count - 1) * $Fraction
    $lo = [int][math]::Floor($position)
    $hi = [int][math]::Ceiling($position)
    [double]($sorted[$lo] + ($sorted[$hi] - $sorted[$lo]) * ($position - $lo))
}

function Get-PerfStageSummary {
    param([Parameter(Mandatory)][object[]]$Samples, [Parameter(Mandatory)][string]$Stage)
    $items = @($Samples | Where-Object { $_.stage -eq $Stage })
    if ($items.Count -eq 0) { return $null }
    $private = @($items | ForEach-Object { $_.privateBytes / 1MB })
    $working = @($items | ForEach-Object { $_.workingSetBytes / 1MB })
    $webview = @($items | ForEach-Object { $_.webview2PrivateBytes / 1MB })
    $main = @($items | ForEach-Object { $_.mainPrivateBytes / 1MB })
    $cpu = @($items | Where-Object { $null -ne $_.cpuPercent } | ForEach-Object { $_.cpuPercent })
    $edge = [math]::Max(1, [int][math]::Ceiling($items.Count * 0.2))
    $startMedian = Get-PerfPercentile -Values @($private | Select-Object -First $edge) -Fraction 0.5
    $endMedian = Get-PerfPercentile -Values @($private | Select-Object -Last $edge) -Fraction 0.5
    $growth = [math]::Round(($endMedian - $startMedian), 2)
    $possiblyGrowing = $Stage -eq 'idle' -and $items.Count -ge 20 -and
        $growth -ge 32 -and $growth -ge (0.1 * [math]::Max(1, $startMedian))
    $last = $items[-1]
    $top = @($last.processes | Sort-Object -Property privateBytes -Descending | Select-Object -First 3 |
        ForEach-Object { [pscustomobject]@{ pid=$_.pid; name=$_.name; role=$_.role; privateMiB=[math]::Round($_.privateBytes / 1MB,2) } })
    [pscustomobject]@{
        stage = $Stage
        sampleCount = $items.Count
        medianPrivateMiB = [math]::Round((Get-PerfPercentile $private 0.5), 2)
        peakPrivateMiB = [math]::Round((Get-PerfPercentile $private 1), 2)
        medianWorkingSetSumMiB = [math]::Round((Get-PerfPercentile $working 0.5), 2)
        peakWorkingSetSumMiB = [math]::Round((Get-PerfPercentile $working 1), 2)
        medianWebView2PrivateMiB = [math]::Round((Get-PerfPercentile $webview 0.5), 2)
        medianMainPrivateMiB = [math]::Round((Get-PerfPercentile $main 0.5), 2)
        medianCpuPercent = if ($cpu.Count) { [math]::Round((Get-PerfPercentile $cpu 0.5),2) } else { $null }
        peakCpuPercent = if ($cpu.Count) { [math]::Round((Get-PerfPercentile $cpu 1),2) } else { $null }
        startPrivateMiB = [math]::Round($startMedian,2)
        endPrivateMiB = [math]::Round($endMedian,2)
        growthMiB = $growth
        investigateIdleGrowth = [bool]$possiblyGrowing
        peakAmbiguousWebView2Candidates = [int](Get-PerfPercentile @($items | ForEach-Object { $_.ambiguousWebView2Candidates }) 1)
        samplesWithMissingMetrics = @($items | Where-Object { $_.missingMetrics -gt 0 }).Count
        topProcessesLastSample = $top
    }
}

function Get-PerfSnapshot {
    param(
        [Parameter(Mandatory)][int]$RootPid,
        [Parameter(Mandatory)][datetime]$RootCreationDate,
        [Parameter(Mandatory)][hashtable]$Seen,
        [Parameter(Mandatory)][hashtable]$CpuHistory,
        [Parameter(Mandatory)][ref]$PreviousTime,
        [Parameter(Mandatory)][string]$Stage,
        [int]$Iteration = 0
    )
    $nodes = @(Get-CimInstance -ClassName Win32_Process -Property ProcessId,ParentProcessId,Name,CreationDate,CommandLine -ErrorAction Stop)
    $owned = @(Select-PerfOwnedProcesses -Nodes $nodes -RootProcessId $RootPid -RootCreationDate $RootCreationDate -PreviouslyOwned $Seen)
    $ownedKeys = @{}
    $ownedDirs = @{}
    foreach ($node in $owned) {
        $key = Get-PerfIdentity $node
        $ownedKeys[$key] = $true
        $Seen[$key] = $true
        if ((Get-PerfRole $node.Name $node.ProcessId $RootPid) -eq 'webview2') {
            $dir = Get-PerfUserDataDir $node.CommandLine
            if ($null -ne $dir) { $ownedDirs[$dir] = $true }
        }
    }
    # WebView2 with the same user-data folder but not demonstrably in our tree
    # is recorded only as an ambiguous candidate, NEVER added to memory totals.
    $externalCandidates = 0
    foreach ($node in $nodes) {
        if ($node.Name -notmatch '^msedgewebview2(\.exe)?$') { continue }
        if ($ownedKeys.ContainsKey((Get-PerfIdentity $node))) { continue }
        $dir = Get-PerfUserDataDir $node.CommandLine
        if ($null -ne $dir -and $ownedDirs.ContainsKey($dir)) { $externalCandidates++ }
    }

    $live = @{}
    $pids = @($owned | ForEach-Object { [int]$_.ProcessId })
    if ($pids.Count) {
        foreach ($p in @(Get-Process -Id $pids -ErrorAction SilentlyContinue)) { $live[[int]$p.Id] = $p }
    }
    $now = (Get-Date).ToUniversalTime()
    $elapsed = if ($null -ne $PreviousTime.Value) { ($now - [datetime]$PreviousTime.Value).TotalSeconds } else { 0 }
    $cores = [math]::Max(1, [environment]::ProcessorCount)
    $out = @()
    $missing = 0
    $privateTotal = [long]0; $workingTotal = [long]0
    $mainTotal = [long]0; $webviewTotal = [long]0; $helperTotal = [long]0
    $cpuTotal = 0.0; $cpuKnown = $false
    foreach ($node in $owned) {
        $id = [int]$node.ProcessId
        if (-not $live.ContainsKey($id)) { $missing++; continue }
        $proc = $live[$id]
        $role = Get-PerfRole $node.Name $id $RootPid
        $key = Get-PerfIdentity $node
        try {
            $private = [long]$proc.PrivateMemorySize64
            $working = [long]$proc.WorkingSet64
            $totalCpu = [double]$proc.CPU
            $handles = [int]$proc.HandleCount
        } catch { $missing++; continue }
        $pct = $null
        if ($elapsed -gt 0 -and $CpuHistory.ContainsKey($key)) {
            $deltaCpu = [math]::Max(0, $totalCpu - [double]$CpuHistory[$key])
            $pct = [math]::Round(100.0 * $deltaCpu / ($elapsed * $cores), 3)
            $cpuTotal += $pct
            $cpuKnown = $true
        }
        $CpuHistory[$key] = $totalCpu
        $privateTotal += $private; $workingTotal += $working
        switch ($role) {
            'main' { $mainTotal += $private }
            'webview2' { $webviewTotal += $private }
            default { $helperTotal += $private }
        }
        $out += [pscustomobject]@{
            pid = $id
            parentPid = [int]$node.ParentProcessId
            startUtc = ([datetime]$node.CreationDate).ToUniversalTime().ToString('o')
            name = [string]$node.Name
            role = $role
            privateBytes = $private
            workingSetBytes = $working
            cpuPercent = $pct
            handleCount = $handles
        }
    }
    $PreviousTime.Value = $now
    [pscustomobject]@{
        stage = $Stage
        iteration = $Iteration
        timestampUtc = $now.ToString('o')
        processCount = $out.Count
        privateBytes = $privateTotal
        workingSetBytes = $workingTotal
        mainPrivateBytes = $mainTotal
        webview2PrivateBytes = $webviewTotal
        helperPrivateBytes = $helperTotal
        cpuPercent = if ($cpuKnown) { [math]::Round($cpuTotal,3) } else { $null }
        ambiguousWebView2Candidates = $externalCandidates
        missingMetrics = $missing
        processes = @($out)
    }
}
