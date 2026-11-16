import AuthenticationServices
import Foundation

#if canImport(UIKit)
import UIKit
#elseif canImport(AppKit)
import AppKit
#endif

/// The production web authenticator on `ASWebAuthenticationSession`, for iOS and macOS.
///
/// The session is not ephemeral by default, so the browser's accounts cookie gives silent sign-in. The callback is a
/// claimed https URL (Universal Link), which needs Associated Domains on the app.
public struct ASWebAuthenticator: WebAuthenticator {
    private let prefersEphemeralSession: Bool

    public init(prefersEphemeralSession: Bool = false) {
        self.prefersEphemeralSession = prefersEphemeralSession
    }

    public func authenticate(url: URL, callbackURL: URL) async throws -> URL {
        guard let host = callbackURL.host else {
            throw AccountsError.config("callbackURL has no host")
        }
        let path: String = callbackURL.path
        let ephemeral: Bool = prefersEphemeralSession
        return try await WebAuthRun().start(url: url, host: host, path: path, ephemeral: ephemeral)
    }
}

/// Holds the session and the presentation anchor for one run; main actor because the sheet is UI.
@MainActor
private final class WebAuthRun: NSObject, ASWebAuthenticationPresentationContextProviding {
    private var session: ASWebAuthenticationSession?

    func start(url: URL, host: String, path: String, ephemeral: Bool) async throws -> URL {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<URL, any Error>) in
            let session = ASWebAuthenticationSession(url: url, callback: .https(host: host, path: path)) { callbackURL, error in
                if let callbackURL {
                    continuation.resume(returning: callbackURL)
                } else if let error = error as? ASWebAuthenticationSessionError, error.code == .canceledLogin {
                    continuation.resume(throwing: AccountsError.cancelled)
                } else {
                    continuation.resume(throwing: AccountsError.network(error?.localizedDescription ?? "sign-in failed"))
                }
            }
            session.presentationContextProvider = self
            session.prefersEphemeralWebBrowserSession = ephemeral
            self.session = session
            if !session.start() {
                continuation.resume(throwing: AccountsError.network("the sign-in sheet could not start"))
            }
        }
    }

    func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        #if canImport(UIKit)
        let scenes = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
        return scenes.flatMap(\.windows).first(where: \.isKeyWindow) ?? scenes.first?.windows.first ?? ASPresentationAnchor()
        #else
        return NSApplication.shared.keyWindow ?? NSApplication.shared.windows.first ?? ASPresentationAnchor()
        #endif
    }
}
