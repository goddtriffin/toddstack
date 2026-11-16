import Foundation
import ToddstackAccounts
import os

/// A mock `WebAuthenticator`; same design and the same `verifyExhausted()` rule as `MockAccountsTransport`.
public final class MockWebAuthenticator: WebAuthenticator {
    /// The owned parameters of one `authenticate` call.
    public struct AuthenticateArgs: Equatable, Sendable {
        public let url: URL
        public let callbackURL: URL

        public init(url: URL, callbackURL: URL) {
            self.url = url
            self.callbackURL = callbackURL
        }
    }

    private struct Seed {
        let args: AuthenticateArgs
        var responses: [Result<URL, any Error>]
    }

    private struct State {
        var seeds: [Seed] = []
        var calls: [AuthenticateArgs] = []
    }

    private let state = OSAllocatedUnfairLock(initialState: State())

    public init() {}

    public var calls: [AuthenticateArgs] { state.withLock { $0.calls } }

    public func seedAuthenticate(url: URL, callbackURL: URL, response: Result<URL, any Error>) {
        let args = AuthenticateArgs(url: url, callbackURL: callbackURL)
        state.withLock { state in
            if let index = state.seeds.firstIndex(where: { $0.args == args }) {
                state.seeds[index].responses.append(response)
            } else {
                state.seeds.append(Seed(args: args, responses: [response]))
            }
        }
    }

    public func authenticate(url: URL, callbackURL: URL) async throws -> URL {
        let args = AuthenticateArgs(url: url, callbackURL: callbackURL)
        let taken: Result<URL, any Error>? = state.withLock { state in
            state.calls.append(args)
            guard let index = state.seeds.firstIndex(where: { $0.args == args && !$0.responses.isEmpty }) else { return nil }
            return state.seeds[index].responses.removeFirst()
        }
        guard let taken else {
            fatalError("mock_no_response_configured: MockWebAuthenticator.authenticate(\(args))")
        }
        return try taken.get()
    }

    public func verifyExhausted() {
        let left: [String] = state.withLock { state in
            state.seeds.filter { !$0.responses.isEmpty }.map { "authenticate(\($0.args)) x\($0.responses.count)" }
        }
        if !left.isEmpty {
            fatalError("mock_unconsumed_responses: MockWebAuthenticator: " + left.joined(separator: ", "))
        }
    }
}
