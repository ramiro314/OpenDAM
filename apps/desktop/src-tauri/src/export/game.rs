//! What the detail panel's Game tab shows for an asset (plan game-export C13): what it exports
//! as, its `assetName`, inferred and yours (C4, C5), and the problems the Checks of the domains
//! holding it find with it. So far the tab edits only the name.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::db::Database;
use crate::export::check::{self, Kind, Problem};
use crate::export::fbx_facts::{self, Reading};
use crate::export::names::pascal;
use crate::export::textures::read_stem;
use crate::export::{edits, settings};
use crate::models::AssetKind;

/// A value the export infers, and yours when you set one, which wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Field<T> {
    pub inferred: T,
    pub yours: Option<T>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameDetail {
    /// The grid item the tab is about: the asset's main item when it is in a group
    pub asset_id: String,
    pub kind: Kind,
    pub asset_name: Field<String>,
    /// The domains holding the item, by name
    pub domains: Vec<String>,
    /// The problems of those domains' Checks that name the item, each once
    pub problems: Vec<Problem>,
}

#[derive(sqlx::FromRow)]
struct Item {
    id: String,
    file_path: String,
    file_name: String,
    kind: String,
}

/// The Game tab of asset `asset_id`, reading FBX facts with `facts`, or `None` when it exports
/// as neither a model, an animation nor a texture. A group's member stands for its main item.
/// Each domain holding the item is checked as the Export view's Check does, without staging.
pub async fn detail(
    db: &Database,
    asset_id: &str,
    facts: &impl fbx_facts::Reader,
) -> Result<Option<GameDetail>, String> {
    let library = db
        .library_of_asset(asset_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("No asset has the id {asset_id}"))?;
    let pool = &library.pool;
    let item: Item = sqlx::query_as(
        "SELECT id, file_path, file_name, kind FROM assets
         WHERE id = (SELECT COALESCE(group_id, id) FROM assets WHERE id = ?)",
    )
    .bind(asset_id)
    .fetch_one(pool)
    .await
    .map_err(|e| e.to_string())?;
    let stem = item
        .file_name
        .rsplit_once('.')
        .map_or(item.file_name.as_str(), |(stem, _)| stem);
    let rows = edits::read_all(pool).await.map_err(|e| e.to_string())?;

    let (kind, asset_name) = match AssetKind::from_str(&item.kind) {
        AssetKind::Model3d => {
            let mut paths: Vec<String> = sqlx::query_scalar(
                "SELECT file_path FROM assets WHERE group_id = ? AND lower(extension) = 'fbx'",
            )
            .bind(&item.id)
            .fetch_all(pool)
            .await
            .map_err(|e| e.to_string())?;
            if item.file_name.to_ascii_lowercase().ends_with(".fbx") {
                paths.push(item.file_path.clone());
            }
            let mut kind = Kind::Model;
            if let Some(fbx) = check::exported_fbx(paths.iter().map(String::as_str)) {
                let root = &library.root_path;
                if let Ok(facts) = facts.read(&root.join(fbx), root).await {
                    if facts.reading() == Some(Reading::Animation) {
                        kind = Kind::Animation;
                    }
                }
            }
            let yours = rows
                .iter()
                .find(|r| r.asset_id == item.id)
                .and_then(|r| edits::asset_name(&r.parsed()));
            (
                kind,
                Field {
                    inferred: pascal(stem),
                    yours,
                },
            )
        }
        _ if edits::is_image(&item.kind) => (
            Kind::Texture,
            Field {
                inferred: pascal(&read_stem(stem).base),
                yours: edits::set_name(&rows, &item.file_path, &item.file_name),
            },
        ),
        _ => return Ok(None),
    };

    // The domains holding the item: each collection it is in counts for the root collection's
    // child above it
    let settings = settings::load(&db.master_pool)
        .await
        .map_err(|e| e.to_string())?;
    let mut domains: BTreeMap<String, String> = BTreeMap::new();
    if let Some(root) = &settings.root_collection {
        let collections: BTreeMap<String, (String, Option<String>)> =
            sqlx::query_as::<_, (String, String, Option<String>)>(
                "SELECT id, name, parent_id FROM collections",
            )
            .fetch_all(&db.master_pool)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|(id, name, parent)| (id, (name, parent)))
            .collect();
        let held: Vec<String> =
            sqlx::query_scalar("SELECT collection_id FROM collection_assets WHERE asset_id = ?")
                .bind(&item.id)
                .fetch_all(pool)
                .await
                .map_err(|e| e.to_string())?;
        for id in held {
            let mut at = id;
            // At most one step per collection, so a cycle ends
            for _ in 0..=collections.len() {
                let Some((name, parent)) = collections.get(&at) else {
                    break;
                };
                if parent.as_deref() == Some(root.as_str()) {
                    domains.insert(at.clone(), name.clone());
                    break;
                }
                let Some(parent) = parent else { break };
                at = parent.clone();
            }
        }
    }

    let mut problems: Vec<Problem> = Vec::new();
    for domain in domains.keys() {
        let inspection = check::inspect(db, domain, facts).await?;
        for problem in inspection.report.problems {
            let names_item = problem
                .assets
                .iter()
                .any(|a| a.library_id == library.id && a.asset_id == item.id);
            if names_item && !problems.contains(&problem) {
                problems.push(problem);
            }
        }
    }
    let mut domains: Vec<String> = domains.into_values().collect();
    domains.sort();
    Ok(Some(GameDetail {
        asset_id: item.id,
        kind,
        asset_name,
        domains,
        problems,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::fbx_facts::FbxFacts;
    use crate::export::test_fixture::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::Path;

    /// The fixture's FBX facts, as the worker answers them.
    struct Snapshot(HashMap<String, FbxFacts>);

    impl Snapshot {
        fn fixture() -> Self {
            Snapshot(
                serde_json::from_str(include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/game-export/fbx_facts.json"
                )))
                .unwrap(),
            )
        }
    }

    impl fbx_facts::Reader for Snapshot {
        async fn read(&self, source: &Path, root: &Path) -> Result<FbxFacts, String> {
            let path = source.strip_prefix(root).unwrap().display().to_string();
            self.0.get(&path).cloned().ok_or(path)
        }
    }

    async fn detail_of(f: &Fixture, path: &str) -> Option<GameDetail> {
        let id = id_of(&f.library.pool, path).await;
        detail(&f.db, &id, &Snapshot::fixture()).await.unwrap()
    }

    #[tokio::test]
    async fn a_texture_shows_its_names_domains_and_the_problems_naming_it() {
        let f = fixture().await;
        let banner = "Kenney Crates/banner.png";
        collection(
            &f,
            "props",
            "Props",
            "dg",
            &["Dungeon Props/Textures/T_Banner_BaseColor.png"],
        )
        .await;
        collection(&f, "sub", "Flags", "props", &[banner]).await;

        let shown = detail_of(&f, banner).await.unwrap();
        assert_eq!(shown.kind, Kind::Texture);
        assert_eq!(
            shown.asset_name,
            Field {
                inferred: "Banner".into(),
                yours: None
            }
        );
        assert_eq!(shown.domains, ["Props"]);
        assert_eq!(shown.problems.len(), 1);
        assert!(shown.problems[0]
            .message
            .starts_with("2 textures are named Banner"));

        let id = id_of(&f.library.pool, banner).await;
        let rename = json!({"assetName": "KenneyBanner"});
        edits::write(&f.library.pool, &id, rename.as_object().unwrap())
            .await
            .unwrap();
        let shown = detail_of(&f, banner).await.unwrap();
        assert_eq!(shown.asset_name.yours.as_deref(), Some("KenneyBanner"));
        assert_eq!(shown.problems, []);
    }

    #[tokio::test]
    async fn a_member_stands_for_its_main_item_outside_any_domain() {
        let f = fixture().await;
        let shown = detail_of(&f, "Dungeon Props/Models/SM_Chest.obj")
            .await
            .unwrap();
        let fbx = id_of(&f.library.pool, "Dungeon Props/Models/SM_Chest.fbx").await;
        assert_eq!(shown.asset_id, fbx);
        assert_eq!(shown.kind, Kind::Model);
        assert_eq!(shown.asset_name.inferred, "Chest");
        assert_eq!(shown.domains, Vec::<String>::new());
        assert_eq!(shown.problems, []);

        assert_eq!(detail_of(&f, "Dungeon Props/Docs/notes.md").await, None);
    }

    #[tokio::test]
    async fn an_fbx_with_no_kept_mesh_and_a_take_shows_as_an_animation() {
        let f = fixture().await;
        let guard = "Dungeon Props/Models/SK_Guard.fbx";
        let mut snapshot = Snapshot::fixture();
        // The fixture's animation lands with animation export, so its guard stands in for one
        let facts = snapshot.0.get_mut(guard).unwrap();
        facts.has_kept_mesh = false;
        facts.kept_meshes.clear();
        let id = id_of(&f.library.pool, guard).await;
        let shown = detail(&f.db, &id, &snapshot).await.unwrap().unwrap();
        assert_eq!(shown.kind, Kind::Animation);
        assert_eq!(shown.asset_name.inferred, "Guard");
    }
}
