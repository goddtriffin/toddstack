import Foundation
import ToddstackAccounts
import ToddstackAccountsTesting
import os

let server: URL = URL(string: "https://www.boggledygook.com")!
let callback: URL = URL(string: "https://www.boggledygook.com/app/callback")!

func makeConfig() throws -> AccountsConfig {
    try AccountsConfig(serverURL: server, callbackURL: callback, clientId: "boggledygook")
}

/// Deterministic randomness: the first request is 0x01 bytes, the second 0x02, and so on.
func countingRandom() -> @Sendable (Int) -> Data {
    let counter = OSAllocatedUnfairLock(initialState: UInt8(0))
    return { count in
        let fill: UInt8 = counter.withLock { value in value += 1; return value }
        return Data(repeating: fill, count: count)
    }
}

let verifier: String = base64URL(Data(repeating: 1, count: 32))
let state: String = base64URL(Data(repeating: 2, count: 32))

func signInURL(prompt: Bool = false) -> URL {
    let challenge: String = pkceChallenge(verifier: verifier)
    let extra: String = prompt ? "&prompt=login" : ""
    return URL(string: "https://www.boggledygook.com/app/sign-in?code_challenge=\(challenge)&state=\(state)\(extra)")!
}

func callbackURL(_ query: String) -> URL { URL(string: "https://www.boggledygook.com/app/callback?\(query)")! }

func json(_ text: String) -> Data { Data(text.utf8) }

func sessionBody(token: String, guest: Bool = false, expires: String = "null") -> String {
    #"{"token":"\#(token)","expires_at":\#(expires),"player":{"id":"p1","username":"todd","picture":null,"guest":\#(guest)}}"#
}

func player(guest: Bool = false) -> Player { Player(id: "p1", username: "todd", picture: nil, guest: guest) }

func sessionRequest(code: String, bearer: String?) -> HTTPRequest {
    var headers: [String: String] = ["Accept": "application/json", "Content-Type": "application/json"]
    if let bearer { headers["Authorization"] = "Bearer \(bearer)" }
    return HTTPRequest(
        url: URL(string: "https://www.boggledygook.com/api/v1/app/session")!,
        method: "POST",
        headers: headers,
        body: json(#"{"code":"\#(code)","code_verifier":"\#(verifier)"}"#)
    )
}

func bearerRequest(_ path: String, token: String) -> HTTPRequest {
    HTTPRequest(
        url: URL(string: "https://www.boggledygook.com\(path)")!,
        method: "POST",
        headers: ["Accept": "application/json", "Content-Type": "application/json", "Authorization": "Bearer \(token)"]
    )
}

struct Harness {
    let client: AccountsClient
    let transport: MockAccountsTransport
    let web: MockWebAuthenticator
    let store: InMemorySessionStore

    init(stored: Session? = nil, now: @escaping @Sendable () -> Date = { Date() }) throws {
        transport = MockAccountsTransport()
        web = MockWebAuthenticator()
        store = InMemorySessionStore(stored)
        client = AccountsClient(
            config: try makeConfig(), transport: transport, webAuthenticator: web, store: store,
            random: countingRandom(), now: now
        )
    }

    func verify() {
        transport.verifyExhausted()
        web.verifyExhausted()
    }
}
