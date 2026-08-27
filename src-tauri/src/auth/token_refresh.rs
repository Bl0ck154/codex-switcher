//! ChatGPT OAuth token refresh helpers

use anyhow::{Context, Result};
use base64::Engine;
use chrono::Utc;
use tokio::time::{sleep, Duration};

use super::{load_accounts, switch_to_account, update_account_chatgpt_tokens, AUTH_OPERATION_LOCK};
use crate::types::{parse_chatgpt_id_token_claims, AuthData, StoredAccount};

const DEFAULT_ISSUER: &str = "https://auth.openai.com";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const EXPIRY_SKEW_SECONDS: i64 = 60;

#[derive(Debug, serde::Deserialize)]
struct RefreshTokenResponse {
    #[serde(default)]
    id_token: Option<String>,
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Debug)]
struct TokenRefreshUpdate {
    id_token: String,
    access_token: String,
    refresh_token: String,
    id_token_error: Option<anyhow::Error>,
}

/// Ensure the account has non-expired ChatGPT OAuth tokens.
/// Returns an updated account when a refresh was performed.
pub async fn ensure_chatgpt_tokens_fresh(account: &StoredAccount) -> Result<StoredAccount> {
    if !chatgpt_tokens_need_refresh(account) {
        return Ok(account.clone());
    }

    let _auth_guard = AUTH_OPERATION_LOCK.lock().await;
    ensure_chatgpt_tokens_fresh_locked(account).await
}

/// Ensure ChatGPT OAuth tokens are fresh while the caller holds AUTH_OPERATION_LOCK.
pub(crate) async fn ensure_chatgpt_tokens_fresh_locked(
    account: &StoredAccount,
) -> Result<StoredAccount> {
    if matches!(account.auth_data, AuthData::ApiKey { .. }) {
        return Ok(account.clone());
    }

    // The account may have been refreshed while this task waited for the lock.
    let current = load_accounts()?
        .accounts
        .into_iter()
        .find(|stored| stored.id == account.id)
        .context("Account not found")?;

    match &current.auth_data {
        AuthData::ApiKey { .. } => Ok(current.clone()),
        AuthData::ChatGPT {
            id_token,
            access_token,
            ..
        } => {
            if chatgpt_tokens_need_refresh_at(id_token, access_token, Utc::now().timestamp()) {
                println!(
                    "[Auth] OAuth token expired/near expiry for account {}, refreshing",
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

    let _auth_guard = AUTH_OPERATION_LOCK.lock().await;
    refresh_chatgpt_tokens_locked(account).await
}

async fn refresh_chatgpt_tokens_locked(account: &StoredAccount) -> Result<StoredAccount> {
    let current = load_accounts()?
        .accounts
        .into_iter()
        .find(|stored| stored.id == account.id)
        .context("Account not found")?;
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

    let is_active = load_accounts()?.active_account_id.as_deref() == Some(account.id.as_str());
    if is_active && crate::commands::process::ensure_codex_not_running().is_err() {
        anyhow::bail!(
            "Cannot refresh the active account while Codex/ChatGPT is running; let the running app refresh its session"
        );
    }

    let refreshed = refresh_tokens_with_refresh_token(&current_refresh_token).await?;
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

    // Refresh tokens can be single-use. Persist a rotated replacement before
    // reporting an unusable ID token, so a later retry can still recover.
    if let Some(error) = next.id_token_error {
        return Err(error);
    }

    // Re-read active state after the network request before touching auth.json.
    let is_active = load_accounts()?.active_account_id.as_deref() == Some(account.id.as_str());
    if is_active {
        if let Err(err) = switch_to_account(&updated) {
            println!("[Auth] Failed to sync active auth.json after token refresh: {err}");
        }
    }

    Ok(updated)
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
        Some(expiry) => expiry <= now + EXPIRY_SKEW_SECONDS,
        None => true,
    }
}

fn token_expired_or_near_expiry_at(token: &str, now: i64) -> bool {
    match parse_jwt_exp(token) {
        Some(expiry) => expiry <= now + EXPIRY_SKEW_SECONDS,
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

async fn refresh_tokens_with_refresh_token(refresh_token: &str) -> Result<RefreshTokenResponse> {
    let client = reqwest::Client::new();
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        urlencoding::encode(refresh_token),
        urlencoding::encode(CLIENT_ID),
    );

    let mut last_send_error = None;
    let mut response = None;

    for attempt in 1..=3u8 {
        match client
            .post(format!("{DEFAULT_ISSUER}/oauth/token"))
            .timeout(Duration::from_secs(10))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body.clone())
            .send()
            .await
        {
            Ok(resp) => {
                response = Some(resp);
                break;
            }
            Err(err) => {
                last_send_error = Some(err);
                if attempt < 3 {
                    sleep(Duration::from_millis(250 * u64::from(attempt))).await;
                }
            }
        }
    }

    let response = match response {
        Some(resp) => resp,
        None => {
            let err = last_send_error.context("Failed to send token refresh request")?;
            return Err(err.into());
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Token refresh failed: {status} - {body}");
    }

    response
        .json::<RefreshTokenResponse>()
        .await
        .context("Failed to parse token refresh response")
}

#[cfg(test)]
mod tests {
    use super::{
        chatgpt_tokens_need_refresh_at, merge_refresh_response, resolve_refreshed_id_token,
        RefreshTokenResponse,
    };
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

    fn jwt_with_exp(exp: i64) -> String {
        let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("header.{payload}.signature")
    }

    #[test]
    fn refresh_required_when_id_token_expired_but_access_token_valid() {
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
}
