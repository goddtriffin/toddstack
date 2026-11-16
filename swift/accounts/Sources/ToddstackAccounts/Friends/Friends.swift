import Foundation

/// A friend, request or blocked person as your app sees them.
public struct Person: Codable, Equatable, Sendable, Identifiable {
    /// Their permanent id.
    public let sub: String
    public let username: String
    public let name: String?
    public let picture: String?
    /// When the friendship, request or block started (RFC 3339).
    public let since: String

    public var id: String { sub }

    public init(sub: String, username: String, name: String?, picture: String?, since: String) {
        self.sub = sub
        self.username = username
        self.name = name
        self.picture = picture
        self.since = since
    }
}

/// Pending friend requests.
public struct FriendRequests: Codable, Equatable, Sendable {
    /// Requests sent to the person.
    public let incoming: [Person]
    /// Requests the person sent.
    public let outgoing: [Person]

    public init(incoming: [Person], outgoing: [Person]) {
        self.incoming = incoming
        self.outgoing = outgoing
    }
}

/// The person's blocks, for keeping people apart in your app.
public struct Blocks: Codable, Equatable, Sendable {
    /// People this person blocked, newest first: for an Unblock list.
    public let blocked: [Person]
    /// Everyone blocked either way. Hide these from the person, but never show it as a block (blocks are silent).
    public let hidden: [String]

    public init(blocked: [Person], hidden: [String]) {
        self.blocked = blocked
        self.hidden = hidden
    }
}

/// What sending a request did.
public enum Sent: String, Codable, Equatable, Sendable {
    case requested
    /// They had already asked: you're friends now.
    case accepted
    case alreadyFriends = "already_friends"
}

/// Where the person stands with someone: what a friend button should say.
public enum Relationship: Equatable, Sendable, Codable {
    /// "Friends".
    case friends
    /// "Requested" (take it back with `cancel`).
    case requestSent
    /// "Accept".
    case requestReceived
    /// "Unblock".
    case blocked
    /// "Add friend". Also the answer for the person themselves, unknown accounts and someone who blocked the person.
    case none
    /// A state newer than this SDK.
    case unknown

    public init(from decoder: any Decoder) throws {
        switch try decoder.singleValueContainer().decode(String.self) {
        case "friends": self = .friends
        case "request_sent": self = .requestSent
        case "request_received": self = .requestReceived
        case "blocked": self = .blocked
        case "none": self = .none
        default: self = .unknown
        }
    }

    public func encode(to encoder: any Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .friends: try container.encode("friends")
        case .requestSent: try container.encode("request_sent")
        case .requestReceived: try container.encode("request_received")
        case .blocked: try container.encode("blocked")
        case .none: try container.encode("none")
        case .unknown: try container.encode("unknown")
        }
    }
}

/// A friends call your server refused. `code` is the server's error code, `message` is meant for the person.
public struct FriendsError: Error, Equatable, Sendable, LocalizedError {
    /// HTTP status.
    public let status: Int
    /// `account_required` (a guest: offer sign-in), `session_expired`, `cross_site`, `missing_scope`, `bad_request`,
    /// `refused` (not found, blocked, a privacy setting or a limit), `not_found`, `conflict`, `unprocessable`,
    /// `rate_limited`, `accounts_unavailable`.
    public let code: String
    public let message: String

    public init(status: Int, code: String, message: String) {
        self.status = status
        self.code = code
        self.message = message
    }

    public var errorDescription: String? { message }
}
