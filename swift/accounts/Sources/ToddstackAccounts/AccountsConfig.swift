import Foundation

/// Whether `url` has a host and is https, or http on a loopback host (`127.0.0.1`, `localhost`, `::1`): the only
/// plain-http URLs the SDK accepts, so local testing never needs production and nothing else goes over the wire
/// unencrypted.
func isAllowedBaseURL(_ url: URL) -> Bool {
    guard let host = url.host?.lowercased(), !host.isEmpty else { return false }
    switch url.scheme?.lowercased() {
    case "https": return true
    case "http": return ["127.0.0.1", "localhost", "::1"].contains(host)
    default: return false
    }
}

/// Whether `url` is an https URL with a host, or a reverse-DNS private-use scheme (RFC 8252: the scheme contains a
/// dot, e.g. `me.toddgriffin.boggledygook.debug:/app/callback`). Plain http is never a callback.
func isAllowedCallbackURL(_ url: URL) -> Bool {
    guard let scheme = url.scheme?.lowercased() else { return false }
    if scheme == "https" { return !(url.host ?? "").isEmpty }
    return scheme != "http" && scheme.contains(".")
}

/// Where a native app signs in. The app talks only to its project's server, never to the accounts service.
public struct AccountsConfig: Sendable, Equatable {
    /// The accounts site running locally. Local testing never touches production: see `local(...)`.
    public static let localAccountsBaseURL: URL = URL(string: "http://127.0.0.1:8790")!

    /// The project server that implements the `/app/sign-in` contract, for example `https://www.boggledygook.com`
    /// (locally, `http://127.0.0.1:8992`).
    public let serverURL: URL
    /// The URL the server redirects to after sign-in: an https Universal Link claimed by the app (Associated Domains),
    /// or a reverse-DNS custom scheme (RFC 8252, e.g. `me.toddgriffin.boggledygook.debug:/app/callback`).
    public let callbackURL: URL
    /// The accounts site, used only for links such as `accountURL(_:)`.
    public let accountsBaseURL: URL
    /// Your project's client id; tags every link to the accounts site.
    public let clientId: String

    /// Throws `AccountsError.config` unless `callbackURL` is an https URL with a host or a reverse-DNS custom scheme URL, and `serverURL` and
    /// `accountsBaseURL` are https URLs with a host, or http on a loopback host (`127.0.0.1`, `localhost`, `::1`).
    /// Local testing: pass a loopback `serverURL` (and `accountsBaseURL`), or use `local(...)`. The app itself must
    /// allow cleartext to loopback: iOS needs `NSAllowsLocalNetworking` in its ATS settings (see the README).
    public init(
        serverURL: URL,
        callbackURL: URL,
        accountsBaseURL: URL = URL(string: "https://accounts.toddgriffin.me")!,
        clientId: String
    ) throws {
        for (name, url) in [("serverURL", serverURL), ("accountsBaseURL", accountsBaseURL)] {
            guard isAllowedBaseURL(url) else {
                throw AccountsError.config("\(name) must be an https URL with a host (http only on loopback)")
            }
        }
        guard isAllowedCallbackURL(callbackURL) else {
            throw AccountsError.config("callbackURL must be an https URL with a host, or a reverse-DNS custom scheme URL")
        }
        guard !clientId.isEmpty else { throw AccountsError.config("clientId must not be empty") }
        self.serverURL = serverURL
        self.callbackURL = callbackURL
        self.accountsBaseURL = accountsBaseURL
        self.clientId = clientId
    }

    /// A config for local testing: your project's server on your machine (for example `http://127.0.0.1:8992`)
    /// and the accounts site at `localAccountsBaseURL`. `callbackURL` is typically a debug custom scheme.
    public static func local(serverURL: URL, callbackURL: URL, clientId: String) throws -> AccountsConfig {
        try AccountsConfig(
            serverURL: serverURL, callbackURL: callbackURL, accountsBaseURL: localAccountsBaseURL, clientId: clientId
        )
    }

    /// `path` on the accounts site tagged with your client:
    /// `accountURL("/account")` is `https://accounts.toddgriffin.me/account?client=<client id>`. Every link to the
    /// accounts site carries it (the canonical pattern). A query already in `path` is kept; a fragment stays last.
    public func accountURL(_ path: String = "/account") -> String {
        var origin: String = accountsBaseURL.absoluteString
        while origin.hasSuffix("/") { origin.removeLast() }
        var fragment: String = ""
        var bare: String = path
        if let hash = path.firstIndex(of: "#") {
            fragment = String(path[hash...])
            bare = String(path[..<hash])
        }
        let slash: String = bare.hasPrefix("/") ? "" : "/"
        let join: String = bare.contains("?") ? "&" : "?"
        var allowed: CharacterSet = .alphanumerics
        allowed.insert(charactersIn: "-._~")
        let client: String = clientId.addingPercentEncoding(withAllowedCharacters: allowed) ?? clientId
        return "\(origin)\(slash)\(bare)\(join)client=\(client)\(fragment)"
    }
}
