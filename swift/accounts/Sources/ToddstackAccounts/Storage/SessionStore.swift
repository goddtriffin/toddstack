import Foundation

/// Where the session lives between launches. Production is the Keychain; tests use `InMemorySessionStore`.
public protocol SessionStore: Sendable {
    func load() throws -> Session?
    func save(_ session: Session) throws
    /// Deleting nothing is not an error.
    func delete() throws
}
