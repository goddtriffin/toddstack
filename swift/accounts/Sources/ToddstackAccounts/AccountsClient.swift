import Foundation

/// Why `signIn` shows the sign-in page.
public enum SignInPrompt: Sendable, Equatable {
    /// Silent when the browser already has an accounts session; the page shows only when needed.
    case none
    /// Forces a fresh sign-in (switching accounts).
    case login
}

/// Signs a native app in through its project's server and keeps the session.
///
/// The app never talks to the accounts service directly: it sends the person to the server's `/app/sign-in`, gets a
/// one-time code back on its claimed https callback, and trades the code plus the PKCE verifier for a bearer token.
public final class AccountsClient: Sendable {
    public let config: AccountsConfig
    private let transport: any AccountsTransport
    private let webAuthenticator: any WebAuthenticator
    private let store: any SessionStore
    private let random: @Sendable (Int) -> Data
    private let now: @Sendable () -> Date

    /// `store` defaults to the Keychain item for `config.clientId`. `random` and `now` exist so tests are deterministic.
    public init(
        config: AccountsConfig,
        transport: any AccountsTransport = URLSessionAccountsTransport(),
        webAuthenticator: any WebAuthenticator = ASWebAuthenticator(),
        store: (any SessionStore)? = nil,
        random: @escaping @Sendable (Int) -> Data = systemRandomBytes,
        now: @escaping @Sendable () -> Date = { Date() }
    ) {
        self.config = config
        self.transport = transport
        self.webAuthenticator = webAuthenticator
        self.store = store ?? KeychainSessionStore(account: config.clientId)
        self.random = random
        self.now = now
    }

    /// The stored session, or nil when signed out. An expired session is deleted and reads as signed out.
    public var currentSession: Session? {
        guard let session = try? store.load() else { return nil }
        if let expiresAt = session.expiresAt, expiresAt <= now() {
            try? store.delete()
            return nil
        }
        return session
    }

    /// `path` on the accounts site, tagged with this client. See `AccountsConfig.accountURL(_:)`.
    public func accountURL(_ path: String = "/account") -> String {
        config.accountURL(path)
    }

    /// Opens the sign-in sheet and, on success, stores and returns the new session. A stored guest token is sent
    /// with the exchange so the server can merge the guest into the player.
    @discardableResult
    public func signIn(prompt: SignInPrompt = .none) async throws -> Session {
        let pkce: PkcePair = PkcePair.generate(random: random)
        let state: String = base64URL(random(32))

        var components = URLComponents(url: endpoint("/app/sign-in"), resolvingAgainstBaseURL: false)!
        var items: [URLQueryItem] = [
            URLQueryItem(name: "code_challenge", value: pkce.challenge),
            URLQueryItem(name: "state", value: state),
        ]
        if prompt == .login { items.append(URLQueryItem(name: "prompt", value: "login")) }
        components.queryItems = items

        let callback: URL = try await webAuthenticator.authenticate(url: components.url!, callbackURL: config.callbackURL)
        let code: String = try Self.code(from: callback, expectedState: state)
        return try await exchange(code: code, verifier: pkce.verifier)
    }

    /// Starts a guest session (no account) and stores it.
    @discardableResult
    public func startGuest() async throws -> Session {
        let response: HTTPResponse = try await transport.send(
            HTTPRequest(url: endpoint("/api/v1/app/guest"), method: "POST", headers: jsonHeaders())
        )
        return try save(try Self.session(from: response))
    }

    /// `request` with the bearer token added. Throws `notSignedIn` with no (or an expired) session.
    public func authorized(_ request: URLRequest) throws -> URLRequest {
        guard let session = currentSession else { throw AccountsError.notSignedIn }
        var signed: URLRequest = request
        signed.setValue("Bearer \(session.token)", forHTTPHeaderField: "Authorization")
        return signed
    }

    /// Signs out everywhere: tells the server, deletes the token, then opens the server's `end_session_url` (global
    /// sign-out) in the web-auth session. The token is deleted even when the network fails, and a failed or cancelled
    /// browser step is ignored; only a Keychain failure throws.
    public func signOut() async throws {
        let session: Session? = try? store.load()
        var endSessionURL: URL?
        if let session {
            endSessionURL = try? await requestEndSession(token: session.token)
        }
        try store.delete()
        if let endSessionURL {
            _ = try? await webAuthenticator.authenticate(url: endSessionURL, callbackURL: config.callbackURL)
        }
    }

    // MARK: internals

    private func exchange(code: String, verifier: String) async throws -> Session {
        var headers: [String: String] = jsonHeaders()
        if let guest = try? store.load() { headers["Authorization"] = "Bearer \(guest.token)" }
        let body: Data = try Self.encode(["code": code, "code_verifier": verifier])
        let response: HTTPResponse = try await transport.send(
            HTTPRequest(url: endpoint("/api/v1/app/session"), method: "POST", headers: headers, body: body)
        )
        return try save(try Self.session(from: response))
    }

    private func requestEndSession(token: String) async throws -> URL? {
        var headers: [String: String] = jsonHeaders()
        headers["Authorization"] = "Bearer \(token)"
        let response: HTTPResponse = try await transport.send(
            HTTPRequest(url: endpoint("/api/v1/app/sign-out"), method: "POST", headers: headers)
        )
        guard (200..<300).contains(response.status) else { return nil }
        struct Body: Decodable { let end_session_url: String? }
        guard let text = try JSONDecoder().decode(Body.self, from: response.body).end_session_url,
              let url = URL(string: text), url.scheme?.lowercased() == "https" else { return nil }
        return url
    }

    private func save(_ session: Session) throws -> Session {
        try store.save(session)
        return session
    }

    private func endpoint(_ path: String) -> URL {
        var base: String = config.serverURL.absoluteString
        while base.hasSuffix("/") { base.removeLast() }
        return URL(string: base + path)!
    }

    private func jsonHeaders() -> [String: String] {
        ["Accept": "application/json", "Content-Type": "application/json"]
    }

    private static func encode(_ value: [String: String]) throws -> Data {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        return try encoder.encode(value)
    }

    /// The state is checked first, in constant time, so a forged callback learns nothing from the error it gets.
    static func code(from callback: URL, expectedState: String) throws -> String {
        let items: [URLQueryItem] = URLComponents(url: callback, resolvingAgainstBaseURL: false)?.queryItems ?? []
        func value(_ name: String) -> String? { items.first(where: { $0.name == name })?.value }
        guard let state = value("state"), constantTimeEquals(state, expectedState) else {
            throw AccountsError.stateMismatch
        }
        if let error = value("error") { throw AccountsError.authorization(code: error) }
        guard let code = value("code"), !code.isEmpty else { throw AccountsError.missingCode }
        return code
    }

    static func session(from response: HTTPResponse) throws -> Session {
        struct ErrorBody: Decodable { let error: String? }
        let error: String? = try? JSONDecoder().decode(ErrorBody.self, from: response.body).error
        switch response.status {
        case 200..<300:
            do { return try JSONDecoder().decode(Session.self, from: response.body) } catch {
                throw AccountsError.invalidResponse("session body: \(error.localizedDescription)")
            }
        case 400 where error == "invalid_grant":
            throw AccountsError.invalidGrant
        default:
            throw AccountsError.api(status: response.status, message: error)
        }
    }
}
