import AuthenticationServices
import Foundation
import os

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
        let ephemeral: Bool = prefersEphemeralSession
        let callback: ASWebAuthenticationSession.Callback
        if callbackURL.scheme?.lowercased() == "https" {
            guard let host = callbackURL.host else { throw AccountsError.config("callbackURL has no host") }
            callback = .https(host: host, path: callbackURL.path)
        } else {
            // a reverse-DNS private-use scheme (RFC 8252), for local debug builds
            guard let scheme = callbackURL.scheme else { throw AccountsError.config("callbackURL has no scheme") }
            callback = .customScheme(scheme)
        }
        return try await WebAuthRun().start(url: url, callback: callback, ephemeral: ephemeral)
    }
}

/// Resumes a continuation at most once, whichever of the completion handler or a failed start gets there first.
private final class OnceResumer: Sendable {
    private let continuation: OSAllocatedUnfairLock<CheckedContinuation<URL, any Error>?>

    init(_ continuation: CheckedContinuation<URL, any Error>) {
        self.continuation = OSAllocatedUnfairLock(initialState: continuation)
    }

    func resume(with result: Result<URL, any Error>) {
        let taken: CheckedContinuation<URL, any Error>? = continuation.withLock { value in
            defer { value = nil }
            return value
        }
        taken?.resume(with: result)
    }
}

/// Holds the session and the presentation anchor for one run; main actor because the sheet is UI.
@MainActor
private final class WebAuthRun: NSObject, ASWebAuthenticationPresentationContextProviding {
    private var session: ASWebAuthenticationSession?
    private var cancelled: Bool = false

    /// Cancelling the calling task closes the sheet; the completion handler then reports `cancelled`.
    func start(url: URL, callback: ASWebAuthenticationSession.Callback, ephemeral: Bool) async throws -> URL {
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<URL, any Error>) in
                let once = OnceResumer(continuation)
                if cancelled {
                    once.resume(with: .failure(AccountsError.cancelled))
                    return
                }
                let session = ASWebAuthenticationSession(url: url, callback: callback) { callbackURL, error in
                    if let callbackURL {
                        once.resume(with: .success(callbackURL))
                    } else if let error = error as? ASWebAuthenticationSessionError, error.code == .canceledLogin {
                        once.resume(with: .failure(AccountsError.cancelled))
                    } else {
                        once.resume(with: .failure(AccountsError.network(error?.localizedDescription ?? "sign-in failed")))
                    }
                }
                session.presentationContextProvider = self
                session.prefersEphemeralWebBrowserSession = ephemeral
                self.session = session
                if !session.start() {
                    once.resume(with: .failure(AccountsError.network("the sign-in sheet could not start")))
                }
            }
        } onCancel: {
            Task { @MainActor in self.cancel() }
        }
    }

    private func cancel() {
        cancelled = true
        session?.cancel()
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
