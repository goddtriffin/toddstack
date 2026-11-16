//! The accounts service's published signing keys, cached. Keys rotate on a
//! schedule (a new key is published before it signs), so an unknown `kid`
//! triggers a refetch, at most once a minute whether or not it succeeds: a
//! burst of tokens with made-up `kid`s can't turn into a burst of fetches.
//! Concurrent callers share one fetch and its outcome.

use std::sync::RwLock;
use std::time::{Duration, Instant};

use jsonwebtoken::DecodingKey;
use jsonwebtoken::jwk::JwkSet;

use crate::retry::{self, RetryPolicy};
use crate::{Error, Result};

const REFRESH_EVERY: Duration = Duration::from_secs(3600);
const MIN_REFETCH: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Cache {
    set: Option<JwkSet>,
    fetched_at: Option<Instant>,
    /// The last fetch made for an unknown `kid`, successful or not.
    refetched_at: Option<Instant>,
    /// Fetches finished, so a caller that waited on one uses its outcome
    /// instead of fetching again.
    attempts: u64,
}

impl Cache {
    fn lookup(&self, kid: &str) -> Option<DecodingKey> {
        self.set
            .as_ref()?
            .find(kid)
            .and_then(|jwk| DecodingKey::from_jwk(jwk).ok())
    }

    fn fresh(&self) -> bool {
        self.fetched_at
            .is_some_and(|at| at.elapsed() <= REFRESH_EVERY)
    }

    fn may_refetch(&self) -> bool {
        self.refetched_at
            .is_none_or(|at| at.elapsed() > MIN_REFETCH)
    }

    fn fresh_lookup(&self, kid: &str) -> Result<DecodingKey> {
        if !self.fresh() {
            return Err(Error::InvalidToken(String::from(
                "the signing keys couldn't be fetched (try again shortly)",
            )));
        }
        self.lookup(kid).ok_or_else(unknown)
    }
}

pub struct Keys {
    url: String,
    /// Pinned keys (tests, offline use) are never refetched.
    pinned: bool,
    cache: RwLock<Cache>,
    /// Held while fetching.
    fetching: tokio::sync::Mutex<()>,
    policy: RetryPolicy,
}

fn unknown() -> Error {
    Error::InvalidToken(String::from("unknown signing key"))
}

fn poisoned() -> Error {
    Error::InvalidToken(String::from("the signing key cache is unusable"))
}

impl Keys {
    pub fn remote(url: String) -> Self {
        Self {
            url,
            pinned: false,
            cache: RwLock::new(Cache::default()),
            fetching: tokio::sync::Mutex::new(()),
            policy: RetryPolicy::READ,
        }
    }

    pub fn pinned(set: JwkSet) -> Self {
        Self {
            url: String::new(),
            pinned: true,
            cache: RwLock::new(Cache {
                set: Some(set),
                fetched_at: Some(Instant::now()),
                ..Cache::default()
            }),
            fetching: tokio::sync::Mutex::new(()),
            policy: RetryPolicy::READ,
        }
    }

    /// The key for `kid`, fetching the key set if it's stale, or if `kid` is
    /// new to it (rate-limited).
    pub async fn get(&self, http: &reqwest::Client, kid: &str) -> Result<DecodingKey> {
        let seen: u64 = {
            let cache = self.cache.read().map_err(|_| poisoned())?;
            if self.pinned {
                return cache.lookup(kid).ok_or_else(unknown);
            }
            if cache.fresh()
                && let Some(key) = cache.lookup(kid)
            {
                return Ok(key);
            }
            cache.attempts
        };
        let _fetching = self.fetching.lock().await;
        {
            let mut cache = self.cache.write().map_err(|_| poisoned())?;
            if cache.attempts != seen {
                return cache.fresh_lookup(kid);
            }
            if cache.fresh() {
                if let Some(key) = cache.lookup(kid) {
                    return Ok(key);
                }
                if !cache.may_refetch() {
                    return Err(unknown());
                }
                cache.refetched_at = Some(Instant::now());
            }
        }
        let fetched: Result<JwkSet> = self.fetch(http).await;
        let mut cache = self.cache.write().map_err(|_| poisoned())?;
        cache.attempts += 1;
        let set: JwkSet = fetched?;
        let now: Instant = Instant::now();
        cache.set = Some(set);
        cache.fetched_at = Some(now);
        cache.refetched_at = Some(now);
        cache.lookup(kid).ok_or_else(unknown)
    }

    /// One logical fetch, retried on an intermittent failure (see
    /// [`crate::retry`]); the once-a-minute limit counts it once.
    async fn fetch(&self, http: &reqwest::Client) -> Result<JwkSet> {
        Ok(
            retry::execute(http, http.get(&self.url).build()?, &self.policy)
                .await?
                .error_for_status()?
                .json()
                .await?,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use p256::SecretKey;
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    fn jwk(seed: u8, kid: &str) -> Value {
        let secret: SecretKey = SecretKey::from_slice(&[seed; 32]).unwrap();
        let point: Box<[u8]> = secret.public_key().to_sec1_bytes();
        json!({
            "kty": "EC", "crv": "P-256", "kid": kid, "use": "sig", "alg": "ES256",
            "x": URL_SAFE_NO_PAD.encode(&point[1..33]), "y": URL_SAFE_NO_PAD.encode(&point[33..65])
        })
    }

    /// A fake JWKS endpoint: serves whatever was last published with the
    /// status last set, and counts requests.
    struct Server {
        url: String,
        body: Arc<RwLock<String>>,
        status: Arc<AtomicU16>,
        hits: Arc<AtomicUsize>,
    }

    impl Server {
        async fn start(keys: &[Value]) -> Self {
            let listener: TcpListener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url: String = format!("http://{}/oidc/jwks", listener.local_addr().unwrap());
            let body: Arc<RwLock<String>> =
                Arc::new(RwLock::new(json!({ "keys": keys }).to_string()));
            let status: Arc<AtomicU16> = Arc::new(AtomicU16::new(200));
            let hits: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
            let (served, code, counted) =
                (Arc::clone(&body), Arc::clone(&status), Arc::clone(&hits));
            tokio::spawn(async move {
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request: [u8; 4096] = [0; 4096];
                    let _ = socket.read(&mut request).await.unwrap();
                    counted.fetch_add(1, Ordering::SeqCst);
                    // Slow enough that concurrent lookups overlap.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let body: String = served.read().unwrap().clone();
                    let response: String = format!(
                        "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        code.load(Ordering::SeqCst),
                        body.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                }
            });
            Self {
                url,
                body,
                status,
                hits,
            }
        }

        fn publish(&self, keys: &[Value]) {
            *self.body.write().unwrap() = json!({ "keys": keys }).to_string();
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    /// Keys from `server`, retrying with millisecond backoff to keep tests fast.
    fn fast(server: &Server) -> Keys {
        Keys {
            policy: crate::retry::tests::FAST,
            ..Keys::remote(server.url.clone())
        }
    }

    fn backdate(keys: &Keys, by: Duration) {
        let mut cache = keys.cache.write().unwrap();
        let then: Instant = Instant::now().checked_sub(by).unwrap();
        cache.fetched_at = Some(then);
        cache.refetched_at = Some(then);
    }

    const PAST_MIN_REFETCH: Duration = Duration::from_secs(61);

    #[tokio::test]
    async fn a_key_published_after_the_last_fetch_is_found_by_refetching() {
        let server: Server = Server::start(&[jwk(1, "old")]).await;
        let keys: Keys = fast(&server);
        let http: reqwest::Client = reqwest::Client::new();
        keys.get(&http, "old").await.unwrap();
        backdate(&keys, PAST_MIN_REFETCH);
        server.publish(&[jwk(1, "old"), jwk(2, "new")]);

        keys.get(&http, "new").await.unwrap();
        assert_eq!(2, server.hits());
    }

    #[tokio::test]
    async fn unknown_kids_refetch_at_most_once_a_minute() {
        let server: Server = Server::start(&[jwk(1, "old")]).await;
        let keys: Keys = fast(&server);
        let http: reqwest::Client = reqwest::Client::new();
        keys.get(&http, "old").await.unwrap();
        backdate(&keys, PAST_MIN_REFETCH);

        for kid in ["made-up-1", "made-up-2", "made-up-3"] {
            assert!(matches!(
                keys.get(&http, kid).await,
                Err(Error::InvalidToken(_))
            ));
        }
        assert_eq!(2, server.hits());
        // The cached key still verifies without a fetch.
        keys.get(&http, "old").await.unwrap();
        assert_eq!(2, server.hits());
    }

    #[tokio::test]
    async fn a_failed_refetch_still_counts_against_the_minute() {
        let server: Server = Server::start(&[jwk(1, "old")]).await;
        let keys: Keys = fast(&server);
        let http: reqwest::Client = reqwest::Client::new();
        keys.get(&http, "old").await.unwrap();
        backdate(&keys, PAST_MIN_REFETCH);
        server.status.store(503, Ordering::SeqCst);

        assert!(matches!(keys.get(&http, "new").await, Err(Error::Http(_))));
        assert!(keys.get(&http, "new").await.is_err());
        // One fetch of three attempts (a 503 is retried), and no second fetch.
        assert_eq!(4, server.hits());
        keys.get(&http, "old").await.unwrap();
    }

    #[tokio::test]
    async fn a_new_key_within_a_minute_of_the_last_fetch_waits_for_the_next_window() {
        let server: Server = Server::start(&[jwk(1, "old")]).await;
        let keys: Keys = fast(&server);
        let http: reqwest::Client = reqwest::Client::new();
        keys.get(&http, "old").await.unwrap();
        server.publish(&[jwk(1, "old"), jwk(2, "new")]);

        assert!(keys.get(&http, "new").await.is_err());
        assert_eq!(1, server.hits());
        backdate(&keys, PAST_MIN_REFETCH);
        keys.get(&http, "new").await.unwrap();
        assert_eq!(2, server.hits());
    }

    #[tokio::test]
    async fn a_stale_cache_is_refreshed_even_for_a_known_kid() {
        let server: Server = Server::start(&[jwk(1, "old")]).await;
        let keys: Keys = fast(&server);
        let http: reqwest::Client = reqwest::Client::new();
        keys.get(&http, "old").await.unwrap();
        backdate(&keys, REFRESH_EVERY + Duration::from_secs(1));
        // The retired key is gone from the published set: it must stop verifying.
        server.publish(&[jwk(2, "new")]);

        assert!(keys.get(&http, "old").await.is_err());
        assert_eq!(2, server.hits());
    }

    #[tokio::test]
    async fn concurrent_lookups_share_one_fetch() {
        let server: Server = Server::start(&[jwk(1, "old"), jwk(2, "new")]).await;
        let keys: Arc<Keys> = Arc::new(fast(&server));
        let http: reqwest::Client = reqwest::Client::new();
        let lookups: Vec<tokio::task::JoinHandle<bool>> = (0..8)
            .map(|_| {
                let (keys, http) = (Arc::clone(&keys), http.clone());
                tokio::spawn(async move { keys.get(&http, "new").await.is_ok() })
            })
            .collect();
        for lookup in lookups {
            assert!(lookup.await.unwrap());
        }
        assert_eq!(1, server.hits());
    }

    #[tokio::test]
    async fn concurrent_lookups_share_a_failed_fetch_instead_of_queueing_more() {
        let server: Server = Server::start(&[jwk(1, "old")]).await;
        server.status.store(503, Ordering::SeqCst);
        let keys: Arc<Keys> = Arc::new(fast(&server));
        let http: reqwest::Client = reqwest::Client::new();
        let lookups: Vec<tokio::task::JoinHandle<bool>> = (0..8)
            .map(|_| {
                let (keys, http) = (Arc::clone(&keys), http.clone());
                tokio::spawn(async move { keys.get(&http, "old").await.is_ok() })
            })
            .collect();
        for lookup in lookups {
            assert!(!lookup.await.unwrap());
        }
        // One fetch of three attempts (a 503 is retried), shared by all eight.
        assert_eq!(3, server.hits());
    }

    #[tokio::test]
    async fn pinned_keys_are_never_fetched() {
        let set: JwkSet = serde_json::from_value(json!({ "keys": [jwk(1, "pinned")] })).unwrap();
        let keys: Keys = Keys::pinned(set);
        let http: reqwest::Client = reqwest::Client::new();
        keys.get(&http, "pinned").await.unwrap();
        assert!(matches!(
            keys.get(&http, "other").await,
            Err(Error::InvalidToken(_))
        ));
    }
}
