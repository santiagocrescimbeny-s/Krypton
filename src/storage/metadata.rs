//! Asynchronous metadata engine: pins the token image and its Metaplex /
//! Pump.fun compatible JSON document to IPFS through Pinata and returns the
//! `metadata_uri` that goes into the on-chain `Create` instruction.
//!
//! Flow of [`MetadataUploader::upload_token_metadata`]:
//! 1. validate the [`TokenMetadata`] against on-chain limits (no network yet);
//! 2. pin the image -> `image_uri`;
//! 3. build the [`MetadataDocument`] around `image_uri` and pin it -> `metadata_uri`.
//!
//! Pinata API (v3): `POST {upload_base}/v3/files`, `multipart/form-data` with
//! `file`, `network=public` and `name`; `Authorization: Bearer <JWT>`; the
//! response is `{"data": {"cid": "...", ...}}`.
//!
//! Security properties:
//! - the JWT lives in a [`SecretString`], is sent as a *sensitive* header, and
//!   never appears in `Debug`, errors or logs;
//! - the JWT is only sent over HTTPS (plain HTTP is accepted solely for loopback
//!   hosts, which makes local mock servers possible);
//! - redirects are never followed, response bodies are size-capped, remote error
//!   text is sanitized, and CIDs are validated before being spliced into URIs.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use reqwest::multipart::{Form, Part};
use reqwest::{Body, Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::config::redact_url;
use crate::errors::{sanitize_remote_message, ConfigError, IpfsError};
use crate::security::SecretString;

// --- Limits (Metaplex Token Metadata / Pump.fun `Create`) -----------------------

/// Maximum token name length in bytes.
pub const MAX_NAME_BYTES: usize = 32;
/// Maximum token symbol length in bytes.
pub const MAX_SYMBOL_BYTES: usize = 10;
/// Maximum metadata URI length in bytes.
pub const MAX_URI_BYTES: usize = 200;
/// Maximum description length in characters (sanity bound).
pub const MAX_DESCRIPTION_CHARS: usize = 1_000;
/// Maximum length of a social link in bytes.
pub const MAX_SOCIAL_URL_BYTES: usize = 200;
/// Maximum accepted image size in bytes (5 MiB).
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
/// Hard cap on any response body we are willing to buffer.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// Value of the `createdOn` field Pump.fun front-ends expect.
pub const CREATED_ON: &str = "https://pump.fun";

// --- Defaults -------------------------------------------------------------------

/// Pinata uploads host.
pub const DEFAULT_UPLOAD_BASE_URL: &str = "https://uploads.pinata.cloud";
/// Public gateway used to build `image` / `metadata_uri` links.
pub const DEFAULT_GATEWAY_BASE_URL: &str = "https://ipfs.io/ipfs";
/// Path of the Pinata v3 upload endpoint.
pub const UPLOAD_PATH: &str = "/v3/files";
/// Default per-request deadline.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default number of retries after the first attempt.
pub const DEFAULT_MAX_RETRIES: u32 = 2;
/// Upper bound for `max_retries`.
pub const MAX_RETRIES_CAP: u32 = 5;
/// Default base delay of the exponential backoff.
pub const DEFAULT_RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
/// Upper bound for a single backoff sleep.
const MAX_BACKOFF: Duration = Duration::from_secs(10);
const USER_AGENT: &str = concat!("krypton/", env!("CARGO_PKG_VERSION"));

/// Environment variable: Pinata JWT (required).
pub const ENV_PINATA_JWT: &str = "PINATA_JWT";
/// Environment variable: upload host override (optional).
pub const ENV_PINATA_UPLOAD_URL: &str = "PINATA_UPLOAD_URL";
/// Environment variable: IPFS gateway override (optional).
pub const ENV_IPFS_GATEWAY_URL: &str = "IPFS_GATEWAY_URL";
/// Environment variable: per-request timeout in seconds (optional).
pub const ENV_IPFS_TIMEOUT_SECS: &str = "IPFS_TIMEOUT_SECS";
/// Environment variable: retries after the first attempt (optional).
pub const ENV_IPFS_MAX_RETRIES: &str = "IPFS_MAX_RETRIES";

// =================================================================================
// Content identifiers
// =================================================================================

/// A syntactically validated IPFS CID.
///
/// Only ASCII alphanumerics, 16..=128 characters: enough for CIDv0 (`Qm...`)
/// and CIDv1 (`baf...`), and strict enough that a hostile response cannot inject
/// path separators or query strings into the URIs we build from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpfsCid(String);

impl IpfsCid {
    /// Validates and wraps a CID string.
    ///
    /// # Errors
    /// [`IpfsError::InvalidCid`] if the value is malformed.
    pub fn parse(raw: &str) -> Result<Self, IpfsError> {
        let valid_len = (16..=128).contains(&raw.len());
        if valid_len && raw.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            Ok(Self(raw.to_owned()))
        } else {
            Err(IpfsError::InvalidCid)
        }
    }

    /// The CID as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IpfsCid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// =================================================================================
// Image
// =================================================================================

/// Image formats accepted by Pump.fun front-ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// `image/png`
    Png,
    /// `image/jpeg`
    Jpeg,
    /// `image/gif`
    Gif,
    /// `image/webp`
    Webp,
}

impl ImageFormat {
    /// MIME type.
    #[must_use]
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }

    /// File extension without the dot.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
        }
    }

    /// Detects the format from magic bytes (the file name is never trusted).
    #[must_use]
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        const PNG_MAGIC: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        const JPEG_MAGIC: &[u8] = &[0xFF, 0xD8, 0xFF];

        if bytes.starts_with(PNG_MAGIC) {
            Some(Self::Png)
        } else if bytes.starts_with(JPEG_MAGIC) {
            Some(Self::Jpeg)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()) {
            Some(Self::Webp)
        } else {
            None
        }
    }
}

/// A validated, in-memory token image.
///
/// The pixels are held as reference-counted [`Bytes`], so cloning an
/// `ImageAsset` (or taking [`ImageAsset::shared_bytes`]) is O(1) and never
/// copies the payload. The uploader relies on this to resend the same image on
/// every retry attempt without allocating up to 5 MiB each time.
#[derive(Clone)]
pub struct ImageAsset {
    bytes: Bytes,
    format: ImageFormat,
}

// Manual `Debug`: never dump up to 5 MiB of pixels into a log line.
impl fmt::Debug for ImageAsset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageAsset")
            .field("format", &self.format)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl ImageAsset {
    /// Validates raw image bytes (non-empty, within [`MAX_IMAGE_BYTES`], known
    /// magic bytes).
    ///
    /// # Errors
    /// [`IpfsError::EmptyImage`], [`IpfsError::ImageTooLarge`] or
    /// [`IpfsError::UnsupportedImageFormat`].
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, IpfsError> {
        // `Bytes::from(Vec<u8>)` takes over the allocation: no copy.
        Self::from_shared(Bytes::from(bytes))
    }

    /// Same validation as [`ImageAsset::from_bytes`] for bytes that are already
    /// reference counted (no copy is made).
    ///
    /// # Errors
    /// See [`ImageAsset::from_bytes`].
    pub fn from_shared(bytes: Bytes) -> Result<Self, IpfsError> {
        if bytes.is_empty() {
            return Err(IpfsError::EmptyImage);
        }
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if size > MAX_IMAGE_BYTES {
            return Err(IpfsError::ImageTooLarge {
                size,
                max: MAX_IMAGE_BYTES,
            });
        }
        let format = ImageFormat::sniff(&bytes).ok_or(IpfsError::UnsupportedImageFormat)?;
        Ok(Self { bytes, format })
    }

    /// Reads and validates an image file. The size is checked *before* the
    /// content is read, so an oversized file is never loaded into memory.
    ///
    /// # Errors
    /// [`IpfsError::ImageRead`] on I/O failure, plus everything from
    /// [`ImageAsset::from_bytes`].
    pub async fn from_path(path: &Path) -> Result<Self, IpfsError> {
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|err| IpfsError::ImageRead(err.kind()))?;
        if metadata.len() > MAX_IMAGE_BYTES {
            return Err(IpfsError::ImageTooLarge {
                size: metadata.len(),
                max: MAX_IMAGE_BYTES,
            });
        }
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|err| IpfsError::ImageRead(err.kind()))?;
        Self::from_bytes(bytes)
    }

    /// Detected format.
    #[must_use]
    pub fn format(&self) -> ImageFormat {
        self.format
    }

    /// Image size in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Always `false`: an empty image cannot be constructed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Raw image bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// A shared handle to the image bytes. O(1): only a reference count is
    /// incremented, the payload is not copied.
    #[must_use]
    pub fn shared_bytes(&self) -> Bytes {
        self.bytes.clone()
    }
}

// =================================================================================
// Token metadata and the JSON document
// =================================================================================

/// User supplied token metadata (before upload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenMetadata {
    /// Token name (<= 32 bytes).
    pub name: String,
    /// Ticker symbol (<= 10 bytes, no whitespace).
    pub symbol: String,
    /// Free-text description (may be empty).
    pub description: String,
    /// Optional X/Twitter link (`http(s)` URL).
    pub twitter: Option<String>,
    /// Optional Telegram link (`http(s)` URL).
    pub telegram: Option<String>,
    /// Optional website (`http(s)` URL).
    pub website: Option<String>,
    /// Whether front-ends should display the name (`showName`).
    pub show_name: bool,
}

impl TokenMetadata {
    /// Creates metadata with no social links and `show_name = true`.
    #[must_use]
    pub fn new(name: &str, symbol: &str, description: &str) -> Self {
        Self {
            name: name.to_owned(),
            symbol: symbol.to_owned(),
            description: description.to_owned(),
            twitter: None,
            telegram: None,
            website: None,
            show_name: true,
        }
    }

    /// Sets the X/Twitter link.
    #[must_use]
    pub fn with_twitter(mut self, url: &str) -> Self {
        self.twitter = Some(url.to_owned());
        self
    }

    /// Sets the Telegram link.
    #[must_use]
    pub fn with_telegram(mut self, url: &str) -> Self {
        self.telegram = Some(url.to_owned());
        self
    }

    /// Sets the website link.
    #[must_use]
    pub fn with_website(mut self, url: &str) -> Self {
        self.website = Some(url.to_owned());
        self
    }

    /// Checks every field against the on-chain / front-end constraints.
    ///
    /// # Errors
    /// [`IpfsError::InvalidMetadata`] naming the first offending field.
    pub fn validate(&self) -> Result<(), IpfsError> {
        validate_text("name", &self.name, MAX_NAME_BYTES)?;
        validate_text("symbol", &self.symbol, MAX_SYMBOL_BYTES)?;
        if self.symbol.chars().any(char::is_whitespace) {
            return Err(invalid_field("symbol", "must not contain whitespace"));
        }

        if self.description.chars().count() > MAX_DESCRIPTION_CHARS {
            return Err(invalid_field(
                "description",
                &format!("must be at most {MAX_DESCRIPTION_CHARS} characters"),
            ));
        }
        let forbidden_control = self
            .description
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'));
        if forbidden_control {
            return Err(invalid_field(
                "description",
                "must not contain control characters",
            ));
        }

        validate_social("twitter", self.twitter.as_deref())?;
        validate_social("telegram", self.telegram.as_deref())?;
        validate_social("website", self.website.as_deref())
    }
}

fn invalid_field(field: &'static str, reason: &str) -> IpfsError {
    IpfsError::InvalidMetadata {
        field,
        reason: reason.to_owned(),
    }
}

fn validate_text(field: &'static str, value: &str, max_bytes: usize) -> Result<(), IpfsError> {
    if value.trim().is_empty() {
        return Err(invalid_field(field, "must not be empty"));
    }
    if value != value.trim() {
        return Err(invalid_field(
            field,
            "must not have leading or trailing whitespace",
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(invalid_field(field, "must not contain control characters"));
    }
    if value.len() > max_bytes {
        return Err(invalid_field(
            field,
            &format!("must be at most {max_bytes} bytes (got {})", value.len()),
        ));
    }
    Ok(())
}

fn validate_social(field: &'static str, value: Option<&str>) -> Result<(), IpfsError> {
    let Some(raw) = value else {
        return Ok(());
    };
    if raw.len() > MAX_SOCIAL_URL_BYTES {
        return Err(invalid_field(
            field,
            &format!("must be at most {MAX_SOCIAL_URL_BYTES} bytes"),
        ));
    }
    if raw != raw.trim() || raw.chars().any(char::is_control) {
        return Err(invalid_field(
            field,
            "must not contain whitespace padding or control characters",
        ));
    }
    let url = Url::parse(raw).map_err(|_| invalid_field(field, "is not a valid URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid_field(field, "scheme must be http or https"));
    }
    if url.host_str().is_none() {
        return Err(invalid_field(field, "URL must include a host"));
    }
    Ok(())
}

/// One entry of `properties.files`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FileEntry {
    /// Gateway URI of the file.
    pub uri: String,
    /// MIME type of the file.
    #[serde(rename = "type")]
    pub mime_type: String,
}

/// The `properties` block of the Metaplex standard.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Properties {
    /// Files that make up the asset.
    pub files: Vec<FileEntry>,
    /// Asset category (`image`).
    pub category: &'static str,
}

/// One entry of `attributes` (kept for Metaplex compatibility; empty by default).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Attribute {
    /// Attribute name.
    pub trait_type: String,
    /// Attribute value.
    pub value: String,
}

/// The off-chain JSON document: Metaplex Token Metadata standard fields plus
/// the Pump.fun extras (`showName`, `createdOn`, `twitter`, `telegram`,
/// `website`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MetadataDocument {
    /// Token name.
    pub name: String,
    /// Token symbol.
    pub symbol: String,
    /// Token description.
    pub description: String,
    /// Gateway URI of the image.
    pub image: String,
    /// Pump.fun: show the name in front-ends.
    #[serde(rename = "showName")]
    pub show_name: bool,
    /// Pump.fun: origin marker.
    #[serde(rename = "createdOn")]
    pub created_on: &'static str,
    /// X/Twitter link.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub twitter: Option<String>,
    /// Telegram link.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telegram: Option<String>,
    /// Website link.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    /// Metaplex `external_url` (mirrors `website`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_url: Option<String>,
    /// Metaplex attributes (empty).
    pub attributes: Vec<Attribute>,
    /// Metaplex properties.
    pub properties: Properties,
}

impl MetadataDocument {
    /// Builds the document around an already pinned image.
    #[must_use]
    pub fn new(metadata: &TokenMetadata, image_uri: &str, format: ImageFormat) -> Self {
        Self {
            name: metadata.name.clone(),
            symbol: metadata.symbol.clone(),
            description: metadata.description.clone(),
            image: image_uri.to_owned(),
            show_name: metadata.show_name,
            created_on: CREATED_ON,
            twitter: metadata.twitter.clone(),
            telegram: metadata.telegram.clone(),
            website: metadata.website.clone(),
            external_url: metadata.website.clone(),
            attributes: Vec::new(),
            properties: Properties {
                files: vec![FileEntry {
                    uri: image_uri.to_owned(),
                    mime_type: format.mime().to_owned(),
                }],
                category: "image",
            },
        }
    }

    /// Serializes to compact JSON bytes.
    ///
    /// # Errors
    /// [`IpfsError::Serialize`] (not expected for this fixed schema).
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, IpfsError> {
        serde_json::to_vec(self).map_err(IpfsError::Serialize)
    }
}

/// Result of a complete metadata upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedMetadata {
    /// CID of the pinned image.
    pub image_cid: IpfsCid,
    /// CID of the pinned JSON document.
    pub metadata_cid: IpfsCid,
    /// Gateway URI of the image.
    pub image_uri: String,
    /// Gateway URI of the JSON document: pass this to `Create`.
    pub metadata_uri: String,
}

// =================================================================================
// Pinata configuration
// =================================================================================

/// Pinata / IPFS client settings.
///
/// `Debug` redacts the JWT and strips paths from URLs.
pub struct PinataConfig {
    jwt: SecretString,
    /// Upload host (HTTPS, or HTTP for loopback only).
    pub upload_base_url: Url,
    /// Gateway used to build public URIs (HTTPS).
    pub gateway_base_url: Url,
    /// Per-request deadline.
    pub timeout: Duration,
    /// Retries after the first attempt (capped at [`MAX_RETRIES_CAP`]).
    pub max_retries: u32,
    /// Base delay of the exponential backoff.
    pub retry_base_delay: Duration,
}

impl PinataConfig {
    /// Creates a configuration with production defaults.
    ///
    /// # Errors
    /// [`IpfsError::MissingCredentials`] if the JWT is blank.
    pub fn new(jwt: SecretString) -> Result<Self, IpfsError> {
        if jwt.is_blank() {
            return Err(IpfsError::MissingCredentials);
        }
        Ok(Self {
            jwt,
            upload_base_url: parse_endpoint("upload_base_url", DEFAULT_UPLOAD_BASE_URL, true)?,
            gateway_base_url: parse_endpoint("gateway_base_url", DEFAULT_GATEWAY_BASE_URL, false)?,
            timeout: DEFAULT_TIMEOUT,
            max_retries: DEFAULT_MAX_RETRIES,
            retry_base_delay: DEFAULT_RETRY_BASE_DELAY,
        })
    }

    /// Overrides the upload host.
    ///
    /// # Errors
    /// [`IpfsError::InvalidEndpoint`] unless it is HTTPS (or loopback HTTP),
    /// credential-free, and free of query/fragment.
    pub fn with_upload_base_url(mut self, raw: &str) -> Result<Self, IpfsError> {
        self.upload_base_url = parse_endpoint("upload_base_url", raw, true)?;
        Ok(self)
    }

    /// Overrides the public gateway.
    ///
    /// # Errors
    /// [`IpfsError::InvalidEndpoint`] unless it is HTTPS, credential-free, and
    /// free of query/fragment.
    pub fn with_gateway_base_url(mut self, raw: &str) -> Result<Self, IpfsError> {
        self.gateway_base_url = parse_endpoint("gateway_base_url", raw, false)?;
        Ok(self)
    }

    /// Sets the per-request deadline.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the retry policy (`max_retries` is capped at [`MAX_RETRIES_CAP`]).
    #[must_use]
    pub fn with_retries(mut self, max_retries: u32, base_delay: Duration) -> Self {
        self.max_retries = max_retries.min(MAX_RETRIES_CAP);
        self.retry_base_delay = base_delay;
        self
    }

    /// Builds the configuration from an explicit variable map
    /// (`PINATA_JWT` required; `PINATA_UPLOAD_URL`, `IPFS_GATEWAY_URL`,
    /// `IPFS_TIMEOUT_SECS`, `IPFS_MAX_RETRIES` optional). Blank values count as
    /// unset. The JWT is *moved* out of the map.
    ///
    /// # Errors
    /// [`ConfigError::MissingVar`] / [`ConfigError::InvalidVar`].
    pub fn from_vars(mut vars: HashMap<String, String>) -> Result<Self, ConfigError> {
        let jwt = vars
            .remove(ENV_PINATA_JWT)
            .map(SecretString::new)
            .filter(|secret| !secret.is_blank())
            .ok_or(ConfigError::MissingVar {
                name: ENV_PINATA_JWT,
            })?;
        let mut config = Self::new(jwt).map_err(|err| config_error(ENV_PINATA_JWT, &err))?;

        let mut optional = |key: &'static str| -> Option<String> {
            vars.remove(key)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };

        if let Some(raw) = optional(ENV_PINATA_UPLOAD_URL) {
            config = config
                .with_upload_base_url(&raw)
                .map_err(|err| config_error(ENV_PINATA_UPLOAD_URL, &err))?;
        }
        if let Some(raw) = optional(ENV_IPFS_GATEWAY_URL) {
            config = config
                .with_gateway_base_url(&raw)
                .map_err(|err| config_error(ENV_IPFS_GATEWAY_URL, &err))?;
        }
        if let Some(raw) = optional(ENV_IPFS_TIMEOUT_SECS) {
            let secs = raw
                .parse::<u64>()
                .ok()
                .filter(|secs| (1..=300).contains(secs))
                .ok_or(ConfigError::InvalidVar {
                    name: ENV_IPFS_TIMEOUT_SECS,
                    reason: "must be an integer between 1 and 300".to_owned(),
                })?;
            config.timeout = Duration::from_secs(secs);
        }
        if let Some(raw) = optional(ENV_IPFS_MAX_RETRIES) {
            let retries = raw
                .parse::<u32>()
                .ok()
                .filter(|retries| *retries <= MAX_RETRIES_CAP)
                .ok_or(ConfigError::InvalidVar {
                    name: ENV_IPFS_MAX_RETRIES,
                    reason: format!("must be an integer between 0 and {MAX_RETRIES_CAP}"),
                })?;
            config.max_retries = retries;
        }
        Ok(config)
    }
}

impl fmt::Debug for PinataConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinataConfig")
            .field("jwt", &"<redacted>")
            .field("upload_base_url", &redact_url(&self.upload_base_url))
            .field("gateway_base_url", &redact_url(&self.gateway_base_url))
            .field("timeout", &self.timeout)
            .field("max_retries", &self.max_retries)
            .field("retry_base_delay", &self.retry_base_delay)
            .finish()
    }
}

fn config_error(name: &'static str, err: &IpfsError) -> ConfigError {
    ConfigError::InvalidVar {
        name,
        reason: err.to_string(),
    }
}

pub(crate) fn is_loopback_host(host: &str) -> bool {
    crate::parsing::is_loopback_host(host)
}

/// Parses an endpoint base URL. `allow_loopback_http` lets local mock servers
/// use plain HTTP; everything else must be HTTPS so the JWT and the on-chain
/// URIs never travel in clear text.
fn parse_endpoint(
    name: &'static str,
    raw: &str,
    allow_loopback_http: bool,
) -> Result<Url, IpfsError> {
    let bad = |reason: &str| IpfsError::InvalidEndpoint {
        name,
        reason: reason.to_owned(),
    };

    let url = Url::parse(raw).map_err(|_| bad("not a valid URL"))?;
    let host = url
        .host_str()
        .ok_or_else(|| bad("URL must include a host"))?;

    let scheme_ok = match url.scheme() {
        "https" => true,
        "http" => allow_loopback_http && is_loopback_host(host),
        _ => false,
    };
    if !scheme_ok {
        return Err(bad(if allow_loopback_http {
            "scheme must be https (http is only allowed for loopback hosts)"
        } else {
            "scheme must be https"
        }));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(bad("must not embed credentials"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(bad("must not contain a query string or fragment"));
    }
    Ok(url)
}

// =================================================================================
// Uploader
// =================================================================================

/// One file to pin.
///
/// The content is a [`Bytes`] handle: cloning it for each retry attempt is O(1)
/// (a reference-count bump), so a 5 MiB image is allocated once per upload, not
/// once per attempt.
struct FilePayload {
    file_name: String,
    mime: &'static str,
    bytes: Bytes,
}

#[derive(Deserialize)]
struct UploadResponse {
    data: UploadData,
}

#[derive(Deserialize)]
struct UploadData {
    cid: String,
}

/// Non-blocking Pinata client. Cheap to share behind an `Arc`; the internal
/// `reqwest::Client` pools connections across uploads.
pub struct MetadataUploader {
    client: Client,
    config: PinataConfig,
    upload_url: Url,
}

impl fmt::Debug for MetadataUploader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetadataUploader")
            .field("config", &self.config)
            .finish()
    }
}

impl MetadataUploader {
    /// Builds an uploader with a dedicated rustls HTTP client (no redirects).
    ///
    /// # Errors
    /// [`IpfsError::ClientBuild`] or [`IpfsError::InvalidEndpoint`].
    pub fn new(config: PinataConfig) -> Result<Self, IpfsError> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(config.timeout)
            .tcp_nodelay(true)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| IpfsError::ClientBuild(err.without_url()))?;
        Self::with_client(client, config)
    }

    /// Builds an uploader around an existing client (shared pool, custom proxy
    /// or TLS settings). The per-request deadline from `config` still applies.
    ///
    /// # Errors
    /// [`IpfsError::InvalidEndpoint`] if the upload URL cannot be derived.
    pub fn with_client(client: Client, config: PinataConfig) -> Result<Self, IpfsError> {
        let joined = format!(
            "{}{}",
            config.upload_base_url.as_str().trim_end_matches('/'),
            UPLOAD_PATH
        );
        let upload_url = Url::parse(&joined).map_err(|_| IpfsError::InvalidEndpoint {
            name: "upload_base_url",
            reason: "cannot derive the upload URL".to_owned(),
        })?;
        Ok(Self {
            client,
            config,
            upload_url,
        })
    }

    /// Public gateway URI for a CID.
    ///
    /// # Errors
    /// [`IpfsError::UriTooLong`] if the result exceeds [`MAX_URI_BYTES`].
    pub fn gateway_uri(&self, cid: &IpfsCid) -> Result<String, IpfsError> {
        let uri = format!(
            "{}/{}",
            self.config.gateway_base_url.as_str().trim_end_matches('/'),
            cid
        );
        if uri.len() > MAX_URI_BYTES {
            return Err(IpfsError::UriTooLong {
                len: uri.len(),
                max: MAX_URI_BYTES,
            });
        }
        Ok(uri)
    }

    /// Validates, pins the image, pins the JSON document, and returns both URIs.
    ///
    /// Validation happens before any network traffic. The two uploads are
    /// sequential because the document embeds the image URI.
    ///
    /// # Errors
    /// Any [`IpfsError`].
    pub async fn upload_token_metadata(
        &self,
        metadata: &TokenMetadata,
        image: &ImageAsset,
    ) -> Result<UploadedMetadata, IpfsError> {
        metadata.validate()?;

        let image_cid = self.upload_image(image, &metadata.symbol).await?;
        let image_uri = self.gateway_uri(&image_cid)?;

        let document = MetadataDocument::new(metadata, &image_uri, image.format());
        let metadata_cid = self.upload_document(&document, &metadata.symbol).await?;
        let metadata_uri = self.gateway_uri(&metadata_cid)?;

        debug!(image_cid = %image_cid, metadata_cid = %metadata_cid, "token metadata pinned");
        Ok(UploadedMetadata {
            image_cid,
            metadata_cid,
            image_uri,
            metadata_uri,
        })
    }

    /// Pins an image. `name_stem` only influences the stored file name and is
    /// sanitized.
    ///
    /// # Errors
    /// Any [`IpfsError`].
    pub async fn upload_image(
        &self,
        image: &ImageAsset,
        name_stem: &str,
    ) -> Result<IpfsCid, IpfsError> {
        let payload = FilePayload {
            file_name: format!(
                "{}-image.{}",
                sanitize_stem(name_stem),
                image.format().extension()
            ),
            mime: image.format().mime(),
            bytes: image.shared_bytes(),
        };
        self.upload_file(&payload).await
    }

    /// Pins a metadata document. `name_stem` only influences the stored file
    /// name and is sanitized.
    ///
    /// # Errors
    /// Any [`IpfsError`].
    pub async fn upload_document(
        &self,
        document: &MetadataDocument,
        name_stem: &str,
    ) -> Result<IpfsCid, IpfsError> {
        let json = document.to_json_bytes()?;
        let payload = FilePayload {
            file_name: format!("{}-metadata.json", sanitize_stem(name_stem)),
            mime: "application/json",
            bytes: Bytes::from(json),
        };
        self.upload_file(&payload).await
    }

    /// Upload with exponential-backoff retries for transient failures.
    ///
    /// # Retry semantics (not idempotent on the Pinata side)
    /// Retrying is *safe for the result* but *not free of side effects*. A
    /// timeout, a dropped connection or a `5xx` can happen **after** Pinata has
    /// already received and pinned the file; the client simply never learns it.
    /// Resending then creates a second pin record for the same content. This
    /// costs storage quota / pin slots, never funds, and never changes the
    /// outcome: IPFS CIDs are content-addressed, so every successful attempt for
    /// the same bytes returns the **same CID**, and the token's `metadata_uri` is
    /// identical whether it took one attempt or five.
    ///
    /// Mitigations in place: the file name is derived deterministically from the
    /// (sanitized) token symbol, so duplicate pins are easy to spot and
    /// de-duplicate by CID in the Pinata dashboard / API; the retry budget is
    /// small and capped ([`MAX_RETRIES_CAP`]); permanent errors (`401`, `403`,
    /// other `4xx`, malformed responses) are never retried. Callers that need
    /// strict at-most-once pinning should set the retry count to `0` and
    /// de-duplicate by CID themselves.
    ///
    /// The payload is a shared [`Bytes`] handle, so each attempt re-sends the
    /// same buffer without copying it.
    async fn upload_file(&self, file: &FilePayload) -> Result<IpfsCid, IpfsError> {
        let max_attempts = self.config.max_retries.saturating_add(1);
        let mut attempt: u32 = 0;
        loop {
            attempt = attempt.saturating_add(1);
            let error = match self.attempt_upload(file).await {
                Ok(cid) => {
                    debug!(attempt, bytes = file.bytes.len(), cid = %cid, "file pinned");
                    return Ok(cid);
                }
                Err(error) => error,
            };

            if !error.is_retryable() {
                return Err(error);
            }
            if attempt >= max_attempts {
                return Err(if attempt > 1 {
                    IpfsError::RetriesExhausted {
                        attempts: attempt,
                        source: Box::new(error),
                    }
                } else {
                    error
                });
            }
            warn!(attempt, max_attempts, error = %error, "transient pinning failure, retrying");
            tokio::time::sleep(self.backoff(attempt)).await;
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(16);
        self.config
            .retry_base_delay
            .saturating_mul(2u32.saturating_pow(exponent))
            .min(MAX_BACKOFF)
    }

    async fn attempt_upload(&self, file: &FilePayload) -> Result<IpfsCid, IpfsError> {
        let form = build_form(file)?;
        let response = self
            .client
            .post(self.upload_url.clone())
            .bearer_auth(self.config.jwt.expose_secret())
            .timeout(self.config.timeout)
            .multipart(form)
            .send()
            .await
            .map_err(map_transport_error)?;

        let status = response.status();
        let body = read_capped(response).await?;
        if status.is_success() {
            parse_upload_response(&body)
        } else {
            Err(status_error(status, &body))
        }
    }
}

/// Reduces free text (the token symbol) to a safe file-name stem.
fn sanitize_stem(raw: &str) -> String {
    let stem: String = raw
        .chars()
        .take(MAX_SYMBOL_BYTES)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if stem.is_empty() {
        "token".to_owned()
    } else {
        stem
    }
}

fn build_form(file: &FilePayload) -> Result<Form, IpfsError> {
    // `Bytes::clone` is O(1): the buffer is shared, not copied, on every attempt.
    // The length is declared explicitly so the multipart body keeps a known
    // `Content-Length` (a bare `Part::stream` would force chunked encoding).
    let length = u64::try_from(file.bytes.len()).unwrap_or(u64::MAX);
    let part = Part::stream_with_length(Body::from(file.bytes.clone()), length)
        .file_name(file.file_name.clone())
        .mime_str(file.mime)
        .map_err(|err| IpfsError::Transport(err.without_url()))?;
    Ok(Form::new()
        .text("network", "public")
        .text("name", file.file_name.clone())
        .part("file", part))
}

fn map_transport_error(err: reqwest::Error) -> IpfsError {
    if err.is_timeout() {
        IpfsError::Timeout
    } else {
        IpfsError::Transport(err.without_url())
    }
}

/// Buffers a response body, refusing anything above [`MAX_RESPONSE_BYTES`].
async fn read_capped(mut response: reqwest::Response) -> Result<Vec<u8>, IpfsError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(map_transport_error)? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(IpfsError::InvalidResponse(
                "response body exceeds the size limit",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn status_error(status: StatusCode, body: &[u8]) -> IpfsError {
    let code = status.as_u16();
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return IpfsError::Unauthorized { status: code };
    }
    IpfsError::Api {
        status: code,
        message: sanitize_remote_message(&String::from_utf8_lossy(body)),
    }
}

fn parse_upload_response(body: &[u8]) -> Result<IpfsCid, IpfsError> {
    let parsed: UploadResponse = serde_json::from_slice(body)
        .map_err(|_| IpfsError::InvalidResponse("body is not the expected JSON document"))?;
    IpfsCid::parse(&parsed.data.cid)
}
