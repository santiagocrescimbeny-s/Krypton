//! Phase 5 test-suite: the Jito-backed sell submitter.
//!
//! Conventions
//! - Tests return `Result` and use `?` / `matches!`; no `unwrap()` / `expect()`.
//! - The Block Engine is the in-process, std-only HTTP server in `tests/common`
//!   (ephemeral loopback port); nothing leaves the machine.
//! - Tests that talk to the server run on the real clock with a 1 ms polling
//!   interval: a paused Tokio clock would auto-advance while the runtime waits
//!   on loopback I/O. Only tests that never touch the network (stale-blockhash
//!   checks, dry-run) pause the clock to age a blockhash.
//! - Signatures are deterministic (Ed25519), so the exact payload that reaches
//!   the server can be compared with an independently built transaction.

mod common;

use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use common::{MockResponse, MockServer, RecordedRequest};
use krypton::builder::math::SellPlan;
use krypton::builder::sell::{SellBuilder, SellSettings};
use krypton::builder::{CreateVariant, CurveParams};
use krypton::errors::{BuilderError, ExitError};
use krypton::monitor::exit::{ExitTrigger, SellOrder, SellOutcome, SellSubmitter};
use krypton::monitor::submit::{
    blockhash_channel, stamp_blockhash, JitoSellSubmitter, TimedBlockhash, MAX_BLOCKHASH_AGE,
};
use krypton::network::jito::{
    encode_base64, serialize_transaction, JitoClient, JitoTip, JITO_TIP_ACCOUNTS,
};
use krypton::security::SecureWallet;
use reqwest::Url;
use serde_json::{json, Value};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_sdk::hash::Hash;
use tokio::sync::watch;

type TestResult = Result<(), Box<dyn Error>>;

const BUNDLE_ID: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";
const TIP_LAMPORTS: u64 = 100_000;

// --- Helpers -------------------------------------------------------------------------------

fn mint() -> Pubkey {
    Pubkey::new_from_array([2u8; 32])
}

fn wallet(seed: u8) -> Result<SecureWallet, Box<dyn Error>> {
    let bytes = Keypair::new_from_array([seed; 32]).to_bytes();
    Ok(SecureWallet::from_keypair_bytes(&bytes)?)
}

fn settings() -> SellSettings {
    SellSettings {
        compute_unit_limit: 200_000,
        priority_fee_micro_lamports: 100_000,
        max_priority_fee_lamports: 5_000_000,
        buyback_fee_recipient: None,
    }
}

fn builder_for(wallet: &SecureWallet) -> Result<SellBuilder, Box<dyn Error>> {
    Ok(SellBuilder::new(
        wallet.pubkey(),
        mint(),
        CreateVariant::CreateV2,
        settings(),
    )?)
}

fn tip() -> Result<JitoTip, Box<dyn Error>> {
    Ok(JitoTip::new(
        JITO_TIP_ACCOUNTS[0],
        TIP_LAMPORTS,
        TIP_LAMPORTS,
    )?)
}

fn order() -> Result<SellOrder, Box<dyn Error>> {
    let plan = SellPlan::from_token_amount(&CurveParams::default(), 24_375_000_000_000, 125, 500)?;
    Ok(SellOrder {
        sequence: 0,
        trigger: ExitTrigger::Tranche { index: 0 },
        plan,
        issued_at_ms: 1_000,
        curve_slot: 10,
        cost_lamports: 125_000,
    })
}

fn rpc_ok(result: &Value) -> MockResponse {
    MockResponse::json(
        200,
        &json!({ "jsonrpc": "2.0", "result": result, "id": 1 }).to_string(),
    )
}

fn status_reply(status: &str, slot: Value) -> MockResponse {
    rpc_ok(&json!({
        "context": { "slot": 280_999_000u64 },
        "value": [{ "bundle_id": BUNDLE_ID, "status": status, "landed_slot": slot }],
    }))
}

fn method_of(request: &RecordedRequest) -> Result<String, Box<dyn Error>> {
    let body: Value = serde_json::from_slice(&request.body)?;
    Ok(body["method"].as_str().ok_or("no method")?.to_owned())
}

struct Fixture {
    submitter: JitoSellSubmitter,
    blockhash: watch::Sender<TimedBlockhash>,
    wallet: Arc<SecureWallet>,
    builder: SellBuilder,
}

fn fixture(server: &MockServer, dry_run: bool, max_polls: u32) -> Result<Fixture, Box<dyn Error>> {
    let wallet = Arc::new(wallet(1)?);
    let builder = builder_for(&wallet)?;
    let client = Arc::new(
        JitoClient::new(&Url::parse(&server.url())?, dry_run)?
            .with_retries(0, Duration::from_millis(1)),
    );
    let (blockhash, receiver) = blockhash_channel(Hash::new_from_array([3u8; 32]));
    let submitter = JitoSellSubmitter::new(
        client,
        builder.clone(),
        Arc::clone(&wallet),
        receiver,
        tip()?,
    )
    .with_polling(Duration::from_millis(1), max_polls);
    Ok(Fixture {
        submitter,
        blockhash,
        wallet,
        builder,
    })
}

/// Script: `sendBundle` returns the id, then each status in turn.
fn scripted(statuses: Vec<MockResponse>) -> Result<MockServer, Box<dyn Error>> {
    Ok(MockServer::start(move |index, _| {
        if index == 0 {
            return rpc_ok(&json!(BUNDLE_ID));
        }
        statuses
            .get(index - 1)
            .or_else(|| statuses.last())
            .cloned()
            .unwrap_or_else(|| status_reply("Pending", Value::Null))
    })?)
}

// --- Tests ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_landed_bundle_reports_its_slot_and_carries_the_signed_sell_and_tip() -> TestResult {
    let server = scripted(vec![
        status_reply("Pending", Value::Null),
        status_reply("Landed", json!(280_999_001u64)),
    ])?;
    let fixture = fixture(&server, false, 5)?;
    let order = order()?;

    let outcome = fixture.submitter.submit(&order).await?;
    assert_eq!(outcome, SellOutcome::Landed { slot: 280_999_001 });

    let requests = server.requests();
    let methods = requests
        .iter()
        .map(method_of)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        methods,
        vec![
            "sendBundle",
            "getInflightBundleStatuses",
            "getInflightBundleStatuses"
        ]
    );

    // The bundle is exactly one transaction: [limit, price, sell_v3, tip], signed.
    let body: Value = serde_json::from_slice(&requests.first().ok_or("no request")?.body)?;
    let sent = body["params"][0]
        .as_array()
        .ok_or("transactions must be an array")?;
    assert_eq!(sent.len(), 1);
    let tip_instruction = tip()?.instruction(&fixture.builder.payer())?;
    let expected = fixture.builder.build_signed(
        &order.plan,
        Hash::new_from_array([3u8; 32]),
        std::slice::from_ref(&tip_instruction),
        &fixture.wallet,
    )?;
    assert_eq!(
        sent.first().and_then(Value::as_str),
        Some(encode_base64(&serialize_transaction(&expected)).as_str())
    );
    Ok(())
}

#[tokio::test]
async fn each_order_uses_the_freshest_blockhash() -> TestResult {
    let server = scripted(vec![status_reply("Landed", json!(1u64))])?;
    let fixture = fixture(&server, false, 3)?;
    let order = order()?;

    fixture.submitter.submit(&order).await?;
    fixture
        .blockhash
        .send(stamp_blockhash(Hash::new_from_array([4u8; 32])))
        .map_err(|_| "receiver dropped")?;
    fixture.submitter.submit(&order).await?;

    let sends: Vec<String> = server
        .requests()
        .iter()
        .filter(|request| matches!(method_of(request).as_deref(), Ok("sendBundle")))
        .map(|request| {
            let body: Result<Value, _> = serde_json::from_slice(&request.body);
            body.map(|value| {
                value["params"][0][0]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
        })
        .collect::<Result<_, _>>()?;
    assert_eq!(sends.len(), 2);
    assert_ne!(
        sends.first(),
        sends.get(1),
        "a new blockhash changes the payload"
    );
    Ok(())
}

#[tokio::test]
async fn dry_run_assembles_and_signs_but_never_sends() -> TestResult {
    let server = scripted(vec![])?;
    let fixture = fixture(&server, true, 3)?;
    let outcome = fixture.submitter.submit(&order()?).await?;
    assert_eq!(outcome, SellOutcome::Simulated);
    assert_eq!(server.request_count(), 0);
    Ok(())
}

#[tokio::test]
async fn failed_and_invalid_bundles_are_reported_as_failed() -> TestResult {
    for status in ["Failed", "Invalid"] {
        let server = scripted(vec![status_reply(status, Value::Null)])?;
        let fixture = fixture(&server, false, 5)?;
        assert_eq!(
            fixture.submitter.submit(&order()?).await?,
            SellOutcome::Failed
        );
        // Decided on the first status check: no more polling.
        assert_eq!(server.request_count(), 2, "{status}");
    }
    Ok(())
}

#[tokio::test]
async fn an_unresolved_bundle_is_unknown_not_failed_after_the_polling_budget() -> TestResult {
    let server = scripted(vec![status_reply("Pending", Value::Null)])?;
    let fixture = fixture(&server, false, 4)?;
    // The bundle may still land: reporting Failed would let the engine sell the
    // same tokens again and burn a second tip.
    assert_eq!(
        fixture.submitter.submit(&order()?).await?,
        SellOutcome::Unknown
    );
    assert_eq!(server.request_count(), 1 + 4);

    // A bundle the engine has never heard of is handled the same way.
    let server = scripted(vec![rpc_ok(
        &json!({ "context": { "slot": 1u64 }, "value": null }),
    )])?;
    let fixture = fixture(&server, false, 2)?;
    assert_eq!(
        fixture.submitter.submit(&order()?).await?,
        SellOutcome::Unknown
    );
    assert_eq!(server.request_count(), 1 + 2);

    // No polling budget at all: nothing is known either.
    let server = scripted(vec![])?;
    let fixture = fixture(&server, false, 0)?;
    assert_eq!(
        fixture.submitter.submit(&order()?).await?,
        SellOutcome::Unknown
    );
    assert_eq!(server.request_count(), 1);
    Ok(())
}

#[tokio::test]
async fn the_first_status_check_is_immediate() -> TestResult {
    let server = scripted(vec![status_reply("Landed", json!(42u64))])?;
    let mut fixture = fixture(&server, false, 3)?;
    // An hour between checks: any sleep before the first one would time out.
    fixture.submitter = fixture
        .submitter
        .with_polling(Duration::from_secs(3_600), 3);
    let order = order()?;
    let outcome = tokio::time::timeout(Duration::from_secs(10), fixture.submitter.submit(&order))
        .await
        .map_err(|_| "the first status check waited for the polling interval")??;
    assert_eq!(outcome, SellOutcome::Landed { slot: 42 });
    assert_eq!(server.request_count(), 2);
    Ok(())
}

#[tokio::test]
async fn transient_status_errors_are_retried_until_resolved() -> TestResult {
    let server = scripted(vec![
        MockResponse::json(503, "busy"),
        MockResponse::json(429, "{}"),
        status_reply("Landed", json!(77u64)),
    ])?;
    let fixture = fixture(&server, false, 6)?;
    assert_eq!(
        fixture.submitter.submit(&order()?).await?,
        SellOutcome::Landed { slot: 77 }
    );
    Ok(())
}

#[tokio::test]
async fn a_permanent_status_error_after_the_send_is_unknown_not_an_error() -> TestResult {
    let server = scripted(vec![MockResponse::json(400, "{}")])?;
    let fixture = fixture(&server, false, 6)?;
    // The Block Engine already accepted the bundle. An error here must not turn
    // into Failed (the executor maps errors to Failed, and the tokens would be resold).
    assert_eq!(
        fixture.submitter.submit(&order()?).await?,
        SellOutcome::Unknown
    );
    assert_eq!(server.request_count(), 2);
    Ok(())
}

#[tokio::test]
async fn a_send_that_times_out_is_unknown() -> TestResult {
    let server =
        MockServer::start(|_, _| rpc_ok(&json!(BUNDLE_ID)).delayed(Duration::from_millis(800)))?;
    let wallet = Arc::new(wallet(1)?);
    let client = Arc::new(
        JitoClient::new(&Url::parse(&server.url())?, false)?
            .with_retries(0, Duration::from_millis(1))
            .with_timeout(Duration::from_millis(100)),
    );
    let (_blockhash, receiver) = blockhash_channel(Hash::new_from_array([3u8; 32]));
    let submitter = JitoSellSubmitter::new(client, builder_for(&wallet)?, wallet, receiver, tip()?);
    // The request may have reached the Block Engine: do not report a failure.
    assert_eq!(submitter.submit(&order()?).await?, SellOutcome::Unknown);
    Ok(())
}

#[tokio::test]
async fn a_definite_send_rejection_is_an_error() -> TestResult {
    let server = MockServer::start(|_, _| MockResponse::json(503, "busy"))?;
    let fixture = fixture(&server, false, 6)?;
    let result = fixture.submitter.submit(&order()?).await;
    assert!(
        matches!(result, Err(ExitError::Jito(ref error)) if error.is_retryable()),
        "{result:?}"
    );
    // A second sendBundle could double-sell: exactly one attempt.
    assert_eq!(server.request_count(), 1);
    Ok(())
}

#[tokio::test]
async fn a_foreign_wallet_is_refused_before_any_request() -> TestResult {
    let server = scripted(vec![])?;
    let mut fixture = fixture(&server, false, 3)?;
    let stranger = Arc::new(wallet(9)?);
    let (_blockhash, receiver) = blockhash_channel(Hash::new_from_array([3u8; 32]));
    let client = Arc::new(
        JitoClient::new(&Url::parse(&server.url())?, false)?
            .with_retries(0, Duration::from_millis(1)),
    );
    fixture.submitter =
        JitoSellSubmitter::new(client, fixture.builder.clone(), stranger, receiver, tip()?);
    let result = fixture.submitter.submit(&order()?).await;
    assert!(
        matches!(result, Err(ExitError::Builder(BuilderError::PayerMismatch))),
        "{result:?}"
    );
    assert_eq!(server.request_count(), 0);
    Ok(())
}

// --- Blockhash freshness (audit finding #29) -----------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_stale_blockhash_is_refused_before_anything_is_signed_or_sent() -> TestResult {
    let server = scripted(vec![])?;
    let fixture = fixture(&server, false, 3)?;
    tokio::time::sleep(MAX_BLOCKHASH_AGE + Duration::from_millis(1)).await;

    let result = fixture.submitter.submit(&order()?).await;
    assert!(
        matches!(result, Err(ExitError::StaleBlockhash)),
        "{result:?}"
    );
    assert_eq!(server.request_count(), 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn the_age_limit_is_forty_seconds_inclusive() -> TestResult {
    assert_eq!(MAX_BLOCKHASH_AGE, Duration::from_secs(40));
    // Dry-run: the whole path is exercised without touching the network.
    let server = scripted(vec![])?;
    let fixture = fixture(&server, true, 3)?;

    tokio::time::sleep(MAX_BLOCKHASH_AGE).await;
    assert_eq!(
        fixture.submitter.submit(&order()?).await?,
        SellOutcome::Simulated
    );

    tokio::time::sleep(Duration::from_millis(1)).await;
    let result = fixture.submitter.submit(&order()?).await;
    assert!(
        matches!(result, Err(ExitError::StaleBlockhash)),
        "{result:?}"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn publishing_a_new_blockhash_unblocks_the_submitter() -> TestResult {
    let server = scripted(vec![])?;
    let fixture = fixture(&server, true, 3)?;
    tokio::time::sleep(Duration::from_secs(120)).await;
    let stale = fixture.submitter.submit(&order()?).await;
    assert!(matches!(stale, Err(ExitError::StaleBlockhash)), "{stale:?}");

    // The refresher is back.
    fixture
        .blockhash
        .send(stamp_blockhash(Hash::new_from_array([5u8; 32])))
        .map_err(|_| "receiver dropped")?;
    assert_eq!(
        fixture.submitter.submit(&order()?).await?,
        SellOutcome::Simulated
    );
    Ok(())
}

#[test]
fn the_submitter_debug_output_never_contains_key_material() -> TestResult {
    let server = scripted(vec![])?;
    let fixture = fixture(&server, true, 3)?;
    let text = format!("{:?}", fixture.submitter);
    assert!(text.starts_with("JitoSellSubmitter"));
    assert!(!text.contains("keypair") && !text.contains("secret"));
    Ok(())
}
