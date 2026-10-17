//! Debug builds only: providers that tag from file-name words, so every flow can be run and
//! screenshotted with no key and no spend (`OPENDAM_AUTOTAG_FAKE=1`).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use super::provider::{Mode, ProposedTag, TagFuture, TagProvider, TagRequest, TagResult};
use super::settings::AiSettings;
use super::vocabulary::normalize_name;

const DEFAULT_DELAY_MS: u64 = 300;

/// Fake (all three modes) and Fake text-only (Existing only, like Jev), when
/// `OPENDAM_AUTOTAG_FAKE=1`. `OPENDAM_AUTOTAG_FAKE_DELAY_MS` (default 300) slows each call.
pub fn from_env() -> Vec<FakeProvider> {
    if std::env::var("OPENDAM_AUTOTAG_FAKE").as_deref() != Ok("1") {
        return Vec::new();
    }
    let delay_ms = std::env::var("OPENDAM_AUTOTAG_FAKE_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_DELAY_MS);
    tracing::info!("OPENDAM_AUTOTAG_FAKE=1: adding the fake tagging providers ({delay_ms} ms)");
    let delay = Duration::from_millis(delay_ms);
    vec![
        FakeProvider {
            text_only: false,
            delay,
        },
        FakeProvider {
            text_only: true,
            delay,
        },
    ]
}

pub struct FakeProvider {
    text_only: bool,
    delay: Duration,
}

/// The lowercase words of a file name without its extension: "Rock_large-02.glb" gives
/// rock, large and 02.
fn file_name_words(file_name: &str) -> Vec<String> {
    let stem = file_name
        .rsplit_once('.')
        .map_or(file_name, |(stem, _)| stem);
    stem.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// "mossy" gives "Mossy".
fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

impl FakeProvider {
    #[cfg(test)]
    pub fn new(text_only: bool, delay: Duration) -> Self {
        Self { text_only, delay }
    }

    fn result(&self, request: &TagRequest) -> TagResult {
        let words = file_name_words(&request.asset.file_name);
        let mut existing = Vec::new();
        let mut known = HashSet::new();
        for tag in &request.vocabulary.tags {
            let leaf = normalize_name(tag.path.rsplit(" > ").next().unwrap_or(&tag.path));
            known.insert(leaf.clone());
            if words.contains(&leaf) {
                existing.push(tag.id.clone());
            }
        }
        // A word this run already proposed comes back spelled as it was first proposed.
        let earlier: HashMap<String, &String> = request
            .run_proposals
            .iter()
            .map(|p| (normalize_name(&p.name), &p.name))
            .collect();
        let proposed = if request.mode == Mode::ExistingOnly {
            Vec::new()
        } else {
            words
                .iter()
                .filter(|w| w.len() >= 3 && w.chars().all(char::is_alphabetic))
                .filter(|w| !known.contains(*w))
                .map(|w| ProposedTag {
                    name: earlier
                        .get(w)
                        .map_or_else(|| capitalize(w), |n| n.to_string()),
                    parent_id: None,
                })
                .collect()
        };
        TagResult {
            existing,
            proposed,
            ..Default::default()
        }
    }
}

impl TagProvider for FakeProvider {
    fn id(&self) -> &'static str {
        if self.text_only {
            "fake-text"
        } else {
            "fake"
        }
    }

    fn label(&self) -> &'static str {
        if self.text_only {
            "Fake text-only"
        } else {
            "Fake"
        }
    }

    fn modes(&self) -> &'static [Mode] {
        if self.text_only {
            &[Mode::ExistingOnly]
        } else {
            &Mode::ALL
        }
    }

    /// Fake shares Anthropic's settings and Fake text-only Jev's, like the providers they stand in for.
    fn concurrency(&self, settings: &AiSettings) -> u32 {
        if self.text_only {
            settings.jev.concurrency
        } else {
            settings.anthropic.concurrency
        }
    }

    fn max_tags(&self, settings: &AiSettings) -> u32 {
        if self.text_only {
            settings.jev.max_tags
        } else {
            settings.anthropic.max_tags
        }
    }

    fn max_proposals(&self, settings: &AiSettings) -> u32 {
        if self.text_only {
            0
        } else {
            settings.anthropic.max_proposals
        }
    }

    fn sees_images(&self) -> bool {
        !self.text_only
    }

    fn unavailable_reason(&self) -> Option<String> {
        None
    }

    fn tag<'a>(&'a self, request: &'a TagRequest) -> TagFuture<'a> {
        Box::pin(async move {
            tokio::time::sleep(self.delay).await;
            Ok(self.result(request))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autotag::provider::AssetInfo;
    use crate::autotag::vocabulary::Vocabulary;
    use std::sync::Arc;

    fn request(file_name: &str, mode: Mode) -> TagRequest {
        request_in_run(file_name, mode, &[])
    }

    fn request_in_run(file_name: &str, mode: Mode, run_proposals: &[&str]) -> TagRequest {
        TagRequest {
            run_id: "run".into(),
            mode,
            model: None,
            max_tags: 8,
            max_proposals: 3,
            threshold: None,
            asset: AssetInfo {
                file_name: file_name.into(),
                relative_path: file_name.into(),
                kind: "model".into(),
                extension: "glb".into(),
                metadata: serde_json::json!({}),
            },
            image: None,
            vocabulary: Arc::new(Vocabulary::from_rows(&[
                ("t-rock".into(), "Rock".into(), None, None, String::new()),
                ("t-tree".into(), "Tree".into(), None, None, String::new()),
            ])),
            run_proposals: run_proposals
                .iter()
                .map(|name| ProposedTag {
                    name: name.to_string(),
                    parent_id: None,
                })
                .collect(),
        }
    }

    fn fake(text_only: bool) -> FakeProvider {
        FakeProvider {
            text_only,
            delay: Duration::ZERO,
        }
    }

    #[tokio::test]
    async fn tags_from_file_name_words() {
        let result = fake(false)
            .tag(&request("Rock_mossy-02.glb", Mode::ExistingOnly))
            .await
            .unwrap();

        assert_eq!(result.existing, ["t-rock"]);
        assert!(result.proposed.is_empty());
    }

    #[tokio::test]
    async fn proposes_words_not_in_the_catalog_outside_existing_only() {
        let result = fake(false)
            .tag(&request("Rock_mossy-02.glb", Mode::PreferExisting))
            .await
            .unwrap();

        let names: Vec<&str> = result.proposed.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Mossy"]);
    }

    #[tokio::test]
    async fn a_word_the_run_already_proposed_keeps_its_first_spelling() {
        let result = fake(false)
            .tag(&request_in_run(
                "mossy_fern.glb",
                Mode::PreferExisting,
                &["MOSSY"],
            ))
            .await
            .unwrap();

        let names: Vec<&str> = result.proposed.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["MOSSY", "Fern"]);
    }

    #[test]
    fn fake_text_only_supports_only_existing_only() {
        assert_eq!(fake(true).modes(), [Mode::ExistingOnly]);
        assert_eq!(fake(true).id(), "fake-text");
        assert_eq!(fake(false).modes(), Mode::ALL);
    }
}
