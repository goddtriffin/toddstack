//! Devices: how your server tells the accounts service which phone to push to.
//!
//! [`Accounts::register_device`] and [`Accounts::unregister_device`] authenticate as your project (HTTP Basic, a
//! confidential client). Native apps never call the accounts service: the app sends its token to your server,
//! which registers it for the signed-in player.
//!
//! Both calls are idempotent, so this crate retries an intermittent failure itself.

use serde::Serialize;
use serde_json::{Value, json};

use crate::tokens::Accounts;
use crate::{Error, Result};

/// The push service a device token belongs to.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    /// APNs. The token is hex (32-200 characters).
    Ios,
    /// FCM. The token is opaque printable text (up to 4096 characters).
    Android,
}

/// Which push environment an iOS token was issued for.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    /// Development builds.
    Sandbox,
    /// Builds shipped to testers and the App Store.
    Production,
}

/// A device to register with [`Accounts::register_device`].
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Device {
    /// The player's `sub`; they must have signed in to your project.
    pub sub: String,
    pub platform: Platform,
    /// The APNs or FCM token.
    pub token: String,
    /// The app's bundle (iOS) or package (Android) id: one an admin registered for your project.
    pub app_id: String,
    pub environment: Environment,
}

impl Accounts {
    /// Registers a device for a player, or moves it to them if it was another player's (one phone, one person;
    /// signing in on a shared phone takes over its pushes). At most 10 devices are kept per player; registering
    /// more drops the least recently registered.
    ///
    /// # Errors
    /// [`Error::Config`] without a client secret; [`Error::NotFound`] when the player never signed in to your
    /// project; [`Error::Conflict`] when the token is registered to another project; [`Error::Api`] for a
    /// malformed token or app id (400) or an app id not registered to your project (403); or the request failed.
    pub async fn register_device(&self, device: &Device) -> Result<()> {
        let body: Value = serde_json::to_value(device).map_err(|e| Error::Api {
            status: 0,
            message: e.to_string(),
        })?;
        self.project_call(reqwest::Method::POST, "/devices", Some(body), true)
            .await?;
        Ok(())
    }

    /// Forgets a device token (on sign-out). Succeeds whether or not it was registered, and only ever touches
    /// your project's own devices.
    ///
    /// # Errors
    /// [`Error::Config`] without a client secret, or the request failed.
    pub async fn unregister_device(&self, platform: Platform, token: &str) -> Result<()> {
        let body: Value = json!({ "platform": platform, "token": token });
        self.project_call(reqwest::Method::DELETE, "/devices", Some(body), true)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_serializes_to_the_wire_shape() {
        let expected: Value = json!({
            "sub": "u1",
            "platform": "ios",
            "token": "ab12",
            "app_id": "me.toddgriffin.boggledygook",
            "environment": "sandbox",
        });
        let actual: Value = serde_json::to_value(Device {
            sub: String::from("u1"),
            platform: Platform::Ios,
            token: String::from("ab12"),
            app_id: String::from("me.toddgriffin.boggledygook"),
            environment: Environment::Sandbox,
        })
        .unwrap();
        assert_eq!(expected, actual);
    }

    #[test]
    fn every_platform_and_environment_uses_the_service_spelling() {
        let expected: Value = json!(["ios", "android", "sandbox", "production"]);
        let actual: Value = json!([
            Platform::Ios,
            Platform::Android,
            Environment::Sandbox,
            Environment::Production,
        ]);
        assert_eq!(expected, actual);
    }
}
