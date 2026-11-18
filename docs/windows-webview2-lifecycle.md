# Windows WebView2 lifecycle (Codex Switcher)

## Policy in this PR

* **Startup:** the native Windows tray icon/menu starts without creating the React `tray.html` WebView. Native right-click account switching, menu labels, system theme handling and Rust quota/metadata refreshers keep working.
* **First left-click:** create the popup on Tauri's UI thread with the existing label, size, transparency, focus handler and cursor positioning. A failed popup creation falls back to showing the main window.
* **Reuse:** show/hide the same popup during the next **90 seconds** to avoid paying the WebView2/React cold-start penalty on rapid subsequent clicks.
* **Hidden:** request WebView2 `ICoreWebView2_19::SetMemoryUsageTargetLevel(LOW)`. Restore `NORMAL` before opening. Unsupported/older WebView2 runtimes fall back to ordinary hide.
* **Idle:** destroy the popup when hidden for 90 seconds. Epoch validation and execution on Tauri's UI thread prevent stale timers from destroying a reopened window; stale instance focus notifications cannot hide a new window.
* **Main window:** request LOW on Windows when hidden/minimized, NORMAL on restore. This does **not** suspend timers/JavaScript or alter React polling. Rust background functions remain independent.

These use supported Tauri `WebviewWindowBuilder`, `WebviewWindow::destroy`, `with_webview`, and the existing Wry/WebView2 COM bindings; no Tauri/WebView2 forks.

## Trade-offs and experiment matrix

| Strategy | Startup RAM from extra tray renderer | RAM when tray is closed | Reopen latency |
| --- | --- | --- | --- |
| Eager permanent (before) | Extra hidden WebView initialized at startup | Usually highest | Lowest; always warm |
| Lazy permanent | No extra tray WebView until first click | Still resident after first click | First click cold, later fast |
| Immediate destroy/recreate | No extra tray WebView until first click | Lowest once destroyed; shared WebView2 browser/GPU processes may remain because main uses them | Every click cold; higher latency and potential flicker |
| **Lazy + 90s reuse + LOW + idle destroy (this PR)** | **No extra tray WebView until first click** | Reduces hidden memory immediately when supported; eventually releases tray renderer | First click cold, repeat clicks within 90s fast, later cold |

**No MB/ms gains are claimed without Windows runtime measurements.** WebView2 uses multiple processes, sometimes shared between main and tray, so summing all `msedgewebview2.exe` processes can double-count or attribute unrelated consumers. Use the independent process-tree benchmark being developed by the performance agent to compare **before vs this PR**, ideally 5+ runs each from a fresh launch.

Required scenarios: launch with main visible, hide main without opening tray, first left-click (cold), second left-click within 90s (warm), right-click native menu/quick account switch, 15s hidden, 100s hidden (destroy expected), click again (recreate), minimize/restore main, 10–30m idle, rapid open/close and account switch under load. Capture per-PID Working Set, Private Bytes, process start/exit, total *unique* WebView2 PIDs and p50/p95 click-to-visible latency. Treat OS page cache/shared pages separately; compare the same Windows/WebView2 versions and GPU state.

## Safety checklist

1. Launch with popup absent and tray icon present. Right-click menu (including long-open native popup) and account switching work; theme/icon and titles remain correct.
2. First left-click opens popup at cursor with accounts/data displayed (initial React mount loads without relying on an event emitted before listener registration).
3. Focus loss hides popup; subsequent rapid left-click reuses it. Test mouse click spam and quick window activation changes.
4. Switching via popup or `Open Codex Switcher` hides popup, and the main app can always be reopened from the native context menu.
5. After 90 seconds hidden, reopen and confirm the WebView label is reusable and no extra orphan popup or lost menu events appear.
6. Minimize/restore main while background quota display and metadata watcher continue. Reopening must not show stale/blank content.
7. Run Windows CI `cargo check --locked`, Rust regression tests and frontend build/tests. Linux/macOS behavior should remain unchanged (macOS retains eager popup).

## API / overhead notes

* WebView2 LOW is a **best-effort hint**, not a reliable MB cap; Microsoft's guidance says cached data may be released or paged out. It does not guarantee immediate reduction in process count. WebView2 `TrySuspend` is intentionally not used because it suspends scripts; `EmptyWorkingSet`/forceful trimming is also avoided (page-fault thrash and misleading Working Set readings).
* Windows Tauri/Wry does not implement `backgroundThrottling` for window builders; existing `backgroundThrottling: disabled` in the main Tauri configuration is not a Windows optimization lever.
* The tray's transparent, always-on-top WebView can incur compositor/GPU overhead. Disabling GPU or altering WebView2 browser arguments globally may hurt rendering and force software rasterization; there is no evidence for doing that here.
* Persistent WebView2 cache/data should not be wiped at hide/destroy. Keep environment/data directory compatibility; shared browser processes may remain while the main WebView runs.
* Follow up only after measured data: consider adjusting the 90s reuse window based on click frequency and p95 cold-start cost. Do not automatically alter React polling or process accounting in this PR.

References: [WebView2 performance](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/performance), [ICoreWebView2_19](https://learn.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/icorewebview2_19), [Tauri WebviewWindow](https://docs.rs/tauri/2/tauri/webview/struct.WebviewWindow.html).
