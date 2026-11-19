use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How a [`Effect::Navigate`] changes the history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NavigateOp {
    Push,
    Replace,
    Back,
}

/// The app-defined effect type of an app that needs none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoEffect {}

/// A capability the shell implements. The built-in set is small and universal; anything else
/// (haptics, sound, share sheets, push registration, review prompts) is the app's own effect type
/// `X`, carried by [`Effect::App`] and serialized flat next to the built-ins, so a shell
/// matches on one key: `{"id":3,"Http":{...}}` or `{"id":4,"Vibrate":{"ms":20}}`.
///
/// Unit-like variants are empty structs so they stay JSON objects and flatten next to the request id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Effect<X = NoEffect> {
    Http {
        method: String,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<Value>,
        /// Attach the session's credential; the shell owns the token.
        #[serde(default)]
        auth: bool,
        /// Extra request headers the shell adds as given (for example the core's version stamp).
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
    },
    /// Resolves with `{"fired":true}` after `after_ms`.
    Timer {
        after_ms: u64,
    },
    CancelTimer {
        timer_id: u64,
    },
    /// Set when `value` is present, otherwise read (resolves with the value or null).
    Store {
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
    },
    /// Like `Store` for secrets: Keychain / Keystore on native, never plain browser storage.
    Secure {
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    /// Resolves with `n` random bytes as a hex string.
    Random {
        n: u32,
    },
    /// Change the address / screen stack.
    Navigate {
        op: NavigateOp,
        #[serde(default)]
        url: String,
    },
    /// Open the sign-in page in a system browser session. Resolves with the callback URL.
    OpenAuthBrowser {
        url: String,
    },
    /// Web only: reload the page to fetch the latest bundle.
    Reload {},
    /// The shell fetches the named blob and calls `provide_blob(name, bytes)`.
    LoadBlob {
        name: String,
    },
    /// A timing for telemetry. Fire and forget: the shell reports a span of `ms` named `name`.
    Span {
        name: String,
        ms: u64,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        tags: BTreeMap<String, String>,
    },
    /// Fire and forget.
    Log {
        level: String,
        msg: String,
    },
    /// Fire and forget: send an error to the shell's error tracker.
    Report {
        error: String,
    },
    /// An effect the app defined.
    #[serde(untagged)]
    App(X),
}

impl<X> Effect<X> {
    /// An `Http` effect with no body, no credential and no extra headers.
    #[must_use]
    pub fn http(method: &str, path: &str) -> Self {
        Self::Http {
            method: method.to_owned(),
            path: path.to_owned(),
            body: None,
            auth: false,
            headers: BTreeMap::new(),
        }
    }
}

/// An effect with its request id, serialized flat: `{"id":3,"Http":{...}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectReq<X = NoEffect> {
    pub id: u64,
    #[serde(flatten)]
    pub effect: Effect<X>,
}

/// The `ok` payload of an answered [`Effect::Http`]. `sent` and `recv` are local wall-clock ms.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct HttpReply {
    pub status: u16,
    #[serde(default)]
    pub body: Value,
    #[serde(default)]
    pub sent: u64,
    #[serde(default)]
    pub recv: u64,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    enum Mine {
        Vibrate { ms: u32 },
        Beep,
    }

    #[test]
    fn built_in_and_app_effects_serialize_flat_next_to_the_id() {
        let built_in: EffectReq<Mine> = EffectReq {
            id: 3,
            effect: Effect::http("GET", "/x"),
        };
        let mine: EffectReq<Mine> = EffectReq {
            id: 4,
            effect: Effect::App(Mine::Vibrate { ms: 20 }),
        };
        assert_eq!(
            json!({"id": 3, "Http": {"method": "GET", "path": "/x", "auth": false}}),
            serde_json::to_value(&built_in).unwrap()
        );
        assert_eq!(
            json!({"id": 4, "Vibrate": {"ms": 20}}),
            serde_json::to_value(&mine).unwrap()
        );
    }

    #[test]
    fn a_flat_effect_reads_back_to_the_same_variant() {
        let built_in: EffectReq<Mine> =
            serde_json::from_value(json!({"id": 1, "Timer": {"after_ms": 5}})).unwrap();
        let mine: EffectReq<Mine> = serde_json::from_value(json!({"id": 2, "Beep": null})).unwrap();
        assert_eq!(Effect::Timer { after_ms: 5 }, built_in.effect);
        assert_eq!(Effect::App(Mine::Beep), mine.effect);
    }
}
