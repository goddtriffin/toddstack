// swift-tools-version: 6.0
// SwiftPM only reads a Package.swift at the repository root, so this manifest points at swift/accounts.
import PackageDescription

let package = Package(
    name: "toddstack",
    platforms: [.iOS("17.4"), .macOS("14.4")],
    products: [
        .library(name: "ToddstackAccounts", targets: ["ToddstackAccounts"]),
        // Mocks for the package's dependency-injected interfaces. Depend on it from test targets only.
        .library(name: "ToddstackAccountsTesting", targets: ["ToddstackAccountsTesting"]),
    ],
    targets: [
        .target(
            name: "ToddstackAccounts",
            path: "swift/accounts/Sources/ToddstackAccounts"
        ),
        .target(
            name: "ToddstackAccountsTesting",
            dependencies: ["ToddstackAccounts"],
            path: "swift/accounts/Sources/ToddstackAccountsTesting"
        ),
        .testTarget(
            name: "ToddstackAccountsTests",
            dependencies: ["ToddstackAccounts", "ToddstackAccountsTesting"],
            path: "swift/accounts/Tests/ToddstackAccountsTests"
        ),
    ]
)
