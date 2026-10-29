//! ChatGPT OAuth token refresh helpers

use anyhow::{Context, Result};
use base64::Engine;
use chrono::{Duration as ChronoDuration, Utc};
use std::{
    collections::HashSet,
    sync::{Mutex, OnceLock},
};
use tokio::time::Duration;

use super::{
    acquire_auth_operation_lock, load_accounts, mutate_accounts, read_current_auth,
    same_saved_account, switch_to_account, sync_active_account_tokens,
    update_account_chatgpt_tokens,
};
use crate::types::{
    parse_chatgpt_id_token_claims, AccountsStore, AuthData, AuthDotJson, StoredAccount,
};

const DEFAULT_ISSUER: &str = "https://auth.openai.com";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ID_TOKEN_REFRESH_WINDOW_SECONDS: i64 = 60;
const ACCESS_TOKEN_REFRESH_WINDOW_SECONDS: i64 = 5 * 60;
const OPAQUE_ACCESS_TOKEN_REFRESH_INTERVAL_DAYS: i64 = 8;

static STALE_CHATGPT_SESSION_IDS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn stale_chatgpt_session_ids() -> &'static Mutex<HashSet<String>> {
    STALE_CHATGPT_SESSION_IDS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn mark_saved_session_stale(account_id: &str) {
    stale_chatgpt_session_ids()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(account_id.to_string());
}

fn clear_saved_session_stale(account_id: &str) {
    stale_chatgpt_session_ids()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(account_id);
}

fn take_saved_session_stale(account_id: &str) -> bool {
    stale_chatgpt_session_ids()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(account_id)
}

#[derive(Debug, serde::Deserialize)]
struct RefreshTokenResponse {
    #[serde(default)]
    id_token: Option<String>,
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct RefreshTokenErrorResponse {
    #[serde(default)]
    error: Option<RefreshTokenErrorDetails>,
}

#[derive(Debug, serde::Deserialize)]
struct RefreshTokenErrorDetails {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

#[derive(Debug)]
struct TokenRefreshUpdate {
    id_token: String,
    access_token: String,
    refresh_token: String,
    id_token_error: Option<anyhow::Error>,
}

/// Ensure the account has a usable ChatGPT access token for background API calls.
///
/// Usage polling and warm-up do not need a fresh ID token. Refreshing only because
/// an ID token expired can unnecessarily consume a single-use refresh token while
/// the access token and the live Codex session are still perfectly valid.
pub async fn ensure_chatgpt_tokens_fresh(account: &StoredAccount) -> Result<StoredAccount> {
    if !chatgpt_access_token_needs_refresh(account) {
        return Ok(account.clone());
    }

    let _auth_guard = acquire_auth_operation_lock().await?;
    ensure_chatgpt_tokens_fresh_with_policy_locked(account, false).await
}

/// Ensure the full ChatGPT OAuth token set is fresh while the caller holds
/// the auth-operation lock. Account switching still needs a fresh ID token because
/// Codex can reject auth.json when its ID token is expired.
pub(crate) async fn ensure_chatgpt_tokens_fresh_locked(
    account: &StoredAccount,
) -> Result<StoredAccount> {
    match ensure_chatgpt_tokens_fresh_with_policy_locked(account, true).await {
        Ok(account) => Ok(account),
        Err(refresh_error) => {
            // A stale/invalidated refresh token must not make the Switch button
            // unusable while the stored access token is still accepted. We still
            // prefer a full refresh first (fresh ID token is best for Codex), but
            // if that cannot be done, fall back to the latest stored/live account
            // as long as its access token is still usable.
            let (fallback, _) = load_account_reconciling_live_auth(&account.id)?;
            if can_fallback_after_refresh_failure(&refresh_error, &fallback) {
                eprintln!(
                    "[Auth] Full OAuth refresh failed before switch for account {}: {refresh_error:#}. \
Proceeding with the existing unexpired access token.",
                    fallback.name
                );
                Ok(fallback)
            } else {
                Err(refresh_error.context(
                    "Codex credentials could not be renewed. Re-login to this account before switching."
                ))
            }
        }
    }
}

async fn ensure_chatgpt_tokens_fresh_with_policy_locked(
    account: &StoredAccount,
    require_fresh_id_token: bool,
) -> Result<StoredAccount> {
    if matches!(account.auth_data, AuthData::ApiKey { .. }) {
        return Ok(account.clone());
    }

    // Codex may have refreshed the active account while this task waited for
    // the lock. Prefer those live credentials over rotating the stored token.
    let (current, _) = load_account_reconciling_live_auth(&account.id)?;

    match &current.auth_data {
        AuthData::ApiKey { .. } => Ok(current.clone()),
        AuthData::ChatGPT {
            id_token,
            access_token,
            ..
        } => {
            let needs_refresh = if require_fresh_id_token {
                chatgpt_tokens_need_refresh_at(id_token, access_token, Utc::now().timestamp())
                    || chatgpt_access_token_needs_refresh(&current)
            } else {
                chatgpt_access_token_needs_refresh(&current)
            };

            if needs_refresh {
                let reason = if require_fresh_id_token {
                    "OAuth token"
                } else {
                    "access token"
                };
                println!(
                    "[Auth] {reason} expired/near expiry for account {}, refreshing",
                    current.name
                );
                refresh_chatgpt_tokens_locked(&current).await
            } else {
                Ok(current)
            }
        }
    }
}

/// Force-refresh ChatGPT OAuth tokens for an account.
pub async fn refresh_chatgpt_tokens(account: &StoredAccount) -> Result<StoredAccount> {
    if matches!(account.auth_data, AuthData::ApiKey { .. }) {
        return Ok(account.clone());
    }

    let _auth_guard = acquire_auth_operation_lock().await?;
    refresh_chatgpt_tokens_locked(account).await
}

async fn refresh_chatgpt_tokens_locked(account: &StoredAccount) -> Result<StoredAccount> {
    let (current, is_active) = load_account_reconciling_live_auth(&account.id)?;

    if is_active && crate::commands::process::ensure_codex_not_running().is_err() {
        return Ok(current);
    }

    let (current_id_token, current_refresh_token, current_account_id) = match &current.auth_data {
        AuthData::ChatGPT {
            id_token,
            refresh_token,
            account_id,
            ..
        } => (id_token.clone(), refresh_token.clone(), account_id.clone()),
        AuthData::ApiKey { .. } => return Ok(current),
    };

    if current_refresh_token.is_empty() {
        anyhow::bail!("Missing refresh token for account {}", current.name);
    }

    let refreshed = match refresh_tokens_with_refresh_token(&current_refresh_token).await {
        Ok(refreshed) => {
            clear_saved_session_stale(&current.id);
            refreshed
        }
        Err(error) => {
            if refresh_error_invalidates_saved_session(&error) {
                mark_saved_session_stale(&current.id);
            }
            return Err(error);
        }
    };
    let next = merge_refresh_response(
        current_id_token,
        current_refresh_token,
        refreshed,
        Utc::now().timestamp(),
    );

    let claims = parse_chatgpt_id_token_claims(&next.id_token);
    let next_account_id = claims.account_id.or(current_account_id);

    let updated = update_account_chatgpt_tokens(
        &account.id,
        next.id_token,
        next.access_token,
        next.refresh_token,
        next_account_id,
        claims.email,
        claims.plan_type,
        claims.subscription_expires_at,
    )?;
    println!("[Auth] Refreshed OAuth tokens for: {}", updated.name);

    // Refresh tokens can be single-use. Persist a rotated replacement before
    // reporting an unusable ID token, so a later retry can still recover.
    if let Some(error) = next.id_token_error {
        return Err(error);
    }

    // Re-read active state after the network request before touching auth.json.
    let is_active = load_accounts()?.active_account_id.as_deref() == Some(account.id.as_str());
    if is_active {
        // Do not claim a successful token refresh when Codex will still load
        // the previous (possibly revoked) token from its live auth.json.
        // The rotated credentials remain safely stored in accounts.json so
        // an explicit switch can retry the live write.
        switch_to_account(&updated).context(
            "OAuth tokens were saved, but Codex auth.json could not be updated. Close Codex and switch to the account again."
        )?;
    }

    Ok(updated)
}

fn reconcile_active_account_from_auth(
    store: &mut AccountsStore,
    account_id: &str,
    auth: &AuthDotJson,
) -> bool {
    if store.active_account_id.as_deref() != Some(account_id) {
        return false;
    }

    sync_active_account_tokens(store, auth)
}

fn load_account_reconciling_live_auth(account_id: &str) -> Result<(StoredAccount, bool)> {
    let live_auth = read_current_auth()?;

    mutate_accounts(|store| {
        let is_active = store.active_account_id.as_deref() == Some(account_id);

        if is_active {
            if let Some(auth) = live_auth.as_ref() {
                reconcile_active_account_from_auth(store, account_id, auth);
            }
        }

        let account = store
            .accounts
            .iter()
            .find(|stored| stored.id == account_id)
            .cloned()
            .context("Account not found")?;
        Ok((account, is_active))
    })
}

/// Build a new ChatGPT account from a refresh token.
/// This is used by slim import to recreate full credentials.
pub async fn create_chatgpt_account_from_refresh_token(
    account_name: String,
    refresh_token: String,
) -> Result<StoredAccount> {
    if refresh_token.trim().is_empty() {
        anyhow::bail!("Missing refresh token for account {account_name}");
    }

    let refreshed = refresh_tokens_with_refresh_token(&refresh_token).await?;
    let id_token = refreshed
        .id_token
        .context("Refresh response did not include id_token")?;
    let next_refresh_token = refreshed.refresh_token.unwrap_or(refresh_token);
    let claims = parse_chatgpt_id_token_claims(&id_token);

    Ok(StoredAccount::new_chatgpt(
        account_name,
        claims.email,
        claims.plan_type,
        claims.subscription_expires_at,
        id_token,
        refreshed.access_token,
        next_refresh_token,
        claims.account_id,
    ))
}

fn chatgpt_access_token_needs_refresh(account: &StoredAccount) -> bool {
    match &account.auth_data {
        AuthData::ApiKey { .. } => false,
        AuthData::ChatGPT { access_token, .. } => should_refresh_proactively_at(
            access_token,
            account.last_refresh_at.map(|time| time.timestamp()),
            Utc::now().timestamp(),
        ),
    }
}

fn refresh_error_invalidates_saved_session(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_lowercase();
    message.contains("refresh_token_invalidated")
        || message.contains("refresh_token_reused")
        || message.contains("saved session is out of date")
        || message.contains("outdated refresh token")
        || message.contains("your session has ended")
        || message.contains("invalid refresh token")
        || message.contains("refresh token is invalid")
        || message.contains("invalid_grant")
}

/// Decide whether replacing a duplicate account should require user confirmation.
///
/// Verify the stored refresh token once even when the current access token still
/// works. A successful refresh means the saved session is genuinely healthy, while
/// terminal OAuth errors such as refresh_token_invalidated/reused mean it is stale
/// and can be replaced automatically. Transient/unknown failures stay conservative
/// and ask before overwriting credentials.
pub async fn duplicate_account_requires_confirmation(candidate: &StoredAccount) -> Result<bool> {
    let store = load_accounts()?;
    let Some(existing) = store
        .accounts
        .iter()
        .find(|account| same_saved_account(account, candidate))
        .cloned()
    else {
        return Ok(false);
    };

    if matches!(existing.auth_data, AuthData::ApiKey { .. }) {
        return Ok(true);
    }

    // Usage polling may already have proved that the saved refresh token is
    // terminally invalid. Preserve that fact across the re-auth flow so an active
    // account is not misclassified as healthy merely because Codex is currently
    // running and refresh_chatgpt_tokens() intentionally avoids rotating its token.
    if take_saved_session_stale(&existing.id) {
        eprintln!(
            "[Auth] Existing duplicate account {} was already observed with a stale OAuth session; replacing without confirmation",
            existing.name
        );
        return Ok(false);
    }

    match refresh_chatgpt_tokens(&existing).await {
        Ok(_) => Ok(true),
        Err(error) if refresh_error_invalidates_saved_session(&error) => {
            eprintln!(
                "[Auth] Existing duplicate account {} has a stale OAuth session; replacing without confirmation: {error:#}",
                existing.name
            );
            Ok(false)
        }
        Err(error) => {
            eprintln!(
                "[Auth] Could not verify duplicate account {} because refresh failed transiently/unknown; asking before replacement: {error:#}",
                existing.name
            );
            Ok(true)
        }
    }
}

fn can_switch_with_existing_access_token(account: &StoredAccount) -> bool {
    !chatgpt_access_token_needs_refresh(account)
}

fn can_fallback_after_refresh_failure(error: &anyhow::Error, account: &StoredAccount) -> bool {
    // An unexpired JWT is not proof of a valid Codex session. Only transient
    // network/provider failures may use the previous access token. A terminal
    // rejection, failed live auth.json write, failed credential persistence or
    // incomplete token response must never turn into a successful switch.
    let message = format!("{error:#}").to_lowercase();
    let transient = message.contains("failed to send token refresh request")
        || ["429", "500", "502", "503", "504"]
            .iter()
            .any(|status| message.contains(&format!("token refresh failed: {status}")));
    transient
        && !refresh_error_invalidates_saved_session(error)
        && can_switch_with_existing_access_token(account)
}

/// JWT access tokens refresh five minutes before expiry. For tokens that
/// do not expose a parseable expiry, use the last persisted refresh time so
/// a still-valid opaque token is not rotated on every usage request.
fn should_refresh_proactively_at(
    access_token: &str,
    last_refresh_at: Option<i64>,
    now: i64,
) -> bool {
    if parse_jwt_exp(access_token).is_some() {
        return token_expired_or_near_expiry_at(access_token, now);
    }

    last_refresh_at.is_some_and(|time| {
        time < now - ChronoDuration::days(OPAQUE_ACCESS_TOKEN_REFRESH_INTERVAL_DAYS).num_seconds()
    })
}

fn chatgpt_tokens_need_refresh(account: &StoredAccount) -> bool {
    match &account.auth_data {
        AuthData::ApiKey { .. } => false,
        AuthData::ChatGPT {
            id_token,
            access_token,
            ..
        } => chatgpt_tokens_need_refresh_at(id_token, access_token, Utc::now().timestamp()),
    }
}

fn chatgpt_tokens_need_refresh_at(id_token: &str, access_token: &str, now: i64) -> bool {
    id_token_needs_refresh_at(id_token, now) || token_expired_or_near_expiry_at(access_token, now)
}

fn id_token_needs_refresh_at(token: &str, now: i64) -> bool {
    match parse_jwt_exp(token) {
        Some(expiry) => expiry <= now + ID_TOKEN_REFRESH_WINDOW_SECONDS,
        None => true,
    }
}

fn token_expired_or_near_expiry_at(token: &str, now: i64) -> bool {
    match parse_jwt_exp(token) {
        Some(expiry) => expiry <= now + ACCESS_TOKEN_REFRESH_WINDOW_SECONDS,
        None => false,
    }
}

fn resolve_refreshed_id_token(
    current_id_token: String,
    refreshed_id_token: Option<String>,
    now: i64,
) -> Result<String> {
    match refreshed_id_token {
        Some(id_token) if id_token_needs_refresh_at(&id_token, now) => {
            anyhow::bail!("Token refresh returned an invalid or expired id_token")
        }
        Some(id_token) => Ok(id_token),
        None if id_token_needs_refresh_at(&current_id_token, now) => {
            anyhow::bail!(
                "Token refresh did not return a fresh id_token; sign in to the account again"
            )
        }
        None => Ok(current_id_token),
    }
}

fn merge_refresh_response(
    current_id_token: String,
    current_refresh_token: String,
    refreshed: RefreshTokenResponse,
    now: i64,
) -> TokenRefreshUpdate {
    let (id_token, id_token_error) =
        match resolve_refreshed_id_token(current_id_token.clone(), refreshed.id_token, now) {
            Ok(id_token) => (id_token, None),
            Err(error) => (current_id_token, Some(error)),
        };

    TokenRefreshUpdate {
        id_token,
        access_token: refreshed.access_token,
        refresh_token: refreshed.refresh_token.unwrap_or(current_refresh_token),
        id_token_error,
    }
}

fn parse_jwt_exp(token: &str) -> Option<i64> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }

    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    json.get("exp").and_then(|v| v.as_i64())
}

fn format_token_refresh_error(status: reqwest::StatusCode, body: &str) -> String {
    let parsed = serde_json::from_str::<RefreshTokenErrorResponse>(body).ok();
    let details = parsed.as_ref().and_then(|payload| payload.error.as_ref());
    let code = details.and_then(|error| error.code.as_deref());

    match code {
        Some("refresh_token_invalidated") | Some("invalid_grant") => {
            "Codex Switcher's saved session is out of date. The Codex app may still be signed in. Re-authenticate this account in Switcher only if usage or account switching actually stops working."
                .to_string()
        }
        Some("refresh_token_reused") => {
            "Codex Switcher has an outdated refresh token because Codex already rotated it. Close Codex before switching accounts, then re-add this account once if needed."
                .to_string()
        }
        _ => {
            if let Some(message) = details.and_then(|error| error.message.as_deref()) {
                format!("Could not refresh the saved Codex Switcher session ({status}). {message}")
            } else {
                format!(
                    "Could not refresh the saved Codex Switcher session ({status}). Please try again."
                )
            }
        }
    }
}

async fn refresh_tokens_with_refresh_token(refresh_token: &str) -> Result<RefreshTokenResponse> {
    refresh_tokens_with_refresh_token_at(
        refresh_token,
        &format!("{DEFAULT_ISSUER}/oauth/token"),
        Duration::from_secs(10),
    )
    .await
}

/// Refresh tokens may be rotated/revoked as soon as the server processes a
/// request. A timeout or connection drop is an *unknown* outcome: the server
/// may have returned a new token that we never received. Retrying the same
/// single-use credential can turn an otherwise healthy session into 401.
///
/// Never retry this POST automatically. Let the user re-authenticate if the
/// token was already consumed and its replacement cannot be recovered.
async fn refresh_tokens_with_refresh_token_at(
    refresh_token: &str,
    endpoint: &str,
    timeout: Duration,
) -> Result<RefreshTokenResponse> {
    let client = reqwest::Client::new();
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        urlencoding::encode(refresh_token),
        urlencoding::encode(CLIENT_ID),
    );

    let response = client
        .post(endpoint)
        .timeout(timeout)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .context(
            "Failed to send token refresh request; outcome unknown, not retrying single-use token",
        )?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        // The identity provider may echo sensitive credential material.
        eprintln!("[Auth] Token refresh failed: {status}");
        anyhow::bail!("{}", format_token_refresh_error(status, &body));
    }

    response
        .json::<RefreshTokenResponse>()
        .await
        .context("Failed to parse token refresh response")
}

#[cfg(test)]
mod tests {
    use super::{
        can_switch_with_existing_access_token, chatgpt_access_token_needs_refresh,
        chatgpt_tokens_need_refresh, chatgpt_tokens_need_refresh_at, clear_saved_session_stale,
        format_token_refresh_error, mark_saved_session_stale, merge_refresh_response,
        reconcile_active_account_from_auth, refresh_error_invalidates_saved_session,
        resolve_refreshed_id_token, should_refresh_proactively_at, take_saved_session_stale,
        token_expired_or_near_expiry_at, RefreshTokenResponse,
    };
    use crate::types::{AccountsStore, AuthData, AuthDotJson, StoredAccount, TokenData};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

    fn jwt_with_exp(exp: i64) -> String {
        let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("header.{payload}.signature")
    }

    fn account_jwt(account_id: &str, exp: i64, signature: &str) -> String {
        let payload = URL_SAFE_NO_PAD.encode(format!(
            r#"{{"exp":{exp},"https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}"}}}}"#
        ));
        format!("header.{payload}.{signature}")
    }

    #[test]
    fn jwt_access_token_refreshes_at_five_minute_boundary() {
        let now = 1_800_000_000;
        assert!(token_expired_or_near_expiry_at(
            &jwt_with_exp(now + 300),
            now
        ));
        assert!(!token_expired_or_near_expiry_at(
            &jwt_with_exp(now + 301),
            now
        ));
    }

    #[test]
    fn opaque_access_tokens_refresh_only_when_known_age_exceeds_eight_days() {
        let now = 1_800_000_000;
        let days = 24 * 60 * 60;
        assert!(!should_refresh_proactively_at("opaque", None, now));
        assert!(!should_refresh_proactively_at(
            "opaque",
            Some(now - 7 * days),
            now
        ));
        assert!(!should_refresh_proactively_at(
            "opaque",
            Some(now - 8 * days),
            now
        ));
        assert!(should_refresh_proactively_at(
            "opaque",
            Some(now - (8 * days + 1)),
            now
        ));
    }

    #[test]
    fn jwt_expiry_takes_precedence_over_refresh_age() {
        let now = 1_800_000_000;
        assert!(!should_refresh_proactively_at(
            &jwt_with_exp(now + 3600),
            Some(1),
            now
        ));
        assert!(should_refresh_proactively_at(
            &jwt_with_exp(now - 1),
            None,
            now
        ));
    }

    #[test]
    fn background_refresh_ignores_expired_id_token_when_access_token_is_valid() {
        let now = chrono::Utc::now().timestamp();
        let account = StoredAccount::new_chatgpt(
            "Background".into(),
            None,
            None,
            None,
            jwt_with_exp(now - 3_600),
            jwt_with_exp(now + 3_600),
            "refresh".into(),
            None,
        );

        assert!(!chatgpt_access_token_needs_refresh(&account));
        assert!(chatgpt_tokens_need_refresh(&account));
    }

    #[test]
    fn switch_fallback_allows_valid_access_token_when_full_refresh_fails() {
        let now = chrono::Utc::now().timestamp();
        let account = StoredAccount::new_chatgpt(
            "Switch fallback".into(),
            None,
            None,
            None,
            jwt_with_exp(now - 3_600),
            jwt_with_exp(now + 3_600),
            "stale-refresh".into(),
            None,
        );

        assert!(can_switch_with_existing_access_token(&account));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ambiguous_transport_failure_never_retries_single_use_refresh_token() {
        use tokio::net::TcpListener;
        use tokio::time::{timeout, Duration};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/oauth/token", listener.local_addr().unwrap());
        let requests = tokio::spawn(async move {
            let mut count = 0;
            while let Ok(Ok((connection, _))) =
                timeout(Duration::from_millis(400), listener.accept()).await
            {
                count += 1;
                // Drop the TCP connection after receiving it, making the POST
                // outcome ambiguous without ever receiving a token response.
                drop(connection);
            }
            count
        });

        assert!(super::refresh_tokens_with_refresh_token_at(
            "dummy-refresh-token",
            &endpoint,
            Duration::from_millis(200),
        )
        .await
        .is_err());
        assert_eq!(requests.await.unwrap(), 1);
    }

    #[test]
    fn revoked_refresh_token_must_not_be_used_as_successful_switch_fallback() {
        let now = chrono::Utc::now().timestamp();
        let account = StoredAccount::new_chatgpt(
            "Revoked".into(),
            None,
            None,
            None,
            jwt_with_exp(now - 3_600),
            jwt_with_exp(now + 3_600),
            "revoked-refresh".into(),
            None,
        );
        let revoked = anyhow::anyhow!("401 Unauthorized: refresh_token_invalidated");
        let temporary = anyhow::anyhow!("Failed to send token refresh request: timed out");
        let failed_live_write =
            anyhow::anyhow!("OAuth tokens were saved, but Codex auth.json could not be updated.");
        let invalid_response = anyhow::anyhow!("Failed to parse token refresh response");
        assert!(!super::can_fallback_after_refresh_failure(
            &revoked, &account
        ));
        assert!(super::can_fallback_after_refresh_failure(
            &temporary, &account
        ));
        assert!(!super::can_fallback_after_refresh_failure(
            &failed_live_write,
            &account
        ));
        assert!(!super::can_fallback_after_refresh_failure(
            &invalid_response,
            &account
        ));
    }

    #[test]
    fn switch_fallback_rejects_expired_access_token() {
        let now = chrono::Utc::now().timestamp();
        let account = StoredAccount::new_chatgpt(
            "Expired access".into(),
            None,
            None,
            None,
            jwt_with_exp(now - 3_600),
            jwt_with_exp(now - 3_600),
            "stale-refresh".into(),
            None,
        );

        assert!(!can_switch_with_existing_access_token(&account));
    }

    #[test]
    fn terminal_refresh_errors_mark_saved_session_as_stale() {
        let invalidated = anyhow::Error::msg(
            r#"Token refresh failed: 401 Unauthorized - {"error":{"code":"refresh_token_invalidated"}}"#,
        );
        let reused = anyhow::anyhow!(
            "Codex Switcher has an outdated refresh token because Codex already rotated it."
        );
        let invalid_refresh_token = anyhow::anyhow!(
            "Could not refresh the saved Codex Switcher session (401 Unauthorized). Invalid refresh token."
        );
        let invalid_grant = anyhow::anyhow!(
            "Could not refresh the saved Codex Switcher session (401 Unauthorized). invalid_grant"
        );
        let generic_unauthorized = anyhow::anyhow!(
            "Could not refresh the saved Codex Switcher session (401 Unauthorized). Please try again."
        );
        let transient = anyhow::anyhow!("Failed to send token refresh request: timed out");

        assert!(refresh_error_invalidates_saved_session(&invalidated));
        assert!(refresh_error_invalidates_saved_session(&reused));
        assert!(refresh_error_invalidates_saved_session(
            &invalid_refresh_token
        ));
        assert!(refresh_error_invalidates_saved_session(&invalid_grant));
        assert!(!refresh_error_invalidates_saved_session(
            &generic_unauthorized
        ));
        assert!(!refresh_error_invalidates_saved_session(&transient));
    }

    #[test]
    fn observed_stale_session_marker_is_consumed_once() {
        let account_id = "test-observed-stale-session";
        clear_saved_session_stale(account_id);
        mark_saved_session_stale(account_id);

        assert!(take_saved_session_stale(account_id));
        assert!(!take_saved_session_stale(account_id));
    }

    #[test]
    fn refresh_required_when_id_token_expired_but_access_token_valid_for_switching() {
        let now = 1_800_000_000;
        let id_token = jwt_with_exp(now - 3_600);
        let access_token = jwt_with_exp(now + 3_600);

        assert!(chatgpt_tokens_need_refresh_at(
            &id_token,
            &access_token,
            now
        ));
    }

    #[test]
    fn refresh_not_required_when_both_tokens_are_valid() {
        let now = 1_800_000_000;
        let id_token = jwt_with_exp(now + 3_600);
        let access_token = jwt_with_exp(now + 3_600);

        assert!(!chatgpt_tokens_need_refresh_at(
            &id_token,
            &access_token,
            now
        ));
    }

    #[test]
    fn refresh_required_when_access_token_expired() {
        let now = 1_800_000_000;
        let id_token = jwt_with_exp(now + 3_600);
        let access_token = jwt_with_exp(now - 3_600);

        assert!(chatgpt_tokens_need_refresh_at(
            &id_token,
            &access_token,
            now
        ));
    }

    #[test]
    fn expired_id_token_requires_replacement_from_refresh_response() {
        let now = 1_800_000_000;
        let current_id_token = jwt_with_exp(now - 3_600);

        let error = resolve_refreshed_id_token(current_id_token, None, now).unwrap_err();

        assert!(error
            .to_string()
            .contains("did not return a fresh id_token"));
    }

    #[test]
    fn valid_id_token_can_be_preserved_when_refresh_response_omits_it() {
        let now = 1_800_000_000;
        let current_id_token = jwt_with_exp(now + 3_600);

        let resolved = resolve_refreshed_id_token(current_id_token.clone(), None, now).unwrap();

        assert_eq!(resolved, current_id_token);
    }

    #[test]
    fn active_account_uses_fresh_tokens_from_live_auth() {
        let now = chrono::Utc::now().timestamp();
        let mut account = StoredAccount::new_chatgpt(
            "Active".into(),
            None,
            None,
            None,
            account_jwt("workspace-a", now - 3_600, "stored"),
            jwt_with_exp(now - 3_600),
            "stored-refresh".into(),
            Some("workspace-a".into()),
        );
        let local_id = account.id.clone();
        let mut store = AccountsStore {
            accounts: vec![account.clone()],
            active_account_id: Some(local_id.clone()),
            ..AccountsStore::default()
        };
        let auth = AuthDotJson {
            openai_api_key: None,
            tokens: Some(TokenData {
                id_token: account_jwt("workspace-a", now + 3_600, "live"),
                access_token: jwt_with_exp(now + 3_600),
                refresh_token: "live-refresh".into(),
                account_id: Some("workspace-a".into()),
            }),
            last_refresh: None,
        };

        assert!(chatgpt_tokens_need_refresh(&account));
        assert!(reconcile_active_account_from_auth(
            &mut store, &local_id, &auth
        ));

        account = store.accounts.remove(0);
        assert!(!chatgpt_tokens_need_refresh(&account));
        let AuthData::ChatGPT { refresh_token, .. } = account.auth_data else {
            panic!("expected ChatGPT account");
        };
        assert_eq!(refresh_token, "live-refresh");
    }

    #[test]
    fn inactive_account_does_not_use_live_auth() {
        let now = chrono::Utc::now().timestamp();
        let account = StoredAccount::new_chatgpt(
            "Inactive".into(),
            None,
            None,
            None,
            account_jwt("workspace-a", now - 3_600, "stored"),
            jwt_with_exp(now - 3_600),
            "stored-refresh".into(),
            Some("workspace-a".into()),
        );
        let local_id = account.id.clone();
        let mut store = AccountsStore {
            accounts: vec![account],
            active_account_id: Some("different-local-account".into()),
            ..AccountsStore::default()
        };
        let auth = AuthDotJson {
            openai_api_key: None,
            tokens: Some(TokenData {
                id_token: account_jwt("workspace-a", now + 3_600, "live"),
                access_token: jwt_with_exp(now + 3_600),
                refresh_token: "live-refresh".into(),
                account_id: Some("workspace-a".into()),
            }),
            last_refresh: None,
        };

        assert!(!reconcile_active_account_from_auth(
            &mut store, &local_id, &auth
        ));
        let AuthData::ChatGPT { refresh_token, .. } = &store.accounts[0].auth_data else {
            panic!("expected ChatGPT account");
        };
        assert_eq!(refresh_token, "stored-refresh");
    }

    #[test]
    fn rotated_refresh_token_is_retained_when_id_token_is_missing() {
        let now = 1_800_000_000;
        let current_id_token = jwt_with_exp(now - 3_600);
        let refreshed = RefreshTokenResponse {
            id_token: None,
            access_token: "new-access".into(),
            refresh_token: Some("rotated-refresh".into()),
        };

        let update = merge_refresh_response(
            current_id_token.clone(),
            "old-refresh".into(),
            refreshed,
            now,
        );

        assert_eq!(update.id_token, current_id_token);
        assert_eq!(update.refresh_token, "rotated-refresh");
        assert!(update.id_token_error.is_some());
    }

    #[test]
    fn refresh_token_invalidated_error_is_human_readable() {
        let body = r#"{"error":{"message":"Your session has ended. Please log in again.","type":"invalid_request_error","param":null,"code":"refresh_token_invalidated"}}"#;
        let message = format_token_refresh_error(reqwest::StatusCode::UNAUTHORIZED, body);

        assert!(message.contains("saved session is out of date"));
        assert!(message.contains("Codex app may still be signed in"));
        assert!(!message.contains("{\"error\""));
    }

    #[test]
    fn invalid_grant_error_is_human_readable() {
        let body = r#"{"error":{"message":"Invalid refresh token.","code":"invalid_grant"}}"#;
        let message = format_token_refresh_error(reqwest::StatusCode::UNAUTHORIZED, body);

        assert!(message.contains("saved session is out of date"));
        assert!(!message.contains("Invalid refresh token"));
    }

    #[test]
    fn refresh_token_reused_error_is_human_readable() {
        let body = r#"{"error":{"message":"already used","code":"refresh_token_reused"}}"#;
        let message = format_token_refresh_error(reqwest::StatusCode::UNAUTHORIZED, body);

        assert!(message.contains("outdated refresh token"));
        assert!(!message.contains("{\"error\""));
    }
}
