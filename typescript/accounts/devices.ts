// Devices: how your server tells the accounts service which phone to push to. `accounts.registerDevice` and
// `accounts.unregisterDevice` authenticate as your project (HTTP Basic, a confidential client). Native apps never call
// the accounts service: the app sends its token to your server, which registers it for the signed-in player.

/** The push service a device token belongs to: APNs (`ios`, a hex token of 32-200 characters) or FCM (`android`, opaque printable text up to 4096). */
export type Platform = "ios" | "android";

/** Which push environment an iOS token was issued for: `sandbox` for development builds, `production` for TestFlight and the App Store. */
export type Environment = "sandbox" | "production";

/** A device to register with `accounts.registerDevice`. */
export interface Device {
  /** The player's `sub`; they must have signed in to your project. */
  sub: string;
  /** Which push service the token belongs to. */
  platform: Platform;
  /** The APNs or FCM token. */
  token: string;
  /** The app's bundle (iOS) or package (Android) id: one an admin registered for your project. */
  appId: string;
  /** The push environment the token was issued for. */
  environment: Environment;
}

/** The JSON body for `POST /api/v1/devices`. */
export function deviceBody(d: Device): Record<string, string> {
  return { sub: d.sub, platform: d.platform, token: d.token, app_id: d.appId, environment: d.environment };
}
