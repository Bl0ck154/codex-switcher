# Windows performance benchmark (Tauri / WebView2)

This is a read-only tool for Windows PowerShell 5.1+ and PowerShell 7+. It measures the Codex Switcher executable and proven descendant processes, including WebView2 browser / renderer / GPU / utility processes. It never changes Codex Switcher settings or credentials, and UI interactions are initiated by the user.

## Important facts and metric semantics

**Historical baseline (before PR #54):** Windows created a second hidden tray WebView at startup and retained it. **Current code (after PR #54):** the Windows tray WebView is created on demand and released after a hidden idle period; hidden/minimized WebViews receive best-effort LOW memory hints. The main WebView remains alive because background account features still depend on React. These are verified code changes, **not measured memory savings or evidence of a leak**. Compare the pre-optimization installed binary to a separately built post-optimization binary; checking out new source does not change an already installed executable.

* **Private Bytes** (Get-Process PrivateMemorySize64): committed private virtual memory. Additive across processes, but **not unique physical RAM**.
* **Working Set** (WorkingSet64): resident pages for each process. The **sum can double-count shared physical pages** between different processes. Reports name this aggregate WS-sum, not physical RAM.
* **CPU %**: delta process CPU seconds divided by wall time × logical processors. The first sample of a phase has no CPU reading. Short peaks between samples may be missed.
* **Process ownership:** the application PID and creation time anchor the graph. A child is counted only if its verified parent chain reaches the root, or if that same PID + creation time was previously observed as owned (orphan case). Each process identity is counted only **once per snapshot**, regardless of how many WebView2 windows use it. PID reuse cannot silently switch targets.
* **Possible shared WebView2:** an external WebView2 process with the same command-line user-data directory is logged as an ambiguous **count**, but never added to totals without proven ancestry. User-data paths and all command lines are deliberately omitted from exports. Identical data folder does not prove that another process belongs to this app. Uniquely shared physical pages cannot be apportioned using these counters; use VMMap / WPA / PSS for physical attribution.
* **Gaps:** inaccessible processes, very short-lived processes, and already-orphaned WebView2 processes may be absent. Check missingMetrics and ambiguousWebView2Candidates.

## Reproducible baseline

On Windows 10/11 with WebView2 installed, close existing Codex Switcher instances, then run from the repository root (replace the executable path):

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\benchmark-windows.ps1 -Executable "C:\Program Files\Codex Switcher\Codex Switcher.exe" -Label before -IdleMinutes 15 -SampleSeconds 3 -OutputDirectory "$env:USERPROFILE\Documents\SwitcherBenchmarks"

The script launches the app and records 20 s of startup. Follow its prompts:

1. Show the main window: record 25 s.
2. Hide the main window: record 25 s.
3. Left-click tray popup open / close: default 3 cycles, 9 s per open/closed state.
4. Hide all windows/popup and avoid interaction for 10–30 min: sustained idle sample.
5. Open main window and manually switch between safe test accounts: default 10 × 10 s.
6. Manually open/close Usage Stats: default 5 × 10 s.

For tray, account and statistics interactions **press Enter first, then perform the action immediately** during the capture interval. Test accounts must already be available. Changing accounts can affect live Codex sessions; save work. The benchmark does not itself switch accounts. Long time spent at the prompts does not appear in sample windows.

Attaching to an already-running process skips cold startup:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\benchmark-windows.ps1 -ProcessId 12345 -Label attached

If a started exe hands off to a different already-running instance, or a target PID is reused/exits, the script errors instead of measuring the wrong program.

## After and before/after comparison

Run the **same installed build type**, account dataset, feature settings, duration, number of switches, stats and tray interactions, Windows version, and similar system/network conditions. Supply a baseline file to the second run:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\benchmark-windows.ps1 -Executable "C:\Program Files\Codex Switcher\Codex Switcher.exe" -Label after -IdleMinutes 15 -SampleSeconds 3 -CompareTo "C:\Users\YOU\Documents\SwitcherBenchmarks\DATE-before\run.json" -OutputDirectory "$env:USERPROFILE\Documents\SwitcherBenchmarks"

Compare already collected runs without rerunning:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\compare-windows-benchmarks.ps1 -Before "C:\path\before\run.json" -After "C:\path\after\run.json" -OutputDirectory "C:\path\comparison"

Outputs in each timestamped run directory:

| File | Content |
|---|---|
| run.json | Metadata, per-stage summaries, sample-by-sample process tree, per-PID counters |
| samples.csv | Time series of aggregate CPU, Private Bytes, WS-sum, WebView2 totals, missing metrics |
| processes.csv | Per-PID attribution: parent PID, creation time, role, private bytes, WS, CPU, handles |
| report.md | Median and peak by phase, highest three memory processes at end of phase, idle-growth flag |
| before-after.json / before-after.csv / before-after.md | Paired stage metrics and deltas |

Delta Private MiB = after minus before; a negative number is an improvement. Reduction percent = 100 × (before − after) / before. A stage missing from either run is omitted rather than compared with zero. Outputs never include raw process command lines, account names, or user data directories.

## Detecting growing memory

Sort processes.csv by stage + timestamp and descending privateBytes to see whether the main Rust process, WebView2 group, or other descendants dominate. Compare the *same* stages; a renderer retaining Chromium caches is not necessarily a leak. Idle growth is flagged only with 20+ samples and a first-20%-median to last-20%-median increase of at least 32 MiB and 10%. This is **a prompt for investigation**, not a leak diagnosis.

Repeat before/after measurements three or more times with stable conditions. If the trend is primarily WebView2, inspect src/App.tsx, src/TrayMenu.tsx and src/components/AccountUsageStats.tsx around mounting/effects, and the hidden tray webview in src-tauri/src/tray.rs. If the main Rust process grows, inspect tray watchers and polling instead. These are code-review hypotheses until a Windows trace exists. The tool does not split Chromium browser / GPU / renderer subtypes beyond the msedgewebview2 executable name; Process Explorer can provide deeper roles.

## Tests

Dependency-free parsing and synthetic regression checks, without requiring live WebView2:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\tests\windows-perf.Tests.ps1
    pwsh -NoProfile -File .\scripts\tests\windows-perf.Tests.ps1

Windows CI runs both PowerShell editions. The real startup, tray interaction and leak outcomes **cannot be verified on a non-Windows runtime**; only on-machine sessions can answer whether the app's approximately 400 MB is committed by particular processes.
