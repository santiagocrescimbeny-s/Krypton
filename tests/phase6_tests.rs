//! Phase 6 test-suite: the dry-run simulation harness, its market model and the
//! latency metrics.
//!
//! Conventions
//! - Tests return `Result` and use `?` / `matches!`; no `unwrap()` / `expect()`.
//! - Every simulation runs on Tokio's paused clock (`start_paused = true`): the
//!   1 s to 10 s exit window, stream jitter, submission latency and the stream
//!   outage take microseconds of real time, and the schedule is exact.
//! - Randomness is seeded (`SplitMix64`): a scenario and a seed replay
//!   identically, which the determinism tests rely on.
//! - Latency metrics use `std::time::Instant` (real time), so tests assert
//!   counts and orderings, and only generous upper bounds on durations.
//! - Nothing here opens a socket: the harness has no network code, and
//!   `source_scan_*` keeps it that way.

use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use krypton::builder::constants::{MAX_TRANSACTION_SIZE, TOKEN_TOTAL_SUPPLY};
use krypton::builder::math::{
    BuyPlan, CurveParams, SellPlan, DEFAULT_TOTAL_FEE_BPS, MAX_SLIPPAGE_BPS,
};
use krypton::config::Config;
use krypton::errors::SimulationError;
use krypton::metrics::{
    duration_micros, micros_between, LatencyHistogram, LatencySnapshot, PipelineMetrics, Stopwatch,
    BUCKETS,
};
use krypton::monitor::curve::BondingCurveState;
use krypton::monitor::exit::SellOutcome;
use krypton::monitor::geyser::StreamUpdate;
use krypton::simulation::harness::{
    ensure_dry_run, OrderFate, Outage, ScenarioConfig, SimulationHarness, SimulationReport,
    StressProfile,
};
use krypton::simulation::market::{
    derive_seed_bytes, encode_curve_account, generate_flow, FlowProfile, MarketState,
    ScheduledTrade, SellExecution, SplitMix64, TradeFlow, BASE_SLOT, CURVE_ACCOUNT_BYTES,
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;

type TestResult = Result<(), Box<dyn Error>>;

// --- Helpers -------------------------------------------------------------------------------------

fn err_of<T, E>(result: Result<T, E>) -> Result<E, Box<dyn Error>> {
    match result {
        Ok(_) => Err("expected an error but the operation succeeded".into()),
        Err(err) => Ok(err),
    }
}

async fn simulate(scenario: ScenarioConfig) -> Result<SimulationReport, Box<dyn Error>> {
    Ok(SimulationHarness::new(scenario)?.run().await?)
}

fn curve_account() -> Pubkey {
    Pubkey::new_from_array([9; 32])
}

fn config_with_dry_run(value: Option<&str>) -> Result<Config, Box<dyn Error>> {
    let key_bytes = Keypair::new_from_array([7; 32]).to_bytes();
    let items: Vec<String> = key_bytes.iter().map(ToString::to_string).collect();
    let mut vars: HashMap<String, String> = [
        ("RPC_URL", "https://rpc.example.com"),
        ("JITO_ENGINE_URL", "https://jito.example.com"),
        ("CREATOR_SOL_AMOUNT", "0.5"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    vars.insert("PRIVATE_KEY".to_owned(), format!("[{}]", items.join(",")));
    if let Some(value) = value {
        vars.insert("DRY_RUN".to_owned(), value.to_owned());
    }
    Ok(Config::from_vars(vars)?)
}

/// A market right after the creator's atomic buy of `supply_bps`.
fn market_after_creator_buy(supply_bps: u16) -> Result<(MarketState, BuyPlan), Box<dyn Error>> {
    let curve = CurveParams::default();
    let buy = BuyPlan::from_supply_bps(&curve, supply_bps, DEFAULT_TOTAL_FEE_BPS, 0)?;
    let mut market = MarketState::new(curve, DEFAULT_TOTAL_FEE_BPS);
    market.apply_creator_buy(&buy)?;
    Ok((market, buy))
}

/// Price in lamports per base unit scaled by 1e12 (virtual SOL over virtual tokens).
fn price_e12(curve: &CurveParams) -> u128 {
    u128::from(curve.virtual_sol_reserves)
        .saturating_mul(1_000_000_000_000)
        .checked_div(u128::from(curve.virtual_token_reserves))
        .unwrap_or(0)
}

// =====================================================================================================
// Metrics: histogram
// =====================================================================================================

#[test]
fn empty_histogram_reports_zeros() {
    let snapshot = LatencyHistogram::new().snapshot();
    assert_eq!(snapshot, LatencySnapshot::default());
    assert_eq!(snapshot.count, 0);
    assert_eq!(snapshot.mean_us(), 0);
    assert_eq!(snapshot.p50_us(), 0);
    assert_eq!(snapshot.p99_us(), 0);
    assert_eq!(snapshot.min_us, 0);
    assert_eq!(snapshot.buckets.len(), BUCKETS);
}

#[test]
fn histogram_tracks_count_sum_min_max_and_mean() {
    let histogram = LatencyHistogram::new();
    for micros in [30, 10, 20] {
        histogram.record_micros(micros);
    }
    let snapshot = histogram.snapshot();
    assert_eq!(histogram.count(), 3);
    assert_eq!(snapshot.count, 3);
    assert_eq!(snapshot.sum_us, 60);
    assert_eq!(snapshot.min_us, 10);
    assert_eq!(snapshot.max_us, 30);
    assert_eq!(snapshot.mean_us(), 20);
    assert_eq!(snapshot.buckets.iter().sum::<u64>(), 3);
}

#[test]
fn percentiles_are_ordered_bounded_and_within_a_factor_of_two() {
    let histogram = LatencyHistogram::new();
    for micros in 1..=1_000u64 {
        histogram.record_micros(micros);
    }
    let snapshot = histogram.snapshot();
    let (p50, p95, p99) = (snapshot.p50_us(), snapshot.p95_us(), snapshot.p99_us());
    assert!(p50 <= p95 && p95 <= p99 && p99 <= snapshot.max_us);
    // The bucket holding rank 500 is 256..=511: its upper bound.
    assert_eq!(p50, 511);
    assert!((500..=1_000).contains(&p50));
    // The top buckets are capped at the largest sample.
    assert_eq!(p99, 1_000);
    assert_eq!(
        snapshot.percentile_us(0),
        snapshot.min_us,
        "rank 0 is the smallest sample"
    );
    assert!(snapshot.percentile_us(0) <= snapshot.percentile_us(1));
    // Values above 100 are clamped.
    assert_eq!(snapshot.percentile_us(200), snapshot.percentile_us(100));
    assert_eq!(snapshot.percentile_us(100), 1_000);
}

#[test]
fn a_single_sample_is_every_percentile() {
    let histogram = LatencyHistogram::new();
    histogram.record_micros(777);
    let snapshot = histogram.snapshot();
    assert_eq!(snapshot.p50_us(), 777);
    assert_eq!(snapshot.p95_us(), 777);
    assert_eq!(snapshot.p99_us(), 777);
    assert_eq!(snapshot.min_us, 777);
    assert_eq!(snapshot.max_us, 777);
}

#[test]
fn zero_sample_lands_in_the_first_bucket() {
    let histogram = LatencyHistogram::new();
    histogram.record_micros(0);
    let snapshot = histogram.snapshot();
    assert_eq!(snapshot.buckets.first().copied(), Some(1));
    assert_eq!(
        (snapshot.min_us, snapshot.max_us, snapshot.p99_us()),
        (0, 0, 0)
    );
}

#[test]
fn extreme_samples_saturate_instead_of_overflowing() {
    let histogram = LatencyHistogram::new();
    histogram.record_micros(u64::MAX);
    histogram.record_micros(u64::MAX);
    histogram.record_micros(5);
    let snapshot = histogram.snapshot();
    assert_eq!(snapshot.count, 3);
    assert_eq!(
        snapshot.sum_us,
        u64::MAX,
        "the sum saturates, it never wraps"
    );
    assert_eq!(snapshot.max_us, u64::MAX);
    assert_eq!(snapshot.min_us, 5);
    assert_eq!(snapshot.buckets.last().copied(), Some(2));
    assert_eq!(snapshot.p99_us(), u64::MAX);
}

#[test]
fn durations_convert_to_whole_microseconds() {
    let histogram = LatencyHistogram::new();
    histogram.record(Duration::from_micros(250));
    histogram.record(Duration::from_nanos(1_999));
    assert_eq!(histogram.snapshot().sum_us, 251);
    assert_eq!(duration_micros(Duration::MAX), u64::MAX);
    assert_eq!(duration_micros(Duration::from_millis(3)), 3_000);
}

#[test]
fn micros_between_never_goes_negative() {
    let earlier = Instant::now();
    thread::sleep(Duration::from_millis(2));
    let later = Instant::now();
    assert!(micros_between(earlier, later) >= 2_000);
    assert_eq!(micros_between(later, earlier), 0);
}

#[test]
fn stopwatch_records_what_it_returns() {
    let histogram = LatencyHistogram::new();
    let stopwatch = Stopwatch::start();
    thread::sleep(Duration::from_millis(1));
    let measured = stopwatch.stop(&histogram);
    assert!(measured >= 1_000);
    let snapshot = histogram.snapshot();
    assert_eq!((snapshot.count, snapshot.sum_us), (1, measured));
    assert!(stopwatch.elapsed_micros() >= measured);
}

// =====================================================================================================
// Metrics: pipeline
// =====================================================================================================

#[test]
fn reactive_order_is_timed_from_ingestion_to_assembly() {
    let metrics = PipelineMetrics::new();
    let ingested = Instant::now();
    metrics.count_observation();
    thread::sleep(Duration::from_millis(2));
    metrics.mark_order(3, Some(ingested));
    thread::sleep(Duration::from_millis(2));
    metrics.mark_assembled(3);

    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.observations, 1);
    assert_eq!(snapshot.orders, 1);
    assert_eq!(snapshot.reactive_orders, 1);
    assert_eq!(snapshot.assembled, 1);
    assert_eq!(snapshot.ingest_to_order.count, 1);
    assert_eq!(snapshot.order_to_assembled.count, 1);
    assert_eq!(snapshot.ingest_to_assembled.count, 1);
    assert!(snapshot.ingest_to_order.min_us >= 2_000);
    assert!(snapshot.order_to_assembled.min_us >= 2_000);
    assert!(
        snapshot.ingest_to_assembled.min_us >= 4_000,
        "the end-to-end delta covers both stages"
    );
    assert!(snapshot.ingest_to_assembled.max_us >= snapshot.ingest_to_order.max_us);
}

#[test]
fn latency_metrics_is_the_pipeline_tracker() {
    let metrics = krypton::metrics::LatencyMetrics::new();
    metrics.mark_order(1, Some(Instant::now()));
    metrics.mark_assembled(1);
    assert_eq!(metrics.snapshot().ingest_to_assembled.count, 1);
}

#[test]
fn timer_order_has_no_ingestion_latency() {
    let metrics = PipelineMetrics::new();
    metrics.mark_order(4, None);
    metrics.mark_assembled(4);
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.orders, 1);
    assert_eq!(snapshot.reactive_orders, 0);
    assert_eq!(snapshot.ingest_to_order.count, 0);
    assert_eq!(snapshot.ingest_to_assembled.count, 0);
    assert_eq!(snapshot.order_to_assembled.count, 1);
}

#[test]
fn assembly_without_an_issue_stamp_is_counted_but_not_timed() {
    let metrics = PipelineMetrics::new();
    metrics.mark_assembled(200);
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.assembled, 1);
    assert_eq!(snapshot.order_to_assembled.count, 0);
    assert_eq!(snapshot.ingest_to_assembled.count, 0);
}

#[test]
fn a_reused_sequence_does_not_inherit_a_stale_ingestion_stamp() {
    let metrics = PipelineMetrics::new();
    metrics.mark_order(5, Some(Instant::now()));
    metrics.mark_assembled(5);
    assert_eq!(metrics.snapshot().ingest_to_assembled.count, 1);

    metrics.mark_order(5, None);
    metrics.mark_assembled(5);
    let snapshot = metrics.snapshot();
    assert_eq!(
        snapshot.ingest_to_assembled.count, 1,
        "no second end-to-end sample"
    );
    assert_eq!(snapshot.order_to_assembled.count, 2);
}

#[test]
fn every_u8_sequence_is_tracked() {
    let metrics = PipelineMetrics::new();
    for sequence in 0..=u8::MAX {
        metrics.mark_order(sequence, Some(Instant::now()));
        metrics.mark_assembled(sequence);
    }
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.orders, 256);
    assert_eq!(snapshot.ingest_to_assembled.count, 256);
}

// =====================================================================================================
// Metrics: concurrent load
// =====================================================================================================

#[test]
fn histogram_is_exact_under_concurrent_recording() -> TestResult {
    const THREADS: u64 = 8;
    const PER_THREAD: u64 = 5_000;
    let histogram = LatencyHistogram::new();
    thread::scope(|scope| -> TestResult {
        let workers: Vec<_> = (0..THREADS)
            .map(|worker| {
                let histogram = &histogram;
                scope.spawn(move || {
                    for index in 0..PER_THREAD {
                        histogram.record_micros(worker * 1_000 + index % 97 + 1);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().map_err(|_| "a recording thread panicked")?;
        }
        Ok(())
    })?;

    let snapshot = histogram.snapshot();
    let expected_sum: u64 = (0..THREADS)
        .map(|worker| {
            (0..PER_THREAD)
                .map(|index| worker * 1_000 + index % 97 + 1)
                .sum::<u64>()
        })
        .sum();
    assert_eq!(snapshot.count, THREADS * PER_THREAD);
    assert_eq!(snapshot.sum_us, expected_sum);
    assert_eq!(snapshot.min_us, 1);
    assert_eq!(snapshot.max_us, (THREADS - 1) * 1_000 + 97);
    assert_eq!(snapshot.buckets.iter().sum::<u64>(), THREADS * PER_THREAD);
    assert!(snapshot.p50_us() <= snapshot.p99_us());
    Ok(())
}

#[test]
fn pipeline_metrics_are_exact_under_concurrent_orders() -> TestResult {
    const THREADS: u8 = 8;
    const PER_THREAD: u8 = 32; // 8 x 32 = 256 distinct sequences
    let metrics = PipelineMetrics::new();
    thread::scope(|scope| -> TestResult {
        let workers: Vec<_> = (0..THREADS)
            .map(|worker| {
                let metrics = &metrics;
                scope.spawn(move || {
                    for index in 0..PER_THREAD {
                        let sequence = worker * PER_THREAD + index;
                        metrics.count_observation();
                        metrics.mark_order(sequence, Some(Instant::now()));
                        metrics.mark_assembled(sequence);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().map_err(|_| "an order thread panicked")?;
        }
        Ok(())
    })?;

    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.observations, 256);
    assert_eq!(snapshot.orders, 256);
    assert_eq!(snapshot.reactive_orders, 256);
    assert_eq!(snapshot.assembled, 256);
    assert_eq!(snapshot.ingest_to_order.count, 256);
    assert_eq!(snapshot.order_to_assembled.count, 256);
    assert_eq!(snapshot.ingest_to_assembled.count, 256);
    assert!(snapshot.ingest_to_assembled.p50_us() <= snapshot.ingest_to_assembled.p99_us());
    Ok(())
}

// =====================================================================================================
// Market model
// =====================================================================================================

#[test]
fn splitmix_is_deterministic_and_stays_in_range() {
    let mut first = SplitMix64::new(42);
    let mut second = SplitMix64::new(42);
    let mut other = SplitMix64::new(43);
    let a: Vec<u64> = (0..8).map(|_| first.next_u64()).collect();
    let b: Vec<u64> = (0..8).map(|_| second.next_u64()).collect();
    let c: Vec<u64> = (0..8).map(|_| other.next_u64()).collect();
    assert_eq!(a, b);
    assert_ne!(a, c);

    let mut rng = SplitMix64::new(1);
    for _ in 0..1_000 {
        assert!(rng.below(10) < 10);
        assert!((5..=9).contains(&rng.range_inclusive(5, 9)));
    }
    assert_eq!(
        rng.below(0),
        0,
        "a zero bound is absorbed, not a division by zero"
    );
    assert_eq!(rng.range_inclusive(7, 7), 7);
    assert!(!rng.chance_bps(0));
    assert!(rng.chance_bps(10_000));
}

#[test]
fn splitmix_chance_tracks_its_probability() {
    let mut rng = SplitMix64::new(2024);
    let hits = (0..10_000).filter(|_| rng.chance_bps(2_500)).count();
    assert!(
        (2_000..3_000).contains(&hits),
        "got {hits} hits for a 25 % chance"
    );
}

#[test]
fn derived_seed_bytes_depend_on_seed_and_domain() {
    assert_eq!(derive_seed_bytes(5, 1), derive_seed_bytes(5, 1));
    assert_ne!(derive_seed_bytes(5, 1), derive_seed_bytes(5, 2));
    assert_ne!(derive_seed_bytes(5, 1), derive_seed_bytes(6, 1));
}

#[test]
fn trade_flow_is_reproducible_ordered_and_bounded() {
    let profile = StressProfile::volatile().flow;
    let first = generate_flow(11, &profile, 0, 10_000);
    let second = generate_flow(11, &profile, 0, 10_000);
    let other = generate_flow(12, &profile, 0, 10_000);
    assert_eq!(first, second);
    assert_ne!(first, other);
    assert!(
        (50..=250).contains(&first.len()),
        "12 trades/s over 10 s, got {}",
        first.len()
    );
    assert!(first.windows(2).all(|pair| pair[0].at_ms <= pair[1].at_ms));
    assert!(first
        .iter()
        .all(|trade| trade.at_ms > 0 && trade.at_ms <= 10_000));
    for trade in &first {
        match trade.flow {
            TradeFlow::Buy { lamports } => {
                assert!((profile.buy_lamports.0..=profile.buy_lamports.1).contains(&lamports));
            }
            TradeFlow::SellHoldings { bps } => {
                assert!((profile.sell_bps.0..=profile.sell_bps.1).contains(&bps));
            }
        }
    }
}

#[test]
fn silent_market_and_empty_windows_generate_nothing() {
    let silent = FlowProfile {
        trades_per_second: 0,
        ..StressProfile::calm().flow
    };
    assert!(generate_flow(1, &silent, 0, 10_000).is_empty());
    let busy = StressProfile::volatile().flow;
    assert!(generate_flow(1, &busy, 5_000, 5_000).is_empty());
}

#[test]
fn curve_account_round_trips_through_the_production_decoder() -> TestResult {
    let curve = CurveParams {
        virtual_token_reserves: 900_000_000_000_000,
        virtual_sol_reserves: 35_000_000_000,
        real_token_reserves: 600_000_000_000_000,
        token_total_supply: TOKEN_TOTAL_SUPPLY,
    };
    let data = encode_curve_account(&curve, 5_000_000_000, false);
    assert_eq!(data.len(), CURVE_ACCOUNT_BYTES);
    let state = BondingCurveState::decode(&data)?;
    assert_eq!(state.curve_params(), curve);
    assert_eq!(state.real_sol_reserves, 5_000_000_000);
    assert!(!state.complete);

    let complete = BondingCurveState::decode(&encode_curve_account(&curve, 0, true))?;
    assert!(complete.complete);
    Ok(())
}

#[test]
fn creator_buy_moves_the_curve_like_the_plan() -> TestResult {
    let (market, buy) = market_after_creator_buy(975)?;
    let start = CurveParams::default();
    assert_eq!(buy.token_amount, TOKEN_TOTAL_SUPPLY / 10_000 * 975);
    assert_eq!(
        market.curve().virtual_token_reserves,
        start.virtual_token_reserves - buy.token_amount
    );
    assert_eq!(
        market.curve().virtual_sol_reserves,
        start.virtual_sol_reserves + buy.net_cost_lamports
    );
    assert_eq!(market.real_sol_reserves(), buy.net_cost_lamports);
    assert!(!market.complete());
    assert_eq!(market.external_holdings(), 0);
    Ok(())
}

#[test]
fn account_updates_decode_and_slots_never_go_back() -> TestResult {
    let (mut market, _) = market_after_creator_buy(500)?;
    let mut last_slot = market.slot();
    assert!(last_slot >= BASE_SLOT);
    let mut last_write = 0;
    for step in 0..6u64 {
        if step % 2 == 0 {
            market.external_buy(300_000_000, step * 100)?;
        } else {
            market.external_sell(2_500, step * 100)?;
        }
        let update = market.account_update(curve_account());
        let StreamUpdate::Account(account) = update else {
            return Err("an account update was expected".into());
        };
        assert_eq!(account.pubkey, curve_account());
        assert!(account.slot > last_slot, "every change has its own slot");
        assert!(account.write_version > last_write);
        last_slot = account.slot;
        last_write = account.write_version;
        let state = BondingCurveState::decode(&account.data)?;
        assert_eq!(state.curve_params(), market.curve());
        assert_eq!(state.real_sol_reserves, market.real_sol_reserves());
    }
    Ok(())
}

#[test]
fn external_flow_moves_the_price_both_ways() -> TestResult {
    let (mut market, _) = market_after_creator_buy(500)?;
    let before = price_e12(&market.curve());

    assert!(market.external_buy(2_000_000_000, 100)?);
    let pumped = price_e12(&market.curve());
    assert!(pumped > before);
    assert!(market.external_holdings() > 0);

    assert!(market.external_sell(5_000, 200)?);
    let cooled = price_e12(&market.curve());
    assert!(cooled < pumped && cooled > before);

    assert!(market.external_sell(10_000, 300)?);
    assert_eq!(market.external_holdings(), 0);
    assert!(price_e12(&market.curve()) <= cooled);

    let ledger = market.ledger();
    assert_eq!((ledger.external_buys, ledger.external_sells), (1, 2));
    Ok(())
}

#[test]
fn nothing_to_sell_or_nothing_to_spend_leaves_the_curve_alone() -> TestResult {
    let (mut market, _) = market_after_creator_buy(500)?;
    let before = market.curve();
    assert!(
        !market.external_sell(5_000, 100)?,
        "no external holdings yet"
    );
    assert!(!market.external_buy(0, 100)?, "a zero budget buys nothing");
    assert_eq!(market.curve(), before);
    Ok(())
}

#[test]
fn multi_tranche_exit_math_matches_an_independent_ledger() -> TestResult {
    let (mut market, buy) = market_after_creator_buy(975)?;
    let tranche = buy.token_amount / 4;
    let mut shadow = market.curve();
    let mut previous_price_per_token = u128::MAX;
    let (mut net_total, mut gross_total) = (0u64, 0u64);

    for index in 0..4u64 {
        // Quote against the live curve with zero tolerance: nothing moves in between.
        let plan = SellPlan::from_token_amount(&market.curve(), tranche, DEFAULT_TOTAL_FEE_BPS, 0)?;
        let expected = SellPlan::from_token_amount(&shadow, tranche, DEFAULT_TOTAL_FEE_BPS, 0)?;
        assert_eq!(plan, expected, "the market and the shadow ledger agree");

        let execution = market.execute_sell(&plan, 1_000 + index * 3_000)?;
        let SellExecution::Landed { net_lamports, .. } = execution else {
            return Err(format!("tranche {index} was not executed: {execution:?}").into());
        };
        assert_eq!(net_lamports, plan.net_proceeds_lamports);
        shadow = plan.apply_to(&shadow)?;
        net_total += net_lamports;
        gross_total += plan.gross_proceeds_lamports;

        // Each tranche moves the price against the next one.
        let per_token = u128::from(net_lamports) * 1_000_000 / u128::from(tranche);
        assert!(
            per_token < previous_price_per_token,
            "tranche {index} did not suffer impact"
        );
        previous_price_per_token = per_token;
    }

    assert_eq!(market.curve(), shadow);
    let ledger = market.ledger();
    assert_eq!(ledger.our_sells_landed, 4);
    assert_eq!(ledger.our_sells_rejected, 0);
    assert_eq!(ledger.our_tokens_sold, tranche * 4);
    assert_eq!(ledger.our_lamports_received, net_total);

    // The constant-product curve is path independent: four tranches pay what one
    // block sale would, up to the floor of each division.
    let (single, _) = market_after_creator_buy(975)?;
    let one_shot =
        SellPlan::from_token_amount(&single.curve(), tranche * 4, DEFAULT_TOTAL_FEE_BPS, 0)?;
    assert!(gross_total.abs_diff(one_shot.gross_proceeds_lamports) <= 8);
    // Everything sold came back to the curve.
    assert_eq!(
        market.curve().virtual_token_reserves,
        single.curve().virtual_token_reserves + tranche * 4
    );
    Ok(())
}

#[test]
fn price_impact_between_quote_and_execution_trips_the_slippage_floor() -> TestResult {
    // A pump the exit saw, then a dump it did not.
    let (mut market, buy) = market_after_creator_buy(975)?;
    market.external_buy(1_000_000_000, 100)?;
    let tranche = buy.token_amount / 4;

    let strict = SellPlan::from_token_amount(&market.curve(), tranche, DEFAULT_TOTAL_FEE_BPS, 0)?;
    let tolerant = SellPlan::from_token_amount(
        &market.curve(),
        tranche,
        DEFAULT_TOTAL_FEE_BPS,
        MAX_SLIPPAGE_BPS,
    )?;
    market.external_sell(10_000, 200)?;
    let after_dump = market.curve();

    assert_eq!(
        market.execute_sell(&strict, 300)?,
        SellExecution::SlippageExceeded
    );
    assert_eq!(
        market.curve(),
        after_dump,
        "a rejected sell leaves no trace"
    );
    assert_eq!(market.ledger().our_sells_rejected, 1);
    assert_eq!(market.ledger().our_tokens_sold, 0);

    assert!(matches!(
        market.execute_sell(&tolerant, 400)?,
        SellExecution::Landed { .. }
    ));
    assert_eq!(market.ledger().our_sells_landed, 1);
    assert_eq!(market.ledger().our_tokens_sold, tranche);
    Ok(())
}

// =====================================================================================================
// Scenario validation
// =====================================================================================================

fn assert_invalid(scenario: &ScenarioConfig) -> TestResult {
    let error = err_of(scenario.validate())?;
    assert!(
        matches!(error, SimulationError::InvalidScenario { .. }),
        "{error:?}"
    );
    let error = err_of(SimulationHarness::new(scenario.clone()))?;
    assert!(
        matches!(error, SimulationError::InvalidScenario { .. }),
        "{error:?}"
    );
    Ok(())
}

#[test]
fn presets_are_valid() -> TestResult {
    for scenario in [
        ScenarioConfig::calm(1),
        ScenarioConfig::volatile(1),
        ScenarioConfig::hostile(1),
    ] {
        scenario.validate()?;
        SimulationHarness::new(scenario)?;
    }
    Ok(())
}

#[test]
fn inconsistent_scenarios_are_rejected_before_anything_runs() -> TestResult {
    let base = ScenarioConfig::calm(1);
    let mut cases = Vec::new();
    let mut case = |change: &dyn Fn(&mut ScenarioConfig)| {
        let mut scenario = base.clone();
        change(&mut scenario);
        cases.push(scenario);
    };
    case(&|s| s.exit_slippage_bps = MAX_SLIPPAGE_BPS + 1);
    case(&|s| s.launch_supply_bps = 0);
    case(&|s| s.launch_supply_bps = 10_001);
    case(&|s| s.horizon_ms = 0);
    case(&|s| s.horizon_ms = 120_001);
    case(&|s| s.heartbeat_ms = 0);
    case(&|s| s.heartbeat_ms = 2_000);
    case(&|s| s.max_exit_orders = 0);
    case(&|s| {
        s.stress.drop_bps = 6_000;
        s.stress.unknown_bps = 4_001;
    });
    case(&|s| s.stress.stream_jitter_ms = (0, 10_001));
    case(&|s| s.stress.submit_latency_ms = (0, 10_001));
    case(&|s| {
        s.stress.outage = Some(Outage {
            start_ms: 1_000,
            duration_ms: 0,
        });
    });
    case(&|s| {
        s.stress.outage = Some(Outage {
            start_ms: s.horizon_ms,
            duration_ms: 1,
        });
    });
    for scenario in &cases {
        assert_invalid(scenario)?;
    }
    assert_eq!(cases.len(), 13);
    Ok(())
}

// =====================================================================================================
// Safety boundary
// =====================================================================================================

#[test]
fn live_configuration_is_refused() -> TestResult {
    let live = config_with_dry_run(Some("false"))?;
    assert!(!live.dry_run);
    assert!(matches!(
        ensure_dry_run(&live),
        Err(SimulationError::LiveModeRefused)
    ));
    let error = err_of(SimulationHarness::from_config(
        &live,
        ScenarioConfig::calm(1),
    ))?;
    assert!(matches!(error, SimulationError::LiveModeRefused));
    assert!(error.to_string().contains("DRY_RUN"));
    Ok(())
}

#[test]
fn dry_run_configuration_is_accepted_and_is_the_default() -> TestResult {
    for value in [Some("true"), None] {
        let config = config_with_dry_run(value)?;
        assert!(config.dry_run);
        ensure_dry_run(&config)?;
        let harness = SimulationHarness::from_config(&config, ScenarioConfig::calm(3))?;
        assert_eq!(harness.scenario().seed, 3);
    }
    Ok(())
}

#[test]
fn source_scan_the_simulation_has_no_network_code() {
    const SOURCES: [(&str, &str); 2] = [
        ("harness.rs", include_str!("../src/simulation/harness.rs")),
        ("market.rs", include_str!("../src/simulation/market.rs")),
    ];
    const FORBIDDEN: [&str; 13] = [
        "reqwest",
        "JitoClient",
        "JitoSellSubmitter",
        "send_bundle",
        "TcpStream",
        "UdpSocket",
        "std::net",
        "tokio::net",
        "tonic",
        "hyper",
        "std::process",
        "Command::new",
        "unsafe",
    ];
    const PANICS: [&str; 5] = [".unwrap()", ".expect(", "panic!", "todo!", "unimplemented!"];
    for (name, source) in SOURCES {
        for word in FORBIDDEN {
            assert!(!source.contains(word), "{name} mentions `{word}`");
        }
        for word in PANICS {
            assert!(!source.contains(word), "{name} contains `{word}`");
        }
    }
}

// =====================================================================================================
// End to end: complete dry-run workflows
// =====================================================================================================

#[tokio::test(start_paused = true)]
async fn calm_market_completes_the_full_lifecycle() -> TestResult {
    let report = simulate(ScenarioConfig::calm(7)).await?;
    let expected_position =
        BuyPlan::from_supply_bps(&CurveParams::default(), 975, DEFAULT_TOTAL_FEE_BPS, 500)?
            .token_amount;

    // Genesis launch + atomic buy.
    assert_eq!(report.launch.position_tokens, expected_position);
    assert!(report.launch.wire_size_bytes > 0);
    assert!(report.launch.wire_size_bytes <= MAX_TRANSACTION_SIZE);

    // Multi-tranche exit: four tranches, all landed, the whole position sold.
    let position = report.exit.position;
    assert_eq!(position.orders_issued, 4);
    assert_eq!(position.tokens_sold, expected_position);
    assert_eq!(
        position.tokens_unsold + position.tokens_in_flight + position.tokens_unresolved,
        0
    );
    assert!(!position.stop_loss_fired);
    assert!(!report.exit.interrupted && !report.exit.unsettled);
    assert_eq!(report.exit.decode_errors, 0);
    assert!(report.exit.observations > 1, "the stream fed the engine");
    assert_eq!(report.orders.len(), 4);
    assert!(report
        .orders
        .iter()
        .all(|order| order.fate == OrderFate::Landed));
    assert!(report
        .orders
        .iter()
        .all(|order| matches!(order.reported, SellOutcome::Landed { .. })));
    assert!(report
        .orders
        .iter()
        .all(|order| order.received_lamports.unwrap_or(0) >= order.min_sol_output));

    // The market agrees with the books.
    assert!(report.accounting_consistent());
    assert_eq!(report.market.our_tokens_sold, expected_position);
    assert_eq!(report.market.our_sells_landed, 4);
    assert_eq!(report.market.our_sells_rejected, 0);
    let received: u64 = report
        .orders
        .iter()
        .filter_map(|order| order.received_lamports)
        .sum();
    assert_eq!(received, report.market.our_lamports_received);
    assert!(
        report.market.external_buys + report.market.external_sells > 0,
        "the market moved"
    );
    assert!(!report.curve_complete);

    // A calm network: one connection, nothing lost.
    assert_eq!(report.monitor.reconnects, 0);
    assert_eq!(report.stream_updates_dropped, 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn orders_are_issued_inside_the_exit_window_in_order() -> TestResult {
    let report = simulate(ScenarioConfig::calm(21)).await?;
    let sequences: Vec<u8> = report.orders.iter().map(|order| order.sequence).collect();
    assert_eq!(sequences, vec![0, 1, 2, 3]);
    // Landing slots are strictly increasing: the tranches are 3 s apart and every
    // execution advances the slot.
    let slots: Vec<u64> = report
        .orders
        .iter()
        .filter_map(|order| match order.reported {
            SellOutcome::Landed { slot } => Some(slot),
            _ => None,
        })
        .collect();
    assert_eq!(slots.len(), 4);
    assert!(slots.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(slots.iter().all(|slot| *slot > BASE_SLOT));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn nothing_leaves_the_process_and_no_tip_is_paid() -> TestResult {
    let scenario = ScenarioConfig::calm(5);
    let tip = scenario.tip_lamports;
    let report = simulate(scenario).await?;
    let boundary = report.boundary;
    assert_eq!(boundary.launch_transactions_assembled, 1);
    assert_eq!(
        boundary.sell_transactions_assembled,
        u64::from(report.exit.position.orders_issued)
    );
    assert_eq!(
        boundary.bundles_swallowed,
        boundary.sell_transactions_assembled
    );
    assert_eq!(
        boundary.virtual_tip_lamports,
        tip * boundary.sell_transactions_assembled,
        "tips are counted, never paid"
    );
    // The metrics saw every assembly too.
    assert_eq!(
        report.metrics.assembled,
        boundary.sell_transactions_assembled
    );
    assert_eq!(
        report.metrics.orders,
        u64::from(report.exit.position.orders_issued)
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn volatile_market_still_balances_the_books() -> TestResult {
    for seed in 0..6 {
        let report = simulate(ScenarioConfig::volatile(seed)).await?;
        assert!(report.accounting_consistent(), "seed {seed}");
        assert_eq!(
            report.orders.len(),
            report.exit.position.orders_issued as usize,
            "seed {seed}"
        );
        assert!(
            report.market.external_buys > 0 && report.market.external_sells > 0,
            "seed {seed}"
        );
        assert_eq!(report.monitor.reconnects, 0, "seed {seed}");
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn hostile_conditions_never_break_the_accounting() -> TestResult {
    let mut unknown = 0;
    let mut dropped = 0;
    let mut slipped = 0;
    let mut hidden_landings = 0;
    for seed in 0..16 {
        let report = simulate(ScenarioConfig::hostile(seed)).await?;
        let position = report.exit.position;
        assert!(report.accounting_consistent(), "seed {seed}");
        assert!(
            report.monitor.reconnects >= 1,
            "seed {seed}: the outage forces a reconnection"
        );
        assert!(!report.exit.unsettled, "seed {seed}");
        assert_eq!(
            report.orders.len(),
            position.orders_issued as usize,
            "seed {seed}"
        );

        for order in &report.orders {
            match order.fate {
                OrderFate::Dropped => {
                    dropped += 1;
                    assert_eq!(order.received_lamports, None);
                }
                OrderFate::SlippageRejected => {
                    slipped += 1;
                    assert_eq!(order.received_lamports, None);
                    assert!(matches!(
                        order.reported,
                        SellOutcome::Failed | SellOutcome::Unknown
                    ));
                }
                OrderFate::Landed => {
                    assert!(order.received_lamports.unwrap_or(0) >= order.min_sol_output);
                }
                OrderFate::CurveComplete => {}
            }
            if order.reported == SellOutcome::Unknown {
                unknown += 1;
            }
        }
        // An unknown outcome is never counted as sold, even when it did land.
        if report.market.our_tokens_sold > position.tokens_sold {
            hidden_landings += 1;
            assert!(position.tokens_unresolved > 0, "seed {seed}");
        }
        // Nothing was re-offered: no more tokens were sold than we ever held.
        assert!(
            report.market.our_tokens_sold <= report.launch.position_tokens,
            "seed {seed}"
        );
    }
    assert!(
        unknown > 0 && dropped > 0 && slipped > 0,
        "the preset stresses all three paths"
    );
    assert!(hidden_landings > 0, "some unknown bundles really landed");
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn stream_outage_is_survived_with_a_fresh_snapshot() -> TestResult {
    let mut scenario = ScenarioConfig::calm(13);
    scenario.stress.outage = Some(Outage {
        start_ms: 2_000,
        duration_ms: 1_500,
    });
    let report = simulate(scenario).await?;
    assert!(report.monitor.reconnects >= 1);
    assert!(report.accounting_consistent());
    // The exit completed even though the stream was down for part of the window.
    assert_eq!(
        report.exit.position.tokens_sold,
        report.launch.position_tokens
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn adverse_moves_between_quote_and_landing_cause_real_slippage_rejections() -> TestResult {
    // No tolerance and slow submissions in a busy market: some quotes go stale.
    let mut rejected = 0;
    for seed in 0..8 {
        let mut scenario = ScenarioConfig::volatile(seed);
        scenario.exit_slippage_bps = 0;
        scenario.stress.submit_latency_ms = (300, 900);
        let report = simulate(scenario).await?;
        assert!(report.accounting_consistent(), "seed {seed}");
        rejected += report.market.our_sells_rejected;
        for order in &report.orders {
            if order.fate == OrderFate::SlippageRejected {
                assert_eq!(order.reported, SellOutcome::Failed);
            }
        }
    }
    assert!(rejected > 0, "zero tolerance must lose some races");
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn whale_dump_shock_is_absorbed_by_the_exit_engine() -> TestResult {
    let mut scenario = ScenarioConfig::calm(31);
    scenario.stress.flow.buy_bias_bps = 9_500;
    scenario.stress.shocks = vec![ScheduledTrade {
        at_ms: 5_000,
        flow: TradeFlow::SellHoldings { bps: 10_000 },
    }];
    let report = simulate(scenario).await?;
    assert!(report.accounting_consistent());
    // The whale really sold: more external sells than the background alone gives.
    assert!(report.market.external_sells >= 1);
    // Whatever happened to the price, no order sold below its own floor.
    for order in &report.orders {
        if let Some(received) = order.received_lamports {
            assert!(received >= order.min_sol_output);
        }
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn stop_loss_reacts_to_updates_and_is_timed_end_to_end() -> TestResult {
    let mut fired = 0;
    for seed in 0..8 {
        let mut scenario = ScenarioConfig::calm(seed);
        // 0.5 %: our own first tranche already moves the price that far.
        scenario.stop_loss_bps = Some(50);
        let report = simulate(scenario).await?;
        assert!(report.accounting_consistent(), "seed {seed}");
        if report.exit.position.stop_loss_fired {
            fired += 1;
            assert_eq!(
                report.exit.position.tokens_sold, report.launch.position_tokens,
                "seed {seed}"
            );
            // A stop-loss is a reaction to a stream update: it has an end-to-end sample.
            assert!(report.metrics.reactive_orders >= 1, "seed {seed}");
            assert!(report.metrics.ingest_to_assembled.count >= 1, "seed {seed}");
            let latency = report.metrics.ingest_to_assembled;
            assert!(latency.min_us <= latency.p50_us() && latency.p50_us() <= latency.max_us);
            assert!(
                latency.max_us < 5_000_000,
                "assembly took {} us",
                latency.max_us
            );
        }
    }
    assert!(fired > 0, "the stop-loss must fire in at least one seed");
    Ok(())
}

// =====================================================================================================
// Determinism and concurrency
// =====================================================================================================

#[tokio::test(start_paused = true)]
async fn the_same_seed_replays_identically() -> TestResult {
    for scenario in [ScenarioConfig::volatile(99), ScenarioConfig::hostile(99)] {
        let first = simulate(scenario.clone()).await?;
        let second = simulate(scenario).await?;
        assert_eq!(first.fingerprint(), second.fingerprint());
        assert_eq!(first.orders, second.orders);
        assert_eq!(first.market, second.market);
        assert_eq!(first.final_curve, second.final_curve);
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn different_seeds_tell_different_stories() -> TestResult {
    let first = simulate(ScenarioConfig::volatile(1)).await?;
    let second = simulate(ScenarioConfig::volatile(2)).await?;
    assert_ne!(first.fingerprint(), second.fingerprint());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn concurrent_runs_share_metrics_without_losing_samples() -> TestResult {
    // Reference results, run one at a time with private metrics.
    let mut expected = Vec::new();
    for seed in 40..44 {
        expected.push(simulate(ScenarioConfig::hostile(seed)).await?.fingerprint());
    }

    // The same four runs at once, all recording into one set of histograms.
    let metrics = Arc::new(PipelineMetrics::new());
    let mut harnesses = Vec::new();
    for seed in 40..44 {
        harnesses.push(
            SimulationHarness::new(ScenarioConfig::hostile(seed))?
                .with_metrics(Arc::clone(&metrics)),
        );
    }
    let [a, b, c, d] = &harnesses[..] else {
        return Err("four harnesses were expected".into());
    };
    let (ra, rb, rc, rd) = tokio::join!(a.run(), b.run(), c.run(), d.run());
    let reports = [ra?, rb?, rc?, rd?];

    let fingerprints: Vec<u64> = reports.iter().map(SimulationReport::fingerprint).collect();
    assert_eq!(
        fingerprints, expected,
        "concurrency does not change any run"
    );

    let orders: u64 = reports
        .iter()
        .map(|r| u64::from(r.exit.position.orders_issued))
        .sum();
    let assembled: u64 = reports
        .iter()
        .map(|r| r.boundary.sell_transactions_assembled)
        .sum();
    let observations: u64 = reports.iter().map(|r| r.exit.observations).sum();
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.orders, orders);
    assert_eq!(snapshot.assembled, assembled);
    assert_eq!(snapshot.observations, observations);
    assert_eq!(
        snapshot.order_to_assembled.count,
        snapshot.order_to_assembled.buckets.iter().sum::<u64>()
    );
    assert!(snapshot.order_to_assembled.count <= assembled);
    assert!(snapshot.order_to_assembled.p50_us() <= snapshot.order_to_assembled.p99_us());
    assert!(reports.iter().all(SimulationReport::accounting_consistent));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn report_consistency_check_catches_tampering() -> TestResult {
    let report = simulate(ScenarioConfig::calm(8)).await?;
    assert!(report.accounting_consistent());

    let mut invented_sale = report.clone();
    invented_sale.exit.position.tokens_sold += 1;
    assert!(
        !invented_sale.accounting_consistent(),
        "buckets no longer add up"
    );

    let mut unreported_landing = report.clone();
    unreported_landing.market.our_tokens_sold = report.launch.position_tokens;
    unreported_landing.exit.position.tokens_sold = 0;
    unreported_landing.exit.position.tokens_unsold = report.launch.position_tokens;
    assert!(
        !unreported_landing.accounting_consistent(),
        "tokens that left the wallet must be accounted for"
    );

    let mut phantom_sale = report.clone();
    phantom_sale.market.our_tokens_sold = 0;
    assert!(
        !phantom_sale.accounting_consistent(),
        "the engine claims sales that never happened"
    );

    let mut other = report.clone();
    other.seed += 1;
    assert_ne!(other.fingerprint(), report.fingerprint());
    Ok(())
}

// =====================================================================================================
// Log hygiene
// =====================================================================================================

/// Records every event at every level, so the checks below hold even when
/// `RUST_LOG=trace` is set.
#[derive(Default)]
struct Captured {
    lines: Mutex<Vec<String>>,
}

thread_local! {
    /// The buffer of the test capturing on this thread, if any.
    static ACTIVE: RefCell<Option<Arc<Captured>>> = const { RefCell::new(None) };
}

/// Installed once as the global default rather than per test with
/// `set_default`: callsite interest is cached process-wide, and a callsite first
/// hit by another test thread while no scoped subscriber exists can be cached as
/// never enabled, silently dropping this test's events.
struct Capture;

/// Stops capturing on this thread when dropped.
struct CaptureGuard;

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        ACTIVE.with(|active| active.borrow_mut().take());
    }
}

struct Collect<'a>(&'a mut String);

impl tracing::field::Visit for Collect<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, " {}={:?}", field.name(), value);
    }
}

impl tracing::Subscriber for Capture {
    fn register_callsite(&self, _: &tracing::Metadata<'_>) -> tracing::subscriber::Interest {
        // Decided per event in `enabled`, never cached.
        tracing::subscriber::Interest::sometimes()
    }
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        ACTIVE.with(|active| active.borrow().is_some())
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = format!("{} {}", event.metadata().level(), event.metadata().target());
        event.record(&mut Collect(&mut line));
        ACTIVE.with(|active| {
            if let Some(captured) = active.borrow().as_ref() {
                if let Ok(mut lines) = captured.lines.lock() {
                    lines.push(line);
                }
            }
        });
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Captures events on this thread until the guard drops. The test runtime is
/// current-thread, so every task the test spawns logs here too.
fn capture() -> Result<(Arc<Captured>, CaptureGuard), Box<dyn Error>> {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    if !*INSTALLED.get_or_init(|| tracing::subscriber::set_global_default(Capture).is_ok()) {
        return Err("another global tracing subscriber is installed".into());
    }
    let captured = Arc::new(Captured::default());
    ACTIVE.with(|active| *active.borrow_mut() = Some(Arc::clone(&captured)));
    Ok((captured, CaptureGuard))
}

/// Words that look like a base58 public key or signature (32+ base58 characters),
/// or like the lower-case hex of one.
fn looks_like_key_material(line: &str) -> bool {
    line.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| {
            word.len() >= 32
                && (word.chars().all(|c| c.is_ascii_hexdigit())
                    || word
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() && !matches!(c, '0' | 'O' | 'I' | 'l')))
        })
}

#[tokio::test(start_paused = true)]
async fn simulation_logs_carry_no_keys_signatures_or_amounts() -> TestResult {
    let (captured, _guard) = capture()?;

    let mut scenario = ScenarioConfig::hostile(3);
    scenario.stop_loss_bps = Some(50);
    let report = simulate(scenario).await?;
    assert!(report.accounting_consistent());

    let lines = captured
        .lines
        .lock()
        .map_err(|_| "the capture buffer is poisoned")?
        .clone();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("simulation finished")),
        "the run summary is logged: {lines:?}"
    );
    assert!(lines
        .iter()
        .any(|line| line.contains("pipeline latency summary")));

    for line in &lines {
        assert!(!looks_like_key_material(line), "key-like text in: {line}");
        let lower = line.to_ascii_lowercase();
        for forbidden in [
            "lamports",
            "balance",
            "signature",
            "secret",
            "private",
            "keypair",
            "tip=",
        ] {
            assert!(!lower.contains(forbidden), "`{forbidden}` in: {line}");
        }
    }

    // Digits only appear as counts, sequences and microsecond percentiles: none of
    // the run's amounts (position, proceeds, reserves) is printed.
    let amounts = [
        report.launch.position_tokens,
        report.market.our_lamports_received,
        report.final_curve.virtual_sol_reserves,
        report.final_curve.virtual_token_reserves,
    ];
    for line in &lines {
        for amount in amounts {
            assert!(
                !line.contains(&amount.to_string()),
                "amount {amount} in: {line}"
            );
        }
    }
    Ok(())
}
