//! What the detail panel's Game tab shows for an asset (plan game-export C13): what it exports
//! as, its `assetName`, inferred and yours (C4, C5), and the problems the Checks of the domains
//! holding it find with it. So far the tab edits only the name.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;

use crate::db::Database;
pub use crate::export::check::Field;
use crate::export::check::{self, ItemRow, Kind, Problem};
use crate::export::fbx_facts;
use crate::export::{edits, infer, settings};

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

/// The Game tab of asset `asset_id`, reading FBX facts with `facts`, or `None` when it exports
/// as neither a model, an animation nor a texture. A group's member stands for its main item.
/// Its kind comes from the Check's rule (`check::exported_kind`). Each domain holding it is
/// checked as the Export view's Check does, without staging, and its name is the one the first
/// of those Checks, by domain name, exports it with, or `check::asset_name` in no domain.
pub async fn detail(
    db: &Database,
    asset_id: &str,
    facts: &impl fbx_facts::Reader,
    memo: &Arc<infer::Memo>,
) -> Result<Option<GameDetail>, String> {
    let library = db
        .library_of_asset(asset_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("No asset has the id {asset_id}"))?;
    let pool = &library.pool;
    let item: ItemRow = sqlx::query_as(
        "SELECT id, file_path, file_name, extension, kind FROM assets
         WHERE id = (SELECT COALESCE(group_id, id) FROM assets WHERE id = ?)",
    )
    .bind(asset_id)
    .fetch_one(pool)
    .await
    .map_err(|e| e.to_string())?;
    let rows = edits::read_all(pool).await.map_err(|e| e.to_string())?;
    // Outside any domain, a texture's inferred name is its own set's default
    let Some(mut asset_name) = check::asset_name(&item, &rows) else {
        return Ok(None);
    };
    let fbx = check::exported_fbx_of(pool, std::slice::from_ref(&item))
        .await
        .map_err(|e| e.to_string())?;
    let item_facts = match fbx.get(&item.id) {
        Some(path) => {
            let root = &library.root_path;
            Some(facts.read(&root.join(path), root).await)
        }
        None => None,
    };
    let kind = check::exported_kind(&item, item_facts.as_ref());

    // The domains that may hold the item: each collection it is in counts for the root
    // collection's child above it, and an image may join a domain through a material or its set
    // (C3), which only a domain with an item in the image's own pack can bring in
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
        if kind == Kind::Texture {
            let packs: Vec<String> = sqlx::query_scalar("SELECT path FROM folders WHERE is_pack")
                .fetch_all(pool)
                .await
                .map_err(|e| e.to_string())?;
            if let Some(pack) = check::pack_at(&packs, &item.file_path) {
                for (id, (name, parent)) in &collections {
                    if parent.as_deref() != Some(root.as_str()) || domains.contains_key(id) {
                        continue;
                    }
                    let subtree = subtree(&collections, id);
                    // The domain's items below the pack folder, as their main items
                    let paths: Vec<String> = sqlx::query_scalar(
                        "SELECT DISTINCT m.file_path FROM collection_assets ca
                         JOIN assets a ON a.id = ca.asset_id
                         JOIN assets m ON m.id = COALESCE(a.group_id, a.id)
                         WHERE ca.collection_id IN (SELECT value FROM json_each(?1))
                           AND (?2 = '' OR (m.file_path >= ?2 || '/' AND m.file_path < ?2 || '0'))",
                    )
                    .bind(serde_json::to_string(&subtree).expect("ids serialize"))
                    .bind(pack)
                    .fetch_all(pool)
                    .await
                    .map_err(|e| e.to_string())?;
                    if paths
                        .iter()
                        .any(|p| check::pack_at(&packs, p) == Some(pack))
                    {
                        domains.insert(id.clone(), name.clone());
                    }
                }
            }
        }
    }

    // By name, so the first domain, whose Check gives the inferred name, is always the same one
    let mut domains: Vec<(String, String)> = domains.into_iter().map(|(id, n)| (n, id)).collect();
    domains.sort();
    let mut problems: Vec<Problem> = Vec::new();
    let mut holding: Vec<String> = Vec::new();
    for (domain_name, domain) in &domains {
        let inspection = check::inspect(db, domain, facts, memo).await?;
        // A domain holds the item when its Check lists it
        let Some(name) = inspection.names.get(&item.id) else {
            continue;
        };
        // In a domain, a texture's inferred name can come from the models that use its set (D24)
        if holding.is_empty() {
            asset_name = name.clone();
        }
        holding.push(domain_name.clone());
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
    Ok(Some(GameDetail {
        asset_id: item.id,
        kind,
        asset_name,
        domains: holding,
        problems,
    }))
}

/// Collection `id` and every collection below it.
fn subtree(collections: &BTreeMap<String, (String, Option<String>)>, id: &str) -> Vec<String> {
    let mut ids = vec![id.to_string()];
    let mut i = 0;
    while i < ids.len() {
        let parent = ids[i].clone();
        ids.extend(
            collections
                .iter()
                .filter(|(c, (_, p))| p.as_deref() == Some(parent.as_str()) && !ids.contains(c))
                .map(|(c, _)| c.clone())
                .collect::<Vec<_>>(),
        );
        i += 1;
    }
    ids
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

    fn memo() -> Arc<infer::Memo> {
        Arc::new(infer::Memo::default())
    }

    async fn detail_of(f: &Fixture, path: &str) -> Option<GameDetail> {
        let id = id_of(&f.library.pool, path).await;
        detail(&f.db, &id, &Snapshot::fixture(), &memo())
            .await
            .unwrap()
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
        // The tab shows the name and kind the Check exports it with
        let report = check::check(&f.db, "props", &Snapshot::fixture(), &memo())
            .await
            .unwrap();
        let row = report
            .assets
            .iter()
            .find(|a| a.asset.asset_id == id)
            .unwrap();
        assert_eq!(row.asset_name, Some(shown.asset_name.value()));
        assert_eq!(row.kind, shown.kind);
    }

    #[tokio::test]
    async fn in_a_domain_a_textures_inferred_name_is_the_one_its_model_gives_it() {
        let f = fixture().await;
        let model = "Dungeon Props/Models/SM_Crate.fbx";
        let normal = "Dungeon Props/Textures/1K/T_Crate_N.png";
        let id = id_of(&f.library.pool, model).await;
        let rename = json!({"assetName": "Box"});
        edits::write(&f.library.pool, &id, rename.as_object().unwrap())
            .await
            .unwrap();

        // Outside any domain, its own stem's
        assert_eq!(
            detail_of(&f, normal).await.unwrap().asset_name.inferred,
            "Crate"
        );
        // In one, its only model's name (D24)
        collection(&f, "props", "Props", "dg", &[model]).await;
        let shown = detail_of(&f, normal).await.unwrap();
        assert_eq!(
            shown.asset_name,
            Field {
                inferred: "Box".into(),
                yours: None
            }
        );
        assert_eq!(
            shown.domains,
            ["Props"],
            "it joins through the model's material"
        );
    }

    /// FBX facts no test may ask for: a Check of a domain holding a model would.
    struct Unread;

    impl fbx_facts::Reader for Unread {
        async fn read(&self, source: &Path, _: &Path) -> Result<FbxFacts, String> {
            panic!("read {} for its facts", source.display())
        }
    }

    #[tokio::test]
    async fn an_image_runs_no_check_of_a_domain_with_nothing_in_its_pack() {
        let f = fixture().await;
        collection(
            &f,
            "props",
            "Props",
            "dg",
            &["Dungeon Props/Models/SM_Chest.fbx"],
        )
        .await;
        let id = id_of(&f.library.pool, "Kenney Crates/crate_wood.png").await;
        let shown = detail(&f.db, &id, &Unread, &memo()).await.unwrap().unwrap();
        assert_eq!(shown.domains, Vec::<String>::new());
        assert_eq!(shown.asset_name.inferred, "CrateWood");
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
        let shown = detail(&f.db, &id, &snapshot, &memo())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(shown.kind, Kind::Animation);
        assert_eq!(shown.asset_name.inferred, "Guard");
    }
}
