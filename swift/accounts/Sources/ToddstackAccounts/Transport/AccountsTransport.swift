import Foundation

/// An HTTP request the SDK makes. A plain value so mocks can compare it.
public struct HTTPRequest: Sendable, Equatable {
    public var url: URL
    public var method: String
    public var headers: [String: String]
    public var body: Data?

    public init(url: URL, method: String, headers: [String: String] = [:], body: Data? = nil) {
        self.url = url
        self.method = method
        self.headers = headers
        self.body = body
    }
}

/// An HTTP response: status and body.
public struct HTTPResponse: Sendable, Equatable {
    public var status: Int
    public var body: Data

    public init(status: Int, body: Data = Data()) {
        self.status = status
        self.body = body
    }
}

/// The network, behind a protocol so tests never touch it. Throws only when the request itself fails;
/// a 4xx or 5xx is a normal response.
public protocol AccountsTransport: Sendable {
    func send(_ request: HTTPRequest) async throws -> HTTPResponse
}
