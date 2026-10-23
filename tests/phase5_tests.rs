//! Phase 5 test-suite: bonding-curve decoding, sell math and `sell_v3`
//! encoding, the Geyser stream supervisor, the micro-tranche exit engine and the
//! async exit manager.
//!
//! Conventions
//! - Tests return `Result` and use `?` / `matches!`; no `unwrap()` / `expect()`.
//! - Streams are mocked with scripted transports; time-dependent tests run on
//!   Tokio's paused clock (`start_paused = true`), so backoff delays, idle
//!   watchdogs and the 1 s to 10 s exit window are checked exactly and instantly.
//! - Sell vectors come from exact integer arithmetic in Python; the account
//!   layout of `BondingCurve` follows `idl/pump.json`.
//! - `Pubkey`s are plain fixed bytes: runs are reproducible.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use krypton::builder::constants::{
    DISCRIMINATORS_BUY, DISCRIMINATORS_SELL, DISCRIMINATOR_BONDING_CURVE_ACCOUNT,
    DISCRIMINATOR_BUY_V3, DISCRIMINATOR_CREATE, DISCRIMINATOR_CREATE_V2, DISCRIMINATOR_SELL_V3,
    INITIAL_REAL_TOKEN_RESERVES, INITIAL_VIRTUAL_SOL_RESERVES, INITIAL_VIRTUAL_TOKEN_RESERVES,
    MAX_TRANSACTION_SIZE, PUMP_PROGRAM_ID, TOKEN_TOTAL_SUPPLY,
};
use krypton::builder::instructions::{buy_v3_instruction, sell_v3_data, sell_v3_instruction};
use krypton::builder::math::{
    BuyPlan, CurveParams, SellPlan, DEFAULT_TOTAL_FEE_BPS, MAX_FEE_BPS, MAX_SLIPPAGE_BPS,
};
use krypton::builder::sell::{SellBuilder, SellSettings, DEFAULT_SELL_COMPUTE_UNIT_LIMIT};
use krypton::builder::{CreateVariant, LaunchAddresses};
use krypton::errors::{BuilderError, ExitError, JitoError, MonitorError, StreamErrorKind};
use krypton::monitor::curve::{
    BondingCurveState, CurveObservation, PriceReference, MIN_BONDING_CURVE_BYTES,
};
use krypton::monitor::exit::{
    order_channel, report_channel, run_sell_executor, EngineSnapshot, ExitEngine, ExitManager,
    ExitPlan, ExitSettings, ExitTrigger, Position, SellOrder, SellOutcome, SellReport,
    SellSubmitter, Tranche, EXIT_WINDOW_END_MS, EXIT_WINDOW_START_MS, MAX_LATENESS_MS,
    MAX_STOP_LOSS_ORDERS, MAX_TRANCHES, ORDER_CHANNEL_CAPACITY, UNRESOLVED_PROJECTION_SLOTS,
};
use krypton::monitor::geyser::{
    AccountUpdate, Backoff, BackoffPolicy, ConnectionState, EventStream, GeyserMonitor,
    HealthStatus, InstructionUpdate, MonitorSettings, PumpInstructionKind, StreamCommitment,
    StreamTransport, StreamUpdate, SubscriptionRequest,
};
use krypton::monitor::{shutdown_channel, ShutdownSignal};
use krypton::security::SecureWallet;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_sdk::hash::Hash;
use tokio::sync::{mpsc, watch};
use tokio::time::{sleep, Instant};

// --- Shared helpers ---------------------------------------------------------------------

fn key(byte: u8) -> Pubkey {
    Pubkey::new_from_array([byte; 32])
}

/// The bonding curve account the exit manager watches.
fn curve_key() -> Pubkey {
    key(77)
}

fn test_wallet() -> Result<SecureWallet, Box<dyn Error>> {
    let bytes = Keypair::new_from_array([1u8; 32]).to_bytes();
    Ok(SecureWallet::from_keypair_bytes(&bytes)?)
}

fn blockhash() -> Hash {
    Hash::new_from_array([3u8; 32])
}

/// Reserves and flags used to build a `BondingCurve` account payload.
#[derive(Clone, Copy)]
struct CurveSpec {
    virtual_token: u64,
    virtual_sol: u64,
    real_token: u64,
    real_sol: u64,
    supply: u64,
    complete: bool,
    mayhem: bool,
    cashback: bool,
}

impl Default for CurveSpec {
    fn default() -> Self {
        Self {
            virtual_token: INITIAL_VIRTUAL_TOKEN_RESERVES,
            virtual_sol: INITIAL_VIRTUAL_SOL_RESERVES,
            real_token: INITIAL_REAL_TOKEN_RESERVES,
            real_sol: 0,
            supply: TOKEN_TOTAL_SUPPLY,
            complete: false,
            mayhem: false,
            cashback: false,
        }
    }
}

impl CurveSpec {
    fn from_params(params: &CurveParams) -> Self {
        Self {
            virtual_token: params.virtual_token_reserves,
            virtual_sol: params.virtual_sol_reserves,
            real_token: params.real_token_reserves,
            supply: params.token_total_supply,
            ..Self::default()
        }
    }

    /// Full-length (166 bytes) Anchor account payload.
    fn bytes(&self) -> Vec<u8> {
        let mut data = Vec::with_capacity(166);
        data.extend_from_slice(&DISCRIMINATOR_BONDING_CURVE_ACCOUNT);
        for value in [
            self.virtual_token,
            self.virtual_sol,
            self.real_token,
            self.real_sol,
            self.supply,
        ] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.push(u8::from(self.complete));
        data.extend_from_slice(&[0u8; 32]); // creator
        data.push(u8::from(self.mayhem));
        data.push(u8::from(self.cashback));
        data.resize(166, 0); // quote_mint, fee fields, depth, post-complete fields
        data
    }
}

fn default_curve() -> CurveParams {
    CurveParams::default()
}

/// The creator's buy: 9.75 % of the supply.
fn launch_buy() -> Result<BuyPlan, Box<dyn Error>> {
    Ok(BuyPlan::from_supply_bps(
        &default_curve(),
        975,
        DEFAULT_TOTAL_FEE_BPS,
        500,
    )?)
}

/// The curve right after the creator's own buy, as the stream would report it.
fn post_buy_params() -> Result<CurveParams, Box<dyn Error>> {
    let buy = launch_buy()?;
    let curve = default_curve();
    Ok(CurveParams {
        virtual_token_reserves: curve.virtual_token_reserves - buy.token_amount,
        virtual_sol_reserves: curve.virtual_sol_reserves + buy.net_cost_lamports,
        real_token_reserves: curve.real_token_reserves - buy.token_amount,
        token_total_supply: curve.token_total_supply,
    })
}

fn post_buy_spec() -> Result<CurveSpec, Box<dyn Error>> {
    Ok(CurveSpec::from_params(&post_buy_params()?))
}

fn account_update(slot: u64, write_version: u64, pubkey: Pubkey, data: Vec<u8>) -> AccountUpdate {
    AccountUpdate {
        slot,
        write_version,
        pubkey,
        owner: PUMP_PROGRAM_ID,
        lamports: 1_000_000,
        data: Bytes::from(data),
    }
}

fn curve_update(slot: u64, spec: &CurveSpec) -> StreamUpdate {
    StreamUpdate::Account(account_update(slot, slot, curve_key(), spec.bytes()))
}

fn observation(slot: u64, spec: &CurveSpec) -> Result<CurveObservation, Box<dyn Error>> {
    Ok(CurveObservation {
        slot,
        write_version: slot,
        state: BondingCurveState::decode(&spec.bytes())?,
    })
}

// --- BondingCurve decoding ---------------------------------------------------------------

#[test]
fn decodes_a_full_length_account() -> Result<(), Box<dyn Error>> {
    let spec = CurveSpec {
        virtual_token: 1_000,
        virtual_sol: 2_000,
        real_token: 900,
        real_sol: 77,
        supply: 5_000,
        complete: false,
        mayhem: true,
        cashback: true,
    };
    let state = BondingCurveState::decode(&spec.bytes())?;
    assert_eq!(state.virtual_token_reserves, 1_000);
    assert_eq!(state.virtual_sol_reserves, 2_000);
    assert_eq!(state.real_token_reserves, 900);
    assert_eq!(state.real_sol_reserves, 77);
    assert_eq!(state.token_total_supply, 5_000);
    assert!(!state.complete);
    assert!(state.is_mayhem_mode);
    assert!(state.is_cashback_coin);
    Ok(())
}

#[test]
fn decodes_a_short_legacy_layout_with_default_flags() -> Result<(), Box<dyn Error>> {
    let full = CurveSpec::default().bytes();
    let short = full.get(..MIN_BONDING_CURVE_BYTES).ok_or("slice")?;
    assert_eq!(short.len(), 49);
    let state = BondingCurveState::decode(short)?;
    assert!(!state.is_mayhem_mode);
    assert!(!state.is_cashback_coin);
    assert_eq!(state.virtual_sol_reserves, INITIAL_VIRTUAL_SOL_RESERVES);
    Ok(())
}

#[test]
fn rejects_truncated_accounts() -> Result<(), Box<dyn Error>> {
    let full = CurveSpec::default().bytes();
    for len in [0usize, 8, 40, 48] {
        let part = full.get(..len).ok_or("slice")?;
        assert_eq!(
            BondingCurveState::decode(part),
            Err(MonitorError::AccountTooShort {
                actual: len,
                min: MIN_BONDING_CURVE_BYTES
            }),
            "length {len}"
        );
    }
    Ok(())
}

#[test]
fn rejects_a_foreign_discriminator() {
    let mut data = CurveSpec::default().bytes();
    if let Some(first) = data.first_mut() {
        *first ^= 0xFF;
    }
    assert_eq!(
        BondingCurveState::decode(&data),
        Err(MonitorError::UnexpectedDiscriminator)
    );
}

#[test]
fn rejects_non_boolean_flags() {
    for offset in [48usize, 81, 82] {
        let mut data = CurveSpec::default().bytes();
        if let Some(byte) = data.get_mut(offset) {
            *byte = 2;
        }
        assert!(
            matches!(
                BondingCurveState::decode(&data),
                Err(MonitorError::InvalidAccountData { .. })
            ),
            "offset {offset}"
        );
    }
}

#[test]
fn live_curves_with_zero_virtual_reserves_are_rejected_but_completed_ones_are_not(
) -> Result<(), Box<dyn Error>> {
    let live = CurveSpec {
        virtual_token: 0,
        ..CurveSpec::default()
    };
    assert!(matches!(
        BondingCurveState::decode(&live.bytes()),
        Err(MonitorError::InvalidAccountData { .. })
    ));
    let live = CurveSpec {
        virtual_sol: 0,
        ..CurveSpec::default()
    };
    assert!(BondingCurveState::decode(&live.bytes()).is_err());
    let done = CurveSpec {
        virtual_token: 0,
        virtual_sol: 0,
        complete: true,
        ..CurveSpec::default()
    };
    assert!(BondingCurveState::decode(&done.bytes())?.complete);
    Ok(())
}

#[test]
fn curve_progress_is_measured_against_the_initial_real_reserves() -> Result<(), Box<dyn Error>> {
    let fresh = BondingCurveState::decode(&CurveSpec::default().bytes())?;
    assert_eq!(fresh.progress_bps()?, 0);

    let half = CurveSpec {
        real_token: INITIAL_REAL_TOKEN_RESERVES / 2,
        ..CurveSpec::default()
    };
    assert_eq!(
        BondingCurveState::decode(&half.bytes())?.progress_bps()?,
        5_000
    );

    let sold_out = CurveSpec {
        real_token: 0,
        ..CurveSpec::default()
    };
    assert_eq!(
        BondingCurveState::decode(&sold_out.bytes())?.progress_bps()?,
        10_000
    );

    // 9.75 % of supply is 12.29 % of the tokens for sale: floor(97.5e12 / 793.1e12 * 1e4).
    let after_buy = BondingCurveState::decode(&post_buy_spec()?.bytes())?;
    assert_eq!(after_buy.progress_bps()?, 1_229);

    // More reserves than the reference means nothing was sold.
    assert_eq!(fresh.progress_bps_against(1)?, 0);
    assert!(fresh.progress_bps_against(0).is_err());
    Ok(())
}

#[test]
fn price_ratio_uses_cross_multiplication() -> Result<(), Box<dyn Error>> {
    let base = BondingCurveState::decode(&CurveSpec::default().bytes())?;
    let reference = PriceReference::from(&base);
    assert_eq!(base.price_ratio_bps(&reference)?, 10_000);

    // Same SOL, half the tokens: the price doubles.
    let doubled = BondingCurveState::decode(
        &CurveSpec {
            virtual_token: INITIAL_VIRTUAL_TOKEN_RESERVES / 2,
            ..CurveSpec::default()
        }
        .bytes(),
    )?;
    assert_eq!(doubled.price_ratio_bps(&reference)?, 20_000);

    // Half the SOL: the price halves.
    let halved = BondingCurveState::decode(
        &CurveSpec {
            virtual_sol: INITIAL_VIRTUAL_SOL_RESERVES / 2,
            ..CurveSpec::default()
        }
        .bytes(),
    )?;
    assert_eq!(halved.price_ratio_bps(&reference)?, 5_000);

    // A zero reference is undefined, not a division by zero.
    let zero = PriceReference {
        virtual_sol_reserves: 0,
        virtual_token_reserves: 1,
    };
    assert!(base.price_ratio_bps(&zero).is_err());

    // u128 overflow is reported, not wrapped.
    let huge = BondingCurveState::decode(
        &CurveSpec {
            virtual_token: u64::MAX,
            virtual_sol: u64::MAX,
            ..CurveSpec::default()
        }
        .bytes(),
    )?;
    let huge_ref = PriceReference {
        virtual_sol_reserves: u64::MAX,
        virtual_token_reserves: u64::MAX,
    };
    assert!(huge.price_ratio_bps(&huge_ref).is_err());
    Ok(())
}

#[test]
fn price_reference_after_the_creator_buy_matches_the_post_buy_curve() -> Result<(), Box<dyn Error>>
{
    let buy = launch_buy()?;
    let reference = PriceReference::after_buy(&default_curve(), &buy)?;
    let post = post_buy_params()?;
    assert_eq!(reference.virtual_sol_reserves, post.virtual_sol_reserves);
    assert_eq!(
        reference.virtual_token_reserves,
        post.virtual_token_reserves
    );

    let too_many = BuyPlan {
        token_amount: INITIAL_VIRTUAL_TOKEN_RESERVES + 1,
        ..buy
    };
    assert!(matches!(
        PriceReference::after_buy(&default_curve(), &too_many),
        Err(BuilderError::ArithmeticOverflow { .. })
    ));
    Ok(())
}

#[test]
fn observations_order_by_slot_then_write_version() -> Result<(), Box<dyn Error>> {
    let spec = CurveSpec::default();
    let a = CurveObservation {
        slot: 10,
        write_version: 5,
        ..observation(10, &spec)?
    };
    let same_slot_later = CurveObservation {
        write_version: 6,
        ..a
    };
    let later_slot = CurveObservation {
        slot: 11,
        write_version: 0,
        ..a
    };
    assert!(same_slot_later.is_newer_than(&a));
    assert!(later_slot.is_newer_than(&same_slot_later));
    assert!(!a.is_newer_than(&a));
    assert!(!a.is_newer_than(&later_slot));
    Ok(())
}

#[test]
fn observation_from_an_account_update_keeps_the_stream_position() -> Result<(), Box<dyn Error>> {
    let update = account_update(42, 9, curve_key(), CurveSpec::default().bytes());
    let observed = CurveObservation::from_account(&update)?;
    assert_eq!(observed.slot, 42);
    assert_eq!(observed.write_version, 9);

    let garbage = account_update(1, 1, curve_key(), vec![1, 2, 3]);
    assert!(CurveObservation::from_account(&garbage).is_err());
    Ok(())
}

// --- Instruction classification ------------------------------------------------------------

fn instruction_update(data: Vec<u8>) -> InstructionUpdate {
    InstructionUpdate {
        slot: 1,
        program_id: PUMP_PROGRAM_ID,
        data: Bytes::from(data),
    }
}

#[test]
fn classifies_every_known_pump_instruction_family() {
    for discriminator in DISCRIMINATORS_BUY {
        let update = instruction_update(discriminator.to_vec());
        assert_eq!(update.kind(), PumpInstructionKind::Buy);
    }
    for discriminator in DISCRIMINATORS_SELL {
        let update = instruction_update(discriminator.to_vec());
        assert_eq!(update.kind(), PumpInstructionKind::Sell);
    }
    for discriminator in [DISCRIMINATOR_CREATE, DISCRIMINATOR_CREATE_V2] {
        let update = instruction_update(discriminator.to_vec());
        assert_eq!(update.kind(), PumpInstructionKind::Create);
    }
    assert!(DISCRIMINATORS_BUY.contains(&DISCRIMINATOR_BUY_V3));
    assert!(DISCRIMINATORS_SELL.contains(&DISCRIMINATOR_SELL_V3));
}

#[test]
fn unknown_and_short_instructions_are_other() {
    assert_eq!(
        instruction_update(vec![0; 8]).kind(),
        PumpInstructionKind::Other
    );
    assert_eq!(
        instruction_update(vec![]).kind(),
        PumpInstructionKind::Other
    );
    assert_eq!(
        instruction_update(
            DISCRIMINATOR_SELL_V3
                .get(..7)
                .map(<[u8]>::to_vec)
                .unwrap_or_default()
        )
        .kind(),
        PumpInstructionKind::Other
    );
    // Trailing arguments do not change the classification.
    let mut data = DISCRIMINATOR_SELL_V3.to_vec();
    data.extend_from_slice(&[9; 16]);
    assert_eq!(instruction_update(data).kind(), PumpInstructionKind::Sell);
}

#[test]
fn discriminator_tables_have_no_overlap() {
    for sell in DISCRIMINATORS_SELL {
        assert!(!DISCRIMINATORS_BUY.contains(&sell));
    }
    assert_eq!(
        DISCRIMINATOR_SELL_V3,
        [28, 146, 222, 119, 38, 196, 105, 213]
    );
    assert_eq!(
        DISCRIMINATOR_BONDING_CURVE_ACCOUNT,
        [23, 183, 248, 55, 96, 216, 172, 96]
    );
}

// --- Sell math --------------------------------------------------------------------------------

/// `(gross, fee, net, min_sol_output)` for the default curve, computed with
/// exact integer arithmetic: gross = floor(vs*t/(vt+t)), fee = ceil(gross*fee/1e4),
/// min = floor(net*(1e4-slip)/1e4).
const SELL_VECTORS: [(u64, u16, u16, u64, u64, u64, u64); 4] = [
    (
        100_000_000_000_000,
        125,
        500,
        2_557_544_757,
        31_969_310,
        2_525_575_447,
        2_399_296_674,
    ),
    (
        100_000_000_000_000,
        0,
        0,
        2_557_544_757,
        0,
        2_557_544_757,
        2_557_544_757,
    ),
    (
        100_000_000_000_000,
        125,
        1_000,
        2_557_544_757,
        31_969_310,
        2_525_575_447,
        2_273_017_902,
    ),
    (
        198_275_000_000_000,
        125,
        500,
        4_678_964_032,
        58_487_051,
        4_620_476_981,
        4_389_453_131,
    ),
];

#[test]
fn sell_plan_matches_independent_vectors() -> Result<(), Box<dyn Error>> {
    for (tokens, fee_bps, slippage_bps, gross, fee, net, floor) in SELL_VECTORS {
        let plan = SellPlan::from_token_amount(&default_curve(), tokens, fee_bps, slippage_bps)?;
        assert_eq!(plan.token_amount, tokens);
        assert_eq!(plan.gross_proceeds_lamports, gross, "gross for {tokens}");
        assert_eq!(plan.fee_lamports, fee, "fee for {tokens}");
        assert_eq!(plan.net_proceeds_lamports, net, "net for {tokens}");
        assert_eq!(plan.min_sol_output_lamports, floor, "floor for {tokens}");
        assert_eq!(plan.fee_bps, fee_bps);
        assert_eq!(plan.slippage_bps, slippage_bps);
    }
    Ok(())
}

#[test]
fn sell_rounding_never_favours_the_seller() -> Result<(), Box<dyn Error>> {
    let curve = default_curve();
    let amounts = [
        1_000_000u64,
        999_999_999,
        12_345_678_901,
        5_000_000_000_000,
        100_000_000_000_000,
        793_100_000_000_000,
    ];
    let mut previous_gross = 0;
    for tokens in amounts {
        let plan = SellPlan::from_token_amount(&curve, tokens, 125, 500)?;
        assert!(plan.min_sol_output_lamports <= plan.net_proceeds_lamports);
        assert!(plan.net_proceeds_lamports <= plan.gross_proceeds_lamports);
        assert_eq!(
            plan.gross_proceeds_lamports - plan.net_proceeds_lamports,
            plan.fee_lamports
        );
        // The fee is rounded up and the floor rounded down.
        let exact_fee = u128::from(plan.gross_proceeds_lamports) * 125;
        assert_eq!(
            u128::from(plan.fee_lamports),
            exact_fee.div_ceil(10_000),
            "fee for {tokens}"
        );
        let exact_floor = u128::from(plan.net_proceeds_lamports) * 9_500;
        assert_eq!(
            u128::from(plan.min_sol_output_lamports),
            exact_floor / 10_000,
            "floor for {tokens}"
        );
        // Proceeds never exceed the linear estimate: price impact only hurts.
        let linear = u128::from(curve.virtual_sol_reserves) * u128::from(tokens)
            / u128::from(curve.virtual_token_reserves);
        assert!(u128::from(plan.gross_proceeds_lamports) <= linear);
        assert!(plan.gross_proceeds_lamports >= previous_gross);
        previous_gross = plan.gross_proceeds_lamports;
    }
    Ok(())
}

#[test]
fn sell_slippage_obeys_the_ten_percent_cap() -> Result<(), Box<dyn Error>> {
    assert_eq!(MAX_SLIPPAGE_BPS, 1_000);
    assert!(
        SellPlan::from_token_amount(&default_curve(), 10_u64.pow(14), 125, MAX_SLIPPAGE_BPS)
            .is_ok()
    );
    for bps in [1_001u16, 2_500, 10_000, u16::MAX] {
        assert_eq!(
            SellPlan::from_token_amount(&default_curve(), 10_u64.pow(14), 125, bps),
            Err(BuilderError::InvalidSlippage {
                bps,
                max: MAX_SLIPPAGE_BPS
            })
        );
    }
    assert_eq!(
        SellPlan::from_token_amount(&default_curve(), 10_u64.pow(14), MAX_FEE_BPS + 1, 100),
        Err(BuilderError::InvalidFee {
            bps: MAX_FEE_BPS + 1,
            max: MAX_FEE_BPS
        })
    );
    Ok(())
}

#[test]
fn sell_rejects_zero_amounts_and_zero_floors() {
    assert_eq!(
        SellPlan::from_token_amount(&default_curve(), 0, 125, 100),
        Err(BuilderError::ZeroSellAmount)
    );
    // One base unit is worth less than a lamport: a zero floor would disable the
    // slippage protection, so it is refused.
    assert!(matches!(
        SellPlan::from_token_amount(&default_curve(), 1, 125, 100),
        Err(BuilderError::InvalidCurve { .. })
    ));
}

#[test]
fn sell_rejects_inconsistent_curves_and_reports_overflow() {
    let broken = CurveParams {
        virtual_token_reserves: 0,
        ..default_curve()
    };
    assert!(matches!(
        SellPlan::from_token_amount(&broken, 1_000, 125, 100),
        Err(BuilderError::InvalidCurve { .. })
    ));
    let huge = CurveParams {
        virtual_token_reserves: u64::MAX,
        virtual_sol_reserves: u64::MAX,
        real_token_reserves: 0,
        token_total_supply: u64::MAX,
    };
    // `vt + t` still fits a u128; the product `vs * t` does too (2 x 64 bits);
    // the plan is computed without wrapping and stays within u64.
    assert!(SellPlan::from_token_amount(&huge, u64::MAX, 0, 0).is_ok());
}

#[test]
fn sell_plan_projects_the_curve_after_the_sale() -> Result<(), Box<dyn Error>> {
    let curve = default_curve();
    let plan = SellPlan::from_token_amount(&curve, 100_000_000_000_000, 125, 500)?;
    let after = plan.apply_to(&curve)?;
    assert_eq!(
        after.virtual_token_reserves,
        curve.virtual_token_reserves + 100_000_000_000_000
    );
    assert_eq!(
        after.virtual_sol_reserves,
        curve.virtual_sol_reserves - 2_557_544_757
    );
    assert_eq!(
        after.real_token_reserves,
        curve.real_token_reserves + 100_000_000_000_000
    );
    assert_eq!(after.token_total_supply, curve.token_total_supply);
    // The constant-product invariant never decreases (rounding favours the curve).
    let before_k =
        u128::from(curve.virtual_token_reserves) * u128::from(curve.virtual_sol_reserves);
    let after_k = u128::from(after.virtual_token_reserves) * u128::from(after.virtual_sol_reserves);
    assert!(after_k >= before_k);

    // Selling the same amount again yields strictly less: price impact compounds.
    let second = SellPlan::from_token_amount(&after, 100_000_000_000_000, 125, 500)?;
    assert!(second.gross_proceeds_lamports < plan.gross_proceeds_lamports);

    let drained = SellPlan {
        gross_proceeds_lamports: u64::MAX,
        ..plan
    };
    assert!(matches!(
        drained.apply_to(&curve),
        Err(BuilderError::ArithmeticOverflow { .. })
    ));
    Ok(())
}

// --- sell_v3 encoding ----------------------------------------------------------------------

fn sample_sell_plan() -> Result<SellPlan, Box<dyn Error>> {
    Ok(SellPlan::from_token_amount(
        &default_curve(),
        100_000_000_000_000,
        125,
        500,
    )?)
}

#[test]
fn sell_v3_data_is_discriminator_amount_and_floor() -> Result<(), Box<dyn Error>> {
    let plan = sample_sell_plan()?;
    let data = sell_v3_data(&plan);
    assert_eq!(data.len(), 24);
    assert_eq!(data.get(..8), Some(DISCRIMINATOR_SELL_V3.as_slice()));
    assert_eq!(
        data.get(8..16),
        Some(100_000_000_000_000u64.to_le_bytes().as_slice())
    );
    assert_eq!(
        data.get(16..24),
        Some(2_399_296_674u64.to_le_bytes().as_slice())
    );
    Ok(())
}

#[test]
fn sell_v3_shares_the_buy_v3_account_list() -> Result<(), Box<dyn Error>> {
    let addresses = LaunchAddresses::derive(&key(2), &key(1), CreateVariant::CreateV2)?;
    let recipient = key(5);
    let sell = sell_v3_instruction(&addresses, &sample_sell_plan()?, &recipient);
    let buy = buy_v3_instruction(&addresses, &launch_buy()?, &recipient);
    assert_eq!(sell.program_id, PUMP_PROGRAM_ID);
    assert_eq!(sell.accounts.len(), 17);
    assert_eq!(sell.accounts, buy.accounts);
    assert_ne!(sell.data, buy.data);

    // Only the seller signs; the writable set follows the IDL.
    let signers: Vec<usize> = sell
        .accounts
        .iter()
        .enumerate()
        .filter(|(_, meta)| meta.is_signer)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(signers, vec![8]);
    let writable: Vec<usize> = sell
        .accounts
        .iter()
        .enumerate()
        .filter(|(_, meta)| meta.is_writable)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(writable, vec![5, 6, 7, 8, 9, 10, 11, 13]);
    assert_eq!(sell.accounts.get(8).map(|meta| meta.pubkey), Some(key(1)));
    assert_eq!(
        sell.accounts.get(13).map(|meta| meta.pubkey),
        Some(recipient)
    );
    Ok(())
}

// --- Sell transaction builder --------------------------------------------------------------

fn sell_settings() -> SellSettings {
    SellSettings {
        compute_unit_limit: 200_000,
        priority_fee_micro_lamports: 100_000,
        max_priority_fee_lamports: 5_000_000,
        buyback_fee_recipient: None,
    }
}

fn sell_builder() -> Result<SellBuilder, Box<dyn Error>> {
    let wallet = test_wallet()?;
    Ok(SellBuilder::new(
        wallet.pubkey(),
        key(2),
        CreateVariant::CreateV2,
        sell_settings(),
    )?)
}

#[test]
fn sell_builder_orders_compute_budget_then_sell() -> Result<(), Box<dyn Error>> {
    let builder = sell_builder()?;
    let plan = sample_sell_plan()?;
    let instructions = builder.instructions(&plan);
    assert_eq!(instructions.len(), 3);
    let limit = instructions.first().ok_or("limit")?;
    assert_eq!(
        limit.data,
        [vec![2u8], 200_000u32.to_le_bytes().to_vec()].concat()
    );
    let price = instructions.get(1).ok_or("price")?;
    assert_eq!(
        price.data,
        [vec![3u8], 100_000u64.to_le_bytes().to_vec()].concat()
    );
    let sell = instructions.get(2).ok_or("sell")?;
    assert_eq!(sell.data, sell_v3_data(&plan));
    assert_eq!(sell.accounts.len(), 17);
    Ok(())
}

#[test]
fn sell_builder_enforces_the_priority_fee_cap() -> Result<(), Box<dyn Error>> {
    let wallet = test_wallet()?;
    let build = |settings: SellSettings| {
        SellBuilder::new(wallet.pubkey(), key(2), CreateVariant::CreateV2, settings)
    };
    // 25_000_000 micro-lamports x 200_000 CU = 5_000_000 lamports: exactly at the cap.
    let at_cap = SellSettings {
        priority_fee_micro_lamports: 25_000_000,
        ..sell_settings()
    };
    assert!(build(at_cap).is_ok());
    let over = SellSettings {
        priority_fee_micro_lamports: 25_000_001,
        ..sell_settings()
    };
    assert!(matches!(
        build(over),
        Err(BuilderError::PriorityFeeTooHigh {
            fee_lamports: 5_000_001,
            max: 5_000_000
        })
    ));
    // The configurable cap can never exceed the absolute ceiling.
    let uncapped = SellSettings {
        priority_fee_micro_lamports: u64::MAX,
        max_priority_fee_lamports: u64::MAX,
        ..sell_settings()
    };
    assert!(matches!(
        build(uncapped),
        Err(BuilderError::PriorityFeeTooHigh {
            max: 50_000_000,
            ..
        })
    ));
    for limit in [0u32, 1_400_001] {
        let bad = SellSettings {
            compute_unit_limit: limit,
            ..sell_settings()
        };
        assert!(matches!(build(bad), Err(BuilderError::InvalidCurve { .. })));
    }
    assert_eq!(DEFAULT_SELL_COMPUTE_UNIT_LIMIT, 200_000);
    Ok(())
}

#[test]
fn sell_order_cost_adds_signature_priority_and_tip() -> Result<(), Box<dyn Error>> {
    let builder = sell_builder()?;
    assert_eq!(builder.priority_fee_lamports(), 20_000);
    assert_eq!(builder.order_cost_lamports(0)?, 5_000 + 20_000);
    assert_eq!(builder.order_cost_lamports(100_000)?, 125_000);
    assert!(matches!(
        builder.order_cost_lamports(u64::MAX),
        Err(BuilderError::ArithmeticOverflow { .. })
    ));
    Ok(())
}

#[test]
fn sell_transactions_fit_a_packet_with_a_tip() -> Result<(), Box<dyn Error>> {
    let builder = sell_builder()?;
    let plan = sample_sell_plan()?;
    let tip = solana_sdk::instruction::Instruction {
        program_id: key(9),
        accounts: vec![solana_sdk::instruction::AccountMeta::new(
            builder.payer(),
            true,
        )],
        data: vec![2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    };
    let plain = builder.compile(&plan, blockhash(), &[])?;
    let tipped = builder.compile(&plan, blockhash(), std::slice::from_ref(&tip))?;
    assert_eq!(plain.message.instructions.len(), 3);
    assert_eq!(tipped.message.instructions.len(), 4);
    assert!(plain.wire_size <= tipped.wire_size);
    assert!(tipped.wire_size < MAX_TRANSACTION_SIZE);
    Ok(())
}

#[test]
fn sell_builder_refuses_a_foreign_wallet() -> Result<(), Box<dyn Error>> {
    let other = SellBuilder::new(key(9), key(2), CreateVariant::CreateV2, sell_settings())?;
    let compiled = other.compile(&sample_sell_plan()?, blockhash(), &[])?;
    assert_eq!(
        other.sign(compiled.message, &test_wallet()?).err(),
        Some(BuilderError::PayerMismatch)
    );

    let builder = sell_builder()?;
    assert!(builder
        .build_signed(&sample_sell_plan()?, blockhash(), &[], &test_wallet()?)
        .is_ok());
    Ok(())
}

// --- Backoff -----------------------------------------------------------------------------------

fn policy(initial_ms: u64, max_ms: u64, jitter_permille: u16) -> BackoffPolicy {
    BackoffPolicy {
        initial: Duration::from_millis(initial_ms),
        max: Duration::from_millis(max_ms),
        jitter_permille,
    }
}

#[test]
fn backoff_doubles_until_the_cap_and_resets() {
    let mut backoff = Backoff::new(policy(50, 400, 0), 1);
    let delays: Vec<u64> = (0..7)
        .map(|_| u64::try_from(backoff.next_delay().as_millis()).unwrap_or(u64::MAX))
        .collect();
    assert_eq!(delays, vec![50, 100, 200, 400, 400, 400, 400]);
    assert_eq!(backoff.attempts(), 7);
    backoff.reset();
    assert_eq!(backoff.attempts(), 0);
    assert_eq!(backoff.next_delay(), Duration::from_millis(50));
}

#[test]
fn backoff_survives_absurd_attempt_counts() {
    let mut backoff = Backoff::new(policy(1, 5_000, 0), 1);
    let mut last = Duration::ZERO;
    for _ in 0..100_000 {
        last = backoff.next_delay();
    }
    assert_eq!(last, Duration::from_secs(5));
    assert_eq!(backoff.attempts(), 100_000);
}

#[test]
fn backoff_jitter_stays_within_bounds_and_is_reproducible() {
    let run = |seed: u64| -> Vec<Duration> {
        let mut backoff = Backoff::new(policy(100, 100, 250), seed);
        (0..200).map(|_| backoff.next_delay()).collect()
    };
    let first = run(7);
    assert_eq!(first, run(7), "same seed, same sequence");
    assert_ne!(first, run(8), "different seeds diverge");
    for delay in &first {
        // base 100 ms, jitter 25 %: [75, 100].
        assert!(*delay >= Duration::from_millis(75), "{delay:?}");
        assert!(*delay <= Duration::from_millis(100), "{delay:?}");
    }
    // The generator really spreads the values.
    let distinct: std::collections::BTreeSet<_> = first.iter().collect();
    assert!(distinct.len() > 5);
    // A zero seed is replaced, not stuck at zero.
    let mut zero_seed = Backoff::new(policy(100, 100, 500), 0);
    let values: std::collections::BTreeSet<_> = (0..50).map(|_| zero_seed.next_delay()).collect();
    assert!(values.len() > 1);
}

#[test]
fn backoff_policy_and_settings_are_validated() {
    let valid = MonitorSettings::default();
    assert!(valid.validate().is_ok());
    let invalid = |settings: MonitorSettings| {
        matches!(settings.validate(), Err(MonitorError::InvalidConfig { .. }))
    };
    assert!(invalid(MonitorSettings {
        backoff: policy(0, 100, 0),
        ..valid
    }));
    assert!(invalid(MonitorSettings {
        backoff: policy(200, 100, 0),
        ..valid
    }));
    assert!(invalid(MonitorSettings {
        backoff: policy(10, 100, 1_001),
        ..valid
    }));
    assert!(invalid(MonitorSettings {
        connect_timeout: Duration::ZERO,
        ..valid
    }));
    assert!(invalid(MonitorSettings {
        idle_timeout: Duration::ZERO,
        ..valid
    }));
}

#[test]
fn only_transient_monitor_errors_are_retryable() {
    let connection = |kind| MonitorError::Connection { kind };
    for kind in [
        StreamErrorKind::Unreachable,
        StreamErrorKind::Tls,
        StreamErrorKind::Overloaded,
        StreamErrorKind::Protocol,
        StreamErrorKind::Other,
    ] {
        assert!(connection(kind).is_retryable(), "{kind}");
    }
    assert!(!connection(StreamErrorKind::Unauthenticated).is_retryable());
    assert!(!connection(StreamErrorKind::InvalidRequest).is_retryable());
    assert!(MonitorError::StreamClosed.is_retryable());
    assert!(MonitorError::IdleTimeout { millis: 1 }.is_retryable());
    assert!(MonitorError::ConnectTimeout { millis: 1 }.is_retryable());
    assert!(!MonitorError::UnexpectedDiscriminator.is_retryable());
    assert!(!MonitorError::ReconnectExhausted { attempts: 1 }.is_retryable());
    assert!(!MonitorError::InvalidConfig { reason: "x" }.is_retryable());
}

// --- Scripted stream transport -------------------------------------------------------------

enum Step {
    Update(StreamUpdate),
    Fail(MonitorError),
    Close,
    Wait(Duration),
    Hang,
}

enum Script {
    Refuse(MonitorError),
    HangConnect,
    Stream(Vec<Step>),
}

struct ScriptedStream {
    steps: VecDeque<Step>,
}

impl EventStream for ScriptedStream {
    async fn next_update(&mut self) -> Result<Option<StreamUpdate>, MonitorError> {
        loop {
            match self.steps.pop_front() {
                Some(Step::Update(update)) => return Ok(Some(update)),
                Some(Step::Fail(error)) => return Err(error),
                Some(Step::Close) => return Ok(None),
                Some(Step::Wait(duration)) => sleep(duration).await,
                Some(Step::Hang) | None => {
                    return std::future::pending::<Result<Option<StreamUpdate>, MonitorError>>()
                        .await
                }
            }
        }
    }
}

struct ScriptedTransport {
    scripts: VecDeque<Script>,
    connects: Arc<Mutex<Vec<Instant>>>,
}

impl StreamTransport for ScriptedTransport {
    type Stream = ScriptedStream;

    async fn connect(
        &mut self,
        _request: &SubscriptionRequest,
    ) -> Result<ScriptedStream, MonitorError> {
        if let Ok(mut times) = self.connects.lock() {
            times.push(Instant::now());
        }
        match self.scripts.pop_front() {
            Some(Script::Refuse(error)) => Err(error),
            Some(Script::HangConnect) => {
                std::future::pending::<Result<ScriptedStream, MonitorError>>().await
            }
            Some(Script::Stream(steps)) => Ok(ScriptedStream {
                steps: steps.into(),
            }),
            // Out of script: the endpoint stays down.
            None => Err(unreachable_error()),
        }
    }
}

fn unreachable_error() -> MonitorError {
    MonitorError::Connection {
        kind: StreamErrorKind::Unreachable,
    }
}

fn request() -> SubscriptionRequest {
    SubscriptionRequest {
        accounts: vec![curve_key()],
        instruction_program: Some(PUMP_PROGRAM_ID),
        commitment: StreamCommitment::Processed,
    }
}

/// Jitter-free settings so delays are exact on the paused clock.
fn test_settings() -> MonitorSettings {
    MonitorSettings {
        backoff: policy(50, 5_000, 0),
        connect_timeout: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(2),
        stable_after: Duration::from_secs(5),
        max_consecutive_failures: Some(20),
    }
}

struct RunningMonitor {
    handle: tokio::task::JoinHandle<Result<HealthStatus, MonitorError>>,
    events: mpsc::Receiver<StreamUpdate>,
    health: watch::Receiver<HealthStatus>,
    trigger: krypton::monitor::ShutdownTrigger,
    connects: Arc<Mutex<Vec<Instant>>>,
}

impl RunningMonitor {
    fn connect_gaps_ms(&self) -> Result<Vec<u128>, Box<dyn Error>> {
        let times = self.connects.lock().map_err(|_| "poisoned")?;
        Ok(times
            .windows(2)
            .map(|pair| match pair {
                [a, b] => b.duration_since(*a).as_millis(),
                _ => 0,
            })
            .collect())
    }

    fn connect_count(&self) -> Result<usize, Box<dyn Error>> {
        Ok(self.connects.lock().map_err(|_| "poisoned")?.len())
    }

    async fn stop(self) -> Result<HealthStatus, Box<dyn Error>> {
        self.trigger.trigger();
        Ok(self.handle.await??)
    }
}

fn start_monitor(
    scripts: Vec<Script>,
    settings: MonitorSettings,
    capacity: usize,
) -> Result<RunningMonitor, Box<dyn Error>> {
    let connects = Arc::new(Mutex::new(Vec::new()));
    let transport = ScriptedTransport {
        scripts: scripts.into(),
        connects: Arc::clone(&connects),
    };
    let monitor = GeyserMonitor::new(transport, request(), settings)?.with_jitter_seed(1);
    let (events_tx, events) = mpsc::channel(capacity);
    let (health_tx, health) = watch::channel(HealthStatus::default());
    let (trigger, signal) = shutdown_channel();
    let handle = tokio::spawn(monitor.run(events_tx, health_tx, signal));
    Ok(RunningMonitor {
        handle,
        events,
        health,
        trigger,
        connects,
    })
}

async fn next_event(monitor: &mut RunningMonitor) -> Result<StreamUpdate, Box<dyn Error>> {
    Ok(monitor.events.recv().await.ok_or("event channel closed")?)
}

// --- Geyser supervisor -----------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn forwards_updates_in_order_and_drops_heartbeats() -> Result<(), Box<dyn Error>> {
    let curve = curve_update(2, &CurveSpec::default());
    let instruction = StreamUpdate::Instruction(instruction_update(DISCRIMINATOR_SELL_V3.to_vec()));
    let mut monitor = start_monitor(
        vec![Script::Stream(vec![
            Step::Update(StreamUpdate::Slot(1)),
            Step::Update(StreamUpdate::Heartbeat),
            Step::Update(curve.clone()),
            Step::Update(StreamUpdate::Heartbeat),
            Step::Update(instruction.clone()),
            Step::Hang,
        ])],
        test_settings(),
        16,
    )?;
    assert_eq!(next_event(&mut monitor).await?, StreamUpdate::Slot(1));
    assert_eq!(next_event(&mut monitor).await?, curve);
    assert_eq!(next_event(&mut monitor).await?, instruction);
    assert_eq!(monitor.health.borrow().state, ConnectionState::Connected);
    let status = monitor.stop().await?;
    assert_eq!(status.state, ConnectionState::Stopped);
    assert_eq!(status.events_forwarded, 3);
    assert_eq!(status.last_slot, 2);
    assert_eq!(status.reconnects, 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn reconnects_with_exponential_backoff() -> Result<(), Box<dyn Error>> {
    let mut monitor = start_monitor(
        vec![
            Script::Refuse(unreachable_error()),
            Script::Refuse(unreachable_error()),
            Script::Refuse(unreachable_error()),
            Script::Stream(vec![Step::Update(StreamUpdate::Slot(5)), Step::Hang]),
        ],
        test_settings(),
        16,
    )?;
    assert_eq!(next_event(&mut monitor).await?, StreamUpdate::Slot(5));
    assert_eq!(monitor.connect_gaps_ms()?, vec![50, 100, 200]);
    let status = monitor.stop().await?;
    // The first successful connection is not a *re*connection.
    assert_eq!(status.reconnects, 0);
    assert_eq!(status.consecutive_failures, 3);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn backoff_is_capped_at_the_configured_maximum() -> Result<(), Box<dyn Error>> {
    let settings = MonitorSettings {
        backoff: policy(50, 400, 0),
        max_consecutive_failures: None,
        ..test_settings()
    };
    let scripts = (0..7)
        .map(|_| Script::Refuse(unreachable_error()))
        .collect();
    let mut monitor = start_monitor(scripts, settings, 16)?;
    // Let the supervisor burn through the refusals on the paused clock.
    sleep(Duration::from_secs(10)).await;
    assert_eq!(
        monitor.connect_gaps_ms()?.get(..6),
        Some([50, 100, 200, 400, 400, 400].as_slice())
    );
    monitor.events.close();
    let _ = monitor.stop().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn resubscribes_after_the_server_closes_the_stream() -> Result<(), Box<dyn Error>> {
    let mut monitor = start_monitor(
        vec![
            Script::Stream(vec![Step::Update(StreamUpdate::Slot(1)), Step::Close]),
            Script::Stream(vec![Step::Update(StreamUpdate::Slot(2)), Step::Hang]),
        ],
        test_settings(),
        16,
    )?;
    assert_eq!(next_event(&mut monitor).await?, StreamUpdate::Slot(1));
    assert_eq!(next_event(&mut monitor).await?, StreamUpdate::Slot(2));
    assert_eq!(monitor.connect_gaps_ms()?, vec![50]);
    let status = monitor.stop().await?;
    assert_eq!(status.reconnects, 1);
    assert_eq!(status.events_forwarded, 2);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn recovers_from_a_mid_stream_error() -> Result<(), Box<dyn Error>> {
    let mut monitor = start_monitor(
        vec![
            Script::Stream(vec![
                Step::Update(StreamUpdate::Slot(1)),
                Step::Fail(MonitorError::Connection {
                    kind: StreamErrorKind::Overloaded,
                }),
            ]),
            Script::Stream(vec![Step::Update(StreamUpdate::Slot(2)), Step::Hang]),
        ],
        test_settings(),
        16,
    )?;
    assert_eq!(next_event(&mut monitor).await?, StreamUpdate::Slot(1));
    assert_eq!(next_event(&mut monitor).await?, StreamUpdate::Slot(2));
    let _ = monitor.stop().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn the_idle_watchdog_replaces_a_silent_stream() -> Result<(), Box<dyn Error>> {
    let mut monitor = start_monitor(
        vec![
            Script::Stream(vec![Step::Update(StreamUpdate::Slot(1)), Step::Hang]),
            Script::Stream(vec![Step::Update(StreamUpdate::Slot(2)), Step::Hang]),
        ],
        test_settings(),
        16,
    )?;
    assert_eq!(next_event(&mut monitor).await?, StreamUpdate::Slot(1));
    assert_eq!(next_event(&mut monitor).await?, StreamUpdate::Slot(2));
    // 2 s of silence, then the 50 ms backoff.
    assert_eq!(monitor.connect_gaps_ms()?, vec![2_050]);
    let _ = monitor.stop().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn heartbeats_prove_liveness() -> Result<(), Box<dyn Error>> {
    let wait = Step::Wait(Duration::from_millis(1_500));
    let mut monitor = start_monitor(
        vec![Script::Stream(vec![
            wait,
            Step::Update(StreamUpdate::Heartbeat),
            Step::Wait(Duration::from_millis(1_500)),
            Step::Update(StreamUpdate::Heartbeat),
            Step::Wait(Duration::from_millis(1_500)),
            Step::Update(StreamUpdate::Slot(9)),
            Step::Hang,
        ])],
        test_settings(),
        16,
    )?;
    let started = Instant::now();
    assert_eq!(next_event(&mut monitor).await?, StreamUpdate::Slot(9));
    assert_eq!(started.elapsed(), Duration::from_millis(4_500));
    assert_eq!(monitor.connect_count()?, 1, "no reconnection was needed");
    let _ = monitor.stop().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_hanging_connection_attempt_times_out() -> Result<(), Box<dyn Error>> {
    let mut monitor = start_monitor(
        vec![
            Script::HangConnect,
            Script::Stream(vec![Step::Update(StreamUpdate::Slot(3)), Step::Hang]),
        ],
        test_settings(),
        16,
    )?;
    assert_eq!(next_event(&mut monitor).await?, StreamUpdate::Slot(3));
    // 1 s connect timeout + 50 ms backoff.
    assert_eq!(monitor.connect_gaps_ms()?, vec![1_050]);
    let _ = monitor.stop().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn rejected_credentials_are_never_retried() -> Result<(), Box<dyn Error>> {
    for kind in [
        StreamErrorKind::Unauthenticated,
        StreamErrorKind::InvalidRequest,
    ] {
        let error = MonitorError::Connection { kind };
        let monitor = start_monitor(vec![Script::Refuse(error)], test_settings(), 16)?;
        let result = monitor.handle.await?;
        assert_eq!(result, Err(MonitorError::Connection { kind }));
        assert_eq!(monitor.connects.lock().map_err(|_| "poisoned")?.len(), 1);
    }
    // The same applies to a failure in the middle of a stream.
    let monitor = start_monitor(
        vec![Script::Stream(vec![Step::Fail(MonitorError::Connection {
            kind: StreamErrorKind::Unauthenticated,
        })])],
        test_settings(),
        16,
    )?;
    assert!(matches!(
        monitor.handle.await?,
        Err(MonitorError::Connection {
            kind: StreamErrorKind::Unauthenticated
        })
    ));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn gives_up_after_the_retry_budget() -> Result<(), Box<dyn Error>> {
    let settings = MonitorSettings {
        max_consecutive_failures: Some(2),
        ..test_settings()
    };
    let monitor = start_monitor(Vec::new(), settings, 16)?;
    let result = monitor.handle.await?;
    assert_eq!(
        result,
        Err(MonitorError::ReconnectExhausted { attempts: 3 })
    );
    assert_eq!(monitor.connects.lock().map_err(|_| "poisoned")?.len(), 3);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn an_unbounded_budget_keeps_trying() -> Result<(), Box<dyn Error>> {
    let settings = MonitorSettings {
        max_consecutive_failures: None,
        backoff: policy(50, 100, 0),
        ..test_settings()
    };
    let monitor = start_monitor(Vec::new(), settings, 16)?;
    sleep(Duration::from_secs(60)).await;
    assert!(!monitor.handle.is_finished());
    assert!(monitor.connect_count()? > 100);
    let _ = monitor.stop().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn stops_quietly_when_the_consumer_goes_away() -> Result<(), Box<dyn Error>> {
    let monitor = start_monitor(
        vec![Script::Stream(vec![
            Step::Update(StreamUpdate::Slot(1)),
            Step::Hang,
        ])],
        test_settings(),
        1,
    )?;
    let RunningMonitor { handle, events, .. } = monitor;
    drop(events);
    let status = handle.await??;
    assert_eq!(status.state, ConnectionState::Stopped);
    assert_eq!(status.events_forwarded, 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn shutdown_interrupts_a_backoff_wait() -> Result<(), Box<dyn Error>> {
    let settings = MonitorSettings {
        backoff: policy(5_000, 5_000, 0),
        ..test_settings()
    };
    let monitor = start_monitor(Vec::new(), settings, 16)?;
    sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    let status = monitor.stop().await?;
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(status.state, ConnectionState::Stopped);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn shutdown_interrupts_a_connection_attempt() -> Result<(), Box<dyn Error>> {
    let monitor = start_monitor(vec![Script::HangConnect], test_settings(), 16)?;
    sleep(Duration::from_millis(10)).await;
    let started = Instant::now();
    let status = monitor.stop().await?;
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(status.state, ConnectionState::Stopped);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn shutdown_is_not_held_up_by_a_stalled_consumer() -> Result<(), Box<dyn Error>> {
    // Capacity 1 and nobody reading: the second send blocks. Backpressure must
    // not turn into a deadlock.
    let monitor = start_monitor(
        vec![Script::Stream(vec![
            Step::Update(StreamUpdate::Slot(1)),
            Step::Update(StreamUpdate::Slot(2)),
            Step::Update(StreamUpdate::Slot(3)),
            Step::Hang,
        ])],
        test_settings(),
        1,
    )?;
    sleep(Duration::from_secs(1)).await;
    assert_eq!(monitor.health.borrow().events_forwarded, 1);
    let status = monitor.stop().await?;
    assert_eq!(status.state, ConnectionState::Stopped);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_stable_connection_resets_the_backoff() -> Result<(), Box<dyn Error>> {
    let settings = MonitorSettings {
        stable_after: Duration::from_secs(1),
        ..test_settings()
    };
    let monitor = start_monitor(
        vec![
            Script::Refuse(unreachable_error()),
            Script::Refuse(unreachable_error()),
            Script::Stream(vec![Step::Hang]),
            Script::Stream(vec![Step::Hang]),
        ],
        settings,
        16,
    )?;
    sleep(Duration::from_secs(6)).await;
    let gaps = monitor.connect_gaps_ms()?;
    // 50, 100 for the refusals; each silent-but-stable stream then fails after the
    // 2 s idle deadline and the delay starts over at 50 ms instead of 200 ms.
    assert_eq!(gaps.get(..4), Some([50, 100, 2_050, 2_050].as_slice()));
    let _ = monitor.stop().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn an_unstable_connection_keeps_growing_the_backoff() -> Result<(), Box<dyn Error>> {
    let monitor = start_monitor(
        vec![
            Script::Stream(vec![Step::Update(StreamUpdate::Slot(1)), Step::Close]),
            Script::Stream(vec![Step::Update(StreamUpdate::Slot(2)), Step::Close]),
            Script::Stream(vec![Step::Update(StreamUpdate::Slot(3)), Step::Close]),
            Script::Stream(vec![Step::Hang]),
        ],
        test_settings(),
        16,
    )?;
    sleep(Duration::from_secs(2)).await;
    // A server that accepts and immediately drops us must not be hammered.
    assert_eq!(
        monitor.connect_gaps_ms()?.get(..3),
        Some([50, 100, 200].as_slice())
    );
    let _ = monitor.stop().await?;
    Ok(())
}

#[test]
fn the_monitor_validates_its_inputs() {
    let transport = || ScriptedTransport {
        scripts: VecDeque::new(),
        connects: Arc::new(Mutex::new(Vec::new())),
    };
    let empty = SubscriptionRequest {
        accounts: Vec::new(),
        ..request()
    };
    assert!(matches!(
        GeyserMonitor::new(transport(), empty, test_settings()),
        Err(MonitorError::InvalidConfig { .. })
    ));
    let bad = MonitorSettings {
        idle_timeout: Duration::ZERO,
        ..test_settings()
    };
    assert!(GeyserMonitor::new(transport(), request(), bad).is_err());
    assert_eq!(StreamCommitment::default(), StreamCommitment::Processed);
}

#[tokio::test]
async fn shutdown_signals_are_level_triggered() {
    let (trigger, mut signal) = shutdown_channel();
    let mut clone: ShutdownSignal = signal.clone();
    assert!(!signal.is_triggered());
    trigger.trigger();
    trigger.trigger(); // idempotent
    assert!(signal.is_triggered());
    signal.wait().await;
    clone.wait().await; // already triggered: returns immediately
    let mut late = signal.clone();
    late.wait().await;

    // Dropping the trigger also releases waiters, so no task is orphaned.
    let (trigger, mut signal) = shutdown_channel();
    drop(trigger);
    assert!(signal.is_triggered());
    signal.wait().await;
}

// --- Exit plan -----------------------------------------------------------------------------------

fn tranche(at_ms: u64, fraction_bps: u16) -> Tranche {
    Tranche {
        at_ms,
        fraction_bps,
    }
}

fn invalid_plan(result: Result<ExitPlan, ExitError>) -> bool {
    matches!(result, Err(ExitError::InvalidPlan { .. }))
}

#[test]
fn exit_plan_validation_matrix() {
    assert_eq!(EXIT_WINDOW_START_MS, 1_000);
    assert_eq!(EXIT_WINDOW_END_MS, 10_000);
    assert_eq!(MAX_TRANCHES, 10);

    assert!(invalid_plan(ExitPlan::new(vec![], None)), "empty");
    let eleven: Vec<Tranche> = (0..11).map(|i| tranche(1_000 + i * 100, 100)).collect();
    assert!(invalid_plan(ExitPlan::new(eleven, None)), "too many");
    assert!(
        invalid_plan(ExitPlan::new(vec![tranche(999, 100)], None)),
        "before window"
    );
    assert!(
        invalid_plan(ExitPlan::new(vec![tranche(10_001, 100)], None)),
        "after window"
    );
    assert!(invalid_plan(ExitPlan::new(vec![tranche(0, 100)], None)));
    assert!(
        invalid_plan(ExitPlan::new(
            vec![tranche(2_000, 100), tranche(2_000, 100)],
            None
        )),
        "equal times"
    );
    assert!(
        invalid_plan(ExitPlan::new(
            vec![tranche(3_000, 100), tranche(2_000, 100)],
            None
        )),
        "decreasing times"
    );
    assert!(
        invalid_plan(ExitPlan::new(vec![tranche(2_000, 0)], None)),
        "zero fraction"
    );
    assert!(
        invalid_plan(ExitPlan::new(
            vec![tranche(2_000, 6_000), tranche(3_000, 4_001)],
            None
        )),
        "above 100 %"
    );
    for stop in [0u16, 10_000, u16::MAX] {
        assert!(
            invalid_plan(ExitPlan::new(vec![tranche(2_000, 100)], Some(stop))),
            "stop {stop}"
        );
    }

    assert!(ExitPlan::new(vec![tranche(1_000, 10_000)], None).is_ok());
    assert!(ExitPlan::new(vec![tranche(10_000, 1)], Some(9_999)).is_ok());
    assert!(ExitPlan::new(vec![tranche(2_000, 6_000), tranche(3_000, 4_000)], Some(1)).is_ok());
}

fn shape(plan: &ExitPlan) -> (Vec<u64>, Vec<u16>) {
    (
        plan.tranches().iter().map(|t| t.at_ms).collect(),
        plan.tranches().iter().map(|t| t.fraction_bps).collect(),
    )
}

#[test]
fn evenly_spaced_plans_cover_the_window() -> Result<(), Box<dyn Error>> {
    let four = ExitPlan::evenly_spaced(4, 10_000, None)?;
    assert_eq!(
        shape(&four),
        (vec![1_000, 4_000, 7_000, 10_000], vec![2_500; 4])
    );
    assert_eq!(four.total_bps(), 10_000);
    assert_eq!(four.stop_loss_bps(), None);

    // The remainder lands on the last tranche so the fractions add up exactly.
    let three = ExitPlan::evenly_spaced(3, 10_000, Some(1_500))?;
    assert_eq!(
        shape(&three),
        (vec![1_000, 5_500, 10_000], vec![3_333, 3_333, 3_334])
    );
    assert_eq!(three.total_bps(), 10_000);
    assert_eq!(three.stop_loss_bps(), Some(1_500));

    let single = ExitPlan::evenly_spaced(1, 10_000, None)?;
    assert_eq!(shape(&single), (vec![1_000], vec![10_000]));

    let half = ExitPlan::evenly_spaced(2, 5_000, None)?;
    assert_eq!(shape(&half), (vec![1_000, 10_000], vec![2_500, 2_500]));

    let ten = ExitPlan::evenly_spaced(10, 10_000, None)?;
    let (times, fractions) = shape(&ten);
    assert_eq!(times, (1..=10).map(|n| n * 1_000).collect::<Vec<u64>>());
    assert_eq!(fractions, vec![1_000; 10]);

    for count in 1..=10u8 {
        for total in [1_000u16, 3_333, 9_999, 10_000] {
            let plan = ExitPlan::evenly_spaced(count, total, None)?;
            assert_eq!(plan.total_bps(), total, "count {count}, total {total}");
            assert_eq!(plan.tranches().len(), usize::from(count));
        }
    }
    Ok(())
}

#[test]
fn evenly_spaced_rejects_degenerate_inputs() {
    assert!(invalid_plan(ExitPlan::evenly_spaced(0, 10_000, None)));
    assert!(invalid_plan(ExitPlan::evenly_spaced(11, 10_000, None)));
    assert!(invalid_plan(ExitPlan::evenly_spaced(4, 0, None)));
    assert!(invalid_plan(ExitPlan::evenly_spaced(4, 10_001, None)));
    // Ten tranches need at least ten basis points between them.
    assert!(invalid_plan(ExitPlan::evenly_spaced(10, 9, None)));
    assert!(invalid_plan(ExitPlan::evenly_spaced(4, 10_000, Some(0))));
}

// --- Exit engine ------------------------------------------------------------------------------------

const POSITION: u64 = 97_500_000_000_003;
const TRANCHE_TOKENS: u64 = 24_375_000_000_000;

fn exit_settings() -> ExitSettings {
    ExitSettings {
        fee_bps: DEFAULT_TOTAL_FEE_BPS,
        slippage_bps: 500,
        order_cost_lamports: 125_000,
        max_total_cost_lamports: 5_000_000,
    }
}

fn position() -> Result<Position, Box<dyn Error>> {
    Ok(Position {
        tokens: POSITION,
        entry: PriceReference::after_buy(&default_curve(), &launch_buy()?)?,
    })
}

fn engine_with(plan: ExitPlan, settings: ExitSettings) -> Result<ExitEngine, Box<dyn Error>> {
    Ok(ExitEngine::new(plan, settings, position()?)?)
}

fn four_tranches(stop_loss: Option<u16>) -> Result<ExitPlan, Box<dyn Error>> {
    Ok(ExitPlan::evenly_spaced(4, 10_000, stop_loss)?)
}

fn engine() -> Result<ExitEngine, Box<dyn Error>> {
    engine_with(four_tranches(None)?, exit_settings())
}

/// The curve as the stream reports it right after the creator's buy.
fn fresh_observation(slot: u64) -> Result<CurveObservation, Box<dyn Error>> {
    observation(slot, &post_buy_spec()?)
}

fn dropped_matches(
    result: &krypton::monitor::exit::PollResult,
    check: impl Fn(&ExitError) -> bool,
) -> bool {
    !result.dropped.is_empty() && result.dropped.iter().all(|dropped| check(&dropped.error))
}

#[test]
fn engine_rejects_invalid_settings_and_empty_positions() -> Result<(), Box<dyn Error>> {
    let plan = four_tranches(None)?;
    let too_wide = ExitSettings {
        slippage_bps: MAX_SLIPPAGE_BPS + 1,
        ..exit_settings()
    };
    assert!(matches!(
        ExitEngine::new(plan.clone(), too_wide, position()?),
        Err(ExitError::InvalidConfig { .. })
    ));
    let too_costly = ExitSettings {
        fee_bps: MAX_FEE_BPS + 1,
        ..exit_settings()
    };
    assert!(matches!(
        ExitEngine::new(plan.clone(), too_costly, position()?),
        Err(ExitError::InvalidConfig { .. })
    ));
    let at_cap = ExitSettings {
        slippage_bps: MAX_SLIPPAGE_BPS,
        ..exit_settings()
    };
    assert!(ExitEngine::new(plan.clone(), at_cap, position()?).is_ok());
    let empty = Position {
        tokens: 0,
        ..position()?
    };
    assert!(matches!(
        ExitEngine::new(plan, exit_settings(), empty),
        Err(ExitError::InvalidConfig { .. })
    ));
    Ok(())
}

#[test]
fn nothing_is_sold_before_the_window_opens() -> Result<(), Box<dyn Error>> {
    let mut engine = engine()?;
    assert!(engine.observe(fresh_observation(10)?));
    for elapsed in [0, 1, 500, 999] {
        let result = engine.poll(elapsed);
        assert!(
            result.orders.is_empty() && result.dropped.is_empty(),
            "{elapsed}"
        );
    }
    assert!(engine.has_pending());
    assert_eq!(engine.snapshot().orders_issued, 0);
    Ok(())
}

#[test]
fn a_due_tranche_waits_for_the_first_observation() -> Result<(), Box<dyn Error>> {
    let mut engine = engine()?;
    let result = engine.poll(1_000);
    assert!(result.orders.is_empty() && result.dropped.is_empty());
    assert!(engine.has_pending());

    assert!(engine.observe(fresh_observation(10)?));
    let result = engine.poll(1_100);
    assert_eq!(result.orders.len(), 1);
    let order = result.orders.first().ok_or("order")?;
    assert_eq!(order.trigger, ExitTrigger::Tranche { index: 0 });
    assert_eq!(order.issued_at_ms, 1_100);
    assert_eq!(order.curve_slot, 10);
    assert_eq!(order.cost_lamports, 125_000);
    Ok(())
}

#[test]
fn the_schedule_sells_exactly_the_position_with_a_dust_sweep() -> Result<(), Box<dyn Error>> {
    let mut engine = engine()?;
    assert!(engine.observe(fresh_observation(10)?));
    let mut orders = Vec::new();
    for elapsed in [1_000, 4_000, 7_000, 10_000] {
        let result = engine.poll(elapsed);
        assert!(result.dropped.is_empty());
        assert_eq!(result.orders.len(), 1, "at {elapsed}");
        orders.extend(result.orders);
    }
    let tokens: Vec<u64> = orders.iter().map(|order| order.plan.token_amount).collect();
    assert_eq!(
        tokens,
        vec![
            TRANCHE_TOKENS,
            TRANCHE_TOKENS,
            TRANCHE_TOKENS,
            TRANCHE_TOKENS + 3
        ],
        "the last tranche takes the rounding dust"
    );
    assert_eq!(tokens.iter().sum::<u64>(), POSITION);
    let sequences: Vec<u8> = orders.iter().map(|order| order.sequence).collect();
    assert_eq!(sequences, vec![0, 1, 2, 3]);
    for order in &orders {
        // Slippage guard: the cap of audit finding #20 holds on every order.
        assert!(order.plan.slippage_bps <= MAX_SLIPPAGE_BPS);
        assert!(order.plan.min_sol_output_lamports > 0);
        assert!(order.plan.min_sol_output_lamports <= order.plan.net_proceeds_lamports);
    }
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.orders_issued, 4);
    assert_eq!(snapshot.tokens_in_flight, POSITION);
    assert_eq!(snapshot.tokens_unsold, 0);
    assert_eq!(snapshot.committed_cost_lamports, 500_000);
    assert!(!engine.has_pending());
    assert!(engine.has_unreported());
    Ok(())
}

#[test]
fn tranches_are_quoted_against_the_impact_of_earlier_ones() -> Result<(), Box<dyn Error>> {
    let mut engine = engine()?;
    let observed = fresh_observation(10)?;
    assert!(engine.observe(observed));
    let first = engine.poll(1_000).orders.first().copied().ok_or("first")?;
    let second = engine.poll(4_000).orders.first().copied().ok_or("second")?;

    // The first order is quoted on the observation itself.
    let curve = observed.state.curve_params();
    let expected_first = SellPlan::from_token_amount(&curve, TRANCHE_TOKENS, 125, 500)?;
    assert_eq!(first.plan, expected_first);

    // The second is quoted on the curve *after* the first one executed, even
    // though the stream has not shown that yet.
    let after_first = first.plan.apply_to(&curve)?;
    let expected_second = SellPlan::from_token_amount(&after_first, TRANCHE_TOKENS, 125, 500)?;
    assert_eq!(second.plan, expected_second);
    assert!(second.plan.min_sol_output_lamports < first.plan.min_sol_output_lamports);
    Ok(())
}

#[test]
fn landed_sales_leave_the_projection_once_the_stream_reflects_them() -> Result<(), Box<dyn Error>> {
    let mut engine = engine()?;
    let observed = fresh_observation(100)?;
    let curve = observed.state.curve_params();
    assert!(engine.observe(observed));

    let first = engine.poll(1_000).orders.first().copied().ok_or("first")?;
    engine.report(SellReport {
        sequence: first.sequence,
        outcome: SellOutcome::Landed { slot: 101 },
    });
    assert!(!engine.has_unreported());
    assert_eq!(engine.snapshot().tokens_sold, TRANCHE_TOKENS);

    // The landing slot is not on the stream yet: the sale is still projected.
    let second = engine.poll(4_000).orders.first().copied().ok_or("second")?;
    let after_first = first.plan.apply_to(&curve)?;
    assert_eq!(
        second.plan,
        SellPlan::from_token_amount(&after_first, TRANCHE_TOKENS, 125, 500)?
    );

    // Slot 101 arrives and already contains the first sale: it must not be
    // counted twice, but the second sale (unreported) still is.
    let reflected = CurveObservation {
        slot: 101,
        write_version: 101,
        state: observed.state.with_params(&after_first),
    };
    assert!(engine.observe(reflected));
    let third = engine.poll(7_000).orders.first().copied().ok_or("third")?;
    let after_second = second.plan.apply_to(&after_first)?;
    assert_eq!(
        third.plan,
        SellPlan::from_token_amount(&after_second, TRANCHE_TOKENS, 125, 500)?
    );
    Ok(())
}

#[test]
fn a_landing_already_on_the_stream_is_dropped_from_the_projection_at_once(
) -> Result<(), Box<dyn Error>> {
    let mut engine = engine()?;
    let observed = fresh_observation(100)?;
    assert!(engine.observe(observed));
    let first = engine.poll(1_000).orders.first().copied().ok_or("first")?;
    let after_first = first.plan.apply_to(&observed.state.curve_params())?;
    let reflected = CurveObservation {
        slot: 105,
        write_version: 105,
        state: observed.state.with_params(&after_first),
    };
    assert!(engine.observe(reflected));
    engine.report(SellReport {
        sequence: first.sequence,
        outcome: SellOutcome::Landed { slot: 102 },
    });
    let second = engine.poll(4_000).orders.first().copied().ok_or("second")?;
    assert_eq!(
        second.plan,
        SellPlan::from_token_amount(&after_first, TRANCHE_TOKENS, 125, 500)?
    );
    Ok(())
}

#[test]
fn failed_orders_are_carried_into_the_next_tranche() -> Result<(), Box<dyn Error>> {
    let mut engine = engine()?;
    assert!(engine.observe(fresh_observation(10)?));
    let first = engine.poll(1_000).orders.first().copied().ok_or("first")?;
    engine.report(SellReport {
        sequence: first.sequence,
        outcome: SellOutcome::Failed,
    });
    assert_eq!(engine.snapshot().tokens_unsold, POSITION);
    assert_eq!(engine.snapshot().tokens_in_flight, 0);

    let second = engine.poll(4_000).orders.first().copied().ok_or("second")?;
    assert_eq!(second.plan.token_amount, 2 * TRANCHE_TOKENS);
    // The failed order left no phantom impact on the quote.
    let curve = fresh_observation(10)?.state.curve_params();
    assert_eq!(
        second.plan,
        SellPlan::from_token_amount(&curve, 2 * TRANCHE_TOKENS, 125, 500)?
    );

    let third = engine.poll(7_000).orders.first().copied().ok_or("third")?;
    assert_eq!(third.plan.token_amount, TRANCHE_TOKENS);
    let fourth = engine
        .poll(10_000)
        .orders
        .first()
        .copied()
        .ok_or("fourth")?;
    assert_eq!(fourth.plan.token_amount, TRANCHE_TOKENS + 3);
    assert_eq!(engine.snapshot().tokens_unsold, 0);
    assert_eq!(
        second.plan.token_amount + third.plan.token_amount + fourth.plan.token_amount,
        POSITION
    );
    Ok(())
}

#[test]
fn reports_are_counted_once_and_unknown_sequences_are_ignored() -> Result<(), Box<dyn Error>> {
    let mut engine = engine()?;
    assert!(engine.observe(fresh_observation(10)?));
    let first = engine.poll(1_000).orders.first().copied().ok_or("first")?;
    let landed = SellReport {
        sequence: first.sequence,
        outcome: SellOutcome::Landed { slot: 50 },
    };
    engine.report(landed);
    engine.report(landed);
    engine.report(SellReport {
        sequence: first.sequence,
        outcome: SellOutcome::Failed,
    });
    engine.report(SellReport {
        sequence: 99,
        outcome: SellOutcome::Failed,
    });
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.tokens_sold, TRANCHE_TOKENS);
    assert_eq!(snapshot.tokens_unsold, POSITION - TRANCHE_TOKENS);
    Ok(())
}

#[test]
fn simulated_orders_count_as_sold_without_projection() -> Result<(), Box<dyn Error>> {
    let mut engine = engine()?;
    let observed = fresh_observation(10)?;
    assert!(engine.observe(observed));
    let first = engine.poll(1_000).orders.first().copied().ok_or("first")?;
    engine.report(SellReport {
        sequence: first.sequence,
        outcome: SellOutcome::Simulated,
    });
    assert_eq!(engine.snapshot().tokens_sold, TRANCHE_TOKENS);
    // Dry-run sells never touch the curve, so later quotes use it unmodified.
    let second = engine.poll(4_000).orders.first().copied().ok_or("second")?;
    assert_eq!(
        second.plan,
        SellPlan::from_token_amount(&observed.state.curve_params(), TRANCHE_TOKENS, 125, 500)?
    );
    Ok(())
}

#[test]
fn a_tranche_that_comes_due_late_is_dropped_and_its_share_carried() -> Result<(), Box<dyn Error>> {
    assert_eq!(MAX_LATENESS_MS, 500);
    let mut engine = engine()?;
    // No observation: tranche 0 waits, then misses the window.
    assert!(engine.poll(1_499).dropped.is_empty());
    let result = engine.poll(1_500);
    assert!(result.orders.is_empty());
    assert!(dropped_matches(&result, |error| matches!(
        error,
        ExitError::WindowMissed { tranche: 0 }
    )));
    assert_eq!(engine.snapshot().dropped, 1);

    assert!(engine.observe(fresh_observation(10)?));
    let second = engine.poll(4_000).orders.first().copied().ok_or("second")?;
    assert_eq!(second.plan.token_amount, 2 * TRANCHE_TOKENS);
    Ok(())
}

#[test]
fn nothing_is_issued_after_the_window_closes() -> Result<(), Box<dyn Error>> {
    let mut engine = engine()?;
    assert!(engine.observe(fresh_observation(10)?));
    // The last instant a tranche scheduled at 10 s may still run: the three
    // earlier tranches have expired by then, the final one has not.
    let mut late = engine_with(four_tranches(None)?, exit_settings())?;
    assert!(late.observe(fresh_observation(10)?));
    let last_moment = late.poll(10_499);
    assert_eq!(last_moment.orders.len(), 1);
    assert_eq!(last_moment.dropped.len(), 3);

    // One millisecond later everything that is left has expired.
    let result = engine.poll(10_500);
    assert!(result.orders.is_empty());
    assert_eq!(result.dropped.len(), 4);
    assert!(dropped_matches(&result, |error| matches!(
        error,
        ExitError::WindowMissed { .. }
    )));
    assert_eq!(engine.snapshot().tokens_unsold, POSITION);
    assert!(!engine.has_pending());
    Ok(())
}

#[test]
fn a_completed_curve_cancels_the_exit() -> Result<(), Box<dyn Error>> {
    let mut engine = engine()?;
    let complete = CurveSpec {
        complete: true,
        ..post_buy_spec()?
    };
    assert!(engine.observe(observation(10, &complete)?));
    let result = engine.poll(1_000);
    assert!(result.orders.is_empty());
    assert!(dropped_matches(&result, |error| matches!(
        error,
        ExitError::CurveComplete
    )));
    assert_eq!(engine.snapshot().orders_issued, 0);
    Ok(())
}

#[test]
fn cashback_coins_are_refused() -> Result<(), Box<dyn Error>> {
    let mut engine = engine()?;
    let cashback = CurveSpec {
        cashback: true,
        ..post_buy_spec()?
    };
    assert!(engine.observe(observation(10, &cashback)?));
    let result = engine.poll(1_000);
    assert!(result.orders.is_empty());
    assert!(dropped_matches(&result, |error| matches!(
        error,
        ExitError::CashbackCoinUnsupported
    )));
    Ok(())
}

#[test]
fn stale_and_replayed_observations_are_ignored() -> Result<(), Box<dyn Error>> {
    let mut engine = engine()?;
    let base = fresh_observation(10)?;
    assert!(engine.observe(base));
    assert!(!engine.observe(fresh_observation(9)?), "older slot");
    assert!(!engine.observe(base), "exact replay");
    let same_slot_newer_write = CurveObservation {
        write_version: base.write_version + 1,
        ..base
    };
    assert!(engine.observe(same_slot_newer_write));
    assert!(engine.observe(fresh_observation(11)?));
    Ok(())
}

#[test]
fn the_engine_reports_when_it_needs_to_wake_up() -> Result<(), Box<dyn Error>> {
    let mut engine = engine()?;
    assert_eq!(engine.next_wakeup_ms(0), Some(1_000));
    assert_eq!(engine.next_wakeup_ms(999), Some(1_000));
    // Due but unquotable: wake at the lateness limit.
    assert_eq!(engine.next_wakeup_ms(1_000), Some(1_500));
    assert!(engine.observe(fresh_observation(10)?));
    let _ = engine.poll(1_000);
    assert_eq!(engine.next_wakeup_ms(1_000), Some(4_000));
    let _ = engine.poll(4_000);
    let _ = engine.poll(7_000);
    let _ = engine.poll(10_000);
    assert_eq!(engine.next_wakeup_ms(10_000), None);
    Ok(())
}

#[test]
fn a_partial_plan_keeps_the_rest_of_the_position() -> Result<(), Box<dyn Error>> {
    let plan = ExitPlan::new(vec![tranche(1_000, 3_000), tranche(5_000, 2_000)], None)?;
    let mut engine = engine_with(plan, exit_settings())?;
    assert!(engine.observe(fresh_observation(10)?));
    let first = engine.poll(1_000).orders.first().copied().ok_or("first")?;
    let second = engine.poll(5_000).orders.first().copied().ok_or("second")?;
    // floor(97_500_000_000_003 x 30 %) and floor(... x 20 %): no dust sweep.
    assert_eq!(first.plan.token_amount, 29_250_000_000_000);
    assert_eq!(second.plan.token_amount, 19_500_000_000_000);
    let snapshot = engine.snapshot();
    assert_eq!(
        snapshot.tokens_unsold,
        POSITION - 29_250_000_000_000 - 19_500_000_000_000
    );
    Ok(())
}

#[test]
fn the_cost_budget_stops_further_orders() -> Result<(), Box<dyn Error>> {
    let settings = ExitSettings {
        max_total_cost_lamports: 250_001,
        ..exit_settings()
    };
    let mut engine = engine_with(four_tranches(None)?, settings)?;
    assert!(engine.observe(fresh_observation(10)?));
    assert_eq!(engine.poll(1_000).orders.len(), 1);
    assert_eq!(engine.poll(4_000).orders.len(), 1);
    let third = engine.poll(7_000);
    assert!(third.orders.is_empty());
    assert!(dropped_matches(&third, |error| matches!(
        error,
        ExitError::BudgetExceeded {
            cost: 125_000,
            remaining: 1
        }
    )));
    let fourth = engine.poll(10_000);
    assert!(fourth.orders.is_empty());
    assert_eq!(engine.snapshot().committed_cost_lamports, 250_000);
    assert_eq!(engine.snapshot().orders_issued, 2);
    Ok(())
}

#[test]
fn dust_positions_never_produce_orders() -> Result<(), Box<dyn Error>> {
    let dust = Position {
        tokens: 3,
        ..position()?
    };
    let mut engine = ExitEngine::new(four_tranches(None)?, exit_settings(), dust)?;
    assert!(engine.observe(fresh_observation(10)?));
    let mut dropped = 0;
    for elapsed in [1_000, 4_000, 7_000, 10_000] {
        let result = engine.poll(elapsed);
        assert!(result.orders.is_empty());
        dropped += result.dropped.len();
    }
    assert_eq!(dropped, 4);
    assert_eq!(engine.snapshot().tokens_unsold, 3);
    Ok(())
}

// --- Stop-loss ---------------------------------------------------------------------------------------

/// A curve whose price is `percent` % of the creator's entry price.
fn crashed_spec(percent: u64) -> Result<CurveSpec, Box<dyn Error>> {
    let base = post_buy_spec()?;
    Ok(CurveSpec {
        virtual_sol: base.virtual_sol * percent / 100,
        ..base
    })
}

#[test]
fn a_price_crash_sells_everything_left_at_once() -> Result<(), Box<dyn Error>> {
    let mut engine = engine_with(four_tranches(Some(2_000))?, exit_settings())?;
    // A 10 % drop is inside the 20 % tolerance.
    assert!(engine.observe(observation(10, &crashed_spec(90)?)?));
    assert_eq!(
        engine.poll(1_000).orders.first().map(|o| o.trigger),
        Some(ExitTrigger::Tranche { index: 0 })
    );

    // A 40 % drop trips the stop: the remaining position goes out in one order.
    assert!(engine.observe(observation(11, &crashed_spec(60)?)?));
    let result = engine.poll(1_200);
    assert_eq!(result.orders.len(), 1);
    let sweep = result.orders.first().copied().ok_or("sweep")?;
    assert_eq!(sweep.trigger, ExitTrigger::StopLoss);
    assert_eq!(sweep.plan.token_amount, POSITION - TRANCHE_TOKENS);
    assert!(sweep.plan.slippage_bps <= MAX_SLIPPAGE_BPS);
    let snapshot = engine.snapshot();
    assert!(snapshot.stop_loss_fired);
    assert_eq!(snapshot.tokens_unsold, 0);

    // The scheduled tranches were superseded.
    assert!(!engine.has_pending());
    for elapsed in [4_000, 7_000, 10_000] {
        assert!(engine.poll(elapsed).orders.is_empty());
    }
    Ok(())
}

#[test]
fn the_stop_loss_waits_for_the_window_and_respects_its_end() -> Result<(), Box<dyn Error>> {
    let mut engine = engine_with(four_tranches(Some(2_000))?, exit_settings())?;
    assert!(engine.observe(observation(10, &crashed_spec(50)?)?));
    assert!(engine.poll(500).orders.is_empty(), "window not open yet");
    assert!(engine.poll(999).orders.is_empty());

    let mut late = engine_with(four_tranches(Some(2_000))?, exit_settings())?;
    assert!(late.observe(observation(10, &crashed_spec(50)?)?));
    let result = late.poll(10_500);
    assert!(result.orders.is_empty(), "window closed");
    assert!(!late.snapshot().stop_loss_fired);
    Ok(())
}

#[test]
fn without_a_stop_loss_a_crash_does_not_change_the_schedule() -> Result<(), Box<dyn Error>> {
    let mut engine = engine()?;
    assert!(engine.observe(observation(10, &crashed_spec(10)?)?));
    let result = engine.poll(1_000);
    assert_eq!(
        result.orders.first().map(|order| order.trigger),
        Some(ExitTrigger::Tranche { index: 0 })
    );
    assert!(!engine.snapshot().stop_loss_fired);
    Ok(())
}

#[test]
fn a_failed_stop_loss_is_retried_a_bounded_number_of_times() -> Result<(), Box<dyn Error>> {
    assert_eq!(MAX_STOP_LOSS_ORDERS, 3);
    let mut engine = engine_with(four_tranches(Some(2_000))?, exit_settings())?;
    assert!(engine.observe(observation(10, &crashed_spec(50)?)?));
    for attempt in 0..MAX_STOP_LOSS_ORDERS {
        let result = engine.poll(1_000 + u64::from(attempt));
        let order = result.orders.first().copied().ok_or("stop-loss order")?;
        assert_eq!(order.trigger, ExitTrigger::StopLoss);
        assert_eq!(order.plan.token_amount, POSITION);
        engine.report(SellReport {
            sequence: order.sequence,
            outcome: SellOutcome::Failed,
        });
    }
    // Out of attempts: no more orders, the position stays unsold.
    assert!(engine.poll(1_100).orders.is_empty());
    assert_eq!(engine.snapshot().tokens_unsold, POSITION);

    // Once fired, retries do not depend on the price any more.
    let mut recovering = engine_with(four_tranches(Some(2_000))?, exit_settings())?;
    assert!(recovering.observe(observation(10, &crashed_spec(50)?)?));
    let first = recovering
        .poll(1_000)
        .orders
        .first()
        .copied()
        .ok_or("first")?;
    assert!(recovering.observe(observation(11, &post_buy_spec()?)?));
    recovering.report(SellReport {
        sequence: first.sequence,
        outcome: SellOutcome::Failed,
    });
    let retry = recovering
        .poll(1_500)
        .orders
        .first()
        .copied()
        .ok_or("retry")?;
    assert_eq!(retry.trigger, ExitTrigger::StopLoss);
    Ok(())
}

#[test]
fn the_stop_loss_is_not_attempted_on_a_completed_curve() -> Result<(), Box<dyn Error>> {
    let mut engine = engine_with(four_tranches(Some(2_000))?, exit_settings())?;
    let complete = CurveSpec {
        complete: true,
        ..crashed_spec(50)?
    };
    assert!(engine.observe(observation(10, &complete)?));
    let result = engine.poll(1_000);
    assert!(result.orders.is_empty());
    assert!(!engine.snapshot().stop_loss_fired);
    Ok(())
}

// --- Exit manager (async) ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Seen {
    sequence: u8,
    at_ms: u128,
    tokens: u64,
    min_out: u64,
    trigger: ExitTrigger,
}

/// A submitter that records every order with the time it arrived and answers
/// from a script (`None` = the submission errors out).
struct Recorder {
    genesis: Instant,
    seen: Mutex<Vec<Seen>>,
    script: Mutex<VecDeque<Option<SellOutcome>>>,
    delay: Duration,
}

impl Recorder {
    fn new(genesis: Instant, script: Vec<Option<SellOutcome>>, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            genesis,
            seen: Mutex::new(Vec::new()),
            script: Mutex::new(script.into()),
            delay,
        })
    }

    fn seen(&self) -> Result<Vec<Seen>, Box<dyn Error>> {
        Ok(self.seen.lock().map_err(|_| "poisoned")?.clone())
    }
}

impl SellSubmitter for Recorder {
    async fn submit(&self, order: &SellOrder) -> Result<SellOutcome, ExitError> {
        let at_ms = Instant::now().duration_since(self.genesis).as_millis();
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(Seen {
                sequence: order.sequence,
                at_ms,
                tokens: order.plan.token_amount,
                min_out: order.plan.min_sol_output_lamports,
                trigger: order.trigger,
            });
        }
        if !self.delay.is_zero() {
            sleep(self.delay).await;
        }
        let scripted = self
            .script
            .lock()
            .ok()
            .and_then(|mut script| script.pop_front());
        match scripted {
            Some(Some(outcome)) => Ok(outcome),
            Some(None) => Err(ExitError::NoCurveState),
            None => Ok(SellOutcome::Landed {
                slot: order.curve_slot + 1,
            }),
        }
    }
}

struct ExitRun {
    updates: mpsc::Sender<StreamUpdate>,
    manager: tokio::task::JoinHandle<Result<krypton::monitor::exit::ExitSummary, ExitError>>,
    executor: tokio::task::JoinHandle<()>,
    recorder: Arc<Recorder>,
    trigger: krypton::monitor::ShutdownTrigger,
    genesis: Instant,
}

fn start_exit(
    plan: ExitPlan,
    script: Vec<Option<SellOutcome>>,
    delay: Duration,
    update_capacity: usize,
) -> Result<ExitRun, Box<dyn Error>> {
    let genesis = Instant::now();
    let engine = engine_with(plan, exit_settings())?;
    let manager =
        ExitManager::new(engine, curve_key(), genesis).with_settle_timeout(Duration::from_secs(5));
    let recorder = Recorder::new(genesis, script, delay);
    let (orders_tx, orders_rx) = order_channel();
    let (reports_tx, reports_rx) = report_channel();
    let (updates, updates_rx) = mpsc::channel(update_capacity);
    let (trigger, signal) = shutdown_channel();
    let executor = tokio::spawn(run_sell_executor(
        Arc::clone(&recorder),
        orders_rx,
        reports_tx,
        signal.clone(),
        Duration::from_secs(5),
    ));
    let manager = tokio::spawn(manager.run(updates_rx, orders_tx, reports_rx, signal));
    Ok(ExitRun {
        updates,
        manager,
        executor,
        recorder,
        trigger,
        genesis,
    })
}

impl ExitRun {
    async fn finish(self) -> Result<krypton::monitor::exit::ExitSummary, Box<dyn Error>> {
        let summary = self.manager.await??;
        self.trigger.trigger();
        self.executor.await?;
        Ok(summary)
    }
}

#[test]
fn channel_capacity_always_exceeds_the_orders_one_engine_can_issue() {
    assert!(ORDER_CHANNEL_CAPACITY > MAX_TRANCHES + usize::from(MAX_STOP_LOSS_ORDERS));
    let (orders, _receiver) = order_channel();
    assert_eq!(orders.capacity(), ORDER_CHANNEL_CAPACITY);
}

#[tokio::test(start_paused = true)]
async fn orders_follow_the_one_to_ten_second_schedule() -> Result<(), Box<dyn Error>> {
    let run = start_exit(four_tranches(None)?, vec![], Duration::ZERO, 16)?;
    run.updates
        .send(curve_update(10, &post_buy_spec()?))
        .await?;
    let recorder = Arc::clone(&run.recorder);
    let genesis = run.genesis;
    let summary = run.finish().await?;

    let seen = recorder.seen()?;
    let times: Vec<u128> = seen.iter().map(|entry| entry.at_ms).collect();
    assert_eq!(times, vec![1_000, 4_000, 7_000, 10_000]);
    let sequences: Vec<u8> = seen.iter().map(|entry| entry.sequence).collect();
    assert_eq!(sequences, vec![0, 1, 2, 3]);
    let tokens: Vec<u64> = seen.iter().map(|entry| entry.tokens).collect();
    assert_eq!(tokens.iter().sum::<u64>(), POSITION);
    assert_eq!(tokens.first(), Some(&TRANCHE_TOKENS));
    assert_eq!(tokens.last(), Some(&(TRANCHE_TOKENS + 3)));
    assert!(seen
        .iter()
        .all(|entry| matches!(entry.trigger, ExitTrigger::Tranche { .. })));

    assert!(!summary.interrupted && !summary.unsettled);
    assert_eq!(summary.observations, 1);
    assert_eq!(summary.decode_errors, 0);
    assert_eq!(summary.position.orders_issued, 4);
    assert_eq!(summary.position.tokens_sold, POSITION);
    assert_eq!(summary.position.tokens_unsold, 0);
    // Done as soon as the last order was reported.
    assert_eq!(genesis.elapsed(), Duration::from_secs(10));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_fresh_curve_update_changes_the_next_quote() -> Result<(), Box<dyn Error>> {
    let run = start_exit(four_tranches(None)?, vec![], Duration::ZERO, 16)?;
    run.updates
        .send(curve_update(10, &post_buy_spec()?))
        .await?;
    sleep(Duration::from_millis(2_500)).await;
    // Someone dumped: the price fell 30 % before the second tranche.
    let crashed = crashed_spec(70)?;
    run.updates.send(curve_update(20, &crashed)).await?;
    let recorder = Arc::clone(&run.recorder);
    let _ = run.finish().await?;

    let seen = recorder.seen()?;
    let first = seen.first().ok_or("first")?;
    let second = seen.get(1).ok_or("second")?;
    assert_eq!(first.tokens, second.tokens);
    // The first sale landed in slot 11, before the crashed observation (slot 20),
    // so the new reserves already include it and it is not projected again.
    let first_plan = SellPlan::from_token_amount(
        &post_buy_params()?,
        TRANCHE_TOKENS,
        exit_settings().fee_bps,
        exit_settings().slippage_bps,
    )?;
    assert_eq!(first.min_out, first_plan.min_sol_output_lamports);
    let crashed_curve = BondingCurveState::decode(&crashed.bytes())?.curve_params();
    let expected = SellPlan::from_token_amount(
        &crashed_curve,
        TRANCHE_TOKENS,
        exit_settings().fee_bps,
        exit_settings().slippage_bps,
    )?;
    assert_eq!(second.min_out, expected.min_sol_output_lamports);
    assert!(second.min_out < first.min_out * 8 / 10);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn undecodable_and_foreign_updates_are_ignored() -> Result<(), Box<dyn Error>> {
    let run = start_exit(four_tranches(None)?, vec![], Duration::ZERO, 16)?;
    let spec = post_buy_spec()?;
    // Garbage under the watched key, a valid curve under another key, and noise.
    run.updates
        .send(StreamUpdate::Account(account_update(
            5,
            5,
            curve_key(),
            vec![1, 2, 3],
        )))
        .await?;
    run.updates
        .send(StreamUpdate::Account(account_update(
            6,
            6,
            key(1),
            spec.bytes(),
        )))
        .await?;
    run.updates.send(StreamUpdate::Slot(7)).await?;
    run.updates.send(StreamUpdate::Heartbeat).await?;
    run.updates
        .send(StreamUpdate::Instruction(instruction_update(
            DISCRIMINATOR_SELL_V3.to_vec(),
        )))
        .await?;
    run.updates.send(curve_update(10, &spec)).await?;
    let summary = run.finish().await?;
    assert_eq!(summary.decode_errors, 1);
    assert_eq!(summary.observations, 1);
    assert_eq!(summary.position.orders_issued, 4);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_late_first_observation_still_triggers_the_waiting_tranche() -> Result<(), Box<dyn Error>>
{
    let run = start_exit(four_tranches(None)?, vec![], Duration::ZERO, 16)?;
    sleep(Duration::from_millis(1_200)).await;
    run.updates
        .send(curve_update(10, &post_buy_spec()?))
        .await?;
    let recorder = Arc::clone(&run.recorder);
    let summary = run.finish().await?;
    let seen = recorder.seen()?;
    assert_eq!(seen.first().map(|entry| entry.at_ms), Some(1_200));
    assert_eq!(summary.position.orders_issued, 4);
    assert_eq!(summary.position.dropped, 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn without_any_observation_every_tranche_misses_the_window() -> Result<(), Box<dyn Error>> {
    let run = start_exit(four_tranches(None)?, vec![], Duration::ZERO, 16)?;
    let genesis = run.genesis;
    let recorder = Arc::clone(&run.recorder);
    let summary = run.finish().await?;
    assert!(recorder.seen()?.is_empty());
    assert_eq!(summary.position.orders_issued, 0);
    assert_eq!(summary.position.dropped, 4);
    assert_eq!(summary.position.tokens_unsold, POSITION);
    // The manager gave up at the last tranche's lateness limit, not later.
    assert_eq!(genesis.elapsed(), Duration::from_millis(10_500));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn the_schedule_survives_the_stream_closing() -> Result<(), Box<dyn Error>> {
    let run = start_exit(four_tranches(None)?, vec![], Duration::ZERO, 16)?;
    run.updates
        .send(curve_update(10, &post_buy_spec()?))
        .await?;
    let ExitRun {
        updates,
        manager,
        executor,
        recorder,
        trigger,
        ..
    } = run;
    drop(updates); // the monitor died; the timers keep running on the last state
    let summary = manager.await??;
    trigger.trigger();
    executor.await?;
    assert_eq!(recorder.seen()?.len(), 4);
    assert_eq!(summary.position.tokens_sold, POSITION);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_failed_submission_is_carried_into_the_next_tranche() -> Result<(), Box<dyn Error>> {
    // First order errors out, everything else lands.
    let run = start_exit(four_tranches(None)?, vec![None], Duration::ZERO, 16)?;
    run.updates
        .send(curve_update(10, &post_buy_spec()?))
        .await?;
    let recorder = Arc::clone(&run.recorder);
    let summary = run.finish().await?;
    let seen = recorder.seen()?;
    let tokens: Vec<u64> = seen.iter().map(|entry| entry.tokens).collect();
    assert_eq!(
        tokens,
        vec![
            TRANCHE_TOKENS,
            2 * TRANCHE_TOKENS,
            TRANCHE_TOKENS,
            TRANCHE_TOKENS + 3
        ]
    );
    // The failed share was re-offered to tranche 1, so nothing is left over.
    assert_eq!(summary.position.tokens_sold, POSITION);
    assert_eq!(summary.position.tokens_unsold, 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn an_explicit_failed_outcome_is_carried_too() -> Result<(), Box<dyn Error>> {
    let run = start_exit(
        four_tranches(None)?,
        vec![Some(SellOutcome::Failed), Some(SellOutcome::Simulated)],
        Duration::ZERO,
        16,
    )?;
    run.updates
        .send(curve_update(10, &post_buy_spec()?))
        .await?;
    let recorder = Arc::clone(&run.recorder);
    let summary = run.finish().await?;
    let tokens: Vec<u64> = recorder.seen()?.iter().map(|entry| entry.tokens).collect();
    assert_eq!(tokens.get(1), Some(&(2 * TRANCHE_TOKENS)));
    // The failed share was re-offered to tranche 1, so the whole position is sold.
    assert_eq!(summary.position.tokens_sold, POSITION);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_price_crash_triggers_the_stop_loss_through_the_pipeline() -> Result<(), Box<dyn Error>> {
    let run = start_exit(four_tranches(Some(2_000))?, vec![], Duration::ZERO, 16)?;
    run.updates
        .send(curve_update(10, &post_buy_spec()?))
        .await?;
    sleep(Duration::from_millis(1_500)).await;
    run.updates
        .send(curve_update(11, &crashed_spec(40)?))
        .await?;
    let recorder = Arc::clone(&run.recorder);
    let summary = run.finish().await?;
    let seen = recorder.seen()?;
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen.first().map(|entry| entry.trigger),
        Some(ExitTrigger::Tranche { index: 0 })
    );
    let sweep = seen.get(1).ok_or("sweep")?;
    assert_eq!(sweep.trigger, ExitTrigger::StopLoss);
    assert_eq!(sweep.tokens, POSITION - TRANCHE_TOKENS);
    assert_eq!(sweep.at_ms, 1_500);
    assert!(summary.position.stop_loss_fired);
    assert_eq!(summary.position.tokens_unsold, 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn shutdown_stops_new_orders_and_reports_the_remainder() -> Result<(), Box<dyn Error>> {
    let run = start_exit(four_tranches(None)?, vec![], Duration::ZERO, 16)?;
    run.updates
        .send(curve_update(10, &post_buy_spec()?))
        .await?;
    sleep(Duration::from_millis(5_000)).await;
    run.trigger.trigger();
    let recorder = Arc::clone(&run.recorder);
    let ExitRun {
        manager, executor, ..
    } = run;
    let summary = manager.await??;
    executor.await?;
    assert!(summary.interrupted);
    let times: Vec<u128> = recorder.seen()?.iter().map(|entry| entry.at_ms).collect();
    assert_eq!(times, vec![1_000, 4_000]);
    assert_eq!(
        summary.position.tokens_unsold,
        POSITION - 2 * TRANCHE_TOKENS
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_dead_executor_is_reported_instead_of_hanging() -> Result<(), Box<dyn Error>> {
    let engine = engine()?;
    let manager = ExitManager::new(engine, curve_key(), Instant::now());
    let (orders_tx, orders_rx) = order_channel();
    let (_reports_tx, reports_rx) = report_channel();
    let (updates_tx, updates_rx) = mpsc::channel(4);
    let (_trigger, signal) = shutdown_channel();
    drop(orders_rx);
    updates_tx.send(curve_update(10, &post_buy_spec()?)).await?;
    let result = manager.run(updates_rx, orders_tx, reports_rx, signal).await;
    assert!(matches!(result, Err(ExitError::OrderChannelClosed)));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn missing_reports_do_not_hang_the_manager() -> Result<(), Box<dyn Error>> {
    // The submitter never answers: the manager stops waiting after the window
    // plus the settle timeout and says so.
    let run = start_exit(four_tranches(None)?, vec![], Duration::from_secs(3_600), 16)?;
    run.updates
        .send(curve_update(10, &post_buy_spec()?))
        .await?;
    let genesis = run.genesis;
    let ExitRun {
        manager,
        executor,
        trigger,
        recorder,
        ..
    } = run;
    let summary = manager.await??;
    assert!(summary.unsettled);
    assert_eq!(genesis.elapsed(), Duration::from_millis(10_500 + 5_000));
    assert_eq!(summary.position.tokens_in_flight, POSITION);
    assert_eq!(recorder.seen()?.len(), 4);
    trigger.trigger();
    // The executor drains for at most its drain timeout, then aborts.
    executor.await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_closed_report_channel_ends_the_run_without_waiting() -> Result<(), Box<dyn Error>> {
    let engine = engine()?;
    let manager = ExitManager::new(engine, curve_key(), Instant::now());
    let (orders_tx, mut orders_rx) = order_channel();
    let (reports_tx, reports_rx) = report_channel();
    let (updates_tx, updates_rx) = mpsc::channel(4);
    let (_trigger, signal) = shutdown_channel();
    drop(reports_tx);
    updates_tx.send(curve_update(10, &post_buy_spec()?)).await?;
    let summary = manager
        .run(updates_rx, orders_tx, reports_rx, signal)
        .await?;
    assert!(summary.unsettled);
    let mut queued = 0;
    while orders_rx.try_recv().is_ok() {
        queued += 1;
    }
    assert_eq!(queued, 4);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_flood_of_updates_cannot_deadlock_the_pipeline() -> Result<(), Box<dyn Error>> {
    // A one-slot channel and ten thousand updates: backpressure, not deadlock.
    let run = start_exit(four_tranches(None)?, vec![], Duration::ZERO, 1)?;
    let spec = post_buy_spec()?;
    let updates = run.updates.clone();
    let producer = tokio::spawn(async move {
        for slot in 1..=10_000u64 {
            if updates.send(curve_update(slot, &spec)).await.is_err() {
                return false;
            }
        }
        true
    });
    let summary = run.finish().await?;
    assert!(producer.await?);
    assert_eq!(summary.observations, 10_000);
    assert_eq!(summary.position.orders_issued, 4);
    Ok(())
}

// --- Sell executor ------------------------------------------------------------------------------------

/// Four real orders from the engine, for feeding the executor directly.
fn sample_orders() -> Result<Vec<SellOrder>, Box<dyn Error>> {
    let mut engine = engine()?;
    assert!(engine.observe(fresh_observation(10)?));
    let mut orders = Vec::new();
    for elapsed in [1_000, 4_000, 7_000, 10_000] {
        orders.extend(engine.poll(elapsed).orders);
    }
    assert_eq!(orders.len(), 4);
    Ok(orders)
}

#[tokio::test(start_paused = true)]
async fn the_executor_submits_orders_concurrently() -> Result<(), Box<dyn Error>> {
    let genesis = Instant::now();
    let recorder = Recorder::new(genesis, vec![], Duration::from_secs(3));
    let (orders_tx, orders_rx) = order_channel();
    let (reports_tx, mut reports_rx) = report_channel();
    let (trigger, signal) = shutdown_channel();
    let executor = tokio::spawn(run_sell_executor(
        Arc::clone(&recorder),
        orders_rx,
        reports_tx,
        signal,
        Duration::from_secs(10),
    ));
    let mut orders = sample_orders()?.into_iter();
    orders_tx.send(orders.next().ok_or("order")?).await?;
    sleep(Duration::from_secs(1)).await;
    orders_tx.send(orders.next().ok_or("order")?).await?;

    let first = reports_rx.recv().await.ok_or("first report")?;
    assert_eq!(genesis.elapsed(), Duration::from_secs(3));
    let second = reports_rx.recv().await.ok_or("second report")?;
    // Serialized submissions would have finished at 6 s.
    assert_eq!(genesis.elapsed(), Duration::from_secs(4));
    assert_eq!((first.sequence, second.sequence), (0, 1));
    assert!(matches!(first.outcome, SellOutcome::Landed { .. }));

    drop(orders_tx); // closing the order channel ends the executor
    executor.await?;
    drop(trigger);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn the_executor_turns_errors_into_failed_reports() -> Result<(), Box<dyn Error>> {
    let recorder = Recorder::new(Instant::now(), vec![None], Duration::ZERO);
    let (orders_tx, orders_rx) = order_channel();
    let (reports_tx, mut reports_rx) = report_channel();
    let (_trigger, signal) = shutdown_channel();
    let executor = tokio::spawn(run_sell_executor(
        recorder,
        orders_rx,
        reports_tx,
        signal,
        Duration::from_secs(1),
    ));
    orders_tx.send(sample_orders()?.remove(0)).await?;
    let report = reports_rx.recv().await.ok_or("report")?;
    assert_eq!(report.outcome, SellOutcome::Failed);
    drop(orders_tx);
    executor.await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn the_executor_drains_short_submissions_and_aborts_long_ones() -> Result<(), Box<dyn Error>>
{
    // Drains: a 2 s submission with a 5 s drain budget still reports.
    let recorder = Recorder::new(Instant::now(), vec![], Duration::from_secs(2));
    let (orders_tx, orders_rx) = order_channel();
    let (reports_tx, mut reports_rx) = report_channel();
    let (trigger, signal) = shutdown_channel();
    let executor = tokio::spawn(run_sell_executor(
        recorder,
        orders_rx,
        reports_tx,
        signal,
        Duration::from_secs(5),
    ));
    orders_tx.send(sample_orders()?.remove(0)).await?;
    sleep(Duration::from_millis(100)).await;
    trigger.trigger();
    executor.await?;
    assert!(reports_rx.recv().await.is_some());

    // Aborts: an hour-long submission cannot hold shutdown hostage.
    let started = Instant::now();
    let recorder = Recorder::new(started, vec![], Duration::from_secs(3_600));
    let (orders_tx, orders_rx) = order_channel();
    let (reports_tx, mut reports_rx) = report_channel();
    let (trigger, signal) = shutdown_channel();
    let executor = tokio::spawn(run_sell_executor(
        recorder,
        orders_rx,
        reports_tx,
        signal,
        Duration::from_secs(5),
    ));
    orders_tx.send(sample_orders()?.remove(0)).await?;
    sleep(Duration::from_millis(100)).await;
    trigger.trigger();
    executor.await?;
    assert_eq!(started.elapsed(), Duration::from_millis(5_100));
    assert!(
        reports_rx.recv().await.is_none(),
        "aborted: no report, channel closed"
    );
    Ok(())
}

// --- Full pipeline: scripted Geyser stream -> exit manager -> executor ---------------------------

#[tokio::test(start_paused = true)]
async fn mocked_stream_drives_the_whole_exit_across_a_reconnection() -> Result<(), Box<dyn Error>> {
    let genesis = Instant::now();
    let spec = post_buy_spec()?;
    let crashed = crashed_spec(80)?;
    // The first connection shows the curve, stays up for 2 s, then drops. The
    // second replays a stale update (older slot, higher price) before a fresh one.
    let transport = ScriptedTransport {
        scripts: vec![
            Script::Stream(vec![
                Step::Update(curve_update(10, &spec)),
                Step::Wait(Duration::from_secs(2)),
                Step::Close,
            ]),
            Script::Stream(vec![
                Step::Update(curve_update(9, &spec)),
                Step::Update(curve_update(30, &crashed)),
                Step::Hang,
            ]),
        ]
        .into(),
        connects: Arc::new(Mutex::new(Vec::new())),
    };
    let monitor = GeyserMonitor::new(transport, request(), test_settings())?;
    let (events_tx, events_rx) = mpsc::channel(32);
    let (health_tx, health_rx) = watch::channel(HealthStatus::default());
    let (trigger, signal) = shutdown_channel();
    let monitor_task = tokio::spawn(monitor.run(events_tx, health_tx, signal.clone()));

    let engine = engine_with(four_tranches(None)?, exit_settings())?;
    let manager = ExitManager::new(engine, curve_key(), genesis);
    let recorder = Recorder::new(genesis, vec![], Duration::ZERO);
    let (orders_tx, orders_rx) = order_channel();
    let (reports_tx, reports_rx) = report_channel();
    let executor = tokio::spawn(run_sell_executor(
        Arc::clone(&recorder),
        orders_rx,
        reports_tx,
        signal.clone(),
        Duration::from_secs(5),
    ));
    let summary = manager
        .run(events_rx, orders_tx, reports_rx, signal)
        .await?;

    trigger.trigger();
    let health = monitor_task.await??;
    executor.await?;

    assert_eq!(health.reconnects, 1);
    assert_eq!(health_rx.borrow().state, ConnectionState::Stopped);
    // Slot 10 and slot 30 were applied; the replayed slot 9 was dropped as stale.
    assert_eq!(summary.observations, 2);
    assert_eq!(summary.position.orders_issued, 4);
    assert_eq!(summary.position.tokens_sold, POSITION);
    let seen = recorder.seen()?;
    let times: Vec<u128> = seen.iter().map(|entry| entry.at_ms).collect();
    assert_eq!(times, vec![1_000, 4_000, 7_000, 10_000]);
    // Tranche 0 was quoted on the slot-10 curve, tranche 1 on the slot-30 one.
    let first_plan = SellPlan::from_token_amount(
        &post_buy_params()?,
        TRANCHE_TOKENS,
        exit_settings().fee_bps,
        exit_settings().slippage_bps,
    )?;
    assert_eq!(
        seen.first().map(|entry| entry.min_out),
        Some(first_plan.min_sol_output_lamports)
    );
    let crashed_curve = BondingCurveState::decode(&crashed.bytes())?.curve_params();
    // The slot-30 observation postdates the first sale (landed in slot 11).
    let expected = SellPlan::from_token_amount(
        &crashed_curve,
        TRANCHE_TOKENS,
        exit_settings().fee_bps,
        exit_settings().slippage_bps,
    )?;
    assert_eq!(
        seen.get(1).map(|entry| entry.min_out),
        Some(expected.min_sol_output_lamports)
    );
    Ok(())
}

// --- Unknown outcomes (audit finding #30) -------------------------------------------------------

/// Every token of the position is in exactly one bucket.
fn accounted(snapshot: &EngineSnapshot) -> u64 {
    snapshot.tokens_sold
        + snapshot.tokens_in_flight
        + snapshot.tokens_unresolved
        + snapshot.tokens_unsold
}

fn issue_first(engine: &mut ExitEngine) -> Result<SellOrder, Box<dyn Error>> {
    assert!(engine.observe(fresh_observation(10)?));
    Ok(engine
        .poll(1_000)
        .orders
        .first()
        .copied()
        .ok_or("first order")?)
}

fn report(engine: &mut ExitEngine, order: &SellOrder, outcome: SellOutcome) {
    engine.report(SellReport {
        sequence: order.sequence,
        outcome,
    });
}

#[test]
fn an_unknown_outcome_is_neither_sold_nor_carried_forward() -> Result<(), Box<dyn Error>> {
    let mut engine = engine()?;
    let first = issue_first(&mut engine)?;
    assert!(engine.has_unreported());
    report(&mut engine, &first, SellOutcome::Unknown);

    let snapshot = engine.snapshot();
    assert_eq!(snapshot.tokens_unresolved, TRANCHE_TOKENS);
    assert_eq!(snapshot.tokens_sold, 0);
    assert_eq!(snapshot.tokens_in_flight, 0);
    assert_eq!(snapshot.tokens_unsold, POSITION - TRANCHE_TOKENS);
    assert_eq!(accounted(&snapshot), POSITION);
    assert!(!engine.has_unreported(), "an unknown order is a report");

    // No carry: the next tranches sell their own share only, and the last one
    // sweeps what is left of the *unallocated* tokens, never the unresolved ones.
    let mut sold = Vec::new();
    for elapsed in [4_000, 7_000, 10_000] {
        let order = engine
            .poll(elapsed)
            .orders
            .first()
            .copied()
            .ok_or("later order")?;
        sold.push(order.plan.token_amount);
        report(&mut engine, &order, SellOutcome::Landed { slot: 11 });
    }
    assert_eq!(
        sold,
        vec![TRANCHE_TOKENS, TRANCHE_TOKENS, TRANCHE_TOKENS + 3]
    );
    let end = engine.snapshot();
    assert_eq!(end.tokens_unsold, 0);
    assert_eq!(end.tokens_unresolved, TRANCHE_TOKENS);
    assert_eq!(end.tokens_sold, POSITION - TRANCHE_TOKENS);
    assert_eq!(accounted(&end), POSITION);
    Ok(())
}

#[test]
fn an_unknown_order_is_reported_once() -> Result<(), Box<dyn Error>> {
    let mut engine = engine()?;
    let first = issue_first(&mut engine)?;
    report(&mut engine, &first, SellOutcome::Unknown);
    // Late or duplicate reports for the same order change nothing.
    report(&mut engine, &first, SellOutcome::Landed { slot: 12 });
    report(&mut engine, &first, SellOutcome::Failed);
    report(&mut engine, &first, SellOutcome::Unknown);
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.tokens_unresolved, TRANCHE_TOKENS);
    assert_eq!(snapshot.tokens_sold, 0);
    assert_eq!(snapshot.tokens_unsold, POSITION - TRANCHE_TOKENS);
    Ok(())
}

#[test]
fn resolving_an_unknown_order_as_sold_counts_it_once() -> Result<(), Box<dyn Error>> {
    let mut engine = engine()?;
    let first = issue_first(&mut engine)?;
    report(&mut engine, &first, SellOutcome::Unknown);
    assert!(engine.resolve_unknown(first.sequence, true));
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.tokens_sold, TRANCHE_TOKENS);
    assert_eq!(snapshot.tokens_unresolved, 0);
    assert_eq!(snapshot.tokens_unsold, POSITION - TRANCHE_TOKENS);
    assert_eq!(accounted(&snapshot), POSITION);
    // Already settled.
    assert!(!engine.resolve_unknown(first.sequence, true));
    assert!(!engine.resolve_unknown(first.sequence, false));
    assert_eq!(engine.snapshot().tokens_sold, TRANCHE_TOKENS);
    Ok(())
}

#[test]
fn resolving_an_unknown_order_as_unsold_offers_it_to_the_next_tranche() -> Result<(), Box<dyn Error>>
{
    let mut engine = engine()?;
    let first = issue_first(&mut engine)?;
    report(&mut engine, &first, SellOutcome::Unknown);
    assert!(engine.resolve_unknown(first.sequence, false));
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.tokens_unresolved, 0);
    assert_eq!(snapshot.tokens_unsold, POSITION);
    assert_eq!(accounted(&snapshot), POSITION);

    let second = engine.poll(4_000).orders.first().copied().ok_or("second")?;
    assert_eq!(second.plan.token_amount, 2 * TRANCHE_TOKENS);
    // The sale never happened, so it leaves no phantom impact on the quote.
    let curve = fresh_observation(10)?.state.curve_params();
    assert_eq!(
        second.plan,
        SellPlan::from_token_amount(
            &curve,
            2 * TRANCHE_TOKENS,
            exit_settings().fee_bps,
            exit_settings().slippage_bps
        )?
    );
    Ok(())
}

#[test]
fn only_unresolved_orders_can_be_resolved() -> Result<(), Box<dyn Error>> {
    let mut engine = engine()?;
    assert!(!engine.resolve_unknown(0, true), "nothing issued yet");
    let first = issue_first(&mut engine)?;
    assert!(
        !engine.resolve_unknown(first.sequence, true),
        "still awaiting its report"
    );
    report(&mut engine, &first, SellOutcome::Landed { slot: 11 });
    assert!(
        !engine.resolve_unknown(first.sequence, false),
        "landed, not unknown"
    );
    assert!(!engine.resolve_unknown(99, true));
    assert_eq!(engine.snapshot().tokens_sold, TRANCHE_TOKENS);
    Ok(())
}

#[test]
fn an_unresolved_sale_keeps_its_impact_until_it_can_no_longer_land() -> Result<(), Box<dyn Error>> {
    let settings = exit_settings();
    let curve = fresh_observation(10)?.state.curve_params();
    let quote = |curve: &CurveParams| {
        SellPlan::from_token_amount(
            curve,
            TRANCHE_TOKENS,
            settings.fee_bps,
            settings.slippage_bps,
        )
    };

    // The first sale was quoted on slot 10. At the last slot it may still land,
    // an observation without its effect is not proof that it failed.
    for (slot, projected) in [
        (20, true),
        (10 + UNRESOLVED_PROJECTION_SLOTS, true),
        (10 + UNRESOLVED_PROJECTION_SLOTS + 1, false),
    ] {
        let mut engine = engine()?;
        let first = issue_first(&mut engine)?;
        report(&mut engine, &first, SellOutcome::Unknown);
        assert!(engine.observe(fresh_observation(slot)?));
        let second = engine.poll(4_000).orders.first().copied().ok_or("second")?;
        let expected = if projected {
            quote(&first.plan.apply_to(&curve)?)?
        } else {
            quote(&curve)?
        };
        assert_eq!(second.plan, expected, "slot {slot}");
    }
    Ok(())
}

#[test]
fn the_stop_loss_never_sweeps_unresolved_tokens() -> Result<(), Box<dyn Error>> {
    let mut engine = engine_with(four_tranches(Some(2_000))?, exit_settings())?;
    let first = issue_first(&mut engine)?;
    report(&mut engine, &first, SellOutcome::Unknown);

    assert!(engine.observe(observation(11, &crashed_spec(50)?)?));
    let sweep = engine.poll(1_500).orders.first().copied().ok_or("sweep")?;
    assert_eq!(sweep.trigger, ExitTrigger::StopLoss);
    assert_eq!(sweep.plan.token_amount, POSITION - TRANCHE_TOKENS);
    report(&mut engine, &sweep, SellOutcome::Landed { slot: 12 });
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.tokens_unresolved, TRANCHE_TOKENS);
    assert_eq!(snapshot.tokens_unsold, 0);
    assert_eq!(accounted(&snapshot), POSITION);
    Ok(())
}

#[test]
fn only_ambiguous_send_failures_are_treated_as_unknown() {
    assert!(JitoError::Timeout.may_have_been_accepted());
    let definite = [
        JitoError::DryRun,
        JitoError::EmptyBundle,
        JitoError::RateLimited,
        JitoError::Unauthorized { status: 401 },
        JitoError::Api {
            status: 503,
            message: String::new(),
        },
        JitoError::Rpc {
            code: -32_602,
            message: String::new(),
        },
        JitoError::InvalidResponse("unexpected"),
    ];
    for error in &definite {
        assert!(!error.may_have_been_accepted(), "{error}");
    }
}

#[tokio::test(start_paused = true)]
async fn an_unknown_outcome_through_the_pipeline_is_not_resubmitted() -> Result<(), Box<dyn Error>>
{
    let run = start_exit(
        four_tranches(None)?,
        vec![Some(SellOutcome::Unknown)],
        Duration::ZERO,
        16,
    )?;
    run.updates
        .send(curve_update(10, &post_buy_spec()?))
        .await?;
    let recorder = Arc::clone(&run.recorder);
    let summary = run.finish().await?;

    let tokens: Vec<u64> = recorder.seen()?.iter().map(|entry| entry.tokens).collect();
    assert_eq!(
        tokens,
        vec![
            TRANCHE_TOKENS,
            TRANCHE_TOKENS,
            TRANCHE_TOKENS,
            TRANCHE_TOKENS + 3
        ],
        "the unknown tranche is not repeated"
    );
    assert_eq!(summary.position.tokens_unresolved, TRANCHE_TOKENS);
    assert_eq!(summary.position.tokens_sold, POSITION - TRANCHE_TOKENS);
    assert_eq!(summary.position.tokens_unsold, 0);
    assert!(!summary.unsettled);
    Ok(())
}

// --- Log hygiene (audit finding #31) -----------------------------------------------------------

/// Records every event at every level, so the checks below hold even when
/// `RUST_LOG=trace` is set in production.
#[derive(Default)]
struct Captured {
    lines: Mutex<Vec<String>>,
}

struct Capture(Arc<Captured>);

struct Collect<'a>(&'a mut String);

impl tracing::field::Visit for Collect<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, " {}={:?}", field.name(), value);
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = format!("{} {}", event.metadata().level(), event.metadata().target());
        event.record(&mut Collect(&mut line));
        if let Ok(mut lines) = self.0.lines.lock() {
            lines.push(line);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn capture() -> (Arc<Captured>, tracing::subscriber::DefaultGuard) {
    let captured = Arc::new(Captured::default());
    let guard = tracing::subscriber::set_default(Capture(Arc::clone(&captured)));
    (captured, guard)
}

/// A submitter whose failures carry Block Engine text.
struct JitoDown;

impl SellSubmitter for JitoDown {
    async fn submit(&self, _order: &SellOrder) -> Result<SellOutcome, ExitError> {
        Err(ExitError::Jito(JitoError::Api {
            status: 503,
            message: "overloaded".to_owned(),
        }))
    }
}

/// Words that look like a base58 public key or signature (32+ base58 characters).
fn looks_like_key_material(line: &str) -> bool {
    line.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| {
            word.len() >= 32
                && word
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() && !matches!(c, '0' | 'O' | 'I' | 'l'))
        })
}

#[tokio::test(start_paused = true)]
async fn logs_never_contain_keys_signatures_amounts_or_balances() -> Result<(), Box<dyn Error>> {
    let (captured, _guard) = capture();

    // 1. A pipeline with an undecodable update, an unknown outcome, a failed
    //    submission, a failed-outcome report and a closed update stream.
    let run = start_exit(
        four_tranches(None)?,
        vec![Some(SellOutcome::Unknown), None, Some(SellOutcome::Failed)],
        Duration::ZERO,
        16,
    )?;
    run.updates
        .send(StreamUpdate::Account(account_update(
            5,
            5,
            curve_key(),
            vec![1, 2, 3],
        )))
        .await?;
    run.updates
        .send(curve_update(10, &post_buy_spec()?))
        .await?;
    let ExitRun {
        updates,
        manager,
        executor,
        recorder,
        trigger,
        ..
    } = run;
    drop(updates);
    let _ = manager.await??;
    trigger.trigger();
    executor.await?;
    let seen = recorder.seen()?;
    assert_eq!(seen.len(), 4);

    // 2. A budget that allows one order: the others are dropped with an error
    //    that carries lamport amounts in its Display text.
    let tight = ExitSettings {
        max_total_cost_lamports: 125_000,
        ..exit_settings()
    };
    let engine = engine_with(four_tranches(None)?, tight)?;
    // A fresh genesis: the first scenario advanced the paused clock.
    let manager = ExitManager::new(engine, curve_key(), Instant::now());
    let (orders_tx, orders_rx) = order_channel();
    let (reports_tx, reports_rx) = report_channel();
    let (updates_tx, updates_rx) = mpsc::channel(4);
    let (trigger, signal) = shutdown_channel();
    let executor = tokio::spawn(run_sell_executor(
        Arc::new(JitoDown),
        orders_rx,
        reports_tx,
        signal.clone(),
        Duration::from_secs(5),
    ));
    updates_tx.send(curve_update(10, &post_buy_spec()?)).await?;
    let summary = manager
        .run(updates_rx, orders_tx, reports_rx, signal)
        .await?;
    trigger.trigger();
    executor.await?;
    assert!(summary.position.dropped >= 3);

    // The scenarios must have produced the events the checks are about.
    let lines = captured.lines.lock().map_err(|_| "poisoned")?.clone();
    let joined = lines.join("\n");
    for expected in [
        "kind=\"budget_exceeded\"",
        "kind=\"jito_error\"",
        "sell outcome unknown",
        "ignoring undecodable bonding curve update",
        "update stream closed",
        "exit finished with part of the position not confirmed sold",
    ] {
        assert!(
            joined.contains(expected),
            "missing event: {expected}\n{joined}"
        );
    }

    // Nothing sensitive, at any level.
    let mut forbidden = vec![
        curve_key().to_string(),
        PUMP_PROGRAM_ID.to_string(),
        POSITION.to_string(),
        TRANCHE_TOKENS.to_string(),
        (TRANCHE_TOKENS + 3).to_string(),
        "125000".to_owned(),
        "5000000".to_owned(),
    ];
    for entry in &seen {
        forbidden.push(entry.min_out.to_string());
    }
    for line in &lines {
        assert!(!looks_like_key_material(line), "key-like text in: {line}");
        for secret in &forbidden {
            assert!(
                !line.contains(secret.as_str()),
                "{secret} leaked in: {line}"
            );
        }
    }
    Ok(())
}

#[test]
fn error_labels_are_static_and_value_free() {
    let errors = [
        ExitError::NoCurveState,
        ExitError::CurveComplete,
        ExitError::CashbackCoinUnsupported,
        ExitError::WindowMissed { tranche: 3 },
        ExitError::BudgetExceeded {
            cost: 123_456,
            remaining: 654_321,
        },
        ExitError::OrderChannelClosed,
        ExitError::OrderChannelFull,
        ExitError::ArithmeticOverflow { context: "x" },
        ExitError::StaleBlockhash,
        ExitError::InvalidPlan { reason: "x" },
        ExitError::InvalidConfig { reason: "x" },
        ExitError::Builder(BuilderError::ZeroSellAmount),
        ExitError::Jito(JitoError::DryRun),
    ];
    for error in &errors {
        let label = error.label();
        assert!(!label.is_empty());
        assert!(
            label.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "{label}"
        );
    }
    // The Display text of the budget error carries amounts; the label never does.
    assert!(ExitError::BudgetExceeded {
        cost: 123_456,
        remaining: 654_321
    }
    .to_string()
    .contains("123456"));
    assert_eq!(
        ExitError::BudgetExceeded {
            cost: 123_456,
            remaining: 654_321
        }
        .label(),
        "budget_exceeded"
    );
}
