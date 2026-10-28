//! OAuth login Tauri commands

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

use crate::auth::oauth_server::{start_oauth_login, wait_for_oauth_login, OAuthLoginResult};
use crate::auth::switch_to_account;
use crate::auth::{
    acquire_auth_operation_lock, add_or_replace_account, duplicate_account_requires_confirmation,
    load_accounts, mutate_accounts, ACCOUNT_REPLACE_CONFIRMATION_PREFIX,
};
use crate::types::{
    parse_chatgpt_id_token_claims, AccountInfo, AuthData, OAuthLoginInfo, StoredAccount,
};
use chrono::Utc;

struct PendingOAuth {
    rx: Option<oneshot::Receiver<anyhow::Result<OAuthLoginResult>>>,
    cancelled: Arc<AtomicBool>,
    reauth_account_id: Option<String>,
}

// Global state for pending OAuth login. If login succeeds but replacing a
// healthy duplicate needs confirmation, keep the completed credentials in
// memory so the user can confirm without authenticating in the browser again.
static OAUTH_START_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static PENDING_OAUTH: Mutex<Option<PendingOAuth>> = Mutex::new(None);
static PENDING_COMPLETED_ACCOUNT: Mutex<Option<StoredAccount>> = Mutex::new(None);

/// Start an add-account or targeted re-login flow.
#[tauri::command]
pub async fn start_login(
    account_name: String,
    reauth_account_id: Option<String>,
) -> Result<OAuthLoginInfo, String> {
    let _start_guard = OAUTH_START_LOCK.lock().await;
    PENDING_COMPLETED_ACCOUNT.lock().unwrap().take();
    if let Some(id) = &reauth_account_id {
        let store = load_accounts().map_err(|e| e.to_string())?;
        let account = store
            .accounts
            .iter()
            .find(|a| &a.id == id)
            .ok_or("Account no longer exists")?;
        if !matches!(account.auth_data, AuthData::ChatGPT { .. }) {
            return Err("Only ChatGPT accounts support re-login".into());
        }
        if store.active_account_id.as_ref() == Some(id) {
            ensure_active_relogin_safe()?;
        }
    }

    if let Some(previous) = PENDING_OAUTH.lock().unwrap().take() {
        previous.cancelled.store(true, Ordering::Relaxed);
    }
    let (info, rx, cancelled) = start_oauth_login(account_name.trim().to_string())
        .await
        .map_err(|e| e.to_string())?;
    *PENDING_OAUTH.lock().unwrap() = Some(PendingOAuth {
        rx: Some(rx),
        cancelled,
        reauth_account_id,
    });
    Ok(info)
}

fn ensure_active_relogin_safe() -> Result<(), String> {
    super::process::ensure_codex_not_running().map_err(|_| {
        "Close Codex or switch to another account before re-logging in to the active account."
            .into()
    })
}

/// Match both the user and workspace; workspace IDs alone can be shared by a team.
fn validate_relogin_identity(old: &StoredAccount, new: &StoredAccount) -> Result<(), String> {
    let (
        AuthData::ChatGPT {
            id_token: old_token,
            account_id: old_id,
            ..
        },
        AuthData::ChatGPT {
            id_token: new_token,
            account_id: new_id,
            ..
        },
    ) = (&old.auth_data, &new.auth_data)
    else {
        return Err("Only ChatGPT accounts support re-login".into());
    };
    let old_claims = parse_chatgpt_id_token_claims(old_token);
    let new_claims = parse_chatgpt_id_token_claims(new_token);
    let old_workspace = old_claims.account_id.as_ref().or(old_id.as_ref());
    let new_workspace = new_claims.account_id.as_ref().or(new_id.as_ref());
    let same_user = match (old_claims.subject.as_deref(), new_claims.subject.as_deref()) {
        (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() => a == b,
        _ => match (old.email.as_deref(), new.email.as_deref()) {
            (Some(a), Some(b)) if !a.trim().is_empty() && !b.trim().is_empty() => {
                a.eq_ignore_ascii_case(b)
            }
            _ => false,
        },
    };
    if !same_user || old_workspace.is_none_or(|id| id.is_empty()) || old_workspace != new_workspace
    {
        return Err("Login does not match this account and workspace. Sign in with the original account and select the same workspace.".into());
    }
    Ok(())
}

fn save_relogin(id: &str, new: StoredAccount) -> Result<StoredAccount, String> {
    save_relogin_with_check(id, new, ensure_active_relogin_safe)
}

fn save_relogin_with_check(
    id: &str,
    new: StoredAccount,
    check_active: impl FnOnce() -> Result<(), String>,
) -> Result<StoredAccount, String> {
    // The outer auth-operation guard protects token rotation; accounts.lock
    // additionally prevents cross-process lost updates.
    mutate_accounts(|store| {
        let active = store.active_account_id.as_deref() == Some(id);
        let account = store
            .accounts
            .iter_mut()
            .find(|a| a.id == id)
            .ok_or_else(|| anyhow::anyhow!("Account no longer exists"))?;
        validate_relogin_identity(account, &new).map_err(anyhow::Error::msg)?;
        if active {
            check_active().map_err(anyhow::Error::msg)?;
        }
        let mut updated = account.clone();
        updated.auth_data = new.auth_data;
        updated.email = new.email;
        updated.plan_type = new.plan_type;
        updated.subscription_expires_at = new.subscription_expires_at;
        updated.last_refresh_at = Some(Utc::now());
        if active {
            // A failed live-auth write must not modify the stored account.
            switch_to_account(&updated).map_err(|e| {
                anyhow::anyhow!("Could not update Codex auth; saved account was not changed: {e}")
            })?;
        }
        *account = updated.clone();
        Ok(updated)
    })
    .map_err(|e| e.to_string())
}

/// Finish a normal OAuth login or renew a selected saved account.
/// Healthy duplicate accounts still require explicit replacement confirmation.
#[tauri::command]
pub async fn complete_login(force_replace: Option<bool>) -> Result<AccountInfo, String> {
    let force_replace = force_replace.unwrap_or(false);
    if force_replace {
        let staged = PENDING_COMPLETED_ACCOUNT.lock().unwrap().take();
        if let Some(account) = staged {
            let _guard = acquire_auth_operation_lock()
                .await
                .map_err(|e| e.to_string())?;
            let saved = add_or_replace_account(account, true).map_err(|e| e.to_string())?;
            let store = load_accounts().map_err(|e| e.to_string())?;
            return Ok(AccountInfo::from_stored(
                &saved,
                store.active_account_id.as_deref(),
            ));
        }
    }

    let (rx, cancelled, target) = {
        let mut pending = PENDING_OAUTH.lock().unwrap();
        let pending = pending.as_mut().ok_or("No pending OAuth login")?;
        (
            pending
                .rx
                .take()
                .ok_or("OAuth login is already being completed")?,
            pending.cancelled.clone(),
            pending.reauth_account_id.clone(),
        )
    };

    let result = async {
        let account = wait_for_oauth_login(rx).await.map_err(|e| e.to_string())?;
        if cancelled.load(Ordering::Relaxed) {
            return Err("OAuth login cancelled".into());
        }
        if let Some(id) = target {
            let _guard = acquire_auth_operation_lock()
                .await
                .map_err(|e| e.to_string())?;
            if cancelled.load(Ordering::Relaxed) {
                return Err("OAuth login cancelled".into());
            }
            // Active re-login is also a credential switch for the managed
            // CLI daemon; stop it before writing the renewed live auth.
            if load_accounts()
                .map_err(|e| e.to_string())?
                .active_account_id
                .as_deref()
                == Some(id.as_str())
            {
                ensure_active_relogin_safe()?;
                let stopped = tokio::task::spawn_blocking(
                    super::cli_daemon::stop_cli_daemon_if_running,
                )
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
                if let Err(error) = stopped {
                    eprintln!("[Auth] Failed to stop CLI daemon on re-login: {error:#}");
                }
            }
            let saved = save_relogin(&id, account)?;
            let store = load_accounts().map_err(|e| e.to_string())?;
            return Ok(AccountInfo::from_stored(
                &saved,
                store.active_account_id.as_deref(),
            ));
        }

        // Checking whether an existing session is healthy can refresh tokens,
        // so do this before acquiring the auth-operation lock.
        if !force_replace
            && duplicate_account_requires_confirmation(&account)
                .await
                .map_err(|e| e.to_string())?
        {
            if cancelled.load(Ordering::Relaxed) {
                return Err("OAuth login cancelled".into());
            }
            let name = account.name.clone();
            PENDING_COMPLETED_ACCOUNT.lock().unwrap().replace(account);
            return Err(format!("{ACCOUNT_REPLACE_CONFIRMATION_PREFIX}{name}"));
        }
        let _guard = acquire_auth_operation_lock()
            .await
            .map_err(|e| e.to_string())?;
        if cancelled.load(Ordering::Relaxed) {
            return Err("OAuth login cancelled".into());
        }
        let saved = add_or_replace_account(account, true).map_err(|e| e.to_string())?;
        let store = load_accounts().map_err(|e| e.to_string())?;
        Ok(AccountInfo::from_stored(
            &saved,
            store.active_account_id.as_deref(),
        ))
    }
    .await;

    let mut pending = PENDING_OAUTH.lock().unwrap();
    if pending
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(&current.cancelled, &cancelled))
    {
        pending.take();
    }
    result
}

#[tauri::command]
pub async fn cancel_login() -> Result<(), String> {
    let _start_guard = OAUTH_START_LOCK.lock().await;
    if let Some(pending) = PENDING_OAUTH.lock().unwrap().take() {
        pending.cancelled.store(true, Ordering::Relaxed);
    }

    PENDING_COMPLETED_ACCOUNT.lock().unwrap().take();
    Ok(())
}

#[cfg(test)]
#[path = "oauth_tests.rs"]
mod tests;
