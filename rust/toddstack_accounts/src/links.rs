//! Links from your project to the accounts site, tagged with your client id.
//!
//! Every link to `accounts.toddgriffin.me` carries `?client=<your client id>`
//! (the canonical pattern): the account pages then show "Back to <your
//! project>", and Todd can see which project sent people there.

use crate::Config;

impl Config {
    /// `path` on the accounts site (the issuer's origin), tagged with this
    /// client: `account_url("/account")` is
    /// `https://accounts.toddgriffin.me/account?client=<client id>`. A query
    /// already in `path` is kept; a fragment stays last.
    #[must_use]
    pub fn account_url(&self, path: &str) -> String {
        let origin: &str = self
            .issuer
            .trim_end_matches('/')
            .strip_suffix("/oidc")
            .unwrap_or(self.issuer.trim_end_matches('/'));
        let (path, fragment): (&str, Option<&str>) = match path.split_once('#') {
            Some((p, f)) => (p, Some(f)),
            None => (path, None),
        };
        let slash: &str = if path.starts_with('/') { "" } else { "/" };
        let join: char = if path.contains('?') { '&' } else { '?' };
        let mut url: String = format!(
            "{origin}{slash}{path}{join}client={}",
            encode(&self.client_id)
        );
        if let Some(f) = fragment {
            url.push('#');
            url.push_str(f);
        }
        url
    }
}

/// Percent-encodes everything but unreserved characters.
fn encode(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out: String = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::Config;

    #[test]
    fn tagged_with_the_client() {
        let c: Config = Config::toddgriffin(
            "boggledygook",
            None,
            "https://www.boggledygook.com/auth/callback",
        );
        assert_eq!(
            c.account_url("/account"),
            "https://accounts.toddgriffin.me/account?client=boggledygook"
        );
        assert_eq!(
            c.account_url("account/sessions"),
            "https://accounts.toddgriffin.me/account/sessions?client=boggledygook"
        );
        assert_eq!(
            c.account_url("/account?tab=1#top"),
            "https://accounts.toddgriffin.me/account?tab=1&client=boggledygook#top"
        );
        let local: Config = Config {
            issuer: String::from("http://localhost:8790/oidc/"),
            client_id: String::from("a b"),
            client_secret: None,
            redirect_uri: String::new(),
        };
        assert_eq!(
            local.account_url("/account"),
            "http://localhost:8790/account?client=a%20b"
        );
    }
}
