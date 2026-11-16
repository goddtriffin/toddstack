import Foundation

/// The production transport, on `URLSession`.
public struct URLSessionAccountsTransport: AccountsTransport {
    private let session: URLSession

    public init(session: URLSession = .shared) {
        self.session = session
    }

    public func send(_ request: HTTPRequest) async throws -> HTTPResponse {
        var urlRequest = URLRequest(url: request.url)
        urlRequest.httpMethod = request.method
        urlRequest.httpBody = request.body
        for (name, value) in request.headers {
            urlRequest.setValue(value, forHTTPHeaderField: name)
        }
        do {
            let (data, response) = try await session.data(for: urlRequest)
            guard let http = response as? HTTPURLResponse else {
                throw AccountsError.invalidResponse("not an HTTP response")
            }
            return HTTPResponse(status: http.statusCode, body: data)
        } catch let error as AccountsError {
            throw error
        } catch {
            throw AccountsError.network(error.localizedDescription)
        }
    }
}
