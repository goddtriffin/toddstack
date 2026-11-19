//! Signing out, as one call in your `/logout` route.
//!
//! Your session is your own: end it yourself. [`crate::Accounts::sign_out`]
//! then revokes your refresh token and hands back the accounts service's
//! end-session URL (RP-initiated logout). Redirect the browser there: with a
//! valid `id_token_hint` the service ends the accounts browser session at once,
//! which signs the person out of every project in that browser (never other
//! devices) and sends each of them a `session.signed_out` webhook. Without a
//! hint it asks "Sign out?" first. For a local-only sign-out, set
//! [`Options::local_only`]: the token is revoked and there is no URL.

use crate::Error;
use crate::tokens::Config;

/// What to put on the end-session URL.
#[derive(Clone, Debug, Default)]
pub struct Options<'a> {
    /// The raw ID token from sign-in ([`crate::TokenSet::id_token`], expired
    /// is fine), which you keep with your session. Sent as `id_token_hint`;
    /// with it the service signs the person out without a confirm page.
    pub id_token_hint: Option<&'a str>,
    /// Where the person lands afterwards: a path on your site (`/`, checked
    /// like [`crate::return_to::safe`]) or an absolute URL on your redirect
    /// URI's origin. Anything else is dropped. The resulting URL must also be
    /// registered for your app as a post-logout redirect URI, exactly, or the
    /// service ignores it and shows its own "You're signed out" page.
    pub post_logout_redirect_uri: Option<&'a str>,
    /// Echoed back as `?state=` on that redirect.
    pub state: Option<&'a str>,
    /// Sign out of your app only: revoke the refresh token and return no URL,
    /// so the accounts browser session (and the person's other projects)
    /// stays signed in and "Sign in" signs them straight back in.
    pub local_only: bool,
}

/// The absolute post-logout URL for `target`, if it stays on your site (the
/// origin of `config.redirect_uri`).
#[must_use]
pub fn post_logout_target(config: &Config, target: &str) -> Option<String> {
    let base: url::Url = url::Url::parse(&config.redirect_uri).ok()?;
    let url: url::Url = match crate::return_to::safe(target) {
        Some(path) => base.join(&path).ok()?,
        None => url::Url::parse(target).ok()?,
    };
    let same_site: bool = url.origin() == base.origin()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none();
    same_site.then(|| url.into())
}

/// The end-session URL. Always carries your `client_id`, so the service
/// recognizes your app (and your post-logout URI) even without an ID token,
/// or with one signed by a key it has since retired.
///
/// # Panics
/// If the configured issuer isn't a URL.
#[must_use]
pub fn end_session_url(config: &Config, options: &Options<'_>) -> String {
    let mut url: url::Url =
        url::Url::parse(&format!("{}/session/end", config.issuer)).expect("the issuer is a URL");
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", &config.client_id);
        if let Some(hint) = options.id_token_hint {
            q.append_pair("id_token_hint", hint);
        }
        if let Some(target) = options
            .post_logout_redirect_uri
            .and_then(|t| post_logout_target(config, t))
        {
            q.append_pair("post_logout_redirect_uri", &target);
        }
        if let Some(state) = options.state {
            q.append_pair("state", state);
        }
    }
    url.into()
}

/// What [`crate::Accounts::sign_out`] did.
#[derive(Debug)]
#[must_use]
pub struct SignOut {
    /// The end-session URL (see [`end_session_url`]): redirect the browser
    /// there. `None` when [`Options::local_only`] was set.
    pub url: Option<String>,
    /// Why revoking the refresh token failed, if it did. Sign-out goes ahead
    /// regardless (drop the token from your store either way); log this.
    pub revoke_error: Option<Error>,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn config() -> Config {
        Config::toddgriffin(
            "boggledygook",
            None,
            "https://www.boggledygook.com/auth/callback",
        )
    }

    fn query(url: &str) -> HashMap<String, String> {
        url::Url::parse(url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }

    #[test]
    fn the_end_session_url_carries_every_parameter_the_service_reads() {
        let url: String = end_session_url(
            &config(),
            &Options {
                id_token_hint: Some("id.token.here"),
                post_logout_redirect_uri: Some("/goodbye"),
                state: Some("s1"),
                local_only: false,
            },
        );
        assert_eq!(
            Some("https://accounts.toddgriffin.me/oidc/session/end"),
            url.split('?').next()
        );
        let expected: HashMap<String, String> = [
            ("client_id", "boggledygook"),
            ("id_token_hint", "id.token.here"),
            (
                "post_logout_redirect_uri",
                "https://www.boggledygook.com/goodbye",
            ),
            ("state", "s1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        assert_eq!(expected, query(&url));
    }

    #[test]
    fn without_options_only_the_client_id_is_sent() {
        let url: String = end_session_url(&config(), &Options::default());
        let expected: HashMap<String, String> =
            HashMap::from([(String::from("client_id"), String::from("boggledygook"))]);
        assert_eq!(expected, query(&url));
    }

    #[test]
    fn post_logout_targets_stay_on_your_site() {
        let c: Config = config();
        assert_eq!(
            Some("https://www.boggledygook.com/"),
            post_logout_target(&c, "/").as_deref()
        );
        assert_eq!(
            Some("https://www.boggledygook.com/bye?x=1"),
            post_logout_target(&c, "https://www.boggledygook.com/bye?x=1").as_deref()
        );
        for bad in [
            "//evil.example/",
            "https://evil.example/",
            "http://www.boggledygook.com/",
            "https://www.boggledygook.com.evil.example/",
            "https://user@www.boggledygook.com/",
            "/\\evil.example",
            "goodbye",
            "",
        ] {
            assert_eq!(None, post_logout_target(&c, bad), "{bad:?}");
        }
    }

    #[test]
    fn an_off_site_post_logout_uri_is_dropped_from_the_url() {
        let url: String = end_session_url(
            &config(),
            &Options {
                post_logout_redirect_uri: Some("https://evil.example/"),
                ..Options::default()
            },
        );
        assert!(!query(&url).contains_key("post_logout_redirect_uri"));
    }
}
