//! The RPC half of the production bridge: the recent-blockhash source.
//!
//! One JSON-RPC call, `getLatestBlockhash`, over `reqwest`. It feeds
//! [`crate::app::run_blockhash_refresher`] and seeds the blockhash channel the
//! sell submitters read.
//!
//! The URL is a secret (providers embed API keys in it): it is never logged and
//! no error carries it. Responses are size-capped, redirects are disabled and
//! no remote text reaches an error.

use std::str::FromStr;
use std::time::Duration;

use reqwest::{Client, Url};
use serde::Deserialize;
use solana_sdk::hash::Hash;

use crate::app::BlockhashSource;
use crate::errors::BridgeError;

/// Largest response accepted, in bytes. The real answer is about 150.
pub const MAX_RPC_RESPONSE_BYTES: usize = 16 * 1024;

/// Hard per-request deadline of the client (the refresher applies its own,
/// tighter one).
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

/// The commitment of the blockhash: `confirmed` is the usual choice for
/// building transactions (a `processed` blockhash may sit on a fork).
const BLOCKHASH_COMMITMENT: &str = "confirmed";

const USER_AGENT: &str = concat!("krypton/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Deserialize)]
struct BlockhashValue {
    blockhash: String,
}

#[derive(Debug, Deserialize)]
struct BlockhashResult {
    value: BlockhashValue,
}

#[derive(Debug, Deserialize)]
struct RpcEnvelope {
    result: Option<BlockhashResult>,
}

/// Parses a `getLatestBlockhash` response.
///
/// # Errors
/// [`BridgeError::RpcMalformed`] for anything that is not a successful answer
/// carrying a valid blockhash (an `error` object included).
pub fn parse_blockhash_response(body: &[u8]) -> Result<Hash, BridgeError> {
    let envelope: RpcEnvelope =
        serde_json::from_slice(body).map_err(|_| BridgeError::RpcMalformed)?;
    let result = envelope.result.ok_or(BridgeError::RpcMalformed)?;
    Hash::from_str(&result.value.blockhash).map_err(|_| BridgeError::RpcMalformed)
}

fn request_body() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getLatestBlockhash",
        "params": [{ "commitment": BLOCKHASH_COMMITMENT }],
    })
}

/// A [`BlockhashSource`] backed by a Solana JSON-RPC node.
///
/// `Debug` prints nothing of the URL.
pub struct RpcBlockhashSource {
    client: Client,
    url: Url,
}

impl std::fmt::Debug for RpcBlockhashSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcBlockhashSource").finish_non_exhaustive()
    }
}

impl RpcBlockhashSource {
    /// Builds a source for the node at `url`.
    ///
    /// # Errors
    /// [`BridgeError::RpcConfig`] if the URL is not `http`/`https` or the HTTP
    /// client cannot be built.
    pub fn new(url: &Url) -> Result<Self, BridgeError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(BridgeError::RpcConfig {
                reason: "the RPC URL must be http or https",
            });
        }
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(CLIENT_TIMEOUT)
            .connect_timeout(CLIENT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| BridgeError::RpcConfig {
                reason: "the HTTP client cannot be built",
            })?;
        Ok(Self {
            client,
            url: url.clone(),
        })
    }
}

impl BlockhashSource for RpcBlockhashSource {
    async fn latest(&mut self) -> Result<Hash, BridgeError> {
        let mut response = self
            .client
            .post(self.url.clone())
            .json(&request_body())
            .send()
            .await
            .map_err(|_| BridgeError::BlockhashUnavailable)?;
        let status = response.status();
        if !status.is_success() {
            return Err(BridgeError::RpcStatus {
                status: status.as_u16(),
            });
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| BridgeError::BlockhashUnavailable)?
        {
            if body.len().saturating_add(chunk.len()) > MAX_RPC_RESPONSE_BYTES {
                return Err(BridgeError::RpcResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        parse_blockhash_response(&body)
    }
}
