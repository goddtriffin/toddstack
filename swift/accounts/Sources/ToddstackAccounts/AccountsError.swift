import Foundation

/// Everything that can go wrong signing in a native app. Switch on the case.
public enum AccountsError: Error, Equatable, Sendable {
    /// Bad configuration (for example a callback URL that is not https).
    case config(String)
    /// The callback's `state` does not match the pending sign-in: forged or stale. Start over.
    case stateMismatch
    /// The project server sent the person back with an error code (`access_denied`, `login_required`, ...).
    case authorization(code: String)
    /// The callback had no `code`.
    case missingCode
    /// The server refused the one-time code (`invalid_grant`: expired, reused or wrong verifier). Sign in again.
    case invalidGrant
    /// The person closed the sign-in sheet.
    case cancelled
    /// An authorized call was made with no session.
    case notSignedIn
    /// The server refused a call; `status` and its error code, when it sent one.
    case api(status: Int, message: String?)
    /// The request itself failed (network, timeout).
    case network(String)
    /// The server answered with something unreadable.
    case invalidResponse(String)
    /// The Keychain refused a read, write or delete.
    case storage(String)
}

extension AccountsError: LocalizedError {
    public var errorDescription: String? {
        switch self {
        case .config(let message): "Accounts is misconfigured: \(message)"
        case .stateMismatch: "The sign-in response did not match this sign-in. Please try again."
        case .authorization(let code): "Sign-in was refused (\(code))."
        case .missingCode: "The sign-in response had no code. Please try again."
        case .invalidGrant: "The sign-in code expired or was already used. Please try again."
        case .cancelled: "Sign-in was cancelled."
        case .notSignedIn: "You are not signed in."
        case .api(let status, let message): "The server refused the request (\(status)\(message.map { ": \($0)" } ?? ""))."
        case .network(let message): "The network request failed: \(message)"
        case .invalidResponse(let message): "The server sent an unreadable response: \(message)"
        case .storage(let message): "The Keychain failed: \(message)"
        }
    }
}
