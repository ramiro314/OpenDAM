//! Anthropic with API-key access: one Messages API request per asset, sent from the backend
//! over HTTPS (docs/plans/ai-auto-tagging/plan.md, "Providers").
//!
//! - **Structured output by mode:** the answer is JSON that the schema of the run's mode
//!   allows. Existing only allows catalog tag ids alone, the other modes new tag names too.
//! - **Caching:** the instructions and the catalog vocabulary form one system block marked
//!   `cache_control`, so later assets reuse that prefix. The run's own vocabulary changes as
//!   the run goes, so it rides in the user message, after the image.
//! - **Refusals:** requests to Sonnet 5.5 and Opus 5.5 ask for the server-side fallback. Haiku
//!   5.5 has none. A refusal that still comes back fails the item.

use base64::Engine as _;
use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

use super::credentials::{self, Credentials};
use super::pricing::{self, TokenUsage};
use super::provider::{
    AssetInfo, ErrorClass, Mode, ProposedTag, ProviderError, TagFuture, TagProvider, TagRequest,
    TagResult, Thumbnail, Usage,
};
use super::settings::{AiSettings, DEFAULT_ANTHROPIC_MODEL};
use super::vocabulary::Vocabulary;

pub const API_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
/// The beta that `fallbacks: "default"` needs.
pub const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Room for low-effort thinking plus the answer, which is about 100 tokens.
const MAX_TOKENS: u32 = 2048;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Models with a server-side refusal fallback. Haiku 5.5 has none, and a custom id may not.
fn supports_fallback(model: &str) -> bool {
    matches!(model, "claude-sonnet-5-5" | "claude-opus-5-5")
}

/// A Messages API request: its JSON body and the beta header it needs, if any.
#[derive(Debug, Clone, PartialEq)]
pub struct MessagesRequest {
    pub body: Value,
    pub beta: Option<&'static str>,
}

/// The JSON schema of the answer. Existing only has no `proposed` field. Array lengths can't
/// be constrained in the schema, so the caps are stated in the instructions and enforced when
/// the answer is read.
fn answer_schema(mode: Mode) -> Value {
    let existing = json!({ "type": "array", "items": { "type": "string" } });
    if mode == Mode::ExistingOnly {
        return json!({
            "type": "object",
            "properties": { "existing": existing },
            "required": ["existing"],
            "additionalProperties": false,
        });
    }
    json!({
        "type": "object",
        "properties": {
            "existing": existing,
            "proposed": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "parent_id": { "anyOf": [{ "type": "string" }, { "type": "null" }] },
                    },
                    "required": ["name", "parent_id"],
                    "additionalProperties": false,
                },
            },
        },
        "required": ["existing", "proposed"],
        "additionalProperties": false,
    })
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The cached system block: what to do in this mode, then every catalog tag as
/// `id: path - hint`. It depends only on the mode, the caps and the catalog, never on the asset.
fn system_prompt(mode: Mode, max_tags: u32, max_proposals: u32, vocabulary: &Vocabulary) -> String {
    let mut text = String::from(
        "You tag game-development assets (3D models, sprites and textures) in an asset library. \
         Each request is one asset: its thumbnail when it has one, then its file name, path \
         relative to the library, kind, extension and metadata.\n\n",
    );
    text.push_str(&format!(
        "Put in \"existing\" the ids of the catalog tags below that fit the asset, at most \
         {max_tags}. Use only ids from the catalog, exactly as written. Pick a tag only when the \
         thumbnail or the name clearly supports it: a few accurate tags beat many guesses. \
         Return an empty list when nothing fits.\n"
    ));
    match mode {
        Mode::ExistingOnly => {}
        Mode::PreferExisting | Mode::CreateFreely => {
            text.push_str(&format!(
                "\nPut in \"proposed\" new tag names, at most {max_proposals}, only for something \
                 important about the asset that no catalog tag covers. Prefer a catalog tag \
                 whenever one fits, and reuse a name this run already proposed rather than a \
                 near-duplicate. Write each name like the catalog's names, without its parents. \
                 Set parent_id to the id of the catalog tag it belongs under, or null for a \
                 top-level tag.\n"
            ));
            text.push_str(if mode == Mode::CreateFreely {
                "New tags are created and applied without review, so propose only names you \
                 are sure of.\n"
            } else {
                "A person reviews every proposed tag before it is created.\n"
            });
        }
    }
    text.push_str("\nCatalog tags, one per line as `id: path`, with a hint after \" - \":\n");
    if vocabulary.tags.is_empty() {
        text.push_str("(the catalog is empty)\n");
    }
    for tag in &vocabulary.tags {
        text.push_str(&format!("{}: {}", tag.id, tag.path));
        let hint = one_line(&tag.hint);
        if !hint.is_empty() {
            text.push_str(&format!(" - {hint}"));
        }
        text.push('\n');
    }
    text
}

/// The asset as text, then the run's own vocabulary when it has any.
fn asset_text(asset: &AssetInfo, run_proposals: &[ProposedTag], vocabulary: &Vocabulary) -> String {
    let metadata = match &asset.metadata {
        Value::Null => "none".to_string(),
        Value::Object(fields) if fields.is_empty() => "none".to_string(),
        other => other.to_string(),
    };
    let mut text = format!(
        "File name: {}\nPath: {}\nKind: {}\nExtension: {}\nMetadata: {metadata}\n",
        asset.file_name, asset.relative_path, asset.kind, asset.extension
    );
    if !run_proposals.is_empty() {
        text.push_str("\nNew tags this run already proposed:\n");
        for proposal in run_proposals {
            let parent = proposal
                .parent_id
                .as_deref()
                .and_then(|id| vocabulary.tags.iter().find(|t| t.id == id));
            match parent {
                Some(parent) => {
                    text.push_str(&format!("- {} (under {})\n", proposal.name, parent.path))
                }
                None => text.push_str(&format!("- {}\n", proposal.name)),
            }
        }
    }
    text
}

fn image_block(image: &Thumbnail) -> Value {
    json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": image.media_type,
            "data": base64::engine::general_purpose::STANDARD.encode(&image.data),
        },
    })
}

/// The production request for one asset.
pub fn build_request(model: &str, request: &TagRequest) -> MessagesRequest {
    let mut content = Vec::new();
    if let Some(image) = &request.image {
        content.push(image_block(image));
    }
    content.push(json!({
        "type": "text",
        "text": asset_text(&request.asset, &request.run_proposals, &request.vocabulary),
    }));
    let max_proposals = if request.mode == Mode::ExistingOnly {
        0
    } else {
        request.max_proposals
    };
    let mut body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "system": [{
            "type": "text",
            "text": system_prompt(request.mode, request.max_tags, max_proposals, &request.vocabulary),
            "cache_control": { "type": "ephemeral" },
        }],
        "messages": [{ "role": "user", "content": content }],
        "output_config": {
            "format": { "type": "json_schema", "schema": answer_schema(request.mode) },
            "effort": "low",
        },
    });
    let beta = supports_fallback(model).then(|| {
        body["fallbacks"] = json!("default");
        FALLBACK_BETA
    });
    MessagesRequest { body, beta }
}

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    stop_details: Option<StopDetails>,
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default)]
    usage: ApiUsage,
}

#[derive(Debug, Deserialize)]
struct StopDetails {
    #[serde(default)]
    category: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ApiUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    cache_creation_input_tokens: Option<i64>,
    #[serde(default)]
    cache_read_input_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct Answer {
    existing: Vec<String>,
    #[serde(default)]
    proposed: Vec<AnswerProposal>,
}

#[derive(Debug, Deserialize)]
struct AnswerProposal {
    name: String,
    #[serde(default)]
    parent_id: Option<String>,
}

/// Tokens and their cost. A fallback model that answered is priced at its own rates.
fn usage_of(response: &MessagesResponse, requested_model: &str) -> Usage {
    let tokens = TokenUsage {
        input: response.usage.input_tokens,
        cache_write: response.usage.cache_creation_input_tokens.unwrap_or(0),
        cache_read: response.usage.cache_read_input_tokens.unwrap_or(0),
        output: response.usage.output_tokens,
    };
    let cost = response
        .model
        .as_deref()
        .and_then(|model| pricing::cost_usd(model, &tokens))
        .or_else(|| pricing::cost_usd(requested_model, &tokens));
    if cost.is_none() {
        tracing::debug!("No price for {requested_model}: its cost counts as 0");
    }
    Usage {
        input_tokens: tokens.prompt(),
        output_tokens: tokens.output,
        cost_usd: cost.unwrap_or(0.0),
    }
}

/// Reads a successful response. Proposals are dropped in Existing only and capped at
/// `max_proposals` otherwise. Unknown tag ids and the tag cap are the engine's job
/// (`Vocabulary::accepted_tags`). A refusal or an answer that doesn't parse fails the item.
pub fn parse_response(
    body: &str,
    mode: Mode,
    max_proposals: u32,
    requested_model: &str,
) -> Result<TagResult, ProviderError> {
    let response: MessagesResponse = serde_json::from_str(body).map_err(|e| {
        ProviderError::new(
            ErrorClass::Item,
            format!("The Anthropic response isn't valid: {e}"),
        )
    })?;
    match response.stop_reason.as_deref() {
        Some("refusal") => {
            let category = response
                .stop_details
                .as_ref()
                .and_then(|d| d.category.as_deref())
                .unwrap_or("no category");
            return Err(ProviderError::new(
                ErrorClass::Item,
                format!("Claude declined to tag this asset ({category})"),
            ));
        }
        Some("max_tokens") => {
            return Err(ProviderError::new(
                ErrorClass::Item,
                "Claude's answer was cut off at the token limit",
            ))
        }
        _ => {}
    }
    let text: String = response
        .content
        .iter()
        .filter(|block| block.kind == "text")
        .filter_map(|block| block.text.as_deref())
        .collect();
    let answer: Answer = serde_json::from_str(&text).map_err(|e| {
        ProviderError::new(
            ErrorClass::Item,
            format!("Claude's answer isn't the expected JSON: {e}"),
        )
    })?;
    let proposed = if mode == Mode::ExistingOnly {
        Vec::new()
    } else {
        answer
            .proposed
            .into_iter()
            .map(|p| ProposedTag {
                name: one_line(&p.name),
                parent_id: p.parent_id.filter(|id| !id.is_empty()),
            })
            .filter(|p| !p.name.is_empty())
            .take(max_proposals as usize)
            .collect()
    };
    Ok(TagResult {
        existing: answer.existing,
        proposed,
        usage: usage_of(&response, requested_model),
    })
}

/// The plan's failure class for an HTTP error status.
pub fn classify_status(status: u16) -> ErrorClass {
    match status {
        408 | 429 | 500..=599 => ErrorClass::Retryable,
        401..=404 => ErrorClass::Fatal,
        _ => ErrorClass::Item,
    }
}

/// `retry-after` in seconds, as the Anthropic API sends it.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let seconds: f64 = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds))
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Debug, Deserialize)]
struct ErrorDetail {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    message: String,
}

fn http_error(status: u16, retry_after: Option<Duration>, body: &str) -> ProviderError {
    let detail = match serde_json::from_str::<ErrorBody>(body) {
        Ok(ErrorBody { error }) => format!("{}: {}", error.kind, error.message),
        Err(_) => "no error details".to_string(),
    };
    let message = match status {
        401 => format!("The Anthropic API rejected the key (401 {detail})"),
        404 => format!("Unknown model or endpoint (404 {detail})"),
        _ => format!("Anthropic API error {status} ({detail})"),
    };
    ProviderError {
        class: classify_status(status),
        message,
        retry_after,
    }
}

fn transport_error(error: reqwest::Error) -> ProviderError {
    let message = if error.is_timeout() {
        "The Anthropic API didn't answer in time".to_string()
    } else {
        format!("Could not reach the Anthropic API: {}", error.without_url())
    };
    ProviderError::new(ErrorClass::Retryable, message)
}

/// The result of "Test connection", as Settings shows it. It never carries the key.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionTest {
    pub ok: bool,
    pub model: String,
    pub message: String,
    /// The test tags the model picked, as paths.
    pub tags: Vec<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
}

/// A tiny catalog for "Test connection": the test image is a red square with a white circle.
fn test_vocabulary() -> Vocabulary {
    let row = |id: &str, name: &str, parent: Option<&str>, hint: &str| {
        (
            id.to_string(),
            name.to_string(),
            None,
            parent.map(str::to_string),
            hint.to_string(),
        )
    };
    Vocabulary::from_rows(&[
        row("test-color", "Color", None, ""),
        row("test-color-red", "Red", Some("test-color"), "Mostly red"),
        row("test-color-blue", "Blue", Some("test-color"), "Mostly blue"),
        row("test-shape", "Shape", None, ""),
        row(
            "test-shape-circle",
            "Circle",
            Some("test-shape"),
            "A round shape",
        ),
        row("test-shape-triangle", "Triangle", Some("test-shape"), ""),
    ])
}

fn test_image() -> Thumbnail {
    let image = image::RgbImage::from_fn(64, 64, |x, y| {
        let (dx, dy) = (x as i32 - 32, y as i32 - 32);
        if dx * dx + dy * dy <= 18 * 18 {
            image::Rgb([255, 255, 255])
        } else {
            image::Rgb([220, 30, 30])
        }
    });
    let mut png = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut png, image::ImageFormat::Png)
        .expect("a PNG encodes in memory");
    Thumbnail {
        media_type: "image/png",
        data: png.into_inner(),
    }
}

/// The "Test connection" request: the production shape (image block, structured output, low
/// effort, the cached system block and, where allowed, the fallback) over a test asset.
pub fn test_request(model: &str) -> TagRequest {
    TagRequest {
        run_id: "connection-test".into(),
        mode: Mode::ExistingOnly,
        model: Some(model.to_string()),
        max_tags: 8,
        max_proposals: 0,
        asset: AssetInfo {
            file_name: "test-image.png".into(),
            relative_path: "connection-test/test-image.png".into(),
            kind: "texture".into(),
            extension: "png".into(),
            metadata: json!({ "width": 64, "height": 64 }),
        },
        image: Some(test_image()),
        vocabulary: Arc::new(test_vocabulary()),
        run_proposals: Vec::new(),
    }
}

pub struct AnthropicProvider {
    credentials: Arc<Credentials>,
    base_url: String,
    http: Result<reqwest::Client, String>,
}

impl AnthropicProvider {
    /// Talks to the Anthropic API, or in a debug build to `OPENDAM_ANTHROPIC_BASE_URL` when it
    /// is set, so a local stand-in can serve screenshots without a key.
    pub fn new(credentials: Arc<Credentials>) -> Self {
        #[cfg(debug_assertions)]
        if let Ok(url) = std::env::var("OPENDAM_ANTHROPIC_BASE_URL") {
            tracing::info!("OPENDAM_ANTHROPIC_BASE_URL: Anthropic requests go to {url}");
            return Self::with_base_url(credentials, url);
        }
        Self::with_base_url(credentials, API_URL.to_string())
    }

    pub fn with_base_url(credentials: Arc<Credentials>, base_url: String) -> Self {
        // reqwest's rustls needs a process-wide provider; installing it twice is harmless
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| format!("Could not set up HTTPS: {e}"));
        Self {
            credentials,
            base_url: base_url.trim_end_matches('/').to_string(),
            http,
        }
    }

    /// The key, read off the async workers because the keyring blocks on D-Bus.
    async fn api_key(&self) -> Result<String, ProviderError> {
        let credentials = self.credentials.clone();
        tokio::task::spawn_blocking(move || credentials.api_key(credentials::Provider::Anthropic))
            .await
            .map_err(|e| ProviderError::new(ErrorClass::Fatal, e.to_string()))?
            .map_err(|e| ProviderError::new(ErrorClass::Fatal, e))?
            .ok_or_else(|| ProviderError::new(ErrorClass::Fatal, "No Anthropic API key is set"))
    }

    /// Sends `request` and returns the body of a successful response.
    async fn send(&self, request: &MessagesRequest) -> Result<String, ProviderError> {
        let http = self
            .http
            .as_ref()
            .map_err(|e| ProviderError::new(ErrorClass::Fatal, e.clone()))?;
        let mut key = HeaderValue::from_str(&self.api_key().await?).map_err(|_| {
            ProviderError::new(
                ErrorClass::Fatal,
                "The Anthropic API key has characters a header can't carry",
            )
        })?;
        key.set_sensitive(true);
        let mut builder = http
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", key)
            .header("anthropic-version", API_VERSION)
            .json(&request.body);
        if let Some(beta) = request.beta {
            builder = builder.header("anthropic-beta", beta);
        }
        let response = builder.send().await.map_err(transport_error)?;
        let status = response.status().as_u16();
        let wait = retry_after(response.headers());
        let body = response.text().await.map_err(transport_error)?;
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(http_error(status, wait, &body))
        }
    }

    /// Sends the test request with the saved key to `model`. In a debug build with
    /// `OPENDAM_ANTHROPIC_FIXTURE=<path>`, a successful response is also saved there, after
    /// checking that it holds no key, to become the parser fixture.
    pub async fn test_connection(&self, model: &str) -> ConnectionTest {
        let request = test_request(model);
        let outcome = async {
            let body = self.send(&build_request(model, &request)).await?;
            #[cfg(debug_assertions)]
            self.save_fixture(&body).await;
            parse_response(&body, request.mode, 0, model)
        }
        .await;
        match outcome {
            Ok(result) => {
                let accepted = request.vocabulary.accepted_tags(&result, 8);
                let tags: Vec<String> = accepted
                    .iter()
                    .filter_map(|id| request.vocabulary.tags.iter().find(|t| &t.id == id))
                    .map(|t| t.path.clone())
                    .collect();
                let message = if tags.is_empty() {
                    format!("Connected to {model}, but it found no tags on the test image")
                } else {
                    format!(
                        "Connected to {model}. It tagged the test image {}",
                        tags.join(", ")
                    )
                };
                ConnectionTest {
                    ok: true,
                    model: model.to_string(),
                    message,
                    tags,
                    input_tokens: result.usage.input_tokens,
                    output_tokens: result.usage.output_tokens,
                    cost_usd: result.usage.cost_usd,
                }
            }
            Err(error) => ConnectionTest {
                ok: false,
                model: model.to_string(),
                message: error.message,
                tags: Vec::new(),
                input_tokens: 0,
                output_tokens: 0,
                cost_usd: 0.0,
            },
        }
    }

    #[cfg(debug_assertions)]
    async fn save_fixture(&self, body: &str) {
        let Ok(path) = std::env::var("OPENDAM_ANTHROPIC_FIXTURE") else {
            return;
        };
        let Ok(key) = self.api_key().await else {
            return;
        };
        if body.contains(&key) {
            tracing::warn!("Not saving the Anthropic fixture: the response holds the key");
            return;
        }
        let pretty = serde_json::from_str::<Value>(body)
            .and_then(|v| serde_json::to_string_pretty(&v))
            .unwrap_or_else(|_| body.to_string());
        match std::fs::write(&path, pretty + "\n") {
            Ok(()) => tracing::info!("Saved the Anthropic response fixture to {path}"),
            Err(e) => tracing::warn!("Could not save the Anthropic fixture to {path}: {e}"),
        }
    }
}

impl TagProvider for AnthropicProvider {
    fn id(&self) -> &'static str {
        "anthropic"
    }

    fn label(&self) -> &'static str {
        "Anthropic"
    }

    fn modes(&self) -> &'static [Mode] {
        &Mode::ALL
    }

    fn concurrency(&self, settings: &AiSettings) -> u32 {
        settings.anthropic.concurrency
    }

    fn max_tags(&self, settings: &AiSettings) -> u32 {
        settings.anthropic.max_tags
    }

    fn max_proposals(&self, settings: &AiSettings) -> u32 {
        settings.anthropic.max_proposals
    }

    fn model(&self, settings: &AiSettings) -> Option<String> {
        Some(settings.anthropic.model.clone())
    }

    fn unavailable_reason(&self) -> Option<String> {
        if let Err(e) = &self.http {
            return Some(e.clone());
        }
        if !self
            .credentials
            .status(credentials::Provider::Anthropic)
            .configured
        {
            return Some("Set up Anthropic in Settings".into());
        }
        None
    }

    fn tag<'a>(&'a self, request: &'a TagRequest) -> TagFuture<'a> {
        Box::pin(async move {
            let model = request.model.as_deref().unwrap_or(DEFAULT_ANTHROPIC_MODEL);
            let body = self.send(&build_request(model, request)).await?;
            parse_response(&body, request.mode, request.max_proposals, model)
        })
    }
}

/// A local stand-in for the Messages API, for tests: it answers every request the same way and
/// records what it received. Nothing leaves the machine.
#[cfg(test)]
pub(crate) mod mock {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[derive(Debug, Clone)]
    pub struct Received {
        /// The request line and headers, lowercased.
        pub head: String,
        pub body: String,
    }

    pub struct MockApi {
        pub url: String,
        pub received: Arc<Mutex<Vec<Received>>>,
    }

    impl MockApi {
        pub fn received(&self) -> Vec<Received> {
            self.received.lock().unwrap().clone()
        }
    }

    fn content_length(head: &str) -> usize {
        head.lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Received> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 16384];
        loop {
            let n = socket.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
            let total = end + 4 + content_length(&head);
            while buf.len() < total {
                let n = socket.read(&mut chunk).await.ok()?;
                if n == 0 {
                    return None;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let body = String::from_utf8_lossy(&buf[end + 4..total]).to_string();
            return Some(Received { head, body });
        }
    }

    /// Answers every request with `status`, the extra `headers` and `body`.
    pub async fn serve(status: u16, headers: &[(&str, &str)], body: &str) -> MockApi {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let received = Arc::new(Mutex::new(Vec::new()));
        let mut reply = format!(
            "HTTP/1.1 {status} Mock\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n",
            body.len()
        );
        for (name, value) in headers {
            reply.push_str(&format!("{name}: {value}\r\n"));
        }
        reply.push_str("\r\n");
        reply.push_str(body);
        let recorded = received.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (recorded, reply) = (recorded.clone(), reply.clone());
                tokio::spawn(async move {
                    if let Some(request) = read_request(&mut socket).await {
                        recorded.lock().unwrap().push(request);
                        let _ = socket.write_all(reply.as_bytes()).await;
                        let _ = socket.shutdown().await;
                    }
                });
            }
        });
        MockApi { url, received }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autotag::credentials::MemoryKeyring;
    use crate::db::TagRow;

    const SUCCESS: &str = include_str!("fixtures/anthropic/test_connection.json");
    const UNKNOWN_IDS: &str = include_str!("fixtures/anthropic/unknown_ids.json");
    const PROPOSALS: &str = include_str!("fixtures/anthropic/proposals.json");
    const REFUSAL: &str = include_str!("fixtures/anthropic/refusal.json");
    const RATE_LIMITED: &str = include_str!("fixtures/anthropic/error_429.json");
    const BAD_KEY: &str = include_str!("fixtures/anthropic/error_401.json");

    fn row(id: &str, name: &str, parent: Option<&str>, hint: &str) -> TagRow {
        (
            id.into(),
            name.into(),
            None,
            parent.map(Into::into),
            hint.into(),
        )
    }

    fn vocabulary() -> Vocabulary {
        Vocabulary::from_rows(&[
            row("nature", "Nature", None, ""),
            row("rock", "Rock", Some("nature"), "Boulders and\n stones"),
            row("tree", "Tree", Some("nature"), ""),
        ])
    }

    fn request(mode: Mode, image: bool) -> TagRequest {
        TagRequest {
            run_id: "run".into(),
            mode,
            model: Some("claude-sonnet-5-5".into()),
            max_tags: 8,
            max_proposals: 3,
            asset: AssetInfo {
                file_name: "rock_large.glb".into(),
                relative_path: "models/rock_large.glb".into(),
                kind: "model".into(),
                extension: "glb".into(),
                metadata: json!({ "vertices": 1200 }),
            },
            image: image.then(|| Thumbnail {
                media_type: "image/png",
                data: vec![1, 2, 3],
            }),
            vocabulary: Arc::new(vocabulary()),
            run_proposals: Vec::new(),
        }
    }

    fn user_content(built: &MessagesRequest) -> &Vec<Value> {
        built.body["messages"][0]["content"].as_array().unwrap()
    }

    #[test]
    fn the_image_block_comes_first_then_the_asset_text() {
        let built = build_request("claude-sonnet-5-5", &request(Mode::ExistingOnly, true));

        let content = user_content(&built);
        assert_eq!(content.len(), 2);
        assert_eq!(
            content[0],
            json!({
                "type": "image",
                "source": { "type": "base64", "media_type": "image/png", "data": "AQID" },
            })
        );
        let text = content[1]["text"].as_str().unwrap();
        for expected in [
            "File name: rock_large.glb",
            "Path: models/rock_large.glb",
            "Kind: model",
            "Extension: glb",
            r#"Metadata: {"vertices":1200}"#,
        ] {
            assert!(text.contains(expected), "{expected} in {text}");
        }
    }

    #[test]
    fn an_asset_with_no_thumbnail_is_sent_as_text_alone() {
        let built = build_request("claude-sonnet-5-5", &request(Mode::ExistingOnly, false));

        let content = user_content(&built);
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
    }

    #[test]
    fn the_cached_system_block_holds_the_instructions_and_the_catalog() {
        let built = build_request("claude-sonnet-5-5", &request(Mode::ExistingOnly, true));

        let system = built.body["system"].as_array().unwrap();
        assert_eq!(system.len(), 1, "one block, so one cache prefix");
        assert_eq!(system[0]["cache_control"], json!({ "type": "ephemeral" }));
        let text = system[0]["text"].as_str().unwrap();
        assert!(text.contains("at most 8"), "{text}");
        assert!(text.contains("nature: Nature\n"), "{text}");
        assert!(text.contains("rock: Nature > Rock - Boulders and stones\n"));
        assert!(text.contains("tree: Nature > Tree\n"));
        assert!(
            !text.contains("rock_large"),
            "the asset stays out of the cache"
        );
        assert_eq!(built.body["output_config"]["effort"], "low");
        assert_eq!(built.body["model"], "claude-sonnet-5-5");
        assert!(built.body["max_tokens"].as_u64().unwrap() >= 1024);
    }

    #[test]
    fn the_run_vocabulary_goes_in_the_user_message_and_keeps_the_cache_prefix() {
        let plain = build_request("claude-sonnet-5-5", &request(Mode::PreferExisting, true));
        let mut with_proposals = request(Mode::PreferExisting, true);
        with_proposals.run_proposals = vec![
            ProposedTag {
                name: "Mossy".into(),
                parent_id: Some("rock".into()),
            },
            ProposedTag {
                name: "Ruins".into(),
                parent_id: None,
            },
        ];
        let built = build_request("claude-sonnet-5-5", &with_proposals);

        assert_eq!(built.body["system"], plain.body["system"]);
        let text = user_content(&built)[1]["text"].as_str().unwrap();
        assert!(text.contains("- Mossy (under Nature > Rock)\n"), "{text}");
        assert!(text.contains("- Ruins\n"), "{text}");
    }

    #[test]
    fn the_schema_follows_the_mode() {
        let schema = |mode| build_request("claude-sonnet-5-5", &request(mode, true)).body;

        let existing_only = schema(Mode::ExistingOnly);
        let format = &existing_only["output_config"]["format"];
        assert_eq!(format["type"], "json_schema");
        assert_eq!(format["schema"]["required"], json!(["existing"]));
        assert!(format["schema"]["properties"].get("proposed").is_none());
        assert_eq!(format["schema"]["additionalProperties"], false);
        let system = existing_only["system"][0]["text"].as_str().unwrap();
        assert!(!system.contains("\"proposed\""), "{system}");

        for mode in [Mode::PreferExisting, Mode::CreateFreely] {
            let body = schema(mode);
            let schema = &body["output_config"]["format"]["schema"];
            assert_eq!(
                schema["required"],
                json!(["existing", "proposed"]),
                "{mode:?}"
            );
            let item = &schema["properties"]["proposed"]["items"];
            assert_eq!(item["required"], json!(["name", "parent_id"]), "{mode:?}");
            assert_eq!(item["additionalProperties"], false);
            let system = body["system"][0]["text"].as_str().unwrap();
            assert!(system.contains("at most 3"), "{mode:?}: {system}");
        }
        let review = schema(Mode::PreferExisting)["system"][0]["text"].clone();
        let create = schema(Mode::CreateFreely)["system"][0]["text"].clone();
        assert!(review
            .as_str()
            .unwrap()
            .contains("reviews every proposed tag"));
        assert!(create.as_str().unwrap().contains("without review"));
    }

    #[test]
    fn the_fallback_and_its_beta_header_go_only_to_models_that_support_it() {
        for model in ["claude-sonnet-5-5", "claude-opus-5-5"] {
            let built = build_request(model, &request(Mode::ExistingOnly, true));
            assert_eq!(built.body["fallbacks"], "default", "{model}");
            assert_eq!(
                built.beta,
                Some("server-side-fallback-2026-07-01"),
                "{model}"
            );
        }
        for model in ["claude-haiku-5-5", "my-custom-model"] {
            let built = build_request(model, &request(Mode::ExistingOnly, true));
            assert!(built.body.get("fallbacks").is_none(), "{model}");
            assert_eq!(built.beta, None, "{model}");
        }
    }

    #[test]
    fn the_test_connection_response_parses_with_its_usage_and_cost() {
        let result = parse_response(SUCCESS, Mode::ExistingOnly, 0, "claude-sonnet-5-5").unwrap();

        let vocabulary = test_vocabulary();
        let accepted = vocabulary.accepted_tags(&result, 8);
        assert!(!accepted.is_empty(), "{result:?}");
        assert_eq!(accepted.len(), result.existing.len(), "only test ids");
        assert!(result.usage.input_tokens > 0 && result.usage.output_tokens > 0);
        assert!(result.usage.cost_usd > 0.0);
    }

    #[test]
    fn unknown_ids_are_dropped_and_the_cap_applies() {
        let result =
            parse_response(UNKNOWN_IDS, Mode::ExistingOnly, 3, "claude-sonnet-5-5").unwrap();

        assert_eq!(result.existing, ["rock", "ghost", "tree", "nature"]);
        assert!(result.proposed.is_empty(), "never in Existing only");
        assert_eq!(
            vocabulary().accepted_tags(&result, 8),
            ["rock", "tree", "nature"]
        );
        assert_eq!(vocabulary().accepted_tags(&result, 2), ["rock", "tree"]);
        // 900 uncached + 100 cache writes + 3000 cache reads, 40 out, on Sonnet 5.5
        assert_eq!(result.usage.input_tokens, 4000);
        assert_eq!(result.usage.output_tokens, 40);
        let expected = (900.0 * 2.0 + 100.0 * 2.5 + 3000.0 * 0.2 + 40.0 * 10.0) / 1e6;
        assert!((result.usage.cost_usd - expected).abs() < 1e-12);
    }

    #[test]
    fn proposals_are_read_past_thinking_blocks_and_capped() {
        let result =
            parse_response(PROPOSALS, Mode::PreferExisting, 2, "claude-sonnet-5-5").unwrap();

        assert_eq!(result.existing, ["rock"]);
        assert_eq!(
            result.proposed,
            [
                ProposedTag {
                    name: "Mossy".into(),
                    parent_id: Some("rock".into()),
                },
                ProposedTag {
                    name: "Ruins".into(),
                    parent_id: None,
                },
            ]
        );
    }

    #[test]
    fn the_model_that_answered_sets_the_rates() {
        let body = |model: &str| {
            format!(
                r#"{{{model}"stop_reason":"end_turn",
                     "content":[{{"type":"text","text":"{{\"existing\":[]}}"}}],
                     "usage":{{"input_tokens":1000000,"output_tokens":0}}}}"#
            )
        };
        let cost = |body: &str, requested: &str| {
            parse_response(body, Mode::ExistingOnly, 0, requested)
                .unwrap()
                .usage
                .cost_usd
        };

        // A fallback to Opus 5.5 is priced at Opus rates ($4/M in), not Sonnet's ($2/M)
        let fallback = body(r#""model":"claude-opus-5-5","#);
        assert!((cost(&fallback, "claude-sonnet-5-5") - 4.0).abs() < 1e-9);
        // No model, or one the table lacks, falls back to the requested model's rates
        assert!((cost(&body(""), "claude-sonnet-5-5") - 2.0).abs() < 1e-9);
        let unknown = body(r#""model":"claude-future-9","#);
        assert!((cost(&unknown, "claude-sonnet-5-5") - 2.0).abs() < 1e-9);
        // Neither priced: the cost counts as 0
        assert_eq!(cost(&unknown, "my-custom-model"), 0.0);
    }

    #[test]
    fn a_final_refusal_fails_the_item() {
        let error =
            parse_response(REFUSAL, Mode::ExistingOnly, 0, "claude-sonnet-5-5").unwrap_err();

        assert_eq!(error.class, ErrorClass::Item);
        assert_eq!(
            error.message,
            "Claude declined to tag this asset (general_harms)"
        );
    }

    #[test]
    fn a_cut_off_or_malformed_answer_fails_the_item() {
        let cut_off = r#"{"stop_reason":"max_tokens","content":[{"type":"text","text":"{\"exi"}],
                          "usage":{"input_tokens":1,"output_tokens":2048}}"#;
        let not_json = r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"rock"}],
                           "usage":{"input_tokens":1,"output_tokens":1}}"#;

        for body in [cut_off, not_json, "<html>"] {
            let error = parse_response(body, Mode::ExistingOnly, 0, "m").unwrap_err();
            assert_eq!(error.class, ErrorClass::Item, "{body}");
        }
    }

    #[test]
    fn http_statuses_map_to_the_plan_failure_classes() {
        for status in [408, 429, 500, 502, 503, 529] {
            assert_eq!(classify_status(status), ErrorClass::Retryable, "{status}");
        }
        for status in [401, 402, 403, 404] {
            assert_eq!(classify_status(status), ErrorClass::Fatal, "{status}");
        }
        for status in [400, 413, 422] {
            assert_eq!(classify_status(status), ErrorClass::Item, "{status}");
        }
    }

    #[test]
    fn retry_after_is_read_in_seconds() {
        let headers = |value: &str| {
            let mut map = HeaderMap::new();
            map.insert(RETRY_AFTER, HeaderValue::from_str(value).unwrap());
            map
        };

        assert_eq!(retry_after(&headers("30")), Some(Duration::from_secs(30)));
        assert_eq!(
            retry_after(&headers("1.5")),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(retry_after(&headers("soon")), None);
        assert_eq!(retry_after(&headers("-1")), None);
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }

    fn provider(url: &str, key: Option<&'static str>) -> AnthropicProvider {
        let credentials = Credentials::new(MemoryKeyring::default(), move |name| {
            (name == "ANTHROPIC_API_KEY")
                .then(|| key.map(str::to_string))
                .flatten()
        });
        AnthropicProvider::with_base_url(Arc::new(credentials), url.to_string())
    }

    #[tokio::test]
    async fn a_request_carries_the_key_version_and_fallback_beta() {
        let api = mock::serve(200, &[], UNKNOWN_IDS).await;
        let anthropic = provider(&api.url, Some("sk-ant-test-key"));

        let result = anthropic
            .tag(&request(Mode::ExistingOnly, true))
            .await
            .unwrap();

        assert_eq!(result.existing.len(), 4);
        let received = api.received();
        assert_eq!(received.len(), 1);
        let head = &received[0].head;
        assert!(head.starts_with("post /v1/messages "), "{head}");
        assert!(head.contains("x-api-key: sk-ant-test-key"), "{head}");
        assert!(head.contains("anthropic-version: 2023-06-01"), "{head}");
        assert!(head.contains("anthropic-beta: server-side-fallback-2026-07-01"));
        let body: Value = serde_json::from_str(&received[0].body).unwrap();
        assert_eq!(
            body,
            build_request("claude-sonnet-5-5", &request(Mode::ExistingOnly, true)).body
        );
    }

    #[tokio::test]
    async fn a_haiku_request_sends_no_beta_header() {
        let api = mock::serve(200, &[], UNKNOWN_IDS).await;
        let anthropic = provider(&api.url, Some("key"));
        let mut haiku = request(Mode::ExistingOnly, false);
        haiku.model = Some("claude-haiku-5-5".into());

        anthropic.tag(&haiku).await.unwrap();

        let head = &api.received()[0].head;
        assert!(!head.contains("anthropic-beta"), "{head}");
    }

    #[tokio::test]
    async fn a_rate_limit_is_retryable_and_passes_retry_after_along() {
        let api = mock::serve(429, &[("retry-after", "7")], RATE_LIMITED).await;
        let anthropic = provider(&api.url, Some("key"));

        let error = anthropic
            .tag(&request(Mode::ExistingOnly, true))
            .await
            .unwrap_err();

        assert_eq!(error.class, ErrorClass::Retryable);
        assert_eq!(error.retry_after, Some(Duration::from_secs(7)));
        assert!(error.message.contains("429"), "{}", error.message);
        assert!(
            error.message.contains("rate_limit_error"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn a_rejected_key_is_fatal() {
        let api = mock::serve(401, &[], BAD_KEY).await;
        let anthropic = provider(&api.url, Some("key"));

        let error = anthropic
            .tag(&request(Mode::ExistingOnly, true))
            .await
            .unwrap_err();

        assert_eq!(error.class, ErrorClass::Fatal);
        assert_eq!(
            error.message,
            "The Anthropic API rejected the key (401 authentication_error: invalid x-api-key)"
        );
    }

    #[tokio::test]
    async fn an_unreachable_api_is_retryable() {
        let anthropic = provider("http://127.0.0.1:1", Some("key"));

        let error = anthropic
            .tag(&request(Mode::ExistingOnly, true))
            .await
            .unwrap_err();

        assert_eq!(error.class, ErrorClass::Retryable, "{}", error.message);
    }

    #[tokio::test]
    async fn with_no_key_nothing_is_sent() {
        let api = mock::serve(200, &[], UNKNOWN_IDS).await;
        let anthropic = provider(&api.url, None);

        assert_eq!(
            anthropic.unavailable_reason().as_deref(),
            Some("Set up Anthropic in Settings")
        );
        let error = anthropic
            .tag(&request(Mode::ExistingOnly, true))
            .await
            .unwrap_err();
        assert_eq!(error.class, ErrorClass::Fatal);
        let test = anthropic.test_connection("claude-sonnet-5-5").await;
        assert!(!test.ok);
        assert_eq!(test.message, "No Anthropic API key is set");
        assert!(api.received().is_empty());
    }

    #[tokio::test]
    async fn test_connection_sends_the_production_shape_and_names_the_tags() {
        let api = mock::serve(200, &[], SUCCESS).await;
        let anthropic = provider(&api.url, Some("key"));

        let test = anthropic.test_connection("claude-sonnet-5-5").await;

        assert!(test.ok, "{test:?}");
        assert_eq!(
            test.message,
            "Connected to claude-sonnet-5-5. It tagged the test image Color > Red, Shape > Circle"
        );
        assert!(test.input_tokens > 0 && test.cost_usd > 0.0);
        let received = api.received();
        assert!(received[0]
            .head
            .contains("anthropic-beta: server-side-fallback-2026-07-01"));
        let body: Value = serde_json::from_str(&received[0].body).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["type"], "image");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["output_config"]["effort"], "low");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert_eq!(body["fallbacks"], "default");
    }

    /// The model saved in Settings when a run starts is the one its requests name, through the
    /// run row, `claim_item` and the engine.
    #[tokio::test]
    async fn a_run_sends_the_model_it_was_started_with() {
        use crate::autotag::engine::{Engine, RunScope};
        use crate::autotag::settings;
        use crate::db::{insert_asset, memory_database, memory_library, memory_library_pool};

        let lib_pool = memory_library_pool().await;
        insert_asset(&lib_pool, "a1").await;
        let db = Arc::new(memory_database(vec![memory_library("lib", lib_pool)]).await);
        settings::update(
            &db.master_pool,
            json!({"anthropic": {"model": "claude-haiku-5-5"}}),
        )
        .await
        .unwrap();
        let api = mock::serve(200, &[], UNKNOWN_IDS).await;
        let engine = Engine::new(
            db.clone(),
            vec![Arc::new(provider(&api.url, Some("key")))],
            Arc::new(|_| {}),
        );
        engine.start().await;

        let started = engine
            .start_run(
                RunScope::Assets {
                    asset_ids: vec!["a1".into()],
                },
                "anthropic",
                Mode::ExistingOnly,
            )
            .await
            .unwrap();
        // A later change to the setting doesn't touch a run already started
        settings::update(
            &db.master_pool,
            json!({"anthropic": {"model": "claude-opus-5-5"}}),
        )
        .await
        .unwrap();
        for _ in 0..500 {
            if !api.received().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(started.run.model.as_deref(), Some("claude-haiku-5-5"));
        let received = api.received();
        assert_eq!(received.len(), 1);
        let body: Value = serde_json::from_str(&received[0].body).unwrap();
        assert_eq!(body["model"], "claude-haiku-5-5");
        assert!(body.get("fallbacks").is_none());
        assert!(
            !received[0].head.contains("anthropic-beta"),
            "{}",
            received[0].head
        );
    }

    #[test]
    fn the_test_image_decodes_as_a_png() {
        let image = test_image();
        let decoded = image::load_from_memory(&image.data).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (64, 64));
    }
}
