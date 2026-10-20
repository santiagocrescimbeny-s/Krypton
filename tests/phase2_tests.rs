//! Phase 2 test-suite: the asynchronous metadata engine (Pinata / IPFS).
//!
//! Network mocking: every test talks to an in-process, std-only HTTP server
//! (`tests/common`) bound to an ephemeral loopback port. Nothing leaves the
//! machine and no extra dev-dependency is needed.
//!
//! Like Phase 1, tests return `Result` and avoid `unwrap()`/`expect()`.

mod common;

use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::{MockResponse, MockServer, RecordedRequest};
use krypton::errors::{
    sanitize_remote_message, ConfigError, IpfsError, LauncherError, MAX_REMOTE_MESSAGE_CHARS,
};
use krypton::security::SecretString;
use krypton::storage::metadata::{
    ImageAsset, ImageFormat, IpfsCid, MetadataDocument, MetadataUploader, PinataConfig,
    TokenMetadata, MAX_IMAGE_BYTES, MAX_RETRIES_CAP, MAX_URI_BYTES,
};
use serde_json::{json, Value};

type TestResult = Result<(), Box<dyn Error>>;

const JWT: &str = "eyJ-test-jwt-SECRET-VALUE";
const IMAGE_CID: &str = "bafkreigh2akiscaildcqabsyg3dfr6chu3fgpregiymsck7e7aqa4s52zy";
const METADATA_CID: &str = "bafkreibm6jg3ux5qumhcn2b3flc3tyu6dmlb4xa7u5bf5xpzq7zscdgxb4";
const GATEWAY: &str = "https://gateway.example/ipfs";

// --- Helpers -------------------------------------------------------------------

fn err_of<T, E>(result: Result<T, E>) -> Result<E, Box<dyn Error>> {
    match result {
        Ok(_) => Err("expected an error but the operation succeeded".into()),
        Err(err) => Ok(err),
    }
}

fn png_bytes() -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.resize(72, 7);
    bytes
}

fn png_image() -> Result<ImageAsset, IpfsError> {
    ImageAsset::from_bytes(png_bytes())
}

fn cid_body(cid: &str) -> String {
    format!(
        r#"{{"data":{{"id":"0190","name":"f","cid":"{cid}","size":72,"number_of_files":1,"mime_type":"x","group_id":null}}}}"#
    )
}

fn ok_cid(cid: &str) -> MockResponse {
    MockResponse::json(200, &cid_body(cid))
}

/// Answers image uploads with `IMAGE_CID` and everything else with `METADATA_CID`.
fn router(_index: usize, request: &RecordedRequest) -> MockResponse {
    if request.body_contains(b"-image.png\"") {
        ok_cid(IMAGE_CID)
    } else {
        ok_cid(METADATA_CID)
    }
}

fn test_client() -> Result<reqwest::Client, reqwest::Error> {
    // `no_proxy` keeps the tests independent of any HTTP(S)_PROXY on the machine.
    reqwest::Client::builder().no_proxy().build()
}

fn uploader_with(
    server: &MockServer,
    timeout: Duration,
    retries: u32,
) -> Result<MetadataUploader, Box<dyn Error>> {
    let config = PinataConfig::new(SecretString::new(JWT.to_owned()))?
        .with_upload_base_url(&server.url())?
        .with_gateway_base_url(GATEWAY)?
        .with_timeout(timeout)
        .with_retries(retries, Duration::from_millis(1));
    Ok(MetadataUploader::with_client(test_client()?, config)?)
}

fn uploader(server: &MockServer, retries: u32) -> Result<MetadataUploader, Box<dyn Error>> {
    uploader_with(server, Duration::from_secs(5), retries)
}

fn sample_metadata() -> TokenMetadata {
    TokenMetadata::new("Krypton Coin", "KRYP", "A test token")
        .with_twitter("https://x.com/krypton")
        .with_telegram("https://t.me/krypton")
        .with_website("https://krypton.example")
}

fn tweaked(edit: impl FnOnce(&mut TokenMetadata)) -> TokenMetadata {
    let mut metadata = sample_metadata();
    edit(&mut metadata);
    metadata
}

/// Extracts the JSON document embedded in a multipart request body.
fn embedded_json(request: &RecordedRequest) -> Result<Value, Box<dyn Error>> {
    let text = request.body_text();
    let start = text
        .find('{')
        .ok_or("no JSON object in the multipart body")?;
    let end = text.rfind('}').ok_or("unterminated JSON object")?;
    let slice = text.get(start..=end).ok_or("invalid JSON slice")?;
    Ok(serde_json::from_str(slice)?)
}

fn assert_no_jwt_leak(err: &IpfsError) {
    assert!(!err.to_string().contains("SECRET-VALUE"), "Display leaked");
    assert!(!format!("{err:?}").contains("SECRET-VALUE"), "Debug leaked");
}

fn temp_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("krypton-phase2-{}-{tag}", std::process::id()))
}

// =================================================================================
// Happy path
// =================================================================================

#[tokio::test]
async fn uploads_image_then_metadata_and_returns_the_metadata_uri() -> TestResult {
    let server = MockServer::start(router)?;
    let uploader = uploader(&server, 0)?;

    let uploaded = uploader
        .upload_token_metadata(&sample_metadata(), &png_image()?)
        .await?;

    assert_eq!(uploaded.image_cid.as_str(), IMAGE_CID);
    assert_eq!(uploaded.metadata_cid.as_str(), METADATA_CID);
    assert_eq!(uploaded.image_uri, format!("{GATEWAY}/{IMAGE_CID}"));
    assert_eq!(uploaded.metadata_uri, format!("{GATEWAY}/{METADATA_CID}"));
    assert!(uploaded.metadata_uri.len() <= MAX_URI_BYTES);

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        2,
        "one request for the image, one for the JSON"
    );

    // Request 1: the image, sent exactly as given.
    let image_request = requests.first().ok_or("missing image request")?;
    assert_eq!(image_request.method, "POST");
    assert_eq!(image_request.path, "/v3/files");
    assert_eq!(
        image_request.header("authorization"),
        Some(format!("Bearer {JWT}").as_str())
    );
    assert!(image_request
        .header("content-type")
        .is_some_and(|value| value.starts_with("multipart/form-data")));
    assert!(image_request.body_contains(b"name=\"network\""));
    assert!(image_request.body_contains(b"public"));
    assert!(image_request.body_contains(b"filename=\"kryp-image.png\""));
    assert!(image_request.body_contains(&png_bytes()));
    assert!(image_request
        .body_text()
        .to_ascii_lowercase()
        .contains("content-type: image/png"));

    // Request 2: the JSON document, pointing at the pinned image.
    let json_request = requests.get(1).ok_or("missing JSON request")?;
    assert!(json_request.body_contains(b"filename=\"kryp-metadata.json\""));
    let document = embedded_json(json_request)?;
    assert_eq!(document["name"], "Krypton Coin");
    assert_eq!(document["symbol"], "KRYP");
    assert_eq!(document["description"], "A test token");
    assert_eq!(document["image"], uploaded.image_uri.as_str());
    assert_eq!(document["showName"], true);
    assert_eq!(document["createdOn"], "https://pump.fun");
    assert_eq!(document["twitter"], "https://x.com/krypton");
    assert_eq!(document["telegram"], "https://t.me/krypton");
    assert_eq!(document["website"], "https://krypton.example");
    assert_eq!(document["external_url"], "https://krypton.example");
    assert_eq!(document["attributes"], json!([]));
    assert_eq!(document["properties"]["category"], "image");
    assert_eq!(
        document["properties"]["files"][0]["uri"],
        uploaded.image_uri.as_str()
    );
    assert_eq!(document["properties"]["files"][0]["type"], "image/png");
    Ok(())
}

#[tokio::test]
async fn optional_social_links_are_omitted_from_the_document() -> TestResult {
    let server = MockServer::start(router)?;
    let uploader = uploader(&server, 0)?;
    let bare = TokenMetadata::new("Bare", "BARE", "");

    uploader.upload_token_metadata(&bare, &png_image()?).await?;

    let requests = server.requests();
    let document = embedded_json(requests.get(1).ok_or("missing JSON request")?)?;
    for absent in ["twitter", "telegram", "website", "external_url"] {
        assert!(document.get(absent).is_none(), "`{absent}` must be omitted");
    }
    assert_eq!(document["description"], "");
    Ok(())
}

#[tokio::test]
async fn one_uploader_serves_concurrent_uploads() -> TestResult {
    let server = MockServer::start(router)?;
    let shared = Arc::new(uploader(&server, 0)?);
    let image = png_image()?;
    let metadata = sample_metadata();

    let (first, second) = tokio::join!(
        shared.upload_token_metadata(&metadata, &image),
        shared.upload_token_metadata(&metadata, &image),
    );

    assert_eq!(first?.metadata_uri, format!("{GATEWAY}/{METADATA_CID}"));
    assert_eq!(second?.metadata_uri, format!("{GATEWAY}/{METADATA_CID}"));
    assert_eq!(server.request_count(), 4);
    Ok(())
}

// =================================================================================
// Validation happens before any network traffic
// =================================================================================

#[tokio::test]
async fn invalid_metadata_is_rejected_without_touching_the_network() -> TestResult {
    let server = MockServer::start(router)?;
    let uploader = uploader(&server, 0)?;
    let image = png_image()?;

    let cases: [(&str, TokenMetadata); 18] = [
        ("name", tweaked(|m| m.name = String::new())),
        ("name", tweaked(|m| m.name = "   ".to_owned())),
        ("name", tweaked(|m| m.name = " padded".to_owned())),
        ("name", tweaked(|m| m.name = "a".repeat(33))),
        ("name", tweaked(|m| m.name = "é".repeat(17))), // 34 bytes
        ("name", tweaked(|m| m.name = "bad\u{0007}name".to_owned())),
        ("symbol", tweaked(|m| m.symbol = String::new())),
        ("symbol", tweaked(|m| m.symbol = "TOOLONGSYMBOL".to_owned())),
        ("symbol", tweaked(|m| m.symbol = "A B".to_owned())),
        ("symbol", tweaked(|m| m.symbol = "AB\n".to_owned())),
        (
            "description",
            tweaked(|m| m.description = "x".repeat(1_001)),
        ),
        (
            "description",
            tweaked(|m| m.description = "bell\u{0007}".to_owned()),
        ),
        (
            "twitter",
            tweaked(|m| m.twitter = Some("javascript:alert(1)".to_owned())),
        ),
        (
            "twitter",
            tweaked(|m| m.twitter = Some("ftp://x.com/a".to_owned())),
        ),
        (
            "telegram",
            tweaked(|m| m.telegram = Some("not a url".to_owned())),
        ),
        (
            "website",
            tweaked(|m| m.website = Some(format!("https://x.com/{}", "a".repeat(200)))),
        ),
        (
            "website",
            tweaked(|m| m.website = Some(" https://x.com".to_owned())),
        ),
        (
            "website",
            tweaked(|m| m.website = Some("https://".to_owned())),
        ),
    ];

    for (field, metadata) in &cases {
        let err = err_of(uploader.upload_token_metadata(metadata, &image).await)?;
        assert!(
            matches!(&err, IpfsError::InvalidMetadata { field: found, .. } if found == field),
            "expected InvalidMetadata for `{field}`, got: {err}"
        );
    }
    assert_eq!(
        server.request_count(),
        0,
        "validation must precede any request"
    );
    Ok(())
}

#[test]
fn boundary_metadata_is_accepted() -> TestResult {
    tweaked(|m| m.name = "é".repeat(16)).validate()?; // exactly 32 bytes
    tweaked(|m| m.symbol = "ABCDEFGHIJ".to_owned()).validate()?; // exactly 10 bytes
    tweaked(|m| m.description = "multi\nline\ttext".to_owned()).validate()?;
    tweaked(|m| m.description = "x".repeat(1_000)).validate()?;
    TokenMetadata::new("N", "S", "").validate()?;
    Ok(())
}

// =================================================================================
// Failure handling and retries
// =================================================================================

#[tokio::test]
async fn rejected_credentials_fail_fast_without_retrying() -> TestResult {
    for status in [401, 403] {
        let server = MockServer::start(move |_, _| {
            MockResponse::json(status, r#"{"error":{"reason":"INVALID_CREDENTIALS"}}"#)
        })?;
        let uploader = uploader(&server, 3)?;

        let err = err_of(uploader.upload_image(&png_image()?, "KRYP").await)?;
        assert!(
            matches!(err, IpfsError::Unauthorized { status: found } if found == status),
            "status {status}: {err}"
        );
        assert_no_jwt_leak(&err);
        assert_eq!(server.request_count(), 1, "credential errors are permanent");
    }
    Ok(())
}

#[tokio::test]
async fn transient_errors_are_retried_until_success() -> TestResult {
    for transient in [500, 502, 503, 429, 408] {
        let server = MockServer::start(move |index, _| {
            if index < 2 {
                MockResponse::json(transient, r#"{"error":"try later"}"#)
            } else {
                ok_cid(IMAGE_CID)
            }
        })?;
        let uploader = uploader(&server, 3)?;

        let cid = uploader.upload_image(&png_image()?, "KRYP").await?;
        assert_eq!(cid.as_str(), IMAGE_CID, "status {transient}");
        assert_eq!(server.request_count(), 3, "status {transient}");
    }
    Ok(())
}

#[tokio::test]
async fn persistent_failures_report_retries_exhausted() -> TestResult {
    let server = MockServer::start(|_, _| MockResponse::json(503, "unavailable"))?;
    let uploader = uploader(&server, 2)?;

    let err = err_of(uploader.upload_image(&png_image()?, "KRYP").await)?;
    assert_no_jwt_leak(&err);
    match err {
        IpfsError::RetriesExhausted { attempts, source } => {
            assert_eq!(attempts, 3);
            assert!(matches!(*source, IpfsError::Api { status: 503, .. }));
        }
        other => return Err(format!("unexpected error: {other}").into()),
    }
    assert_eq!(server.request_count(), 3);
    Ok(())
}

#[tokio::test]
async fn zero_retries_returns_the_original_error() -> TestResult {
    let server = MockServer::start(|_, _| MockResponse::json(500, "boom"))?;
    let uploader = uploader(&server, 0)?;

    let err = err_of(uploader.upload_image(&png_image()?, "KRYP").await)?;
    assert!(matches!(err, IpfsError::Api { status: 500, .. }), "{err}");
    assert_eq!(server.request_count(), 1);
    Ok(())
}

#[tokio::test]
async fn permanent_client_errors_are_not_retried_and_remote_text_is_sanitized() -> TestResult {
    let hostile = "bad request at https://api.example/?key=TOPSECRET token eyJhbGciOi.payload.sig";
    let server = MockServer::start(move |_, _| MockResponse::json(400, hostile))?;
    let uploader = uploader(&server, 3)?;

    let err = err_of(uploader.upload_image(&png_image()?, "KRYP").await)?;
    match &err {
        IpfsError::Api { status, message } => {
            assert_eq!(*status, 400);
            assert!(message.contains("<url>"), "{message}");
            assert!(message.contains("<redacted>"), "{message}");
            for leaked in ["TOPSECRET", "https://", "eyJ"] {
                assert!(
                    !message.contains(leaked),
                    "message leaked {leaked}: {message}"
                );
            }
        }
        other => return Err(format!("unexpected error: {other}").into()),
    }
    assert_eq!(server.request_count(), 1);
    Ok(())
}

#[tokio::test]
async fn malformed_success_responses_are_rejected_without_retrying() -> TestResult {
    let long_cid = format!(r#"{{"data":{{"cid":"{IMAGE_CID}/extra"}}}}"#);
    let cases: [(String, bool); 8] = [
        ("not json".to_owned(), false),
        ("{}".to_owned(), false),
        (r#"{"data":{}}"#.to_owned(), false),
        (r#"{"data":{"cid":42}}"#.to_owned(), false),
        (r#"{"data":{"cid":""}}"#.to_owned(), true),
        (r#"{"data":{"cid":"short"}}"#.to_owned(), true),
        (
            r#"{"data":{"cid":"../../../../etc/passwd/xxxxxxxx"}}"#.to_owned(),
            true,
        ),
        (long_cid, true),
    ];

    for (body, is_cid_error) in cases {
        let shown = body.clone();
        let server = MockServer::start(move |_, _| MockResponse::json(200, &body))?;
        let uploader = uploader(&server, 3)?;

        let err = err_of(uploader.upload_image(&png_image()?, "KRYP").await)?;
        let as_expected = if is_cid_error {
            matches!(err, IpfsError::InvalidCid)
        } else {
            matches!(err, IpfsError::InvalidResponse(_))
        };
        assert!(as_expected, "body {shown:?} produced: {err}");
        assert_eq!(server.request_count(), 1, "parse errors are permanent");
    }
    Ok(())
}

#[tokio::test]
async fn oversized_response_bodies_are_refused() -> TestResult {
    let server = MockServer::start(|_, _| MockResponse::raw(200, vec![b'a'; 70_000]))?;
    let uploader = uploader(&server, 0)?;

    let err = err_of(uploader.upload_image(&png_image()?, "KRYP").await)?;
    assert!(matches!(err, IpfsError::InvalidResponse(_)), "{err}");
    Ok(())
}

#[tokio::test]
async fn slow_servers_trigger_a_timeout() -> TestResult {
    let server = MockServer::start(|_, _| ok_cid(IMAGE_CID).delayed(Duration::from_millis(600)))?;
    let uploader = uploader_with(&server, Duration::from_millis(60), 0)?;

    let err = err_of(uploader.upload_image(&png_image()?, "KRYP").await)?;
    assert!(matches!(err, IpfsError::Timeout), "{err}");
    assert!(err.is_retryable());
    Ok(())
}

#[tokio::test]
async fn unreachable_endpoints_surface_as_retryable_transport_errors() -> TestResult {
    let dead_url = {
        let server = MockServer::start(router)?;
        server.url()
    }; // dropped here: the port is closed again

    let config = PinataConfig::new(SecretString::new(JWT.to_owned()))?
        .with_upload_base_url(&dead_url)?
        .with_retries(1, Duration::from_millis(1))
        .with_timeout(Duration::from_secs(5));
    let uploader = MetadataUploader::with_client(test_client()?, config)?;

    let err = err_of(uploader.upload_image(&png_image()?, "KRYP").await)?;
    assert_no_jwt_leak(&err);
    match err {
        IpfsError::RetriesExhausted { attempts, source } => {
            assert_eq!(attempts, 2);
            assert!(matches!(*source, IpfsError::Transport(_)), "{source}");
        }
        other => return Err(format!("unexpected error: {other}").into()),
    }
    Ok(())
}

#[test]
fn retryability_classification() -> TestResult {
    for status in [408, 425, 429, 500, 502, 503, 599] {
        let err = IpfsError::Api {
            status,
            message: String::new(),
        };
        assert!(err.is_retryable(), "{status} should be retryable");
    }
    for status in [400, 404, 409, 422] {
        let err = IpfsError::Api {
            status,
            message: String::new(),
        };
        assert!(!err.is_retryable(), "{status} should be permanent");
    }
    assert!(IpfsError::Timeout.is_retryable());
    for permanent in [
        IpfsError::Unauthorized { status: 401 },
        IpfsError::InvalidCid,
        IpfsError::EmptyImage,
        IpfsError::MissingCredentials,
        IpfsError::InvalidResponse("x"),
    ] {
        assert!(!permanent.is_retryable(), "{permanent}");
    }

    // A request that never left the builder is a permanent failure.
    let builder_error = err_of(test_client()?.get("not a url").build())?;
    assert!(!IpfsError::Transport(builder_error).is_retryable());
    Ok(())
}

// =================================================================================
// Images
// =================================================================================

#[test]
fn image_formats_are_detected_by_magic_bytes() -> TestResult {
    let mut webp = b"RIFF\0\0\0\0WEBPVP8 ".to_vec();
    webp.resize(40, 0);
    let cases: [(Vec<u8>, ImageFormat, &str, &str); 5] = [
        (png_bytes(), ImageFormat::Png, "image/png", "png"),
        (
            vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F'],
            ImageFormat::Jpeg,
            "image/jpeg",
            "jpg",
        ),
        (
            b"GIF89a\x01\x00\x01\x00".to_vec(),
            ImageFormat::Gif,
            "image/gif",
            "gif",
        ),
        (
            b"GIF87a\x01\x00\x01\x00".to_vec(),
            ImageFormat::Gif,
            "image/gif",
            "gif",
        ),
        (webp, ImageFormat::Webp, "image/webp", "webp"),
    ];

    for (bytes, format, mime, extension) in cases {
        let len = bytes.len();
        let image = ImageAsset::from_bytes(bytes)?;
        assert_eq!(image.format(), format);
        assert_eq!(image.format().mime(), mime);
        assert_eq!(image.format().extension(), extension);
        assert_eq!(image.len(), len);
        assert!(!image.is_empty());
    }
    Ok(())
}

#[test]
fn invalid_images_are_rejected() -> TestResult {
    assert!(matches!(
        err_of(ImageAsset::from_bytes(Vec::new()))?,
        IpfsError::EmptyImage
    ));
    for bad in [
        b"hello world".to_vec(),
        b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec(),
        b"RIFF\0\0\0\0WAVEfmt ".to_vec(), // RIFF container, but not WebP
        vec![0x89, b'P', b'N'],           // truncated PNG signature
        vec![0xFF, 0xD8],                 // truncated JPEG signature
    ] {
        assert!(matches!(
            err_of(ImageAsset::from_bytes(bad))?,
            IpfsError::UnsupportedImageFormat
        ));
    }
    Ok(())
}

#[test]
fn image_size_limit_is_enforced_exactly() -> TestResult {
    let limit = usize::try_from(MAX_IMAGE_BYTES)?;

    let mut exact = png_bytes();
    exact.resize(limit, 0);
    assert_eq!(ImageAsset::from_bytes(exact)?.len(), limit);

    let mut over = png_bytes();
    over.resize(limit + 1, 0);
    match err_of(ImageAsset::from_bytes(over))? {
        IpfsError::ImageTooLarge { size, max } => {
            assert_eq!(size, MAX_IMAGE_BYTES + 1);
            assert_eq!(max, MAX_IMAGE_BYTES);
        }
        other => return Err(format!("unexpected error: {other}").into()),
    }
    Ok(())
}

#[tokio::test]
async fn images_load_from_disk_with_validation() -> TestResult {
    let path = temp_path("ok.png");
    std::fs::write(&path, png_bytes())?;
    let loaded = ImageAsset::from_path(&path).await;
    let _ = std::fs::remove_file(&path);
    assert_eq!(loaded?.format(), ImageFormat::Png);

    let missing = temp_path("does-not-exist.png");
    assert!(matches!(
        err_of(ImageAsset::from_path(&missing).await)?,
        IpfsError::ImageRead(std::io::ErrorKind::NotFound)
    ));

    let text_file = temp_path("not-an-image.txt");
    std::fs::write(&text_file, b"plain text")?;
    let rejected = ImageAsset::from_path(&text_file).await;
    let _ = std::fs::remove_file(&text_file);
    assert!(matches!(
        err_of(rejected)?,
        IpfsError::UnsupportedImageFormat
    ));

    // A directory is not an image file (the exact I/O kind is platform specific).
    assert!(matches!(
        err_of(ImageAsset::from_path(&std::env::temp_dir()).await)?,
        IpfsError::ImageRead(_)
    ));
    Ok(())
}

#[tokio::test]
async fn oversized_files_are_refused_before_being_read() -> TestResult {
    let path = temp_path("huge.bin");
    let mut big = png_bytes();
    big.resize(usize::try_from(MAX_IMAGE_BYTES)? + 1, 0);
    std::fs::write(&path, big)?;
    let result = ImageAsset::from_path(&path).await;
    let _ = std::fs::remove_file(&path);

    assert!(matches!(err_of(result)?, IpfsError::ImageTooLarge { .. }));
    Ok(())
}

// =================================================================================
// Document, CIDs and URIs
// =================================================================================

#[test]
fn metadata_document_serialization_is_compact_and_stable() -> TestResult {
    let metadata = TokenMetadata::new("A", "B", "");
    let uri = format!("{GATEWAY}/{IMAGE_CID}");
    let bytes = MetadataDocument::new(&metadata, &uri, ImageFormat::Png).to_json_bytes()?;

    let template = concat!(
        r#"{"name":"A","symbol":"B","description":"","image":"@URI@","#,
        r#""showName":true,"createdOn":"https://pump.fun","attributes":[],"#,
        r#""properties":{"files":[{"uri":"@URI@","type":"image/png"}],"category":"image"}}"#
    );
    let expected = template.replace("@URI@", &uri);
    assert_eq!(String::from_utf8(bytes)?, expected);
    Ok(())
}

#[test]
fn cids_are_validated_before_use() -> TestResult {
    let shortest = "a".repeat(16);
    let longest = "a".repeat(128);
    let too_short = "a".repeat(15);
    let too_long = "a".repeat(129);
    for valid in [
        "QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG",
        IMAGE_CID,
        shortest.as_str(),
        longest.as_str(),
    ] {
        assert_eq!(IpfsCid::parse(valid)?.as_str(), valid);
        assert_eq!(IpfsCid::parse(valid)?.to_string(), valid);
    }
    for invalid in [
        "",
        "short",
        "../../etc/passwd/aaaa",
        "bafy?query=1&aaaaaaaaaaaa",
        "bafy with space aaaaaaaaaa",
        "bafy#fragment-aaaaaaaaaaaa",
        "bäfkreigh2akiscaildcqabsyg3dfr6chu3fgpregiym",
        too_long.as_str(),
        too_short.as_str(),
    ] {
        assert!(
            matches!(err_of(IpfsCid::parse(invalid))?, IpfsError::InvalidCid),
            "{invalid:?}"
        );
    }
    Ok(())
}

#[test]
fn gateway_uris_respect_the_onchain_length_limit() -> TestResult {
    let cid = IpfsCid::parse(IMAGE_CID)?;

    let normal = PinataConfig::new(SecretString::new(JWT.to_owned()))?
        .with_gateway_base_url("https://gateway.example/ipfs/")?; // trailing slash is normalized
    let uploader = MetadataUploader::new(normal)?;
    assert_eq!(
        uploader.gateway_uri(&cid)?,
        format!("{GATEWAY}/{IMAGE_CID}")
    );

    let long_base = format!("https://gateway.example/{}", "a".repeat(170));
    let long =
        PinataConfig::new(SecretString::new(JWT.to_owned()))?.with_gateway_base_url(&long_base)?;
    let uploader = MetadataUploader::new(long)?;
    match err_of(uploader.gateway_uri(&cid))? {
        IpfsError::UriTooLong { len, max } => {
            assert!(len > MAX_URI_BYTES);
            assert_eq!(max, MAX_URI_BYTES);
        }
        other => return Err(format!("unexpected error: {other}").into()),
    }
    Ok(())
}

// =================================================================================
// Configuration
// =================================================================================

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

#[test]
fn pinata_config_defaults() -> TestResult {
    let config = PinataConfig::from_vars(vars(&[("PINATA_JWT", JWT)]))?;
    assert_eq!(
        config.upload_base_url.host_str(),
        Some("uploads.pinata.cloud")
    );
    assert_eq!(config.gateway_base_url.host_str(), Some("ipfs.io"));
    assert_eq!(config.timeout, Duration::from_secs(30));
    assert_eq!(config.max_retries, 2);
    Ok(())
}

#[test]
fn pinata_config_overrides_and_blank_values() -> TestResult {
    let config = PinataConfig::from_vars(vars(&[
        ("PINATA_JWT", JWT),
        ("PINATA_UPLOAD_URL", "https://uploads.example"),
        ("IPFS_GATEWAY_URL", "https://gw.example/ipfs"),
        ("IPFS_TIMEOUT_SECS", "12"),
        ("IPFS_MAX_RETRIES", "4"),
    ]))?;
    assert_eq!(config.upload_base_url.host_str(), Some("uploads.example"));
    assert_eq!(config.gateway_base_url.host_str(), Some("gw.example"));
    assert_eq!(config.timeout, Duration::from_secs(12));
    assert_eq!(config.max_retries, 4);

    let blank = PinataConfig::from_vars(vars(&[
        ("PINATA_JWT", JWT),
        ("PINATA_UPLOAD_URL", "  "),
        ("IPFS_TIMEOUT_SECS", ""),
    ]))?;
    assert_eq!(blank.timeout, Duration::from_secs(30));
    Ok(())
}

#[test]
fn pinata_config_requires_a_jwt() -> TestResult {
    for map in [HashMap::new(), vars(&[("PINATA_JWT", "   ")])] {
        let err = err_of(PinataConfig::from_vars(map))?;
        assert!(
            matches!(err, ConfigError::MissingVar { name: "PINATA_JWT" }),
            "{err}"
        );
    }
    assert!(matches!(
        err_of(PinataConfig::new(SecretString::new("  ".to_owned())))?,
        IpfsError::MissingCredentials
    ));
    Ok(())
}

#[test]
fn pinata_config_rejects_unsafe_or_malformed_values() -> TestResult {
    let cases = [
        ("PINATA_UPLOAD_URL", "http://uploads.example"), // cleartext JWT
        ("PINATA_UPLOAD_URL", "ftp://uploads.example"),
        ("PINATA_UPLOAD_URL", "https://user:pw@uploads.example"),
        ("PINATA_UPLOAD_URL", "https://uploads.example/?token=1"),
        ("PINATA_UPLOAD_URL", "not a url"),
        ("IPFS_GATEWAY_URL", "http://ipfs.io/ipfs"), // gateway must be https, even on loopback
        ("IPFS_GATEWAY_URL", "http://127.0.0.1:8080/ipfs"),
        ("IPFS_GATEWAY_URL", "https://ipfs.io/ipfs#frag"),
        ("IPFS_TIMEOUT_SECS", "0"),
        ("IPFS_TIMEOUT_SECS", "301"),
        ("IPFS_TIMEOUT_SECS", "abc"),
        ("IPFS_MAX_RETRIES", "6"),
        ("IPFS_MAX_RETRIES", "-1"),
    ];
    for (key, value) in cases {
        let err = err_of(PinataConfig::from_vars(vars(&[
            ("PINATA_JWT", JWT),
            (key, value),
        ])))?;
        assert!(
            matches!(&err, ConfigError::InvalidVar { name, .. } if *name == key),
            "{key}={value:?}: {err}"
        );
        assert!(!err.to_string().contains("SECRET-VALUE"));
    }
    Ok(())
}

#[test]
fn loopback_hosts_may_use_plain_http_for_the_upload_endpoint() -> TestResult {
    for url in [
        "http://127.0.0.1:8080",
        "http://localhost:9000",
        "http://[::1]:8080",
    ] {
        PinataConfig::new(SecretString::new(JWT.to_owned()))?.with_upload_base_url(url)?;
    }
    Ok(())
}

#[test]
fn retry_count_is_capped() -> TestResult {
    let config = PinataConfig::new(SecretString::new(JWT.to_owned()))?
        .with_retries(1_000, Duration::from_millis(5));
    assert_eq!(config.max_retries, MAX_RETRIES_CAP);
    Ok(())
}

#[test]
fn debug_output_never_reveals_the_jwt() -> TestResult {
    let config = PinataConfig::new(SecretString::new(JWT.to_owned()))?
        .with_upload_base_url("https://uploads.example/private/path")?;
    let uploader_debug = format!(
        "{:?}",
        MetadataUploader::new(PinataConfig::new(SecretString::new(JWT.to_owned()),)?)?
    );
    let config_debug = format!("{config:?}");

    for rendered in [&config_debug, &uploader_debug] {
        assert!(!rendered.contains("SECRET-VALUE"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }
    assert!(!config_debug.contains("private/path"), "{config_debug}");
    Ok(())
}

// =================================================================================
// Error plumbing
// =================================================================================

#[test]
fn sanitizer_neutralizes_urls_tokens_and_control_characters() {
    assert_eq!(
        sanitize_remote_message("see https://x.y/?k=SECRET now"),
        "see <url> now"
    );
    assert_eq!(
        sanitize_remote_message("token eyJhbGciOiJIUzI1NiJ9.e30.sig failed"),
        "token <redacted> failed"
    );
    assert_eq!(
        sanitize_remote_message("line1\nline2\t\u{0007}end   spaced"),
        "line1 line2 end spaced"
    );
    assert_eq!(sanitize_remote_message(""), "");
    assert_eq!(sanitize_remote_message("   \n "), "");
}

#[test]
fn sanitizer_truncates_on_character_boundaries() {
    let long = "é".repeat(1_000);
    let cleaned = sanitize_remote_message(&long);
    assert_eq!(cleaned.chars().count(), MAX_REMOTE_MESSAGE_CHARS);
    assert!(cleaned.chars().all(|ch| ch == 'é'));
}

#[test]
fn ipfs_errors_convert_into_the_launcher_error() {
    let wrapped: LauncherError = IpfsError::Timeout.into();
    assert!(wrapped.to_string().starts_with("IPFS error:"));
    assert!(matches!(wrapped, LauncherError::Ipfs(IpfsError::Timeout)));

    fn pipeline(raw: Vec<u8>) -> krypton::Result<ImageAsset> {
        Ok(ImageAsset::from_bytes(raw)?)
    }
    assert!(pipeline(png_bytes()).is_ok());
    assert!(matches!(
        pipeline(Vec::new()),
        Err(LauncherError::Ipfs(IpfsError::EmptyImage))
    ));
}

// =================================================================================
// Shared image buffers (audit finding #15) and retry payloads
// =================================================================================

#[test]
fn image_handles_share_one_buffer() -> TestResult {
    let image = png_image()?;
    let clone = image.clone();
    let shared = image.shared_bytes();

    // Same allocation: cloning never copies the pixels.
    assert_eq!(image.as_bytes().as_ptr(), clone.as_bytes().as_ptr());
    assert_eq!(image.as_bytes().as_ptr(), shared.as_ptr());
    assert_eq!(shared.len(), image.len());
    assert_eq!(&shared[..], image.as_bytes());
    Ok(())
}

#[test]
fn from_shared_validates_like_from_bytes() -> TestResult {
    let bytes = Bytes::from(png_bytes());
    let image = ImageAsset::from_shared(bytes.clone())?;
    assert_eq!(image.as_bytes().as_ptr(), bytes.as_ptr());
    assert_eq!(image.format(), ImageFormat::Png);
    assert_eq!(image.len(), bytes.len());

    assert!(matches!(
        err_of(ImageAsset::from_shared(Bytes::new()))?,
        IpfsError::EmptyImage
    ));
    assert!(matches!(
        err_of(ImageAsset::from_shared(Bytes::from_static(b"not an image")))?,
        IpfsError::UnsupportedImageFormat
    ));
    Ok(())
}

#[tokio::test]
async fn every_retry_attempt_resends_the_complete_image() -> TestResult {
    let server = MockServer::start(|index, _| {
        if index < 2 {
            MockResponse::json(503, r#"{"error":"try later"}"#)
        } else {
            ok_cid(IMAGE_CID)
        }
    })?;
    let uploader = uploader(&server, 3)?;
    let image = png_image()?;

    let cid = uploader.upload_image(&image, "KRYP").await?;
    assert_eq!(cid.as_str(), IMAGE_CID);

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    for (attempt, request) in requests.iter().enumerate() {
        assert!(
            request.body_contains(image.as_bytes()),
            "attempt {attempt} lost image bytes"
        );
        assert!(request.body_contains(b"-image.png\""));
    }
    // The shared handle is still intact after three sends.
    assert_eq!(image.as_bytes(), png_bytes().as_slice());
    Ok(())
}

#[tokio::test]
async fn uploads_declare_a_content_length() -> TestResult {
    let server = MockServer::start(|_, _| ok_cid(IMAGE_CID))?;
    let uploader = uploader(&server, 0)?;
    uploader.upload_image(&png_image()?, "KRYP").await?;

    let requests = server.requests();
    let request = requests.first().ok_or("no request recorded")?;
    let declared: usize = request
        .header("content-length")
        .ok_or("missing Content-Length")?
        .parse()?;
    assert_eq!(declared, request.body.len());
    assert!(request.header("transfer-encoding").is_none());
    Ok(())
}

#[tokio::test]
async fn retried_document_uploads_resend_the_same_json() -> TestResult {
    let server = MockServer::start(|index, _| {
        if index == 0 {
            MockResponse::json(502, "bad gateway")
        } else {
            ok_cid(METADATA_CID)
        }
    })?;
    let uploader = uploader(&server, 2)?;
    let document = MetadataDocument::new(
        &sample_metadata(),
        "https://gateway.example/ipfs/img",
        ImageFormat::Png,
    );

    uploader.upload_document(&document, "KRYP").await?;
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let first = embedded_json(requests.first().ok_or("missing first request")?)?;
    let second = embedded_json(requests.get(1).ok_or("missing second request")?)?;
    assert_eq!(first, second);
    Ok(())
}
