import Foundation

/// The signed-in person, as the project server reports them.
public struct Player: Codable, Sendable, Equatable {
    public let id: String
    public let username: String
    /// Avatar URL, when the person has one.
    public let picture: String?
    /// True for a guest session (no account; customize nothing).
    public let guest: Bool

    public init(id: String, username: String, picture: String?, guest: Bool) {
        self.id = id
        self.username = username
        self.picture = picture
        self.guest = guest
    }
}

/// A bearer token and who it belongs to. Stored in the Keychain between launches.
public struct Session: Codable, Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    /// Sent as `Authorization: Bearer <token>`.
    public let token: String
    /// When the token stops working; nil when it does not expire.
    public let expiresAt: Date?
    public let player: Player

    public init(token: String, expiresAt: Date?, player: Player) {
        self.token = token
        self.expiresAt = expiresAt
        self.player = player
    }

    /// Redacts the token so logging a session never leaks it.
    public var description: String { "Session(token: <redacted>, expiresAt: \(String(describing: expiresAt)), player: \(player))" }
    public var debugDescription: String { description }

    private enum CodingKeys: String, CodingKey {
        case token, player
        case expiresAt = "expires_at"
    }

    public init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        token = try container.decode(String.self, forKey: .token)
        player = try container.decode(Player.self, forKey: .player)
        if let text = try container.decodeIfPresent(String.self, forKey: .expiresAt) {
            guard let date = Session.parse(text) else {
                throw DecodingError.dataCorruptedError(forKey: .expiresAt, in: container, debugDescription: "not RFC 3339: \(text)")
            }
            expiresAt = date
        } else {
            expiresAt = nil
        }
    }

    public func encode(to encoder: any Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(token, forKey: .token)
        try container.encode(player, forKey: .player)
        try container.encodeIfPresent(expiresAt.map { Session.format($0) }, forKey: .expiresAt)
    }

    private static func parse(_ text: String) -> Date? {
        let plain = ISO8601DateFormatter()
        let fractional = ISO8601DateFormatter()
        fractional.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return plain.date(from: text) ?? fractional.date(from: text)
    }

    private static func format(_ date: Date) -> String {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return formatter.string(from: date)
    }
}
