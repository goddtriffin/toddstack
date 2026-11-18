//! Which requests become Sentry transactions, through the real router with
//! the real tracing layer and sampler at a rate of 1 (so a request that is
//! missing was *excluded*, not unlucky), and what the browser is told at each
//! browser rate.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::http::Method;
use sentry::protocol::{EnvelopeItem, Transaction};
use sentry::test::TestTransport;
use sentry::{Client, ClientOptions, Hub, HubSwitchGuard};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;

use super::{
    Environment, FrontendParams, Pages, Shutdown, WebServer, analytics_home, built_site,
    frontend_with, frontend_with_analytics, get, request, text,
};
use crate::TracesSampleRate;
use crate::analytics::AnalyticsConfig;
use crate::observability::with_traces;
use crate::templates::PageTemplateData;

/// The transactions Sentry would receive for `requests`, in order, from a
/// server whose router is `router`. Each request is a `(method, uri)`.
async fn transactions(router: &Router, requests: &[(Method, &str)]) -> Vec<Transaction<'static>> {
    let transport: Arc<TestTransport> = TestTransport::new();
    let mut options: ClientOptions = with_traces(ClientOptions::default(), TracesSampleRate::ALL);
    options.dsn = Some(
        "https://public@sentry.invalid/1"
            .parse()
            .expect("a valid DSN"),
    );
    options.transport = Some(Arc::new(transport.clone()));
    let hub: Arc<Hub> = Arc::new(Hub::new(
        Some(Arc::new(Client::from(options))),
        Arc::new(sentry::Scope::default()),
    ));
    let _hub: HubSwitchGuard = HubSwitchGuard::new(hub);
    let _subscriber: tracing::subscriber::DefaultGuard = tracing::subscriber::set_default(
        tracing_subscriber::registry()
            .with(sentry::integrations::tracing::SpanLayer::new().with_filter(LevelFilter::INFO)),
    );

    for (method, uri) in requests {
        axum::body::to_bytes(
            request(router, method.clone(), uri).await.into_body(),
            usize::MAX,
        )
        .await
        .expect("a readable body");
    }

    transport
        .fetch_and_clear_envelopes()
        .iter()
        .flat_map(|envelope| envelope.items())
        .filter_map(|item| match item {
            EnvelopeItem::Transaction(transaction) => Some((**transaction).clone()),
            _ => None,
        })
        .collect()
}

fn names(transactions: &[Transaction<'static>]) -> Vec<String> {
    transactions
        .iter()
        .map(|transaction| transaction.name.clone().unwrap_or_default())
        .collect()
}

fn pages() -> Pages {
    Pages::new()
        .static_page(PageTemplateData::new("home", "Home", "/"), ())
        .dynamic_page("/things/{slug}", axum::routing::get(super::thing))
        .unlisted()
}

#[tokio::test(flavor = "current_thread")]
async fn only_the_sites_own_routes_are_traced_by_pattern_and_the_rest_never() {
    let root: PathBuf = built_site("trace-routes");
    let router: Router = WebServer::new("127.0.0.1", 0, Environment::Local)
        .root_dir(&root)
        .frontend(frontend_with(pages()))
        .into_router(Shutdown::manual())
        .expect("the built site boots");
    let hashed: String = crate::assets::CacheBuster::load_in(&root)
        .expect("the manifest the build wrote")
        .get_file(super::STYLESHEET);
    let hashed_path: String = format!("/{hashed}");
    let get_all: Vec<(Method, &str)> = vec![
        (Method::GET, "/"),
        (Method::GET, "/things/a-slug"),
        (Method::GET, "/api/v1/health"),
        (Method::GET, "/robots.txt"),
        (Method::GET, "/humans.txt"),
        (Method::GET, "/site.webmanifest"),
        (Method::GET, "/sitemap.xml"),
        (Method::GET, "/favicon.ico"),
        (Method::GET, "/apple-touch-icon.png"),
        (Method::GET, &hashed_path),
        (Method::GET, "/static/no/such/file.css"),
        (Method::GET, "/no/such/page"),
    ];
    let actual: Vec<String> = names(&transactions(&router, &get_all).await);

    let expected: Vec<String> = vec![String::from("GET /"), String::from("GET /things/{slug}")];
    assert_eq!(expected, actual);
}

#[tokio::test(flavor = "current_thread")]
async fn a_dynamic_page_without_a_matching_resource_is_still_traced_and_a_scanner_404_is_not() {
    let root: PathBuf = built_site("trace-404");
    let router: Router = WebServer::new("127.0.0.1", 0, Environment::Local)
        .root_dir(&root)
        .frontend(frontend_with(pages()))
        .into_router(Shutdown::manual())
        .expect("the built site boots");

    let actual: Vec<String> = names(
        &transactions(
            &router,
            &[
                (Method::GET, "/things/missing"),
                (Method::GET, "/wp-admin/setup.php"),
            ],
        )
        .await,
    );

    assert_eq!(vec![String::from("GET /things/{slug}")], actual);
}

#[tokio::test(flavor = "current_thread")]
async fn the_proxy_scripts_and_endpoints_are_on_the_untraced_list() {
    let root: PathBuf = built_site("trace-library");
    let cache_buster: crate::assets::CacheBuster =
        crate::assets::CacheBuster::load_in(&root).expect("the manifest the build wrote");
    let frontend: crate::webserver::Frontend<()> = crate::webserver::Frontend::build(
        frontend_with_analytics(analytics_home(), AnalyticsConfig::new("pa-fixture")),
        None,
        &cache_buster,
        Environment::Production,
    )
    .expect("the frontend builds");

    let routes: Vec<String> = super::super::server::library_routes(&frontend);

    for path in [
        &frontend.runtime.sentry_browser.script_path,
        &frontend.runtime.sentry_browser.tunnel_path,
        &frontend
            .runtime
            .analytics
            .as_ref()
            .expect("enabled")
            .script_path,
        &frontend
            .runtime
            .analytics
            .as_ref()
            .expect("enabled")
            .event_path,
    ] {
        assert!(routes.contains(path), "{path} in {routes:?}");
    }
}

fn params_at(rate: Option<TracesSampleRate>) -> FrontendParams {
    let params: FrontendParams = frontend_with(analytics_home());
    match rate {
        Some(rate) => params.with_browser_traces_sample_rate(rate),
        None => params,
    }
}

async fn page_at(name: &str, rate: Option<TracesSampleRate>) -> String {
    let root: PathBuf = built_site(name);
    let router: Router = WebServer::new("127.0.0.1", 0, Environment::Local)
        .root_dir(&root)
        .frontend(params_at(rate))
        .into_router(Shutdown::manual())
        .expect("the built site boots");
    text(get(&router, "/").await).await
}

#[tokio::test]
async fn a_browser_rate_of_zero_renders_errors_only_init_and_a_no_op_span_helper() {
    let body: String = page_at("trace-browser-off", Some(TracesSampleRate::OFF)).await;

    assert!(body.contains("Sentry.init({"), "{body}");
    assert!(
        body.contains(
            "dsn: \"https://abc123@o4504844394627072.ingest.sentry.io/4504862450515968\""
        ),
        "{body}"
    );
    assert!(body.contains("tunnel: \"/api/v1/fixture-"), "{body}");
    assert!(!body.contains("tracesSampleRate"), "{body}");
    assert!(!body.contains("browserTracingIntegration"), "{body}");
    assert!(!body.contains("startInactiveSpan"), "{body}");
    assert!(body.contains("window.webserverBase = {"), "{body}");
    assert!(!body.contains("sentryOnLoad"), "{body}");
}

#[tokio::test]
async fn a_browser_rate_above_zero_renders_tracing_init_at_that_rate() {
    let body: String = page_at(
        "trace-browser-on",
        Some(TracesSampleRate::new(0.05).expect("valid")),
    )
    .await;

    assert!(body.contains("tracesSampleRate: 0.05,"), "{body}");
    assert!(
        body.contains("integrations: [Sentry.browserTracingIntegration()],"),
        "{body}"
    );
    assert!(body.contains("Sentry.startInactiveSpan({"), "{body}");
    assert!(body.contains("window.webserverBase = {"), "{body}");
}

#[tokio::test]
async fn the_browser_rate_defaults_to_the_named_constant() {
    let body: String = page_at("trace-browser-default", None).await;

    let expected: String = format!(
        "tracesSampleRate: {},",
        crate::DEFAULT_BROWSER_TRACES_SAMPLE_RATE
    );
    assert!(body.contains(&expected), "{body}");
}

#[tokio::test]
async fn the_page_script_order_puts_the_bundle_before_the_init_module() {
    let body: String = page_at("trace-browser-order", None).await;

    let bundle: usize = body
        .find("<script defer src=\"/script/fixture-")
        .expect("the bundle tag");
    let init: usize = body
        .find("<script type=\"module\">")
        .expect("the init module");
    assert!(bundle < init, "{body}");
}
