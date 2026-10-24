//! The RPC half of the production bridge: the recent-blockhash source and the
//! token-account balance reader.
//!
//! Two JSON-RPC calls over `reqwest`, both through [`RpcClient`]:
//! `getLatestBlockhash` (it feeds [`crate::app::run_blockhash_refresher`] and
//! seeds the blockhash channel the sell submitters read) and
//! `getTokenAccountBalance` at `confirmed` (it settles orders whose outcome is
//! unknown, audit finding #45).
//!
//! The URL is a secret (providers embed API keys in it): it is never logged and
//! no error carries it. Responses are size-capped, redirects are disabled and
//! no remote text reaches an error.

use std::str::FromStr;
use std::time::Duration;

use reqwest::{Client, Url};
use serde::Deserialize;
use solana_pubkey::Pubkey;
use solana_sdk::hash::Hash;

use crate::app::BlockhashSource;
use crate::errors::BridgeError;
use crate::parsing::endpoint_scheme_allowed;

/// Largest response accepted, in bytes. The real answer is about 150.
pub const MAX_RPC_RESPONSE_BYTES: usize = 16 * 1024;

/// Hard per-request deadline of the client (the refresher applies its own,
/// tighter one).
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

/// The commitment of the blockhash: `confirmed` is the usual choice for
/// building transactions (a `processed` blockhash may sit on a fork).
const BLOCKHASH_COMMITMENT: &str = "confirmed";

/// The commitment of the token balance read: `confirmed`, so a sale that landed
/// a moment ago is already visible and a balance on a fork is not trusted.
pub const TOKEN_BALANCE_COMMITMENT: &str = "confirmed";

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

#[derive(Debug, Deserialize)]
struct TokenAmount {
    /// The raw amount in base units, as a decimal string (it can exceed 2^53).
    amount: String,
}

#[derive(Debug, Deserialize)]
struct TokenBalanceResult {
    value: TokenAmount,
}

#[derive(Debug, Deserialize)]
struct TokenBalanceEnvelope {
    result: Option<TokenBalanceResult>,
}

/// Parses a `getTokenAccountBalance` response into base units.
///
/// # Errors
/// [`BridgeError::RpcMalformed`] for anything that is not a successful answer
/// carrying a decimal `amount` that fits a `u64` (an `error` object, such as
/// "could not find account", included).
pub fn parse_token_balance_response(body: &[u8]) -> Result<u64, BridgeError> {
    let envelope: TokenBalanceEnvelope =
        serde_json::from_slice(body).map_err(|_| BridgeError::RpcMalformed)?;
    let result = envelope.result.ok_or(BridgeError::RpcMalformed)?;
    result
        .value
        .amount
        .parse::<u64>()
        .map_err(|_| BridgeError::RpcMalformed)
}

fn token_balance_body(account: &Pubkey) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getTokenAccountBalance",
        "params": [account.to_string(), { "commitment": TOKEN_BALANCE_COMMITMENT }],
    })
}

fn request_body() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getLatestBlockhash",
        "params": [{ "commitment": BLOCKHASH_COMMITMENT }],
    })
}

/// A JSON-RPC client for one Solana node.
///
/// `Debug` prints nothing of the URL.
pub struct RpcClient {
    client: Client,
    url: Url,
}

impl std::fmt::Debug for RpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcClient").finish_non_exhaustive()
    }
}

impl RpcClient {
    /// Builds a client for the node at `url`.
    ///
    /// # Errors
    /// [`BridgeError::RpcConfig`] if the URL is not `https` (plain `http` is
    /// accepted for loopback hosts only, audit finding #47) or the HTTP client
    /// cannot be built.
    pub fn new(url: &Url) -> Result<Self, BridgeError> {
        let host = url.host_str().unwrap_or_default();
        if !endpoint_scheme_allowed(url.scheme(), host) {
            return Err(BridgeError::RpcConfig {
                reason: "the RPC URL must be https (http only for loopback hosts)",
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

    /// Posts one JSON-RPC request and returns the size-capped body of a
    /// successful HTTP answer.
    async fn call(&self, request: &serde_json::Value) -> Result<Vec<u8>, BridgeError> {
        let mut response = self
            .client
            .post(self.url.clone())
            .json(request)
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
        Ok(body)
    }

    /// The latest blockhash at `confirmed`.
    ///
    /// # Errors
    /// [`BridgeError::BlockhashUnavailable`], [`BridgeError::RpcStatus`],
    /// [`BridgeError::RpcResponseTooLarge`] or [`BridgeError::RpcMalformed`].
    pub async fn latest_blockhash(&self) -> Result<Hash, BridgeError> {
        parse_blockhash_response(&self.call(&request_body()).await?)
    }

    /// The balance of the token account `account` (an ATA), in base units, at
    /// the `confirmed` commitment.
    ///
    /// # Errors
    /// [`BridgeError::BlockhashUnavailable`] (transport), [`BridgeError::RpcStatus`],
    /// [`BridgeError::RpcResponseTooLarge`] or [`BridgeError::RpcMalformed`]
    /// (including an account that does not exist).
    pub async fn token_balance(&self, account: &Pubkey) -> Result<u64, BridgeError> {
        parse_token_balance_response(&self.call(&token_balance_body(account)).await?)
    }
}

/// A [`BlockhashSource`] backed by a Solana JSON-RPC node.
///
/// `Debug` prints nothing of the URL.
pub struct RpcBlockhashSource {
    rpc: RpcClient,
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
    /// [`BridgeError::RpcConfig`] as for [`RpcClient::new`].
    pub fn new(url: &Url) -> Result<Self, BridgeError> {
        Ok(Self {
            rpc: RpcClient::new(url)?,
        })
    }
}

impl BlockhashSource for RpcBlockhashSource {
    async fn latest(&mut self) -> Result<Hash, BridgeError> {
        self.rpc.latest_blockhash().await
    }
}
