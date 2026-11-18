# Windows main WebView2 hibernation

This is separate from the tray-popup lazy initialization added in PR #49.

## Runtime behavior
- A normal window close hides the **main** WebView. It does not terminate the Rust process or its native tray menu.
- React reports whether the main WebView is safe to release. Unknown, unsynchronized or busy state is **never** permission to release it.
- After 90 seconds fully hidden (not minimized), a still-idle main WebView is destroyed. Opening via native tray rebuilds the main WebView from the Tauri window configuration and restores in-process geometry. An epoch check cancels stale eviction timers when users reopen or start work.
- The native Rust tray continues to refresh the active quota and account metadata with no main WebView.
- Auto-warm-up and timed warm-up normally remain in React while that WebView exists. After it is destroyed, Rust takes over polling eligible accounts and sending warm-ups. Before React resumes scheduling, it reconciles its localStorage ledger with a persistent native ledger under an async handoff mutex; stale frontend state must not repeat native activity.
- **Auto-switch uses native detection and React execution:** the native tray's recent usage cache is checked every 30 seconds while the main WebView is absent. If quota is confirmed exhausted, the main UI is reopened (with a 5-minute wake cooldown) so the existing React switch/close/reopen/session recovery flow can run. No additional quota HTTP requests are made by the wake monitor. The UI may become visible when an automatic switch is required.
- Active dialogs, OAuth relogin windows, account switches, import/export, interactive background operations and pending session recovery block hibernation.

## Manual Windows validation (required before public release)
1. With auto-switch disabled and no active operation, open the app, record Private Bytes and per-process WebView2 Working Set.
2. Close the main window, leave the tray untouched for over 90 seconds, and verify the main WebView process is no longer allocated. The Rust process and native tray must remain.
3. Reopen from the tray; repeat five times, including rapid close/reopen near the 90-second timeout. Verify full UI, account cards, theme, resizing, sizing, positioning and tray left/right clicks.
4. Test a switch from the native tray with the main UI absent; verify the active account, Codex auth and next reopened UI agree.
5. Enable auto-warm-up for one account, keep the main UI closed across the next eligible quota window. Confirm **one** automatic request per window (including app restart, React rehydration and manual warm-up).
6. Enable timed warm-up with a schedule close to local current time; verify one execution in that minute, no late replay after sleep/resume, and no duplicate on reopening.
7. Turn on auto-switch with Codex sessions running, close the main window and wait for hibernation. With spare quota it should stay absent; upon a verified exhausted quota the main UI must reopen and the existing close/reopen/session-recovery behavior must remain functional. Confirm the native wake cooldown prevents repeated popups.
8. Toggle 'Warm up all' from the tray while main is absent; confirm the setting is honored by the Rust scheduler and is shown after reopening.
9. Verify Quit fully exits the application and Windows logout still works. Test on Windows 10/11 with WebView2.
10. Compare pre/post binaries with `scripts/benchmark-windows.ps1`. Do not sum shared Working Sets as unique physical RAM.

## Caveats
- No RAM reduction is guaranteed while the main UI is visible: it is still a React/WebView2 application.
- The native scheduler reads account tokens via the existing serialized OAuth code; it does not overwrite auth.json or switch accounts.
- A live Windows desktop end-to-end smoke test is not replaced by compilation-only CI.
