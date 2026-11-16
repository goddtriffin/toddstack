import Foundation

/// Where a native app signs in. The app talks only to its project's server, never to the accounts service.
public struct AccountsConfig: Sendable, Equatable {
    /// The project server that implements the `/app/sign-in` contract, for example `https://www.boggledygook.com`.
    public let serverURL: URL
    /// The https URL the server redirects to after sign-in. Must be claimed by the app (Associated Domains).
    public let callbackURL: URL
    /// The accounts site, used only for links such as `accountURL(_:)`.
    public let accountsBaseURL: URL
    /// Your project's client id; tags every link to the accounts site.
    public let clientId: String

    /// Throws `AccountsError.config` unless both `serverURL` and `callbackURL` are https URLs with a host.
    public init(
        serverURL: URL,
        callbackURL: URL,
        accountsBaseURL: URL = URL(string: "https://accounts.toddgriffin.me")!,
        clientId: String
    ) throws {
        for (name, url) in [("serverURL", serverURL), ("callbackURL", callbackURL), ("accountsBaseURL", accountsBaseURL)] {
            guard url.scheme?.lowercased() == "https", let host = url.host, !host.isEmpty else {
                throw AccountsError.config("\(name) must be an https URL with a host")
            }
        }
        guard !clientId.isEmpty else { throw AccountsError.config("clientId must not be empty") }
        self.serverURL = serverURL
        self.callbackURL = callbackURL
        self.accountsBaseURL = accountsBaseURL
        self.clientId = clientId
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
