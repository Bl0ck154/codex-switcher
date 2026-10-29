//! Account storage module - manages reading and writing accounts.json

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::types::{
    parse_chatgpt_id_token_claims, AccountsStore, AppSettings, AuthData, AuthDotJson, StoredAccount,
};

pub fn sync_active_account_tokens(store: &mut AccountsStore, auth: &AuthDotJson) -> bool {
    let Some(active_id) = store.active_account_id.as_deref() else {
        return false;
    };
    let Some(tokens) = auth.tokens.as_ref() else {
        return false;
    };
    let Some(account) = store
        .accounts
        .iter_mut()
        .find(|account| account.id == active_id)
    else {
        return false;
    };
    let AuthData::ChatGPT {
        id_token,
        access_token,
        refresh_token,
        account_id,
    } = &mut account.auth_data
    else {
        return false;
    };

    let stored_account_id = parse_chatgpt_id_token_claims(id_token)
        .account_id
        .or_else(|| account_id.clone());
    let current_account_id = parse_chatgpt_id_token_claims(&tokens.id_token)
        .account_id
        .or_else(|| tokens.account_id.clone());
    let (Some(stored_account_id), Some(current_account_id)) =
        (stored_account_id, current_account_id)
    else {
        return false;
    };
    if stored_account_id != current_account_id {
        return false;
    }

    // The live file can be older than the last single-use refresh token
    // persisted in accounts.json. Never move saved credentials backwards.
    if runtime_snapshot_is_older(account.last_refresh_at, auth.last_refresh) {
        return false;
    }

    let changed = *id_token != tokens.id_token
        || *access_token != tokens.access_token
        || *refresh_token != tokens.refresh_token
        || account_id.as_ref() != Some(&current_account_id)
        || account.last_refresh_at != auth.last_refresh;
    if !changed {
        return false;
    }

    id_token.clone_from(&tokens.id_token);
    access_token.clone_from(&tokens.access_token);
    refresh_token.clone_from(&tokens.refresh_token);
    *account_id = Some(current_account_id);
    account.last_refresh_at = auth.last_refresh;
    true
}

fn runtime_snapshot_is_older(
    stored_generation: Option<DateTime<Utc>>,
    runtime_generation: Option<DateTime<Utc>>,
) -> bool {
    match (stored_generation, runtime_generation) {
        (Some(stored), Some(runtime)) => runtime < stored,
        // Old files may have no generation; allow initial migration, but
        // never overwrite credentials with a known generation using them.
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Get the path to the codex-switcher config directory
pub fn get_config_dir() -> Result<PathBuf> {
    #[cfg(test)]
    {
        Ok(super::test_support::home_dir()?.join(".codex-switcher"))
    }

    #[cfg(not(test))]
    {
        let home = dirs::home_dir().context("Could not find home directory")?;
        Ok(home.join(".codex-switcher"))
    }
}

/// Get the path to accounts.json
pub fn get_accounts_file() -> Result<PathBuf> {
    Ok(get_config_dir()?.join("accounts.json"))
}

pub fn get_settings_file() -> Result<PathBuf> {
    Ok(get_config_dir()?.join("settings.json"))
}

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct MutationLock {
    _file: File,
}

impl Drop for MutationLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

pub(crate) fn acquire_mutation_lock(lock_name: &str) -> Result<MutationLock> {
    let config_dir = get_config_dir()?;
    fs::create_dir_all(&config_dir).with_context(|| {
        format!(
            "Failed to create config directory: {}",
            config_dir.display()
        )
    })?;
    acquire_mutation_lock_at(&config_dir.join(lock_name))
}

pub(crate) fn acquire_mutation_lock_at(path: &Path) -> Result<MutationLock> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("Failed to open mutation lock: {}", path.display()))?;

        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("Failed to acquire mutation lock: {}", path.display()));
        }
        return Ok(MutationLock { _file: file });
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .share_mode(0)
                .open(path)
            {
                Ok(file) => return Ok(MutationLock { _file: file }),
                Err(error) if std::time::Instant::now() < deadline => {
                    let _ = error;
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("Timed out waiting for mutation lock: {}", path.display())
                    });
                }
            }
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("Failed to open mutation lock: {}", path.display()))?;
        Ok(MutationLock { _file: file })
    }
}

pub(crate) fn write_file_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    write_file_atomic_with_pre_replace(path, bytes, || Ok(()))
}

fn write_file_atomic_with_pre_replace(
    path: &Path,
    bytes: &[u8],
    before_replace: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let parent = path
        .parent()
        .context("Atomic write target has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create directory: {}", parent.display()))?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("Atomic write target has an invalid file name")?;
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp_path = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        sequence
    ));

    let mut options = OpenOptions::new();
    options.create_new(true).write(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options
        .open(&temp_path)
        .with_context(|| format!("Failed to create temporary file: {}", temp_path.display()))?;

    let write_result = (|| -> Result<()> {
        file.write_all(bytes)
            .with_context(|| format!("Failed to write temporary file: {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to sync temporary file: {}", temp_path.display()))?;
        drop(file);

        before_replace()?;
        replace_file(&temp_path, path)?;

        #[cfg(unix)]
        {
            let directory = File::open(parent)
                .with_context(|| format!("Failed to open directory: {}", parent.display()))?;
            directory
                .sync_all()
                .with_context(|| format!("Failed to sync directory: {}", parent.display()))?;
        }

        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }

    write_result
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).with_context(|| {
        format!(
            "Failed to atomically replace {} with {}",
            to.display(),
            from.display()
        )
    })
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let from_wide: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to_wide: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // Antivirus and background readers can temporarily deny replacing a file
    // on Windows. Preserve the atomic move and retry only common sharing/lock
    // errors (plus access denied, often returned by file scanners).
    const RETRY_DELAYS_MS: [u64; 4] = [25, 50, 100, 200];
    for attempt in 0..=RETRY_DELAYS_MS.len() {
        let result = unsafe {
            MoveFileExW(
                from_wide.as_ptr(),
                to_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if result != 0 {
            return Ok(());
        }

        let error = std::io::Error::last_os_error();
        let can_retry = matches!(error.raw_os_error(), Some(5 | 32 | 33));
        if !can_retry || attempt == RETRY_DELAYS_MS.len() {
            return Err(error).with_context(|| {
                format!(
                    "Failed to atomically replace {} with {} after {} attempt(s)",
                    to.display(),
                    from.display(),
                    attempt + 1
                )
            });
        }
        std::thread::sleep(std::time::Duration::from_millis(RETRY_DELAYS_MS[attempt]));
    }
    unreachable!("replace either succeeds or returns its last error")
}

/// Load the accounts store from disk
pub fn load_accounts() -> Result<AccountsStore> {
    let path = get_accounts_file()?;

    if !path.exists() {
        return Ok(AccountsStore::default());
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read accounts file: {}", path.display()))?;

    let store: AccountsStore = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse accounts file: {}", path.display()))?;

    Ok(store)
}

pub fn load_app_settings() -> Result<AppSettings> {
    let path = get_settings_file()?;

    if !path.exists() {
        return Ok(AppSettings::default());
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read settings file: {}", path.display()))?;

    let settings: AppSettings = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse settings file: {}", path.display()))?;

    Ok(settings)
}

pub fn save_app_settings(settings: &AppSettings) -> Result<()> {
    let _lock = acquire_mutation_lock("settings.lock")?;
    save_app_settings_unlocked(settings)
}

fn save_app_settings_unlocked(settings: &AppSettings) -> Result<()> {
    let path = get_settings_file()?;
    let content = serde_json::to_vec_pretty(settings).context("Failed to serialize settings")?;
    write_file_atomic(&path, &content)
}

pub fn mutate_app_settings<T>(mutate: impl FnOnce(&mut AppSettings) -> Result<T>) -> Result<T> {
    let _lock = acquire_mutation_lock("settings.lock")?;
    let mut settings = load_app_settings()?;
    let result = mutate(&mut settings)?;
    save_app_settings_unlocked(&settings)?;
    Ok(result)
}

/// Save the accounts store to disk
pub fn save_accounts(store: &AccountsStore) -> Result<()> {
    let _lock = acquire_mutation_lock("accounts.lock")?;
    save_accounts_unlocked(store)
}

fn save_accounts_unlocked(store: &AccountsStore) -> Result<()> {
    let path = get_accounts_file()?;
    let content = serde_json::to_vec_pretty(store).context("Failed to serialize accounts store")?;
    write_file_atomic(&path, &content)
}

/// Apply one logical account-store mutation and persist its resulting snapshot.
/// This boundary owns the complete read-modify-write transaction.
pub fn mutate_accounts<T>(mutate: impl FnOnce(&mut AccountsStore) -> Result<T>) -> Result<T> {
    let _lock = acquire_mutation_lock("accounts.lock")?;
    let mut store = load_accounts()?;
    let result = mutate(&mut store)?;
    save_accounts_unlocked(&store)?;
    Ok(result)
}

pub const ACCOUNT_REPLACE_CONFIRMATION_PREFIX: &str = "ACCOUNT_REPLACE_CONFIRMATION_REQUIRED:";

/// Workspace identity is not an email, plan, or user-assigned name.
/// JWT claims take precedence over a stale cached account ID.
pub fn chatgpt_workspace_id(account: &StoredAccount) -> Option<String> {
    let AuthData::ChatGPT {
        id_token,
        account_id,
        ..
    } = &account.auth_data
    else {
        return None;
    };
    parse_chatgpt_id_token_claims(id_token)
        .account_id
        .or_else(|| account_id.clone())
        .filter(|id| !id.trim().is_empty())
}

pub fn same_saved_account(existing: &StoredAccount, candidate: &StoredAccount) -> bool {
    match (&existing.auth_data, &candidate.auth_data) {
        (AuthData::ChatGPT { .. }, AuthData::ChatGPT { .. }) => {
            match (
                chatgpt_workspace_id(existing),
                chatgpt_workspace_id(candidate),
            ) {
                (Some(a), Some(b)) => a == b,
                _ => false, // Missing identity must never authorize overwriting credentials.
            }
        }
        (AuthData::ApiKey { .. }, AuthData::ApiKey { .. }) => existing.name == candidate.name,
        _ => false,
    }
}

/// A visual label is not an account identity. Keep separate workspace sessions.
fn disambiguate_account_name(store: &AccountsStore, account: &mut StoredAccount) {
    if !store
        .accounts
        .iter()
        .any(|saved| saved.name == account.name)
    {
        return;
    }
    let base = account.name.clone();
    let plan = account
        .plan_type
        .as_deref()
        .map(str::trim)
        .filter(|plan| !plan.is_empty())
        .unwrap_or("workspace");
    let mut candidate = format!("{base} ({plan})");
    let mut number = 2;
    while store.accounts.iter().any(|saved| saved.name == candidate) {
        candidate = format!("{base} ({plan} {number})");
        number += 1;
    }
    account.name = candidate;
}

fn replace_account_in_store(
    store: &mut AccountsStore,
    existing_index: usize,
    mut replacement: StoredAccount,
) -> StoredAccount {
    let existing = &store.accounts[existing_index];
    replacement.id = existing.id.clone();
    replacement.name = existing.name.clone(); // Preserve custom name on re-login.
    replacement.created_at = existing.created_at;
    replacement.last_used_at = existing.last_used_at;
    // Only OAuth credential replacements get a new refresh generation.
    // API-key accounts do not use token rotation.
    replacement.last_refresh_at = match &replacement.auth_data {
        AuthData::ChatGPT { .. } => Some(Utc::now()),
        AuthData::ApiKey { .. } => None,
    };
    store.accounts[existing_index] = replacement.clone();
    replacement
}

/// Replace the same ChatGPT workspace only, never a different workspace with the same name.
pub fn add_or_replace_account(
    account: StoredAccount,
    force_replace: bool,
) -> Result<StoredAccount> {
    mutate_accounts(|store| add_or_replace_account_to_store(store, account, force_replace))
}

fn add_or_replace_account_to_store(
    store: &mut AccountsStore,
    mut account: StoredAccount,
    force_replace: bool,
) -> Result<StoredAccount> {
    if let Some(index) = store
        .accounts
        .iter()
        .position(|saved| same_saved_account(saved, &account))
    {
        if !force_replace {
            anyhow::bail!(
                "{ACCOUNT_REPLACE_CONFIRMATION_PREFIX}{}",
                store.accounts[index].name
            );
        }
        return Ok(replace_account_in_store(store, index, account));
    }

    disambiguate_account_name(store, &mut account);
    let saved = account.clone();
    store.accounts.push(account);
    // New profiles are saved inactive; explicit Switch changes Codex auth.json.
    Ok(saved)
}

/// Preserve the non-replacement path for adding new accounts.
pub fn add_account(account: StoredAccount) -> Result<StoredAccount> {
    mutate_accounts(|store| {
        if store
            .accounts
            .iter()
            .any(|saved| same_saved_account(saved, &account))
        {
            anyhow::bail!("This account/workspace is already saved");
        }
        add_or_replace_account_to_store(store, account, false)
    })
}

/// Remove an account by ID
pub fn remove_account(account_id: &str) -> Result<()> {
    mutate_accounts(|store| {
        let initial_len = store.accounts.len();
        store.accounts.retain(|a| a.id != account_id);

        if store.accounts.len() == initial_len {
            anyhow::bail!("Account not found: {account_id}");
        }

        if store.active_account_id.as_deref() == Some(account_id) {
            store.active_account_id = store.accounts.first().map(|account| account.id.clone());
        }

        Ok(())
    })
}

/// Update the active account ID
pub fn set_active_account(account_id: &str) -> Result<()> {
    mutate_accounts(|store| {
        if !store.accounts.iter().any(|a| a.id == account_id) {
            anyhow::bail!("Account not found: {account_id}");
        }
        store.active_account_id = Some(account_id.to_string());
        Ok(())
    })
}

/// Get an account by ID
pub fn get_account(account_id: &str) -> Result<Option<StoredAccount>> {
    let store = load_accounts()?;
    Ok(store.accounts.into_iter().find(|a| a.id == account_id))
}

/// Get the currently active account
pub fn get_active_account() -> Result<Option<StoredAccount>> {
    let store = load_accounts()?;
    let active_id = match &store.active_account_id {
        Some(id) => id,
        None => return Ok(None),
    };
    Ok(store.accounts.into_iter().find(|a| a.id == *active_id))
}

/// Update an account's last_used_at timestamp
pub fn touch_account(account_id: &str) -> Result<()> {
    mutate_accounts(|store| {
        if let Some(account) = store.accounts.iter_mut().find(|a| a.id == account_id) {
            account.last_used_at = Some(chrono::Utc::now());
        }
        Ok(())
    })
}

/// Update an account's metadata (name, email, plan_type, subscription expiry)
pub fn update_account_metadata(
    account_id: &str,
    name: Option<String>,
    email: Option<String>,
    plan_type: Option<String>,
    subscription_expires_at: Option<Option<DateTime<Utc>>>,
) -> Result<StoredAccount> {
    mutate_accounts(|store| {
        if let Some(ref new_name) = name {
            if store
                .accounts
                .iter()
                .any(|a| a.id != account_id && a.name == *new_name)
            {
                anyhow::bail!("An account with name '{new_name}' already exists");
            }
        }

        let account = store
            .accounts
            .iter_mut()
            .find(|a| a.id == account_id)
            .context("Account not found")?;

        if let Some(new_name) = name {
            account.name = new_name;
        }
        if let Some(new_email) = email {
            account.email = Some(new_email);
        }
        if let Some(new_plan_type) = plan_type {
            account.plan_type = Some(new_plan_type);
        }
        if let Some(subscription_expires_at) = subscription_expires_at {
            account.subscription_expires_at = subscription_expires_at;
        }

        Ok(account.clone())
    })
}

/// Update ChatGPT OAuth tokens for an account and return the updated account.
pub fn update_account_chatgpt_tokens(
    account_id: &str,
    id_token: String,
    access_token: String,
    refresh_token: String,
    chatgpt_account_id: Option<String>,
    email: Option<String>,
    plan_type: Option<String>,
    subscription_expires_at: Option<DateTime<Utc>>,
) -> Result<StoredAccount> {
    mutate_accounts(|store| {
        let account = store
            .accounts
            .iter_mut()
            .find(|a| a.id == account_id)
            .context("Account not found")?;

        match &mut account.auth_data {
            AuthData::ChatGPT {
                id_token: stored_id_token,
                access_token: stored_access_token,
                refresh_token: stored_refresh_token,
                account_id: stored_account_id,
            } => {
                *stored_id_token = id_token;
                *stored_access_token = access_token;
                *stored_refresh_token = refresh_token;
                if let Some(new_account_id) = chatgpt_account_id {
                    *stored_account_id = Some(new_account_id);
                }
            }
            AuthData::ApiKey { .. } => {
                anyhow::bail!("Cannot update OAuth tokens for an API key account");
            }
        }

        if let Some(new_email) = email {
            account.email = Some(new_email);
        }
        if let Some(new_plan_type) = plan_type {
            account.plan_type = Some(new_plan_type);
        }
        if let Some(subscription_expires_at) = subscription_expires_at {
            account.subscription_expires_at = Some(subscription_expires_at);
        }

        account.last_refresh_at = Some(Utc::now());
        Ok(account.clone())
    })
}

/// Get the list of masked account IDs
pub fn get_masked_account_ids() -> Result<Vec<String>> {
    let store = load_accounts()?;
    Ok(store.masked_account_ids.clone())
}

/// Set the list of masked account IDs
pub fn set_masked_account_ids(ids: Vec<String>) -> Result<()> {
    mutate_accounts(|store| {
        store.masked_account_ids = ids;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::{
        acquire_mutation_lock_at, add_or_replace_account_to_store, chatgpt_workspace_id,
        replace_account_in_store, same_saved_account, sync_active_account_tokens,
        write_file_atomic, write_file_atomic_with_pre_replace,
    };
    use crate::types::{AccountsStore, AuthData, AuthDotJson, StoredAccount, TokenData};
    use base64::Engine;
    use chrono::{Duration, Utc};

    #[test]
    fn mutation_lock_serializes_competing_writers() {
        let dir = std::env::temp_dir().join(format!(
            "codex-switcher-mutation-lock-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.lock");

        let first = acquire_mutation_lock_at(&path).unwrap();
        let second_path = path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let second = acquire_mutation_lock_at(&second_path).unwrap();
            tx.send(()).unwrap();
            drop(second);
        });

        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(rx.try_recv().is_err());

        drop(first);
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        worker.join().unwrap();

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn atomic_write_preserves_old_file_when_pre_replace_step_fails() {
        let dir = std::env::temp_dir().join(format!(
            "codex-switcher-atomic-failure-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(&path, b"old").unwrap();

        let result = write_file_atomic_with_pre_replace(&path, b"new", || {
            anyhow::bail!("injected failure before replacement")
        });

        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn atomic_write_replaces_existing_file() {
        let dir = std::env::temp_dir().join(format!(
            "codex-switcher-atomic-write-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(&path, b"old").unwrap();

        write_file_atomic(&path, b"new").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_secret_write_has_restrictive_permissions_from_creation() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "codex-switcher-atomic-permissions-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");

        write_file_atomic(&path, br#"{"tokens":{}}"#).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        std::fs::remove_dir_all(dir).unwrap();
    }

    fn account(name: &str, account_id: &str, suffix: &str) -> StoredAccount {
        StoredAccount::new_chatgpt(
            name.into(),
            None,
            None,
            None,
            format!("id-{suffix}"),
            format!("access-{suffix}"),
            format!("refresh-{suffix}"),
            Some(account_id.into()),
        )
    }

    fn auth(account_id: &str, suffix: &str) -> AuthDotJson {
        AuthDotJson {
            openai_api_key: None,
            tokens: Some(TokenData {
                id_token: format!("id-{suffix}"),
                access_token: format!("access-{suffix}"),
                refresh_token: format!("refresh-{suffix}"),
                account_id: Some(account_id.into()),
            }),
            last_refresh: None,
        }
    }

    fn refresh_token(account: &StoredAccount) -> &str {
        match &account.auth_data {
            AuthData::ChatGPT { refresh_token, .. } => refresh_token,
            AuthData::ApiKey { .. } => panic!("expected ChatGPT account"),
        }
    }

    fn id_token_with_account_id(account_id: &str, suffix: &str) -> String {
        let payload =
            format!(r#"{{"https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}"}}}}"#);
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        format!("header.{encoded}.{suffix}")
    }

    #[test]
    fn stale_runtime_generation_does_not_undo_rotated_refresh_token() {
        let mut saved = account("A", "workspace-a", "new");
        let generation = Utc::now();
        saved.last_refresh_at = Some(generation);
        let mut store = AccountsStore {
            active_account_id: Some(saved.id.clone()),
            accounts: vec![saved],
            ..AccountsStore::default()
        };

        let mut live = auth("workspace-a", "old");
        live.last_refresh = Some(generation - Duration::minutes(5));
        assert!(!sync_active_account_tokens(&mut store, &live));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-new");
        assert_eq!(store.accounts[0].last_refresh_at, Some(generation));

        // A legacy runtime snapshot without a generation is also stale.
        live.last_refresh = None;
        assert!(!sync_active_account_tokens(&mut store, &live));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-new");
    }

    #[test]
    fn newer_runtime_generation_reconciles_rotated_credentials() {
        let mut saved = account("A", "workspace-a", "old");
        let generation = Utc::now();
        saved.last_refresh_at = Some(generation);
        let mut store = AccountsStore {
            active_account_id: Some(saved.id.clone()),
            accounts: vec![saved],
            ..AccountsStore::default()
        };

        let mut live = auth("workspace-a", "new");
        let newer = generation + Duration::seconds(20);
        live.last_refresh = Some(newer);
        assert!(sync_active_account_tokens(&mut store, &live));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-new");
        assert_eq!(store.accounts[0].last_refresh_at, Some(newer));
    }

    #[test]
    fn replacing_api_key_does_not_add_oauth_generation() {
        let existing = StoredAccount::new_api_key("API".into(), "old-key".into());
        let mut store = AccountsStore {
            accounts: vec![existing],
            ..AccountsStore::default()
        };
        let updated = replace_account_in_store(
            &mut store,
            0,
            StoredAccount::new_api_key("API".into(), "new-key".into()),
        );
        assert!(updated.last_refresh_at.is_none());
    }

    #[test]
    fn legacy_account_json_without_generation_remains_compatible() {
        let saved = account("A", "workspace-a", "old");
        let mut value = serde_json::to_value(&saved).unwrap();
        value.as_object_mut().unwrap().remove("last_refresh_at");
        let parsed: StoredAccount = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.last_refresh_at, None);
        assert_eq!(refresh_token(&parsed), "refresh-old");
    }

    #[test]
    fn refreshed_login_is_newer_than_previous_live_snapshot() {
        let mut existing = account("A", "workspace-a", "old");
        existing.last_refresh_at = Some(Utc::now() - Duration::hours(2));
        let mut store = AccountsStore {
            active_account_id: Some(existing.id.clone()),
            accounts: vec![existing],
            ..AccountsStore::default()
        };
        let replacement = account("A", "workspace-a", "new");
        let stored = replace_account_in_store(&mut store, 0, replacement);
        assert!(stored.last_refresh_at.is_some());
        let mut live = auth("workspace-a", "old");
        live.last_refresh = Some(Utc::now() - Duration::minutes(30));
        assert!(!sync_active_account_tokens(&mut store, &live));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-new");
    }

    #[test]
    fn preserves_rotated_tokens_before_switching_away_and_back() {
        let account_a = account("A", "workspace-a", "a1");
        let account_a_id = account_a.id.clone();
        let account_b = account("B", "workspace-b", "b1");
        let account_b_id = account_b.id.clone();
        let mut store = AccountsStore {
            accounts: vec![account_a, account_b],
            active_account_id: Some(account_a_id.clone()),
            ..AccountsStore::default()
        };

        assert!(!sync_active_account_tokens(
            &mut store,
            &auth("workspace-b", "wrong-account")
        ));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-a1");

        let mut auth_without_top_level_id = auth("workspace-b", "missing-id");
        let tokens = auth_without_top_level_id.tokens.as_mut().unwrap();
        tokens.id_token = id_token_with_account_id("workspace-b", "signature");
        tokens.account_id = None;
        assert!(!sync_active_account_tokens(
            &mut store,
            &auth_without_top_level_id
        ));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-a1");

        let mut auth_without_identity = auth("workspace-a", "unknown");
        auth_without_identity.tokens.as_mut().unwrap().account_id = None;
        assert!(!sync_active_account_tokens(
            &mut store,
            &auth_without_identity
        ));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-a1");

        assert!(sync_active_account_tokens(
            &mut store,
            &auth("workspace-a", "a2")
        ));
        store.active_account_id = Some(account_b_id);

        let restored_a = store
            .accounts
            .iter()
            .find(|account| account.id == account_a_id)
            .unwrap();
        let AuthData::ChatGPT { refresh_token, .. } = &restored_a.auth_data else {
            panic!("expected ChatGPT account");
        };
        assert_eq!(refresh_token, "refresh-a2");
    }

    #[test]
    fn rejects_live_tokens_when_stored_account_identity_is_unknown() {
        let mut account = account("A", "workspace-a", "a1");
        let account_id = account.id.clone();
        let AuthData::ChatGPT {
            id_token,
            account_id: chatgpt_account_id,
            ..
        } = &mut account.auth_data
        else {
            panic!("expected ChatGPT account");
        };
        *id_token = "opaque-id-token".into();
        *chatgpt_account_id = None;

        let mut store = AccountsStore {
            accounts: vec![account],
            active_account_id: Some(account_id),
            ..AccountsStore::default()
        };

        assert!(!sync_active_account_tokens(
            &mut store,
            &auth("workspace-a", "a2")
        ));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-a1");
    }

    #[test]
    fn derives_stored_identity_from_id_token_and_backfills_account_id() {
        let mut account = account("A", "workspace-a", "a1");
        let account_id = account.id.clone();
        let AuthData::ChatGPT {
            id_token,
            account_id: chatgpt_account_id,
            ..
        } = &mut account.auth_data
        else {
            panic!("expected ChatGPT account");
        };
        *id_token = id_token_with_account_id("workspace-a", "stored");
        *chatgpt_account_id = None;

        let mut store = AccountsStore {
            accounts: vec![account],
            active_account_id: Some(account_id),
            ..AccountsStore::default()
        };

        assert!(sync_active_account_tokens(
            &mut store,
            &auth("workspace-a", "a2")
        ));
        let AuthData::ChatGPT { account_id, .. } = &store.accounts[0].auth_data else {
            panic!("expected ChatGPT account");
        };
        assert_eq!(account_id.as_deref(), Some("workspace-a"));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-a2");
    }
    #[test]
    fn replacing_duplicate_preserves_internal_identity_and_history() {
        let mut existing = account("A", "workspace-a", "old");
        existing.last_used_at = Some(Utc::now() - Duration::hours(2));
        let existing_id = existing.id.clone();
        let existing_created_at = existing.created_at;
        let existing_last_used_at = existing.last_used_at;

        let replacement = account("A", "workspace-a", "new");
        let mut store = AccountsStore {
            accounts: vec![existing],
            active_account_id: Some(existing_id.clone()),
            ..AccountsStore::default()
        };

        let replaced = replace_account_in_store(&mut store, 0, replacement);

        assert_eq!(replaced.id, existing_id);
        assert_eq!(replaced.created_at, existing_created_at);
        assert_eq!(replaced.last_used_at, existing_last_used_at);
        assert_eq!(
            store.active_account_id.as_deref(),
            Some(replaced.id.as_str())
        );
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-new");
    }
    #[test]
    fn personal_and_business_with_same_email_keep_separate_sessions() {
        let mut personal = account("user@example.com", "personal-workspace", "personal");
        personal.email = Some("user@example.com".into());
        personal.plan_type = Some("plus".into());
        let personal_id = personal.id.clone();
        let mut business = account("user@example.com", "business-workspace", "business");
        business.email = personal.email.clone();
        business.plan_type = Some("business".into());
        let mut store = AccountsStore {
            accounts: vec![personal],
            active_account_id: Some(personal_id.clone()),
            ..AccountsStore::default()
        };

        // Even a confirmed replacement cannot overwrite a different workspace.
        let saved = add_or_replace_account_to_store(&mut store, business, true).unwrap();
        assert_eq!(store.accounts.len(), 2);
        assert_eq!(saved.name, "user@example.com (business)");
        assert_eq!(
            store.active_account_id.as_deref(),
            Some(personal_id.as_str())
        );
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-personal");
        assert_eq!(refresh_token(&store.accounts[1]), "refresh-business");
    }

    #[test]
    fn two_business_workspaces_same_email_and_plan_are_distinct() {
        let mut store = AccountsStore::default();
        for n in 1..=3 {
            let mut candidate = account(
                "shared@example.com",
                &format!("workspace-{n}"),
                &n.to_string(),
            );
            candidate.plan_type = Some("business".into());
            add_or_replace_account_to_store(&mut store, candidate, false).unwrap();
        }
        assert_eq!(store.accounts.len(), 3);
        assert_eq!(store.accounts[1].name, "shared@example.com (business)");
        assert_eq!(store.accounts[2].name, "shared@example.com (business 2)");
    }

    #[test]
    fn relogin_same_workspace_preserves_internal_id_and_custom_name() {
        let original = account("My work", "workspace-one", "old");
        let original_id = original.id.clone();
        let mut store = AccountsStore {
            accounts: vec![original],
            ..AccountsStore::default()
        };
        let renewed = account("user@example.com", "workspace-one", "new");
        assert!(same_saved_account(&store.accounts[0], &renewed));
        assert!(add_or_replace_account_to_store(&mut store, renewed.clone(), false).is_err());
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-old");
        let saved = add_or_replace_account_to_store(&mut store, renewed, true).unwrap();
        assert_eq!(store.accounts.len(), 1);
        assert_eq!(saved.id, original_id);
        assert_eq!(saved.name, "My work");
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-new");
    }

    #[test]
    fn unknown_workspace_identity_is_never_overwritten_by_name() {
        let mut old = account("same@example.com", "old", "old");
        let mut new = account("same@example.com", "new", "new");
        for candidate in [&mut old, &mut new] {
            candidate.auth_data = AuthData::ChatGPT {
                id_token: "opaque".into(),
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                account_id: None,
            };
        }
        assert!(chatgpt_workspace_id(&old).is_none());
        assert!(!same_saved_account(&old, &new));
        let mut store = AccountsStore {
            accounts: vec![old],
            ..AccountsStore::default()
        };
        add_or_replace_account_to_store(&mut store, new, true).unwrap();
        assert_eq!(store.accounts.len(), 2);
    }

    #[test]
    fn token_claim_precedes_stale_denormalized_workspace_id() {
        let mut existing = account("email@example.com", "stale", "a");
        if let AuthData::ChatGPT { id_token, .. } = &mut existing.auth_data {
            *id_token = id_token_with_account_id("actual", "signed");
        }
        assert!(same_saved_account(
            &existing,
            &account("email@example.com", "actual", "b")
        ));
    }

    #[test]
    fn adding_first_account_does_not_activate_it() {
        let mut store = AccountsStore::default();
        add_or_replace_account_to_store(&mut store, account("first", "ws-first", "one"), false)
            .unwrap();
        assert_eq!(store.accounts.len(), 1);
        assert!(store.active_account_id.is_none());
    }
}
