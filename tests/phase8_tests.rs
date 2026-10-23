//! Phase 8 test-suite: the Yellowstone gRPC adapter and the Jito tip-floor feed.
//!
//! Conventions
//! - Tests return `Result` and use `?` / `matches!`; no `unwrap()` / `expect()`.
//! - There is no gRPC server here: the adapter is exercised at its two seams,
//!   `YellowstoneLink` / `YellowstoneSession`, with scripted connections that
//!   deliver messages, fail with a chosen gRPC status, close or go silent. That is
//!   exactly the boundary the `tonic` binding implements, so every decision the
//!   adapter takes (conversion, ping handling, retry classification) runs for real,
//!   together with the real `GeyserMonitor` supervisor and `LiveExecutor::run_exit`.
//! - Time-dependent tests run on Tokio's paused clock.
//! - The HTTP half of the tip feed (parser, `reqwest` source) is covered in
//!   `phase8_http_tests.rs`.

use std::collections::VecDeque;
use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use krypton::builder::alt::LaunchLookupTable;
use krypton::builder::math::{BuyPlan, CurveParams, DEFAULT_TOTAL_FEE_BPS};
use krypton::builder::sell::{SellBuilder, SellSettings};
use krypton::builder::transaction::{LaunchBuilder, LaunchParams, MintSigner};
use krypton::builder::{CreateOptions, CreateVariant, LaunchMetadata};
use krypton::errors::{JitoError, MonitorError, StreamErrorKind, TipFeedError, YellowstoneError};
use krypton::execution::gate::ArmedLive;
use krypton::execution::guards::GuardLimits;
use krypton::execution::live::{BlockEngine, Clock, ExitRun, LaunchRequest, LiveExecutor};
use krypton::execution::tip::{TipBasis, TipBook, TipFloor, TipPolicy, Urgency};
use krypton::execution::tip_feed::{
    floor_from_sol, run_tip_feed, sol_to_lamports, SolTipFloor, TipFeedSettings, TipSource,
    MAX_PLAUSIBLE_TIP_LAMPORTS,
};
use krypton::monitor::curve::PriceReference;
use krypton::monitor::exit::{ExitEngine, ExitPlan, ExitSettings, Position};
use krypton::monitor::geyser::{
    ConnectionState, EventStream, GeyserMonitor, HealthStatus, MonitorSettings, StreamCommitment,
    StreamTransport, StreamUpdate, SubscriptionRequest,
};
use krypton::monitor::shutdown_channel;
use krypton::monitor::submit::blockhash_channel;
use krypton::network::yellowstone::{
    build_wire_request, classify_grpc_code, commitment_code, convert_message, grpc_failure,
    Followup, WireAccount, WireInstruction, WireMessage, WireRequest, WireTransaction,
    YellowstoneLink, YellowstoneSession, YellowstoneSettings, YellowstoneStream,
    YellowstoneTransport, MAX_INSTRUCTIONS_PER_TRANSACTION, PING_ID,
};
use krypton::network::{BundleId, InflightStatus};
use krypton::security::{SecretString, SecureWallet};
use krypton::simulation::market::MarketState;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_sdk::hash::Hash;
use solana_sdk::transaction::VersionedTransaction;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

type TestResult = Result<(), Box<dyn Error>>;

fn err_of<T, E>(result: Result<T, E>) -> Result<E, Box<dyn Error>> {
    match result {
        Ok(_) => Err("expected an error but the operation succeeded".into()),
        Err(err) => Ok(err),
    }
}

fn key(byte: u8) -> Pubkey {
    Pubkey::new_from_array([byte; 32])
}

fn protocol() -> MonitorError {
    MonitorError::Connection {
        kind: StreamErrorKind::Protocol,
    }
}

// --- Live-executor fixtures (shared shape with phase7_tests.rs) ----------------------------------

fn keypair_bytes(seed: u8) -> [u8; 64] {
    Keypair::new_from_array([seed; 32]).to_bytes()
}

fn wallet(seed: u8) -> Result<SecureWallet, Box<dyn Error>> {
    Ok(SecureWallet::from_keypair_bytes(&keypair_bytes(seed))?)
}

#[derive(Default)]
struct Script {
    sends: VecDeque<Result<BundleId, JitoError>>,
    statuses: VecDeque<Result<InflightStatus, JitoError>>,
}

struct FakeEngine {
    dry_run: bool,
    script: Mutex<Script>,
    /// Answer once the scripted statuses run out.
    default_status: InflightStatus,
    sent: AtomicUsize,
    polls: AtomicUsize,
}

impl FakeEngine {
    fn new(default_status: InflightStatus) -> Self {
        Self {
            dry_run: false,
            script: Mutex::new(Script::default()),
            default_status,
            sent: AtomicUsize::new(0),
            polls: AtomicUsize::new(0),
        }
    }

    fn landing() -> Self {
        Self::new(InflightStatus::Landed { slot: 777 })
    }

    fn sent(&self) -> usize {
        self.sent.load(Ordering::SeqCst)
    }
}

fn bundle_id() -> Result<BundleId, JitoError> {
    BundleId::parse(&"ab".repeat(32))
}

impl BlockEngine for FakeEngine {
    fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    async fn send_bundle(
        &self,
        transactions: &[VersionedTransaction],
    ) -> Result<BundleId, JitoError> {
        self.sent.fetch_add(transactions.len(), Ordering::SeqCst);
        let scripted = self
            .script
            .lock()
            .ok()
            .and_then(|mut s| s.sends.pop_front());
        scripted.unwrap_or_else(bundle_id)
    }

    async fn inflight_status(&self, _id: &BundleId) -> Result<InflightStatus, JitoError> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        let scripted = self
            .script
            .lock()
            .ok()
            .and_then(|mut s| s.statuses.pop_front());
        scripted.unwrap_or(Ok(self.default_status))
    }
}

fn mint_signer() -> MintSigner {
    MintSigner::from_seed([5; 32])
}

fn launch_builder(
    payer: &SecureWallet,
    supply_bps: u16,
    slippage_bps: u16,
) -> Result<LaunchBuilder, Box<dyn Error>> {
    let buy = BuyPlan::from_supply_bps(
        &CurveParams::default(),
        supply_bps,
        DEFAULT_TOTAL_FEE_BPS,
        slippage_bps,
    )?;
    let params = LaunchParams {
        metadata: LaunchMetadata::new(
            "Krypton Live",
            "KLIVE",
            &format!("https://ipfs.io/ipfs/{}", "b".repeat(46)),
        )?,
        variant: CreateVariant::CreateV2,
        options: CreateOptions::default(),
        creator: None,
        buy,
        compute_unit_limit: 300_000,
        priority_fee_micro_lamports: 100_000,
        max_priority_fee_lamports: 5_000_000,
        buyback_fee_recipient: None,
    };
    Ok(LaunchBuilder::new(
        payer.pubkey(),
        mint_signer().pubkey(),
        params,
    )?)
}

const RENT: u64 = 5_000_000;
const FALLBACK_TIP: u64 = 100_000;
const TIP_CAP: u64 = 1_000_000;

fn armed(per_tx: u64, daily: u64) -> Result<ArmedLive, Box<dyn Error>> {
    Ok(ArmedLive {
        limits: GuardLimits::new(per_tx, daily, 500)?,
        tip_policy: TipPolicy::new(FALLBACK_TIP, TIP_CAP)?,
    })
}

fn fixed_clock(now: u64) -> Clock {
    Arc::new(move || Ok(now))
}

fn executor(
    engine: Arc<FakeEngine>,
    payer: &SecureWallet,
    armed: &ArmedLive,
) -> Result<LiveExecutor<FakeEngine>, Box<dyn Error>> {
    // `SecureWallet` is deliberately not `Clone`: the executor gets its own instance of
    // the same key, which must be the payer the fixture's builders were created for.
    let own = wallet(7)?;
    if own.pubkey() != payer.pubkey() {
        return Err("the fixture payer is not the seed-7 wallet".into());
    }
    Ok(LiveExecutor::with_engine(engine, Arc::new(own), armed)?
        .with_polling(Duration::from_millis(100), 5)
        .with_clock(fixed_clock(1_000)))
}

fn request<'a>(
    builder: &'a LaunchBuilder,
    mint: &'a MintSigner,
    table: &'a LaunchLookupTable,
    available: u64,
) -> LaunchRequest<'a> {
    LaunchRequest {
        builder,
        mint,
        table,
        available_lamports: available,
        create_rent_lamports: RENT,
        recent_blockhash: Hash::new_from_array([4; 32]),
    }
}

struct Fixture {
    payer: SecureWallet,
    builder: LaunchBuilder,
    mint: MintSigner,
    table: LaunchLookupTable,
}

fn fixture() -> Result<Fixture, Box<dyn Error>> {
    let payer = wallet(7)?;
    let builder = launch_builder(&payer, 100, 500)?;
    Ok(Fixture {
        payer,
        builder,
        mint: mint_signer(),
        table: LaunchLookupTable::from_static(Pubkey::new_from_array([0xAA; 32]))?,
    })
}

fn sell_builder(payer: &SecureWallet) -> Result<SellBuilder, Box<dyn Error>> {
    Ok(SellBuilder::new(
        payer.pubkey(),
        mint_signer().pubkey(),
        CreateVariant::CreateV2,
        SellSettings {
            compute_unit_limit: 200_000,
            priority_fee_micro_lamports: 100_000,
            max_priority_fee_lamports: 5_000_000,
            buyback_fee_recipient: None,
        },
    )?)
}

// =====================================================================================================
// Scripted gRPC link
// =====================================================================================================

/// One step of a scripted connection.
enum Step {
    Msg(WireMessage),
    Fail(MonitorError),
    Close,
    /// Silence: the server never sends again (the idle watchdog must notice).
    Hang,
}

/// One scripted `open`.
enum Connection {
    Refuse(MonitorError),
    Serve(Vec<Step>),
}

#[derive(Default)]
struct LinkLog {
    opened_at: Mutex<Vec<Instant>>,
    requests: Mutex<Vec<WireRequest>>,
    origins: Mutex<Vec<String>>,
    pings_answered: AtomicUsize,
}

struct ScriptedLink {
    script: VecDeque<Connection>,
    log: Arc<LinkLog>,
    /// The first `answer_ping` never completes (to prove cancel safety).
    stall_first_ping: bool,
    fail_ping: bool,
}

impl ScriptedLink {
    fn new(script: Vec<Connection>) -> (Self, Arc<LinkLog>) {
        let log = Arc::new(LinkLog::default());
        (
            Self {
                script: script.into(),
                log: Arc::clone(&log),
                stall_first_ping: false,
                fail_ping: false,
            },
            log,
        )
    }
}

struct ScriptedSession {
    steps: VecDeque<Step>,
    log: Arc<LinkLog>,
    stall_next_ping: bool,
    fail_ping: bool,
}

impl YellowstoneLink for ScriptedLink {
    type Session = ScriptedSession;

    async fn open(
        &mut self,
        settings: &YellowstoneSettings,
        request: &WireRequest,
    ) -> Result<ScriptedSession, MonitorError> {
        if let Ok(mut times) = self.log.opened_at.lock() {
            times.push(Instant::now());
        }
        if let Ok(mut requests) = self.log.requests.lock() {
            requests.push(request.clone());
        }
        if let Ok(mut origins) = self.log.origins.lock() {
            origins.push(settings.origin().to_owned());
        }
        // Once the script is used up the server stays connected and silent.
        let next = self
            .script
            .pop_front()
            .unwrap_or_else(|| Connection::Serve(vec![Step::Hang]));
        match next {
            Connection::Refuse(error) => Err(error),
            Connection::Serve(steps) => Ok(ScriptedSession {
                steps: steps.into(),
                log: Arc::clone(&self.log),
                stall_next_ping: std::mem::take(&mut self.stall_first_ping),
                fail_ping: self.fail_ping,
            }),
        }
    }
}

impl YellowstoneSession for ScriptedSession {
    async fn next_message(&mut self) -> Result<Option<WireMessage>, MonitorError> {
        match self.steps.pop_front() {
            Some(Step::Msg(message)) => Ok(Some(message)),
            Some(Step::Fail(error)) => Err(error),
            Some(Step::Close) => Ok(None),
            Some(Step::Hang) | None => {
                std::future::pending::<()>().await;
                Ok(None)
            }
        }
    }

    async fn answer_ping(&mut self) -> Result<(), MonitorError> {
        if self.fail_ping {
            return Err(grpc_failure(14));
        }
        if self.stall_next_ping {
            self.stall_next_ping = false;
            std::future::pending::<()>().await;
        }
        self.log.pings_answered.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn settings() -> Result<YellowstoneSettings, Box<dyn Error>> {
    Ok(YellowstoneSettings::new(
        "https://grpc.example.com:443/some-path-token",
        Some(SecretString::new("x-token-value".to_owned())),
    )?)
}

fn request_for(curve: Pubkey) -> SubscriptionRequest {
    SubscriptionRequest {
        accounts: vec![curve],
        instruction_program: Some(key(0x77)),
        commitment: StreamCommitment::Processed,
    }
}

fn account_message(slot: u64, pubkey: Pubkey, data: Vec<u8>) -> WireMessage {
    WireMessage::Account(WireAccount {
        slot,
        pubkey: pubkey.to_bytes().to_vec(),
        owner: key(0x77).to_bytes().to_vec(),
        lamports: 1_000,
        write_version: slot.saturating_mul(10),
        data,
    })
}

fn tx_message(slot: u64, keys: &[Pubkey], instructions: &[(u32, &[u8])]) -> WireTransaction {
    WireTransaction {
        slot,
        account_keys: keys.iter().map(|k| k.to_bytes().to_vec()).collect(),
        loaded_writable: Vec::new(),
        loaded_readonly: Vec::new(),
        instructions: instructions
            .iter()
            .map(|(index, data)| WireInstruction {
                program_id_index: *index,
                data: data.to_vec(),
            })
            .collect(),
        inner_instructions: Vec::new(),
    }
}

fn convert(
    message: WireMessage,
    program: Option<Pubkey>,
) -> Result<(Vec<StreamUpdate>, Followup), MonitorError> {
    let mut out = VecDeque::new();
    let followup = convert_message(message, program, &mut out)?;
    Ok((out.into_iter().collect(), followup))
}

// =====================================================================================================
// Settings
// =====================================================================================================

#[test]
fn valid_endpoints_are_accepted() -> TestResult {
    for endpoint in [
        "https://grpc.example.com",
        "https://grpc.example.com:443",
        "https://grpc.example.com/some/path",
        "HTTPS://GRPC.EXAMPLE.COM",
        "https://1.2.3.4:10000",
        "http://localhost:10000",
        "http://127.0.0.1:10000",
        "http://[::1]:10000",
        "  https://grpc.example.com  ",
    ] {
        YellowstoneSettings::new(endpoint, None)?;
    }
    Ok(())
}

#[test]
fn unsafe_endpoints_are_rejected_without_being_echoed() -> TestResult {
    let long = format!("https://{}.com", "a".repeat(3_000));
    let cases: Vec<(&str, &str)> = vec![
        ("", "is empty"),
        ("grpc.example.com", "has no scheme"),
        ("ftp://grpc.example.com", "scheme must be https"),
        ("ws://grpc.example.com", "scheme must be https"),
        (
            "http://grpc.example.com",
            "plain http is only allowed for loopback",
        ),
        (
            "http://localhost.evil.com",
            "plain http is only allowed for loopback",
        ),
        (
            "https://user:pass@grpc.example.com",
            "must not carry credentials",
        ),
        (
            "https://token@grpc.example.com",
            "must not carry credentials",
        ),
        ("https://", "has no host"),
        ("https:///path", "has no host"),
        ("https://:443", "has no host"),
        ("https://grpc.example.com:0", "has an invalid port"),
        ("https://grpc.example.com:99999", "has an invalid port"),
        ("https://grpc.example.com:abc", "has an invalid port"),
        ("https://[::1", "has an invalid host"),
        (
            "https://exa mple.com",
            "contains whitespace or control characters",
        ),
        (long.as_str(), "is too long"),
    ];
    for (endpoint, reason) in cases {
        let error = err_of(YellowstoneSettings::new(endpoint, None))?;
        let text = error.to_string();
        assert!(text.contains(reason), "{endpoint:?}: {text}");
        assert!(
            endpoint.len() < 8 || !text.contains(endpoint),
            "the endpoint leaked into: {text}"
        );
    }
    Ok(())
}

#[test]
fn the_x_token_is_validated() -> TestResult {
    for token in ["", "   ", "has space", "tab\there", "new\nline"] {
        let error = err_of(YellowstoneSettings::new(
            "https://grpc.example.com",
            Some(SecretString::new(token.to_owned())),
        ))?;
        assert!(
            matches!(error, YellowstoneError::InvalidToken { .. }),
            "{error:?}"
        );
        if !token.trim().is_empty() {
            assert!(!error.to_string().contains(token), "the token leaked");
        }
    }
    let too_long = SecretString::new("t".repeat(3_000));
    assert!(YellowstoneSettings::new("https://grpc.example.com", Some(too_long)).is_err());
    assert!(YellowstoneSettings::new(
        "https://grpc.example.com",
        Some(SecretString::new("abc123".to_owned()))
    )
    .is_ok());
    Ok(())
}

#[test]
fn debug_and_origin_never_reveal_the_path_or_the_token() -> TestResult {
    let settings = settings()?;
    assert_eq!(settings.origin(), "https://grpc.example.com:443");
    let text = format!("{settings:?}");
    assert!(text.contains("grpc.example.com"), "{text}");
    assert!(!text.contains("some-path-token"), "{text}");
    assert!(!text.contains("x-token-value"), "{text}");
    assert_eq!(
        settings.endpoint(),
        "https://grpc.example.com:443/some-path-token"
    );
    assert_eq!(
        settings.x_token().map(SecretString::expose_secret),
        Some("x-token-value")
    );

    let query = YellowstoneSettings::new("https://grpc.example.com?token=abc", None)?;
    assert_eq!(query.origin(), "https://grpc.example.com");
    let ipv6 = YellowstoneSettings::new("http://[::1]:10000/x", None)?;
    assert_eq!(ipv6.origin(), "http://[::1]:10000");
    Ok(())
}

// =====================================================================================================
// gRPC status classification
// =====================================================================================================

#[test]
fn grpc_codes_map_onto_retry_decisions() {
    use StreamErrorKind::{InvalidRequest, Other, Overloaded, Protocol, Unauthenticated};
    let expected = [
        (0, Other),
        (1, Other),
        (2, Other),
        (3, InvalidRequest),
        (4, Other),
        (5, Other),
        (6, Other),
        (7, Unauthenticated),
        (8, Overloaded),
        (9, Other),
        (10, Other),
        (11, Other),
        (12, InvalidRequest),
        (13, Protocol),
        (14, Overloaded),
        (15, Protocol),
        (16, Unauthenticated),
        (-1, Other),
        (17, Other),
        (i32::MAX, Other),
    ];
    for (code, kind) in expected {
        assert_eq!(classify_grpc_code(code), kind, "code {code}");
    }
}

#[test]
fn bad_credentials_and_bad_requests_are_never_retried() {
    for code in [3, 7, 12, 16] {
        assert!(!grpc_failure(code).is_retryable(), "code {code}");
    }
    for code in [1, 2, 4, 8, 13, 14, 15] {
        assert!(grpc_failure(code).is_retryable(), "code {code}");
    }
}

// =====================================================================================================
// The subscription request
// =====================================================================================================

#[test]
fn commitment_codes_match_the_protobuf_enum() {
    assert_eq!(commitment_code(StreamCommitment::Processed), 0);
    assert_eq!(commitment_code(StreamCommitment::Confirmed), 1);
    assert_eq!(commitment_code(StreamCommitment::Finalized), 2);
}

#[test]
fn the_wire_request_watches_the_accounts_and_their_transactions() {
    let curve = key(1);
    let token_account = key(2);
    let request = SubscriptionRequest {
        accounts: vec![curve, token_account],
        instruction_program: Some(key(0x77)),
        commitment: StreamCommitment::Confirmed,
    };
    let wire = build_wire_request(&request, false);
    assert_eq!(
        wire.accounts,
        vec![curve.to_string(), token_account.to_string()]
    );
    assert_eq!(wire.transaction_account_include, wire.accounts);
    assert!(wire.exclude_votes && wire.exclude_failed && wire.include_slots);
    assert_eq!(wire.commitment, 1);
    assert_eq!(wire.ping_id, Some(PING_ID));
}

#[test]
fn the_program_wide_option_adds_the_program_to_the_transaction_filter() {
    let request = request_for(key(1));
    let narrow = build_wire_request(&request, false);
    let wide = build_wire_request(&request, true);
    assert_eq!(
        wide.accounts, narrow.accounts,
        "the account filter is unchanged"
    );
    assert_eq!(wide.transaction_account_include.len(), 2);
    assert_eq!(
        wide.transaction_account_include.last(),
        Some(&key(0x77).to_string())
    );
}

#[test]
fn without_a_program_there_is_no_transaction_filter() {
    let request = SubscriptionRequest {
        accounts: vec![key(1)],
        instruction_program: None,
        commitment: StreamCommitment::Processed,
    };
    for program_wide in [false, true] {
        let wire = build_wire_request(&request, program_wide);
        assert!(wire.transaction_account_include.is_empty());
        assert_eq!(wire.accounts.len(), 1);
    }
}

// =====================================================================================================
// Message conversion
// =====================================================================================================

#[test]
fn an_account_write_becomes_an_account_update() -> TestResult {
    let (updates, followup) = convert(account_message(42, key(9), vec![1, 2, 3]), None)?;
    assert_eq!(followup, Followup::None);
    assert_eq!(updates.len(), 1);
    let Some(StreamUpdate::Account(account)) = updates.first() else {
        return Err("expected an account update".into());
    };
    assert_eq!(account.slot, 42);
    assert_eq!(account.write_version, 420);
    assert_eq!(account.pubkey, key(9));
    assert_eq!(account.owner, key(0x77));
    assert_eq!(account.lamports, 1_000);
    assert_eq!(account.data.as_ref(), &[1, 2, 3]);
    Ok(())
}

#[test]
fn keys_of_the_wrong_length_are_protocol_violations_and_append_nothing() -> TestResult {
    for (pubkey_len, owner_len) in [(31, 32), (33, 32), (32, 31), (0, 32), (32, 0)] {
        let message = WireMessage::Account(WireAccount {
            slot: 1,
            pubkey: vec![1; pubkey_len],
            owner: vec![2; owner_len],
            lamports: 0,
            write_version: 0,
            data: vec![],
        });
        let mut out = VecDeque::new();
        let error = err_of(convert_message(message, None, &mut out))?;
        assert_eq!(error, protocol(), "{pubkey_len}/{owner_len}");
        assert!(out.is_empty());
    }
    Ok(())
}

#[test]
fn slots_pongs_and_unknown_updates_are_cheap_signals() -> TestResult {
    assert_eq!(
        convert(WireMessage::Slot(7), None)?,
        (vec![StreamUpdate::Slot(7)], Followup::None)
    );
    for message in [WireMessage::Pong, WireMessage::Unsupported] {
        assert_eq!(
            convert(message, None)?,
            (vec![StreamUpdate::Heartbeat], Followup::None)
        );
    }
    Ok(())
}

#[test]
fn a_server_ping_is_a_heartbeat_that_must_be_answered() -> TestResult {
    assert_eq!(
        convert(WireMessage::Ping, None)?,
        (vec![StreamUpdate::Heartbeat], Followup::AnswerPing)
    );
    Ok(())
}

#[test]
fn a_transaction_yields_one_update_per_matching_instruction_in_order() -> TestResult {
    let program = key(0x77);
    let other = key(0x55);
    let tx = tx_message(
        50,
        &[key(1), program, other],
        &[(1, &[0xA1]), (2, &[0xB2]), (1, &[0xC3, 0xC4])],
    );
    let (updates, _) = convert(WireMessage::Transaction(tx), Some(program))?;
    let datas: Vec<&[u8]> = updates
        .iter()
        .filter_map(|update| match update {
            StreamUpdate::Instruction(i) => Some(i.data.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(datas, vec![&[0xA1][..], &[0xC3, 0xC4][..]]);
    for update in &updates {
        let StreamUpdate::Instruction(instruction) = update else {
            return Err("expected instructions only".into());
        };
        assert_eq!((instruction.slot, instruction.program_id), (50, program));
    }
    Ok(())
}

#[test]
fn without_a_program_filter_every_instruction_is_kept() -> TestResult {
    let tx = tx_message(1, &[key(1), key(2)], &[(0, &[1]), (1, &[2])]);
    let (updates, _) = convert(WireMessage::Transaction(tx), None)?;
    assert_eq!(updates.len(), 2);
    Ok(())
}

#[test]
fn inner_instructions_follow_the_outer_ones() -> TestResult {
    let program = key(0x77);
    let mut tx = tx_message(1, &[key(1), program], &[(1, &[1])]);
    tx.inner_instructions = vec![
        WireInstruction {
            program_id_index: 1,
            data: vec![2],
        },
        WireInstruction {
            program_id_index: 0,
            data: vec![3],
        },
    ];
    let (updates, _) = convert(WireMessage::Transaction(tx), Some(program))?;
    let datas: Vec<u8> = updates
        .iter()
        .filter_map(|update| match update {
            StreamUpdate::Instruction(i) => i.data.first().copied(),
            _ => None,
        })
        .collect();
    assert_eq!(datas, vec![1, 2]);
    Ok(())
}

#[test]
fn lookup_table_addresses_extend_the_account_list_in_runtime_order() -> TestResult {
    let program = key(0x77);
    let mut tx = tx_message(1, &[key(1), key(2)], &[]);
    tx.loaded_writable = vec![key(3).to_bytes().to_vec()];
    tx.loaded_readonly = vec![key(4).to_bytes().to_vec(), program.to_bytes().to_vec()];
    // Indices: 0,1 static; 2 writable; 3,4 readonly. The program is index 4.
    tx.instructions = vec![
        WireInstruction {
            program_id_index: 4,
            data: vec![9],
        },
        WireInstruction {
            program_id_index: 2,
            data: vec![8],
        },
    ];
    let (updates, _) = convert(WireMessage::Transaction(tx.clone()), Some(program))?;
    assert_eq!(updates.len(), 1);
    let (all, _) = convert(WireMessage::Transaction(tx), None)?;
    let programs: Vec<Pubkey> = all
        .iter()
        .filter_map(|update| match update {
            StreamUpdate::Instruction(i) => Some(i.program_id),
            _ => None,
        })
        .collect();
    assert_eq!(programs, vec![program, key(3)]);
    Ok(())
}

#[test]
fn malformed_transactions_are_rejected_whole() -> TestResult {
    let program = key(0x77);
    // A program index outside the list.
    let out_of_range = tx_message(1, &[key(1), program], &[(1, &[1]), (2, &[2])]);
    // A key of the wrong length.
    let mut bad_key = tx_message(1, &[key(1), program], &[(1, &[1]), (0, &[2])]);
    bad_key.account_keys[0] = vec![0; 5];
    // Far more instructions than any transaction can hold.
    let many = tx_message(
        1,
        &[program],
        &vec![(0, &[0u8][..]); MAX_INSTRUCTIONS_PER_TRANSACTION + 1],
    );
    for tx in [out_of_range, bad_key, many] {
        let mut out = VecDeque::new();
        let error = err_of(convert_message(
            WireMessage::Transaction(tx),
            Some(program),
            &mut out,
        ))?;
        assert_eq!(error, protocol());
        assert!(out.is_empty(), "a partial transaction was appended");
    }
    Ok(())
}

#[test]
fn the_instruction_cap_is_inclusive() -> TestResult {
    let program = key(0x77);
    let tx = tx_message(
        1,
        &[program],
        &vec![(0, &[0u8][..]); MAX_INSTRUCTIONS_PER_TRANSACTION],
    );
    let (updates, _) = convert(WireMessage::Transaction(tx), Some(program))?;
    assert_eq!(updates.len(), MAX_INSTRUCTIONS_PER_TRANSACTION);
    Ok(())
}

// =====================================================================================================
// The stream
// =====================================================================================================

async fn open_stream(
    steps: Vec<Step>,
    program: Option<Pubkey>,
) -> Result<(YellowstoneStream<ScriptedSession>, Arc<LinkLog>), Box<dyn Error>> {
    let (mut link, log) = ScriptedLink::new(vec![Connection::Serve(steps)]);
    let session = link
        .open(
            &settings()?,
            &build_wire_request(&request_for(key(1)), false),
        )
        .await?;
    Ok((YellowstoneStream::new(session, program), log))
}

#[tokio::test]
async fn updates_come_out_in_arrival_order() -> TestResult {
    let program = key(0x77);
    let tx = tx_message(3, &[program], &[(0, &[1]), (0, &[2])]);
    let (mut stream, _) = open_stream(
        vec![
            Step::Msg(WireMessage::Slot(1)),
            Step::Msg(WireMessage::Transaction(tx)),
            Step::Msg(account_message(4, key(9), vec![7])),
            Step::Close,
        ],
        Some(program),
    )
    .await?;
    let mut seen = Vec::new();
    while let Some(update) = stream.next_update().await? {
        seen.push(update);
    }
    assert!(matches!(seen.first(), Some(StreamUpdate::Slot(1))));
    assert!(matches!(seen.get(1), Some(StreamUpdate::Instruction(i)) if i.data.as_ref() == [1]));
    assert!(matches!(seen.get(2), Some(StreamUpdate::Instruction(i)) if i.data.as_ref() == [2]));
    assert!(matches!(seen.get(3), Some(StreamUpdate::Account(a)) if a.slot == 4));
    assert_eq!(seen.len(), 4);
    Ok(())
}

#[tokio::test]
async fn a_transaction_without_matching_instructions_yields_nothing() -> TestResult {
    let tx = tx_message(3, &[key(0x55)], &[(0, &[1])]);
    let (mut stream, _) = open_stream(
        vec![
            Step::Msg(WireMessage::Transaction(tx)),
            Step::Msg(WireMessage::Slot(9)),
            Step::Close,
        ],
        Some(key(0x77)),
    )
    .await?;
    assert_eq!(stream.next_update().await?, Some(StreamUpdate::Slot(9)));
    assert_eq!(stream.next_update().await?, None);
    Ok(())
}

#[tokio::test]
async fn a_server_ping_is_answered_exactly_once_before_the_heartbeat() -> TestResult {
    let (mut stream, log) = open_stream(
        vec![
            Step::Msg(WireMessage::Ping),
            Step::Msg(WireMessage::Slot(2)),
        ],
        None,
    )
    .await?;
    assert_eq!(stream.next_update().await?, Some(StreamUpdate::Heartbeat));
    assert_eq!(log.pings_answered.load(Ordering::SeqCst), 1);
    assert_eq!(stream.next_update().await?, Some(StreamUpdate::Slot(2)));
    assert_eq!(log.pings_answered.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn an_interrupted_ping_answer_is_not_lost() -> TestResult {
    let (mut link, log) = ScriptedLink::new(vec![Connection::Serve(vec![
        Step::Msg(WireMessage::Ping),
        Step::Msg(WireMessage::Slot(2)),
    ])]);
    link.stall_first_ping = true;
    let session = link
        .open(
            &settings()?,
            &build_wire_request(&request_for(key(1)), false),
        )
        .await?;
    let mut stream = YellowstoneStream::new(session, None);

    // The watchdog drops the call while the answer is in flight...
    let cancelled = tokio::time::timeout(Duration::from_secs(1), stream.next_update()).await;
    assert!(cancelled.is_err());
    assert_eq!(log.pings_answered.load(Ordering::SeqCst), 0);
    // ...and the next call answers it first, then delivers the queued heartbeat.
    assert_eq!(stream.next_update().await?, Some(StreamUpdate::Heartbeat));
    assert_eq!(log.pings_answered.load(Ordering::SeqCst), 1);
    assert_eq!(stream.next_update().await?, Some(StreamUpdate::Slot(2)));
    Ok(())
}

#[tokio::test]
async fn a_failed_ping_answer_ends_the_stream_with_the_classified_error() -> TestResult {
    let (mut link, _log) =
        ScriptedLink::new(vec![Connection::Serve(vec![Step::Msg(WireMessage::Ping)])]);
    link.fail_ping = true;
    let session = link
        .open(
            &settings()?,
            &build_wire_request(&request_for(key(1)), false),
        )
        .await?;
    let mut stream = YellowstoneStream::new(session, None);
    assert_eq!(err_of(stream.next_update().await)?, grpc_failure(14));
    Ok(())
}

#[tokio::test]
async fn server_close_and_stream_errors_pass_through() -> TestResult {
    let (mut closed, _) = open_stream(vec![Step::Close], None).await?;
    assert_eq!(closed.next_update().await?, None);

    let (mut failing, _) = open_stream(vec![Step::Fail(grpc_failure(16))], None).await?;
    assert_eq!(err_of(failing.next_update().await)?, grpc_failure(16));

    let bad = WireMessage::Account(WireAccount {
        slot: 1,
        pubkey: vec![1],
        owner: vec![2],
        lamports: 0,
        write_version: 0,
        data: vec![],
    });
    let (mut malformed, _) = open_stream(vec![Step::Msg(bad)], None).await?;
    assert_eq!(err_of(malformed.next_update().await)?, protocol());
    Ok(())
}

// =====================================================================================================
// The transport
// =====================================================================================================

#[tokio::test]
async fn connect_sends_the_wire_request_and_keeps_the_endpoint_out_of_errors() -> TestResult {
    let (link, log) = ScriptedLink::new(vec![
        Connection::Refuse(MonitorError::Connection {
            kind: StreamErrorKind::Unreachable,
        }),
        Connection::Serve(vec![Step::Close]),
    ]);
    let mut transport = YellowstoneTransport::new(link, settings()?);
    let request = request_for(key(1));

    let error = match transport.connect(&request).await {
        Err(error) => error,
        Ok(_) => return Err("the first connection should have been refused".into()),
    };
    assert!(error.is_retryable());
    assert!(!error.to_string().contains("grpc.example.com"));

    let mut stream = transport.connect(&request).await?;
    assert_eq!(stream.next_update().await?, None);

    let requests = log.requests.lock().map_err(|_| "poisoned")?;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests.first(), Some(&build_wire_request(&request, false)));
    let origins = log.origins.lock().map_err(|_| "poisoned")?;
    assert_eq!(
        origins.first().map(String::as_str),
        Some("https://grpc.example.com:443")
    );
    Ok(())
}

#[tokio::test]
async fn the_program_wide_transport_subscribes_to_the_program() -> TestResult {
    let (link, log) = ScriptedLink::new(vec![Connection::Serve(vec![Step::Close])]);
    let mut transport =
        YellowstoneTransport::new(link, settings()?).with_program_wide_transactions(true);
    let request = request_for(key(1));
    let _stream = transport.connect(&request).await?;
    let requests = log.requests.lock().map_err(|_| "poisoned")?;
    assert_eq!(requests.first(), Some(&build_wire_request(&request, true)));
    assert_eq!(
        transport.settings().origin(),
        "https://grpc.example.com:443"
    );
    Ok(())
}

// =====================================================================================================
// The adapter under the real supervisor (reconnection, backoff, propagation)
// =====================================================================================================

struct Running {
    events: mpsc::Receiver<StreamUpdate>,
    health: watch::Receiver<HealthStatus>,
    trigger: krypton::monitor::ShutdownTrigger,
    task: tokio::task::JoinHandle<Result<HealthStatus, MonitorError>>,
}

fn run_monitor(
    link: ScriptedLink,
    monitor_settings: MonitorSettings,
) -> Result<Running, Box<dyn Error>> {
    let transport = YellowstoneTransport::new(link, settings()?);
    let monitor =
        GeyserMonitor::new(transport, request_for(key(1)), monitor_settings)?.with_jitter_seed(7);
    let (events_tx, events) = mpsc::channel(64);
    let (health_tx, health) = watch::channel(HealthStatus::default());
    let (trigger, shutdown) = shutdown_channel();
    let task = tokio::spawn(monitor.run(events_tx, health_tx, shutdown));
    Ok(Running {
        events,
        health,
        trigger,
        task,
    })
}

async fn next_event(running: &mut Running) -> Result<StreamUpdate, Box<dyn Error>> {
    let event = tokio::time::timeout(Duration::from_secs(60), running.events.recv())
        .await
        .map_err(|_| "no event within 60 virtual seconds")?;
    event.ok_or_else(|| "the event channel closed".into())
}

#[tokio::test(start_paused = true)]
async fn the_stream_survives_refusals_drops_silence_and_garbage() -> TestResult {
    let program = key(0x77);
    let tx = tx_message(21, &[program], &[(0, &[0xEE])]);
    let bad = WireMessage::Account(WireAccount {
        slot: 1,
        pubkey: vec![1],
        owner: vec![2],
        lamports: 0,
        write_version: 0,
        data: vec![],
    });
    let (link, log) = ScriptedLink::new(vec![
        // 1. the endpoint is down,
        Connection::Refuse(MonitorError::Connection {
            kind: StreamErrorKind::Unreachable,
        }),
        // 2. it serves, then sheds load,
        Connection::Serve(vec![
            Step::Msg(WireMessage::Slot(10)),
            Step::Msg(account_message(11, key(1), vec![1, 1])),
            Step::Fail(grpc_failure(14)),
        ]),
        // 3. it serves a transaction, a ping, then goes silent,
        Connection::Serve(vec![
            Step::Msg(WireMessage::Transaction(tx)),
            Step::Msg(WireMessage::Ping),
            Step::Hang,
        ]),
        // 4. it sends garbage,
        Connection::Serve(vec![Step::Msg(bad)]),
        // 5. and finally behaves.
        Connection::Serve(vec![
            Step::Msg(account_message(30, key(1), vec![3])),
            Step::Hang,
        ]),
    ]);
    let mut running = run_monitor(link, MonitorSettings::default())?;

    assert_eq!(next_event(&mut running).await?, StreamUpdate::Slot(10));
    assert!(matches!(next_event(&mut running).await?, StreamUpdate::Account(a) if a.slot == 11));
    assert!(
        matches!(next_event(&mut running).await?, StreamUpdate::Instruction(i) if i.data.as_ref() == [0xEE])
    );
    assert!(matches!(next_event(&mut running).await?, StreamUpdate::Account(a) if a.slot == 30));
    assert_eq!(log.pings_answered.load(Ordering::SeqCst), 1);

    let health = *running.health.borrow();
    assert_eq!(health.state, ConnectionState::Connected);
    assert_eq!(
        health.reconnects, 3,
        "connections 3, 4 and 5 were reconnects"
    );
    assert_eq!(health.events_forwarded, 4);
    assert_eq!(health.last_slot, 30);

    running.trigger.trigger();
    let final_health = running.task.await??;
    assert_eq!(final_health.state, ConnectionState::Stopped);
    assert_eq!(log.requests.lock().map_err(|_| "poisoned")?.len(), 5);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn reconnection_backs_off_exponentially() -> TestResult {
    let refuse = || {
        Connection::Refuse(MonitorError::Connection {
            kind: StreamErrorKind::Unreachable,
        })
    };
    let (link, log) = ScriptedLink::new(vec![
        refuse(),
        refuse(),
        refuse(),
        refuse(),
        Connection::Serve(vec![Step::Msg(WireMessage::Slot(1)), Step::Hang]),
    ]);
    let mut running = run_monitor(link, MonitorSettings::default())?;
    assert_eq!(next_event(&mut running).await?, StreamUpdate::Slot(1));

    let times = log.opened_at.lock().map_err(|_| "poisoned")?.clone();
    assert_eq!(times.len(), 5);
    let gaps: Vec<Duration> = times
        .windows(2)
        .map(|pair| pair[1].saturating_duration_since(pair[0]))
        .collect();
    // Defaults: 50 ms doubling, 20 % downward jitter -> [40,50], [80,100], [160,200], [320,400].
    let bounds = [(40, 50), (80, 100), (160, 200), (320, 400)];
    for (gap, (low, high)) in gaps.iter().zip(bounds) {
        let millis = gap.as_millis();
        assert!(
            (low..=high).contains(&millis),
            "gap {millis} ms not in {low}..={high}"
        );
    }
    running.trigger.trigger();
    running.task.await??;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn rejected_credentials_are_not_retried() -> TestResult {
    for code in [16, 7, 3, 12] {
        let (link, log) = ScriptedLink::new(vec![Connection::Refuse(grpc_failure(code))]);
        let running = run_monitor(link, MonitorSettings::default())?;
        let result = running.task.await?;
        assert_eq!(result.err(), Some(grpc_failure(code)), "code {code}");
        assert_eq!(log.requests.lock().map_err(|_| "poisoned")?.len(), 1);
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn credentials_rejected_mid_stream_stop_the_monitor_too() -> TestResult {
    let (link, log) = ScriptedLink::new(vec![Connection::Serve(vec![
        Step::Msg(WireMessage::Slot(1)),
        Step::Fail(grpc_failure(16)),
    ])]);
    let mut running = run_monitor(link, MonitorSettings::default())?;
    assert_eq!(next_event(&mut running).await?, StreamUpdate::Slot(1));
    assert_eq!(running.task.await?.err(), Some(grpc_failure(16)));
    assert_eq!(log.requests.lock().map_err(|_| "poisoned")?.len(), 1);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_dead_endpoint_exhausts_the_retry_budget() -> TestResult {
    let script = (0..10)
        .map(|_| {
            Connection::Refuse(MonitorError::Connection {
                kind: StreamErrorKind::Unreachable,
            })
        })
        .collect();
    let (link, log) = ScriptedLink::new(script);
    let settings = MonitorSettings {
        max_consecutive_failures: Some(3),
        ..MonitorSettings::default()
    };
    let running = run_monitor(link, settings)?;
    let result = running.task.await?;
    assert_eq!(
        result.err(),
        Some(MonitorError::ReconnectExhausted { attempts: 4 })
    );
    assert_eq!(log.requests.lock().map_err(|_| "poisoned")?.len(), 4);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_clean_shutdown_stops_a_silent_stream() -> TestResult {
    let (link, _log) = ScriptedLink::new(vec![Connection::Serve(vec![Step::Hang])]);
    let running = run_monitor(link, MonitorSettings::default())?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    running.trigger.trigger();
    let health = running.task.await??;
    assert_eq!(health.state, ConnectionState::Stopped);
    Ok(())
}

// =====================================================================================================
// The adapter under run_exit
// =====================================================================================================

#[tokio::test(start_paused = true)]
async fn the_yellowstone_transport_drives_a_live_exit() -> TestResult {
    let f = fixture()?;
    let engine = Arc::new(FakeEngine::landing());
    let executor = executor(
        Arc::clone(&engine),
        &f.payer,
        &armed(2_000_000_000, 5_000_000_000)?,
    )?;

    // The market after our 1 % buy, delivered the way Yellowstone would deliver it.
    let curve = CurveParams::default();
    let buy = BuyPlan::from_supply_bps(&curve, 100, DEFAULT_TOTAL_FEE_BPS, 500)?;
    let mut market = MarketState::new(curve, DEFAULT_TOTAL_FEE_BPS);
    market.apply_creator_buy(&buy)?;
    let curve_account = f.builder.addresses().bonding_curve;
    let StreamUpdate::Account(update) = market.account_update(curve_account) else {
        return Err("the market must produce an account update".into());
    };
    let wire = WireMessage::Account(WireAccount {
        slot: update.slot,
        pubkey: update.pubkey.to_bytes().to_vec(),
        owner: update.owner.to_bytes().to_vec(),
        lamports: update.lamports,
        write_version: update.write_version,
        data: update.data.to_vec(),
    });

    // The first connection is shed, the second one delivers the curve.
    let (link, log) = ScriptedLink::new(vec![
        Connection::Refuse(grpc_failure(14)),
        Connection::Serve(vec![
            Step::Msg(WireMessage::Slot(update.slot)),
            Step::Msg(wire),
            Step::Hang,
        ]),
    ]);
    let transport = YellowstoneTransport::new(link, settings()?);

    let exit_engine = ExitEngine::new(
        ExitPlan::evenly_spaced(2, 10_000, None)?,
        ExitSettings {
            fee_bps: DEFAULT_TOTAL_FEE_BPS,
            slippage_bps: 500,
            order_cost_lamports: 400_000,
            max_total_cost_lamports: 4_000_000,
        },
        Position {
            tokens: buy.token_amount,
            entry: PriceReference::after_buy(&curve, &buy)?,
        },
    )?;
    let (_hash_tx, blockhash) = blockhash_channel(Hash::new_from_array([2; 32]));
    let (_trigger, shutdown) = shutdown_channel();

    let report = executor
        .run_exit(
            transport,
            ExitRun {
                engine: exit_engine,
                curve_account,
                genesis: tokio::time::Instant::now(),
                builder: sell_builder(&f.payer)?,
                blockhash,
                monitor: MonitorSettings::default(),
                drain_timeout: Duration::from_secs(5),
            },
            shutdown,
        )
        .await?;

    assert_eq!(report.summary.position.orders_issued, 2);
    assert_eq!(report.summary.position.tokens_sold, buy.token_amount);
    assert_eq!(report.summary.position.tokens_unsold, 0);
    assert_eq!(engine.sent(), 2, "one bundle per tranche");
    assert!(executor.guards().snapshot()?.returned_lamports > 0);

    let requests = log.requests.lock().map_err(|_| "poisoned")?;
    // The refused connection was retried; later connections (the stream goes quiet
    // once the curve was delivered) are the supervisor's idle-watchdog reconnects.
    assert!(
        requests.len() >= 2,
        "the refused connection was not retried"
    );
    let first = requests.first().ok_or("no request recorded")?;
    assert!(first.accounts.contains(&curve_account.to_string()));
    Ok(())
}

// =====================================================================================================
// Tip feed
// =====================================================================================================

#[test]
fn sol_amounts_convert_to_lamports() -> TestResult {
    assert_eq!(sol_to_lamports(0.0)?, 0);
    assert_eq!(sol_to_lamports(0.000_001)?, 1_000);
    assert_eq!(sol_to_lamports(0.000_01)?, 10_000);
    assert_eq!(sol_to_lamports(0.001)?, 1_000_000);
    assert_eq!(sol_to_lamports(1.0)?, MAX_PLAUSIBLE_TIP_LAMPORTS);
    // Rounding to the nearest lamport, not truncation.
    assert_eq!(sol_to_lamports(0.000_000_001_4)?, 1);
    assert_eq!(sol_to_lamports(0.000_000_001_6)?, 2);
    Ok(())
}

#[test]
fn nonsense_amounts_are_rejected() {
    for value in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        -0.000_001,
        -1.0,
        1.000_000_001,
        5.0,
        1e300,
    ] {
        assert!(
            matches!(sol_to_lamports(value), Err(TipFeedError::Malformed { .. })),
            "{value}"
        );
    }
}

fn sol_floor() -> SolTipFloor {
    SolTipFloor {
        p50: 0.000_02,
        p75: 0.000_05,
        p95: 0.0002,
        p99: 0.0009,
        ema_p50: 0.000_04,
    }
}

#[test]
fn a_published_floor_converts_to_the_tip_book_shape() -> TestResult {
    assert_eq!(
        floor_from_sol(&sol_floor())?,
        TipFloor {
            p50: 20_000,
            p75: 50_000,
            p95: 200_000,
            p99: 900_000,
            ema_p50: 40_000,
        }
    );
    Ok(())
}

#[test]
fn unordered_percentiles_are_rejected() {
    for broken in [
        SolTipFloor {
            p75: 0.000_01,
            ..sol_floor()
        },
        SolTipFloor {
            p95: 0.000_04,
            ..sol_floor()
        },
        SolTipFloor {
            p99: 0.000_1,
            ..sol_floor()
        },
        SolTipFloor {
            p50: f64::NAN,
            ..sol_floor()
        },
        SolTipFloor {
            ema_p50: -1.0,
            ..sol_floor()
        },
    ] {
        assert!(matches!(
            floor_from_sol(&broken),
            Err(TipFeedError::Malformed { .. })
        ));
    }
    // The EMA is independent of the ordering.
    assert!(floor_from_sol(&SolTipFloor {
        ema_p50: 0.9,
        ..sol_floor()
    })
    .is_ok());
}

#[test]
fn feed_settings_are_validated() -> TestResult {
    TipFeedSettings::default().validate()?;
    let base = TipFeedSettings::default();
    for broken in [
        TipFeedSettings {
            interval: Duration::from_millis(999),
            ..base
        },
        TipFeedSettings {
            request_timeout: Duration::ZERO,
            ..base
        },
        TipFeedSettings {
            max_backoff: Duration::from_secs(9),
            ..base
        },
    ] {
        assert!(matches!(
            broken.validate(),
            Err(TipFeedError::InvalidConfig { .. })
        ));
    }
    assert!(TipFeedSettings {
        interval: Duration::from_secs(1),
        max_backoff: Duration::from_secs(1),
        ..base
    }
    .validate()
    .is_ok());
    Ok(())
}

#[test]
fn the_failure_delay_doubles_up_to_the_ceiling() {
    let settings = TipFeedSettings::default();
    let secs = |failures| settings.delay_after(failures).as_secs();
    assert_eq!(secs(0), 10);
    assert_eq!(secs(1), 20);
    assert_eq!(secs(2), 40);
    assert_eq!(secs(3), 60, "capped by max_backoff");
    assert_eq!(secs(10), 60);
    assert_eq!(secs(u32::MAX), 60, "no overflow");
}

enum FeedStep {
    Floor(TipFloor),
    Fail(TipFeedError),
    Hang,
}

struct ScriptedSource {
    steps: VecDeque<FeedStep>,
    fetched_at: Arc<Mutex<Vec<Instant>>>,
}

impl TipSource for ScriptedSource {
    async fn fetch(&mut self) -> Result<TipFloor, TipFeedError> {
        if let Ok(mut times) = self.fetched_at.lock() {
            times.push(Instant::now());
        }
        match self.steps.pop_front() {
            Some(FeedStep::Floor(floor)) => Ok(floor),
            Some(FeedStep::Fail(error)) => Err(error),
            // Out of script: keep answering with the same silence.
            Some(FeedStep::Hang) | None => {
                std::future::pending::<()>().await;
                Err(TipFeedError::Timeout)
            }
        }
    }
}

fn source(steps: Vec<FeedStep>) -> (ScriptedSource, Arc<Mutex<Vec<Instant>>>) {
    let fetched_at = Arc::new(Mutex::new(Vec::new()));
    (
        ScriptedSource {
            steps: steps.into(),
            fetched_at: Arc::clone(&fetched_at),
        },
        fetched_at,
    )
}

fn book() -> Result<Arc<TipBook>, Box<dyn Error>> {
    Ok(Arc::new(TipBook::new(TipPolicy::new(10_000, 1_000_000)?)))
}

fn market_floor() -> TipFloor {
    TipFloor {
        p50: 20_000,
        p75: 50_000,
        p95: 200_000,
        p99: 900_000,
        ema_p50: 40_000,
    }
}

async fn settle() {
    // Lets the feed task run until it is waiting on a timer (paused clock).
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn the_feed_moves_the_tip_from_the_fallback_to_the_market() -> TestResult {
    let book = book()?;
    assert_eq!(book.quote(Urgency::Launch, 0)?.basis, TipBasis::Fallback);

    let (source, fetched_at) = source(vec![FeedStep::Floor(market_floor())]);
    let (trigger, shutdown) = shutdown_channel();
    let task = tokio::spawn(run_tip_feed(
        source,
        Arc::clone(&book),
        TipFeedSettings::default(),
        shutdown,
    ));
    settle().await;

    let quote = book.quote(Urgency::Launch, 0)?;
    assert_ne!(quote.basis, TipBasis::Fallback);
    assert!(quote.lamports > 10_000);
    assert_eq!(
        fetched_at.lock().map_err(|_| "poisoned")?.len(),
        1,
        "the first fetch is immediate"
    );

    trigger.trigger();
    let stats = task.await??;
    assert_eq!((stats.updates, stats.failures), (1, 0));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn failures_back_off_keep_the_book_and_recover() -> TestResult {
    let book = book()?;
    let (source, fetched_at) = source(vec![
        FeedStep::Fail(TipFeedError::Unreachable),
        FeedStep::Fail(TipFeedError::HttpStatus { status: 503 }),
        FeedStep::Fail(TipFeedError::Malformed { reason: "x" }),
        FeedStep::Floor(market_floor()),
    ]);
    let (trigger, shutdown) = shutdown_channel();
    let task = tokio::spawn(run_tip_feed(
        source,
        Arc::clone(&book),
        TipFeedSettings::default(),
        shutdown,
    ));

    // After the three failures (waits of 20 s, 40 s, 60 s) the fourth fetch lands.
    tokio::time::sleep(Duration::from_secs(121)).await;
    settle().await;
    assert_ne!(book.quote(Urgency::Launch, 0)?.basis, TipBasis::Fallback);

    let times = fetched_at.lock().map_err(|_| "poisoned")?.clone();
    let gaps: Vec<u64> = times
        .windows(2)
        .map(|pair| pair[1].saturating_duration_since(pair[0]).as_secs())
        .collect();
    assert_eq!(gaps, vec![20, 40, 60]);

    trigger.trigger();
    let stats = task.await??;
    assert_eq!((stats.updates, stats.failures), (1, 3));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_silent_feed_is_a_timeout_not_a_hang() -> TestResult {
    let book = book()?;
    let (source, _) = source(vec![FeedStep::Hang, FeedStep::Floor(market_floor())]);
    let (trigger, shutdown) = shutdown_channel();
    let task = tokio::spawn(run_tip_feed(
        source,
        Arc::clone(&book),
        TipFeedSettings::default(),
        shutdown,
    ));
    // 3 s deadline, then the 20 s backoff, then the good answer.
    tokio::time::sleep(Duration::from_secs(24)).await;
    settle().await;
    assert_ne!(book.quote(Urgency::Launch, 0)?.basis, TipBasis::Fallback);
    trigger.trigger();
    let stats = task.await??;
    assert_eq!((stats.updates, stats.failures), (1, 1));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_failing_feed_never_updates_the_book() -> TestResult {
    let book = book()?;
    let (source, _) = source(vec![
        FeedStep::Fail(TipFeedError::Unreachable),
        FeedStep::Fail(TipFeedError::Unreachable),
    ]);
    let (trigger, shutdown) = shutdown_channel();
    let task = tokio::spawn(run_tip_feed(
        source,
        Arc::clone(&book),
        TipFeedSettings::default(),
        shutdown,
    ));
    tokio::time::sleep(Duration::from_secs(70)).await;
    assert_eq!(book.quote(Urgency::Launch, 0)?.basis, TipBasis::Fallback);
    trigger.trigger();
    let stats = task.await??;
    assert_eq!(stats.updates, 0);
    assert!(stats.failures >= 2);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn shutdown_interrupts_the_wait_and_the_fetch() -> TestResult {
    let (idle_source, _) = source(vec![FeedStep::Floor(market_floor())]);
    let (trigger, shutdown) = shutdown_channel();
    let task = tokio::spawn(run_tip_feed(
        idle_source,
        book()?,
        TipFeedSettings::default(),
        shutdown,
    ));
    settle().await;
    trigger.trigger();
    assert_eq!(
        task.await??.updates,
        1,
        "stopped while waiting for the next interval"
    );

    let (hanging, _) = source(vec![FeedStep::Hang]);
    let (trigger, shutdown) = shutdown_channel();
    let task = tokio::spawn(run_tip_feed(
        hanging,
        book()?,
        TipFeedSettings::default(),
        shutdown,
    ));
    settle().await;
    trigger.trigger();
    assert_eq!(task.await??.updates, 0, "stopped in the middle of a fetch");

    // Already triggered: nothing is fetched at all.
    let (never, fetched_at) = source(vec![FeedStep::Floor(market_floor())]);
    let (trigger, shutdown) = shutdown_channel();
    trigger.trigger();
    let stats = run_tip_feed(never, book()?, TipFeedSettings::default(), shutdown).await?;
    assert_eq!(stats.updates, 0);
    assert!(fetched_at.lock().map_err(|_| "poisoned")?.is_empty());
    Ok(())
}

#[tokio::test]
async fn invalid_feed_settings_fetch_nothing() -> TestResult {
    let (source, fetched_at) = source(vec![FeedStep::Floor(market_floor())]);
    let (_trigger, shutdown) = shutdown_channel();
    let settings = TipFeedSettings {
        interval: Duration::from_millis(10),
        ..TipFeedSettings::default()
    };
    let error = err_of(run_tip_feed(source, book()?, settings, shutdown).await)?;
    assert!(matches!(error, TipFeedError::InvalidConfig { .. }));
    assert!(fetched_at.lock().map_err(|_| "poisoned")?.is_empty());
    Ok(())
}

#[test]
fn feed_errors_have_value_free_labels() {
    for error in [
        TipFeedError::Unreachable,
        TipFeedError::Timeout,
        TipFeedError::HttpStatus { status: 503 },
        TipFeedError::ResponseTooLarge,
        TipFeedError::Malformed { reason: "r" },
        TipFeedError::BookUnavailable,
        TipFeedError::InvalidConfig { reason: "r" },
    ] {
        let label = error.label();
        assert!(!label.is_empty() && label.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
    }
}

#[tokio::test(start_paused = true)]
async fn a_fed_executor_launches_with_the_market_tip() -> TestResult {
    let f = fixture()?;
    let engine = Arc::new(FakeEngine::landing());
    let executor = executor(
        Arc::clone(&engine),
        &f.payer,
        &armed(2_000_000_000, 5_000_000_000)?,
    )?;
    let (src, _) = source(vec![FeedStep::Floor(market_floor())]);
    let (trigger, shutdown) = shutdown_channel();
    let feed = tokio::spawn(run_tip_feed(
        src,
        Arc::clone(executor.tips()),
        TipFeedSettings::default(),
        shutdown,
    ));
    settle().await;

    let receipt = executor
        .launch(&request(&f.builder, &f.mint, &f.table, 10_000_000_000))
        .await?;
    assert_ne!(receipt.tip_basis, TipBasis::Fallback);
    assert!(receipt.tip_lamports > 0 && receipt.tip_lamports <= TIP_CAP);

    trigger.trigger();
    feed.await??;
    Ok(())
}

// =====================================================================================================
// Source hygiene
// =====================================================================================================

#[test]
fn source_scan_phase8_modules_have_no_panics_or_unsafe_code() {
    const SOURCES: [(&str, &str); 2] = [
        (
            "yellowstone.rs",
            include_str!("../src/network/yellowstone.rs"),
        ),
        ("tip_feed.rs", include_str!("../src/execution/tip_feed.rs")),
    ];
    const FORBIDDEN: [&str; 7] = [
        ".unwrap()",
        ".expect(",
        "panic!",
        "todo!",
        "unimplemented!",
        "unsafe",
        "println!",
    ];
    for (name, source) in SOURCES {
        for word in FORBIDDEN {
            assert!(!source.contains(word), "{name} contains `{word}`");
        }
    }
}

#[test]
fn the_adapter_never_logs() {
    // The adapter handles the endpoint and the x-token: it emits no events at all.
    let source = include_str!("../src/network/yellowstone.rs");
    for macro_name in ["info!", "debug!", "warn!", "error!", "trace!", "tracing::"] {
        assert!(
            !source.contains(macro_name),
            "yellowstone.rs uses `{macro_name}`"
        );
    }
}
