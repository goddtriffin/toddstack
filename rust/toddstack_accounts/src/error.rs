/// Everything that can go wrong talking to the accounts service.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The callback's `state` doesn't match the pending login (a forged or
    /// stale callback: start over).
    #[error("the sign-in callback doesn't match this browser's pending sign-in")]
    StateMismatch,
    /// The accounts service sent the person back with an OAuth error
    /// (`access_denied` when they cancelled, `login_required` for `prompt=none`).
    #[error("sign-in failed: {error}")]
    Authorization {
        error: String,
        description: Option<String>,
    },
    /// The callback had no `code`.
    #[error("the callback has no code")]
    MissingCode,
    /// The token endpoint refused (`invalid_grant`: expired, reused or revoked).
    #[error("token request refused: {error}")]
    TokenRefused {
        error: String,
        description: Option<String>,
    },
    /// A token failed verification (signature, issuer, audience, expiry, nonce).
    #[error("token rejected: {0}")]
    InvalidToken(String),
    /// A webhook failed verification.
    #[error("webhook rejected: {0}")]
    InvalidWebhook(&'static str),
    /// The API refused a call (status and its `error` message).
    #[error("accounts API returned {status}: {message}")]
    Api { status: u16, message: String },
    /// A notification call named something that doesn't exist (404): a player
    /// who never signed in to your project, or an unknown send.
    #[error("not found: {0}")]
    NotFound(String),
    /// A device registration named a token that belongs to another project (409).
    #[error("conflict: {0}")]
    Conflict(String),
    /// A notification call was refused as unprocessable (422): the same
    /// idempotency key with a different request, or a field the service can't
    /// parse.
    #[error("unprocessable: {0}")]
    Unprocessable(String),
    /// A notification call hit a rate limit (429). Wait before sending again.
    #[error("rate limited: {0}")]
    RateLimited(String),
    /// A Server API call (`Accounts::player`, `Accounts::report_as_player`)
    /// was refused. `code` is the stable `error` field of the answer
    /// (`not_found`, `suspended`, `scope`, `rate_limited`, `bad_request`,
    /// `unprocessable`, ...): match on it, not on `message`. Everything but a
    /// 429 and a 5xx is final.
    #[error("accounts API returned {status} {code}: {message}")]
    Server {
        status: u16,
        code: String,
        message: String,
    },
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("configuration: {0}")]
    Config(String),
}

impl Error {
    /// The API's `error` code, for [`Error::Server`].
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Server { code, .. } => Some(code),
            _ => None,
        }
    }
}
