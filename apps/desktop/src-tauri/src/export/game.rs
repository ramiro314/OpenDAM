//! What the detail panel's Game tab shows for an asset (plan game-export C13): what it exports
//! as, its `assetName`, inferred and yours (C4, C5), and the problems the Checks of the domains
//! holding it find with it. A model shows each material slot's name, surface and texture per
//! role, and its pack's images for the texture pickers. A texture shows its own layout, role and
//! normal-map convention, and its texture set. Each value is inferred, or yours when you set it.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;

use crate::db::Database;
pub use crate::export::check::Field;
use crate::export::check::{self, Inspection, ItemRow, Kind, PackImage, Problem, TextureGame};
use crate::export::fbx_facts;
use crate::export::infer::{Binding, Surface};
use crate::export::textures::Role;
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
    /// A model's materials (C7), in slot order
    pub materials: Vec<MaterialDetail>,
    /// A model's pack's images, by path, which its texture pickers offer
    pub images: Arc<Vec<PackImage>>,
    /// A texture's own layout, role and normal-map convention, and its texture set
    pub texture: Option<TextureGame>,
}

/// One material slot of a model, as the Game tab shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaterialDetail {
    pub slot: String,
    /// The slot the export adds for a kept mesh with no material (C9 step 2)
    pub added: bool,
    pub name: Field<String>,
    pub surface: Field<Surface>,
    /// Its BaseColor has a cut-out alpha: a suggestion you accept with one click (D23)
    pub cutout_suggested: bool,
    /// Every role, in the contract's order
    pub textures: Vec<RoleTexture>,
}

/// One role of a material: its texture, inferred and yours.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleTexture {
    pub role: Role,
    pub inferred: Option<TexturePick>,
    /// Whether you chose this role's texture, or none: `value` is then yours
    pub yours: bool,
    /// The image you picked, by asset id, even when the export can't use it (it left the pack,
    /// or its layout no longer gives the role), so a write of the slot keeps it
    pub picked: Option<String>,
    /// What the export uses, `None` for no texture
    pub value: Option<TexturePick>,
}

/// A texture set filling a role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TexturePick {
    /// The source the role is bound by: the image you picked, else the set's first giving it
    pub asset_id: String,
    /// The set's name
    pub asset_name: String,
    /// The set's sources that give the role
    pub files: Vec<String>,
}

impl From<&Binding> for TexturePick {
    fn from(binding: &Binding) -> Self {
        TexturePick {
            asset_id: binding.source.clone(),
            asset_name: binding.asset_name.clone(),
            files: binding.files.clone(),
        }
    }
}

impl From<&infer::Material> for MaterialDetail {
    fn from(material: &infer::Material) -> Self {
        let textures = Role::ALL
            .into_iter()
            .map(|role| RoleTexture {
                role,
                inferred: material.inferred.textures.get(&role).map(TexturePick::from),
                yours: material.yours.textures.contains_key(&role),
                picked: material.yours.textures.get(&role).cloned().flatten(),
                value: material.textures.get(&role).map(TexturePick::from),
            })
            .collect();
        MaterialDetail {
            slot: material.slot.clone(),
            added: material.added,
            name: Field {
                inferred: material.inferred.name.clone(),
                yours: material.yours.name.clone(),
            },
            surface: Field {
                inferred: material.inferred.surface,
                yours: material.yours.surface,
            },
            cutout_suggested: material.cutout_suggested,
            textures,
        }
    }
}

/// The Game tab of asset `asset_id`, reading FBX facts with `facts`, or `None` when it exports
/// as neither a model, an animation nor a texture. A group's member stands for its main item.
/// Its kind comes from the Check's rule (`check::exported_kind`). Each domain holding it is
/// checked as the Export view's Check does, without staging, and its name, materials and texture
/// set are the ones the first of those Checks, by domain name, exports it with. In no domain,
/// they are a Check's of it alone (`check::inspect_alone`).
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
    let mut shown: Option<Inspection> = None;
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
        for problem in &inspection.report.problems {
            let names_item = problem
                .assets
                .iter()
                .any(|a| a.library_id == library.id && a.asset_id == item.id);
            if names_item && !problems.contains(problem) {
                problems.push(problem.clone());
            }
        }
        shown.get_or_insert(inspection);
    }
    let shown = match shown {
        Some(inspection) => inspection,
        None => check::inspect_alone(library.clone(), &item.id, facts, memo).await?,
    };
    let (materials, images) = match shown.models.get(&item.id) {
        Some(model) => (
            model.materials.iter().map(MaterialDetail::from).collect(),
            model.images.clone(),
        ),
        None => (Vec::new(), Arc::default()),
    };
    Ok(Some(GameDetail {
        asset_id: item.id.clone(),
        kind,
        asset_name,
        domains: holding,
        problems,
        materials,
        images,
        texture: shown.textures.get(&item.id).cloned(),
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
    use crate::export::textures::Convention;
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

    fn pick_files(pick: &Option<TexturePick>) -> Vec<&str> {
        pick.iter()
            .flat_map(|p| p.files.iter().map(String::as_str))
            .collect()
    }

    #[tokio::test]
    async fn a_model_shows_each_slot_inferred_and_yours_and_its_packs_images() {
        let f = fixture().await;
        let model = "Dungeon Props/Models/SM_Crate.fbx";
        collection(&f, "props", "Props", "dg", &[model]).await;
        let shown = detail_of(&f, model).await.unwrap();
        let [planks] = shown.materials.as_slice() else {
            panic!("{:?}", shown.materials)
        };
        assert_eq!(planks.slot, "Planks");
        assert!(!planks.added);
        assert_eq!(
            planks.name,
            Field {
                inferred: "Planks".into(),
                yours: None
            }
        );
        assert_eq!(
            planks.surface,
            Field {
                inferred: Surface::Opaque,
                yours: None
            }
        );
        let roles: Vec<Role> = planks.textures.iter().map(|t| t.role).collect();
        assert_eq!(roles, Role::ALL, "every role, each with a picker");
        let normal = &planks.textures[1];
        assert_eq!(
            pick_files(&normal.inferred),
            ["Dungeon Props/Textures/1K/T_Crate_N.png"]
        );
        assert!(!normal.yours);
        assert_eq!(normal.value, normal.inferred);
        assert_eq!(planks.textures[0].value, None);
        // The pickers offer the pack's images, each with the roles it gives
        let crate_n: Vec<(&str, &[Role])> = shown
            .images
            .iter()
            .filter(|i| i.file_path.ends_with("T_Crate_N.png"))
            .map(|i| (i.file_path.as_str(), i.roles.as_slice()))
            .collect();
        assert_eq!(
            crate_n,
            [
                (
                    "Dungeon Props/Textures/1K/T_Crate_N.png",
                    &[Role::Normal][..]
                ),
                (
                    "Dungeon Props/Textures/4K/T_Crate_N.png",
                    &[Role::Normal][..]
                ),
            ]
        );
        assert!(shown
            .images
            .iter()
            .all(|i| i.file_path.starts_with("Dungeon Props/")));

        let id = id_of(&f.library.pool, model).await;
        let crate_4k = id_of(&f.library.pool, "Dungeon Props/Textures/4K/T_Crate_N.png").await;
        let edit = json!({"materials": {"Planks": {
            "name": "Boards",
            "surface": "cutout",
            "textures": {"Normal": crate_4k, "BaseColor": null}
        }}});
        edits::write(&f.library.pool, &id, edit.as_object().unwrap())
            .await
            .unwrap();
        let shown = detail_of(&f, model).await.unwrap();
        let planks = &shown.materials[0];
        assert_eq!(planks.name.yours.as_deref(), Some("Boards"));
        assert_eq!(planks.surface.value(), Surface::Cutout);
        let normal = &planks.textures[1];
        assert!(normal.yours);
        assert_eq!(
            pick_files(&normal.value),
            ["Dungeon Props/Textures/4K/T_Crate_N.png"]
        );
        assert_eq!(normal.value.as_ref().unwrap().asset_id, crate_4k);
        assert_eq!(
            pick_files(&normal.inferred),
            ["Dungeon Props/Textures/1K/T_Crate_N.png"]
        );
        let base = &planks.textures[0];
        assert!(base.yours, "none, chosen, is yours");
        assert_eq!((base.inferred.clone(), base.value.clone()), (None, None));
    }

    #[tokio::test]
    async fn a_model_in_no_domain_shows_its_materials_and_the_added_slot() {
        let f = fixture().await;
        let guard = detail_of(&f, "Dungeon Props/Models/SK_Guard.fbx")
            .await
            .unwrap();
        assert_eq!(guard.domains, Vec::<String>::new());
        assert_eq!(guard.problems, []);
        assert_eq!(guard.materials.len(), 1);
        assert!(guard.materials[0].cutout_suggested);

        let boulder = detail_of(&f, "Dungeon Props/Models/SM_Boulder.fbx")
            .await
            .unwrap();
        let added = &boulder.materials[0];
        assert_eq!((added.slot.as_str(), added.added), ("OpenDAMDefault", true));
        assert_eq!(
            pick_files(&added.textures[0].value),
            ["Dungeon Props/Models/SM_Boulder.png"]
        );
    }

    #[tokio::test]
    async fn a_texture_shows_its_set_and_its_own_layout_role_and_convention() {
        let f = fixture().await;
        let normal = "Dungeon Props/Rocks/T_Rock_N_DX.png";
        let rough = "Dungeon Props/Rocks/T_Rock_R.png";
        let shown = detail_of(&f, normal).await.unwrap();
        assert!(shown.materials.is_empty());
        let texture = shown.texture.unwrap();
        assert_eq!(
            texture.layout,
            Field {
                inferred: "passThrough",
                yours: None
            }
        );
        assert_eq!(
            texture.role,
            Some(Field {
                inferred: Role::Normal,
                yours: None
            })
        );
        assert_eq!(
            texture.normal_convention,
            Field {
                inferred: Convention::DirectX,
                yours: None
            }
        );
        let set: Vec<(&str, &str)> = texture
            .set
            .iter()
            .map(|s| (s.file_path.as_str(), s.layout))
            .collect();
        assert_eq!(
            set,
            [
                ("Dungeon Props/Rocks/T_Rock_M.png", "Metallic"),
                (normal, "Normal"),
                (rough, "Roughness"),
            ]
        );
        assert_eq!(texture.outputs, [Role::Normal, Role::MetallicSmoothness]);

        // A Roughness map merges into its set's MetallicSmoothness: no role of its own
        let texture = detail_of(&f, rough).await.unwrap().texture.unwrap();
        assert_eq!(texture.layout.inferred, "Roughness");
        assert_eq!(texture.role, None);

        let id = id_of(&f.library.pool, rough).await;
        let edit = json!({"layout": "passThrough", "normalConvention": "directx"});
        edits::write(&f.library.pool, &id, edit.as_object().unwrap())
            .await
            .unwrap();
        let texture = detail_of(&f, rough).await.unwrap().texture.unwrap();
        assert_eq!(texture.layout.yours, Some("passThrough"));
        assert_eq!(
            texture.role,
            Some(Field {
                inferred: Role::BaseColor,
                yours: None
            })
        );
        assert_eq!(texture.normal_convention.yours, Some(Convention::DirectX));
        assert_eq!(
            texture.outputs,
            [Role::BaseColor, Role::Normal, Role::MetallicSmoothness]
        );
    }

    #[tokio::test]
    async fn a_pick_the_export_cant_use_still_shows_as_yours() {
        let f = fixture().await;
        let model = "Dungeon Props/Models/SM_Crate.fbx";
        let id = id_of(&f.library.pool, model).await;
        let normal = id_of(&f.library.pool, "Dungeon Props/Textures/4K/T_Crate_N.png").await;
        let edit = json!({"materials": {"Planks": {"textures": {"Normal": normal}}}});
        edits::write(&f.library.pool, &id, edit.as_object().unwrap())
            .await
            .unwrap();
        sqlx::query("DELETE FROM assets WHERE id = ?")
            .bind(&normal)
            .execute(&f.library.pool)
            .await
            .unwrap();
        let shown = detail_of(&f, model).await.unwrap();
        let role = &shown.materials[0].textures[1];
        assert_eq!(role.role, Role::Normal);
        assert!(role.yours);
        assert_eq!(role.picked.as_deref(), Some(normal.as_str()));
        assert_eq!(role.value, None, "the export binds nothing");
        // A role you set to none has no pick
        assert_eq!(shown.materials[0].textures[0].picked, None);
    }
}
