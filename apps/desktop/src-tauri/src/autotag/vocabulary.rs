//! The tag list a provider picks from, and the checks on what it picks.

use sqlx::{Pool, Sqlite};
use std::collections::{HashMap, HashSet};

use super::provider::TagResult;
use crate::db::TagRow;

/// A catalog tag as a provider sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct VocabularyTag {
    pub id: String,
    /// The full path, root first: "Nature > Rock".
    pub path: String,
    /// The tag's description, empty when it has none.
    pub hint: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Vocabulary {
    /// Every catalog tag, ordered by path.
    pub tags: Vec<VocabularyTag>,
}

/// Lowercased and trimmed, with inner whitespace collapsed.
pub fn normalize_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

impl Vocabulary {
    /// Every tag in the master catalog.
    pub async fn load(master: &Pool<Sqlite>) -> sqlx::Result<Self> {
        let rows: Vec<TagRow> =
            sqlx::query_as("SELECT id, name, color, parent_id, description FROM tags")
                .fetch_all(master)
                .await?;
        Ok(Self::from_rows(&rows))
    }

    pub fn from_rows(rows: &[TagRow]) -> Self {
        let by_id: HashMap<&str, &TagRow> = rows.iter().map(|r| (r.0.as_str(), r)).collect();
        let mut tags: Vec<VocabularyTag> = rows
            .iter()
            .map(|row| {
                let mut names = vec![row.1.as_str()];
                let mut seen = HashSet::from([row.0.as_str()]);
                let mut parent = row.3.as_deref();
                // A parent cycle or a missing parent ends the path.
                while let Some(p) = parent.and_then(|id| by_id.get(id)) {
                    if !seen.insert(p.0.as_str()) {
                        break;
                    }
                    names.push(p.1.as_str());
                    parent = p.3.as_deref();
                }
                names.reverse();
                VocabularyTag {
                    id: row.0.clone(),
                    path: names.join(" > "),
                    hint: row.4.clone(),
                }
            })
            .collect();
        tags.sort_by(|a, b| a.path.cmp(&b.path));
        Self { tags }
    }

    /// The catalog tag ids to apply from a provider's result: unknown ids dropped, proposals
    /// that name a catalog tag counted as that tag, no repeats, at most `max_tags`.
    pub fn accepted_tags(&self, result: &TagResult, max_tags: usize) -> Vec<String> {
        let ids: HashSet<&str> = self.tags.iter().map(|t| t.id.as_str()).collect();
        let by_name: HashMap<String, &str> = self
            .tags
            .iter()
            .map(|t| {
                let leaf = t.path.rsplit(" > ").next().unwrap_or(&t.path);
                (normalize_name(leaf), t.id.as_str())
            })
            .collect();

        let named = result
            .proposed
            .iter()
            .filter_map(|p| by_name.get(&normalize_name(&p.name)).copied());
        let mut seen = HashSet::new();
        result
            .existing
            .iter()
            .map(String::as_str)
            .filter(|id| ids.contains(id))
            .chain(named)
            .filter(|id| seen.insert(*id))
            .take(max_tags)
            .map(str::to_string)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autotag::provider::ProposedTag;

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
            row("rock", "Rock", Some("nature"), "Boulders and stones"),
            row("nature", "Nature", None, ""),
            row("pebble", "Pebble", Some("rock"), ""),
            row("tree", "Tree", Some("nature"), "Any tree"),
        ])
    }

    #[test]
    fn every_catalog_tag_is_listed_as_a_full_path_with_its_hint_and_id() {
        let vocabulary = vocabulary();
        let paths: Vec<(&str, &str, &str)> = vocabulary
            .tags
            .iter()
            .map(|t| (t.id.as_str(), t.path.as_str(), t.hint.as_str()))
            .collect();

        assert_eq!(
            paths,
            [
                ("nature", "Nature", ""),
                ("rock", "Nature > Rock", "Boulders and stones"),
                ("pebble", "Nature > Rock > Pebble", ""),
                ("tree", "Nature > Tree", "Any tree"),
            ]
        );
    }

    #[test]
    fn a_parent_cycle_ends_the_path() {
        let vocabulary =
            Vocabulary::from_rows(&[row("a", "A", Some("b"), ""), row("b", "B", Some("a"), "")]);
        let paths: Vec<(&str, &str)> = vocabulary
            .tags
            .iter()
            .map(|t| (t.id.as_str(), t.path.as_str()))
            .collect();
        assert_eq!(paths, [("b", "A > B"), ("a", "B > A")]);
    }

    #[test]
    fn unknown_ids_are_dropped_and_the_cap_applies() {
        let result = TagResult {
            existing: ["ghost", "tree", "rock", "tree", "pebble"]
                .map(String::from)
                .to_vec(),
            ..Default::default()
        };

        assert_eq!(vocabulary().accepted_tags(&result, 2), ["tree", "rock"]);
        assert_eq!(
            vocabulary().accepted_tags(&result, 8),
            ["tree", "rock", "pebble"]
        );
    }

    #[test]
    fn a_proposal_naming_a_catalog_tag_counts_as_that_tag() {
        let result = TagResult {
            existing: vec!["rock".into()],
            proposed: vec![
                ProposedTag {
                    name: "  TREE ".into(),
                    parent_id: None,
                },
                ProposedTag {
                    name: "rock".into(),
                    parent_id: None,
                },
                ProposedTag {
                    name: "Mushroom".into(),
                    parent_id: None,
                },
            ],
            ..Default::default()
        };

        assert_eq!(vocabulary().accepted_tags(&result, 8), ["rock", "tree"]);
    }

    #[test]
    fn names_are_normalized_by_case_and_spacing() {
        assert_eq!(normalize_name("  Big   Rock\t"), "big rock");
    }
}
