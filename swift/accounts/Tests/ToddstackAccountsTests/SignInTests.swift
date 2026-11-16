import Foundation
import Testing
import ToddstackAccounts
import ToddstackAccountsTesting

@Suite struct SignInTests {
    @Test func signInExchangesTheCodeAndStoresTheSession() async throws {
        let h = try Harness()
        h.web.seedAuthenticate(url: signInURL(), callbackURL: callback, response: .success(callbackURL("code=one-time&state=\(state)")))
        h.transport.seedSend(sessionRequest(code: "one-time", bearer: nil), response: .success(HTTPResponse(status: 200, body: json(sessionBody(token: "tok", expires: #""2030-01-02T03:04:05Z""#)))))
        let session: Session = try await h.client.signIn()
        let expected = Session(token: "tok", expiresAt: Date(timeIntervalSince1970: 1_893_553_445), player: player())
        #expect(expected == session)
        #expect(expected == (try h.client.currentSession()))
        #expect(expected == (try h.store.load()))
        h.verify()
    }

    @Test func promptLoginAddsPromptLoginToTheSignInURL() async throws {
        let h = try Harness()
        h.web.seedAuthenticate(url: signInURL(prompt: true), callbackURL: callback, response: .success(callbackURL("code=c&state=\(state)")))
        h.transport.seedSend(sessionRequest(code: "c", bearer: nil), response: .success(HTTPResponse(status: 200, body: json(sessionBody(token: "t")))))
        _ = try await h.client.signIn(prompt: .login)
        h.verify()
    }

    @Test func aStateMismatchIsRefusedBeforeAnyNetworkCall() async throws {
        let h = try Harness()
        h.web.seedAuthenticate(url: signInURL(), callbackURL: callback, response: .success(callbackURL("code=c&state=forged")))
        await #expect(throws: AccountsError.stateMismatch) { try await h.client.signIn() }
        #expect(h.transport.calls.isEmpty)
        #expect(try h.client.currentSession() == nil)
        h.verify()
    }

    @Test func aMissingStateIsAMismatchToo() async throws {
        let h = try Harness()
        h.web.seedAuthenticate(url: signInURL(), callbackURL: callback, response: .success(callbackURL("code=c")))
        await #expect(throws: AccountsError.stateMismatch) { try await h.client.signIn() }
        h.verify()
    }

    @Test func aCallbackErrorMapsToAuthorization() async throws {
        let h = try Harness()
        h.web.seedAuthenticate(url: signInURL(), callbackURL: callback, response: .success(callbackURL("error=access_denied&state=\(state)")))
        await #expect(throws: AccountsError.authorization(code: "access_denied")) { try await h.client.signIn() }
        h.verify()
    }

    @Test func aCallbackWithoutCodeIsMissingCode() async throws {
        let h = try Harness()
        h.web.seedAuthenticate(url: signInURL(), callbackURL: callback, response: .success(callbackURL("state=\(state)")))
        await #expect(throws: AccountsError.missingCode) { try await h.client.signIn() }
        h.verify()
    }

    @Test func closingTheSheetIsCancelled() async throws {
        let h = try Harness()
        h.web.seedAuthenticate(url: signInURL(), callbackURL: callback, response: .failure(AccountsError.cancelled))
        await #expect(throws: AccountsError.cancelled) { try await h.client.signIn() }
        h.verify()
    }

    @Test func invalidGrantIsTypedAndStoresNothing() async throws {
        let h = try Harness()
        h.web.seedAuthenticate(url: signInURL(), callbackURL: callback, response: .success(callbackURL("code=used&state=\(state)")))
        h.transport.seedSend(sessionRequest(code: "used", bearer: nil), response: .success(HTTPResponse(status: 400, body: json(#"{"error":"invalid_grant"}"#))))
        await #expect(throws: AccountsError.invalidGrant) { try await h.client.signIn() }
        #expect(try h.client.currentSession() == nil)
        h.verify()
    }

    @Test func otherFailuresAreApiErrorsWithTheStatus() async throws {
        let h = try Harness()
        h.web.seedAuthenticate(url: signInURL(), callbackURL: callback, response: .success(callbackURL("code=c&state=\(state)")))
        h.transport.seedSend(sessionRequest(code: "c", bearer: nil), response: .success(HTTPResponse(status: 503)))
        await #expect(throws: AccountsError.api(status: 503, message: nil)) { try await h.client.signIn() }
        h.verify()
    }

    @Test func aStoredGuestTokenIsSentOnTheExchangeAndReplacedByTheNewToken() async throws {
        let guest = Session(token: "guest-tok", expiresAt: nil, player: player(guest: true))
        let h = try Harness(stored: guest)
        h.web.seedAuthenticate(url: signInURL(), callbackURL: callback, response: .success(callbackURL("code=c&state=\(state)")))
        h.transport.seedSend(sessionRequest(code: "c", bearer: "guest-tok"), response: .success(HTTPResponse(status: 200, body: json(sessionBody(token: "new-tok")))))
        let session: Session = try await h.client.signIn()
        #expect("new-tok" == session.token)
        #expect("new-tok" == (try h.client.currentSession())?.token)
        #expect("Bearer guest-tok" == h.transport.calls.first?.request.headers["Authorization"])
        h.verify()
    }

    @Test func noAuthorizationHeaderIsSentOnTheExchangeWhenNothingIsStored() async throws {
        let h = try Harness()
        h.web.seedAuthenticate(url: signInURL(), callbackURL: callback, response: .success(callbackURL("code=c&state=\(state)")))
        h.transport.seedSend(sessionRequest(code: "c", bearer: nil), response: .success(HTTPResponse(status: 200, body: json(sessionBody(token: "t")))))
        _ = try await h.client.signIn()
        #expect(h.transport.calls.first?.request.headers["Authorization"] == nil)
        h.verify()
    }

    @Test func aSignedInAccountsTokenIsNeverSentOnTheExchange() async throws {
        let account = Session(token: "account-tok", expiresAt: nil, player: player(guest: false))
        let h = try Harness(stored: account)
        h.web.seedAuthenticate(url: signInURL(prompt: true), callbackURL: callback, response: .success(callbackURL("code=c&state=\(state)")))
        h.transport.seedSend(sessionRequest(code: "c", bearer: nil), response: .success(HTTPResponse(status: 200, body: json(sessionBody(token: "other-tok")))))
        _ = try await h.client.signIn(prompt: .login)
        #expect(h.transport.calls.first?.request.headers["Authorization"] == nil)
        h.verify()
    }

    @Test func aSecondSignInWhileOneIsOpenFailsFast() async throws {
        let gate = BlockingWebAuthenticator()
        let client = AccountsClient(config: try makeConfig(), transport: MockAccountsTransport(), webAuthenticator: gate, store: InMemorySessionStore(), random: countingRandom())
        let first = Task { try await client.signIn() }
        await gate.waitUntilOpen()
        await #expect(throws: AccountsError.signInInProgress) { try await client.signIn() }
        await gate.finish(with: AccountsError.cancelled)
        await #expect(throws: AccountsError.cancelled) { try await first.value }
        #expect(await gate.opened == 1)
    }

    @Test func theSignInURLAsksForS256() throws {
        #expect(signInURL().absoluteString.contains("code_challenge_method=S256"))
    }
}
