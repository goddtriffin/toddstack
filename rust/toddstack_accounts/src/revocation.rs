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
//! # async fn example() -> Result<(), toddstack_accounts::Error> {
//! use toddstack_accounts::{Accounts, Config};
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
//! poll lands, so a token revoked before this process started is refused only
//! after startup's poll: [`Accounts::start_revocation_poller`] awaits it.
//! If accounts is unreachable, verification keeps working (fail open) and
//! `on_error` hears about each failed poll.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;

use crate::tokens::Accounts;
use crate::webhook::{self, EventKind};
use crate::{Error, Result};

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

/// The token families known to be revoked, shared between the webhook handler,
/// the poller and token verification. Cheap to share; deduplicated by `fam`.
#[derive(Debug, Default)]
pub struct Revocations {
    /// `fam` to the Unix second it stops mattering.
    families: RwLock<HashMap<String, i64>>,
}

impl Revocations {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a dead family until `expires_at` (Unix seconds). Repeats are
    /// fine (the service's polls overlap): the later expiry wins.
    pub fn revoke(&self, fam: &str, expires_at: i64) {
        let now: i64 = now_unix();
        if let Ok(mut families) = self.families.write() {
            families.retain(|_, until| *until > now);
            let entry: &mut i64 = families.entry(fam.to_owned()).or_insert(expires_at);
            *entry = (*entry).max(expires_at);
        }
    }

    /// Whether `fam` is revoked and its access tokens could still be alive.
    #[must_use]
    pub fn is_revoked(&self, fam: &str) -> bool {
        let now: i64 = now_unix();
        self.families
            .read()
            .is_ok_and(|families| families.get(fam).is_some_and(|until| *until > now))
    }

    /// How many families are being held.
    #[must_use]
    pub fn len(&self) -> usize {
        let now: i64 = now_unix();
        self.families
            .read()
            .map_or(0, |f| f.values().filter(|until| **until > now).count())
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
        for fam in &families {
            self.revoke(fam, until);
        }
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
/// [`POLL_INTERVAL`], failures ignored).
#[derive(Clone)]
pub struct PollerOptions {
    pub interval: Duration,
    /// Called with each failed poll (the poller retries on its own). Log it.
    pub on_error: Option<OnError>,
}

impl Default for PollerOptions {
    fn default() -> Self {
        Self {
            interval: POLL_INTERVAL,
            on_error: None,
        }
    }
}

/// The running poll. Dropping it (or [`RevocationPoller::stop`]) stops it.
#[derive(Debug)]
pub struct RevocationPoller {
    task: tokio::task::JoinHandle<()>,
}

impl RevocationPoller {
    pub fn stop(self) {
        drop(self);
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
        for family in &polled.families {
            // An unreadable expiry still revokes, for the longest a token lives.
            let until: i64 = unix(&family.expires_at).unwrap_or(now_unix() + WEBHOOK_TTL_SECONDS);
            self.revoked.revoke(&family.fam, until);
        }
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
    /// [`Error::Config`] without a client secret. A failed poll is not an
    /// error here: it goes to `on_error` and is retried.
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
        if let Err(error) = self.poll_revocations(&mut cursor).await
            && let Some(on_error) = &options.on_error
        {
            on_error(&error);
        }
        let accounts: Arc<Self> = Arc::clone(self);
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(options.interval).await;
                if let Err(error) = accounts.poll_revocations(&mut cursor).await
                    && let Some(on_error) = &options.on_error
                {
                    on_error(&error);
                }
            }
        });
        Ok(RevocationPoller { task })
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
