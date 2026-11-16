import Foundation
import os

/// A store that forgets on exit. For tests and previews.
public final class InMemorySessionStore: SessionStore {
    private let session: OSAllocatedUnfairLock<Session?>

    public init(_ session: Session? = nil) {
        self.session = OSAllocatedUnfairLock(initialState: session)
    }

    public func load() throws -> Session? { session.withLock { $0 } }
    public func save(_ new: Session) throws { session.withLock { $0 = new } }
    public func delete() throws { session.withLock { $0 = nil } }
}
