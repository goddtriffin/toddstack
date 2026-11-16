import Foundation
import Testing
import ToddstackAccounts
import ToddstackAccountsTesting

@Suite struct SessionTests {
    private let guestRequest = HTTPRequest(
        url: URL(string: "https://www.boggledygook.com/api/v1/app/guest")!, method: "POST",
        headers: ["Accept": "application/json", "Content-Type": "application/json"]
    )

    @Test func startGuestStoresAGuestSession() async throws {
        let h = try Harness()
        h.transport.seedSend(guestRequest, response: .success(HTTPResponse(status: 200, body: json(sessionBody(token: "g", guest: true)))))
        let session: Session = try await h.client.startGuest()
        #expect(Session(token: "g", expiresAt: nil, player: player(guest: true)) == session)
        #expect(session == h.client.currentSession)
        h.verify()
    }

    @Test func authorizedAddsTheBearerToken() async throws {
        let h = try Harness(stored: Session(token: "tok", expiresAt: nil, player: player()))
        let request: URLRequest = try h.client.authorized(URLRequest(url: URL(string: "https://www.boggledygook.com/api/v1/me")!))
        #expect("Bearer tok" == request.value(forHTTPHeaderField: "Authorization"))
    }

    @Test func authorizedThrowsWhenSignedOut() async throws {
        let h = try Harness()
        #expect(throws: AccountsError.notSignedIn) {
            try h.client.authorized(URLRequest(url: URL(string: "https://www.boggledygook.com/x")!))
        }
    }

    @Test func anExpiredSessionReadsAsSignedOutAndIsDeleted() async throws {
        let past = Session(token: "old", expiresAt: Date(timeIntervalSince1970: 100), player: player())
        let h = try Harness(stored: past, now: { Date(timeIntervalSince1970: 200) })
        #expect(h.client.currentSession == nil)
        #expect(try h.store.load() == nil)
    }

    @Test func aSessionRoundTripsThroughJSONWithFractionalSeconds() async throws {
        let text: String = sessionBody(token: "t", expires: #""2030-01-02T03:04:05.250Z""#)
        let session: Session = try JSONDecoder().decode(Session.self, from: json(text))
        #expect(Date(timeIntervalSince1970: 1_893_553_445.25) == session.expiresAt)
        #expect(session == (try JSONDecoder().decode(Session.self, from: JSONEncoder().encode(session))))
    }

    @Test func signOutPostsDeletesTheTokenAndOpensTheEndSessionURL() async throws {
        let h = try Harness(stored: Session(token: "tok", expiresAt: nil, player: player()))
        let end = URL(string: "https://accounts.toddgriffin.me/oidc/session/end?id_token_hint=x")!
        h.transport.seedSend(bearerRequest("/api/v1/app/sign-out", token: "tok"), response: .success(HTTPResponse(status: 200, body: json(#"{"end_session_url":"\#(end.absoluteString)"}"#))))
        h.web.seedAuthenticate(url: end, callbackURL: callback, response: .success(callback))
        try await h.client.signOut()
        #expect(try h.store.load() == nil)
        h.verify()
    }

    @Test func signOutClearsTheTokenEvenWhenTheNetworkFails() async throws {
        let h = try Harness(stored: Session(token: "tok", expiresAt: nil, player: player()))
        h.transport.seedSend(bearerRequest("/api/v1/app/sign-out", token: "tok"), response: .failure(AccountsError.network("offline")))
        try await h.client.signOut()
        #expect(try h.store.load() == nil)
        #expect(h.web.calls.isEmpty)
        h.verify()
    }

    @Test func signOutIgnoresACancelledEndSessionSheet() async throws {
        let h = try Harness(stored: Session(token: "tok", expiresAt: nil, player: player()))
        let end = URL(string: "https://accounts.toddgriffin.me/oidc/session/end")!
        h.transport.seedSend(bearerRequest("/api/v1/app/sign-out", token: "tok"), response: .success(HTTPResponse(status: 200, body: json(#"{"end_session_url":"\#(end.absoluteString)"}"#))))
        h.web.seedAuthenticate(url: end, callbackURL: callback, response: .failure(AccountsError.cancelled))
        try await h.client.signOut()
        #expect(try h.store.load() == nil)
        h.verify()
    }

    @Test func signOutWithANullEndSessionURLOpensNoSheet() async throws {
        let h = try Harness(stored: Session(token: "tok", expiresAt: nil, player: player()))
        h.transport.seedSend(bearerRequest("/api/v1/app/sign-out", token: "tok"), response: .success(HTTPResponse(status: 200, body: json(#"{"end_session_url":null}"#))))
        try await h.client.signOut()
        #expect(h.web.calls.isEmpty)
        h.verify()
    }

    @Test func signOutNeverOpensANonHttpsEndSessionURL() async throws {
        let h = try Harness(stored: Session(token: "tok", expiresAt: nil, player: player()))
        h.transport.seedSend(bearerRequest("/api/v1/app/sign-out", token: "tok"), response: .success(HTTPResponse(status: 200, body: json(#"{"end_session_url":"http://evil.example/x"}"#))))
        try await h.client.signOut()
        #expect(h.web.calls.isEmpty)
        h.verify()
    }

    @Test func signOutWhenSignedOutMakesNoCalls() async throws {
        let h = try Harness()
        try await h.client.signOut()
        #expect(h.transport.calls.isEmpty)
    }
}
