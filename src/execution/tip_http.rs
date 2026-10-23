//! HTTP side of the tip-floor feed: the document parser and the `reqwest` source.
//!
//! The feed is Jito's public `GET /api/v1/bundles/tip_floor`: a JSON array with
//! one object whose amounts are in **SOL**:
//!
//! ```json
//! [{"time": "...", "landed_tips_25th_percentile": 0.000001,
//!   "landed_tips_50th_percentile": 0.00001, "landed_tips_75th_percentile": 0.00005,
//!   "landed_tips_95th_percentile": 0.0004, "landed_tips_99th_percentile": 0.002,
//!   "ema_landed_tips_50th_percentile": 0.00002}]
//! ```
//!
//! A JSON-RPC envelope (`{"result": [ ... ]}`, as some providers wrap it) is
//! accepted as well. Responses are size-capped, redirects are disabled and no
//! remote text ever reaches an error.

use std::time::Duration;

use reqwest::{Client, Url};
use serde::Deserialize;

use super::tip::TipFloor;
use super::tip_feed::{floor_from_sol, SolTipFloor, TipSource};
use crate::errors::TipFeedError;

/// The public tip-floor endpoint.
pub const DEFAULT_TIP_FLOOR_URL: &str = "https://bundles.jito.wtf/api/v1/bundles/tip_floor";

/// Largest response accepted, in bytes. The real document is a few hundred.
pub const MAX_FEED_RESPONSE_BYTES: usize = 16 * 1024;

/// Hard per-request deadline of the client (the polling loop applies its own,
/// tighter one).
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

const USER_AGENT: &str = concat!("krypton/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Deserialize)]
struct FeedEntry {
    #[serde(rename = "landed_tips_50th_percentile")]
    p50: f64,
    #[serde(rename = "landed_tips_75th_percentile")]
    p75: f64,
    #[serde(rename = "landed_tips_95th_percentile")]
    p95: f64,
    #[serde(rename = "landed_tips_99th_percentile")]
    p99: f64,
    #[serde(rename = "ema_landed_tips_50th_percentile")]
    ema_p50: f64,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum FeedDocument {
    Entries(Vec<FeedEntry>),
    Envelope { result: Vec<FeedEntry> },
}

/// Parses a tip-floor document into lamports.
///
/// # Errors
/// [`TipFeedError::Malformed`] for anything that is not a tip-floor document, an
/// empty array, a missing field, or values that fail
/// [`floor_from_sol`]'s checks.
pub fn parse_tip_floor(body: &[u8]) -> Result<TipFloor, TipFeedError> {
    let document: FeedDocument =
        serde_json::from_slice(body).map_err(|_| TipFeedError::Malformed {
            reason: "not a tip floor document",
        })?;
    let entries = match document {
        FeedDocument::Entries(entries) | FeedDocument::Envelope { result: entries } => entries,
    };
    let entry = entries.into_iter().next().ok_or(TipFeedError::Malformed {
        reason: "the document has no entries",
    })?;
    floor_from_sol(&SolTipFloor {
        p50: entry.p50,
        p75: entry.p75,
        p95: entry.p95,
        p99: entry.p99,
        ema_p50: entry.ema_p50,
    })
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "[::1]"
}

fn invalid(reason: &'static str) -> TipFeedError {
    TipFeedError::InvalidConfig { reason }
}

fn validate_url(url: &Url) -> Result<(), TipFeedError> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("the URL must not carry credentials"));
    }
    let Some(host) = url.host_str() else {
        return Err(invalid("the URL has no host"));
    };
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback(host) => Ok(()),
        _ => Err(invalid("the URL must be https (http only for loopback)")),
    }
}

/// A [`TipSource`] that reads the feed over HTTP.
#[derive(Debug)]
pub struct HttpTipSource {
    client: Client,
    url: Url,
}

impl HttpTipSource {
    /// Builds a source for `url`.
    ///
    /// # Errors
    /// [`TipFeedError::InvalidConfig`] for a URL that is not `https` (loopback
    /// `http` is allowed for tests) or that carries credentials, or if the HTTP
    /// client cannot be built.
    pub fn new(url: &str) -> Result<Self, TipFeedError> {
        let url = Url::parse(url).map_err(|_| invalid("the URL cannot be parsed"))?;
        validate_url(&url)?;
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(CLIENT_TIMEOUT)
            .connect_timeout(CLIENT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| invalid("the HTTP client cannot be built"))?;
        Ok(Self { client, url })
    }

    /// A source for the public mainnet feed ([`DEFAULT_TIP_FLOOR_URL`]).
    ///
    /// # Errors
    /// See [`HttpTipSource::new`].
    pub fn mainnet() -> Result<Self, TipFeedError> {
        Self::new(DEFAULT_TIP_FLOOR_URL)
    }
}

fn map_transport_error(error: &reqwest::Error) -> TipFeedError {
    if error.is_timeout() {
        TipFeedError::Timeout
    } else {
        TipFeedError::Unreachable
    }
}

impl TipSource for HttpTipSource {
    async fn fetch(&mut self) -> Result<TipFloor, TipFeedError> {
        let mut response = self
            .client
            .get(self.url.clone())
            .send()
            .await
            .map_err(|error| map_transport_error(&error))?;
        let status = response.status();
        if !status.is_success() {
            return Err(TipFeedError::HttpStatus {
                status: status.as_u16(),
            });
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| map_transport_error(&error))?
        {
            if body.len().saturating_add(chunk.len()) > MAX_FEED_RESPONSE_BYTES {
                return Err(TipFeedError::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        parse_tip_floor(&body)
    }
}
