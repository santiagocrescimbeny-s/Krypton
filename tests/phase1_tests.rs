//! Phase 1 test-suite: keypair loading, zeroization and environment parsing.
//!
//! Conventions
//! - Tests return `Result` and use `?` / `matches!` instead of `unwrap()`, so
//!   the Zero Panics Policy is respected even here.
//! - Configuration is exercised through `Config::from_vars` with explicit maps:
//!   deterministic, no global-state mutation, no accidental pickup of a
//!   developer's real `.env`.
//! - Wallet vectors are derived from fixed seeds, so every run is reproducible.

use std::collections::HashMap;
use std::error::Error;
use std::mem::ManuallyDrop;

use krypton::config::{
    redact_url, Config, DEFAULT_COMPUTE_UNIT_LIMIT, DEFAULT_JITO_TIP_LAMPORTS,
    DEFAULT_PRIORITY_FEE_MICRO_LAMPORTS, DEFAULT_SLIPPAGE_BPS, ENV_KEYS,
};
use krypton::errors::{ConfigError, LauncherError, WalletError};
use krypton::security::wallet::{KEYPAIR_BYTES, PUBLIC_KEY_BYTES};
use krypton::security::{SecretKeyBytes, SecretString, SecureWallet};
use solana_commitment_config::CommitmentConfig;
use solana_keypair::Keypair;
use zeroize::{Zeroize, ZeroizeOnDrop};

type TestResult = Result<(), Box<dyn Error>>;

// --- Helpers -------------------------------------------------------------------

/// Deterministic, internally consistent 64-byte keypair for a given seed byte.
fn keypair_bytes(seed_byte: u8) -> [u8; KEYPAIR_BYTES] {
    Keypair::new_from_array([seed_byte; 32]).to_bytes()
}

fn base58_of(bytes: &[u8]) -> String {
    bs58::encode(bytes).into_string()
}

fn json_of(bytes: &[u8]) -> String {
    let items: Vec<String> = bytes.iter().map(ToString::to_string).collect();
    format!("[{}]", items.join(","))
}

fn err_of<T, E>(result: Result<T, E>) -> Result<E, Box<dyn Error>> {
    match result {
        Ok(_) => Err("expected an error but the operation succeeded".into()),
        Err(err) => Ok(err),
    }
}

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

/// Smallest valid environment (only the required keys).
fn minimal_vars() -> HashMap<String, String> {
    let key = base58_of(&keypair_bytes(7));
    vars(&[
        ("RPC_URL", "https://rpc.example.com"),
        ("JITO_ENGINE_URL", "https://jito.example.com"),
        ("PRIVATE_KEY", key.as_str()),
        ("CREATOR_SOL_AMOUNT", "0.5"),
    ])
}

fn with(mut map: HashMap<String, String>, key: &str, value: &str) -> HashMap<String, String> {
    map.insert(key.to_owned(), value.to_owned());
    map
}

fn without(mut map: HashMap<String, String>, key: &str) -> HashMap<String, String> {
    map.remove(key);
    map
}

// =================================================================================
// Wallet: loading
// =================================================================================

#[test]
fn loads_base58_keypair_and_derives_the_right_pubkey() -> TestResult {
    let bytes = keypair_bytes(7);
    let wallet = SecureWallet::from_secret_str(&base58_of(&bytes))?;

    let expected_public = bytes
        .last_chunk::<PUBLIC_KEY_BYTES>()
        .ok_or("short keypair")?;
    assert_eq!(&wallet.pubkey_bytes(), expected_public);
    assert_eq!(wallet.secret().expose(), &bytes);
    // `Pubkey`'s Display is the Base58 form of its bytes.
    assert_eq!(wallet.pubkey().to_string(), base58_of(expected_public));
    Ok(())
}

#[test]
fn loads_json_array_keypair_identically_to_base58() -> TestResult {
    let bytes = keypair_bytes(9);
    let from_b58 = SecureWallet::from_secret_str(&base58_of(&bytes))?;
    let from_json = SecureWallet::from_secret_str(&json_of(&bytes))?;

    assert_eq!(from_b58.pubkey_bytes(), from_json.pubkey_bytes());
    assert_eq!(from_b58.secret().expose(), from_json.secret().expose());
    Ok(())
}

#[test]
fn tolerates_whitespace_and_pretty_printed_json() -> TestResult {
    let bytes = keypair_bytes(11);
    let pretty = json_of(&bytes).replace(',', ",\n  ");
    let padded = format!("\n  {pretty}\n");
    let wallet = SecureWallet::from_secret_str(&padded)?;
    assert_eq!(wallet.secret().expose(), &bytes);

    let padded_b58 = format!("  {}\t\n", base58_of(&bytes));
    let wallet = SecureWallet::from_secret_str(&padded_b58)?;
    assert_eq!(wallet.secret().expose(), &bytes);
    Ok(())
}

#[test]
fn loads_from_raw_bytes_and_rejects_wrong_slice_lengths() -> TestResult {
    let bytes = keypair_bytes(5);
    let wallet = SecureWallet::from_keypair_bytes(&bytes)?;
    assert_eq!(wallet.secret().expose(), &bytes);

    let short = bytes.get(..63).ok_or("slice")?;
    assert_eq!(
        err_of(SecureWallet::from_keypair_bytes(short))?,
        WalletError::InvalidLength {
            expected: 64,
            actual: 63
        }
    );
    assert_eq!(
        err_of(SecureWallet::from_keypair_bytes(&[]))?,
        WalletError::InvalidLength {
            expected: 64,
            actual: 0
        }
    );
    Ok(())
}

#[test]
fn rebuilt_keypair_matches_the_original_bytes() -> TestResult {
    let bytes = keypair_bytes(13);
    let wallet = SecureWallet::from_secret_str(&base58_of(&bytes))?;
    assert_eq!(wallet.keypair().to_bytes(), bytes);
    Ok(())
}

// =================================================================================
// Wallet: rejection paths
// =================================================================================

#[test]
fn rejects_empty_input() -> TestResult {
    for blank in ["", "   ", "\n\t "] {
        assert_eq!(
            err_of(SecureWallet::from_secret_str(blank))?,
            WalletError::EmptySecret
        );
    }
    Ok(())
}

#[test]
fn rejects_invalid_base58_without_echoing_the_input() -> TestResult {
    // '0', 'O', 'I' and 'l' are not part of the Base58 alphabet.
    let bad = "secret-0OIl-not-base58";
    let err = err_of(SecureWallet::from_secret_str(bad))?;
    assert_eq!(err, WalletError::InvalidBase58);
    assert!(!err.to_string().contains("0OIl"));
    assert!(!format!("{err:?}").contains("0OIl"));

    assert_eq!(
        err_of(SecureWallet::from_secret_str("héllo"))?,
        WalletError::InvalidBase58
    );
    Ok(())
}

#[test]
fn rejects_wrong_length_base58() -> TestResult {
    let bytes = keypair_bytes(3);
    let seed_only = bytes.get(..32).ok_or("slice")?;
    assert_eq!(
        err_of(SecureWallet::from_secret_str(&base58_of(seed_only)))?,
        WalletError::InvalidLength {
            expected: 64,
            actual: 32
        }
    );

    let mut too_long = bytes.to_vec();
    too_long.push(1);
    assert_eq!(
        err_of(SecureWallet::from_secret_str(&base58_of(&too_long)))?,
        WalletError::InvalidLength {
            expected: 64,
            actual: 65
        }
    );
    Ok(())
}

#[test]
fn rejects_oversized_input_early() -> TestResult {
    // Decodes to more bytes than the scratch buffer -> buffer overflow path.
    assert_eq!(
        err_of(SecureWallet::from_secret_str(&"z".repeat(1000)))?,
        WalletError::InputTooLarge
    );
    // Longer than the character cap -> rejected before any decoding.
    assert_eq!(
        err_of(SecureWallet::from_secret_str(&"z".repeat(3000)))?,
        WalletError::InputTooLarge
    );
    Ok(())
}

#[test]
fn rejects_malformed_and_mis_sized_json_arrays() -> TestResult {
    let valid = keypair_bytes(2);
    let mut with_256 = valid.map(u16::from).to_vec();
    if let Some(first) = with_256.first_mut() {
        *first = 256;
    }
    let with_256 = format!(
        "[{}]",
        with_256
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    );

    for bad in [
        "[abc]",
        "[1,2,3",
        "[1,2,,3]",
        "[1,2,]",
        "[-1,2,3]",
        "[[1,2,3]]",
        with_256.as_str(),
    ] {
        assert_eq!(
            err_of(SecureWallet::from_secret_str(bad))?,
            WalletError::InvalidJsonArray,
            "input should be rejected as malformed JSON"
        );
    }

    assert_eq!(
        err_of(SecureWallet::from_secret_str("[]"))?,
        WalletError::InvalidLength {
            expected: 64,
            actual: 0
        }
    );
    assert_eq!(
        err_of(SecureWallet::from_secret_str("[1,2,3]"))?,
        WalletError::InvalidLength {
            expected: 64,
            actual: 3
        }
    );

    let mut too_long = valid.to_vec();
    too_long.push(0);
    assert_eq!(
        err_of(SecureWallet::from_secret_str(&json_of(&too_long)))?,
        WalletError::InvalidLength {
            expected: 64,
            actual: 65
        }
    );
    Ok(())
}

#[test]
fn rejects_a_public_key_that_does_not_match_the_seed() -> TestResult {
    let mut bytes = keypair_bytes(4);
    let last = bytes.last_mut().ok_or("empty keypair")?;
    *last ^= 0x01;

    for encoded in [base58_of(&bytes), json_of(&bytes)] {
        assert_eq!(
            err_of(SecureWallet::from_secret_str(&encoded))?,
            WalletError::PublicKeyMismatch
        );
    }
    assert_eq!(
        err_of(SecureWallet::from_keypair_bytes(&bytes))?,
        WalletError::PublicKeyMismatch
    );

    // A public half spliced in from a different key is rejected too.
    let mut spliced = keypair_bytes(4);
    let other = keypair_bytes(5);
    for (dst, src) in spliced.iter_mut().skip(32).zip(other.iter().skip(32)) {
        *dst = *src;
    }
    assert_eq!(
        err_of(SecureWallet::from_keypair_bytes(&spliced))?,
        WalletError::PublicKeyMismatch
    );
    Ok(())
}

// =================================================================================
// Zeroize behaviour
// =================================================================================

fn assert_zeroize_on_drop<T: ZeroizeOnDrop>() {}
fn assert_zeroize<T: Zeroize>() {}

#[test]
fn secret_containers_implement_the_zeroize_traits() {
    assert_zeroize_on_drop::<SecretKeyBytes>();
    assert_zeroize_on_drop::<SecureWallet>();
    assert_zeroize_on_drop::<SecretString>();
    assert_zeroize::<SecretKeyBytes>();
    assert_zeroize::<SecretString>();
}

#[test]
fn secret_bytes_are_physically_wiped_when_the_wallet_is_dropped() -> TestResult {
    let encoded = base58_of(&keypair_bytes(7));
    let mut slot = ManuallyDrop::new(SecureWallet::from_secret_str(&encoded)?);

    // Where do the secret bytes live inside the wallet value?
    let wallet_addr = (&*slot as *const SecureWallet) as usize;
    let secret_addr = slot.secret().expose().as_ptr() as usize;
    let offset = secret_addr - wallet_addr;

    // Sanity: before dropping, the key material is really there.
    assert!(
        slot.secret().expose().iter().any(|byte| *byte != 0),
        "the test keypair must contain non-zero bytes"
    );

    // SAFETY: `slot` is never used as a live `SecureWallet` again.
    unsafe { ManuallyDrop::drop(&mut slot) };

    // The wallet lives inline on this stack frame, so its memory is still
    // allocated and initialised after `drop`; `read_volatile` stops the
    // compiler from reasoning about the value having been dropped.
    let base = (&*slot as *const SecureWallet).cast::<u8>();
    let all_zero = (0..KEYPAIR_BYTES).all(|index| {
        // SAFETY: `offset + index` stays inside the `SecureWallet` value.
        unsafe { std::ptr::read_volatile(base.add(offset + index)) == 0 }
    });
    assert!(all_zero, "ZeroizeOnDrop must clear all 64 secret bytes");
    Ok(())
}

#[test]
fn secret_string_clears_on_explicit_zeroize() {
    let mut secret = SecretString::new("super-secret-value".to_owned());
    assert_eq!(secret.expose_secret(), "super-secret-value");
    secret.zeroize();
    assert!(secret.expose_secret().is_empty());
    assert!(secret.is_blank());
}

#[test]
fn debug_output_never_reveals_secrets() -> TestResult {
    let bytes = keypair_bytes(7);
    let encoded = base58_of(&bytes);
    let wallet = SecureWallet::from_secret_str(&encoded)?;

    let wallet_debug = format!("{wallet:?}");
    let secret_debug = format!("{:?}", wallet.secret());
    let string_debug = format!("{:?}", SecretString::new(encoded.clone()));

    for rendered in [&wallet_debug, &secret_debug, &string_debug] {
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(!rendered.contains(&encoded), "{rendered}");
        assert!(!rendered.contains(&json_of(&bytes)), "{rendered}");
    }
    // The public key is not a secret and may be shown.
    assert!(wallet_debug.contains(&wallet.pubkey().to_string()));
    Ok(())
}

// =================================================================================
// Configuration: happy paths
// =================================================================================

#[test]
fn minimal_configuration_applies_safe_defaults() -> TestResult {
    let config = Config::from_vars(minimal_vars())?;

    assert_eq!(config.rpc_url.as_str(), "https://rpc.example.com/");
    assert_eq!(config.jito_engine_url.as_str(), "https://jito.example.com/");
    assert!(config.rpc_ws_url.is_none());
    assert_eq!(config.commitment, CommitmentConfig::confirmed());
    assert_eq!(config.preflight_commitment, CommitmentConfig::processed());
    assert_eq!(config.creator_buy_lamports, 500_000_000);
    assert_eq!(config.slippage_bps, DEFAULT_SLIPPAGE_BPS);
    assert_eq!(config.compute_unit_limit, DEFAULT_COMPUTE_UNIT_LIMIT);
    assert_eq!(
        config.priority_fee_micro_lamports,
        DEFAULT_PRIORITY_FEE_MICRO_LAMPORTS
    );
    assert_eq!(config.jito_tip_lamports, DEFAULT_JITO_TIP_LAMPORTS);
    assert!(
        config.dry_run,
        "dry-run must be ON unless explicitly disabled"
    );
    assert_eq!(config.log_dir, std::path::PathBuf::from("logs"));
    assert_eq!(config.log_level, tracing::Level::INFO);
    Ok(())
}

#[test]
fn every_documented_variable_is_honoured() -> TestResult {
    let key = json_of(&keypair_bytes(8));
    let config = Config::from_vars(vars(&[
        ("RPC_URL", "http://127.0.0.1:8899"),
        ("RPC_WS_URL", "ws://127.0.0.1:8900"),
        (
            "JITO_ENGINE_URL",
            "https://frankfurt.mainnet.block-engine.jito.wtf",
        ),
        ("PRIVATE_KEY", key.as_str()),
        ("COMMITMENT", "finalized"),
        ("PREFLIGHT_COMMITMENT", "confirmed"),
        ("CREATOR_SOL_AMOUNT", "1.25"),
        ("SLIPPAGE_BPS", "1200"),
        ("COMPUTE_UNIT_LIMIT", "400000"),
        ("PRIORITY_FEE_MICRO_LAMPORTS", "250000"),
        ("JITO_TIP_LAMPORTS", "5000000"),
        ("DRY_RUN", "false"),
        ("LOG_DIR", "var/krypton-logs"),
        ("LOG_LEVEL", "debug"),
    ]))?;

    assert_eq!(config.rpc_url.host_str(), Some("127.0.0.1"));
    assert_eq!(config.rpc_url.port(), Some(8899));
    assert_eq!(
        config.rpc_ws_url.as_ref().map(|url| url.scheme()),
        Some("ws")
    );
    assert_eq!(
        config.jito_engine_url.host_str(),
        Some("frankfurt.mainnet.block-engine.jito.wtf")
    );
    assert_eq!(config.commitment, CommitmentConfig::finalized());
    assert_eq!(config.preflight_commitment, CommitmentConfig::confirmed());
    assert_eq!(config.creator_buy_lamports, 1_250_000_000);
    assert_eq!(config.slippage_bps, 1200);
    assert_eq!(config.compute_unit_limit, 400_000);
    assert_eq!(config.priority_fee_micro_lamports, 250_000);
    assert_eq!(config.jito_tip_lamports, 5_000_000);
    assert!(!config.dry_run);
    assert_eq!(config.log_dir, std::path::PathBuf::from("var/krypton-logs"));
    assert_eq!(config.log_level, tracing::Level::DEBUG);

    // The JSON-array key also resolves to the right wallet.
    let expected = keypair_bytes(8);
    let expected_public = expected.last_chunk::<PUBLIC_KEY_BYTES>().ok_or("short")?;
    assert_eq!(&config.load_wallet()?.pubkey_bytes(), expected_public);
    Ok(())
}

#[test]
fn env_key_list_is_complete_and_unique() {
    assert_eq!(ENV_KEYS.len(), 14);
    let unique: std::collections::HashSet<_> = ENV_KEYS.iter().collect();
    assert_eq!(unique.len(), ENV_KEYS.len());
}

#[test]
fn values_are_trimmed_and_commitment_is_case_insensitive() -> TestResult {
    let config = Config::from_vars(with(
        with(minimal_vars(), "COMMITMENT", "  Finalized "),
        "RPC_URL",
        "  https://rpc.example.com  ",
    ))?;
    assert_eq!(config.commitment, CommitmentConfig::finalized());
    assert_eq!(config.rpc_url.host_str(), Some("rpc.example.com"));
    Ok(())
}

#[test]
fn blank_optionals_fall_back_to_defaults_and_unknown_keys_are_ignored() -> TestResult {
    let mut map = minimal_vars();
    for key in [
        "RPC_WS_URL",
        "COMMITMENT",
        "SLIPPAGE_BPS",
        "DRY_RUN",
        "LOG_DIR",
    ] {
        map.insert(key.to_owned(), "   ".to_owned());
    }
    map.insert("SOMETHING_UNRELATED".to_owned(), "ignored".to_owned());

    let config = Config::from_vars(map)?;
    assert!(config.rpc_ws_url.is_none());
    assert_eq!(config.commitment, CommitmentConfig::confirmed());
    assert_eq!(config.slippage_bps, DEFAULT_SLIPPAGE_BPS);
    assert!(config.dry_run);
    assert_eq!(config.log_dir, std::path::PathBuf::from("logs"));
    Ok(())
}

#[test]
fn boundary_values_are_accepted() -> TestResult {
    for (key, value) in [
        ("SLIPPAGE_BPS", "0"),
        ("SLIPPAGE_BPS", "10000"),
        ("COMPUTE_UNIT_LIMIT", "1"),
        ("COMPUTE_UNIT_LIMIT", "1400000"),
        ("JITO_TIP_LAMPORTS", "1000"),
        ("PRIORITY_FEE_MICRO_LAMPORTS", "0"),
        ("CREATOR_SOL_AMOUNT", "0.000000001"),
        ("DRY_RUN", "0"),
        ("DRY_RUN", "On"),
    ] {
        Config::from_vars(with(minimal_vars(), key, value))?;
    }
    Ok(())
}

// =================================================================================
// Configuration: rejection paths
// =================================================================================

#[test]
fn missing_or_blank_required_variables_are_reported_by_name() -> TestResult {
    for name in [
        "RPC_URL",
        "JITO_ENGINE_URL",
        "PRIVATE_KEY",
        "CREATOR_SOL_AMOUNT",
    ] {
        let err = err_of(Config::from_vars(without(minimal_vars(), name)))?;
        assert!(
            matches!(err, ConfigError::MissingVar { name: found } if found == name),
            "{name}: {err}"
        );

        let err = err_of(Config::from_vars(with(minimal_vars(), name, "   ")))?;
        assert!(
            matches!(err, ConfigError::MissingVar { name: found } if found == name),
            "{name} (blank): {err}"
        );
    }
    Ok(())
}

#[test]
fn invalid_values_are_rejected_and_blame_the_right_variable() -> TestResult {
    let cases = [
        ("RPC_URL", "ftp://rpc.example.com"),
        ("RPC_URL", "not a url"),
        ("RPC_URL", "wss://rpc.example.com"),
        ("RPC_URL", "http://"),
        ("RPC_WS_URL", "https://rpc.example.com"),
        ("JITO_ENGINE_URL", "mailto:someone@example.com"),
        ("CREATOR_SOL_AMOUNT", "0"),
        ("CREATOR_SOL_AMOUNT", "0.0"),
        ("CREATOR_SOL_AMOUNT", "-1"),
        ("CREATOR_SOL_AMOUNT", "1e3"),
        ("CREATOR_SOL_AMOUNT", "abc"),
        ("CREATOR_SOL_AMOUNT", "0.0000000001"),
        ("CREATOR_SOL_AMOUNT", "99999999999999999999"),
        ("SLIPPAGE_BPS", "10001"),
        ("SLIPPAGE_BPS", "-5"),
        ("SLIPPAGE_BPS", "5%"),
        ("COMPUTE_UNIT_LIMIT", "0"),
        ("COMPUTE_UNIT_LIMIT", "1400001"),
        ("COMPUTE_UNIT_LIMIT", "lots"),
        ("PRIORITY_FEE_MICRO_LAMPORTS", "-1"),
        ("JITO_TIP_LAMPORTS", "999"),
        ("JITO_TIP_LAMPORTS", "0"),
        ("DRY_RUN", "maybe"),
        ("COMMITMENT", "max"),
        ("PREFLIGHT_COMMITMENT", "fast"),
        ("LOG_LEVEL", "loud"),
        ("PRIVATE_KEY", "definitely-not-a-key"),
        ("PRIVATE_KEY", "[1,2,3]"),
    ];

    for (key, value) in cases {
        let err = err_of(Config::from_vars(with(minimal_vars(), key, value)))?;
        assert!(
            matches!(err, ConfigError::InvalidVar { name, .. } if name == key),
            "{key}={value:?} produced: {err}"
        );
    }
    Ok(())
}

#[test]
fn invalid_private_key_errors_do_not_leak_the_key() -> TestResult {
    let leaky = "AAAA-secret-material-0OIl";
    let err = err_of(Config::from_vars(with(
        minimal_vars(),
        "PRIVATE_KEY",
        leaky,
    )))?;
    assert!(!err.to_string().contains("secret-material"));
    assert!(!format!("{err:?}").contains("secret-material"));
    Ok(())
}

// =================================================================================
// Configuration: redaction and wallet bridge
// =================================================================================

#[test]
fn debug_output_redacts_urls_and_the_private_key() -> TestResult {
    let key = base58_of(&keypair_bytes(7));
    let config = Config::from_vars(vars(&[
        (
            "RPC_URL",
            "https://mainnet.rpc.example/?api-key=SUPERSECRET123",
        ),
        ("RPC_WS_URL", "wss://user:pw@ws.rpc.example/TOKEN777"),
        ("JITO_ENGINE_URL", "https://jito.example:8443/path/TOKEN999"),
        ("PRIVATE_KEY", key.as_str()),
        ("CREATOR_SOL_AMOUNT", "0.5"),
    ]))?;

    let rendered = format!("{config:?}");
    for leaked in [
        "SUPERSECRET123",
        "TOKEN777",
        "TOKEN999",
        "user:pw",
        key.as_str(),
    ] {
        assert!(!rendered.contains(leaked), "Debug leaked {leaked}");
    }
    assert!(rendered.contains("https://mainnet.rpc.example/..."));
    assert!(rendered.contains("https://jito.example:8443/..."));
    assert!(rendered.contains("<redacted>"));
    Ok(())
}

#[test]
fn redact_url_keeps_only_scheme_host_and_port() -> TestResult {
    let url = reqwest::Url::parse("https://u:p@host.example:8443/a/b?k=v#frag")?;
    assert_eq!(redact_url(&url), "https://host.example:8443/...");
    let url = reqwest::Url::parse("wss://host.example/secret")?;
    assert_eq!(redact_url(&url), "wss://host.example/...");
    Ok(())
}

#[test]
fn config_loads_a_working_wallet() -> TestResult {
    let config = Config::from_vars(minimal_vars())?;
    let wallet = config.load_wallet()?;

    let expected = keypair_bytes(7);
    let expected_public = expected.last_chunk::<PUBLIC_KEY_BYTES>().ok_or("short")?;
    assert_eq!(&wallet.pubkey_bytes(), expected_public);
    Ok(())
}

// =================================================================================
// Error hierarchy
// =================================================================================

#[test]
fn launcher_error_wraps_and_displays_every_source() -> TestResult {
    let config: LauncherError = ConfigError::MissingVar { name: "RPC_URL" }.into();
    assert!(config.to_string().contains("configuration error"));
    assert!(config.to_string().contains("RPC_URL"));

    let wallet: LauncherError = WalletError::EmptySecret.into();
    assert!(wallet.to_string().contains("wallet error"));

    let io: LauncherError = std::io::Error::new(std::io::ErrorKind::NotFound, "missing").into();
    assert!(io.to_string().contains("I/O error"));

    let json_err = err_of(serde_json::from_str::<u8>("not json"))?;
    let serialization: LauncherError = json_err.into();
    assert!(serialization.to_string().contains("serialization error"));

    let rpc = LauncherError::Rpc {
        context: "simulate bundle",
        message: "blockhash expired".to_owned(),
    };
    assert_eq!(
        rpc.to_string(),
        "RPC error during simulate bundle: blockhash expired"
    );

    let timeout = LauncherError::Timeout {
        operation: "pinata upload",
        millis: 5_000,
    };
    assert_eq!(
        timeout.to_string(),
        "operation `pinata upload` timed out after 5000 ms"
    );
    Ok(())
}

#[test]
fn http_errors_convert_into_the_network_variant() -> TestResult {
    let client = reqwest::Client::new();
    let http_err = err_of(client.get("not a url").build())?;
    let converted: LauncherError = http_err.into();
    assert!(matches!(converted, LauncherError::Http(_)));
    assert!(converted.to_string().starts_with("HTTP error"));
    Ok(())
}

#[test]
fn question_mark_converts_config_errors_into_the_crate_result() {
    fn load(map: HashMap<String, String>) -> krypton::Result<Config> {
        Ok(Config::from_vars(map)?)
    }
    assert!(load(minimal_vars()).is_ok());
    assert!(matches!(
        load(HashMap::new()),
        Err(LauncherError::Config(ConfigError::MissingVar { .. }))
    ));
}
