import Foundation

/// The browser sheet, behind a protocol so tests never open one.
public protocol WebAuthenticator: Sendable {
    /// Opens `url` in a web-auth session and returns the URL it was redirected to on `callbackURL`'s host and path.
    /// Throws `AccountsError.cancelled` when the person closes the sheet.
    func authenticate(url: URL, callbackURL: URL) async throws -> URL
}
