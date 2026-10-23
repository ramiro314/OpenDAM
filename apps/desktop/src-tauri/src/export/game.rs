//! What the detail panel's Game tab shows for an asset (plan game-export C13): what it exports
//! as, its `assetName`, inferred and yours (C4, C5), and the problems the Checks of the domains
//! holding it find with it. A model's rig and facing are edits too, and "Show as exported" has
//! Blender write the normalized model as a GLB for the tab's viewer.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;

use crate::db::Database;
pub use crate::export::check::Field;
use crate::export::check::{self, ItemRow, Kind, Problem};
use crate::export::edits::Rig;
use crate::export::fbx_facts;
use crate::export::packs::Origin;
use crate::export::{blender, cache, edits, hashes, infer, settings};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameDetail {
    /// The grid item the tab is about: the asset's main item when it is in a group
    pub asset_id: String,
    pub kind: Kind,
    pub asset_name: Field<String>,
    /// A model's rig and facing, `None` for anything else
    pub model: Option<ModelDetail>,
    /// The domains holding the item, by name
    pub domains: Vec<String>,
    /// The problems of those domains' Checks that name the item, each once
    pub problems: Vec<Problem>,
}

/// A model's rig and facing, inferred and yours (C13, D13, D20).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDetail {
    /// Inferred from its FBX's facts: none without bones, else generic. At export, Blender's
    /// result says whether it has an armature (A3).
    pub rig: Field<Rig>,
    /// Your quarter turns about Y after the origin's facing, inferred 0: the origin's default
    pub facing_quarter_turns: Field<u8>,
    /// The origin of its pack, whose default facing comes first, `None` in no pack
    pub origin: Option<Origin>,
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
    let model = if kind == Kind::Model {
        let yours = edits::of(&rows, &item.id);
        let has_bones = matches!(&item_facts, Some(Ok(facts)) if !facts.bones.is_empty());
        Some(ModelDetail {
            rig: Field {
                inferred: Rig::inferred(has_bones),
                yours: edits::rig(&yours),
            },
            facing_quarter_turns: Field {
                inferred: 0,
                yours: edits::facing_quarter_turns(&yours),
            },
            origin: check::origin_at(&library, &item.file_path).await?,
        })
    } else {
        None
    };

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
        let inspection = check::inspect(db, domain, facts, memo, check::Models::Cached).await?;
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
        model,
        domains: holding,
        problems,
    }))
}

/// The GLB that shows model `asset_id` as the next export writes it (C13): its normalized FBX,
/// with your facing and the origin's, written as a GLB beside its export cache entry, reading
/// FBX facts with `facts`. A group's member stands for its main item. Blender runs only when the
/// cache holds no GLB for the model's key, so it never runs unless you ask, and a missing or
/// refused Blender is the error. When the cache doesn't hold the normalized FBX either, Blender
/// makes it first, into the entry the next Check reads.
pub async fn exported_view(
    db: &Database,
    asset_id: &str,
    facts: &impl fbx_facts::Reader,
) -> Result<PathBuf, String> {
    let library = db
        .library_of_asset(asset_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("No asset has the id {asset_id}"))?;
    let pool = &library.pool;
    let root = library.root_path.clone();
    let item: ItemRow = sqlx::query_as(
        "SELECT id, file_path, file_name, extension, kind FROM assets
         WHERE id = (SELECT COALESCE(group_id, id) FROM assets WHERE id = ?)",
    )
    .bind(asset_id)
    .fetch_one(pool)
    .await
    .map_err(|e| e.to_string())?;
    let path = item.file_path.clone();
    let fbx = check::exported_fbx_of(pool, std::slice::from_ref(&item))
        .await
        .map_err(|e| e.to_string())?
        .remove(&item.id)
        .ok_or_else(|| {
            format!("{path}: it has no FBX file: a model exports from its group's FBX")
        })?;
    let item_facts = facts
        .read(&root.join(&fbx), &root)
        .await
        .map_err(|e| format!("{fbx}: the preview engine can't read it: {e}"))?;
    // The Check's rule, so the tab shows what the bundle would ship
    if check::exported_kind(&item, Some(&Ok(item_facts.clone()))) != Kind::Model {
        return Err(format!(
            "{path}: it doesn't export as a model, so it has no normalized model to show"
        ));
    }
    let origin = check::origin_at(&library, &path)
        .await?
        .ok_or_else(|| format!("{path}: it is in no pack: mark its folder as a pack"))?;
    let rows = edits::read_all(pool).await.map_err(|e| e.to_string())?;
    let facing = edits::facing_quarter_turns(&edits::of(&rows, &item.id)).unwrap_or(0);
    let source = root.join(&fbx);
    let sha256 = tokio::task::spawn_blocking(move || hashes::sha256_file(&source))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{fbx} can't be read: {e}"))?;
    let (job, key) = check::model_job(path, &root, &fbx, &sha256, origin, facing, &item_facts);
    let glb = cache::model_view(&root, &key);
    if tokio::fs::try_exists(&glb).await.unwrap_or(false) {
        return Ok(glb);
    }

    let settings = settings::load(&db.master_pool)
        .await
        .map_err(|e| e.to_string())?;
    let blender = blender::gate(settings.blender_path.as_deref())
        .await
        .map_err(|e| format!("Blender can't show {} as exported: {e}", job.asset))?;
    let runner = blender::Runner::new(blender);
    let (lookup_root, lookup_key) = (root.clone(), key.clone());
    let cached =
        tokio::task::spawn_blocking(move || cache::cached_model(&lookup_root, &lookup_key))
            .await
            .map_err(|e| e.to_string())?;
    let cached = match cached {
        Some(cached) => cached,
        None => {
            let normalized = runner.run(&job).await?;
            let (save_root, save_key) = (root.clone(), key.clone());
            tokio::task::spawn_blocking(move || {
                cache::save_model(&save_root, &save_key, normalized)
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("{}: {e}", job.asset))?
        }
    };
    runner.view(&job.asset, &cached.fbx, &glb).await?;
    Ok(glb)
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

    #[tokio::test]
    async fn a_model_shows_its_rig_and_facing_inferred_and_yours() {
        let f = fixture().await;
        let stool = detail_of(&f, "Dungeon Props/Models/SM_Stool.fbx")
            .await
            .unwrap();
        assert_eq!(
            stool.model,
            Some(ModelDetail {
                rig: Field {
                    inferred: Rig::None,
                    yours: None
                },
                facing_quarter_turns: Field {
                    inferred: 0,
                    yours: None
                },
                origin: Some(Origin::Generic),
            })
        );
        let guard = "Dungeon Props/Models/SK_Guard.fbx";
        let shown = detail_of(&f, guard).await.unwrap().model.unwrap();
        assert_eq!(shown.rig.inferred, Rig::Generic, "it has bones");

        let id = id_of(&f.library.pool, guard).await;
        let edit = json!({"rig": "none", "facingQuarterTurns": 2});
        edits::write(&f.library.pool, &id, edit.as_object().unwrap())
            .await
            .unwrap();
        let shown = detail_of(&f, guard).await.unwrap().model.unwrap();
        assert_eq!(shown.rig.yours, Some(Rig::None));
        assert_eq!(shown.facing_quarter_turns.yours, Some(2));
        assert_eq!(
            detail_of(&f, "Kenney Crates/crate_wood.png")
                .await
                .unwrap()
                .model,
            None
        );
    }

    /// A Blender at `dir/blender` that answers 5.2.2 and leaves `dir/ran` behind when anything
    /// runs it.
    fn recording_blender(dir: &Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("blender");
        let script = format!(
            "#!/bin/sh\necho run >> '{}'\nprintf 'Blender 5.2.2\\n'\n",
            dir.join("ran").display()
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    async fn set_blender(f: &Fixture, path: &Path) {
        settings::update(&f.db.master_pool, json!({ "blenderPath": path }))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn opening_the_tab_never_starts_blender_and_a_missing_one_says_why() {
        let f = fixture().await;
        let stool = "Dungeon Props/Models/SM_Stool.fbx";
        collection(&f, "props", "Props", "dg", &[stool]).await;
        let dir = tempfile::tempdir().unwrap();
        set_blender(&f, &recording_blender(dir.path())).await;
        let shown = detail_of(&f, stool).await.unwrap();
        assert_eq!(shown.domains, ["Props"]);
        assert!(!dir.path().join("ran").exists(), "Blender ran");

        set_blender(&f, &dir.path().join("no-blender")).await;
        let id = id_of(&f.library.pool, stool).await;
        let shown = exported_view(&f.db, &id, &Snapshot::fixture()).await;
        assert_eq!(
            shown,
            Err(format!(
                "Blender can't show {stool} as exported: No Blender at {}",
                dir.path().join("no-blender").display()
            ))
        );
        let models = cache::cache_dir(&f.library.root_path).join("models");
        assert!(!models.exists(), "nothing was written");

        let image = id_of(&f.library.pool, "Kenney Crates/crate_wood.png").await;
        let shown = exported_view(&f.db, &image, &Snapshot::fixture()).await;
        assert_eq!(
            shown,
            Err("Kenney Crates/crate_wood.png: it has no FBX file: a model exports from its group's FBX".into())
        );
    }

    /// The cached FBX of the GLB `glb`, by its key.
    fn cached_fbx(root: &Path, glb: &Path) -> Option<std::path::PathBuf> {
        let key = glb.file_stem().unwrap().to_str().unwrap();
        cache::cached_model(root, key).map(|cached| cached.fbx)
    }

    /// The file the domain `props`'s Check ships as `Models/Totem.fbx`, taking models only from
    /// the export cache, as the Game tab's Check does.
    async fn checked_model(f: &Fixture) -> Option<std::path::PathBuf> {
        let inspection = check::inspect(
            &f.db,
            "props",
            &Snapshot::fixture(),
            &memo(),
            check::Models::Cached,
        )
        .await
        .unwrap();
        inspection
            .files
            .iter()
            .find(|file| file.file == "Models/Totem.fbx")
            .map(|file| file.source.clone())
    }

    /// With the Blender in `OPENDAM_BLENDER`: "Show as exported" writes the normalized model as a
    /// GLB beside its cache entry, facing glTF's +Z, which the viewer's arrow points along, and
    /// your quarter turn makes a new one.
    #[tokio::test]
    async fn show_as_exported_writes_a_glb_of_the_normalized_model_that_turns_with_your_facing() {
        use crate::export::blender::tests::{glb_vertices, nose};
        let Some(program) = blender::test_blender() else {
            return;
        };
        let f = fixture().await;
        set_blender(&f, &program).await;
        let root = &f.library.root_path;
        let totem = "Dungeon Props/Models/SM_Totem.fbx";
        let id = id_of(&f.library.pool, totem).await;

        let glb = exported_view(&f.db, &id, &Snapshot::fixture())
            .await
            .unwrap();
        let bytes = std::fs::read(&glb).unwrap();
        assert_eq!(&bytes[..4], b"glTF");
        assert_eq!(
            glb.parent(),
            Some(cache::cache_dir(root).join("models").as_path())
        );
        assert_eq!(nose(&glb_vertices(&program, &glb).await), ["+Z"]);
        // The normalized FBX it showed is the entry the Check reads: a Check that takes models
        // only from the cache ships it, from the GLB's own key
        collection(&f, "props", "Props", "dg", &[totem]).await;
        assert_eq!(
            checked_model(&f).await.unwrap(),
            cached_fbx(root, &glb).unwrap()
        );

        // Asked again, the GLB comes from the cache, with no Blender to run
        set_blender(&f, &f.dir.path().join("no-blender")).await;
        assert_eq!(
            exported_view(&f.db, &id, &Snapshot::fixture()).await,
            Ok(glb.clone())
        );

        // A quarter turn is another model, which Blender makes
        let edit = json!({"facingQuarterTurns": 1});
        edits::write(&f.library.pool, &id, edit.as_object().unwrap())
            .await
            .unwrap();
        assert!(exported_view(&f.db, &id, &Snapshot::fixture())
            .await
            .unwrap_err()
            .starts_with("Blender can't show"));
        set_blender(&f, &program).await;
        let turned = exported_view(&f.db, &id, &Snapshot::fixture())
            .await
            .unwrap();
        assert_ne!(turned, glb);
        assert_eq!(nose(&glb_vertices(&program, &turned).await), ["+X"]);
        assert_eq!(
            checked_model(&f).await.unwrap(),
            cached_fbx(root, &turned).unwrap()
        );
    }
}
