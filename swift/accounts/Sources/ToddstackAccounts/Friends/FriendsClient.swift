import Foundation

/// In-app friend requests: calls the friends routes your project's server mounts (the Rust crate's `friends_router`).
/// The app never sees an accounts token; it authenticates to its own server with the session `AccountsClient` keeps.
///
/// ; a guest session is refused by the server as
/// `FriendsError` with code `account_required` (offer `signIn()`).
public final class FriendsClient: Sendable {
    private let accounts: AccountsClient
    private let transport: any AccountsTransport

    public init(accounts: AccountsClient, transport: any AccountsTransport = URLSessionAccountsTransport()) {
        self.accounts = accounts
        self.transport = transport
    }

    /// Friends who also use your app.
    public func list() async throws -> [Person] {
        struct Body: Decodable { let friends: [Person] }
        return try decode(Body.self, await call("GET", "/friends")).friends
    }

    /// Pending requests, incoming and outgoing.
    public func requests() async throws -> FriendRequests {
        try decode(FriendRequests.self, await call("GET", "/friends/requests"))
    }

    /// Sends a request by exact username. `.accepted` means they had already asked.
    public func request(username: String) async throws -> Sent {
        try await send(["username": username])
    }

    /// Sends a request by friend code (works even when the person only takes requests with their code).
    public func request(friendCode: String) async throws -> Sent {
        try await send(["friend_code": friendCode])
    }

    /// Accepts a request the person received.
    public func accept(_ sub: String) async throws {
        _ = try await call("POST", "/friends/requests/\(try segment(sub))/accept")
    }

    /// Declines a request the person received.
    public func decline(_ sub: String) async throws {
        _ = try await call("POST", "/friends/requests/\(try segment(sub))/decline")
    }

    /// Takes back a request the person sent.
    public func cancel(_ sub: String) async throws {
        _ = try await call("DELETE", "/friends/requests/\(try segment(sub))")
    }

    /// Ends a friendship.
    public func remove(_ sub: String) async throws {
        _ = try await call("DELETE", "/friends/\(try segment(sub))")
    }

    /// Blocks: ends the friendship and requests, both ways.
    public func block(_ sub: String) async throws {
        _ = try await call("POST", "/friends/blocks", body: ["sub": try segment(sub)])
    }

    /// Undoes `block`.
    public func unblock(_ sub: String) async throws {
        _ = try await call("DELETE", "/friends/blocks/\(try segment(sub))")
    }

    /// Who the person blocked, and everyone to keep apart from them.
    public func blocks() async throws -> Blocks {
        try decode(Blocks.self, await call("GET", "/friends/blocks"))
    }

    /// Where the person stands with each of `subs` (1 to 100), keyed by sub: what a friend button should say.
    public func relationships(_ subs: [String]) async throws -> [String: Relationship] {
        struct Body: Decodable { let relationships: [String: Relationship] }
        var allowed: CharacterSet = .urlQueryAllowed
        allowed.remove(charactersIn: ",&+=")
        let query: String = try subs.map(segment).joined(separator: ",").addingPercentEncoding(withAllowedCharacters: allowed) ?? ""
        return try decode(Body.self, await call("GET", "/friends/relationships?subs=\(query)")).relationships
    }

    // MARK: internals

    private func send(_ body: [String: String]) async throws -> Sent {
        struct Result: Decodable { let result: Sent }
        return try decode(Result.self, await call("POST", "/friends/requests", body: body)).result
    }

    /// 1 to 64 of `A-Za-z0-9_-`: a real `sub` is a UUID, and anything else (`.`, `/`, `?`, `#`, `%`) could
    /// redirect the call to another path on the server.
    static func isValidID(_ value: String) -> Bool {
        (1...64).contains(value.utf8.count)
            && value.utf8.allSatisfy { ($0 >= 48 && $0 <= 57) || ($0 >= 65 && $0 <= 90) || ($0 >= 97 && $0 <= 122) || $0 == 45 || $0 == 95 }
    }

    private func segment(_ sub: String) throws -> String {
        guard Self.isValidID(sub) else {
            throw FriendsError(status: 400, code: "bad_request", message: "That isn't a valid person id.")
        }
        return sub
    }

    private func call(_ method: String, _ path: String, body: [String: String]? = nil) async throws -> Data {
        let base: String = accounts.config.serverURL.absoluteString.replacingOccurrences(of: "/+$", with: "", options: .regularExpression)
        guard let url = URL(string: base + "/api/v1" + path) else {
            throw AccountsError.config("friends URL")
        }
        var request = URLRequest(url: url)
        request.httpMethod = method
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        var payload: Data?
        if let body {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            let encoder = JSONEncoder()
            encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
            payload = try encoder.encode(body)
        }
        let signed: URLRequest
        do { signed = try accounts.authorized(request) } catch AccountsError.notSignedIn {
            throw FriendsError(status: 401, code: "account_required", message: "Sign in to use friends.")
        }
        var headers: [String: String] = [:]
        for (name, value) in signed.allHTTPHeaderFields ?? [:] { headers[name] = value }
        let response: HTTPResponse = try await transport.send(
            HTTPRequest(url: url, method: method, headers: headers, body: payload)
        )
        guard (200..<300).contains(response.status) else { throw Self.failure(response) }
        return response.body
    }

    private func decode<T: Decodable>(_ type: T.Type, _ data: Data) throws -> T {
        do { return try JSONDecoder().decode(type, from: data) } catch {
            throw AccountsError.invalidResponse("friends body: \(error.localizedDescription)")
        }
    }

    static func failure(_ response: HTTPResponse) -> FriendsError {
        struct Body: Decodable { let error: String?; let message: String? }
        let body: Body? = try? JSONDecoder().decode(Body.self, from: response.body)
        return FriendsError(
            status: response.status,
            code: body?.error ?? "accounts_unavailable",
            message: body?.message ?? "Friends request failed (\(response.status))."
        )
    }
}
