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
        #expect(expected == h.client.currentSession)
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
        #expect(h.client.currentSession == nil)
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
        #expect(h.client.currentSession == nil)
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
        #expect("new-tok" == h.client.currentSession?.token)
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
}
