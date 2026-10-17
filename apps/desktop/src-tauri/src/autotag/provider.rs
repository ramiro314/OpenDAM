//! The `TagProvider` trait every tagging provider implements, and its request and result types.

use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use super::credentials::{self, Credentials};
use super::settings::AiSettings;
use super::vocabulary::Vocabulary;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    ExistingOnly,
    PreferExisting,
    CreateFreely,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::ExistingOnly, Mode::PreferExisting, Mode::CreateFreely];

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::ExistingOnly => "existing_only",
            Mode::PreferExisting => "prefer_existing",
            Mode::CreateFreely => "create_freely",
        }
    }
}

/// What a provider learns about one asset. Paths are relative to the library root.
#[derive(Debug, Clone)]
pub struct AssetInfo {
    pub file_name: String,
    pub relative_path: String,
    pub kind: String,
    pub extension: String,
    pub metadata: serde_json::Value,
}

/// The asset's thumbnail, downscaled to at most 512 px on its long edge.
#[derive(Debug, Clone)]
pub struct Thumbnail {
    pub media_type: &'static str,
    pub data: Vec<u8>,
}

/// One asset per request.
#[derive(Debug, Clone)]
pub struct TagRequest {
    pub run_id: String,
    pub mode: Mode,
    /// The run's model, for providers that have one.
    pub model: Option<String>,
    /// The most catalog tags to pick; the engine also enforces it.
    pub max_tags: u32,
    /// The most new tag names to propose, outside Existing only.
    pub max_proposals: u32,
    pub asset: AssetInfo,
    /// None when the asset has no thumbnail yet: the request is text only.
    pub image: Option<Thumbnail>,
    pub vocabulary: Arc<Vocabulary>,
    /// This run's own vocabulary (D26): names it already proposed or created, which later
    /// assets should reuse rather than near-duplicate.
    pub run_proposals: Vec<ProposedTag>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposedTag {
    pub name: String,
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
}

/// What every provider returns: catalog tag ids, and new tag names (never in Existing only).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TagResult {
    pub existing: Vec<String>,
    pub proposed: Vec<ProposedTag>,
    pub usage: Usage,
}

/// How a failed call is handled (the plan's Failures).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// Worth another try later: rate limits, overload, server errors, timeouts. Retried after
    /// a backoff, then the item fails.
    Retryable,
    /// Every item of the run would fail the same way: bad key, unknown model. Pauses the run.
    Fatal,
    /// Only this asset: a refusal, output that doesn't parse, the asset gone. Fails the item.
    Item,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderError {
    pub class: ErrorClass,
    pub message: String,
    /// How long the provider asked us to wait before trying again (HTTP `retry-after`).
    pub retry_after: Option<Duration>,
}

impl ProviderError {
    pub fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            retry_after: None,
        }
    }

    pub fn with_retry_after(mut self, wait: Duration) -> Self {
        self.retry_after = Some(wait);
        self
    }
}

pub type TagFuture<'a> =
    Pin<Box<dyn Future<Output = Result<TagResult, ProviderError>> + Send + 'a>>;

pub trait TagProvider: Send + Sync {
    /// Stored in `tag_runs.provider`.
    fn id(&self) -> &'static str;
    fn label(&self) -> &'static str;
    /// The modes the provider can do; the run dialog disables the rest.
    fn modes(&self) -> &'static [Mode];
    /// Requests in flight at once, shared by every run of this provider.
    fn concurrency(&self, settings: &AiSettings) -> u32;
    /// The most catalog tags kept per asset.
    fn max_tags(&self, settings: &AiSettings) -> u32;
    /// The most new tag names proposed per asset, for providers that propose any.
    fn max_proposals(&self, _settings: &AiSettings) -> u32 {
        0
    }
    fn model(&self, _settings: &AiSettings) -> Option<String> {
        None
    }
    /// Why no run can start, when none can. May block on the keyring: call it off async workers.
    fn unavailable_reason(&self) -> Option<String>;
    fn tag<'a>(&'a self, request: &'a TagRequest) -> TagFuture<'a>;
}

/// A provider whose requests come in a later ticket. It shows in the run dialog, with why it
/// can't be picked yet.
pub struct NotYetBuilt {
    pub provider: credentials::Provider,
    pub credentials: Arc<Credentials>,
}

impl TagProvider for NotYetBuilt {
    fn id(&self) -> &'static str {
        match self.provider {
            credentials::Provider::Anthropic => "anthropic",
            credentials::Provider::Jev => "jev",
        }
    }

    fn label(&self) -> &'static str {
        match self.provider {
            credentials::Provider::Anthropic => "Anthropic",
            credentials::Provider::Jev => "Jev",
        }
    }

    fn modes(&self) -> &'static [Mode] {
        match self.provider {
            credentials::Provider::Anthropic => &Mode::ALL,
            credentials::Provider::Jev => &[Mode::ExistingOnly],
        }
    }

    fn concurrency(&self, settings: &AiSettings) -> u32 {
        match self.provider {
            credentials::Provider::Anthropic => settings.anthropic.concurrency,
            credentials::Provider::Jev => settings.jev.concurrency,
        }
    }

    fn max_tags(&self, settings: &AiSettings) -> u32 {
        match self.provider {
            credentials::Provider::Anthropic => settings.anthropic.max_tags,
            credentials::Provider::Jev => settings.jev.max_tags,
        }
    }

    fn max_proposals(&self, settings: &AiSettings) -> u32 {
        match self.provider {
            credentials::Provider::Anthropic => settings.anthropic.max_proposals,
            credentials::Provider::Jev => 0,
        }
    }

    fn unavailable_reason(&self) -> Option<String> {
        if !self.credentials.status(self.provider).configured {
            return Some(format!("Set up {} in Settings", self.label()));
        }
        Some(format!("{} tagging isn't built yet", self.label()))
    }

    fn tag<'a>(&'a self, _request: &'a TagRequest) -> TagFuture<'a> {
        let label = self.label();
        Box::pin(async move {
            Err(ProviderError::new(
                ErrorClass::Fatal,
                format!("{label} tagging isn't built yet"),
            ))
        })
    }
}
