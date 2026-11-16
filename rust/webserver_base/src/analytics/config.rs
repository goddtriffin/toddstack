//! What the proxies need to know about their upstreams.

use crate::env::{self, EnvError};
use crate::environment::Environment;

/// The environment variable holding the Plausible script id.
pub const ENV_ANALYTICS_ID: &str = "WSB_ANALYTICS_ID";

/// The environment variable holding the browser Sentry DSN.
pub const ENV_SENTRY_BROWSER_DSN: &str = "WSB_SENTRY_BROWSER_DSN";

/// Plausible's script host.
pub const PLAUSIBLE_HOST: &str = "https://plausible.io";

/// Sentry's loader-script CDN.
pub const SENTRY_CDN_HOST: &str = "https://js.sentry-cdn.com";

/// Which Plausible site events belong to, or that none do.
///
/// Modern Plausible identifies a site by the id embedded in its script URL
/// rather than a `data-domain` attribute, so this is the whole configuration.
/// Deriving it from the request's hostname instead would split `www.` and
/// apex traffic into two sites Plausible does not both know about, and would
/// let a staging deploy pour into production's numbers.
///
/// A config can also be *disabled*: no script tag, no proxy routes, no
/// upstream fetch, no event forwarding. That is what a local run gets, and
/// what a blank or missing id gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyticsConfig {
    script_id: Option<String>,
}

impl AnalyticsConfig {
    /// Events for the site this script id belongs to. A blank id disables
    /// analytics, as [`AnalyticsConfig::disabled`] does.
    #[must_use]
    pub fn new(script_id: impl Into<String>) -> Self {
        let script_id: String = script_id.into();
        Self::from_optional_id(Some(&script_id))
    }

    /// Analytics off: nothing is rendered, mounted, fetched or forwarded.
    #[must_use]
    pub const fn disabled() -> Self {
        Self { script_id: None }
    }

    /// Enabled only for a non-blank id.
    #[must_use]
    pub fn from_optional_id(script_id: Option<&str>) -> Self {
        Self {
            script_id: script_id
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(String::from),
        }
    }

    /// Reads [`ENV_ANALYTICS_ID`] and `WSB_ENVIRONMENT`.
    ///
    /// Disabled when the environment is `local`, or when the id is unset or
    /// blank. Only a production run with an id is enabled.
    ///
    /// # Errors
    ///
    /// [`EnvError`] if `WSB_ENVIRONMENT` is unset, blank or invalid.
    pub fn from_env() -> Result<Self, EnvError> {
        let environment: Environment = Environment::from_env()?;
        Ok(Self::resolve(
            environment,
            env::optional(ENV_ANALYTICS_ID).as_deref(),
        ))
    }

    /// The config for `environment` and an optional id: disabled when local or
    /// when the id is missing or blank.
    #[must_use]
    pub fn resolve(environment: Environment, script_id: Option<&str>) -> Self {
        if environment.is_local() {
            Self::disabled()
        } else {
            Self::from_optional_id(script_id)
        }
    }

    /// This config as it applies in `environment`: always disabled when local,
    /// whatever id was passed in.
    #[must_use]
    pub fn for_environment(self, environment: Environment) -> Self {
        if environment.is_local() {
            Self::disabled()
        } else {
            self
        }
    }

    /// Whether analytics is on.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.script_id.is_some()
    }

    /// The configured script id, if enabled.
    #[must_use]
    pub fn script_id(&self) -> Option<&str> {
        self.script_id.as_deref()
    }

    /// The upstream script URL this proxy fetches, if enabled.
    #[must_use]
    pub fn upstream_script_url(&self) -> Option<String> {
        self.script_id
            .as_ref()
            .map(|id| format!("{PLAUSIBLE_HOST}/js/{id}.js"))
    }

    /// The upstream event endpoint this proxy forwards to.
    #[must_use]
    pub fn upstream_event_url() -> String {
        format!("{PLAUSIBLE_HOST}/api/event")
    }
}

/// A parsed Sentry DSN.
///
/// Only the pieces the tunnel needs: which host to relay to, and which project.
/// Because a frontend declares exactly one browser DSN, the tunnel never has to
/// parse an envelope to discover where an event belongs — which keeps it a dumb
/// byte relay and stops it being usable as an open relay to arbitrary projects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentryDsn {
    public_key: String,
    host: String,
    project_id: String,
}

/// Why a DSN could not be understood.
#[derive(Debug, thiserror::Error)]
#[error("`{dsn}` is not a Sentry DSN; expected `https://<key>@<host>/<project id>`")]
pub struct SentryDsnParseError {
    dsn: String,
}

impl SentryDsn {
    /// Parses `https://<public key>@<host>/<project id>`.
    ///
    /// # Errors
    ///
    /// [`SentryDsnParseError`] if the DSN is not in that shape.
    pub fn parse(dsn: &str) -> Result<Self, SentryDsnParseError> {
        let malformed = || SentryDsnParseError {
            dsn: dsn.to_string(),
        };

        let scheme_end: usize = dsn.find("://").ok_or_else(malformed)? + 3;
        let rest: &str = &dsn[scheme_end..];
        let (public_key, remainder) = rest.split_once('@').ok_or_else(malformed)?;
        let (host, project_id) = remainder.rsplit_once('/').ok_or_else(malformed)?;

        if public_key.is_empty() || host.is_empty() || project_id.is_empty() {
            return Err(malformed());
        }

        Ok(Self {
            public_key: public_key.to_string(),
            host: host.to_string(),
            project_id: project_id.to_string(),
        })
    }

    /// The upstream loader script for this DSN's public key.
    #[must_use]
    pub fn upstream_script_url(&self) -> String {
        format!("{SENTRY_CDN_HOST}/{}.min.js", self.public_key)
    }

    /// Where envelopes are relayed, verbatim.
    #[must_use]
    pub fn upstream_envelope_url(&self) -> String {
        format!("https://{}/api/{}/envelope/", self.host, self.project_id)
    }
}

#[cfg(test)]
mod tests {
    use super::{AnalyticsConfig, SentryDsn};
    use crate::environment::Environment;

    #[test]
    fn local_mode_disables_analytics_even_with_an_id() {
        let config: AnalyticsConfig = AnalyticsConfig::resolve(Environment::Local, Some("pa-x"));
        assert!(!config.is_enabled());
        assert_eq!(None, config.upstream_script_url());

        let config: AnalyticsConfig =
            AnalyticsConfig::new("pa-x").for_environment(Environment::Local);
        assert!(!config.is_enabled());
    }

    #[test]
    fn an_unset_or_blank_id_disables_analytics_in_production() {
        assert!(!AnalyticsConfig::resolve(Environment::Production, None).is_enabled());
        assert!(!AnalyticsConfig::resolve(Environment::Production, Some("")).is_enabled());
        assert!(!AnalyticsConfig::resolve(Environment::Production, Some("  ")).is_enabled());
        assert!(!AnalyticsConfig::new("").is_enabled());
    }

    #[test]
    fn production_with_an_id_is_unchanged() {
        let config: AnalyticsConfig =
            AnalyticsConfig::resolve(Environment::Production, Some("pa-x"));
        assert!(config.is_enabled());
        assert_eq!(Some("pa-x"), config.script_id());
        assert_eq!(config, AnalyticsConfig::new("pa-x"));
        assert_eq!(
            config,
            AnalyticsConfig::new("pa-x").for_environment(Environment::Production)
        );
    }

    #[test]
    fn the_plausible_script_url_embeds_the_site_id() {
        let config: AnalyticsConfig = AnalyticsConfig::new("pa-1qi0TQEvpewNxVHboeeOC");

        let expected: Option<String> = Some(String::from(
            "https://plausible.io/js/pa-1qi0TQEvpewNxVHboeeOC.js",
        ));
        let actual: Option<String> = config.upstream_script_url();
        assert_eq!(expected, actual);
    }

    #[test]
    fn a_dsn_yields_its_loader_script_and_envelope_endpoint() {
        let dsn: SentryDsn =
            SentryDsn::parse("https://abc123@o4504844394627072.ingest.sentry.io/4504862450515968")
                .expect("a well-formed DSN");

        let expected: String = String::from("https://js.sentry-cdn.com/abc123.min.js");
        let actual: String = dsn.upstream_script_url();
        assert_eq!(expected, actual);

        let expected: String = String::from(
            "https://o4504844394627072.ingest.sentry.io/api/4504862450515968/envelope/",
        );
        let actual: String = dsn.upstream_envelope_url();
        assert_eq!(expected, actual);
    }

    #[test]
    fn a_malformed_dsn_is_rejected_at_boot_rather_than_at_the_first_error() {
        assert!(SentryDsn::parse("not-a-dsn").is_err());
        assert!(SentryDsn::parse("https://o123.ingest.sentry.io/456").is_err());
        assert!(SentryDsn::parse("https://key@host/").is_err());
        assert!(SentryDsn::parse("https://@host/456").is_err());
    }
}
