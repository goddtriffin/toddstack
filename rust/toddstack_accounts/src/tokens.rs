//! Tokens: exchanging codes, refreshing, revoking, and verifying ID and access
//! tokens.

use std::sync::Arc;

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, Header, Validation, decode, decode_header};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::jwks::Keys;
use crate::login::{PendingLogin, parse_callback};
use crate::logout::{self, SignOut};
use crate::revocation::Revocations;
use crate::{Error, ISSUER, LOCAL_BASE_URL, Result};

/// Your app's registration with the accounts service.
#[derive(Clone)]
pub struct Config {
    /// `https://accounts.toddgriffin.me/oidc` in production.
    pub issuer: String,
    pub client_id: String,
    /// None for public clients (native and desktop apps).
    pub client_secret: Option<String>,
    /// Exactly as registered.
    pub redirect_uri: String,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                if self.client_secret.is_some() {
                    &"[redacted]" as &dyn std::fmt::Debug
                } else {
                    &None::<&str>
                },
            )
            .field("redirect_uri", &self.redirect_uri)
            .finish()
    }
}

impl Config {
    /// A config for the production accounts service.
    #[must_use]
    pub fn toddgriffin(client_id: &str, client_secret: Option<String>, redirect_uri: &str) -> Self {
        Self {
            issuer: String::from(ISSUER),
            client_id: client_id.to_owned(),
            client_secret,
            redirect_uri: redirect_uri.to_owned(),
        }
    }

    /// A config for an accounts service at `base_url` (its origin, e.g.
    /// `http://127.0.0.1:8790`): the issuer is `<base_url>/oidc`, and the
    /// JWKS, authorize, token, end-session, API and account links all derive
    /// from it. Local testing must never touch production: point at the
    /// service you run on your machine.
    ///
    /// # Errors
    /// [`Error::Config`] if `base_url` isn't an `https` URL, or an `http`
    /// URL whose host is loopback (`127.0.0.1`, `localhost`, `::1`).
    pub fn toddgriffin_at(
        base_url: &str,
        client_id: &str,
        client_secret: Option<String>,
        redirect_uri: &str,
    ) -> Result<Self> {
        let base: &str = base_url.trim_end_matches('/');
        check_url(base)?;
        Ok(Self {
            issuer: format!("{base}/oidc"),
            client_id: client_id.to_owned(),
            client_secret,
            redirect_uri: redirect_uri.to_owned(),
        })
    }

    /// A config for the accounts service running locally
    /// ([`LOCAL_BASE_URL`], `http://127.0.0.1:8790`).
    #[must_use]
    pub fn local(client_id: &str, client_secret: Option<String>, redirect_uri: &str) -> Self {
        Self {
            issuer: format!("{LOCAL_BASE_URL}/oidc"),
            client_id: client_id.to_owned(),
            client_secret,
            redirect_uri: redirect_uri.to_owned(),
        }
    }
}

/// Accepts `https` URLs, and `http` only on loopback hosts.
fn check_url(url: &str) -> Result<()> {
    let parsed: url::Url =
        url::Url::parse(url).map_err(|e| Error::Config(format!("`{url}` isn't a URL: {e}")))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")) => Ok(()),
        _ => Err(Error::Config(format!(
            "`{url}` must be https (http is allowed only on 127.0.0.1, localhost or ::1)"
        ))),
    }
}

/// The token endpoint's answer.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TokenSet {
    pub access_token: String,
    /// The raw signed ID token. Keep it with your session: sign-out sends it
    /// as `id_token_hint` ([`crate::logout::Options::id_token_hint`]).
    pub id_token: String,
    /// Present when `offline_access` was granted. Rotates on every refresh:
    /// always store the new one, and never reuse an old one (that revokes the
    /// whole family).
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    pub scope: String,
}

/// How the service treats an account. Review accounts (the App Store and Play
/// reviewers' logins) are hidden from other people: they never appear to
/// friends and get no friend-facing sender details on notifications.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// An ordinary account. The default, and what an absent claim means.
    #[default]
    Normal,
    /// A review account.
    Review,
}

impl Visibility {
    /// Whether this is a review account.
    #[must_use]
    pub const fn is_review(self) -> bool {
        matches!(self, Self::Review)
    }

    /// Reads the wire value (`"review"` or `"normal"`); anything else, a newer
    /// value included, is [`Visibility::Normal`].
    #[must_use]
    pub fn from_wire(value: Option<&str>) -> Self {
        if value == Some("review") {
            Self::Review
        } else {
            Self::Normal
        }
    }
}

/// Lenient: an unknown or `null` value reads as [`Visibility::Normal`], so a
/// value the service adds later never breaks sign-in.
impl<'de> Deserialize<'de> for Visibility {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Option::<String>::deserialize(deserializer)?;
        Ok(Self::from_wire(value.as_deref()))
    }
}

/// Verified ID token claims.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct IdClaims {
    /// The person's permanent id. Key everything on this.
    pub sub: String,
    pub username: Option<String>,
    pub name: Option<String>,
    pub picture: Option<String>,
    pub email: Option<String>,
    pub email_verified: Option<bool>,
    /// When they last proved who they are (Unix seconds).
    pub auth_time: Option<i64>,
    /// How: `pwd`, `otp`, `hwk` (passkey), `fed` (social), `mfa`, ...
    #[serde(default)]
    pub amr: Vec<String>,
    pub nonce: Option<String>,
    /// The accounts browser session this sign-in belongs to, opaque and
    /// stable for that session. Store it with your session and drop every
    /// local session with the same `sid` on `session.signed_out`. `None` for
    /// tokens issued before the service added it.
    pub sid: Option<String>,
    /// The `toddgriffin_visibility` claim: [`Visibility::Review`] for a review
    /// account. The claim is absent for normal accounts, so this defaults to
    /// [`Visibility::Normal`].
    #[serde(default, rename = "toddgriffin_visibility")]
    pub visibility: Visibility,
}

impl IdClaims {
    /// Whether this is a review account (`visibility` is [`Visibility::Review`]).
    #[must_use]
    pub const fn is_review(&self) -> bool {
        self.visibility.is_review()
    }
}

/// Verified access token claims (RFC 9068).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct AccessClaims {
    pub sub: String,
    pub client_id: String,
    pub scope: String,
    pub exp: i64,
    /// The token family (`fam`): every token from one sign-in and its
    /// refreshes. A revoked family is refused by
    /// [`Accounts::verify_access_token`]. `None` for tokens issued before the
    /// service added it.
    #[serde(default)]
    pub fam: Option<String>,
}

impl AccessClaims {
    #[must_use]
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scope.split(' ').any(|s| s == scope)
    }
}

/// A finished sign-in.
#[derive(Clone, Debug)]
pub struct SignedIn {
    pub claims: IdClaims,
    pub tokens: TokenSet,
    /// The checked path from [`crate::login::start`], if any.
    pub return_to: Option<String>,
}

/// The client. Cheap to share (`Arc` it); caches the signing keys.
pub struct Accounts {
    config: Config,
    http: reqwest::Client,
    keys: Keys,
    pub(crate) revoked: Arc<Revocations>,
}

#[derive(Deserialize)]
struct OAuthError {
    error: String,
    error_description: Option<String>,
}

impl Accounts {
    /// # Errors
    /// The HTTP client can't be built, or the issuer is plain `http` off
    /// loopback ([`Error::Config`]).
    pub fn new(config: Config) -> Result<Self> {
        check_url(&config.issuer)?;
        let keys: Keys = Keys::remote(format!("{}/jwks", config.issuer));
        Ok(Self {
            config,
            http: http()?,
            keys,
            revoked: Arc::default(),
        })
    }

    /// A client that verifies tokens against fixed keys and never fetches
    /// them: for tests, or a service that pins keys.
    ///
    /// # Errors
    /// The HTTP client can't be built, or the issuer is plain `http` off loopback.
    pub fn with_keys(config: Config, keys: JwkSet) -> Result<Self> {
        check_url(&config.issuer)?;
        Ok(Self {
            config,
            http: http()?,
            keys: Keys::pinned(keys),
            revoked: Arc::default(),
        })
    }

    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.config
    }

    #[must_use]
    pub const fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Completes a sign-in from the callback's query string: checks `state`
    /// and `iss`, exchanges the code, and verifies the ID token (including
    /// the nonce).
    ///
    /// # Errors
    /// The callback doesn't match, the person cancelled, or a token check failed.
    pub async fn finish_login(
        &self,
        pending: &PendingLogin,
        callback_query: &str,
    ) -> Result<SignedIn> {
        let callback = parse_callback(callback_query);
        if callback.state.as_deref() != Some(pending.state.as_str()) {
            return Err(Error::StateMismatch);
        }
        if let Some(iss) = &callback.iss
            && iss != &self.config.issuer
        {
            return Err(Error::StateMismatch);
        }
        if let Some(error) = callback.error {
            return Err(Error::Authorization {
                error,
                description: callback.error_description,
            });
        }
        let code: String = callback.code.ok_or(Error::MissingCode)?;
        let tokens: TokenSet = self
            .token_request(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", &self.config.redirect_uri),
                ("code_verifier", &pending.verifier),
            ])
            .await?;
        let claims: IdClaims = self
            .verify_id_token(&tokens.id_token, Some(&pending.nonce))
            .await?;
        Ok(SignedIn {
            claims,
            tokens,
            return_to: pending.return_to.clone(),
        })
    }

    /// Swaps a refresh token for new tokens. Store the new refresh token.
    ///
    /// # Errors
    /// [`Error::TokenRefused`] (`invalid_grant`) when it's expired, reused or
    /// the person removed your app: sign them in again.
    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenSet> {
        self.token_request(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .await
    }

    /// Revokes a refresh token (and its whole family), e.g. on sign-out.
    ///
    /// # Errors
    /// The request failed.
    pub async fn revoke(&self, refresh_token: &str) -> Result<()> {
        let mut form: Vec<(&str, &str)> = vec![
            ("token", refresh_token),
            ("token_type_hint", "refresh_token"),
        ];
        // Sent once: a write, like every non-GET (see `crate::retry`).
        let response = self
            .authed(
                self.http
                    .post(format!("{}/token/revocation", self.config.issuer)),
                &mut form,
            )
            .form(&form)
            .send()
            .await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(Error::Api {
                status: response.status().as_u16(),
                message: response.text().await.unwrap_or_default(),
            })
        }
    }

    /// Signs out in one call: revokes `refresh_token` (if you hold one) and
    /// builds the end-session URL. End your own session first, then redirect
    /// to [`SignOut::url`]: that signs the person out of the accounts service
    /// and so of every project in that browser. Set
    /// [`logout::Options::local_only`] to sign out of your app alone (no URL).
    /// A failed revoke doesn't stop sign-out: it comes back in
    /// [`SignOut::revoke_error`].
    pub async fn sign_out(
        &self,
        refresh_token: Option<&str>,
        options: &logout::Options<'_>,
    ) -> SignOut {
        let revoke_error: Option<Error> = match refresh_token {
            Some(token) => self.revoke(token).await.err(),
            None => None,
        };
        SignOut {
            url: (!options.local_only).then(|| logout::end_session_url(&self.config, options)),
            revoke_error,
        }
    }

    fn authed<'a>(
        &'a self,
        request: reqwest::RequestBuilder,
        form: &mut Vec<(&'a str, &'a str)>,
    ) -> reqwest::RequestBuilder {
        if let Some(secret) = &self.config.client_secret {
            request.basic_auth(
                url::form_urlencoded::byte_serialize(self.config.client_id.as_bytes())
                    .collect::<String>(),
                Some(url::form_urlencoded::byte_serialize(secret.as_bytes()).collect::<String>()),
            )
        } else {
            form.push(("client_id", &self.config.client_id));
            request
        }
    }

    /// Sent once, never retried: a code is single-use and a refresh rotates
    /// the token, so a retry of a request that had in fact landed fails with
    /// `invalid_grant` and signs the person out.
    async fn token_request(&self, params: &[(&str, &str)]) -> Result<TokenSet> {
        let mut form: Vec<(&str, &str)> = params.to_vec();
        let request = self.authed(
            self.http.post(format!("{}/token", self.config.issuer)),
            &mut form,
        );
        let response = request.form(&form).send().await?;
        let status = response.status();
        if status.is_success() {
            Ok(response.json().await?)
        } else {
            let error: OAuthError = response.json().await.unwrap_or(OAuthError {
                error: format!("http_{}", status.as_u16()),
                error_description: None,
            });
            Err(Error::TokenRefused {
                error: error.error,
                description: error.error_description,
            })
        }
    }

    async fn decode<T: for<'de> Deserialize<'de>>(
        &self,
        token: &str,
        typ: Option<&str>,
    ) -> Result<T> {
        let header: Header =
            decode_header(token).map_err(|e| Error::InvalidToken(e.to_string()))?;
        if header.alg != Algorithm::ES256 {
            return Err(Error::InvalidToken(String::from("unexpected algorithm")));
        }
        if let Some(typ) = typ
            && !header
                .typ
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case(typ))
        {
            return Err(Error::InvalidToken(String::from("not an access token")));
        }
        let kid: String = header
            .kid
            .ok_or_else(|| Error::InvalidToken(String::from("no kid")))?;
        let key = self.keys.get(&self.http, &kid).await?;
        let mut validation: Validation = Validation::new(Algorithm::ES256);
        validation.set_issuer(&[self.config.issuer.as_str()]);
        validation.set_audience(&[self.config.client_id.as_str()]);
        validation.leeway = 30;
        decode::<T>(token, &key, &validation)
            .map(|d| d.claims)
            .map_err(|e| Error::InvalidToken(e.to_string()))
    }

    /// Verifies an ID token: signature, ES256, issuer, audience (your client
    /// id), expiry, and the nonce when given.
    ///
    /// # Errors
    /// [`Error::InvalidToken`].
    pub async fn verify_id_token(&self, token: &str, nonce: Option<&str>) -> Result<IdClaims> {
        let claims: IdClaims = self.decode(token, None).await?;
        if let Some(nonce) = nonce
            && claims.nonce.as_deref() != Some(nonce)
        {
            return Err(Error::InvalidToken(String::from("nonce mismatch")));
        }
        Ok(claims)
    }

    /// Verifies an access token sent to *your* API (for example by your
    /// native app): signature, `typ: at+jwt`, issuer, audience = your client
    /// id, expiry (30 s of clock skew allowed). Tokens issued to other apps
    /// are refused, and so is a token whose `fam` has been revoked (see
    /// [`crate::revocation`]; the set is fed by [`Accounts::handle_webhook`]
    /// and [`Accounts::start_revocation_poller`]). The signing keys are
    /// fetched from `/oidc/jwks`, cached, and refetched on an unknown `kid`.
    /// Never calls the accounts service per token.
    ///
    /// # Errors
    /// [`Error::InvalidToken`].
    pub async fn verify_access_token(&self, token: &str) -> Result<AccessClaims> {
        let claims: AccessClaims = self.decode(token, Some("at+jwt")).await?;
        if claims
            .fam
            .as_deref()
            .is_some_and(|fam| self.revoked.is_revoked(fam))
        {
            return Err(Error::InvalidToken(String::from("token revoked")));
        }
        Ok(claims)
    }

    /// The person's profile from `/me`, with their access token.
    ///
    /// Deprecated: a project server must not hold a player's token. Use the
    /// ID token's claims from [`Accounts::finish_login`], then
    /// [`Accounts::player`]`(sub)?.profile()`. Removed in 0.10.0.
    ///
    /// # Errors
    /// The token was refused or the request failed.
    #[deprecated(
        since = "0.9.0",
        note = "use the ID token's claims, then `accounts.player(sub)?.profile()`; removed in 0.10.0"
    )]
    pub async fn userinfo(&self, access_token: &str) -> Result<Value> {
        crate::friends::call(
            self,
            access_token,
            reqwest::Method::GET,
            &format!("{}/me", self.config.issuer),
            None,
        )
        .await
    }
}

fn http() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("toddstack-accounts/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| Error::Config(e.to_string()))
}
