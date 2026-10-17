//! The review of a Prefer existing run's new tag names: what the review modal shows, and
//! applying its decisions through the write protocol (docs/plans/ai-auto-tagging/plan.md,
//! "Write protocol > Review apply").

use serde::{Deserialize, Serialize};
use sqlx::{Pool, Sqlite};
use std::collections::{BTreeMap, HashMap, HashSet};

use super::store::{self, Applier, PendingPair, BATCH};
use super::vocabulary::{clean_name, normalize_name};
use crate::db::{resolve_path, upsert_tag, Database, LibraryHandle};

/// An asset a proposal is for, as the review shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewAsset {
    pub asset_id: String,
    /// None when its library is offline or the asset is gone.
    pub file_name: Option<String>,
    /// Absolute, for the webview.
    pub thumbnail_path: Option<String>,
    pub included: bool,
}

/// A new tag name waiting for review.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewProposal {
    pub id: String,
    pub name: String,
    /// The catalog tag the model suggested as its parent, if it still exists.
    pub parent_id: Option<String>,
    pub assets: Vec<ReviewAsset>,
}

/// What the review decided for one proposal. `asset_ids` are the ticked assets.
#[derive(Debug, Clone, Deserialize)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ProposalDecision {
    /// Create the tag, named and placed as reviewed. A name a catalog tag already has maps onto
    /// that tag instead.
    Accept {
        proposal_id: String,
        name: String,
        parent_id: Option<String>,
        asset_ids: Vec<String>,
    },
    /// Apply an existing catalog tag instead.
    Map {
        proposal_id: String,
        tag_id: String,
        asset_ids: Vec<String>,
    },
    Reject {
        proposal_id: String,
    },
}

impl ProposalDecision {
    fn proposal_id(&self) -> &str {
        match self {
            Self::Accept { proposal_id, .. }
            | Self::Map { proposal_id, .. }
            | Self::Reject { proposal_id } => proposal_id,
        }
    }
}

/// The run's pending proposal rows, one per asset, in the order they were collected.
async fn pending_rows(
    master: &Pool<Sqlite>,
    run_id: &str,
) -> sqlx::Result<Vec<(String, String, Option<String>, String, bool, Option<String>)>> {
    sqlx::query_as(
        "SELECT p.id, p.name, p.parent_id, pa.asset_id, pa.included, i.library_id
         FROM tag_run_proposals p
         JOIN tag_run_proposal_assets pa ON pa.proposal_id = p.id
         LEFT JOIN tag_run_items i ON i.run_id = p.run_id AND i.asset_id = pa.asset_id
         WHERE p.run_id = ? AND p.status = 'pending'
         ORDER BY p.rowid, pa.rowid",
    )
    .bind(run_id)
    .fetch_all(master)
    .await
}

/// The file name and absolute thumbnail path of `asset_ids` in one library.
async fn asset_previews(
    lib: &LibraryHandle,
    asset_ids: &[String],
) -> sqlx::Result<HashMap<String, (String, Option<String>)>> {
    let mut previews = HashMap::new();
    for chunk in asset_ids.chunks(BATCH) {
        let sql = format!(
            "SELECT id, file_name, thumbnail_path FROM assets WHERE id IN ({})",
            vec!["?"; chunk.len()].join(", ")
        );
        let mut query = sqlx::query_as::<_, (String, String, Option<String>)>(&sql);
        for id in chunk {
            query = query.bind(id);
        }
        for (id, file_name, thumb) in query.fetch_all(&lib.pool).await? {
            let thumb = thumb.map(|t| resolve_path(&lib.root_path, &t));
            previews.insert(id, (file_name, thumb));
        }
    }
    Ok(previews)
}

/// The run's proposals still waiting for review, each with its assets.
pub async fn load(db: &Database, run_id: &str) -> sqlx::Result<Vec<ReviewProposal>> {
    let rows = pending_rows(&db.master_pool, run_id).await?;
    let catalog: HashSet<String> = sqlx::query_scalar("SELECT id FROM tags")
        .fetch_all(&db.master_pool)
        .await?
        .into_iter()
        .collect();

    let mut by_library: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in &rows {
        if let Some(library_id) = &row.5 {
            by_library
                .entry(library_id.clone())
                .or_default()
                .push(row.3.clone());
        }
    }
    let mut previews = HashMap::new();
    for (library_id, asset_ids) in by_library {
        if let Some(lib) = db.get_library_handle(&library_id).await {
            previews.extend(asset_previews(&lib, &asset_ids).await?);
        }
    }

    let mut proposals: Vec<ReviewProposal> = Vec::new();
    for (id, name, parent_id, asset_id, included, _) in rows {
        if proposals.last().is_none_or(|p| p.id != id) {
            proposals.push(ReviewProposal {
                id,
                name,
                parent_id: parent_id.filter(|p| catalog.contains(p)),
                assets: Vec::new(),
            });
        }
        let (file_name, thumbnail_path) = match previews.get(&asset_id) {
            Some((name, thumb)) => (Some(name.clone()), thumb.clone()),
            None => (None, None),
        };
        proposals.last_mut().unwrap().assets.push(ReviewAsset {
            asset_id,
            file_name,
            thumbnail_path,
            included,
        });
    }
    Ok(proposals)
}

/// What one decision resolves to.
struct Resolved {
    proposal_id: String,
    /// `accepted`, `mapped` or `rejected`.
    status: &'static str,
    /// The tag to apply: an existing catalog tag, or one this review creates.
    tag_id: Option<String>,
    created_tag: bool,
    ticked: Vec<String>,
}

/// What a review applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    /// The run's status afterwards: `done` once nothing is left to review, or as it was.
    pub status: String,
    /// Assets whose tags changed in an open library.
    pub changed_assets: Vec<String>,
}

/// Applies the review of a run that is `awaiting_review`, or `cancelled` with proposals left,
/// through the write protocol:
///
/// 1. One master transaction creates the accepted tags, resolves the proposals, records which
///    assets stayed ticked and inserts `pending` rows for the ticked assets' new pairs. An
///    `awaiting_review` run with nothing left to review becomes `done`; a cancelled run stays
///    `cancelled`.
/// 2. The created tags are mirrored to every open library.
/// 3. and 4. The pairs are written to their open libraries and confirmed. An offline library's
///    pairs wait for replay.
///
/// Proposals without a decision stay pending.
pub async fn apply(
    db: &Database,
    run_id: &str,
    decisions: Vec<ProposalDecision>,
) -> Result<Applied, String> {
    let master = &db.master_pool;
    let (status, in_flight): (String, i64) = sqlx::query_as(
        "SELECT status, (SELECT COUNT(*) FROM tag_run_items
                         WHERE run_id = tag_runs.id AND status = 'running')
         FROM tag_runs WHERE id = ?",
    )
    .bind(run_id)
    .fetch_optional(master)
    .await
    .map_err(|e| e.to_string())?
    .ok_or("The run no longer exists")?;
    if status != "awaiting_review" && status != "cancelled" {
        return Err("Only a run waiting for review, or a cancelled one, can be reviewed".into());
    }
    if in_flight > 0 {
        return Err("Wait for the requests still in flight to finish".into());
    }

    // The proposals, their assets, and each asset's library.
    let mut assets_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut library_of: HashMap<String, String> = HashMap::new();
    for (id, _, _, asset_id, _, library_id) in pending_rows(master, run_id)
        .await
        .map_err(|e| e.to_string())?
    {
        if let Some(library_id) = library_id {
            library_of.insert(asset_id.clone(), library_id);
        }
        assets_of.entry(id).or_default().push(asset_id);
    }
    let catalog: Vec<(String, String)> = sqlx::query_as("SELECT id, name FROM tags")
        .fetch_all(master)
        .await
        .map_err(|e| e.to_string())?;
    let catalog_ids: HashSet<&str> = catalog.iter().map(|(id, _)| id.as_str()).collect();
    let catalog_by_name: HashMap<String, &str> = catalog
        .iter()
        .map(|(id, name)| (normalize_name(name), id.as_str()))
        .collect();

    // New tags by normalized name, so two proposals renamed alike create one tag.
    let mut new_tags: BTreeMap<String, (String, String, Option<String>)> = BTreeMap::new();
    let mut resolved = Vec::new();
    let mut seen = HashSet::new();
    for decision in decisions {
        let proposal_id = decision.proposal_id().to_string();
        let Some(proposal_assets) = assets_of.get(&proposal_id) else {
            return Err("A proposal was already reviewed, or isn't part of this run".into());
        };
        if !seen.insert(proposal_id.clone()) {
            return Err("A proposal has two decisions".into());
        }
        let ticked = |asset_ids: Vec<String>| -> Vec<String> {
            let wanted: HashSet<String> = asset_ids.into_iter().collect();
            proposal_assets
                .iter()
                .filter(|a| wanted.contains(*a))
                .cloned()
                .collect()
        };
        resolved.push(match decision {
            ProposalDecision::Reject { .. } => Resolved {
                proposal_id,
                status: "rejected",
                tag_id: None,
                created_tag: false,
                ticked: Vec::new(),
            },
            ProposalDecision::Map {
                tag_id, asset_ids, ..
            } => {
                if !catalog_ids.contains(tag_id.as_str()) {
                    return Err("The tag to map onto no longer exists".into());
                }
                Resolved {
                    proposal_id,
                    status: "mapped",
                    tag_id: Some(tag_id),
                    created_tag: false,
                    ticked: ticked(asset_ids),
                }
            }
            ProposalDecision::Accept {
                name,
                parent_id,
                asset_ids,
                ..
            } => {
                let name = clean_name(&name);
                if name.is_empty() {
                    return Err("A tag needs a name".into());
                }
                if let Some(parent) = &parent_id {
                    if !catalog_ids.contains(parent.as_str()) {
                        return Err(format!("The parent chosen for \"{name}\" no longer exists"));
                    }
                }
                let normalized = normalize_name(&name);
                match catalog_by_name.get(&normalized) {
                    Some(existing) => Resolved {
                        proposal_id,
                        status: "mapped",
                        tag_id: Some(existing.to_string()),
                        created_tag: false,
                        ticked: ticked(asset_ids),
                    },
                    None => {
                        let (tag_id, _, _) = new_tags
                            .entry(normalized)
                            .or_insert_with(|| (uuid::Uuid::new_v4().to_string(), name, parent_id));
                        Resolved {
                            proposal_id,
                            status: "accepted",
                            tag_id: Some(tag_id.clone()),
                            created_tag: true,
                            ticked: ticked(asset_ids),
                        }
                    }
                }
            }
        });
    }

    // Libraries open now; any other waits for replay. Taken before step 1, so a library that
    // opens meanwhile is left to the replay that reads its rows as it opens.
    let mut open: BTreeMap<String, LibraryHandle> = BTreeMap::new();
    for lib in db.get_all_libraries().await {
        open.insert(lib.id.clone(), lib);
    }

    // The ticked pairs, without those an open library already has.
    let mut pairs: Vec<PendingPair> = Vec::new();
    for r in &resolved {
        let Some(tag_id) = &r.tag_id else { continue };
        for asset_id in &r.ticked {
            if let Some(library_id) = library_of.get(asset_id) {
                pairs.push(PendingPair {
                    run_id: run_id.to_string(),
                    asset_id: asset_id.clone(),
                    library_id: library_id.clone(),
                    tag_id: tag_id.clone(),
                    run_cleared: false,
                });
            }
        }
    }
    pairs.sort();
    pairs.dedup();
    let present = present_pairs(&open, &pairs)
        .await
        .map_err(|e| e.to_string())?;
    pairs.retain(|p| !present.contains(&(p.asset_id.clone(), p.tag_id.clone())));

    // Step 1.
    let status = record_review(master, run_id, &new_tags, &resolved, &pairs)
        .await
        .map_err(|e| match e {
            ReviewError::AlreadyReviewed => {
                "A proposal was already reviewed, or isn't part of this run".to_string()
            }
            ReviewError::Db(e) => e.to_string(),
        })?;

    // Step 2.
    for (tag_id, _, _) in new_tags.values() {
        mirror_tag(db, &open, tag_id).await;
    }

    // Steps 3 and 4.
    let mut changed = Vec::new();
    let mut by_library: BTreeMap<&str, Vec<PendingPair>> = BTreeMap::new();
    for pair in &pairs {
        by_library
            .entry(pair.library_id.as_str())
            .or_default()
            .push(pair.clone());
    }
    for (library_id, pairs) in by_library {
        let Some(lib) = open.get(library_id) else {
            continue;
        };
        match store::apply_pairs(db, lib, &pairs, Applier::Item).await {
            Ok(assets) => changed.extend(assets),
            Err(e) => tracing::warn!("Reviewed tags for library {library_id} wait for replay: {e}"),
        }
    }
    Ok(Applied {
        status,
        changed_assets: changed,
    })
}

/// The `(asset, tag)` pairs among `pairs` that their open library already has.
async fn present_pairs(
    open: &BTreeMap<String, LibraryHandle>,
    pairs: &[PendingPair],
) -> sqlx::Result<HashSet<(String, String)>> {
    let mut by_library: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for pair in pairs {
        by_library
            .entry(pair.library_id.as_str())
            .or_default()
            .push(pair.asset_id.as_str());
    }
    let mut present = HashSet::new();
    for (library_id, mut asset_ids) in by_library {
        let Some(lib) = open.get(library_id) else {
            continue;
        };
        asset_ids.dedup();
        for chunk in asset_ids.chunks(BATCH) {
            let sql = format!(
                "SELECT asset_id, tag_id FROM asset_tags WHERE asset_id IN ({})",
                vec!["?"; chunk.len()].join(", ")
            );
            let mut query = sqlx::query_as::<_, (String, String)>(&sql);
            for id in chunk {
                query = query.bind(*id);
            }
            present.extend(query.fetch_all(&lib.pool).await?);
        }
    }
    Ok(present)
}

enum ReviewError {
    AlreadyReviewed,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for ReviewError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// Write protocol step 1 for a review, in one master transaction. Returns the run's status.
async fn record_review(
    master: &Pool<Sqlite>,
    run_id: &str,
    new_tags: &BTreeMap<String, (String, String, Option<String>)>,
    resolved: &[Resolved],
    pairs: &[PendingPair],
) -> Result<String, ReviewError> {
    let mut tx = master.begin().await?;
    for (tag_id, name, parent_id) in new_tags.values() {
        sqlx::query("INSERT INTO tags (id, name, parent_id) VALUES (?, ?, ?)")
            .bind(tag_id)
            .bind(name)
            .bind(parent_id)
            .execute(&mut *tx)
            .await?;
    }
    for r in resolved {
        // Only a pending proposal: a review applied meanwhile rolls this one back.
        let updated = sqlx::query(
            "UPDATE tag_run_proposals SET status = ?, final_tag_id = ?, created_tag = ?
             WHERE id = ? AND run_id = ? AND status = 'pending'",
        )
        .bind(r.status)
        .bind(&r.tag_id)
        .bind(r.created_tag)
        .bind(&r.proposal_id)
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            return Err(ReviewError::AlreadyReviewed);
        }
        if r.status == "rejected" {
            continue;
        }
        sqlx::query("UPDATE tag_run_proposal_assets SET included = 0 WHERE proposal_id = ?")
            .bind(&r.proposal_id)
            .execute(&mut *tx)
            .await?;
        for asset_id in &r.ticked {
            sqlx::query(
                "UPDATE tag_run_proposal_assets SET included = 1
                 WHERE proposal_id = ? AND asset_id = ?",
            )
            .bind(&r.proposal_id)
            .bind(asset_id)
            .execute(&mut *tx)
            .await?;
        }
    }
    store::insert_pending(&mut tx, pairs).await?;
    sqlx::query(
        "UPDATE tag_runs SET status = 'done'
         WHERE id = ?1 AND status = 'awaiting_review'
           AND NOT EXISTS (SELECT 1 FROM tag_run_proposals
                           WHERE run_id = ?1 AND status = 'pending')",
    )
    .bind(run_id)
    .execute(&mut *tx)
    .await?;
    let status = sqlx::query_scalar("SELECT status FROM tag_runs WHERE id = ?")
        .bind(run_id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(status)
}

/// Write protocol step 2: a created tag, with its ancestors, into every open library's mirror.
/// Idempotent, and `sync_open_libraries` repairs a mirror that misses it at the next open.
async fn mirror_tag(db: &Database, open: &BTreeMap<String, LibraryHandle>, tag_id: &str) {
    let chain = match db.tag_with_ancestors(tag_id).await {
        Ok(chain) => chain,
        Err(e) => {
            tracing::warn!("Could not read created tag {tag_id} to mirror it: {e}");
            return;
        }
    };
    for lib in open.values() {
        let mirror = async {
            let mut tx = lib.pool.begin().await?;
            for definition in &chain {
                upsert_tag(&mut *tx, definition).await?;
            }
            tx.commit().await
        };
        if let Err(e) = mirror.await {
            tracing::warn!("Failed to mirror tag {tag_id} to library {}: {e}", lib.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{
        insert_asset, insert_tag, memory_database, memory_library, memory_library_pool,
    };

    struct Setup {
        db: Database,
        a: Pool<Sqlite>,
        b: Pool<Sqlite>,
    }

    /// A run over a1 and a2 in library A and b1 in library B, whose proposals are:
    /// "Rock" (under Nature) for all three, "Stones" for a2, "Moss" for a1, "Gravel" for b1 and
    /// "Junk" for b1. The catalog holds Nature and Stone.
    async fn run_with_proposals(status: &str) -> Setup {
        let a = memory_library_pool().await;
        let b = memory_library_pool().await;
        insert_asset(&a, "a1").await;
        insert_asset(&a, "a2").await;
        insert_asset(&b, "b1").await;
        sqlx::query(
            "UPDATE assets SET thumbnail_path = '.opendam/thumbs/a1_thumb.png' WHERE id = 'a1'",
        )
        .execute(&a)
        .await
        .unwrap();
        let db = memory_database(vec![
            memory_library("a-lib", a.clone()),
            memory_library("b-lib", b.clone()),
        ])
        .await;
        let master = &db.master_pool;
        insert_tag(master, "t-nature", "Nature").await;
        insert_tag(master, "t-stone", "Stone").await;
        sqlx::query(
            "INSERT INTO tag_runs (id, label, provider, mode, status, created_at)
             VALUES ('run', '3 assets', 'fake', 'prefer_existing', ?, 0)",
        )
        .bind(status)
        .execute(master)
        .await
        .unwrap();
        for (asset, library) in [("a1", "a-lib"), ("a2", "a-lib"), ("b1", "b-lib")] {
            sqlx::query(
                "INSERT INTO tag_run_items (run_id, asset_id, library_id, status)
                 VALUES ('run', ?, ?, 'done')",
            )
            .bind(asset)
            .bind(library)
            .execute(master)
            .await
            .unwrap();
        }
        let proposals: [(&str, &str, Option<&str>, &[&str]); 5] = [
            ("p-rock", "Rock", Some("t-nature"), &["a1", "a2", "b1"]),
            ("p-stones", "Stones", None, &["a2"]),
            ("p-moss", "Moss", None, &["a1"]),
            ("p-gravel", "Gravel", None, &["b1"]),
            ("p-junk", "Junk", None, &["b1"]),
        ];
        for (id, name, parent, assets) in proposals {
            sqlx::query(
                "INSERT INTO tag_run_proposals (id, run_id, name, normalized_name, parent_id)
                 VALUES (?, 'run', ?, ?, ?)",
            )
            .bind(id)
            .bind(name)
            .bind(normalize_name(name))
            .bind(parent)
            .execute(master)
            .await
            .unwrap();
            for asset in assets {
                sqlx::query(
                    "INSERT INTO tag_run_proposal_assets (proposal_id, asset_id) VALUES (?, ?)",
                )
                .bind(id)
                .bind(asset)
                .execute(master)
                .await
                .unwrap();
            }
        }
        Setup { db, a, b }
    }

    fn accept(id: &str, name: &str, parent: Option<&str>, assets: &[&str]) -> ProposalDecision {
        ProposalDecision::Accept {
            proposal_id: id.into(),
            name: name.into(),
            parent_id: parent.map(Into::into),
            asset_ids: assets.iter().map(|a| a.to_string()).collect(),
        }
    }

    fn map(id: &str, tag: &str, assets: &[&str]) -> ProposalDecision {
        ProposalDecision::Map {
            proposal_id: id.into(),
            tag_id: tag.into(),
            asset_ids: assets.iter().map(|a| a.to_string()).collect(),
        }
    }

    fn reject(id: &str) -> ProposalDecision {
        ProposalDecision::Reject {
            proposal_id: id.into(),
        }
    }

    async fn tag_names(pool: &Pool<Sqlite>) -> Vec<(String, Option<String>)> {
        sqlx::query_as(
            "SELECT t.name, p.name FROM tags t LEFT JOIN tags p ON p.id = t.parent_id
             ORDER BY t.name",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    async fn asset_tag_names(pool: &Pool<Sqlite>) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT at.asset_id, t.name FROM asset_tags at JOIN tags t ON t.id = at.tag_id
             ORDER BY at.asset_id, t.name",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    async fn run_status(db: &Database) -> String {
        sqlx::query_scalar("SELECT status FROM tag_runs WHERE id = 'run'")
            .fetch_one(&db.master_pool)
            .await
            .unwrap()
    }

    fn pair(asset: &str, name: &str) -> (String, String) {
        (asset.into(), name.into())
    }

    #[tokio::test]
    async fn the_review_lists_pending_proposals_with_their_assets() {
        let s = run_with_proposals("awaiting_review").await;
        sqlx::query("DELETE FROM tags WHERE id = 't-nature'")
            .execute(&s.db.master_pool)
            .await
            .unwrap();

        let proposals = load(&s.db, "run").await.unwrap();

        let names: Vec<&str> = proposals.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Rock", "Stones", "Moss", "Gravel", "Junk"]);
        assert_eq!(
            proposals[0].parent_id, None,
            "a suggested parent that left the catalog is dropped"
        );
        assert_eq!(
            proposals[0].assets[0],
            ReviewAsset {
                asset_id: "a1".into(),
                file_name: Some("a1.glb".into()),
                thumbnail_path: Some("/mnt/a-lib/.opendam/thumbs/a1_thumb.png".into()),
                included: true,
            }
        );
        let assets: Vec<&str> = proposals[0]
            .assets
            .iter()
            .map(|a| a.asset_id.as_str())
            .collect();
        assert_eq!(assets, ["a1", "a2", "b1"]);
    }

    #[tokio::test]
    async fn review_accepts_renames_maps_unticks_and_rejects() {
        let s = run_with_proposals("awaiting_review").await;
        assert_eq!(
            load(&s.db, "run").await.unwrap()[0].parent_id.as_deref(),
            Some("t-nature")
        );

        let applied = apply(
            &s.db,
            "run",
            vec![
                // Renamed, kept under the suggested parent, a2 unticked.
                accept("p-rock", "  Boulder ", Some("t-nature"), &["a1", "b1"]),
                map("p-stones", "t-stone", &["a2"]),
                accept("p-moss", "Moss", None, &["a1"]),
                // Renamed to a catalog tag's name: maps onto it.
                accept("p-gravel", "STONE", None, &["b1"]),
                reject("p-junk"),
            ],
        )
        .await
        .unwrap();

        assert_eq!(applied.status, "done");
        let catalog = vec![
            ("Boulder".to_string(), Some("Nature".to_string())),
            ("Moss".into(), None),
            ("Nature".into(), None),
            ("Stone".into(), None),
        ];
        assert_eq!(tag_names(&s.db.master_pool).await, catalog);
        for mirror in [&s.a, &s.b] {
            let names: Vec<String> = tag_names(mirror).await.into_iter().map(|t| t.0).collect();
            for created in ["Boulder", "Moss", "Nature"] {
                assert!(names.contains(&created.to_string()), "{names:?}");
            }
        }
        assert_eq!(
            asset_tag_names(&s.a).await,
            [
                pair("a1", "Boulder"),
                pair("a1", "Moss"),
                pair("a2", "Stone")
            ],
            "a2 was unticked for Boulder"
        );
        assert_eq!(
            asset_tag_names(&s.b).await,
            [pair("b1", "Boulder"), pair("b1", "Stone")]
        );

        let master = &s.db.master_pool;
        let outcomes: Vec<(String, String, Option<String>, bool)> = sqlx::query_as(
            "SELECT p.id, p.status, t.name, p.created_tag
             FROM tag_run_proposals p LEFT JOIN tags t ON t.id = p.final_tag_id ORDER BY p.id",
        )
        .fetch_all(master)
        .await
        .unwrap();
        assert_eq!(
            outcomes,
            [
                (
                    "p-gravel".into(),
                    "mapped".into(),
                    Some("Stone".into()),
                    false
                ),
                ("p-junk".into(), "rejected".into(), None, false),
                (
                    "p-moss".into(),
                    "accepted".into(),
                    Some("Moss".into()),
                    true
                ),
                (
                    "p-rock".into(),
                    "accepted".into(),
                    Some("Boulder".into()),
                    true
                ),
                (
                    "p-stones".into(),
                    "mapped".into(),
                    Some("Stone".into()),
                    false
                ),
            ]
        );
        let unticked: Vec<(String, String)> = sqlx::query_as(
            "SELECT proposal_id, asset_id FROM tag_run_proposal_assets WHERE included = 0",
        )
        .fetch_all(master)
        .await
        .unwrap();
        assert_eq!(unticked, [("p-rock".into(), "a2".into())]);
        let recorded: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT a.asset_id, t.name, a.state FROM tag_run_applied a
             JOIN tags t ON t.id = a.tag_id ORDER BY a.asset_id, t.name",
        )
        .fetch_all(master)
        .await
        .unwrap();
        assert_eq!(
            recorded,
            [
                ("a1".into(), "Boulder".into(), "applied".into()),
                ("a1".into(), "Moss".into(), "applied".into()),
                ("a2".into(), "Stone".into(), "applied".into()),
                ("b1".into(), "Boulder".into(), "applied".into()),
                ("b1".into(), "Stone".into(), "applied".into()),
            ],
            "every pair is recorded for undo"
        );
        assert!(load(&s.db, "run").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_cancelled_runs_review_keeps_it_cancelled_and_undecided_proposals_wait() {
        let s = run_with_proposals("cancelled").await;

        let applied = apply(
            &s.db,
            "run",
            vec![accept("p-moss", "Moss", None, &["a1"]), reject("p-junk")],
        )
        .await
        .unwrap();

        assert_eq!(applied.status, "cancelled");
        assert_eq!(applied.changed_assets, ["a1"]);
        assert_eq!(asset_tag_names(&s.a).await, [pair("a1", "Moss")]);
        let left: Vec<String> = load(&s.db, "run")
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(left, ["Rock", "Stones", "Gravel"]);

        let rest = vec![reject("p-rock"), reject("p-stones"), reject("p-gravel")];
        assert_eq!(apply(&s.db, "run", rest).await.unwrap().status, "cancelled");
        assert_eq!(run_status(&s.db).await, "cancelled");
    }

    #[tokio::test]
    async fn an_awaiting_run_is_done_only_once_nothing_is_left_to_review() {
        let s = run_with_proposals("awaiting_review").await;

        let partial = apply(&s.db, "run", vec![reject("p-junk")]).await.unwrap();

        assert_eq!(partial.status, "awaiting_review");
        let rest = ["p-rock", "p-stones", "p-moss", "p-gravel"]
            .map(reject)
            .to_vec();
        assert_eq!(apply(&s.db, "run", rest).await.unwrap().status, "done");
        assert_eq!(
            tag_names(&s.db.master_pool).await.len(),
            2,
            "rejecting everything creates nothing"
        );
    }

    #[tokio::test]
    async fn a_review_is_refused_while_the_run_is_going_or_for_a_proposal_already_reviewed() {
        for status in ["running", "paused", "done", "queued"] {
            let s = run_with_proposals(status).await;
            let error = apply(&s.db, "run", vec![reject("p-junk")])
                .await
                .unwrap_err();
            assert!(error.contains("can be reviewed"), "{status}: {error}");
        }

        let s = run_with_proposals("cancelled").await;
        sqlx::query("UPDATE tag_run_items SET status = 'running' WHERE asset_id = 'a1'")
            .execute(&s.db.master_pool)
            .await
            .unwrap();
        let error = apply(&s.db, "run", vec![reject("p-junk")])
            .await
            .unwrap_err();
        assert!(error.contains("in flight"), "{error}");

        let s = run_with_proposals("awaiting_review").await;
        apply(&s.db, "run", vec![reject("p-junk")]).await.unwrap();
        let error = apply(&s.db, "run", vec![reject("p-junk")])
            .await
            .unwrap_err();
        assert!(error.contains("already reviewed"), "{error}");
        let error = apply(
            &s.db,
            "run",
            vec![accept("p-moss", "Moss", Some("t-ghost"), &["a1"])],
        )
        .await
        .unwrap_err();
        assert!(error.contains("no longer exists"), "{error}");
        assert_eq!(
            tag_names(&s.db.master_pool).await.len(),
            2,
            "nothing created"
        );
    }

    #[tokio::test]
    async fn a_review_with_a_bad_decision_writes_nothing() {
        let cases = [
            (
                map("p-stones", "t-ghost", &["a2"]),
                "The tag to map onto no longer exists",
            ),
            (
                accept("p-stones", "   ", None, &["a2"]),
                "A tag needs a name",
            ),
            (reject("p-moss"), "A proposal has two decisions"),
        ];
        for (bad, message) in cases {
            let s = run_with_proposals("awaiting_review").await;
            let master = &s.db.master_pool;

            // A good decision goes first, so a partial write would show.
            let error = apply(
                &s.db,
                "run",
                vec![accept("p-moss", "Moss", None, &["a1"]), bad],
            )
            .await
            .unwrap_err();

            assert_eq!(error, message);
            assert_eq!(
                count(
                    master,
                    "SELECT COUNT(*) FROM tag_run_proposals WHERE status = 'pending'"
                )
                .await,
                5,
                "{message}: every proposal stays pending"
            );
            assert_eq!(
                tag_names(master).await.len(),
                2,
                "{message}: no tag created"
            );
            assert_eq!(
                count(master, "SELECT COUNT(*) FROM tag_run_applied").await,
                0
            );
            assert!(asset_tag_names(&s.a).await.is_empty());
            assert_eq!(run_status(&s.db).await, "awaiting_review");
        }
    }

    #[tokio::test]
    async fn a_ticked_asset_in_an_offline_library_waits_for_replay() {
        let s = run_with_proposals("awaiting_review").await;
        s.db.unregister_library("b-lib").await.unwrap();

        let applied = apply(
            &s.db,
            "run",
            vec![accept("p-rock", "Rock", Some("t-nature"), &["a1", "b1"])],
        )
        .await
        .unwrap();

        assert_eq!(applied.changed_assets, ["a1"]);
        let master = &s.db.master_pool;
        let rock: String = sqlx::query_scalar("SELECT id FROM tags WHERE name = 'Rock'")
            .fetch_one(master)
            .await
            .unwrap();
        assert!(tag_names(&s.a)
            .await
            .contains(&("Rock".into(), Some("Nature".into()))));
        assert!(
            tag_names(&s.b).await.is_empty(),
            "an offline library's mirror isn't written"
        );
        assert_eq!(asset_tag_names(&s.a).await, [pair("a1", "Rock")]);
        let recorded: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT asset_id, library_id, tag_id, state FROM tag_run_applied ORDER BY asset_id",
        )
        .fetch_all(master)
        .await
        .unwrap();
        assert_eq!(
            recorded,
            [
                ("a1".into(), "a-lib".into(), rock.clone(), "applied".into()),
                ("b1".into(), "b-lib".into(), rock, "pending".into()),
            ],
            "b1's pair is recorded for undo and waits for replay"
        );
    }

    async fn count(pool: &Pool<Sqlite>, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
    }

    #[tokio::test]
    async fn two_proposals_accepted_under_one_name_create_one_tag() {
        let s = run_with_proposals("awaiting_review").await;

        apply(
            &s.db,
            "run",
            vec![
                accept("p-moss", "Lichen", None, &["a1"]),
                accept("p-gravel", "lichen", None, &["b1"]),
            ],
        )
        .await
        .unwrap();

        let lichens: Vec<(String, Option<String>)> = tag_names(&s.db.master_pool)
            .await
            .into_iter()
            .filter(|t| t.0.to_lowercase() == "lichen")
            .collect();
        assert_eq!(lichens, [("Lichen".into(), None)]);
        assert_eq!(asset_tag_names(&s.a).await, [pair("a1", "Lichen")]);
        assert_eq!(asset_tag_names(&s.b).await, [pair("b1", "Lichen")]);
    }
}
