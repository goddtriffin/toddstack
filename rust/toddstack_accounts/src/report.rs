//! Reporting someone (their username, picture or behaviour) to Todd's review
//! queue. A signed-in person reports through [`crate::friends::Friends::report`]
//! with their own token; a guest has no token, so your server reports for them
//! with [`Accounts::report_as_guest`] and your client credentials.
//!
//! Only accounts can be reported, never guests. Reporting is always answered
//! the same way, so show "Thanks, we'll take a look" whatever comes back short
//! of an error.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::tokens::Accounts;
use crate::{Error, Result};

/// Why the person is being reported.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Their username.
    Name,
    /// Their profile picture.
    Picture,
    Cheating,
    Harassment,
    Other,
}

/// The service refuses notes longer than this many characters.
pub const MAX_NOTE_CHARS: usize = 500;

/// A report your server files for a guest.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct GuestReport {
    /// The account being reported.
    pub sub: String,
    pub reason: Reason,
    /// What happened, in the guest's words (at most [`MAX_NOTE_CHARS`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Your own stable id for the guest, so their reports are limited and
    /// deduplicated as one reporter. Without it, the IP is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest_id: Option<String>,
    /// The guest's verified IP (the one your nginx passed on), kept with the
    /// report for abuse review.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest_ip: Option<String>,
}

pub(crate) fn body(sub: &str, reason: Reason, note: Option<&str>) -> Value {
    let mut body: Value = json!({ "sub": sub, "reason": reason });
    if let Some(note) = note {
        body["note"] = Value::from(note);
    }
    body
}

impl Accounts {
    /// Reports someone for a guest, authenticated with your client
    /// credentials (so the config needs its client secret).
    ///
    /// # Errors
    /// No client secret configured, a refused report (400 reporting yourself
    /// or a bad reason or note, 404 unknown account or a guest, 429 too many),
    /// or the request failed.
    pub async fn report_as_guest(&self, report: &GuestReport) -> Result<()> {
        let secret: &str = self.config().client_secret.as_deref().ok_or_else(|| {
            Error::Config(String::from(
                "reporting for a guest needs the client secret: set Config::client_secret",
            ))
        })?;
        let url: String = format!(
            "{}/api/v1/reports",
            self.config().issuer.trim_end_matches("/oidc")
        );
        // RFC 6749 2.3.1, as at the token endpoint: form-encode both parts.
        let encode =
            |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
        let response = self
            .http()
            .post(url)
            .basic_auth(encode(&self.config().client_id), Some(encode(secret)))
            .json(report)
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let text: String = response.text().await?;
        let message: String = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or(text);
        Err(Error::Api {
            status: status.as_u16(),
            message,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_use_the_service_names() {
        let expected: Value = json!(["name", "picture", "cheating", "harassment", "other"]);
        let actual: Value = serde_json::to_value([
            Reason::Name,
            Reason::Picture,
            Reason::Cheating,
            Reason::Harassment,
            Reason::Other,
        ])
        .unwrap();
        assert_eq!(expected, actual);
    }

    #[test]
    fn guest_reports_leave_out_what_was_not_given() {
        let report: GuestReport = GuestReport {
            sub: String::from("u1"),
            reason: Reason::Harassment,
            note: None,
            guest_id: Some(String::from("g1")),
            guest_ip: None,
        };
        let expected: Value = json!({ "sub": "u1", "reason": "harassment", "guest_id": "g1" });
        let actual: Value = serde_json::to_value(&report).unwrap();
        assert_eq!(expected, actual);
    }

    #[test]
    fn a_person_report_carries_the_note_only_when_given() {
        assert_eq!(
            json!({ "sub": "u1", "reason": "name" }),
            body("u1", Reason::Name, None)
        );
        assert_eq!(
            json!({ "sub": "u1", "reason": "other", "note": "spam" }),
            body("u1", Reason::Other, Some("spam"))
        );
    }
}
