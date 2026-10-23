//! The sessions `main` runs, wired from the real configuration.
//!
//! - [`check`]: validates everything that can be validated without the network
//!   (keypair file, barrier, endpoints) and reports it.
//! - [`run_exit_command`]: the exit of one position. In [`Mode::Dry`] the sell
//!   orders are assembled and signed against a dry Block Engine client (nothing
//!   leaves the process); in [`Mode::Live`] the executor is armed through
//!   [`LiveExecutor::arm`] (which re-checks the whole barrier and reads the
//!   keypair file under its `0600` rule) and runs [`LiveExecutor::run_exit`].
//!
//! # Wiring
//! ```text
//! RpcBlockhashSource --(Services: refresher)--> watch<TimedBlockhash> --+
//! HttpTipSource ------(Services: tip feed)----> TipBook ------------+   |
//!                                                                   v   v
//! YellowstoneTransport<Link> --> GeyserMonitor --> ExitManager --> order executor --> Jito
//! ```
//!
//! # Policy choices (for the owner to review)
//! - `genesis`, the zero of the 1 s to 10 s tranche window, is the moment the
//!   stream is about to start: the launch is not driven from here.
//! - The cost budget per order assumes the **maximum** tip
//!   (`MAX_JITO_TIP_LAMPORTS`), the worst case the dynamic tip can reach.
//! - Dry mode pays the static `JITO_TIP_LAMPORTS` and does not poll the tip feed.
//! - The blockhash is fetched at `confirmed`.
//!
//! # Logging hygiene
//! Static labels and counters only (see [`crate::app`]).

use std::sync::Arc;

use tokio::time::{timeout, Instant};
use tracing::{info, warn};

use crate::app::{
    decide_mode, log_summary, preflight_keypair, summarize, BlockhashFeed, BlockhashSource,
    ExitArgs, LiveRequest, Mode, Services, SessionSummary, TipFeed, BLOCKHASH_FETCH_TIMEOUT,
    DEFAULT_BLOCKHASH_INTERVAL,
};
use crate::builder::sell::{SellBuilder, SellSettings};
use crate::config::Config;
use crate::errors::{BridgeError, LauncherError};
use crate::execution::live::{
    run_exit_pipeline, ExitPipeline, ExitRun, LiveExecutor, DEFAULT_DRAIN_TIMEOUT,
};
use crate::execution::tip_feed::TipFeedSettings;
use crate::execution::tip_http::HttpTipSource;
use crate::monitor::geyser::MonitorSettings;
use crate::monitor::submit::{blockhash_channel, JitoSellSubmitter};
use crate::monitor::ShutdownSignal;
use crate::network::rpc::RpcBlockhashSource;
use crate::network::yellowstone::YellowstoneTransport;
use crate::network::{tip_account_for, JitoClient, JitoTip};

/// The gRPC link of this build.
#[cfg(feature = "grpc")]
type Link = crate::network::grpc::TonicLink;
/// The gRPC link of this build: none (see `docs/yellowstone-adapter.md`).
#[cfg(not(feature = "grpc"))]
type Link = crate::network::yellowstone::UnavailableLink;

/// Whether this build has the `tonic` link.
pub const GRPC_COMPILED: bool = cfg!(feature = "grpc");

/// What [`check`] found. Nothing in it is secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    /// The mode `exit` would run in with the same flags.
    pub mode: Mode,
    /// A keypair file is configured and passed the `0600` preflight.
    pub keypair_verified: bool,
    /// `scheme://host[:port]` of the Yellowstone endpoint, if configured.
    pub yellowstone_origin: Option<String>,
    /// A `JITO_TIP_STREAM_URL` is configured (otherwise the public Jito
    /// endpoint is used in live mode).
    pub tip_feed_configured: bool,
    /// This build has the `tonic` link.
    pub grpc_compiled: bool,
    /// The live barrier was crossed (only when `--live` was requested).
    pub live_armed: bool,
}

/// Validates the setup without touching the network.
///
/// # Errors
/// The first failed check: the keypair preflight, the barrier, or the
/// construction of a client from the configuration.
pub fn check(config: &Config, request: &LiveRequest) -> Result<CheckReport, LauncherError> {
    let keypair_verified = preflight_keypair(&config.live, config.wallet())?;
    let mode = decide_mode(request, config.dry_run, &config.live)?;
    let live_armed = match mode {
        Mode::Live => {
            // Re-runs every barrier check and reads the keypair file; connects
            // to nothing. The executor is dropped (and its wallet wiped) at once.
            LiveExecutor::arm(config)?;
            true
        }
        Mode::Dry => false,
    };
    // Constructing the clients validates their configuration.
    RpcBlockhashSource::new(&config.rpc_url)?;
    if let Some(url) = &config.production.tip_feed_url {
        HttpTipSource::new(url)?;
    }
    Ok(CheckReport {
        mode,
        keypair_verified,
        yellowstone_origin: config
            .production
            .yellowstone
            .as_ref()
            .map(|settings| settings.origin().to_owned()),
        tip_feed_configured: config.production.tip_feed_url.is_some(),
        grpc_compiled: GRPC_COMPILED,
        live_armed,
    })
}

async fn first_blockhash(
    source: &mut RpcBlockhashSource,
) -> Result<solana_sdk::hash::Hash, BridgeError> {
    timeout(BLOCKHASH_FETCH_TIMEOUT, source.latest())
        .await
        .map_err(|_| BridgeError::BlockhashUnavailable)?
}

/// Runs the exit of `args` until it is done or `shutdown` fires.
///
/// `mode` comes from [`decide_mode`]; [`Mode::Live`] additionally passes
/// through [`LiveExecutor::arm`] here, so a caller that skipped the barrier
/// still cannot spend.
///
/// # Errors
/// Any [`LauncherError`]: missing Yellowstone configuration, a build without
/// the gRPC link, a refused barrier, the first blockhash, or the pipeline.
pub async fn run_exit_command(
    mut config: Config,
    args: ExitArgs,
    mode: Mode,
    shutdown: ShutdownSignal,
) -> Result<SessionSummary, LauncherError> {
    let settings = config
        .production
        .yellowstone
        .take()
        .ok_or(BridgeError::YellowstoneNotConfigured)?;
    if !GRPC_COMPILED {
        return Err(BridgeError::GrpcNotCompiled.into());
    }
    info!(
        mode = mode.label(),
        stream = settings.origin(),
        "starting the exit session"
    );

    let builder = SellBuilder::new(
        config.wallet().pubkey(),
        args.mint,
        args.variant(),
        SellSettings::from_config(&config),
    )?;
    let order_cost = builder.order_cost_lamports(config.max_jito_tip_lamports)?;
    let engine = args.engine(config.slippage_bps, order_cost)?;
    let curve_account = builder.addresses().bonding_curve;

    let mut rpc = RpcBlockhashSource::new(&config.rpc_url)?;
    let initial = first_blockhash(&mut rpc).await?;
    let (hash_sender, hash_receiver) = blockhash_channel(initial);
    let mut services = Services::new().with_blockhash(BlockhashFeed {
        source: rpc,
        sender: hash_sender,
        interval: DEFAULT_BLOCKHASH_INTERVAL,
    });

    let transport = YellowstoneTransport::new(Link::default(), settings);
    let monitor = MonitorSettings::default();
    let genesis = Instant::now();

    let outcome = match mode {
        Mode::Dry => {
            // Always a dry client, whatever DRY_RUN says: only `--live` crosses.
            let client = JitoClient::new(&config.jito_engine_url, true)?;
            let tip = JitoTip::from_config(&config, tip_account_for(&args.mint))?;
            let submitter = JitoSellSubmitter::new(
                Arc::new(client),
                builder,
                Arc::new(config.load_wallet()?),
                hash_receiver,
                tip,
            );
            info!("tip feed not polled in dry mode: the static tip is used");
            run_exit_pipeline(
                transport,
                Arc::new(submitter),
                ExitPipeline {
                    engine,
                    curve_account,
                    genesis,
                    monitor,
                    drain_timeout: DEFAULT_DRAIN_TIMEOUT,
                },
                None,
                shutdown,
            )
            .await
        }
        Mode::Live => {
            let executor = LiveExecutor::arm(&config)?;
            let source = match &config.production.tip_feed_url {
                Some(url) => HttpTipSource::new(url)?,
                None => {
                    info!("JITO_TIP_STREAM_URL not set: polling the public Jito tip floor");
                    HttpTipSource::mainnet()?
                }
            };
            services = services.with_tip_feed(TipFeed {
                source,
                book: Arc::clone(executor.tips()),
                settings: TipFeedSettings::default(),
            });
            executor
                .run_exit(
                    transport,
                    ExitRun {
                        engine,
                        curve_account,
                        genesis,
                        builder,
                        blockhash: hash_receiver,
                        monitor,
                        drain_timeout: DEFAULT_DRAIN_TIMEOUT,
                    },
                    shutdown,
                )
                .await
        }
    };

    // Stop the helpers whether or not the pipeline succeeded.
    match services.stop().await {
        Ok(report) => info!(
            blockhashes = report.blockhash.refreshed,
            blockhash_failures = report.blockhash.failures,
            tip_updates = report.tip_feed.map_or(0, |stats| stats.updates),
            "services stopped"
        ),
        Err(error) => warn!(error = error.label(), "a service failed"),
    }

    let report = outcome?;
    let summary = summarize(&report);
    log_summary(mode, &summary);
    Ok(summary)
}
