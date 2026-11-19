//! Instant logout for servers that verify access tokens locally.
//!
//! [`Accounts::verify_access_token`] never calls the accounts service, so on
//! its own it cannot see a sign-out. The service tells you which token
//! families (`fam`) are dead, two ways, and both feed one in-memory set that
//! `verify_access_token` consults:
//!
//! - the signed `tokens.revoked` webhook ([`Accounts::handle_webhook`], or
//!   [`Revocations::apply`] if you verify webhooks yourself), and
//! - a poll of `GET /api/v1/revoked-tokens` at startup and every 30 seconds,
//!   the backup for a missed webhook ([`Accounts::start_revocation_poller`]).
//!
//! The easy wiring is one call:
//!
//! ```no_run
//! # async fn example() -> Result<(), toddstack_accounts_server_sdk::Error> {
//! use toddstack_accounts_server_sdk::{Accounts, Config};
//!
//! let config = Config::toddgriffin("boggledygook", Some(String::from("secret")), "https://x/cb");
//! // Keep `_poller` alive for as long as the server runs; dropping it stops the poll.
//! let (accounts, _poller) = Accounts::with_revocation_poller(config).await?;
//! # let token = "";
//! let claims = accounts.verify_access_token(token).await?; // refuses a revoked `fam`
//! # let _ = claims;
//! # Ok(()) }
//! ```
//!
//! Entries are kept until their `expires_at` (when the family's last access
//! token has died), so the set stays small. The set is empty until the first
//! poll lands, so [`Accounts::start_revocation_poller`] awaits it (retrying
//! quickly, about 3 tries over a few seconds) and returns an error if it
//! still fails: you decide whether to start without instant logout or refuse
//! to boot. After that, if accounts is unreachable, verification keeps
//! working (fail open), `on_error` hears about each failed poll, and
//! [`RevocationPoller::last_success`] tells a health check how stale the set is.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;

use crate::tokens::Accounts;
use crate::webhook::{self, EventKind};
use crate::{Error, Result};

/// Tries at the startup poll before giving up.
const STARTUP_TRIES: u32 = 3;

/// How often the poller asks for new revocations.
pub const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// How long a family from a webhook is kept: the access token lifetime (10
/// minutes) plus the service's own margin, rounded up. The webhook carries no
/// `expires_at`.
const WEBHOOK_TTL_SECONDS: i64 = 15 * 60;

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Expired entries are dropped at most this often, so a burst of inserts never rescans the whole set each time.
const PRUNE_EVERY_SECONDS: i64 = 10;

#[derive(Debug, Default)]
struct Held {
    /// `fam` to the Unix second it stops mattering.
    families: HashMap<String, i64>,
    next_prune: i64,
}

/// The token families known to be revoked, shared between the webhook handler,
/// the poller and token verification. Cheap to share; deduplicated by `fam`.
#[derive(Debug, Default)]
pub struct Revocations {
    held: RwLock<Held>,
}

impl Revocations {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a dead family until `expires_at` (Unix seconds). Repeats are
    /// fine (the service's polls overlap): the later expiry wins. For many
    /// families use [`Revocations::revoke_many`]: one lock for the batch.
    pub fn revoke(&self, fam: &str, expires_at: i64) {
        self.revoke_many([(fam, expires_at)]);
    }

    /// [`Revocations::revoke`] for a batch, under one write lock, so a mass
    /// revocation neither stalls token verification nor costs more than one
    /// pass over the set.
    pub fn revoke_many<'a>(&self, families: impl IntoIterator<Item = (&'a str, i64)>) {
        let now: i64 = now_unix();
        if let Ok(mut held) = self.held.write() {
            for (fam, expires_at) in families {
                let entry: &mut i64 = held.families.entry(fam.to_owned()).or_insert(expires_at);
                *entry = (*entry).max(expires_at);
            }
            if now >= held.next_prune {
                held.families.retain(|_, until| *until > now);
                held.next_prune = now + PRUNE_EVERY_SECONDS;
            }
        }
    }

    /// Whether `fam` is revoked and its access tokens could still be alive.
    #[must_use]
    pub fn is_revoked(&self, fam: &str) -> bool {
        let now: i64 = now_unix();
        self.held
            .read()
            .is_ok_and(|held| held.families.get(fam).is_some_and(|until| *until > now))
    }

    /// How many families are being held.
    #[must_use]
    pub fn len(&self) -> usize {
        let now: i64 = now_unix();
        self.held.read().map_or(0, |held| {
            held.families.values().filter(|until| **until > now).count()
        })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Records the families of a verified `tokens.revoked` webhook; any other
    /// event is ignored. Returns how many families the event named.
    pub fn apply(&self, event: &webhook::Event) -> usize {
        if event.kind != EventKind::TokensRevoked {
            return 0;
        }
        let families: Vec<&str> = event.families();
        let until: i64 = now_unix() + WEBHOOK_TTL_SECONDS;
        self.revoke_many(families.iter().map(|fam| (*fam, until)));
        families.len()
    }
}

#[derive(Deserialize)]
struct Revoked {
    fam: String,
    expires_at: String,
}

#[derive(Deserialize)]
struct Polled {
    #[serde(default)]
    families: Vec<Revoked>,
    now: Option<String>,
}

/// Unix seconds of an RFC 3339 time, or `None` if it doesn't parse.
fn unix(rfc3339: &str) -> Option<i64> {
    rfc3339
        .parse::<jiff::Timestamp>()
        .ok()
        .map(jiff::Timestamp::as_second)
}

/// What happens to a failed poll.
pub type OnError = Arc<dyn Fn(&Error) + Send + Sync>;

/// How the poller runs. [`Default`] is the production setting (every
/// [`POLL_INTERVAL`], failures ignored, startup retried after 1 s then 2 s).
#[derive(Clone)]
pub struct PollerOptions {
    pub interval: Duration,
    /// Called with each failed poll (the poller retries on its own). Log it.
    /// A panic in it is caught: the poller keeps polling.
    pub on_error: Option<OnError>,
    /// The wait before the second startup try; the third waits twice as long.
    pub startup_backoff: Duration,
}

impl Default for PollerOptions {
    fn default() -> Self {
        Self {
            interval: POLL_INTERVAL,
            on_error: None,
            startup_backoff: Duration::from_secs(1),
        }
    }
}

/// The running poll. Dropping it (or [`RevocationPoller::stop`]) stops it.
#[derive(Debug)]
pub struct RevocationPoller {
    task: tokio::task::JoinHandle<()>,
    /// Unix seconds of the last good poll.
    last_success: Arc<AtomicI64>,
}

impl RevocationPoller {
    pub fn stop(self) {
        drop(self);
    }

    /// When the last poll succeeded (the startup poll counts). For a health
    /// check: a stale value means revocations may be missed and only webhooks
    /// are protecting you.
    #[must_use]
    pub fn last_success(&self) -> Option<jiff::Timestamp> {
        match self.last_success.load(Ordering::Relaxed) {
            0 => None,
            second => jiff::Timestamp::from_second(second).ok(),
        }
    }
}

impl Drop for RevocationPoller {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Accounts {
    /// The set of revoked families this client checks tokens against.
    #[must_use]
    pub const fn revocations(&self) -> &Arc<Revocations> {
        &self.revoked
    }

    /// One poll of `GET /api/v1/revoked-tokens`: records the families it lists
    /// and moves `cursor` to the service's clock for the next call (start with
    /// `None`: the service answers the last 15 minutes). Returns how many
    /// families the answer listed. [`Accounts::start_revocation_poller`] does
    /// this for you.
    ///
    /// # Errors
    /// No client secret, or the API refused or failed.
    pub async fn poll_revocations(&self, cursor: &mut Option<String>) -> Result<usize> {
        let path: String = match cursor {
            Some(since) => format!(
                "/revoked-tokens?since={}",
                url::form_urlencoded::byte_serialize(since.as_bytes()).collect::<String>()
            ),
            None => String::from("/revoked-tokens"),
        };
        let answer: Value = self
            .project_call(reqwest::Method::GET, &path, None, false)
            .await?;
        let polled: Polled = serde_json::from_value(answer).map_err(|e| Error::Api {
            status: 200,
            message: e.to_string(),
        })?;
        // An unreadable expiry still revokes, for the longest a token lives.
        let fallback: i64 = now_unix() + WEBHOOK_TTL_SECONDS;
        self.revoked.revoke_many(
            polled
                .families
                .iter()
                .map(|f| (f.fam.as_str(), unix(&f.expires_at).unwrap_or(fallback))),
        );
        if polled.now.is_some() {
            *cursor = polled.now;
        }
        Ok(polled.families.len())
    }

    /// Starts the backup poll: one poll now (awaited, so tokens revoked before
    /// this process started are already refused when it returns), then one
    /// every 30 seconds. Needs the client secret. Keep the handle for as long
    /// as the server runs.
    ///
    /// # Errors
    /// [`Error::Config`] without a client secret. If the first poll still
    /// fails after 3 tries (a few seconds), its error: until a poll lands,
    /// revoked tokens verify, so you choose whether to run without it. Later
    /// failed polls are not errors: they go to `on_error` and are retried.
    pub async fn start_revocation_poller(self: &Arc<Self>) -> Result<RevocationPoller> {
        self.start_revocation_poller_with(PollerOptions::default())
            .await
    }

    /// [`Accounts::start_revocation_poller`] with a different interval or an
    /// error hook.
    ///
    /// # Errors
    /// As for [`Accounts::start_revocation_poller`].
    pub async fn start_revocation_poller_with(
        self: &Arc<Self>,
        options: PollerOptions,
    ) -> Result<RevocationPoller> {
        if self.config().client_secret.is_none() {
            return Err(Error::Config(String::from(
                "polling revocations needs the client secret: set Config::client_secret",
            )));
        }
        let mut cursor: Option<String> = None;
        let mut backoff: Duration = options.startup_backoff;
        for attempt in 1..=STARTUP_TRIES {
            match self.poll_revocations(&mut cursor).await {
                Ok(_) => break,
                Err(error) if attempt == STARTUP_TRIES => return Err(error),
                Err(_) => {
                    tokio::time::sleep(backoff).await;
                    backoff *= 2;
                }
            }
        }
        let last_success: Arc<AtomicI64> = Arc::new(AtomicI64::new(now_unix()));
        let seen: Arc<AtomicI64> = Arc::clone(&last_success);
        let accounts: Arc<Self> = Arc::clone(self);
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(options.interval).await;
                match accounts.poll_revocations(&mut cursor).await {
                    Ok(_) => seen.store(now_unix(), Ordering::Relaxed),
                    Err(error) => {
                        if let Some(on_error) = &options.on_error {
                            // User code must not be able to end the poll.
                            let _ = catch_unwind(AssertUnwindSafe(|| on_error(&error)));
                        }
                    }
                }
            }
        });
        Ok(RevocationPoller { task, last_success })
    }

    /// A client that checks tokens against revocations from the start: builds
    /// it and starts the poller. Pass the webhook handler the same `Arc`
    /// ([`Accounts::handle_webhook`]).
    ///
    /// # Errors
    /// As for [`Accounts::new`] and [`Accounts::start_revocation_poller`].
    pub async fn with_revocation_poller(
        config: crate::Config,
    ) -> Result<(Arc<Self>, RevocationPoller)> {
        let accounts: Arc<Self> = Arc::new(Self::new(config)?);
        let poller: RevocationPoller = accounts.start_revocation_poller().await?;
        Ok((accounts, poller))
    }

    /// Verifies a webhook (see [`webhook::verify`]) and, when it is
    /// `tokens.revoked`, records its families so tokens of those families stop
    /// verifying at once. Returns the event for the rest of your handling.
    ///
    /// # Errors
    /// [`Error::InvalidWebhook`].
    pub fn handle_webhook(
        &self,
        secret: &[u8],
        signature_header: &str,
        body: &[u8],
    ) -> Result<webhook::Event> {
        let event: webhook::Event = webhook::verify(secret, signature_header, body, now_unix())?;
        self.revoked.apply(&event);
        Ok(event)
    }
}
