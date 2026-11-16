import Foundation
import Testing
import ToddstackAccounts

@Suite struct ConfigTests {
    @Test func accountURLTagsThePathWithTheClient() throws {
        let config: AccountsConfig = try makeConfig()
        #expect("https://accounts.toddgriffin.me/account?client=boggledygook" == config.accountURL())
        #expect("https://accounts.toddgriffin.me/friends?client=boggledygook" == config.accountURL("friends"))
    }

    @Test func accountURLKeepsAnExistingQueryAndPutsTheFragmentLast() throws {
        let config: AccountsConfig = try makeConfig()
        #expect("https://accounts.toddgriffin.me/account?tab=a&client=boggledygook#x" == config.accountURL("/account?tab=a#x"))
    }

    @Test func accountURLStripsATrailingSlashAndEncodesTheClient() throws {
        let config = try AccountsConfig(
            serverURL: server, callbackURL: callback,
            accountsBaseURL: URL(string: "https://accounts.example.com/")!, clientId: "a b&c"
        )
        #expect("https://accounts.example.com/account?client=a%20b%26c" == config.accountURL())
    }

    @Test func nonHttpsUrlsAreRejected() {
        #expect(throws: AccountsError.config("callbackURL must be an https URL with a host")) {
            try AccountsConfig(serverURL: server, callbackURL: URL(string: "boggle://callback")!, clientId: "x")
        }
        #expect(throws: AccountsError.config("serverURL must be an https URL with a host")) {
            try AccountsConfig(serverURL: URL(string: "http://localhost:3000")!, callbackURL: callback, clientId: "x")
        }
    }
}
