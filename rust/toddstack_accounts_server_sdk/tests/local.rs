//! Local mode: point at services on your machine, never production; plain `http` only on loopback.

use toddstack_accounts_server_sdk::{Accounts, Config, Error, LOCAL_BASE_URL};

#[test]
fn local_derives_everything_from_the_local_base() {
    let c: Config = Config::local("app", None, "http://127.0.0.1:8992/auth/callback");
    assert_eq!(LOCAL_BASE_URL, "http://127.0.0.1:8790");
    assert_eq!(c.issuer, "http://127.0.0.1:8790/oidc");
    assert_eq!(
        c.account_url("/account"),
        "http://127.0.0.1:8790/account?client=app"
    );
    assert!(Accounts::new(c).is_ok());
}

#[test]
fn a_base_url_overrides_the_issuer() {
    let c: Config = Config::toddgriffin_at("http://localhost:9000/", "app", None, "x").unwrap();
    assert_eq!(c.issuer, "http://localhost:9000/oidc");
    let c: Config = Config::toddgriffin_at("http://[::1]:9000", "app", None, "x").unwrap();
    assert_eq!(c.issuer, "http://[::1]:9000/oidc");
    let c: Config =
        Config::toddgriffin_at("https://staging.example.com", "app", None, "x").unwrap();
    assert_eq!(c.issuer, "https://staging.example.com/oidc");
}

#[test]
fn http_off_loopback_is_rejected() {
    for base in [
        "http://accounts.toddgriffin.me",
        "http://10.0.2.2:8790",
        "http://192.168.1.5:8790",
        "http://127.0.0.1.evil.example",
        "http://localhost@evil.com",
        "http://localhost.",
        "http://[::ffff:127.0.0.1]",
        "http://127.0.0.2",
        "ftp://127.0.0.1",
        "nonsense",
    ] {
        assert!(
            matches!(
                Config::toddgriffin_at(base, "app", None, "x"),
                Err(Error::Config(_))
            ),
            "{base}"
        );
    }
}

#[test]
fn a_hand_built_http_issuer_off_loopback_is_rejected() {
    let c: Config = Config {
        issuer: String::from("http://accounts.example.com/oidc"),
        client_id: String::from("app"),
        client_secret: None,
        redirect_uri: String::new(),
    };
    assert!(matches!(Accounts::new(c), Err(Error::Config(_))));
}

#[test]
fn loopback_hosts_compare_case_insensitively() {
    let c: Config = Config::toddgriffin_at("http://LOCALHOST:8790", "app", None, "x").unwrap();
    assert!(c.issuer.ends_with(":8790/oidc"));
}
