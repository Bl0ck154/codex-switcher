//! Reclaim the *main* WebView2 when the app is only serving the native tray.
//! This deliberately requires an explicit, conservative frontend idle handshake:
//! auto-switch/session recovery and automatic warm-up still run in React.
//! Never discard that runtime while any of those features is enabled.
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        LazyLock, Mutex,
    },
    time::Duration,
};

use tauri::{
    AppHandle, Manager, PhysicalPosition, PhysicalSize, Runtime, WebviewWindowBuilder,
};

const HIBERNATE_AFTER: Duration = Duration::from_secs(90);
static READY: AtomicBool = AtomicBool::new(false);
static MAY_HIBERNATE: AtomicBool = AtomicBool::new(false);
static EPOCH: AtomicU64 = AtomicU64::new(0);
static REOPEN_PENDING: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy)]
struct Geometry {
    position: PhysicalPosition<i32>,
    size: PhysicalSize<u32>,
}
static LAST_GEOMETRY: LazyLock<Mutex<Option<Geometry>>> =
    LazyLock::new(|| Mutex::new(None));

/// Tauri command called after React has initialized its persisted settings.
/// It is deliberately false until the UI can positively prove no background
/// automation or interactive operation depends on the main WebView.
pub fn report_frontend_idle<R: Runtime>(app: &AppHandle<R>, idle: bool) {
    READY.store(true, Ordering::Release);
    MAY_HIBERNATE.store(idle, Ordering::Release);
    EPOCH.fetch_add(1, Ordering::AcqRel);
    if idle && app.get_webview_window("main")
        .is_some_and(|window| window.is_visible().ok() == Some(false))
    {
        schedule_hibernation(app);
    }
}

pub fn main_hidden<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window("main") {
        if let (Ok(position), Ok(size)) = (window.outer_position(), window.inner_size()) {
            if let Ok(mut geometry) = LAST_GEOMETRY.lock() {
                *geometry = Some(Geometry { position, size });
            }
        }
    }
    schedule_hibernation(app);
}

fn schedule_hibernation<R: Runtime>(app: &AppHandle<R>) {
    let epoch = EPOCH.fetch_add(1, Ordering::AcqRel) + 1;
    if !READY.load(Ordering::Acquire) || !MAY_HIBERNATE.load(Ordering::Acquire) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(HIBERNATE_AFTER).await;
        let ui_app = app.clone();
        if let Err(error) = app.run_on_main_thread(move || {
            let Some(window) = ui_app.get_webview_window("main") else {
                return;
            };
            if should_destroy(
                epoch,
                EPOCH.load(Ordering::Acquire),
                READY.load(Ordering::Acquire),
                MAY_HIBERNATE.load(Ordering::Acquire),
                window.is_visible().ok(),
            ) {
                // Do not let a stale frontend report authorize a later main
                // window instance. The new page must explicitly handshake.
                READY.store(false, Ordering::Release);
                MAY_HIBERNATE.store(false, Ordering::Release);
                if let Err(error) = window.destroy() {
                    eprintln!("[Hibernate] Could not release main WebView2: {error}");
                }
            }
        }) {
            eprintln!("[Hibernate] Could not schedule main WebView2 release: {error}");
        }
    });
}

fn should_destroy(
    scheduled_epoch: u64,
    current_epoch: u64,
    ready: bool,
    idle: bool,
    visible: Option<bool>,
) -> bool {
    scheduled_epoch == current_epoch && ready && idle && visible == Some(false)
}

/// Reopening invalidates in-flight idle evictions. If the main WebView2 was
/// destroyed, create it off the Tauri event loop (a synchronous event-handler
/// WebView2 build can deadlock Windows).
pub fn show_or_create<R: Runtime>(app: &AppHandle<R>) {
    EPOCH.fetch_add(1, Ordering::AcqRel);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        return;
    }
    if REOPEN_PENDING.swap(true, Ordering::AcqRel) {
        return;
    }

    let app = app.clone();
    std::thread::spawn(move || {
        let config = app
            .config()
            .app
            .windows
            .iter()
            .find(|window| window.label == "main")
            .cloned();
        if let Some(config) = config {
            match WebviewWindowBuilder::from_config(&app, &config)
                .and_then(|builder| builder.build())
            {
                Ok(window) => {
                    if let Ok(geometry) = LAST_GEOMETRY.lock() {
                        if let Some(geometry) = *geometry {
                            let _ = window.set_size(geometry.size);
                            let _ = window.set_position(geometry.position);
                        }
                    }
                    let _ = window.show();
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                }
                Err(error) => eprintln!("[Hibernate] Failed to reopen main UI: {error}"),
            }
        } else {
            eprintln!("[Hibernate] Missing main window config");
        }
        REOPEN_PENDING.store(false, Ordering::Release);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_matching_generation_and_explicit_idle_handshake() {
        assert!(should_destroy(2, 2, true, true, Some(false)));
        assert!(!should_destroy(2, 3, true, true, Some(false)));
        assert!(!should_destroy(2, 2, false, true, Some(false)));
        assert!(!should_destroy(2, 2, true, false, Some(false)));
        assert!(!should_destroy(2, 2, true, true, Some(true)));
        assert!(!should_destroy(2, 2, true, true, None));
    }
}
