//! Bounded, account-scoped response cache and single-flight HTTP requests.
//! OAuth token POSTs are deliberately never cached or deduplicated here.

use std::{
    any::Any,
    collections::HashMap,
    future::Future,
    hash::{Hash, Hasher},
    mem::size_of,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use reqwest::{header::HeaderMap, Client, Response};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

use super::usage::ChatGptAccountMetadata;
use crate::types::{AuthData, StoredAccount};

const MAX_ENTRIES: usize = 128;
const MAX_FLIGHTS: usize = 128;
const MAX_CACHE_BYTES: usize = 512 * 1024;
const GET_CONCURRENCY: usize = 6;
pub const USAGE_TTL: Duration = Duration::from_secs(4);
pub const STATS_TTL: Duration = Duration::from_secs(20);
pub const RESET_CREDITS_TTL: Duration = Duration::from_secs(12);
pub const METADATA_TTL: Duration = Duration::from_secs(60);

static CLIENT: LazyLock<Client> = LazyLock::new(Client::new);
static GET_GATE: Semaphore = Semaphore::const_new(GET_CONCURRENCY);
static CACHE: LazyLock<RequestCache> = LazyLock::new(RequestCache::default);

pub fn client() -> &'static Client {
    &CLIENT
}

/// Count actual outgoing attempts, including failures and retries.
pub fn record_outbound_request() {
    CACHE.state().outbound_requests += 1;
}

pub async fn send_get(url: &str, headers: HeaderMap) -> Result<Response> {
    // Only GET requests are throttled. Never queue single-use OAuth refreshes
    // behind statistics/usage polling or a lengthy warm-up.
    let _permit = GET_GATE.acquire().await.map_err(|e| anyhow!(e))?;
    record_outbound_request();
    Ok(client().get(url).headers(headers).send().await?)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum Endpoint {
    Usage,
    Stats,
    ResetCredits,
    Metadata,
}

#[derive(Clone, Eq, PartialEq)]
struct Key {
    endpoint: Endpoint,
    account_id: String,
    digest: [u8; 32],
}

impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.endpoint.hash(state);
        self.account_id.hash(state);
        self.digest.hash(state);
    }
}

fn key_for(account: &StoredAccount, endpoint: Endpoint) -> Option<Key> {
    let AuthData::ChatGPT {
        access_token,
        account_id,
        ..
    } = &account.auth_data
    else {
        return None;
    };

    // Never retain bearer tokens in cache keys (nor expose them in metrics).
    let mut hasher = Sha256::new();
    hasher.update(access_token.as_bytes());
    hasher.update([0]);
    hasher.update(account_id.as_deref().unwrap_or("").as_bytes());
    Some(Key {
        endpoint,
        account_id: account.id.clone(),
        digest: hasher.finalize().into(),
    })
}

struct Entry {
    value: Arc<dyn Any + Send + Sync>,
    expires_at: Instant,
    approx_bytes: usize,
}

#[derive(Default)]
struct State {
    epoch: u64,
    entries: HashMap<Key, Entry>,
    flights: HashMap<Key, Arc<AsyncMutex<()>>>,
    metadata: HashMap<String, (Instant, ChatGptAccountMetadata)>,
    cache_hits: u64,
    joined_flights: u64,
    fetches: u64,
    outbound_requests: u64,
}

#[derive(Default)]
pub struct RequestCache {
    inner: Mutex<State>,
}

#[derive(Debug, Clone, Copy)]
pub struct CacheMetrics {
    pub cache_entries: usize,
    pub active_flight_keys: usize,
    pub cache_bytes_estimate: usize,
    pub cache_hits: u64,
    pub joined_flights: u64,
    pub fetches: u64,
    pub outbound_requests: u64,
}

impl RequestCache {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn prune(state: &mut State) {
        let now = Instant::now();
        state.entries.retain(|_, entry| entry.expires_at > now);
        state.metadata.retain(|_, (expires, _)| *expires > now);
        // Do not evict in-flight locks: their waiters must remain deduplicated.
        state
            .flights
            .retain(|_, flight| Arc::strong_count(flight) > 1);
    }

    fn lookup<T: Clone + Send + Sync + 'static>(state: &mut State, key: &Key) -> Option<T> {
        let entry = state.entries.get(key)?;
        if entry.expires_at <= Instant::now() {
            return None;
        }
        let value = entry.value.downcast_ref::<T>()?.clone();
        state.cache_hits += 1;
        Some(value)
    }

    fn bytes(state: &State) -> usize {
        state
            .entries
            .iter()
            .map(|(key, value)| value.approx_bytes + key.account_id.len())
            .sum::<usize>()
            + state
                .flights
                .keys()
                .map(|key| key.account_id.len() + size_of::<Key>() + size_of::<AsyncMutex<()>>())
                .sum::<usize>()
            + state
                .metadata
                .iter()
                .map(|(id, (_, m))| {
                    id.len()
                        + size_of::<ChatGptAccountMetadata>()
                        + m.plan_type.as_ref().map_or(0, String::len)
                })
                .sum::<usize>()
    }

    fn insert<T: Serialize + Clone + Send + Sync + 'static>(
        state: &mut State,
        key: Key,
        ttl: Duration,
        value: T,
    ) {
        // Serialize only to measure bounded memory; store a typed value to
        // avoid JSON round trips on frequent cache hits.
        let Ok(serialized) = serde_json::to_vec(&value) else {
            return;
        };
        let bytes = serialized.len() + size_of::<Entry>() + size_of::<Key>() + key.account_id.len();
        if bytes > MAX_CACHE_BYTES {
            return;
        }
        state.entries.remove(&key);
        while state.entries.len() >= MAX_ENTRIES
            || state
                .entries
                .values()
                .map(|entry| entry.approx_bytes)
                .sum::<usize>()
                + bytes
                > MAX_CACHE_BYTES
        {
            let Some(oldest) = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            state.entries.remove(&oldest);
        }
        state.entries.insert(
            key,
            Entry {
                value: Arc::new(value),
                expires_at: Instant::now() + ttl,
                approx_bytes: bytes,
            },
        );
    }

    pub fn invalidate(&self) {
        let mut state = self.state();
        state.epoch = state.epoch.wrapping_add(1);
        state.entries.clear();
        state.flights.clear();
        state.metadata.clear();
    }

    pub fn metrics(&self) -> CacheMetrics {
        let mut state = self.state();
        Self::prune(&mut state);
        CacheMetrics {
            cache_entries: state.entries.len(),
            active_flight_keys: state.flights.len(),
            cache_bytes_estimate: Self::bytes(&state),
            cache_hits: state.cache_hits,
            joined_flights: state.joined_flights,
            fetches: state.fetches,
            outbound_requests: state.outbound_requests,
        }
    }

    pub async fn get_or_fetch<T, F, Fut, P>(
        &self,
        account: &StoredAccount,
        endpoint: Endpoint,
        ttl: Duration,
        cacheable: P,
        fetch: F,
    ) -> Result<T>
    where
        T: Serialize + Clone + Send + Sync + 'static,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
        P: Fn(&T) -> bool,
    {
        let Some(key) = key_for(account, endpoint) else {
            return fetch().await;
        };

        let (flight, epoch) = {
            let mut state = self.state();
            Self::prune(&mut state);
            if let Some(value) = Self::lookup(&mut state, &key) {
                return Ok(value);
            }
            let epoch = state.epoch;
            if state.flights.contains_key(&key) {
                state.joined_flights += 1;
            }
            // Overflow keys still work but are not retained: bound memory
            // independently of the number of imported accounts.
            let flight = if let Some(lock) = state.flights.get(&key) {
                lock.clone()
            } else if state.flights.len() < MAX_FLIGHTS {
                let lock = Arc::new(AsyncMutex::new(()));
                state.flights.insert(key.clone(), lock.clone());
                lock
            } else {
                Arc::new(AsyncMutex::new(()))
            };
            (flight, epoch)
        };

        let _flight_guard = flight.lock().await;
        {
            let mut state = self.state();
            if state.epoch != epoch {
                return Err(anyhow!(
                    "Account data changed while request was queued; retry"
                ));
            }
            if let Some(value) = Self::lookup(&mut state, &key) {
                return Ok(value);
            }
            state.fetches += 1;
        }

        let result = fetch().await;
        let mut state = self.state();
        // Switch, OAuth rotation, warm-up, and resets invalidate the entire
        // generation. Never publish or cache a response from an older one.
        if state.epoch != epoch {
            return Err(anyhow!("Account data changed during request; retry"));
        }
        if let Ok(ref value) = result {
            if cacheable(value) {
                Self::insert(&mut state, key, ttl, value.clone());
            }
        }
        result
    }

    fn metadata_for(&self, account_id: &str) -> Option<ChatGptAccountMetadata> {
        let mut state = self.state();
        Self::prune(&mut state);
        state.metadata.get(account_id).map(|(_, data)| data.clone())
    }

    fn store_metadata(&self, account: &StoredAccount, data: &ChatGptAccountMetadata) {
        let Some(key) = key_for(account, Endpoint::Metadata) else {
            return;
        };
        let mut state = self.state();
        Self::insert(&mut state, key, METADATA_TTL, data.clone());
        state.metadata.insert(
            account.id.clone(),
            (Instant::now() + METADATA_TTL, data.clone()),
        );
    }
}

pub fn invalidate_all() {
    CACHE.invalidate();
}

pub fn metrics() -> CacheMetrics {
    CACHE.metrics()
}

pub fn get_cached_metadata(account_id: &str) -> Option<ChatGptAccountMetadata> {
    CACHE.metadata_for(account_id)
}

/// Re-seed after persistence, which deliberately invalidates old generations.
pub fn store_metadata(account: &StoredAccount, data: &ChatGptAccountMetadata) {
    CACHE.store_metadata(account, data);
}

pub async fn get_or_fetch<T, F, Fut, P>(
    account: &StoredAccount,
    endpoint: Endpoint,
    ttl: Duration,
    cacheable: P,
    fetch: F,
) -> Result<T>
where
    T: Serialize + Clone + Send + Sync + 'static,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T>>,
    P: Fn(&T) -> bool,
{
    CACHE
        .get_or_fetch(account, endpoint, ttl, cacheable, fetch)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AuthMode;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn account(token: &str) -> StoredAccount {
        StoredAccount {
            id: "account-1".into(),
            name: "Test".into(),
            email: None,
            plan_type: None,
            subscription_expires_at: None,
            auth_mode: AuthMode::ChatGPT,
            auth_data: AuthData::ChatGPT {
                id_token: "".into(),
                access_token: token.into(),
                refresh_token: "single-use".into(),
                account_id: Some("workspace".into()),
            },
            created_at: chrono::Utc::now(),
            last_used_at: None,
            last_refresh_at: None,
        }
    }

    #[tokio::test]
    async fn overlapping_requests_share_one_fetch() {
        let cache = RequestCache::default();
        let calls = AtomicUsize::new(0);
        let account = account("token-a");
        let results = futures::future::join_all((0..32).map(|_| {
            let c = &cache;
            let calls = &calls;
            let a = &account;
            async move {
                c.get_or_fetch(
                    a,
                    Endpoint::Usage,
                    USAGE_TTL,
                    |_| true,
                    || async {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        Ok(42usize)
                    },
                )
                .await
            }
        }))
        .await;
        assert!(results.iter().all(|value| matches!(value, Ok(42))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(cache.metrics().fetches, 1);
        assert!(cache.metrics().joined_flights > 0);
    }

    #[tokio::test]
    async fn failures_are_not_cached_and_token_changes_split_keys() {
        let cache = RequestCache::default();
        let a = account("token-a");
        let b = account("token-b");
        let first = cache
            .get_or_fetch(
                &a,
                Endpoint::ResetCredits,
                RESET_CREDITS_TTL,
                |_| true,
                || async { Err::<usize, _>(anyhow!("HTTP 503")) },
            )
            .await;
        assert!(first.is_err());
        assert_eq!(cache.metrics().cache_entries, 0);
        assert_eq!(
            cache
                .get_or_fetch(
                    &a,
                    Endpoint::ResetCredits,
                    RESET_CREDITS_TTL,
                    |_| true,
                    || async { Ok::<usize, anyhow::Error>(1) }
                )
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            cache
                .get_or_fetch(
                    &b,
                    Endpoint::ResetCredits,
                    RESET_CREDITS_TTL,
                    |_| true,
                    || async { Ok::<usize, anyhow::Error>(2) }
                )
                .await
                .unwrap(),
            2
        );
        assert_eq!(cache.metrics().cache_entries, 2);
    }

    #[tokio::test]
    async fn invalidation_rejects_in_flight_stale_result() {
        let cache = Arc::new(RequestCache::default());
        let acc = account("token-a");
        let c = cache.clone();
        let task = tokio::spawn(async move {
            c.get_or_fetch(
                &acc,
                Endpoint::Stats,
                STATS_TTL,
                |_| true,
                || async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok::<_, anyhow::Error>(7usize)
                },
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        cache.invalidate();
        assert!(task.await.unwrap().is_err());
        assert_eq!(cache.metrics().cache_entries, 0);
    }

    #[tokio::test]
    async fn expiry_and_memory_limit() {
        let cache = RequestCache::default();
        let acc = account("token-a");
        cache
            .get_or_fetch(
                &acc,
                Endpoint::Stats,
                Duration::from_millis(5),
                |_| true,
                || async { Ok::<_, anyhow::Error>(vec![0u8; 1024]) },
            )
            .await
            .unwrap();
        assert!(cache.metrics().cache_bytes_estimate < MAX_CACHE_BYTES);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(cache.metrics().cache_entries, 0);
    }

    #[tokio::test]
    async fn high_cardinality_does_not_grow_unbounded() {
        let cache = RequestCache::default();
        for n in 0..300 {
            let a = account(&format!("access-{n}"));
            cache
                .get_or_fetch(
                    &a,
                    Endpoint::Usage,
                    USAGE_TTL,
                    |_| true,
                    || async { Ok::<_, anyhow::Error>([0u8; 128].to_vec()) },
                )
                .await
                .unwrap();
        }
        let m = cache.metrics();
        assert!(m.cache_entries <= MAX_ENTRIES);
        assert!(m.cache_bytes_estimate < MAX_CACHE_BYTES);
        assert_eq!(m.fetches, 300);
    }
}
