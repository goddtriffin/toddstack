import Foundation
import Testing
import ToddstackAccounts

@Suite struct StoreTests {
    @Test func keychainStoreSavesLoadsReplacesAndDeletes() throws {
        let store = KeychainSessionStore(service: "toddstack-accounts-tests-\(UUID().uuidString)", account: "session", useDataProtectionKeychain: false)
        defer { try? store.delete() }
        #expect(try store.load() == nil)
        let first = Session(token: "a", expiresAt: nil, player: player())
        let second = Session(token: "b", expiresAt: Date(timeIntervalSince1970: 1_893_553_445), player: player(guest: true))
        try store.save(first)
        #expect(first == (try store.load()))
        try store.save(second)
        #expect(second == (try store.load()))
        try store.delete()
        try store.delete()
        #expect(try store.load() == nil)
    }

    @Test func inMemoryStoreSavesLoadsAndDeletes() throws {
        let store = InMemorySessionStore()
        #expect(try store.load() == nil)
        let session = Session(token: "a", expiresAt: nil, player: player())
        try store.save(session)
        #expect(session == (try store.load()))
        try store.delete()
        #expect(try store.load() == nil)
    }
}
