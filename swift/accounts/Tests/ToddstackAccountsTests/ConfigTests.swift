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
        #expect(throws: AccountsError.config("callbackURL must be an https URL with a host, or a reverse-DNS custom scheme URL")) {
            try AccountsConfig(serverURL: server, callbackURL: URL(string: "boggle://callback")!, clientId: "x")
        }
        #expect(throws: AccountsError.config("serverURL must be an https URL with a host (http only on loopback)")) {
            try AccountsConfig(serverURL: URL(string: "http://example.com:3000")!, callbackURL: callback, clientId: "x")
        }
    }

    @Test func httpIsAllowedOnlyOnLoopback() throws {
        for host in ["127.0.0.1", "localhost", "[::1]"] {
            let config = try AccountsConfig(
                serverURL: URL(string: "http://\(host):8992")!, callbackURL: callback,
                accountsBaseURL: URL(string: "http://\(host):8790")!, clientId: "x"
            )
            #expect(config.serverURL.host != nil)
        }
        for bad in ["http://10.0.2.2:8992", "http://192.168.1.5:8992", "http://127.0.0.1.evil.example", "ftp://127.0.0.1"] {
            #expect(throws: AccountsError.self) {
                try AccountsConfig(serverURL: URL(string: bad)!, callbackURL: callback, clientId: "x")
            }
            #expect(throws: AccountsError.self) {
                try AccountsConfig(
                    serverURL: server, callbackURL: callback, accountsBaseURL: URL(string: bad)!, clientId: "x"
                )
            }
        }
    }

    @Test func callbackAcceptsHttpsOrAReverseDnsCustomScheme() throws {
        for ok in ["https://www.boggledygook.com/app/callback", "me.toddgriffin.boggledygook.debug:/app/callback"] {
            _ = try AccountsConfig(serverURL: server, callbackURL: URL(string: ok)!, clientId: "x")
        }
        for bad in ["http://127.0.0.1:8992/app/callback", "boggle://callback", "boggle:/app/callback", "https:///x"] {
            #expect(throws: AccountsError.self) {
                try AccountsConfig(serverURL: server, callbackURL: URL(string: bad)!, clientId: "x")
            }
        }
        let local = try AccountsConfig.local(
            serverURL: URL(string: "http://127.0.0.1:8992")!,
            callbackURL: URL(string: "me.toddgriffin.boggledygook.debug:/app/callback")!, clientId: "x"
        )
        #expect(local.callbackURL.scheme == "me.toddgriffin.boggledygook.debug")
    }

    @Test func localPointsAtTheLocalAccountsSite() throws {
        let config = try AccountsConfig.local(
            serverURL: URL(string: "http://127.0.0.1:8992")!, callbackURL: callback, clientId: "boggledygook"
        )
        #expect("http://127.0.0.1:8790/account?client=boggledygook" == config.accountURL())
    }
}
