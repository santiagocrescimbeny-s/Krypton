//! Phase 8 test-suite, HTTP half: the Jito tip-floor document parser and the
//! `reqwest` source, against the in-process std-only HTTP server of
//! `tests/common` (ephemeral loopback port; nothing leaves the machine).
//!
//! Tests that talk to the server run on the real clock: a paused Tokio clock would
//! auto-advance while the runtime waits on loopback I/O.

mod common;

use std::error::Error;
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use common::{MockResponse, MockServer};
use krypton::errors::TipFeedError;
use krypton::execution::tip::{TipBasis, TipBook, TipFloor, TipPolicy, Urgency};
use krypton::execution::tip_feed::{run_tip_feed, TipFeedSettings, TipSource};
use krypton::execution::tip_http::{
    parse_tip_floor, HttpTipSource, DEFAULT_TIP_FLOOR_URL, MAX_FEED_RESPONSE_BYTES,
};
use krypton::monitor::shutdown_channel;
use serde_json::json;

type TestResult = Result<(), Box<dyn Error>>;

const PATH: &str = "/api/v1/bundles/tip_floor";

fn err_of<T, E>(result: Result<T, E>) -> Result<E, Box<dyn Error>> {
    match result {
        Ok(_) => Err("expected an error but the operation succeeded".into()),
        Err(err) => Ok(err),
    }
}

/// A document shaped like the real feed's answer, with the given amounts (SOL).
fn doc(p50: f64, p75: f64, p95: f64, p99: f64, ema: f64) -> serde_json::Value {
    json!([{
        "time": "2026-10-24T09:30:00Z",
        "landed_tips_25th_percentile": 0.000_001,
        "landed_tips_50th_percentile": p50,
        "landed_tips_75th_percentile": p75,
        "landed_tips_95th_percentile": p95,
        "landed_tips_99th_percentile": p99,
        "ema_landed_tips_50th_percentile": ema
    }])
}

fn document() -> String {
    doc(0.000_02, 0.000_05, 0.0002, 0.0009, 0.000_04).to_string()
}

fn expected() -> TipFloor {
    TipFloor {
        p50: 20_000,
        p75: 50_000,
        p95: 200_000,
        p99: 900_000,
        ema_p50: 40_000,
    }
}

fn server_with(body: String) -> Result<MockServer, Box<dyn Error>> {
    Ok(MockServer::start(move |_, _| {
        MockResponse::json(200, &body)
    })?)
}

fn source_for(server: &MockServer) -> Result<HttpTipSource, Box<dyn Error>> {
    Ok(HttpTipSource::new(&format!("{}{PATH}", server.url()))?)
}

// =====================================================================================================
// The parser
// =====================================================================================================

#[test]
fn the_real_document_shape_is_parsed_into_lamports() -> TestResult {
    assert_eq!(parse_tip_floor(document().as_bytes())?, expected());
    Ok(())
}

#[test]
fn a_json_rpc_envelope_is_accepted_too() -> TestResult {
    let wrapped = format!(r#"{{"jsonrpc":"2.0","id":1,"result":{}}}"#, document());
    assert_eq!(parse_tip_floor(wrapped.as_bytes())?, expected());
    Ok(())
}

#[test]
fn integers_extra_fields_and_extra_entries_are_fine() -> TestResult {
    let body = json!([
        {
            "landed_tips_50th_percentile": 0,
            "landed_tips_75th_percentile": 0,
            "landed_tips_95th_percentile": 1,
            "landed_tips_99th_percentile": 1,
            "ema_landed_tips_50th_percentile": 0,
            "something_new": "ignored"
        },
        {
            "landed_tips_50th_percentile": 0.5,
            "landed_tips_75th_percentile": 0.5,
            "landed_tips_95th_percentile": 0.5,
            "landed_tips_99th_percentile": 0.5,
            "ema_landed_tips_50th_percentile": 0.5
        }
    ])
    .to_string();
    // The first entry is the current one.
    assert_eq!(
        parse_tip_floor(body.as_bytes())?,
        TipFloor {
            p50: 0,
            p75: 0,
            p95: 1_000_000_000,
            p99: 1_000_000_000,
            ema_p50: 0,
        }
    );
    Ok(())
}

#[test]
fn anything_else_is_malformed() -> TestResult {
    let missing_field = json!([{
        "landed_tips_50th_percentile": 0.1,
        "landed_tips_75th_percentile": 0.1,
        "landed_tips_95th_percentile": 0.1,
        "landed_tips_99th_percentile": 0.1
    }])
    .to_string();
    let mut string_amount = doc(0.000_02, 0.000_05, 0.0002, 0.0009, 0.000_04);
    string_amount[0]["landed_tips_50th_percentile"] = json!("0.00002");
    let negative = doc(-0.000_02, 0.000_05, 0.0002, 0.0009, 0.000_04).to_string();
    let huge = doc(0.000_02, 0.000_05, 0.0002, 12.0, 0.000_04).to_string();
    let unordered = doc(0.000_02, 0.000_01, 0.0002, 0.0009, 0.000_04).to_string();
    let cases: Vec<(&str, String)> = vec![
        ("empty body", String::new()),
        ("not json", "<html>rate limited</html>".to_owned()),
        ("null", "null".to_owned()),
        ("empty array", "[]".to_owned()),
        ("empty envelope", r#"{"result":[]}"#.to_owned()),
        ("object without result", r#"{"time":"x"}"#.to_owned()),
        ("number", "42".to_owned()),
        ("missing field", missing_field),
        ("string amount", string_amount.to_string()),
        ("negative amount", negative),
        ("implausible amount", huge),
        ("unordered percentiles", unordered),
    ];
    for (name, body) in cases {
        let error = err_of(parse_tip_floor(body.as_bytes()))?;
        assert!(
            matches!(error, TipFeedError::Malformed { .. }),
            "{name}: {error:?}"
        );
        // The remote text is never copied into the error.
        assert!(
            body.len() < 8 || !error.to_string().contains(body.as_str()),
            "{name}"
        );
    }
    Ok(())
}

#[test]
fn invalid_utf8_and_binary_noise_are_malformed() -> TestResult {
    for body in [
        &[0xff_u8, 0xfe, 0x00][..],
        &[b'[', 0x00, b']'][..],
        &b"[{"[..],
    ] {
        assert!(matches!(
            parse_tip_floor(body),
            Err(TipFeedError::Malformed { .. })
        ));
    }
    Ok(())
}

// =====================================================================================================
// The source
// =====================================================================================================

#[test]
fn the_default_url_is_https_and_valid() -> TestResult {
    assert!(DEFAULT_TIP_FLOOR_URL.starts_with("https://"));
    HttpTipSource::mainnet()?;
    Ok(())
}

#[test]
fn unsafe_urls_are_rejected() -> TestResult {
    for url in [
        "http://bundles.jito.wtf/api/v1/bundles/tip_floor",
        "ftp://bundles.jito.wtf/x",
        "https://user:pass@bundles.jito.wtf/x",
        "https://user@bundles.jito.wtf/x",
        "not a url",
        "",
        "https://",
    ] {
        let error = err_of(HttpTipSource::new(url))?;
        assert!(
            matches!(error, TipFeedError::InvalidConfig { .. }),
            "{url}: {error:?}"
        );
        assert!(url.len() < 8 || !error.to_string().contains(url));
    }
    HttpTipSource::new("http://127.0.0.1:8080/x")?;
    HttpTipSource::new("http://localhost:8080/x")?;
    HttpTipSource::new("https://bundles.jito.wtf/api/v1/bundles/tip_floor")?;
    Ok(())
}

#[tokio::test]
async fn a_good_answer_is_fetched_and_parsed() -> TestResult {
    let server = server_with(document())?;
    let mut source = source_for(&server)?;
    assert_eq!(source.fetch().await?, expected());
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = requests.first().ok_or("no request recorded")?;
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, PATH);
    assert!(request
        .header("user-agent")
        .is_some_and(|agent| agent.starts_with("krypton/")));
    Ok(())
}

#[tokio::test]
async fn non_success_statuses_are_reported_by_code_only() -> TestResult {
    for status in [429_u16, 500, 503] {
        let server = MockServer::start(move |_, _| {
            MockResponse::json(
                status,
                r#"{"error":"secret detail with https://x.example/token"}"#,
            )
        })?;
        let mut source = source_for(&server)?;
        let error = err_of(source.fetch().await)?;
        assert_eq!(error, TipFeedError::HttpStatus { status });
        assert!(!error.to_string().contains("secret"));
    }
    Ok(())
}

#[tokio::test]
async fn a_malformed_answer_is_malformed_not_a_panic() -> TestResult {
    let server = server_with("<html>Cloudflare</html>".to_owned())?;
    let mut source = source_for(&server)?;
    assert!(matches!(
        err_of(source.fetch().await)?,
        TipFeedError::Malformed { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn an_oversized_answer_is_refused() -> TestResult {
    let server =
        MockServer::start(|_, _| MockResponse::raw(200, vec![b' '; MAX_FEED_RESPONSE_BYTES + 1]))?;
    let mut source = source_for(&server)?;
    assert_eq!(
        err_of(source.fetch().await)?,
        TipFeedError::ResponseTooLarge
    );
    Ok(())
}

#[tokio::test]
async fn a_closed_port_is_unreachable() -> TestResult {
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.local_addr()?.port()
    };
    let mut source = HttpTipSource::new(&format!("http://127.0.0.1:{port}{PATH}"))?;
    assert_eq!(err_of(source.fetch().await)?, TipFeedError::Unreachable);
    Ok(())
}

// =====================================================================================================
// The whole feed, over HTTP, into the tip book
// =====================================================================================================

#[tokio::test]
async fn the_http_feed_fills_the_tip_book() -> TestResult {
    let server = server_with(document())?;
    let source = source_for(&server)?;
    let book = Arc::new(TipBook::new(TipPolicy::new(10_000, 1_000_000)?));
    assert_eq!(book.quote(Urgency::Launch, 0)?.basis, TipBasis::Fallback);

    let (trigger, shutdown) = shutdown_channel();
    let task = tokio::spawn(run_tip_feed(
        source,
        Arc::clone(&book),
        TipFeedSettings::default(),
        shutdown,
    ));

    let mut basis = TipBasis::Fallback;
    for _ in 0..100 {
        basis = book.quote(Urgency::Launch, 0)?.basis;
        if basis != TipBasis::Fallback {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_ne!(basis, TipBasis::Fallback, "the feed never reached the book");

    trigger.trigger();
    let stats = task.await??;
    assert_eq!((stats.updates, stats.failures), (1, 0));
    assert_eq!(server.request_count(), 1);
    Ok(())
}

#[tokio::test]
async fn a_rate_limited_feed_leaves_the_book_on_the_fallback() -> TestResult {
    let server = MockServer::start(|_, _| MockResponse::json(429, "{}"))?;
    let source = source_for(&server)?;
    let book = Arc::new(TipBook::new(TipPolicy::new(10_000, 1_000_000)?));
    let (trigger, shutdown) = shutdown_channel();
    let task = tokio::spawn(run_tip_feed(
        source,
        Arc::clone(&book),
        TipFeedSettings::default(),
        shutdown,
    ));
    for _ in 0..100 {
        if server.request_count() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    trigger.trigger();
    let stats = task.await??;
    // Whether the failure was counted before the shutdown is a race; the book is not.
    assert_eq!(stats.updates, 0);
    assert!(server.request_count() >= 1);
    assert_eq!(book.quote(Urgency::Launch, 0)?.basis, TipBasis::Fallback);
    Ok(())
}

// =====================================================================================================
// Source hygiene
// =====================================================================================================

#[test]
fn source_scan_tip_http_has_no_panics_unsafe_code_or_logging() {
    let source = include_str!("../src/execution/tip_http.rs");
    for word in [
        ".unwrap()",
        ".expect(",
        "panic!",
        "todo!",
        "unimplemented!",
        "unsafe",
        "println!",
        "info!",
        "debug!",
        "warn!",
        "error!",
    ] {
        assert!(!source.contains(word), "tip_http.rs contains `{word}`");
    }
}
