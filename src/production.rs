//! Production wiring settings (the mainnet bridge): the Yellowstone endpoint and
//! token, the Jito tip-feed URL and the aliases of two existing variables.
//!
//! Like [`crate::execution::settings`], parsing is pure: nothing here touches the
//! network or the filesystem, every value is validated, nothing is clamped and no
//! error echoes the offending value (endpoints often carry an access token in
//! their path).
//!
//! # Variables
//! | Variable | Meaning |
//! |----------|---------|
//! | `YELLOWSTONE_ENDPOINT` | gRPC endpoint of the Geyser stream (`https`; loopback `http` for tests) |
//! | `YELLOWSTONE_X_TOKEN` | its `x-token` credential; requires the endpoint; scrubbed from the process environment after reading |
//! | `JITO_BLOCK_ENGINE_URL` | alias of the older `JITO_ENGINE_URL`; both may be set only if equal |
//! | `JITO_TIP_STREAM_URL` | the **HTTP** tip-floor endpoint the tip feed polls (`.../bundles/tip_floor`); a WebSocket URL is rejected, the feed does not speak it |
//! | `PAYER_KEYPAIR_PATH` | alias of `LIVE_KEYPAIR_PATH`; both may be set only if equal |
//!
//! The keypair file behind `PAYER_KEYPAIR_PATH` must be a regular file, not a
//! symlink, readable by its owner only (`0600`) on Unix; see
//! [`crate::execution::gate::load_keypair_file`].

use crate::errors::ConfigError;
use crate::network::yellowstone::{check_endpoint, YellowstoneSettings};
use crate::security::SecretString;

/// gRPC endpoint of the Yellowstone/Geyser stream.
pub const ENV_YELLOWSTONE_ENDPOINT: &str = "YELLOWSTONE_ENDPOINT";
/// `x-token` of the Yellowstone endpoint.
pub const ENV_YELLOWSTONE_X_TOKEN: &str = "YELLOWSTONE_X_TOKEN";
/// Alias of [`crate::config::ENV_JITO_ENGINE_URL`].
pub const ENV_JITO_BLOCK_ENGINE_URL: &str = "JITO_BLOCK_ENGINE_URL";
/// HTTP tip-floor endpoint of the tip feed.
pub const ENV_JITO_TIP_STREAM_URL: &str = "JITO_TIP_STREAM_URL";
/// Alias of [`crate::execution::settings::ENV_LIVE_KEYPAIR_PATH`].
pub const ENV_PAYER_KEYPAIR_PATH: &str = "PAYER_KEYPAIR_PATH";

/// Unvalidated production variables, straight from the environment.
#[derive(Default)]
pub struct RawProductionSettings {
    /// `YELLOWSTONE_ENDPOINT`.
    pub yellowstone_endpoint: Option<String>,
    /// `YELLOWSTONE_X_TOKEN`.
    pub yellowstone_x_token: Option<SecretString>,
    /// `JITO_TIP_STREAM_URL`.
    pub jito_tip_stream_url: Option<String>,
}

/// Validated production settings. Everything is optional: a configuration
/// without them is valid (dry runs and tests need none).
#[derive(Default)]
pub struct ProductionSettings {
    /// The Yellowstone endpoint and token, when configured.
    pub yellowstone: Option<YellowstoneSettings>,
    /// The tip-floor URL, when configured. Absent: the tip feed is not started
    /// (or uses the public mainnet endpoint, the caller's choice).
    pub tip_feed_url: Option<String>,
}

impl std::fmt::Debug for ProductionSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProductionSettings")
            .field("yellowstone", &self.yellowstone)
            .field("tip_feed_url", &self.tip_feed_url.is_some())
            .finish()
    }
}

fn invalid(name: &'static str, reason: &str) -> ConfigError {
    ConfigError::InvalidVar {
        name,
        reason: reason.to_owned(),
    }
}

fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// Resolves a variable that has an older and a newer name.
///
/// Blank counts as unset. If both are set they must be identical; a conflict is
/// an error (never a silent preference) naming the alias, not the values.
///
/// # Errors
/// [`ConfigError::InvalidVar`] for `alias_name` when both are set and differ.
pub fn merge_alias(
    canonical_name: &'static str,
    alias_name: &'static str,
    canonical: Option<String>,
    alias: Option<String>,
) -> Result<Option<String>, ConfigError> {
    match (clean(canonical), clean(alias)) {
        (Some(first), Some(second)) if first != second => Err(invalid(
            alias_name,
            &format!("conflicts with {canonical_name}; set only one of them"),
        )),
        (Some(first), _) => Ok(Some(first)),
        (None, second) => Ok(second),
    }
}

fn is_websocket(url: &str) -> bool {
    let lower = url.get(..6).unwrap_or(url).to_ascii_lowercase();
    lower.starts_with("ws://") || lower.starts_with("wss://")
}

impl ProductionSettings {
    /// Validates the raw variables.
    ///
    /// # Errors
    /// [`ConfigError::InvalidVar`] for a malformed endpoint, a token without an
    /// endpoint, a malformed token or a bad tip-feed URL.
    pub fn from_raw(raw: RawProductionSettings) -> Result<Self, ConfigError> {
        let endpoint = clean(raw.yellowstone_endpoint);
        let token = raw.yellowstone_x_token.filter(|secret| !secret.is_blank());
        let yellowstone = match (endpoint, token) {
            (None, None) => None,
            (None, Some(_)) => {
                return Err(invalid(
                    ENV_YELLOWSTONE_X_TOKEN,
                    "is set but YELLOWSTONE_ENDPOINT is not",
                ))
            }
            (Some(endpoint), token) => {
                Some(YellowstoneSettings::new(&endpoint, token).map_err(|error| {
                    let name = match error {
                        crate::errors::YellowstoneError::InvalidEndpoint { .. } => {
                            ENV_YELLOWSTONE_ENDPOINT
                        }
                        crate::errors::YellowstoneError::InvalidToken { .. } => {
                            ENV_YELLOWSTONE_X_TOKEN
                        }
                    };
                    invalid(name, &error.to_string())
                })?)
            }
        };

        let tip_feed_url = match clean(raw.jito_tip_stream_url) {
            None => None,
            Some(url) => {
                if is_websocket(&url) {
                    return Err(invalid(
                        ENV_JITO_TIP_STREAM_URL,
                        "a WebSocket tip stream is not supported; use the HTTP tip_floor URL",
                    ));
                }
                check_endpoint(&url).map_err(|reason| invalid(ENV_JITO_TIP_STREAM_URL, reason))?;
                Some(url)
            }
        };
        Ok(Self {
            yellowstone,
            tip_feed_url,
        })
    }
}
