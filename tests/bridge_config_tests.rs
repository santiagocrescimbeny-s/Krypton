//! Production-bridge tests that need the real configuration loader, the process
//! environment or the `reqwest`/`serde_json` based sources.
//!
//! Conventions
//! - Tests return `Result` and use `?` / `matches!`; no `unwrap()` / `expect()`.
//! - Nothing here touches the network.
//! - The tests that edit the process environment serialize on a mutex and start
//!   from a clean slate, like `env_hygiene_tests.rs`.

use std::collections::HashMap;
use std::error::Error;
use std::sync::{Mutex, MutexGuard};

use krypton::config::{Config, ENV_KEYS, PRODUCTION_ENV_KEYS};
use krypton::errors::{BridgeError, ConfigError};
use krypton::network::rpc::{
    parse_blockhash_response, parse_token_balance_response, RpcBlockhashSource, RpcClient,
};
use reqwest::Url;
use solana_keypair::Keypair;
use solana_sdk::hash::Hash;

type TestResult = Result<(), Box<dyn Error>>;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn err_of<T, E>(result: Result<T, E>) -> Result<E, Box<dyn Error>> {
    match result {
        Ok(_) => Err("expected an error but the operation succeeded".into()),
        Err(err) => Ok(err),
    }
}

fn private_key_text() -> String {
    bs58::encode(Keypair::new_from_array([7u8; 32]).to_bytes()).into_string()
}

fn minimal() -> HashMap<String, String> {
    [
        ("RPC_URL", "https://rpc.example.com".to_owned()),
        ("JITO_ENGINE_URL", "https://jito.example.com".to_owned()),
        ("PRIVATE_KEY", private_key_text()),
        ("CREATOR_SOL_AMOUNT", "0.5".to_owned()),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value))
    .collect()
}

fn with(mut vars: HashMap<String, String>, name: &str, value: &str) -> HashMap<String, String> {
    vars.insert(name.to_owned(), value.to_owned());
    vars
}

// =====================================================================================================
// The new variables
// =====================================================================================================

#[test]
fn the_core_key_list_is_unchanged_and_the_production_keys_are_separate() {
    // phase1_tests.rs pins the first list at 21: the bridge must not move it.
    assert_eq!(ENV_KEYS.len(), 21);
    assert_eq!(PRODUCTION_ENV_KEYS.len(), 6);
    let all: std::collections::HashSet<_> =
        ENV_KEYS.iter().chain(PRODUCTION_ENV_KEYS.iter()).collect();
    assert_eq!(all.len(), 27, "the two lists overlap");
    assert!(PRODUCTION_ENV_KEYS.contains(&"GUARD_STATE_PATH"));
}

#[test]
fn without_the_new_variables_the_configuration_is_as_before() -> TestResult {
    let config = Config::from_vars(minimal())?;
    assert!(config.production.yellowstone.is_none());
    assert!(config.production.tip_feed_url.is_none());
    assert!(config.live.keypair_path.is_none());
    Ok(())
}

#[test]
fn the_yellowstone_and_tip_variables_are_read() -> TestResult {
    let mut vars = with(
        minimal(),
        "YELLOWSTONE_ENDPOINT",
        "https://grpc.example.com:443",
    );
    vars = with(vars, "YELLOWSTONE_X_TOKEN", "abc123token");
    vars = with(
        vars,
        "JITO_TIP_STREAM_URL",
        "https://bundles.jito.wtf/api/v1/bundles/tip_floor",
    );
    let config = Config::from_vars(vars)?;
    let yellowstone = config
        .production
        .yellowstone
        .as_ref()
        .ok_or("no yellowstone settings")?;
    assert_eq!(yellowstone.origin(), "https://grpc.example.com:443");
    assert!(yellowstone.x_token().is_some());
    assert_eq!(
        config.production.tip_feed_url.as_deref(),
        Some("https://bundles.jito.wtf/api/v1/bundles/tip_floor")
    );
    Ok(())
}

#[test]
fn bad_production_values_fail_the_configuration_without_being_echoed() -> TestResult {
    let secret = "supersecretvalue";
    for (name, value) in [
        (
            "YELLOWSTONE_ENDPOINT",
            format!("http://{secret}.example.com"),
        ),
        (
            "YELLOWSTONE_ENDPOINT",
            format!("https://user:{secret}@example.com"),
        ),
        ("YELLOWSTONE_X_TOKEN", secret.to_owned()), // a token without an endpoint
        ("JITO_TIP_STREAM_URL", format!("wss://{secret}.example.com")),
    ] {
        let error = err_of(Config::from_vars(with(minimal(), name, &value)))?;
        assert!(
            matches!(error, ConfigError::InvalidVar { name: reported, .. } if reported == name),
            "{name}: {error:?}"
        );
        let text = format!("{error} {error:?}");
        assert!(!text.contains(secret), "{name} leaked: {text}");
    }
    Ok(())
}

#[test]
fn the_debug_of_the_configuration_hides_the_new_secrets() -> TestResult {
    let mut vars = with(
        minimal(),
        "YELLOWSTONE_ENDPOINT",
        "https://grpc.example.com/very-secret-path",
    );
    vars = with(vars, "YELLOWSTONE_X_TOKEN", "tok3n-value");
    vars = with(
        vars,
        "JITO_TIP_STREAM_URL",
        "https://tips.example.com/key-in-path",
    );
    vars = with(
        vars,
        "LIVE_KEYPAIR_PATH",
        "/home/someone/secret-dir/key.json",
    );
    let text = format!("{:?}", Config::from_vars(vars)?);
    for secret in [
        "very-secret-path",
        "tok3n-value",
        "key-in-path",
        "secret-dir",
    ] {
        assert!(!text.contains(secret), "leaked {secret}: {text}");
    }
    Ok(())
}

// =====================================================================================================
// The aliases
// =====================================================================================================

#[test]
fn payer_keypair_path_is_an_alias_of_live_keypair_path() -> TestResult {
    let new_name = Config::from_vars(with(minimal(), "PAYER_KEYPAIR_PATH", "/secure/payer.json"))?;
    assert_eq!(
        new_name.live.keypair_path.as_deref(),
        Some(std::path::Path::new("/secure/payer.json"))
    );
    let old_name = Config::from_vars(with(minimal(), "LIVE_KEYPAIR_PATH", "/secure/payer.json"))?;
    assert_eq!(new_name.live.keypair_path, old_name.live.keypair_path);

    // Both, equal: fine. Both, different: an error that names the alias.
    let both_equal = with(
        with(minimal(), "LIVE_KEYPAIR_PATH", "/secure/payer.json"),
        "PAYER_KEYPAIR_PATH",
        "/secure/payer.json",
    );
    assert!(Config::from_vars(both_equal).is_ok());
    let both_different = with(
        with(minimal(), "LIVE_KEYPAIR_PATH", "/secure/one.json"),
        "PAYER_KEYPAIR_PATH",
        "/secure/two.json",
    );
    let error = err_of(Config::from_vars(both_different))?;
    assert!(matches!(
        error,
        ConfigError::InvalidVar {
            name: "PAYER_KEYPAIR_PATH",
            ..
        }
    ));
    let text = error.to_string();
    assert!(!text.contains("one.json") && !text.contains("two.json"));
    Ok(())
}

#[test]
fn jito_block_engine_url_is_an_alias_of_jito_engine_url() -> TestResult {
    let mut only_new = minimal();
    only_new.remove("JITO_ENGINE_URL");
    let only_new = with(
        only_new,
        "JITO_BLOCK_ENGINE_URL",
        "https://block.example.com",
    );
    let config = Config::from_vars(only_new)?;
    assert_eq!(config.jito_engine_url.host_str(), Some("block.example.com"));

    // Neither: the same MissingVar as before the bridge existed.
    let mut neither = minimal();
    neither.remove("JITO_ENGINE_URL");
    let error = err_of(Config::from_vars(neither))?;
    assert!(matches!(
        error,
        ConfigError::MissingVar {
            name: "JITO_ENGINE_URL"
        }
    ));

    // Both, different: refused.
    let conflict = with(
        minimal(),
        "JITO_BLOCK_ENGINE_URL",
        "https://other.example.com",
    );
    let error = err_of(Config::from_vars(conflict))?;
    assert!(matches!(
        error,
        ConfigError::InvalidVar {
            name: "JITO_BLOCK_ENGINE_URL",
            ..
        }
    ));
    Ok(())
}

// =====================================================================================================
// The process environment
// =====================================================================================================

fn clear_env() {
    for key in ENV_KEYS.into_iter().chain(PRODUCTION_ENV_KEYS) {
        std::env::remove_var(key);
    }
}

#[test]
fn the_x_token_is_scrubbed_from_the_process_environment() -> TestResult {
    let _guard = lock();
    clear_env();
    let dir = std::env::temp_dir().join(format!("krypton-bridge-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let dotenv = dir.join("test.env");
    std::fs::write(
        &dotenv,
        format!(
            "RPC_URL=https://rpc.example.com\nJITO_ENGINE_URL=https://jito.example.com\n\
             PRIVATE_KEY={}\nCREATOR_SOL_AMOUNT=0.5\n\
             YELLOWSTONE_ENDPOINT=https://grpc.example.com\nYELLOWSTONE_X_TOKEN=tok3n-value\n",
            private_key_text()
        ),
    )?;
    let config = Config::from_env_file(&dotenv);
    let token_left = std::env::var("YELLOWSTONE_X_TOKEN").is_ok();
    let key_left = std::env::var("PRIVATE_KEY").is_ok();
    clear_env();
    let _ = std::fs::remove_dir_all(&dir);

    let config = config?;
    assert!(!token_left, "YELLOWSTONE_X_TOKEN stayed in the environment");
    assert!(!key_left, "PRIVATE_KEY stayed in the environment");
    // ...and the token still reached the settings.
    let yellowstone = config
        .production
        .yellowstone
        .as_ref()
        .ok_or("no yellowstone settings")?;
    assert!(yellowstone.x_token().is_some());
    Ok(())
}

#[test]
fn the_x_token_is_scrubbed_even_when_the_configuration_is_invalid() -> TestResult {
    let _guard = lock();
    clear_env();
    std::env::set_var("YELLOWSTONE_X_TOKEN", "tok3n-value");
    // No RPC_URL etc.: validation fails.
    let result = Config::from_env_file(std::path::Path::new("/nonexistent/krypton.env"));
    let token_left = std::env::var("YELLOWSTONE_X_TOKEN").is_ok();
    clear_env();
    assert!(result.is_err());
    assert!(!token_left, "YELLOWSTONE_X_TOKEN stayed in the environment");
    Ok(())
}

// =====================================================================================================
// The RPC blockhash source
// =====================================================================================================

fn blockhash_answer(blockhash: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","result":{{"context":{{"slot":123}},"value":{{"blockhash":"{blockhash}","lastValidBlockHeight":456}}}},"id":1}}"#
    )
}

#[test]
fn a_getlatestblockhash_answer_is_parsed() -> TestResult {
    let hash = Hash::new_from_array([9; 32]);
    let parsed = parse_blockhash_response(blockhash_answer(&hash.to_string()).as_bytes())?;
    assert_eq!(parsed, hash);
    Ok(())
}

#[test]
fn anything_else_is_malformed() -> TestResult {
    let good = Hash::new_from_array([9; 32]).to_string();
    for body in [
        String::new(),
        "not json".to_owned(),
        "[]".to_owned(),
        r#"{"jsonrpc":"2.0","error":{"code":-32005,"message":"Node is behind"},"id":1}"#.to_owned(),
        r#"{"jsonrpc":"2.0","result":{"value":{}},"id":1}"#.to_owned(),
        blockhash_answer("not-a-hash"),
        blockhash_answer(""),
        blockhash_answer(&format!("{good}{good}")),
    ] {
        assert_eq!(
            err_of(parse_blockhash_response(body.as_bytes()))?,
            BridgeError::RpcMalformed,
            "{body}"
        );
    }
    Ok(())
}

#[test]
fn rpc_errors_carry_no_remote_text() -> TestResult {
    let body = r#"{"jsonrpc":"2.0","error":{"code":-32005,"message":"REMOTE-TEXT-secret"},"id":1}"#;
    let error = err_of(parse_blockhash_response(body.as_bytes()))?;
    assert!(!format!("{error} {error:?}").contains("REMOTE-TEXT"));
    Ok(())
}

#[test]
fn the_source_accepts_http_urls_and_hides_them() -> TestResult {
    let url = Url::parse("https://rpc.example.com/?api-key=SECRET-KEY")?;
    let source = RpcBlockhashSource::new(&url)?;
    assert!(!format!("{source:?}").contains("SECRET-KEY"));
    let local = Url::parse("http://127.0.0.1:8899")?;
    assert!(RpcBlockhashSource::new(&local).is_ok());
    let wrong = Url::parse("wss://rpc.example.com")?;
    assert!(matches!(
        err_of(RpcBlockhashSource::new(&wrong))?,
        BridgeError::RpcConfig { .. }
    ));
    Ok(())
}

// =====================================================================================================
// Audit #47: https for the endpoints that carry orders and keys
// =====================================================================================================

#[test]
fn plain_http_is_refused_for_rpc_and_jito_unless_the_host_is_loopback() -> TestResult {
    let secret = "supersecretkey";
    for name in ["RPC_URL", "JITO_ENGINE_URL"] {
        for refused in [
            format!("http://rpc.example.com/?api-key={secret}"),
            format!("http://{secret}.example.com"),
            "http://10.0.0.5:8899".to_owned(),
            "http://localhost.evil.com".to_owned(),
            "http://127.0.0.1.evil.com".to_owned(),
        ] {
            let error = err_of(Config::from_vars(with(minimal(), name, &refused)))?;
            assert!(
                matches!(error, ConfigError::InvalidVar { name: reported, .. } if reported == name),
                "{name} {refused}: {error:?}"
            );
            let text = format!("{error} {error:?}");
            assert!(!text.contains(secret), "{name} echoed the URL: {text}");
        }
        for allowed in [
            "https://rpc.example.com",
            "https://rpc.example.com:8899/?api-key=k",
            "http://localhost:8899",
            "http://127.0.0.1:8899",
            "http://[::1]:8899",
        ] {
            Config::from_vars(with(minimal(), name, allowed))
                .map_err(|error| format!("{name} {allowed}: {error}"))?;
        }
    }
    Ok(())
}

#[test]
fn other_schemes_stay_refused() -> TestResult {
    for name in ["RPC_URL", "JITO_ENGINE_URL"] {
        for refused in [
            "ftp://rpc.example.com",
            "wss://rpc.example.com",
            "file:///etc/passwd",
        ] {
            assert!(
                Config::from_vars(with(minimal(), name, refused)).is_err(),
                "{name} {refused}"
            );
        }
    }
    Ok(())
}

// =====================================================================================================
// Audit #35: the state file path
// =====================================================================================================

#[test]
fn the_guard_state_path_is_read_and_never_printed() -> TestResult {
    let without = Config::from_vars(minimal())?;
    assert!(without.live.guard_state_path.is_none());

    let config = Config::from_vars(with(
        minimal(),
        "GUARD_STATE_PATH",
        "  /var/lib/krypton/secret-dir/guards.state  ",
    ))?;
    assert_eq!(
        config.live.guard_state_path.as_deref(),
        Some(std::path::Path::new(
            "/var/lib/krypton/secret-dir/guards.state"
        ))
    );
    assert!(!format!("{config:?}").contains("secret-dir"));

    let blank = Config::from_vars(with(minimal(), "GUARD_STATE_PATH", "   "))?;
    assert!(
        blank.live.guard_state_path.is_none(),
        "blank counts as unset"
    );
    Ok(())
}

// =====================================================================================================
// Audit #45: getTokenAccountBalance
// =====================================================================================================

fn balance_answer(amount: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","result":{{"context":{{"slot":1}},"value":{{"amount":"{amount}","decimals":6,"uiAmount":1.0,"uiAmountString":"1"}}}},"id":1}}"#
    )
}

#[test]
fn a_gettokenaccountbalance_answer_is_parsed_in_base_units() -> TestResult {
    assert_eq!(
        parse_token_balance_response(balance_answer("0").as_bytes())?,
        0
    );
    assert_eq!(
        parse_token_balance_response(balance_answer("97500000000003").as_bytes())?,
        97_500_000_000_003
    );
    // Above 2^53: the amount is a string precisely so it stays exact.
    assert_eq!(
        parse_token_balance_response(balance_answer("18446744073709551615").as_bytes())?,
        u64::MAX
    );
    Ok(())
}

#[test]
fn an_unusable_balance_answer_is_malformed_never_zero() -> TestResult {
    for body in [
        String::new(),
        "not json".to_owned(),
        r#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"Invalid param: could not find account"},"id":1}"#
            .to_owned(),
        r#"{"jsonrpc":"2.0","result":{"value":{}},"id":1}"#.to_owned(),
        r#"{"jsonrpc":"2.0","result":{"value":{"amount":5}},"id":1}"#.to_owned(),
        balance_answer("-1"),
        balance_answer("1.5"),
        balance_answer("18446744073709551616"),
        balance_answer(""),
        balance_answer("0x10"),
    ] {
        assert_eq!(
            err_of(parse_token_balance_response(body.as_bytes()))?,
            BridgeError::RpcMalformed,
            "{body}"
        );
    }
    Ok(())
}

#[test]
fn balance_errors_carry_no_remote_text() -> TestResult {
    let body = r#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"REMOTE-TEXT-secret"},"id":1}"#;
    let error = err_of(parse_token_balance_response(body.as_bytes()))?;
    assert!(!format!("{error} {error:?}").contains("REMOTE-TEXT"));
    Ok(())
}

#[test]
fn the_rpc_client_enforces_the_transport_policy_and_hides_its_url() -> TestResult {
    let secret = Url::parse("https://rpc.example.com/?api-key=SECRET-KEY")?;
    let client = RpcClient::new(&secret)?;
    assert!(!format!("{client:?}").contains("SECRET-KEY"));
    for local in [
        "http://127.0.0.1:8899",
        "http://localhost:8899",
        "http://[::1]:8899",
    ] {
        assert!(RpcClient::new(&Url::parse(local)?).is_ok(), "{local}");
    }
    for refused in [
        "http://rpc.example.com",
        "http://10.0.0.5:8899",
        "wss://rpc.example.com",
        "ftp://rpc.example.com",
    ] {
        assert!(
            matches!(
                err_of(RpcClient::new(&Url::parse(refused)?))?,
                BridgeError::RpcConfig { .. }
            ),
            "{refused}"
        );
        assert!(
            RpcBlockhashSource::new(&Url::parse(refused)?).is_err(),
            "{refused}"
        );
    }
    Ok(())
}
