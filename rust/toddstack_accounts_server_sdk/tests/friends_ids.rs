//! `Friends` refuses ids that could leave its URL paths, before any request is made.
#![expect(deprecated, reason = "covers `Friends`, which stays until 0.10.0")]

use toddstack_accounts_server_sdk::friends::Friends;
use toddstack_accounts_server_sdk::{Accounts, Config, Error};

#[tokio::test]
async fn a_traversal_slash_or_query_in_an_id_is_refused_without_a_request() {
    // Nothing listens on port 1: any request that was attempted would be Error::Http, not the 400 below.
    let accounts: Accounts = Accounts::new(
        Config::toddgriffin_at("http://127.0.0.1:1", "x", None, "https://x.example/cb").unwrap(),
    )
    .unwrap();
    let friends: Friends<'_> = Friends::new(&accounts, "tok");
    for bad in ["../../oidc/x", "a/b", "a?b=1", "a#b", "a%2Fb", ""] {
        for result in [
            friends.accept(bad).await,
            friends.decline(bad).await,
            friends.cancel(bad).await,
            friends.remove(bad).await,
            friends.block(bad).await,
            friends.unblock(bad).await,
            friends.mark_read(bad).await,
            friends.mute(bad).await,
        ] {
            assert!(
                matches!(result, Err(Error::Api { status: 400, .. })),
                "{bad}"
            );
        }
        assert!(matches!(
            friends.relationships(&["u2", bad]).await,
            Err(Error::Api { status: 400, .. })
        ));
    }
}
