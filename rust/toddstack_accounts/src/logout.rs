//! Signing out, as one call in your `/logout` route.
//!
//! Your session is your own: end it yourself. [`crate::Accounts::sign_out`]
//! then revokes your refresh token and hands back the accounts service's
//! end-session URL (RP-initiated logout). Redirect there only if the person
//! should leave *every* project: the service asks "Sign out?" and, on yes,
//! signs them out of all of Todd's projects in that browser. To sign out of
//! your app alone, revoke and skip the redirect.

use crate::Error;
use crate::tokens::Config;

/// What to put on the end-session URL.
#[derive(Clone, Debug, Default)]
pub struct Options<'a> {
    /// The ID token from sign-in (expired is fine). Sent as `id_token_hint`.
    pub id_token_hint: Option<&'a str>,
    /// Where the person lands afterwards: a path on your site (`/`, checked
    /// like [`crate::return_to::safe`]) or an absolute URL on your redirect
    /// URI's origin. Anything else is dropped. The resulting URL must also be
    /// registered for your app as a post-logout redirect URI, exactly, or the
    /// service ignores it and shows its own "You're signed out" page.
    pub post_logout_redirect_uri: Option<&'a str>,
    /// Echoed back as `?state=` on that redirect.
    pub state: Option<&'a str>,
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
    /// The end-session URL (see [`end_session_url`]).
    pub url: String,
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
