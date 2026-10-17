//! Jev, TypeSafe AI's System One model: text only, Existing only
//! (docs/plans/ai-auto-tagging/plan.md, "Providers").
//!
//! - **Questions:** the asset's text is the `state`, and every catalog tag is one `noul` (yes/no
//!   probability) question, with ids `q0..qN` mapped back to tag ids.
//! - **Selection:** tags scoring at or above the run's threshold are kept, highest first, up to
//!   the run's cap.
//! - **Chunking:** at most 50 questions per request, the plan's default until a live call
//!   confirms the limit. TypeSafe's docs state no limit on the number of questions, only on
//!   context: 64k tokens for the state plus every question, 32k for the state plus any one
//!   question (docs.typesafe.ai/models). A request is also closed before a conservative estimate
//!   of the first limit, which matters only for very long hints.
//! - **Never an image:** the request builder has no image field, whatever the request holds.

use reqwest::header::HeaderValue;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};

use super::anthropic::ConnectionTest;
use super::credentials::{self, Credentials};
use super::http::{self, classify_status, retry_after};
use super::pricing::{self, TokenUsage};
use super::provider::{
    AssetInfo, ErrorClass, Mode, ProviderError, TagFuture, TagProvider, TagRequest, TagResult,
    Usage,
};
use super::settings::AiSettings;
use super::vocabulary::{Vocabulary, VocabularyTag};

pub const API_URL: &str = "https://api.typesafe.ai";
pub const MODEL: &str = "jev-latest";
/// The estimated tokens one request may hold: the state once plus its questions. Jev's limit is
/// 64k; the estimate is rough, so it keeps a wide margin.
const REQUEST_TOKEN_BUDGET: usize = 40_000;
/// The most questions one request holds. TypeSafe documents no such limit, but none has been
/// confirmed with a live call yet, so the plan's chunks of 50 stay until one is.
const MAX_QUESTIONS_PER_REQUEST: usize = 50;
/// What a question costs beyond its instructions: its type and the JSON around it.
const QUESTION_OVERHEAD_TOKENS: usize = 8;
/// The most answered chunks kept for retries before the store is emptied. Each entry is a
/// chunk's scores, a few kilobytes at most.
const MAX_ANSWERED_CHUNKS: usize = 2_000;
/// Used when Settings has no threshold for a run, as for runs started before it was recorded.
const DEFAULT_THRESHOLD: f64 = 0.7;

/// Tokens in `text`, overestimated: about three bytes per token, which holds for English and
/// for the three-byte UTF-8 characters that tokenize one per token.
fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(3)
}

/// One Jev request: its body and which tag each of its question ids asks about.
#[derive(Debug, Clone, PartialEq)]
pub struct JevRequest {
    pub body: Value,
    /// `(question id, tag id)`, in question order.
    pub questions: Vec<(String, String)>,
}

/// The asset as Jev's `state`: its text fields by name.
fn state(asset: &AssetInfo) -> Value {
    json!({
        "file_name": asset.file_name,
        "path": asset.relative_path,
        "kind": asset.kind,
        "extension": asset.extension,
        "metadata": asset.metadata,
    })
}

/// The yes/no question for one catalog tag, with its path and hint.
fn question(tag: &VocabularyTag) -> String {
    let mut text = format!(
        "Should the game-development asset in `state` be tagged \"{}\"?",
        tag.path
    );
    let hint = tag.hint.split_whitespace().collect::<Vec<_>>().join(" ");
    if !hint.is_empty() {
        text.push_str(&format!(" The tag means: {hint}"));
    }
    text
}

/// Every request for one asset, each holding at most `MAX_QUESTIONS_PER_REQUEST` questions, and
/// no more than fit `budget` estimated tokens alongside the state. A question too long to share
/// a request gets one to itself.
fn build_requests_within(request: &TagRequest, budget: usize) -> Vec<JevRequest> {
    let state = state(&request.asset);
    let state_tokens = estimate_tokens(&state.to_string());
    let mut requests = Vec::new();
    let mut questions = serde_json::Map::new();
    let mut ids = Vec::new();
    let mut tokens = state_tokens;
    let mut flush = |questions: &mut serde_json::Map<String, Value>,
                     ids: &mut Vec<(String, String)>| {
        if !ids.is_empty() {
            requests.push(JevRequest {
                body: json!({
                    "model": MODEL,
                    "state": state,
                    "questions": std::mem::take(questions),
                }),
                questions: std::mem::take(ids),
            });
        }
    };
    for (index, tag) in request.vocabulary.tags.iter().enumerate() {
        let instructions = question(tag);
        let cost = estimate_tokens(&instructions) + QUESTION_OVERHEAD_TOKENS;
        if ids.len() == MAX_QUESTIONS_PER_REQUEST || (!ids.is_empty() && tokens + cost > budget) {
            flush(&mut questions, &mut ids);
            tokens = state_tokens;
        }
        let id = format!("q{index}");
        questions.insert(
            id.clone(),
            json!({ "type": "noul", "instructions": instructions }),
        );
        ids.push((id, tag.id.clone()));
        tokens += cost;
    }
    flush(&mut questions, &mut ids);
    requests
}

#[derive(Debug, Deserialize)]
struct Response {
    #[serde(default)]
    model: Option<String>,
    answers: HashMap<String, Answer>,
    #[serde(default)]
    usage: ApiUsage,
}

#[derive(Debug, Deserialize)]
struct Answer {
    #[serde(default)]
    noul: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct ApiUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
}

/// What one response says: each tag's score, the model that answered, and its usage.
#[derive(Debug, Clone, PartialEq)]
pub struct Scores {
    /// `(tag id, noul)`, in question order.
    pub scores: Vec<(String, f64)>,
    pub model: String,
    pub usage: Usage,
}

/// Reads a successful response to `request`. A missing or non-numeric answer fails the item.
pub fn parse_response(body: &str, request: &JevRequest) -> Result<Scores, ProviderError> {
    let response: Response = serde_json::from_str(body).map_err(|e| {
        ProviderError::new(
            ErrorClass::Item,
            format!("The Jev response isn't valid: {e}"),
        )
    })?;
    let scores = request
        .questions
        .iter()
        .map(|(question, tag)| {
            response
                .answers
                .get(question)
                .and_then(|a| a.noul)
                .filter(|noul| noul.is_finite())
                .map(|noul| (tag.clone(), noul))
                .ok_or_else(|| {
                    ProviderError::new(
                        ErrorClass::Item,
                        format!("Jev's response has no score for question {question}"),
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let model = response.model.unwrap_or_else(|| MODEL.to_string());
    let tokens = TokenUsage {
        input: response.usage.input_tokens,
        output: response.usage.output_tokens,
        ..Default::default()
    };
    let cost = pricing::cost_usd(&model, &tokens).unwrap_or_else(|| {
        tracing::debug!("No price for {model}: its cost counts as 0");
        0.0
    });
    Ok(Scores {
        scores,
        model,
        usage: Usage {
            input_tokens: tokens.input,
            output_tokens: tokens.output,
            cost_usd: cost,
        },
    })
}

/// The tags to keep: those scoring at least `threshold`, highest first, at most `max_tags`.
/// Equal scores keep catalog order.
pub fn select(scores: &[(String, f64)], threshold: f64, max_tags: usize) -> Vec<String> {
    let mut kept: Vec<&(String, f64)> = scores.iter().filter(|(_, s)| *s >= threshold).collect();
    kept.sort_by(|a, b| b.1.total_cmp(&a.1));
    kept.into_iter()
        .take(max_tags)
        .map(|(tag, _)| tag.clone())
        .collect()
}

/// The error body's own words, whatever shape it takes, or a note that it had none.
fn error_detail(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return "no error details".to_string();
    };
    let text = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    };
    let detail = value
        .pointer("/error/message")
        .and_then(text)
        .or_else(|| value.get("error").and_then(text))
        .or_else(|| value.get("detail").and_then(text))
        .or_else(|| value.get("message").and_then(text))
        .unwrap_or_else(|| "no error details".to_string());
    let mut detail = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    if detail.chars().count() > 200 {
        detail = detail.chars().take(200).collect::<String>() + "…";
    }
    detail
}

fn http_error(status: u16, wait: Option<std::time::Duration>, body: &str) -> ProviderError {
    let detail = error_detail(body);
    let message = match status {
        401 => format!("TypeSafe rejected the Jev API key (401 {detail})"),
        404 => format!("Unknown Jev model or endpoint (404 {detail})"),
        400 if body.contains("max_tokens_exceeded") => {
            format!("The asset is too long for Jev's context (400 {detail})")
        }
        _ => format!("Jev API error {status} ({detail})"),
    };
    ProviderError {
        class: classify_status(status),
        message,
        retry_after: wait,
    }
}

fn transport_error(error: reqwest::Error) -> ProviderError {
    let message = if error.is_timeout() {
        "The Jev API didn't answer in time".to_string()
    } else {
        format!("Could not reach the Jev API: {}", error.without_url())
    };
    ProviderError::new(ErrorClass::Retryable, message)
}

/// A tiny catalog for "Test connection", against a red brick wall texture.
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
        row("test-material", "Material", None, ""),
        row("test-material-brick", "Brick", Some("test-material"), ""),
        row(
            "test-material-wood",
            "Wood",
            Some("test-material"),
            "Planks, logs or bark",
        ),
    ])
}

/// The "Test connection" request: the production shape over a test asset.
pub fn test_request(threshold: f64, max_tags: u32) -> TagRequest {
    TagRequest {
        run_id: "connection-test".into(),
        mode: Mode::ExistingOnly,
        model: None,
        max_tags,
        max_proposals: 0,
        threshold: Some(threshold),
        asset: AssetInfo {
            file_name: "red_brick_wall_albedo.png".into(),
            relative_path: "textures/walls/red_brick_wall_albedo.png".into(),
            kind: "texture".into(),
            extension: "png".into(),
            metadata: json!({ "width": 1024, "height": 1024 }),
        },
        image: None,
        vocabulary: Arc::new(test_vocabulary()),
        run_proposals: Vec::new(),
    }
}

pub struct JevProvider {
    credentials: Arc<Credentials>,
    base_url: String,
    http: Result<reqwest::Client, String>,
    /// Estimated tokens per request; tests lower it to split a small catalog.
    budget: usize,
    /// Chunks already answered for an asset whose later chunk failed, by `chunk_key`. A retry
    /// of the asset sends only the chunks still unanswered, and the stored chunks' usage counts
    /// once, when the asset completes.
    answered: Mutex<HashMap<u64, Scores>>,
}

/// One chunk of one run: its run id and its exact body, so a retry after the catalog or the
/// asset changed asks again.
fn chunk_key(run_id: &str, chunk: &JevRequest) -> u64 {
    let mut hasher = DefaultHasher::new();
    run_id.hash(&mut hasher);
    chunk.body.to_string().hash(&mut hasher);
    hasher.finish()
}

impl JevProvider {
    /// Talks to the TypeSafe API, or in a debug build to `OPENDAM_JEV_BASE_URL` when it is set,
    /// so a local stand-in can serve screenshots without a key.
    pub fn new(credentials: Arc<Credentials>) -> Self {
        #[cfg(debug_assertions)]
        if let Ok(url) = std::env::var("OPENDAM_JEV_BASE_URL") {
            tracing::info!("OPENDAM_JEV_BASE_URL: Jev requests go to {url}");
            return Self::with_base_url(credentials, url);
        }
        Self::with_base_url(credentials, API_URL.to_string())
    }

    pub fn with_base_url(credentials: Arc<Credentials>, base_url: String) -> Self {
        Self {
            credentials,
            base_url: base_url.trim_end_matches('/').to_string(),
            http: http::client(),
            budget: REQUEST_TOKEN_BUDGET,
            answered: Mutex::new(HashMap::new()),
        }
    }

    /// The key, read off the async workers because the keyring blocks on D-Bus.
    async fn api_key(&self) -> Result<String, ProviderError> {
        let credentials = self.credentials.clone();
        tokio::task::spawn_blocking(move || credentials.api_key(credentials::Provider::Jev))
            .await
            .map_err(|e| ProviderError::new(ErrorClass::Fatal, e.to_string()))?
            .map_err(|e| ProviderError::new(ErrorClass::Fatal, e))?
            .ok_or_else(|| ProviderError::new(ErrorClass::Fatal, "No TypeSafe API key is set"))
    }

    /// Sends `request` and reads its scores.
    async fn send(&self, key: &HeaderValue, request: &JevRequest) -> Result<Scores, ProviderError> {
        let http = self
            .http
            .as_ref()
            .map_err(|e| ProviderError::new(ErrorClass::Fatal, e.clone()))?;
        let response = http
            .post(format!("{}/v1/systemone", self.base_url))
            .header(reqwest::header::AUTHORIZATION, key.clone())
            .json(&request.body)
            .send()
            .await
            .map_err(transport_error)?;
        let status = response.status().as_u16();
        let wait = retry_after(response.headers());
        let body = response.text().await.map_err(transport_error)?;
        if (200..300).contains(&status) {
            parse_response(&body, request)
        } else {
            Err(http_error(status, wait, &body))
        }
    }

    /// Every tag's score for one asset, one request per chunk of questions, sent one after
    /// another so a run stays within its requests-in-flight setting. Usage is summed. When a
    /// chunk fails, the chunks before it are kept, so the engine's retry of the asset doesn't
    /// send them again while the API is asking for less traffic.
    async fn score(&self, request: &TagRequest) -> Result<Scores, ProviderError> {
        let requests = build_requests_within(request, self.budget);
        let mut total = Scores {
            scores: Vec::new(),
            model: MODEL.to_string(),
            usage: Usage::default(),
        };
        if requests.is_empty() {
            return Ok(total);
        }
        let mut key =
            HeaderValue::from_str(&format!("Bearer {}", self.api_key().await?)).map_err(|_| {
                ProviderError::new(
                    ErrorClass::Fatal,
                    "The TypeSafe API key has characters a header can't carry",
                )
            })?;
        key.set_sensitive(true);
        let keys: Vec<u64> = requests
            .iter()
            .map(|chunk| chunk_key(&request.run_id, chunk))
            .collect();
        for (chunk, chunk_key) in requests.iter().zip(&keys) {
            let stored = self.answered.lock().unwrap().get(chunk_key).cloned();
            let scores = match stored {
                Some(scores) => scores,
                None => {
                    let scores = self.send(&key, chunk).await?;
                    let mut answered = self.answered.lock().unwrap();
                    if answered.len() >= MAX_ANSWERED_CHUNKS {
                        answered.clear();
                    }
                    answered.insert(*chunk_key, scores.clone());
                    scores
                }
            };
            total.scores.extend(scores.scores);
            total.model = scores.model;
            total.usage.input_tokens += scores.usage.input_tokens;
            total.usage.output_tokens += scores.usage.output_tokens;
            total.usage.cost_usd += scores.usage.cost_usd;
        }
        let mut answered = self.answered.lock().unwrap();
        for chunk_key in &keys {
            answered.remove(chunk_key);
        }
        Ok(total)
    }

    /// Scores the test asset with the saved key, and names the tags the settings would keep.
    pub async fn test_connection(&self, threshold: f64, max_tags: u32) -> ConnectionTest {
        let request = test_request(threshold, max_tags);
        match self.score(&request).await {
            Ok(scores) => {
                let kept = select(&scores.scores, threshold, max_tags as usize);
                let tags: Vec<String> = kept
                    .iter()
                    .filter_map(|id| request.vocabulary.tags.iter().find(|t| &t.id == id))
                    .map(|t| t.path.clone())
                    .collect();
                let message = if tags.is_empty() {
                    format!(
                        "Connected to {}, but no test tag scored {threshold} or higher",
                        scores.model
                    )
                } else {
                    format!(
                        "Connected to {}. It tagged the test asset {}",
                        scores.model,
                        tags.join(", ")
                    )
                };
                ConnectionTest {
                    ok: true,
                    model: scores.model,
                    message,
                    tags,
                    input_tokens: scores.usage.input_tokens,
                    output_tokens: scores.usage.output_tokens,
                    cost_usd: scores.usage.cost_usd,
                }
            }
            Err(error) => ConnectionTest {
                ok: false,
                model: MODEL.to_string(),
                message: error.message,
                tags: Vec::new(),
                input_tokens: 0,
                output_tokens: 0,
                cost_usd: 0.0,
            },
        }
    }
}

impl TagProvider for JevProvider {
    fn id(&self) -> &'static str {
        "jev"
    }

    fn label(&self) -> &'static str {
        "Jev"
    }

    /// Jev can't write a tag name, only score the ones it is asked about (D1).
    fn modes(&self) -> &'static [Mode] {
        &[Mode::ExistingOnly]
    }

    fn concurrency(&self, settings: &AiSettings) -> u32 {
        settings.jev.concurrency
    }

    fn max_tags(&self, settings: &AiSettings) -> u32 {
        settings.jev.max_tags
    }

    fn threshold(&self, settings: &AiSettings) -> Option<f64> {
        Some(settings.jev.threshold)
    }

    fn sees_images(&self) -> bool {
        false
    }

    fn unavailable_reason(&self) -> Option<String> {
        if let Err(e) = &self.http {
            return Some(e.clone());
        }
        if !self
            .credentials
            .status(credentials::Provider::Jev)
            .configured
        {
            return Some("Set up Jev in Settings".into());
        }
        None
    }

    fn tag<'a>(&'a self, request: &'a TagRequest) -> TagFuture<'a> {
        Box::pin(async move {
            let scores = self.score(request).await?;
            let threshold = request.threshold.unwrap_or(DEFAULT_THRESHOLD);
            Ok(TagResult {
                existing: select(&scores.scores, threshold, request.max_tags as usize),
                proposed: Vec::new(),
                usage: scores.usage,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autotag::anthropic::mock;
    use crate::autotag::credentials::MemoryKeyring;
    use crate::autotag::provider::Thumbnail;
    use crate::db::TagRow;
    use std::time::Duration;

    /// The documented response shape (docs.typesafe.ai/api), with a score for each question of
    /// the test catalog: q0 Color, q1 Color > Blue, q2 Color > Red, q3 Material,
    /// q4 Material > Brick, q5 Material > Wood.
    const SCORES: &str = include_str!("fixtures/jev/scores.json");

    fn row(id: &str, name: &str, parent: Option<&str>, hint: &str) -> TagRow {
        (
            id.into(),
            name.into(),
            None,
            parent.map(Into::into),
            hint.into(),
        )
    }

    fn request_with(vocabulary: Vocabulary, image: bool) -> TagRequest {
        TagRequest {
            run_id: "run".into(),
            mode: Mode::ExistingOnly,
            model: None,
            max_tags: 8,
            max_proposals: 0,
            threshold: Some(0.7),
            asset: AssetInfo {
                file_name: "rock_large.glb".into(),
                relative_path: "models/rock_large.glb".into(),
                kind: "model".into(),
                extension: "glb".into(),
                metadata: json!({ "vertices": 1200 }),
            },
            image: image.then(|| Thumbnail {
                media_type: "image/png",
                data: vec![0x89, b'P', b'N', b'G', 1, 2, 3],
            }),
            vocabulary: Arc::new(vocabulary),
            run_proposals: Vec::new(),
        }
    }

    fn request(image: bool) -> TagRequest {
        request_with(
            Vocabulary::from_rows(&[
                row("nature", "Nature", None, ""),
                row("rock", "Rock", Some("nature"), "Boulders and\n stones"),
                row("tree", "Tree", Some("nature"), ""),
            ]),
            image,
        )
    }

    /// A catalog of `n` tags with hints.
    fn big_catalog(n: usize) -> Vocabulary {
        let rows: Vec<TagRow> = (0..n)
            .map(|i| {
                row(
                    &format!("tag-{i:05}"),
                    &format!("Tag number {i:05}"),
                    None,
                    "A hint long enough to make each question take a few dozen tokens",
                )
            })
            .collect();
        Vocabulary::from_rows(&rows)
    }

    /// The production requests for one asset: none for an empty catalog.
    fn build_requests(request: &TagRequest) -> Vec<JevRequest> {
        build_requests_within(request, REQUEST_TOKEN_BUDGET)
    }

    fn test_ids() -> JevRequest {
        build_requests(&test_request(0.7, 8)).remove(0)
    }

    #[test]
    fn one_noul_question_per_catalog_tag_with_its_path_and_hint() {
        let requests = build_requests(&request(false));

        assert_eq!(requests.len(), 1);
        let body = &requests[0].body;
        assert_eq!(body["model"], "jev-latest");
        assert_eq!(
            body["state"],
            json!({
                "file_name": "rock_large.glb",
                "path": "models/rock_large.glb",
                "kind": "model",
                "extension": "glb",
                "metadata": { "vertices": 1200 },
            })
        );
        assert_eq!(
            body["questions"],
            json!({
                "q0": {
                    "type": "noul",
                    "instructions": "Should the game-development asset in `state` be tagged \"Nature\"?",
                },
                "q1": {
                    "type": "noul",
                    "instructions": "Should the game-development asset in `state` be tagged \"Nature > Rock\"? The tag means: Boulders and stones",
                },
                "q2": {
                    "type": "noul",
                    "instructions": "Should the game-development asset in `state` be tagged \"Nature > Tree\"?",
                },
            })
        );
        assert_eq!(
            requests[0].questions,
            [
                ("q0".to_string(), "nature".to_string()),
                ("q1".to_string(), "rock".to_string()),
                ("q2".to_string(), "tree".to_string()),
            ]
        );
    }

    #[test]
    fn a_request_never_carries_an_image_even_when_the_asset_has_one() {
        let with_image = build_requests(&request(true));

        assert_eq!(with_image, build_requests(&request(false)));
        let body = with_image[0].body.to_string();
        assert!(!body.contains("image"), "{body}");
        assert!(!body.contains("base64"), "{body}");
        assert!(!body.contains("iVBOR"), "no PNG data: {body}");
    }

    #[test]
    fn an_empty_catalog_sends_nothing() {
        assert!(build_requests(&request_with(Vocabulary::default(), false)).is_empty());
    }

    #[test]
    fn a_large_catalog_is_split_into_requests_that_fit_the_budget() {
        let request = request_with(big_catalog(5_000), false);

        let requests = build_requests(&request);

        assert_eq!(requests.len(), 100, "5,000 questions in chunks of 50");
        let mut asked = Vec::new();
        for chunk in &requests {
            let questions = chunk.body["questions"].as_object().unwrap();
            assert_eq!(questions.len(), chunk.questions.len());
            assert_eq!(questions.len(), MAX_QUESTIONS_PER_REQUEST);
            let estimate = estimate_tokens(&chunk.body["state"].to_string())
                + questions
                    .values()
                    .map(|q| {
                        estimate_tokens(q["instructions"].as_str().unwrap())
                            + QUESTION_OVERHEAD_TOKENS
                    })
                    .sum::<usize>();
            assert!(estimate <= REQUEST_TOKEN_BUDGET, "{estimate}");
            // Within Jev's 64k context even counting all of the JSON at 3 bytes a token
            let bytes = chunk.body.to_string().len();
            assert!(bytes / 3 < 64_000, "{bytes}");
            assert_eq!(chunk.body["state"], requests[0].body["state"]);
            asked.extend(chunk.questions.iter().cloned());
        }
        let expected: Vec<(String, String)> = request
            .vocabulary
            .tags
            .iter()
            .enumerate()
            .map(|(i, t)| (format!("q{i}"), t.id.clone()))
            .collect();
        assert_eq!(asked, expected, "every tag asked once, ids q0..qN in order");
    }

    #[test]
    fn chunks_fill_up_to_the_budget_and_no_further() {
        let request = request_with(big_catalog(10), false);
        let state = estimate_tokens(&state(&request.asset).to_string());
        let per_question =
            estimate_tokens(&question(&request.vocabulary.tags[0])) + QUESTION_OVERHEAD_TOKENS;

        let sizes = |budget: usize| -> Vec<usize> {
            build_requests_within(&request, budget)
                .iter()
                .map(|r| r.questions.len())
                .collect()
        };

        assert_eq!(sizes(state + per_question * 4), [4, 4, 2]);
        assert_eq!(sizes(state + per_question * 4 - 1), [3, 3, 3, 1]);
        assert_eq!(sizes(usize::MAX), [10]);
        assert_eq!(sizes(0), vec![1; 10], "an oversized question goes alone");
    }

    #[test]
    fn a_request_holds_at_most_50_questions_however_much_room_is_left() {
        let sizes = |tags: usize| -> Vec<usize> {
            build_requests_within(&request_with(big_catalog(tags), false), usize::MAX)
                .iter()
                .map(|r| r.questions.len())
                .collect()
        };

        assert_eq!(sizes(49), [49]);
        assert_eq!(sizes(50), [50]);
        assert_eq!(sizes(51), [50, 1]);
        assert_eq!(sizes(120), [50, 50, 20]);
    }

    #[test]
    fn tags_at_or_above_the_threshold_are_kept_highest_first_up_to_the_cap() {
        let scores = parse_response(SCORES, &test_ids()).unwrap();

        // Brick 0.97, Red 0.93, Material 0.70 (at the threshold), Color 0.66, Wood 0.08, Blue 0.03
        assert_eq!(
            select(&scores.scores, 0.7, 8),
            ["test-material-brick", "test-color-red", "test-material"]
        );
        assert_eq!(
            select(&scores.scores, 0.7, 2),
            ["test-material-brick", "test-color-red"]
        );
        assert_eq!(
            select(&scores.scores, 0.65, 8),
            [
                "test-material-brick",
                "test-color-red",
                "test-material",
                "test-color"
            ]
        );
        assert!(select(&scores.scores, 0.99, 8).is_empty());
    }

    #[test]
    fn usage_is_priced_at_jevs_rate() {
        let scores = parse_response(SCORES, &test_ids()).unwrap();

        assert_eq!(scores.model, "jev-1.14.0");
        assert_eq!(scores.usage.input_tokens, 412);
        assert_eq!(scores.usage.output_tokens, 120);
        // $0.042 per million input tokens, output free
        assert!((scores.usage.cost_usd - 412.0 * 0.042 / 1e6).abs() < 1e-15);
    }

    #[test]
    fn a_response_missing_a_score_or_not_json_fails_the_item() {
        let missing = r#"{"model":"jev-1.14.0","answers":{"q0":{"type":"noul","noul":0.5}},
                          "usage":{"input_tokens":1,"output_tokens":1}}"#;
        let not_a_number = SCORES.replace("0.97", "\"high\"");

        for body in [missing, not_a_number.as_str(), "<html>"] {
            let error = parse_response(body, &test_ids()).unwrap_err();
            assert_eq!(error.class, ErrorClass::Item, "{body}");
        }
    }

    #[test]
    fn http_failures_map_to_the_plan_failure_classes_with_readable_messages() {
        let cases = [
            (
                401,
                r#"{"error":{"type":"unauthorized","message":"Invalid API key"}}"#,
                ErrorClass::Fatal,
                "TypeSafe rejected the Jev API key (401 Invalid API key)",
            ),
            (
                404,
                r#"{"detail":"model not found"}"#,
                ErrorClass::Fatal,
                "Unknown Jev model or endpoint (404 model not found)",
            ),
            (
                400,
                r#"{"error":"max_tokens_exceeded"}"#,
                ErrorClass::Item,
                "The asset is too long for Jev's context (400 max_tokens_exceeded)",
            ),
            (
                422,
                r#"{"detail":[{"loc":["questions"],"msg":"bad"}]}"#,
                ErrorClass::Item,
                r#"Jev API error 422 ([{"loc":["questions"],"msg":"bad"}])"#,
            ),
            (
                429,
                "",
                ErrorClass::Retryable,
                "Jev API error 429 (no error details)",
            ),
            (
                529,
                r#"{"message":"Overloaded"}"#,
                ErrorClass::Retryable,
                "Jev API error 529 (Overloaded)",
            ),
        ];
        for (status, body, class, message) in cases {
            let error = http_error(status, None, body);
            assert_eq!((error.class, error.message.as_str()), (class, message));
        }
    }

    fn provider(url: &str, key: Option<&'static str>) -> JevProvider {
        let credentials = Credentials::new(MemoryKeyring::default(), move |name| {
            (name == "TYPESAFE_API_KEY")
                .then(|| key.map(str::to_string))
                .flatten()
        });
        JevProvider::with_base_url(Arc::new(credentials), url.to_string())
    }

    #[tokio::test]
    async fn a_request_carries_the_bearer_key_and_the_built_body() {
        let api = mock::serve(200, &[], SCORES).await;
        let jev = provider(&api.url, Some("ts-test-key"));
        let mut request = test_request(0.7, 8);
        request.image = Some(Thumbnail {
            media_type: "image/png",
            data: vec![1, 2, 3],
        });

        let result = jev.tag(&request).await.unwrap();

        assert_eq!(
            result.existing,
            ["test-material-brick", "test-color-red", "test-material"]
        );
        assert!(result.proposed.is_empty());
        assert!(result.usage.cost_usd > 0.0);
        let received = api.received();
        assert_eq!(received.len(), 1);
        let head = &received[0].head;
        assert!(head.starts_with("post /v1/systemone "), "{head}");
        assert!(head.contains("authorization: bearer ts-test-key"), "{head}");
        let body: Value = serde_json::from_str(&received[0].body).unwrap();
        assert_eq!(body, build_requests(&request)[0].body);
        assert!(body.get("image").is_none() && !received[0].body.contains("AQID"));
    }

    #[tokio::test]
    async fn the_run_threshold_and_cap_decide_what_is_kept() {
        let api = mock::serve(200, &[], SCORES).await;
        let jev = provider(&api.url, Some("key"));

        let strict = jev.tag(&test_request(0.95, 8)).await.unwrap();
        let capped = jev.tag(&test_request(0.5, 1)).await.unwrap();

        assert_eq!(strict.existing, ["test-material-brick"]);
        assert_eq!(capped.existing, ["test-material-brick"]);
    }

    #[tokio::test]
    async fn a_rate_limit_is_retryable_and_passes_retry_after_along() {
        let api = mock::serve(429, &[("retry-after", "3")], r#"{"detail":"slow down"}"#).await;
        let jev = provider(&api.url, Some("key"));

        let error = jev.tag(&test_request(0.7, 8)).await.unwrap_err();

        assert_eq!(error.class, ErrorClass::Retryable);
        assert_eq!(error.retry_after, Some(Duration::from_secs(3)));
    }

    /// One answer per question of the test catalog, in its own response, with its own usage.
    fn one_answer(question: usize, noul: f64) -> String {
        format!(
            r#"{{"model":"jev-1.14.0","answers":{{"q{question}":{{"type":"noul","noul":{noul}}}}},
                "usage":{{"input_tokens":{},"output_tokens":20}}}}"#,
            100 + question
        )
    }

    #[tokio::test]
    async fn scores_and_usage_from_every_chunk_are_combined() {
        // The test catalog's scores, one question per request
        let nouls = [0.66, 0.03, 0.93, 0.70, 0.97, 0.08];
        let bodies: Vec<String> = nouls
            .iter()
            .enumerate()
            .map(|(q, noul)| one_answer(q, *noul))
            .collect();
        let replies: Vec<mock::Reply> = bodies.iter().map(|b| (200, &[][..], b.as_str())).collect();
        let api = mock::serve_sequence(&replies).await;
        let mut jev = provider(&api.url, Some("key"));
        jev.budget = 0;

        let result = jev.tag(&test_request(0.7, 8)).await.unwrap();

        let received = api.received();
        assert_eq!(received.len(), 6, "one request per question");
        for (q, request) in received.iter().enumerate() {
            let body: Value = serde_json::from_str(&request.body).unwrap();
            let questions: Vec<&String> = body["questions"].as_object().unwrap().keys().collect();
            assert_eq!(questions, [&format!("q{q}")]);
        }
        assert_eq!(
            result.existing,
            ["test-material-brick", "test-color-red", "test-material"],
            "highest first across chunks 5, 3 and 4"
        );
        let input: i64 = (100..106).sum();
        assert_eq!(result.usage.input_tokens, input);
        assert_eq!(result.usage.output_tokens, 6 * 20);
        assert!((result.usage.cost_usd - input as f64 * 0.042 / 1e6).abs() < 1e-15);
    }

    #[tokio::test]
    async fn a_retry_sends_only_the_chunks_still_unanswered_and_counts_usage_once() {
        let nouls = [0.66, 0.03, 0.93, 0.70, 0.97, 0.08];
        let bodies: Vec<String> = nouls
            .iter()
            .enumerate()
            .map(|(q, noul)| one_answer(q, *noul))
            .collect();
        let rate_limited: &[(&str, &str)] = &[("retry-after", "1")];
        let mut replies: Vec<mock::Reply> = vec![
            (200, &[], bodies[0].as_str()),
            (200, &[], bodies[1].as_str()),
            (429, rate_limited, r#"{"detail":"slow down"}"#),
        ];
        replies.extend(bodies[2..].iter().map(|b| (200, &[][..], b.as_str())));
        let api = mock::serve_sequence(&replies).await;
        let mut jev = provider(&api.url, Some("key"));
        jev.budget = 0;
        let request = test_request(0.7, 8);

        let error = jev.tag(&request).await.unwrap_err();
        let retried = jev.tag(&request).await.unwrap();

        assert_eq!(error.class, ErrorClass::Retryable);
        let sent: Vec<String> = api
            .received()
            .iter()
            .map(|r| {
                let body: Value = serde_json::from_str(&r.body).unwrap();
                body["questions"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .next()
                    .unwrap()
                    .clone()
            })
            .collect();
        assert_eq!(
            sent,
            ["q0", "q1", "q2", "q2", "q3", "q4", "q5"],
            "the retry starts at the chunk that failed"
        );
        assert_eq!(
            retried.existing,
            ["test-material-brick", "test-color-red", "test-material"]
        );
        let input: i64 = (100..106).sum();
        assert_eq!(
            retried.usage.input_tokens, input,
            "every chunk counted once"
        );
        assert!((retried.usage.cost_usd - input as f64 * 0.042 / 1e6).abs() < 1e-15);
        assert!(
            jev.answered.lock().unwrap().is_empty(),
            "a finished asset keeps nothing"
        );
    }

    #[tokio::test]
    async fn another_run_or_a_changed_request_asks_again() {
        let api = mock::serve(200, &[], SCORES).await;
        let jev = provider(&api.url, Some("key"));
        let request = test_request(0.7, 8);
        let chunk = build_requests(&request).remove(0);
        jev.answered.lock().unwrap().insert(
            chunk_key("an-earlier-run", &chunk),
            parse_response(SCORES, &chunk).unwrap(),
        );

        jev.tag(&request).await.unwrap();

        assert_eq!(api.received().len(), 1, "stored for another run, so sent");
        let mut changed = test_request(0.7, 8);
        changed.asset.file_name = "blue_planks.png".into();
        assert_ne!(
            chunk_key("run", &build_requests(&changed)[0]),
            chunk_key("run", &build_requests(&request)[0])
        );
    }

    #[tokio::test]
    async fn a_failing_chunk_fails_the_asset_with_its_class_and_stops() {
        let first = one_answer(0, 0.9);
        let api = mock::serve_sequence(&[
            (200, &[], first.as_str()),
            (401, &[], r#"{"detail":"Invalid API key"}"#),
            (200, &[], first.as_str()),
        ])
        .await;
        let mut jev = provider(&api.url, Some("key"));
        jev.budget = 0;

        let error = jev.tag(&test_request(0.7, 8)).await.unwrap_err();

        assert_eq!(error.class, ErrorClass::Fatal);
        assert_eq!(
            error.message,
            "TypeSafe rejected the Jev API key (401 Invalid API key)"
        );
        assert_eq!(api.received().len(), 2, "no request after the failure");
    }

    #[tokio::test]
    async fn an_unreachable_api_is_retryable() {
        let jev = provider("http://127.0.0.1:1", Some("key"));

        let error = jev.tag(&test_request(0.7, 8)).await.unwrap_err();

        assert_eq!(error.class, ErrorClass::Retryable, "{}", error.message);
        assert!(
            error.message.starts_with("Could not reach the Jev API: "),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn with_no_key_nothing_is_sent() {
        let api = mock::serve(200, &[], SCORES).await;
        let jev = provider(&api.url, None);

        assert_eq!(
            jev.unavailable_reason().as_deref(),
            Some("Set up Jev in Settings")
        );
        let error = jev.tag(&test_request(0.7, 8)).await.unwrap_err();
        assert_eq!(error.class, ErrorClass::Fatal);
        let test = jev.test_connection(0.7, 8).await;
        assert!(!test.ok);
        assert_eq!(test.message, "No TypeSafe API key is set");
        assert!(api.received().is_empty());
    }

    #[tokio::test]
    async fn test_connection_names_the_tags_the_settings_keep() {
        let api = mock::serve(200, &[], SCORES).await;
        let jev = provider(&api.url, Some("key"));

        let test = jev.test_connection(0.7, 8).await;

        assert!(test.ok, "{test:?}");
        assert_eq!(test.model, "jev-1.14.0");
        assert_eq!(
            test.message,
            "Connected to jev-1.14.0. It tagged the test asset Material > Brick, Color > Red, Material"
        );
        assert!(test.input_tokens > 0 && test.cost_usd > 0.0);
        let none = jev.test_connection(0.99, 8).await;
        assert_eq!(
            none.message,
            "Connected to jev-1.14.0, but no test tag scored 0.99 or higher"
        );
    }

    #[test]
    fn jev_supports_only_existing_only_and_never_sees_images() {
        let jev = provider("http://127.0.0.1:1", None);
        assert_eq!(jev.modes(), [Mode::ExistingOnly]);
        assert!(!jev.sees_images());
    }

    /// The threshold saved in Settings when a run starts reaches the request through the run's
    /// options, and the run's cost is priced at Jev's rate.
    #[tokio::test]
    async fn a_run_keeps_the_tags_its_settings_allow() {
        use crate::autotag::engine::{Engine, RunScope};
        use crate::autotag::settings;
        use crate::db::{
            insert_asset, insert_tag, memory_database, memory_library, memory_library_pool,
        };

        let lib_pool = memory_library_pool().await;
        insert_asset(&lib_pool, "a1").await;
        let db = Arc::new(memory_database(vec![memory_library("lib", lib_pool.clone())]).await);
        for (id, name, parent) in [
            ("test-color", "Color", None),
            ("test-color-blue", "Blue", Some("test-color")),
            ("test-color-red", "Red", Some("test-color")),
            ("test-material", "Material", None),
            ("test-material-brick", "Brick", Some("test-material")),
            ("test-material-wood", "Wood", Some("test-material")),
        ] {
            insert_tag(&db.master_pool, id, name).await;
            sqlx::query("UPDATE tags SET parent_id = ? WHERE id = ?")
                .bind(parent)
                .bind(id)
                .execute(&db.master_pool)
                .await
                .unwrap();
        }
        settings::update(
            &db.master_pool,
            json!({"jev": {"threshold": 0.95, "maxTags": 8}}),
        )
        .await
        .unwrap();
        let api = mock::serve(200, &[], SCORES).await;
        let engine = Engine::new(
            db.clone(),
            vec![Arc::new(provider(&api.url, Some("key")))],
            Arc::new(|_| {}),
            Arc::new(|_| {}),
        );
        engine.start().await;

        engine
            .start_run(
                RunScope::Assets {
                    asset_ids: vec!["a1".into()],
                },
                "jev",
                Mode::ExistingOnly,
            )
            .await
            .unwrap();
        let mut runs = Vec::new();
        for _ in 0..500 {
            runs = engine.list_runs().await.unwrap();
            if runs[0].status == "done" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(runs[0].status, "done");
        assert!((runs[0].cost_usd - 412.0 * 0.042 / 1e6).abs() < 1e-12);
        let tags: Vec<String> =
            sqlx::query_scalar("SELECT tag_id FROM asset_tags WHERE asset_id = 'a1'")
                .fetch_all(&lib_pool)
                .await
                .unwrap();
        assert_eq!(
            tags,
            ["test-material-brick"],
            "0.95 keeps Brick alone, where the 0.7 default would add Red and Material"
        );
        let body: Value = serde_json::from_str(&api.received()[0].body).unwrap();
        assert_eq!(body["questions"].as_object().unwrap().len(), 6);
    }
}
