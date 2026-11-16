import Foundation

/// The hex string for an APNs device token (the `Data` that
/// `application(_:didRegisterForRemoteNotificationsWithDeviceToken:)` hands you): lowercase, two digits per byte.
/// This is the form your server registers for the player.
public func apnsTokenHex(_ deviceToken: Data) -> String {
    deviceToken.map { String(format: "%02x", $0) }.joined()
}
