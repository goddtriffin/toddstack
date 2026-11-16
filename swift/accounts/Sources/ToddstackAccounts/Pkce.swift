import CryptoKit
import Foundation

/// A PKCE verifier and its S256 challenge (RFC 7636).
public struct PkcePair: Sendable, Equatable {
    /// Stays in the app; sent only to the server's session endpoint.
    public let verifier: String
    /// `BASE64URL(SHA-256(verifier))`; sent in the sign-in URL.
    public let challenge: String

    /// Builds the pair for a known verifier.
    public init(verifier: String) {
        self.verifier = verifier
        self.challenge = pkceChallenge(verifier: verifier)
    }

    /// A fresh pair from 32 random bytes, as RFC 7636 recommends.
    public static func generate(random: @Sendable (Int) -> Data = systemRandomBytes) -> PkcePair {
        PkcePair(verifier: base64URL(random(32)))
    }
}

/// The PKCE S256 challenge for `verifier`.
public func pkceChallenge(verifier: String) -> String {
    base64URL(Data(SHA256.hash(data: Data(verifier.utf8))))
}

/// Unpadded base64url, as PKCE and OAuth state values use.
public func base64URL(_ data: Data) -> String {
    data.base64EncodedString()
        .replacingOccurrences(of: "+", with: "-")
        .replacingOccurrences(of: "/", with: "_")
        .replacingOccurrences(of: "=", with: "")
}

/// Cryptographically secure random bytes.
@Sendable
public func systemRandomBytes(_ count: Int) -> Data {
    var generator = SystemRandomNumberGenerator()
    return Data((0..<count).map { _ in UInt8.random(in: .min ... .max, using: &generator) })
}

/// Compares two strings in time that depends only on their length, so a forged `state` leaks nothing.
public func constantTimeEquals(_ a: String, _ b: String) -> Bool {
    let left: [UInt8] = Array(a.utf8)
    let right: [UInt8] = Array(b.utf8)
    var difference: UInt8 = left.count == right.count ? 0 : 1
    for index in 0..<max(left.count, right.count) {
        let x: UInt8 = index < left.count ? left[index] : 0
        let y: UInt8 = index < right.count ? right[index] : 0
        difference |= x ^ y
    }
    return difference == 0
}
