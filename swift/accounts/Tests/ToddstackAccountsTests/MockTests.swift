import Foundation
import Testing
import ToddstackAccounts
import ToddstackAccountsTesting

@Suite struct MockTests {
    private let request = HTTPRequest(url: URL(string: "https://x.test/a")!, method: "GET")
    private let other = HTTPRequest(url: URL(string: "https://x.test/b")!, method: "GET")

    @Test func transportReturnsTheSeededResponseAndRecordsTheCall() async throws {
        let mock = MockAccountsTransport()
        mock.seedSend(request, response: .success(HTTPResponse(status: 201)))
        #expect(HTTPResponse(status: 201) == (try await mock.send(request)))
        #expect([MockAccountsTransport.SendArgs(request: request)] == mock.calls)
        mock.verifyExhausted()
    }

    @Test func transportResolvesByArgumentsNotCallOrder() async throws {
        let mock = MockAccountsTransport()
        mock.seedSend(request, response: .success(HTTPResponse(status: 1)))
        mock.seedSend(other, response: .success(HTTPResponse(status: 2)))
        #expect(2 == (try await mock.send(other)).status)
        #expect(1 == (try await mock.send(request)).status)
        mock.verifyExhausted()
    }

    @Test func transportAnswersTheSameRequestInFifoOrder() async throws {
        let mock = MockAccountsTransport()
        mock.seedSend(request, response: .success(HTTPResponse(status: 1)))
        mock.seedSend(request, response: .failure(AccountsError.network("second")))
        #expect(1 == (try await mock.send(request)).status)
        await #expect(throws: AccountsError.network("second")) { try await mock.send(request) }
        mock.verifyExhausted()
    }

    @Test func transportPanicsWhenUnseeded() async {
        await #expect(processExitsWith: .failure) {
            _ = try? await MockAccountsTransport().send(HTTPRequest(url: URL(string: "https://x.test/a")!, method: "GET"))
        }
    }

    @Test func transportExhaustivenessFailsWhenAResponseIsUnconsumed() async {
        await #expect(processExitsWith: .failure) {
            let mock = MockAccountsTransport()
            mock.seedSend(HTTPRequest(url: URL(string: "https://x.test/a")!, method: "GET"), response: .success(HTTPResponse(status: 200)))
            mock.verifyExhausted()
        }
    }

    @Test func authenticatorReturnsTheSeededURLAndRecordsTheCall() async throws {
        let mock = MockWebAuthenticator()
        let url = URL(string: "https://x.test/in")!
        mock.seedAuthenticate(url: url, callbackURL: callback, response: .success(callback))
        #expect(callback == (try await mock.authenticate(url: url, callbackURL: callback)))
        #expect([MockWebAuthenticator.AuthenticateArgs(url: url, callbackURL: callback)] == mock.calls)
        mock.verifyExhausted()
    }

    @Test func authenticatorAnswersInFifoOrder() async throws {
        let mock = MockWebAuthenticator()
        let url = URL(string: "https://x.test/in")!
        let first = URL(string: "https://x.test/1")!
        let second = URL(string: "https://x.test/2")!
        mock.seedAuthenticate(url: url, callbackURL: callback, response: .success(first))
        mock.seedAuthenticate(url: url, callbackURL: callback, response: .success(second))
        #expect(first == (try await mock.authenticate(url: url, callbackURL: callback)))
        #expect(second == (try await mock.authenticate(url: url, callbackURL: callback)))
        mock.verifyExhausted()
    }

    @Test func authenticatorPanicsWhenUnseeded() async {
        await #expect(processExitsWith: .failure) {
            let url = URL(string: "https://x.test/in")!
            _ = try? await MockWebAuthenticator().authenticate(url: url, callbackURL: url)
        }
    }

    @Test func authenticatorExhaustivenessFailsWhenAResponseIsUnconsumed() async {
        await #expect(processExitsWith: .failure) {
            let url = URL(string: "https://x.test/in")!
            let mock = MockWebAuthenticator()
            mock.seedAuthenticate(url: url, callbackURL: url, response: .success(url))
            mock.verifyExhausted()
        }
    }
}
