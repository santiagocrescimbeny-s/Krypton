//! Process-environment hygiene of `Config::from_env` / `Config::from_env_file`
//! (audit findings #4 and #11).
//!
//! These tests mutate the process environment and the working directory, which
//! are global. They therefore live in their own test binary and serialize on a
//! mutex; every test starts from a clean slate and restores the working
//! directory when it ends.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use krypton::config::{Config, DEFAULT_DOTENV_PATH, ENV_KEYS};
use krypton::errors::ConfigError;
use solana_keypair::Keypair;

type TestResult = Result<(), Box<dyn Error>>;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> Result<MutexGuard<'static, ()>, Box<dyn Error>> {
    // A poisoned lock only means another test failed; the guard is still usable.
    Ok(ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()))
}

fn err_of<T, E>(result: Result<T, E>) -> Result<E, Box<dyn Error>> {
    match result {
        Ok(_) => Err("expected an error but the operation succeeded".into()),
        Err(err) => Ok(err),
    }
}

fn clear_env() {
    for key in ENV_KEYS {
        std::env::remove_var(key);
    }
}

fn private_key_text() -> String {
    bs58::encode(Keypair::new_from_array([7u8; 32]).to_bytes()).into_string()
}

fn dotenv_contents(rpc_url: &str) -> String {
    format!(
        "RPC_URL={rpc_url}\nJITO_ENGINE_URL=https://jito.example.com\nPRIVATE_KEY={}\nCREATOR_SOL_AMOUNT=0.5\n",
        private_key_text()
    )
}

/// A scratch directory removed on drop; the working directory is restored too.
struct Scratch {
    root: PathBuf,
    original_cwd: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Result<Self, Box<dyn Error>> {
        let root = std::env::temp_dir().join(format!("krypton-env-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            original_cwd: std::env::current_dir()?,
        })
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original_cwd);
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn from_env_file_scrubs_the_private_key_from_the_environment() -> TestResult {
    let _guard = lock()?;
    clear_env();
    let scratch = Scratch::new("scrub")?;
    let file = scratch.path().join(".env");
    fs::write(&file, dotenv_contents("https://rpc.example.com"))?;

    let config = Config::from_env_file(&file)?;

    // Loaded from the explicit file...
    assert_eq!(config.rpc_url.host_str(), Some("rpc.example.com"));
    assert_eq!(config.creator_buy_lamports, 500_000_000);
    // ...and the secret is gone from the process environment afterwards.
    assert!(std::env::var("PRIVATE_KEY").is_err());
    assert!(!std::env::vars().any(|(_, value)| value == private_key_text()));
    // The wallet survives in the config.
    let expected = Keypair::new_from_array([7u8; 32]).to_bytes();
    assert_eq!(config.load_wallet()?.secret().expose(), &expected);
    clear_env();
    Ok(())
}

#[test]
fn the_private_key_is_scrubbed_even_when_validation_fails() -> TestResult {
    let _guard = lock()?;
    clear_env();
    let scratch = Scratch::new("scrub-fail")?;
    let file = scratch.path().join(".env");
    // Missing CREATOR_SOL_AMOUNT: validation fails after the key was read.
    fs::write(
        &file,
        format!(
            "RPC_URL=https://rpc.example.com\nJITO_ENGINE_URL=https://jito.example.com\nPRIVATE_KEY={}\n",
            private_key_text()
        ),
    )?;

    let err = err_of(Config::from_env_file(&file))?;
    assert!(matches!(
        err,
        ConfigError::MissingVar {
            name: "CREATOR_SOL_AMOUNT"
        }
    ));
    assert!(std::env::var("PRIVATE_KEY").is_err());
    clear_env();
    Ok(())
}

#[test]
fn a_key_exported_in_the_shell_is_also_scrubbed() -> TestResult {
    let _guard = lock()?;
    clear_env();
    let scratch = Scratch::new("scrub-shell")?;
    let file = scratch.path().join("missing.env");

    std::env::set_var("RPC_URL", "https://rpc.example.com");
    std::env::set_var("JITO_ENGINE_URL", "https://jito.example.com");
    std::env::set_var("PRIVATE_KEY", private_key_text());
    std::env::set_var("CREATOR_SOL_AMOUNT", "1");

    let config = Config::from_env_file(&file)?;
    assert_eq!(config.creator_buy_lamports, 1_000_000_000);
    assert!(std::env::var("PRIVATE_KEY").is_err());
    // Non-secret variables are left alone.
    assert_eq!(
        std::env::var("RPC_URL").as_deref(),
        Ok("https://rpc.example.com")
    );
    clear_env();
    Ok(())
}

#[test]
fn process_environment_wins_over_the_dotenv_file() -> TestResult {
    let _guard = lock()?;
    clear_env();
    let scratch = Scratch::new("precedence")?;
    let file = scratch.path().join(".env");
    fs::write(&file, dotenv_contents("https://from-file.example.com"))?;
    std::env::set_var("RPC_URL", "https://from-env.example.com");

    let config = Config::from_env_file(&file)?;
    assert_eq!(config.rpc_url.host_str(), Some("from-env.example.com"));
    clear_env();
    Ok(())
}

#[test]
fn a_malformed_dotenv_file_is_reported_without_its_contents() -> TestResult {
    let _guard = lock()?;
    clear_env();
    let scratch = Scratch::new("malformed")?;
    let file = scratch.path().join(".env");
    fs::write(
        &file,
        "PRIVATE_KEY=top-secret-value\nthis line is not valid ===\n",
    )?;

    let err = err_of(Config::from_env_file(&file))?;
    assert!(matches!(err, ConfigError::DotEnvSyntax { .. }));
    assert!(!err.to_string().contains("top-secret-value"));
    assert!(!format!("{err:?}").contains("top-secret-value"));
    // dotenvy applies the file line by line: the key from line 1 may already be
    // exported when line 2 fails, and it must not stay in the environment.
    assert!(std::env::var("PRIVATE_KEY").is_err());
    clear_env();
    Ok(())
}

#[test]
fn from_env_never_reads_dotenv_files_from_parent_directories() -> TestResult {
    let _guard = lock()?;
    clear_env();
    let scratch = Scratch::new("parents")?;
    let child = scratch.path().join("child");
    fs::create_dir_all(&child)?;
    // A perfectly valid `.env` in the PARENT directory only.
    fs::write(
        scratch.path().join(DEFAULT_DOTENV_PATH),
        dotenv_contents("https://rpc.example.com"),
    )?;

    // From the child directory the parent's file must be invisible.
    std::env::set_current_dir(&child)?;
    let err = err_of(Config::from_env())?;
    assert!(matches!(err, ConfigError::MissingVar { name: "RPC_URL" }));

    // From the directory that actually holds the file it is picked up.
    std::env::set_current_dir(scratch.path())?;
    let config = Config::from_env()?;
    assert_eq!(config.rpc_url.host_str(), Some("rpc.example.com"));
    assert!(std::env::var("PRIVATE_KEY").is_err());
    clear_env();
    Ok(())
}
