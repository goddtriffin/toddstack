import Foundation
import Security

/// Keeps the session in the Keychain as a generic password, readable after the first unlock and never synced
/// or restored to another device.
public struct KeychainSessionStore: SessionStore {
    private let service: String
    private let account: String

    public init(service: String = "toddstack-accounts", account: String = "session") {
        self.service = service
        self.account = account
    }

    private var query: [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
    }

    public func load() throws -> Session? {
        var request: [String: Any] = query
        request[kSecReturnData as String] = true
        request[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: CFTypeRef?
        let status: OSStatus = SecItemCopyMatching(request as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let data = result as? Data else {
            throw AccountsError.storage("read failed (\(status))")
        }
        do {
            return try JSONDecoder().decode(Session.self, from: data)
        } catch {
            // An unreadable item can never become a session; drop it so the next sign-in starts clean.
            try? delete()
            return nil
        }
    }

    public func save(_ session: Session) throws {
        let data: Data
        do { data = try JSONEncoder().encode(session) } catch { throw AccountsError.storage("encode failed") }
        let update: [String: Any] = [kSecValueData as String: data]
        var status: OSStatus = SecItemUpdate(query as CFDictionary, update as CFDictionary)
        if status == errSecItemNotFound {
            var add: [String: Any] = query
            add[kSecValueData as String] = data
            add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
            status = SecItemAdd(add as CFDictionary, nil)
        }
        guard status == errSecSuccess else { throw AccountsError.storage("write failed (\(status))") }
    }

    public func delete() throws {
        let status: OSStatus = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw AccountsError.storage("delete failed (\(status))")
        }
    }
}
