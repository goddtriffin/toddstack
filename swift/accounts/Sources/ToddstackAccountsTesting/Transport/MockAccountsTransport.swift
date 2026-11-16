import Foundation
import ToddstackAccounts
import os

/// A mock `AccountsTransport` (mock design: args record, seeded FIFO queues, call log, exhaustiveness check).
///
/// Responses resolve by request value, never by call order. An unseeded call is a test-authoring bug and traps.
/// Swift has no hook that knows whether a test is already failing, so the automatic scope-exit check is dropped:
/// end every test that seeds anything with `verifyExhausted()`.
public final class MockAccountsTransport: AccountsTransport {
    /// The owned parameters of one `send` call.
    public struct SendArgs: Equatable, Sendable {
        public let request: HTTPRequest

        public init(request: HTTPRequest) { self.request = request }
    }

    private struct Seed {
        let args: SendArgs
        var responses: [Result<HTTPResponse, any Error>]
    }

    private struct State {
        var seeds: [Seed] = []
        var calls: [SendArgs] = []
    }

    private let state = OSAllocatedUnfairLock(initialState: State())

    public init() {}

    /// Every call received, in order.
    public var calls: [SendArgs] { state.withLock { $0.calls } }

    /// Queues `response` for `send(request)`; the same request seeded twice answers in FIFO order.
    public func seedSend(_ request: HTTPRequest, response: Result<HTTPResponse, any Error>) {
        let args = SendArgs(request: request)
        state.withLock { state in
            if let index = state.seeds.firstIndex(where: { $0.args == args }) {
                state.seeds[index].responses.append(response)
            } else {
                state.seeds.append(Seed(args: args, responses: [response]))
            }
        }
    }

    public func send(_ request: HTTPRequest) async throws -> HTTPResponse {
        let args = SendArgs(request: request)
        let taken: Result<HTTPResponse, any Error>? = state.withLock { state in
            state.calls.append(args)
            guard let index = state.seeds.firstIndex(where: { $0.args == args && !$0.responses.isEmpty }) else { return nil }
            return state.seeds[index].responses.removeFirst()
        }
        guard let taken else {
            fatalError("mock_no_response_configured: MockAccountsTransport.send(\(args))")
        }
        return try taken.get()
    }

    /// Traps when a seeded response was never consumed, naming each method, its args and how many are left.
    public func verifyExhausted() {
        let left: [String] = state.withLock { state in
            state.seeds.filter { !$0.responses.isEmpty }.map { "send(\($0.args)) x\($0.responses.count)" }
        }
        if !left.isEmpty {
            fatalError("mock_unconsumed_responses: MockAccountsTransport: " + left.joined(separator: ", "))
        }
    }
}
