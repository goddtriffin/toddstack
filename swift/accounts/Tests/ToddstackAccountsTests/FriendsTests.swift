import Foundation
import Testing
import ToddstackAccounts
import ToddstackAccountsTesting

private func friendsRequest(_ method: String, _ path: String, body: String? = nil) -> HTTPRequest {
    var headers: [String: String] = ["Accept": "application/json", "Authorization": "Bearer tok"]
    if body != nil { headers["Content-Type"] = "application/json" }
    return HTTPRequest(
        url: URL(string: "https://www.boggledygook.com/api/v1\(path)")!,
        method: method,
        headers: headers,
        body: body.map(json)
    )
}

private let alex: String = #"{"sub":"u2","username":"alex","name":null,"picture":null,"since":"2026-11-15T10:00:00Z"}"#
private let alexPerson = Person(sub: "u2", username: "alex", name: nil, picture: nil, since: "2026-11-15T10:00:00Z")

private func signedIn() throws -> (FriendsClient, MockAccountsTransport) {
    let stored = Session(token: "tok", expiresAt: nil, player: player())
    let harness = try Harness(stored: stored)
    return (FriendsClient(accounts: harness.client, transport: harness.transport), harness.transport)
}

@Suite struct FriendsTests {
    @Test func listAndRequestsDecodeThePeople() async throws {
        let (friends, transport) = try signedIn()
        transport.seedSend(friendsRequest("GET", "/friends"), response: .success(HTTPResponse(status: 200, body: json(#"{"friends":[\#(alex)]}"#))))
        transport.seedSend(friendsRequest("GET", "/friends/requests"), response: .success(HTTPResponse(status: 200, body: json(#"{"incoming":[\#(alex)],"outgoing":[]}"#))))
        #expect([alexPerson] == (try await friends.list()))
        #expect(FriendRequests(incoming: [alexPerson], outgoing: []) == (try await friends.requests()))
        transport.verifyExhausted()
    }

    @Test func sendingByUsernameAndByCodeReturnsTheResult() async throws {
        let (friends, transport) = try signedIn()
        transport.seedSend(friendsRequest("POST", "/friends/requests", body: #"{"username":"alex"}"#), response: .success(HTTPResponse(status: 200, body: json(#"{"result":"requested"}"#))))
        transport.seedSend(friendsRequest("POST", "/friends/requests", body: #"{"friend_code":"ABCD2345"}"#), response: .success(HTTPResponse(status: 200, body: json(#"{"result":"already_friends"}"#))))
        #expect(Sent.requested == (try await friends.request(username: "alex")))
        #expect(Sent.alreadyFriends == (try await friends.request(friendCode: "ABCD2345")))
        transport.verifyExhausted()
    }

    @Test func acceptDeclineCancelRemoveBlockAndUnblockHitTheirRoutes() async throws {
        let (friends, transport) = try signedIn()
        let ok = Result<HTTPResponse, any Error>.success(HTTPResponse(status: 204))
        transport.seedSend(friendsRequest("POST", "/friends/requests/u2/accept"), response: ok)
        transport.seedSend(friendsRequest("POST", "/friends/requests/u3/decline"), response: ok)
        transport.seedSend(friendsRequest("DELETE", "/friends/requests/u4"), response: ok)
        transport.seedSend(friendsRequest("DELETE", "/friends/u5"), response: ok)
        transport.seedSend(friendsRequest("POST", "/friends/blocks", body: #"{"sub":"u6"}"#), response: ok)
        transport.seedSend(friendsRequest("DELETE", "/friends/blocks/u7"), response: ok)
        try await friends.accept("u2")
        try await friends.decline("u3")
        try await friends.cancel("u4")
        try await friends.remove("u5")
        try await friends.block("u6")
        try await friends.unblock("u7")
        transport.verifyExhausted()
    }

    @Test func blocksAndRelationshipsDecodeAndANewStateReadsAsUnknown() async throws {
        let (friends, transport) = try signedIn()
        transport.seedSend(friendsRequest("GET", "/friends/blocks"), response: .success(HTTPResponse(status: 200, body: json(#"{"blocked":[\#(alex)],"hidden":["u2"]}"#))))
        transport.seedSend(
            friendsRequest("GET", "/friends/relationships?subs=u2%2Cu3"),
            response: .success(HTTPResponse(status: 200, body: json(#"{"relationships":{"u2":"request_received","u3":"something_new"}}"#)))
        )
        #expect(Blocks(blocked: [alexPerson], hidden: ["u2"]) == (try await friends.blocks()))
        let expected: [String: Relationship] = ["u2": .requestReceived, "u3": .unknown]
        #expect(expected == (try await friends.relationships(["u2", "u3"])))
        transport.verifyExhausted()
    }

    @Test func aServerErrorThrowsFriendsErrorWithTheServersCodeAndMessage() async throws {
        let (friends, transport) = try signedIn()
        transport.seedSend(
            friendsRequest("POST", "/friends/requests", body: #"{"username":"x"}"#),
            response: .success(HTTPResponse(status: 429, body: json(#"{"error":"rate_limited","message":"Please wait an hour."}"#)))
        )
        transport.seedSend(friendsRequest("GET", "/friends"), response: .success(HTTPResponse(status: 502, body: json("<html>"))))
        await #expect(throws: FriendsError(status: 429, code: "rate_limited", message: "Please wait an hour.")) {
            try await friends.request(username: "x")
        }
        await #expect(throws: FriendsError(status: 502, code: "accounts_unavailable", message: "Friends request failed (502).")) {
            try await friends.list()
        }
        transport.verifyExhausted()
    }

    @Test func withNoSessionNothingIsSentAndItThrowsAccountRequired() async throws {
        let harness = try Harness()
        let friends = FriendsClient(accounts: harness.client, transport: harness.transport)
        await #expect(throws: FriendsError(status: 401, code: "account_required", message: "Sign in to use friends.")) {
            try await friends.list()
        }
        #expect(harness.transport.calls.isEmpty)
    }

    @Test func anIdThatCouldLeaveTheFriendsPathsIsRefusedBeforeAnyRequest() async throws {
        let (friends, transport) = try signedIn()
        let refused = FriendsError(status: 400, code: "bad_request", message: "That isn't a valid person id.")
        for bad in ["../../oidc/x", "..", "a/b", "a?b=1", "a#b", "a%2Fb", ""] {
            await #expect(throws: refused) { try await friends.accept(bad) }
            await #expect(throws: refused) { try await friends.decline(bad) }
            await #expect(throws: refused) { try await friends.cancel(bad) }
            await #expect(throws: refused) { try await friends.remove(bad) }
            await #expect(throws: refused) { try await friends.block(bad) }
            await #expect(throws: refused) { try await friends.unblock(bad) }
            await #expect(throws: refused) { try await friends.relationships(["u2", bad]) }
        }
        #expect(transport.calls.isEmpty)
    }
}
