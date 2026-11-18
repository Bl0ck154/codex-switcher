//! Native Windows warm-up scheduling for periods when the main React WebView
//! has been destroyed. While it exists, React remains the scheduler to preserve
//! the original UI behavior and avoid duplicate warm-up requests.
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use crate::{
    api::usage::{get_account_usage, warmup_account},
    auth::{load_accounts, load_app_settings, mutate_app_settings},
    types::{NativeBackgroundWarmup, NativeWarmupHistory, StoredAccount, UsageInfo},
};
use chrono::{Datelike, Local, Utc};
use tauri::{AppHandle, Manager};

// Serializes native tick and React handoff: no two schedulers can start the
// same warm-up while the UI is hydrating the updated native ledger.
static WARMUP_HANDOFF: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const CHECK_INTERVAL: Duration = Duration::from_secs(30);
const AUTO_USAGE_INTERVAL: Duration = Duration::from_secs(60);
const MIN_SUCCESS_INTERVAL_MS: i64 = 60 * 60 * 1000;
const FULL_WINDOW_SLACK_MINUTES: i64 = 5;
const FULL_LIMIT: f64 = 99.5;

fn merge_history(
    stored: &mut HashMap<String, NativeWarmupHistory>,
    incoming: HashMap<String, NativeWarmupHistory>,
) {
    for (id, value) in incoming {
        let old = stored.entry(id).or_default();
        let old_at = old.last_successful_warmup_at.unwrap_or(0);
        let new_at = value.last_successful_warmup_at.unwrap_or(0);
        if new_at > old_at {
            old.last_successful_warmup_at = Some(new_at);
            if value.last_auto_window_key.is_some() {
                old.last_auto_window_key = value.last_auto_window_key;
                old.last_auto_window_kind = value.last_auto_window_kind;
            }
        } else if new_at == old_at && old.last_auto_window_key.is_none() {
            old.last_auto_window_key = value.last_auto_window_key;
            old.last_auto_window_kind = value.last_auto_window_kind;
        }
    }
}

fn date_parts(s: &str) -> Option<(i32, u32, u32)> {
    let mut parts = s.split('-');
    let year = parts.next()?.parse().ok()?;
    let month = parts.next()?.parse().ok()?;
    let day = parts.next()?.parse().ok()?;
    (parts.next().is_none() && month <= 11 && (1..=31).contains(&day)).then_some((year, month, day))
}

/// Called on every UI config/ledger change and once on each cold WebView start.
/// Previously recorded native activity always wins over stale localStorage.
pub async fn sync_from_frontend(
    auto_warmup_all_enabled: bool,
    auto_warmup_account_ids: Vec<String>,
    auto_warmup_ledger: HashMap<String, NativeWarmupHistory>,
    timed_warmup_enabled: bool,
    timed_warmup_times: Vec<String>,
    timed_warmup_ledger: HashMap<String, String>,
) -> Result<NativeBackgroundWarmup, String> {
    let _guard = WARMUP_HANDOFF.lock().await;
    mutate_app_settings(|settings| {
        let target = &mut settings.windows_background_warmup;
        target.initialized = true;
        target.auto_warmup_all_enabled = auto_warmup_all_enabled;
        target.auto_warmup_account_ids = auto_warmup_account_ids;
        target.timed_warmup_enabled = timed_warmup_enabled;
        target.timed_warmup_times = timed_warmup_times
            .into_iter()
            .filter(|value| valid_time(value))
            .take(24)
            .collect();
        merge_history(&mut target.auto_warmup_ledger, auto_warmup_ledger);
        for (time, day) in timed_warmup_ledger {
            if !valid_time(&time) {
                continue;
            }
            let Some(candidate) = date_parts(&day) else {
                continue;
            };
            let previous = target
                .timed_warmup_ledger
                .get(&time)
                .and_then(|value| date_parts(value));
            if previous.is_none_or(|prev| candidate > prev) {
                target.timed_warmup_ledger.insert(time, day);
            }
        }
        Ok(target.clone())
    })
    .map_err(|err| err.to_string())
}

/// The tray popup can update this setting while the main WebView is absent.
pub async fn change_all_enabled(enabled: bool) -> Result<(), String> {
    let _guard = WARMUP_HANDOFF.lock().await;
    mutate_app_settings(|settings| {
        if settings.windows_background_warmup.initialized {
            settings.windows_background_warmup.auto_warmup_all_enabled = enabled;
        }
        Ok(())
    })
    .map_err(|err| err.to_string())
}

pub fn setup(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut last_auto: Option<Instant> = None;
        loop {
            tokio::time::sleep(CHECK_INTERVAL).await;
            // Don't run alongside React or before its localStorage settings
            // have been migrated. A created-but-hidden WebView is still alive.
            if app.get_webview_window("main").is_some() {
                last_auto = None;
                continue;
            }
            let auto_due = last_auto.is_none_or(|last| last.elapsed() >= AUTO_USAGE_INTERVAL);
            if auto_due {
                last_auto = Some(Instant::now());
            }
            if let Err(error) = run_tick(&app, auto_due).await {
                eprintln!("[NativeWarmup] Background tick failed: {error}");
            }
        }
    });
}

fn valid_time(time: &str) -> bool {
    let bytes = time.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return false;
    }
    let (Ok(hour), Ok(minute)) = (time[..2].parse::<u8>(), time[3..].parse::<u8>()) else {
        return false;
    };
    hour < 24 && minute < 60
}

fn due_window(
    usage: &UsageInfo,
    history: Option<&NativeWarmupHistory>,
    now_ms: i64,
) -> Option<(String, &'static str)> {
    if usage.error.is_some() {
        return None;
    }
    let primary = usage.primary_used_percent.is_some()
        || usage.primary_window_minutes.is_some()
        || usage.primary_resets_at.is_some();
    let secondary = usage.secondary_used_percent.is_some()
        || usage.secondary_window_minutes.is_some()
        || usage.secondary_resets_at.is_some();
    let (kind, duration, reset) = if primary {
        if usage
            .secondary_used_percent
            .is_some_and(|used| used >= FULL_LIMIT)
        {
            return None;
        }
        (
            "session",
            usage.primary_window_minutes.unwrap_or(300),
            usage.primary_resets_at?,
        )
    } else if secondary {
        (
            "weekly",
            usage.secondary_window_minutes.unwrap_or(10_080),
            usage.secondary_resets_at?,
        )
    } else {
        return None;
    };
    if duration <= 0 || reset <= 0 {
        return None;
    }
    if reset.saturating_mul(1000).saturating_sub(now_ms)
        < (duration - FULL_WINDOW_SLACK_MINUTES)
            .max(0)
            .saturating_mul(60_000)
    {
        return None;
    }
    let key = format!("{kind}:{duration}:{reset}");
    if history.and_then(|h| h.last_auto_window_key.as_ref()) == Some(&key) {
        return None;
    }
    if let Some(h) = history {
        if let Some(last) = h.last_successful_warmup_at {
            if now_ms.saturating_sub(last) < MIN_SUCCESS_INTERVAL_MS
                && h.last_auto_window_kind
                    .as_deref()
                    .is_none_or(|previous| previous == kind)
            {
                return None;
            }
        }
    }
    Some((key, kind))
}

async fn run_tick(app: &AppHandle, auto_due: bool) -> Result<(), String> {
    let _guard = WARMUP_HANDOFF.lock().await;
    if app.get_webview_window("main").is_some() {
        return Ok(());
    }
    let config = load_app_settings()
        .map_err(|e| e.to_string())?
        .windows_background_warmup;
    if !config.initialized {
        return Ok(());
    }

    let now = Local::now();
    let current_time = now.format("%H:%M").to_string();
    // Historical frontend date format has zero-based months.
    let today = format!("{}-{}-{}", now.year(), now.month0(), now.day());
    let timed_due = config.timed_warmup_enabled
        && config
            .timed_warmup_times
            .iter()
            .any(|time| time == &current_time)
        && config.timed_warmup_ledger.get(&current_time) != Some(&today);
    let ids: HashSet<&str> = config
        .auto_warmup_account_ids
        .iter()
        .map(String::as_str)
        .collect();
    if !timed_due && (!auto_due || (!config.auto_warmup_all_enabled && ids.is_empty())) {
        return Ok(());
    }

    let accounts = load_accounts().map_err(|e| e.to_string())?.accounts;
    let mut candidates: Vec<(StoredAccount, UsageInfo)> = Vec::new();
    for account in accounts {
        if !timed_due && !(config.auto_warmup_all_enabled || ids.contains(account.id.as_str())) {
            continue;
        }
        if app.get_webview_window("main").is_some() {
            return Ok(());
        }
        match get_account_usage(&account).await {
            Ok(usage) if usage.error.is_none() => candidates.push((account, usage)),
            _ => continue,
        }
    }
    if timed_due {
        let targets: Vec<_> = candidates
            .iter()
            .filter(|(_, usage)| {
                usage
                    .secondary_used_percent
                    .is_none_or(|used| used < FULL_LIMIT)
            })
            .map(|(account, _)| account.clone())
            .collect();
        if !targets.is_empty() {
            // Mark before sending: a long-running warm-up cannot re-fire within
            // the same minute. On failure, the next scheduled day can retry.
            mutate_app_settings(|settings| {
                settings
                    .windows_background_warmup
                    .timed_warmup_ledger
                    .insert(current_time.clone(), today.clone());
                Ok(())
            })
            .map_err(|e| e.to_string())?;
            for account in targets {
                if app.get_webview_window("main").is_some() {
                    return Ok(());
                }
                if warmup_account(&account).await.is_ok() {
                    record_success(&account.id, None)?;
                }
            }
        }
    }
    if auto_due {
        let now_ms = Utc::now().timestamp_millis();
        for (account, usage) in candidates {
            if !(config.auto_warmup_all_enabled || ids.contains(account.id.as_str())) {
                continue;
            }
            if app.get_webview_window("main").is_some() {
                return Ok(());
            }
            let latest = load_app_settings()
                .map_err(|e| e.to_string())?
                .windows_background_warmup;
            let Some((key, kind)) =
                due_window(&usage, latest.auto_warmup_ledger.get(&account.id), now_ms)
            else {
                continue;
            };
            if warmup_account(&account).await.is_ok() {
                record_success(&account.id, Some((key, kind)))?;
            }
        }
    }
    Ok(())
}

fn record_success(account_id: &str, window: Option<(String, &str)>) -> Result<(), String> {
    mutate_app_settings(|settings| {
        let entry = settings
            .windows_background_warmup
            .auto_warmup_ledger
            .entry(account_id.to_owned())
            .or_default();
        entry.last_successful_warmup_at = Some(Utc::now().timestamp_millis());
        if let Some((key, kind)) = window {
            entry.last_auto_window_key = Some(key);
            entry.last_auto_window_kind = Some(kind.to_owned());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(reset: i64, weekly: f64) -> UsageInfo {
        UsageInfo {
            account_id: "a".into(),
            plan_type: None,
            primary_used_percent: Some(0.0),
            primary_window_minutes: Some(300),
            primary_resets_at: Some(reset),
            secondary_used_percent: Some(weekly),
            secondary_window_minutes: Some(10080),
            secondary_resets_at: None,
            luna_reserve: None,
            has_credits: None,
            unlimited_credits: None,
            credits_balance: None,
            error: None,
        }
    }
    #[test]
    fn only_fresh_unused_windows_are_due() {
        let now = 1_700_000_000_000i64;
        let reset = now / 1000 + 300 * 60;
        let first = due_window(&usage(reset, 0.0), None, now).unwrap();
        let history = NativeWarmupHistory {
            last_successful_warmup_at: Some(now),
            last_auto_window_key: Some(first.0),
            last_auto_window_kind: Some("session".into()),
        };
        assert!(due_window(&usage(reset, 0.0), Some(&history), now).is_none());
        assert!(due_window(&usage(reset, 100.0), None, now).is_none());
        assert!(due_window(&usage(reset - 601, 0.0), None, now).is_none());
    }
    #[test]
    fn merged_handoff_cannot_lose_newer_native_success() {
        let mut stored = HashMap::from([(
            "a".to_owned(),
            NativeWarmupHistory {
                last_successful_warmup_at: Some(150),
                last_auto_window_key: Some("s1".into()),
                last_auto_window_kind: Some("session".into()),
            },
        )]);
        merge_history(
            &mut stored,
            HashMap::from([(
                "a".to_owned(),
                NativeWarmupHistory {
                    last_successful_warmup_at: Some(100),
                    last_auto_window_key: Some("old".into()),
                    last_auto_window_kind: Some("weekly".into()),
                },
            )]),
        );
        assert_eq!(stored["a"].last_auto_window_key.as_deref(), Some("s1"));
    }
    #[test]
    fn strict_24_hour_times_and_legacy_date() {
        assert!(valid_time("09:00"));
        assert!(!valid_time("9:00"));
        assert!(!valid_time("25:03"));
        assert_eq!(date_parts("2026-10-18"), Some((2026, 10, 18)));
    }
}
