//! Authentication module

pub mod oauth_server;
pub mod storage;
pub mod switcher;
pub mod token_refresh;

#[cfg(test)]
pub(crate) mod test_support;

// Within one process, the Tokio mutex orders credentials operations. The
// additional filesystem lock serializes desktop + web server processes, too.
static AUTH_OPERATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) struct AuthOperationGuard {
    _local: tokio::sync::MutexGuard<'static, ()>,
    _file: storage::MutationLock,
}

/// Always acquire the auth-operation lock before accounts.lock or auth.lock.
/// Obtain the filesystem path on the caller's thread to retain isolated
/// test homes; wait for the OS lock on a blocking thread, off the runtime.
pub(crate) async fn acquire_auth_operation_lock() -> anyhow::Result<AuthOperationGuard> {
    use anyhow::Context;

    let local = AUTH_OPERATION_LOCK.lock().await;
    let directory = storage::get_config_dir()?;
    std::fs::create_dir_all(&directory)?;
    let path = directory.join("auth-operation.lock");
    let file = tokio::task::spawn_blocking(move || storage::acquire_mutation_lock_at(&path))
        .await
        .context("Auth-operation lock worker terminated")??;
    Ok(AuthOperationGuard {
        _local: local,
        _file: file,
    })
}

#[cfg(test)]
mod auth_operation_tests {
    use super::acquire_auth_operation_lock;
    use super::test_support::TestHome;
    use tokio::time::{timeout, Duration};

    #[tokio::test(flavor = "current_thread")]
    async fn second_auth_operation_waits_until_first_releases() {
        let _home = TestHome::new();
        let first = acquire_auth_operation_lock().await.unwrap();
        assert!(timeout(Duration::from_millis(60), acquire_auth_operation_lock())
            .await
            .is_err());
        drop(first);
        let second = timeout(Duration::from_secs(2), acquire_auth_operation_lock())
            .await
            .unwrap()
            .unwrap();
        drop(second);
    }
}

pub use oauth_server::*;
pub use storage::*;
pub use switcher::*;
pub use token_refresh::*;
