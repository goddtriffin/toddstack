//! Client for Todd Griffin's Projects accounts (`accounts.toddgriffin.me`).
//!
//! What a project's backend needs to use the shared accounts, and nothing
//! else:
//!
//! - [`login`]: the `OpenID` Connect sign-in round trip (authorization code +
//!   PKCE), as two calls around a redirect.
//! - [`Accounts`]: token exchange, refresh, revocation, and checking ID and
//!   access tokens against the published signing keys.
//! - [`webhook`]: verifying the signed webhooks the accounts service sends.
//! - [`player`]: [`Accounts::player`], everything your server does for a
//!   signed-in player (profile, friends, blocks, events, inbox, mutes, time
//!   zone, reports), as your project, never with the player's token.
//! - [`revocation`]: verifying access tokens locally and refusing a revoked
//!   one at once (webhook plus poller).
//! - [`friends`]: the friends, blocks, events and inbox API with a player's
//!   token. **Deprecated**: removed in 0.10.0, use [`player`].
//! - `friends_router` (feature `axum`): in-app friend requests as one mountable axum router.
//! - [`notifications`]: sending notifications to players from your server, and
//!   the time zone hint behind `send_at_local`.
//! - [`devices`]: registering a player's phone for push from your server.
//! - [`report`]: reporting someone to Todd's review queue, for people and guests.
//! - [`logout`]: signing out (revoke, then the end-session URL) in one call.
//! - [`return_to`]: keeping post-sign-in redirects on your own site.
//! - [`Config::account_url`]: links to the accounts site, tagged with your
//!   client id (every link there carries it).
//!
//! It holds no secrets of its own and never logs tokens. Your app's client
//! secret and webhook secret come from your environment.
//!
//! ```no_run
//! # async fn example() -> Result<(), toddstack_accounts::Error> {
//! use toddstack_accounts::{Accounts, Config, login};
//!
//! let accounts = Accounts::new(Config::toddgriffin(
//!     "boggledygook",
//!     Some(String::from("client secret from 1Password")),
//!     "https://www.boggledygook.com/auth/callback",
//! ))?;
//!
//! // GET /auth/login: send the person to sign in; keep `pending` in your session.
//! let (url, pending) = login::start(accounts.config(), &["openid", "profile", "offline_access"], Some("/play"));
//!
//! // GET /auth/callback?code=..&state=..: finish, then start your own session.
//! # let query = "";
//! let signed_in = accounts.finish_login(&pending, query).await?;
//! println!("{} signed in", signed_in.claims.sub);
//! # Ok(()) }
//! ```

pub mod devices;
mod error;
pub mod friends;
#[cfg(feature = "axum")]
pub mod friends_router;
mod jwks;
mod links;
pub mod login;
pub mod logout;
pub mod notifications;
pub mod player;
pub mod report;
mod retry;
pub mod return_to;
pub mod revocation;
mod tokens;
pub mod webhook;

pub use error::Error;
pub use login::PendingLogin;
pub use tokens::{AccessClaims, Accounts, Config, IdClaims, SignedIn, TokenSet, Visibility};

/// The production issuer. Permanent: every app and passkey depends on it.
pub const ISSUER: &str = "https://accounts.toddgriffin.me/oidc";

/// The accounts service running locally. Local testing never touches production:
/// use [`Config::local`] or [`Config::toddgriffin_at`].
pub const LOCAL_BASE_URL: &str = "http://127.0.0.1:8790";

pub type Result<T, E = Error> = std::result::Result<T, E>;
