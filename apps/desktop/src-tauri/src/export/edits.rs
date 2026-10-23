//! Your edits to what the game export infers (plan game-export C4, D8, D32). Each library's
//! `export_edits` table holds, per asset, a JSON object of only the values you set; everything
//! else is inferred again whenever it is shown or exported. A write reaches every member of the
//! asset's group (`structure::group_members`), and a texture's `assetName` every source of its
//! texture set (C8), in one transaction. When a group forms, the structure pass gives every
//! member the main item's edits (A7, `structure::apply_unions`).
//!
//! The edits are `assetName` for every asset, `materials` for a model (each slot's `name`,
//! `surface` and `textures`, keyed by slot name), and `layout`, `role` and `normalConvention`
//! for an image, which reach the image's own group but not the rest of its set. A model also
//! takes `rig` and `facingQuarterTurns`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::{Pool, Sqlite};

use crate::export::check;
use crate::export::infer::{self, SlotEdits, Surface};
use crate::export::names::is_pascal;
use crate::export::textures::{read_stem, Convention, LayoutEdit, Role, SourceEdits};
use crate::models::AssetKind;
use crate::structure;

/// One asset's edits: each key you set, with its value.
pub type Edits = Map<String, Value>;

pub const ASSET_NAME: &str = "assetName";
pub const MATERIALS: &str = "materials";
pub const LAYOUT: &str = "layout";
pub const ROLE: &str = "role";
pub const NORMAL_CONVENTION: &str = "normalConvention";
pub const RIG: &str = "rig";
pub const FACING_QUARTER_TURNS: &str = "facingQuarterTurns";

/// A model's rig as the manifest lists it (D20). The export infers none without an armature,
/// else generic. Humanoid, which only you set, comes with animation export (build order step 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rig {
    None,
    Generic,
}

impl Rig {
    /// Every rig an edit can hold
    pub const ALL: [Rig; 2] = [Rig::None, Rig::Generic];

    /// What the export infers for a model: generic when it has an armature
    pub fn inferred(has_armature: bool) -> Self {
        if has_armature {
            Rig::Generic
        } else {
            Rig::None
        }
    }
}

const SLOT_NAME: &str = "name";
const SURFACE: &str = "surface";
const TEXTURES: &str = "textures";

/// One `export_edits` row, with the file it is on.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Row {
    pub asset_id: String,
    pub file_path: String,
    pub file_name: String,
    pub kind: String,
    pub edits: String,
}

impl Row {
    pub fn parsed(&self) -> Edits {
        parse(&self.edits)
    }
}

/// `text` as edits. A row that isn't a JSON object holds no edit.
pub fn parse(text: &str) -> Edits {
    match serde_json::from_str(text) {
        Ok(Value::Object(edits)) => edits,
        _ => Edits::new(),
    }
}

/// The `assetName` you set in `edits`, if any.
pub fn asset_name(edits: &Edits) -> Option<String> {
    edits
        .get(ASSET_NAME)
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The `rig` you set in `edits`, if any.
pub fn rig(edits: &Edits) -> Option<Rig> {
    edits.get(RIG).and_then(parse_rig)
}

/// `value` as a rig, the one reading of a stored or sent `rig`.
fn parse_rig(value: &Value) -> Option<Rig> {
    serde_json::from_value(value.clone()).ok()
}

/// The most quarter turns an edit holds: four make a whole turn.
pub const MAX_QUARTER_TURNS: u8 = 3;

/// The quarter turns about Y you set in `edits` (D13), if any: 0 to 3, each clockwise seen from
/// above, after the pack origin's default facing.
pub fn facing_quarter_turns(edits: &Edits) -> Option<u8> {
    edits.get(FACING_QUARTER_TURNS).and_then(parse_turns)
}

/// `value` as quarter turns, 0 to `MAX_QUARTER_TURNS`, the one reading of a stored or sent
/// `facingQuarterTurns`.
fn parse_turns(value: &Value) -> Option<u8> {
    value
        .as_u64()
        .and_then(|turns| u8::try_from(turns).ok())
        .filter(|turns| *turns <= MAX_QUARTER_TURNS)
}

/// The edits of asset `asset_id` among `rows`, empty when it has none.
pub fn of(rows: &[Row], asset_id: &str) -> Edits {
    rows.iter()
        .find(|row| row.asset_id == asset_id)
        .map(Row::parsed)
        .unwrap_or_default()
}

fn parse_surface(text: &str) -> Option<Surface> {
    match text {
        "opaque" => Some(Surface::Opaque),
        "cutout" => Some(Surface::Cutout),
        _ => None,
    }
}

fn parse_convention(text: &str) -> Option<Convention> {
    serde_json::from_value(Value::String(text.into())).ok()
}

/// Your edits to a source image's layout, role and normal-map convention in `edits` (C4).
pub fn source_edits(edits: &Edits) -> SourceEdits {
    let text = |key: &str| edits.get(key).and_then(Value::as_str);
    SourceEdits {
        layout: text(LAYOUT).and_then(LayoutEdit::parse),
        role: text(ROLE).and_then(Role::parse),
        convention: text(NORMAL_CONVENTION).and_then(parse_convention),
    }
}

/// Your edits to a model's material slots in `edits`, by slot name (C4).
pub fn slot_edits(edits: &Edits) -> BTreeMap<String, SlotEdits> {
    let Some(Value::Object(slots)) = edits.get(MATERIALS) else {
        return BTreeMap::new();
    };
    (slots.iter())
        .filter_map(|(slot, value)| {
            let value = value.as_object()?;
            let textures = match value.get(TEXTURES) {
                Some(Value::Object(textures)) => (textures.iter())
                    .filter_map(|(role, pick)| {
                        Some((Role::parse(role)?, pick.as_str().map(str::to_string)))
                    })
                    .collect(),
                _ => BTreeMap::new(),
            };
            let edits = SlotEdits {
                name: value
                    .get(SLOT_NAME)
                    .and_then(Value::as_str)
                    .map(str::to_string),
                surface: value
                    .get(SURFACE)
                    .and_then(Value::as_str)
                    .and_then(parse_surface),
                textures,
            };
            Some((slot.clone(), edits))
        })
        .collect()
}

/// Whether an asset of `kind` (as `assets.kind` holds it) is an image, which exports as a
/// texture set's source (C3).
pub fn is_image(kind: &str) -> bool {
    matches!(
        AssetKind::from_str(kind),
        AssetKind::Texture | AssetKind::Image | AssetKind::Hdri
    )
}

/// The texture set an image named `file_name` shares with the other images of its folder, as
/// the Check groups them (`infer::set_key`, C8). `T_Rock_D.png`, `T_Rock_N_DX.png` and
/// `t_rock_r.tga` are all in the set `rock`. `None` makes it a set of its own, such as an image
/// whose suffix names no role or layout.
pub fn set_key(file_name: &str) -> Option<String> {
    let stem = file_name
        .rsplit_once('.')
        .map_or(file_name, |(stem, _)| stem);
    infer::set_key(&read_stem(stem))
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Whether `row` is a source of the texture set of the image at `file_path` named `file_name`:
/// an image in the same folder in the same set, the image itself included.
pub fn in_set_of(row: &Row, file_path: &str, file_name: &str) -> bool {
    if row.file_path == file_path {
        return true;
    }
    let key = set_key(file_name);
    is_image(&row.kind)
        && dir_of(&row.file_path) == dir_of(file_path)
        && key.is_some()
        && set_key(&row.file_name) == key
}

/// The `assetName` you set for the texture set of the image at `file_path`, among `rows`: the
/// first source's by path, ignoring case, that holds one. Every source holds the same name once
/// it is written, so they disagree only after the set changed, such as when a file joins it.
pub fn set_name<'a>(
    rows: impl IntoIterator<Item = &'a Row>,
    file_path: &str,
    file_name: &str,
) -> Option<String> {
    first_name(
        rows.into_iter()
            .filter(|row| in_set_of(row, file_path, file_name)),
    )
}

/// The `assetName` of the first of `rows` by path, ignoring case, that holds one: a texture
/// set's name among its sources' rows.
pub fn first_name<'a>(rows: impl IntoIterator<Item = &'a Row>) -> Option<String> {
    let mut named: Vec<(String, &str, String)> = rows
        .into_iter()
        .filter_map(|row| {
            asset_name(&row.parsed())
                .map(|name| (row.file_path.to_lowercase(), row.file_path.as_str(), name))
        })
        .collect();
    named.sort();
    named.into_iter().next().map(|(_, _, name)| name)
}

/// Every `export_edits` row of the library, with its file. The table holds only what you set,
/// so it is small.
pub async fn read_all<'e, E>(executor: E) -> sqlx::Result<Vec<Row>>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    sqlx::query_as(
        "SELECT e.asset_id, a.file_path, a.file_name, a.kind, e.edits
         FROM export_edits e JOIN assets a ON a.id = e.asset_id
         ORDER BY a.file_path",
    )
    .fetch_all(executor)
    .await
}

/// Refuses a PascalCase `name` that isn't one (the contract's `pascalName`).
fn validate_name(name: &Value, what: &str) -> Result<(), String> {
    match name {
        Value::String(name) if is_pascal(name) => Ok(()),
        Value::String(name) => Err(format!(
            "{name:?} isn't PascalCase: a game name starts with a capital letter, then only letters and digits"
        )),
        _ => Err(format!("{what} must be text")),
    }
}

/// Refuses a `materials` edit that isn't an object of slots, each null (reset) or an object of
/// a PascalCase `name`, a `surface` and `textures`, a map from role to an asset id or null.
fn validate_materials(materials: &Value) -> Result<(), String> {
    let slots = match materials {
        Value::Null => return Ok(()),
        Value::Object(slots) => slots,
        _ => return Err("materials must map slot names to your edits".into()),
    };
    for (slot, value) in slots {
        let fields = match value {
            Value::Null => continue,
            Value::Object(fields) => fields,
            _ => {
                return Err(format!(
                    "The edits of the material slot {slot} must be an object"
                ))
            }
        };
        for (key, value) in fields {
            match key.as_str() {
                SLOT_NAME => validate_name(value, "A material's name")?,
                SURFACE if value.as_str().and_then(parse_surface).is_some() => {}
                SURFACE => return Err("A material's surface is opaque or cutout".into()),
                TEXTURES => {
                    let Value::Object(textures) = value else {
                        return Err("A material's textures must map roles to images".into());
                    };
                    for (role, pick) in textures {
                        if Role::parse(role).is_none() {
                            return Err(format!("{role} isn't a texture role"));
                        }
                        if !(pick.is_null() || pick.is_string()) {
                            return Err(format!(
                                "A material's {role} is an image's id, or null for none"
                            ));
                        }
                    }
                }
                _ => return Err(format!("{key} isn't an edit of a material")),
            }
        }
    }
    Ok(())
}

/// Refuses a `patch` holding a key that isn't an edit of an asset of `kind`, or a value an edit
/// can't take: an `assetName` must be a PascalCase string, `materials` (a model's) slots of
/// `validate_materials`, a model's `rig` a `Rig` and `facingQuarterTurns` 0 to
/// `MAX_QUARTER_TURNS`, read by the same parsers as the readers, and an image's `layout`, `role`
/// and `normalConvention` one of their names. Null resets any of them.
fn validate(kind: &str, patch: &Edits) -> Result<(), String> {
    let model = AssetKind::from_str(kind) == AssetKind::Model3d;
    let image = is_image(kind);
    for (key, value) in patch {
        let text = value.as_str();
        match key.as_str() {
            ASSET_NAME if value.is_null() => {}
            ASSET_NAME => validate_name(value, "An assetName")?,
            MATERIALS if model => validate_materials(value)?,
            RIG | FACING_QUARTER_TURNS if model && value.is_null() => {}
            RIG if model && parse_rig(value).is_some() => {}
            RIG if model => {
                let names: Vec<String> = Rig::ALL
                    .iter()
                    .map(|r| {
                        serde_json::to_value(r)
                            .expect("a rig serializes")
                            .to_string()
                    })
                    .map(|name| name.trim_matches('"').to_string())
                    .collect();
                return Err(format!("A rig must be {}", names.join(" or ")));
            }
            FACING_QUARTER_TURNS if model && parse_turns(value).is_some() => {}
            FACING_QUARTER_TURNS if model => {
                return Err(format!(
                    "facingQuarterTurns must be a whole number from 0 to {MAX_QUARTER_TURNS}"
                ))
            }
            LAYOUT | ROLE | NORMAL_CONVENTION if image && value.is_null() => {}
            LAYOUT if image && text.and_then(LayoutEdit::parse).is_some() => {}
            LAYOUT if image => {
                return Err(
                    "A layout is passThrough, ORM, MaskMap, Roughness, Gloss or Metallic".into(),
                )
            }
            ROLE if image && text.and_then(Role::parse).is_some() => {}
            ROLE if image => return Err(format!("{value} isn't a texture role")),
            NORMAL_CONVENTION if image && text.and_then(parse_convention).is_some() => {}
            NORMAL_CONVENTION if image => {
                return Err("A normal-map convention is opengl or directx".into())
            }
            _ => return Err(format!("{key} isn't an export edit of this asset")),
        }
    }
    Ok(())
}

/// Refuses a texture picked in `materials` for the model at `model_path` that isn't an image of
/// the model's pack (C13). A pick already in `stored`, the model's slot edits, passes as it is:
/// a slot is written whole, so an edit to another of its fields re-sends a pick whose image may
/// have left the pack since, which the Check reports until you pick another or reset it.
async fn validate_picks(
    conn: &mut sqlx::SqliteConnection,
    model_path: &str,
    materials: &Value,
    stored: &BTreeMap<String, SlotEdits>,
) -> Result<(), String> {
    let kept = |slot: &str, role: &str, id: &str| {
        let role = Role::parse(role);
        (stored.get(slot)).is_some_and(|s| {
            role.is_some_and(|r| s.textures.get(&r) == Some(&Some(id.to_string())))
        })
    };
    let picks: Vec<&str> = (materials.as_object().into_iter().flatten())
        .filter_map(|(slot, edits)| Some((slot, edits.get(TEXTURES)?.as_object()?)))
        .flat_map(|(slot, textures)| {
            (textures.iter()).filter_map(move |(role, pick)| Some((slot, role, pick.as_str()?)))
        })
        .filter(|(slot, role, id)| !kept(slot, role, id))
        .map(|(_, _, id)| id)
        .collect();
    if picks.is_empty() {
        return Ok(());
    }
    let packs: Vec<String> = sqlx::query_scalar("SELECT path FROM folders WHERE is_pack")
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| e.to_string())?;
    let Some(pack) = check::pack_at(&packs, model_path) else {
        return Err(
            "The model is in no pack, so it has no images to pick: mark its folder as a pack"
                .into(),
        );
    };
    for id in picks {
        let file: Option<(String, String)> =
            sqlx::query_as("SELECT file_path, kind FROM assets WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(|e| e.to_string())?;
        match file {
            Some((path, kind))
                if is_image(&kind) && check::pack_at(&packs, &path) == Some(pack) => {}
            Some((path, _)) => {
                return Err(format!("{path} isn't an image of the model's pack {pack}"))
            }
            None => return Err(format!("No asset has the id {id}")),
        }
    }
    Ok(())
}

/// Applies `patch` to `edits`: each key set to its value, or removed when the value is null.
/// `materials` merges by slot, each slot's edits replaced by the patch's, or removed when null.
/// A layout that doesn't pass one role through drops your role, which it fixes, and so does a
/// layout reset: the role came with your layout, and under an inferred layout that fixes roles
/// it would be left counting for nothing, with no control to reset it.
fn apply(edits: &mut Edits, patch: &Edits) {
    for (key, value) in patch {
        match (key.as_str(), value) {
            (_, Value::Null) => {
                edits.remove(key);
            }
            (MATERIALS, Value::Object(slots)) => {
                let mut merged = match edits.remove(MATERIALS) {
                    Some(Value::Object(merged)) => merged,
                    _ => Map::new(),
                };
                for (slot, value) in slots {
                    let mut fields = value.as_object().cloned().unwrap_or_default();
                    if fields
                        .get(TEXTURES)
                        .and_then(Value::as_object)
                        .is_some_and(Map::is_empty)
                    {
                        fields.remove(TEXTURES);
                    }
                    if fields.is_empty() {
                        merged.remove(slot);
                    } else {
                        merged.insert(slot.clone(), Value::Object(fields));
                    }
                }
                if !merged.is_empty() {
                    edits.insert(MATERIALS.into(), Value::Object(merged));
                }
            }
            _ => {
                edits.insert(key.clone(), value.clone());
            }
        }
    }
    let layout = edits
        .get(LAYOUT)
        .and_then(Value::as_str)
        .and_then(LayoutEdit::parse);
    let layout_reset = patch.get(LAYOUT).is_some_and(Value::is_null);
    if layout_reset || matches!(layout, Some(LayoutEdit::Fixed(_))) {
        edits.remove(ROLE);
    }
}

/// Writes your edit `patch` to asset `asset_id` (C4): each key set to its value, or reset to
/// inferred when the value is null. A group's member stands for its main item. It reaches every
/// member of the item's group, and an image's `assetName` every source of its texture set and
/// their groups, in one transaction. A row left with no edit is deleted, so the table holds only
/// what you set. Returns the ids it wrote.
pub async fn write(
    pool: &Pool<Sqlite>,
    asset_id: &str,
    patch: &Edits,
) -> Result<Vec<String>, String> {
    // Immediate, so the read-then-write waits out the preview queue's writes on its busy
    // timeout, rather than failing to upgrade its read lock
    let mut tx = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(|e| e.to_string())?;
    // A member stands for its main item, whose set is the one the Check uses
    let (item_id, file_path, file_name, kind): (String, String, String, String) = sqlx::query_as(
        "SELECT id, file_path, file_name, kind FROM assets
             WHERE id = (SELECT COALESCE(group_id, id) FROM assets WHERE id = ?)",
    )
    .bind(asset_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| e.to_string())?
    .ok_or_else(|| format!("No asset has the id {asset_id}"))?;
    validate(&kind, patch)?;
    if let Some(materials) = patch.get(MATERIALS) {
        let stored: Option<String> =
            sqlx::query_scalar("SELECT edits FROM export_edits WHERE asset_id = ?")
                .bind(&item_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        let stored = slot_edits(&stored.as_deref().map(parse).unwrap_or_default());
        validate_picks(&mut tx, &file_path, materials, &stored).await?;
    }
    let own: BTreeSet<String> = structure::group_members(&mut *tx, &item_id)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    // An image's name is its set's; every other edit is the item's own
    let sources: Vec<String> = if is_image(&kind) && patch.contains_key(ASSET_NAME) {
        let folder = match dir_of(&file_path) {
            "" => String::new(),
            dir => format!("{dir}/"),
        };
        let images: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT id, file_name, kind FROM assets WHERE file_path = ?1 || file_name",
        )
        .bind(&folder)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        match set_key(&file_name) {
            None => vec![item_id],
            key => images
                .into_iter()
                .filter(|(_, name, kind)| is_image(kind) && set_key(name) == key)
                .map(|(id, _, _)| id)
                .collect(),
        }
    } else {
        vec![item_id]
    };
    let mut named = BTreeSet::new();
    for source in &sources {
        named.extend(
            structure::group_members(&mut *tx, source)
                .await
                .map_err(|e| e.to_string())?,
        );
    }
    let (name_patch, own_patch): (Edits, Edits) =
        (patch.clone().into_iter()).partition(|(key, _)| key == ASSET_NAME && is_image(&kind));
    let targets: BTreeSet<String> = (own.iter().filter(|_| !own_patch.is_empty()))
        .chain(named.iter().filter(|_| !name_patch.is_empty()))
        .cloned()
        .collect();

    for id in &targets {
        let text: Option<String> =
            sqlx::query_scalar("SELECT edits FROM export_edits WHERE asset_id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        let mut edits = text.as_deref().map(parse).unwrap_or_default();
        if own.contains(id) {
            apply(&mut edits, &own_patch);
        }
        if named.contains(id) {
            apply(&mut edits, &name_patch);
        }
        let written = if edits.is_empty() {
            sqlx::query("DELETE FROM export_edits WHERE asset_id = ?").bind(id)
        } else {
            sqlx::query(
                "INSERT INTO export_edits (asset_id, edits) VALUES (?, ?)
                 ON CONFLICT(asset_id) DO UPDATE SET edits = excluded.edits",
            )
            .bind(id)
            .bind(Value::Object(edits).to_string())
        };
        written.execute(&mut *tx).await.map_err(|e| e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(targets.into_iter().collect())
}

/// The assets an Export view should count as edited after `patch` wrote `written` to asset
/// `asset_id`: those ids, and for an image's layout, role or normal-map convention, the models
/// of its pack. Those edits feed the inference of the whole pack (C7), so they can bind the
/// image to a model's material, or unbind it, while the image itself isn't in the view's Check.
pub async fn touched(
    pool: &Pool<Sqlite>,
    asset_id: &str,
    patch: &Edits,
    written: Vec<String>,
) -> Result<Vec<String>, String> {
    let mut ids = written;
    if ![LAYOUT, ROLE, NORMAL_CONVENTION]
        .iter()
        .any(|key| patch.contains_key(*key))
    {
        return Ok(ids);
    }
    let file_path: String = sqlx::query_scalar("SELECT file_path FROM assets WHERE id = ?")
        .bind(asset_id)
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?;
    let packs: Vec<String> = sqlx::query_scalar("SELECT path FROM folders WHERE is_pack")
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?;
    let Some(pack) = check::pack_at(&packs, &file_path) else {
        return Ok(ids);
    };
    // Each model as its grid item, which is what a Check lists
    let models: Vec<(String, String)> = sqlx::query_as(
        "SELECT COALESCE(group_id, id), file_path FROM assets
         WHERE kind = ?2 AND (?1 = '' OR (file_path >= ?1 || '/' AND file_path < ?1 || '0'))",
    )
    .bind(pack)
    .bind(AssetKind::Model3d.as_str())
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;
    for (item, path) in models {
        if check::pack_at(&packs, &path) == Some(pack) && !ids.contains(&item) {
            ids.push(item);
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memory_library_pool;
    use crate::export::textures::Layout;
    use crate::structure::tests::{car_kit, insert_path, unmounted, SEDAN};
    use serde_json::json;

    fn patch(value: Value) -> Edits {
        value.as_object().unwrap().clone()
    }

    async fn rows(pool: &Pool<Sqlite>) -> Vec<(String, String)> {
        sqlx::query_as("SELECT asset_id, edits FROM export_edits ORDER BY asset_id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_write_reaches_every_group_member_and_a_reset_deletes_the_rows() {
        let pool = car_kit().await;
        let written = write(&pool, SEDAN[2], &patch(json!({"assetName": "Sedan"})))
            .await
            .unwrap();
        assert_eq!(written, SEDAN);
        let row = r#"{"assetName":"Sedan"}"#.to_string();
        assert_eq!(
            rows(&pool).await,
            SEDAN.map(|id| (id.to_string(), row.clone()))
        );

        write(&pool, SEDAN[0], &patch(json!({"assetName": null})))
            .await
            .unwrap();
        assert_eq!(rows(&pool).await, []);
    }

    #[tokio::test]
    async fn a_texture_name_reaches_every_source_of_its_set_and_their_groups() {
        let pool = memory_library_pool().await;
        for path in [
            "Pack/Textures/Default/T_Rock_D.png",
            "Pack/Textures/Default/T_Rock_N_DX.png",
            "Pack/Textures/Default/t_rock_r.tga",
            "Pack/Textures/Default/T_Rockface_D.png",
            "Pack/Textures/Default/Rock.fbx",
            // A scale copy of the set's BaseColor: a 2D stack with it, outside the set's folder
            "Pack/Textures/Double/T_Rock_D.png",
            "Pack/Other/T_Rock_D.png",
        ] {
            insert_path(&pool, path).await;
        }
        structure::rebuild(&pool, unmounted()).await.unwrap();
        let mut stack = structure::group_members(&pool, "Pack/Textures/Double/T_Rock_D.png")
            .await
            .unwrap();
        stack.sort();
        assert_eq!(
            stack,
            [
                "Pack/Textures/Default/T_Rock_D.png",
                "Pack/Textures/Double/T_Rock_D.png",
            ],
            "the copies form a stack"
        );

        let written = write(
            &pool,
            "Pack/Textures/Default/T_Rock_N_DX.png",
            &patch(json!({"assetName": "Boulder"})),
        )
        .await
        .unwrap();
        // Written from the stack's copy, it reaches the main item's set all the same
        let from_copy = write(
            &pool,
            "Pack/Textures/Double/T_Rock_D.png",
            &patch(json!({"assetName": "Boulder"})),
        )
        .await
        .unwrap();
        assert_eq!(from_copy, written);
        let expected = [
            "Pack/Textures/Default/T_Rock_D.png",
            "Pack/Textures/Default/T_Rock_N_DX.png",
            "Pack/Textures/Default/t_rock_r.tga",
            "Pack/Textures/Double/T_Rock_D.png",
        ];
        assert_eq!(written, expected);
        let row = r#"{"assetName":"Boulder"}"#.to_string();
        assert_eq!(
            rows(&pool).await,
            expected.map(|id| (id.to_string(), row.clone()))
        );
        let all = read_all(&pool).await.unwrap();
        assert_eq!(
            set_name(&all, "Pack/Textures/Default/T_Rock_M.png", "T_Rock_M.png").as_deref(),
            Some("Boulder")
        );
        assert_eq!(
            set_name(&all, "Pack/Other/T_Rock_D.png", "T_Rock_D.png"),
            None
        );
    }

    #[test]
    fn the_first_source_by_path_wins_when_a_set_disagrees() {
        let row = |path: &str, name: Option<&str>| {
            let file_name = path.rsplit('/').next().unwrap().to_string();
            Row {
                asset_id: path.into(),
                file_path: path.into(),
                file_name,
                kind: "texture".into(),
                edits: match name {
                    Some(name) => json!({ "assetName": name }).to_string(),
                    None => "{}".into(),
                },
            }
        };
        let rows = [
            row("P/T_Rock_N.png", Some("Late")),
            row("P/a_extra.png", Some("Other")),
            row("P/t_rock_D.png", Some("Early")),
            row("P/T_Rock_AO.png", None),
        ];
        assert_eq!(
            set_name(&rows, "P/T_Rock_N.png", "T_Rock_N.png").as_deref(),
            Some("Early")
        );
    }

    #[tokio::test]
    async fn a_name_that_isnt_pascal_case_or_an_unknown_edit_is_refused() {
        let pool = car_kit().await;
        for (value, error) in [
            (
                json!({"assetName": "sedan car"}),
                "\"sedan car\" isn't PascalCase: a game name starts with a capital letter, then only letters and digits",
            ),
            (json!({"assetName": 3}), "An assetName must be text"),
            (json!({"rig": "humanoid"}), "A rig must be none or generic"),
            (
                json!({"facingQuarterTurns": 4}),
                "facingQuarterTurns must be a whole number from 0 to 3",
            ),
            (
                json!({"facingQuarterTurns": 1.5}),
                "facingQuarterTurns must be a whole number from 0 to 3",
            ),
            (json!({"colour": "red"}), "colour isn't an export edit of this asset"),
        ] {
            assert_eq!(
                write(&pool, SEDAN[0], &patch(value)).await,
                Err(error.to_string())
            );
        }
        assert_eq!(
            write(&pool, "gone", &patch(json!({"assetName": "Gone"}))).await,
            Err("No asset has the id gone".to_string())
        );
        assert_eq!(rows(&pool).await, []);
    }

    #[tokio::test]
    async fn material_edits_merge_by_slot_and_reset_by_slot() {
        let pool = car_kit().await;
        let materials = |value: Value| patch(json!({ "materials": value }));
        write(
            &pool,
            SEDAN[1],
            &materials(json!({"Body": {"name": "Paint"}})),
        )
        .await
        .unwrap();
        let both = json!({"Body": {"surface": "cutout"}, "Glass": {"surface": "cutout"}});
        write(&pool, SEDAN[0], &materials(both)).await.unwrap();
        // A slot's edits are replaced whole, and other slots kept
        let row = r#"{"materials":{"Body":{"surface":"cutout"},"Glass":{"surface":"cutout"}}}"#;
        assert_eq!(
            rows(&pool).await,
            SEDAN.map(|id| (id.to_string(), row.to_string()))
        );
        write(
            &pool,
            SEDAN[2],
            &materials(json!({"Body": null, "Glass": {"textures": {}}})),
        )
        .await
        .unwrap();
        assert_eq!(rows(&pool).await, []);
    }

    #[tokio::test]
    async fn an_images_layout_reaches_its_group_and_its_name_the_whole_set() {
        let pool = memory_library_pool().await;
        for path in [
            "Pack/Textures/Default/T_Rock_D.png",
            "Pack/Textures/Default/T_Rock_N_DX.png",
            "Pack/Textures/Default/T_Rock_R.png",
            "Pack/Textures/Double/T_Rock_D.png",
        ] {
            insert_path(&pool, path).await;
        }
        structure::rebuild(&pool, unmounted()).await.unwrap();

        // A layout with several outputs fixes the role, so yours is dropped
        let written = write(
            &pool,
            "Pack/Textures/Double/T_Rock_D.png",
            &patch(json!({"layout": "ORM", "role": "Height"})),
        )
        .await
        .unwrap();
        assert_eq!(
            written,
            [
                "Pack/Textures/Default/T_Rock_D.png",
                "Pack/Textures/Double/T_Rock_D.png",
            ]
        );
        let written = write(
            &pool,
            "Pack/Textures/Default/T_Rock_N_DX.png",
            &patch(json!({"assetName": "Boulder", "normalConvention": "opengl"})),
        )
        .await
        .unwrap();
        assert_eq!(written.len(), 4);
        assert_eq!(
            rows(&pool).await,
            [
                (
                    "Pack/Textures/Default/T_Rock_D.png".into(),
                    r#"{"assetName":"Boulder","layout":"ORM"}"#.into()
                ),
                (
                    "Pack/Textures/Default/T_Rock_N_DX.png".into(),
                    r#"{"assetName":"Boulder","normalConvention":"opengl"}"#.into()
                ),
                (
                    "Pack/Textures/Default/T_Rock_R.png".into(),
                    r#"{"assetName":"Boulder"}"#.into()
                ),
                (
                    "Pack/Textures/Double/T_Rock_D.png".into(),
                    r#"{"assetName":"Boulder","layout":"ORM"}"#.into()
                ),
            ]
        );
        let all = read_all(&pool).await.unwrap();
        assert_eq!(
            source_edits(&all[1].parsed()),
            SourceEdits {
                layout: None,
                role: None,
                convention: Some(Convention::OpenGl)
            }
        );
        assert_eq!(
            source_edits(&all[0].parsed()).layout,
            Some(LayoutEdit::Fixed(Layout::Orm))
        );
    }

    #[tokio::test]
    async fn a_picked_texture_must_be_an_image_of_the_models_pack() {
        use crate::export::test_fixture::{fixture, id_of};
        let f = fixture().await;
        let pool = &f.library.pool;
        let model = id_of(pool, "Dungeon Props/Models/SM_Crate.fbx").await;
        let pick = |id: &str| patch(json!({"materials": {"Planks": {"textures": {"Normal": id}}}}));
        let normal = id_of(pool, "Dungeon Props/Textures/4K/T_Crate_N.png").await;
        write(pool, &model, &pick(&normal)).await.unwrap();
        let all = read_all(pool).await.unwrap();
        let slots = slot_edits(&all[0].parsed());
        assert_eq!(
            slots["Planks"].textures,
            BTreeMap::from([(Role::Normal, Some(normal.clone()))])
        );

        for (path, error) in [
            (
                "Kenney Crates/crate_wood.png",
                "Kenney Crates/crate_wood.png isn't an image of the model's pack Dungeon Props",
            ),
            (
                "Dungeon Props/Models/SM_Chest.obj",
                "Dungeon Props/Models/SM_Chest.obj isn't an image of the model's pack Dungeon Props",
            ),
        ] {
            let id = id_of(pool, path).await;
            assert_eq!(write(pool, &model, &pick(&id)).await, Err(error.into()));
        }
        assert_eq!(
            write(pool, &model, &pick("gone")).await,
            Err("No asset has the id gone".into())
        );
        // None needs no image
        let none = patch(json!({"materials": {"Planks": {"textures": {"Normal": null}}}}));
        write(pool, &model, &none).await.unwrap();
        let all = read_all(pool).await.unwrap();
        assert_eq!(
            slot_edits(&all[0].parsed())["Planks"].textures,
            BTreeMap::from([(Role::Normal, None)])
        );
    }

    #[tokio::test]
    async fn edits_an_asset_cant_take_or_values_they_cant_hold_are_refused() {
        let pool = memory_library_pool().await;
        for path in ["Pack/T_Rock_D.png", "Pack/rock.fbx"] {
            insert_path(&pool, path).await;
        }
        let (image, model) = ("Pack/T_Rock_D.png", "Pack/rock.fbx");
        for (id, value, error) in [
            (model, json!({"layout": "ORM"}), "layout isn't an export edit of this asset"),
            (model, json!({"role": null}), "role isn't an export edit of this asset"),
            (image, json!({"materials": {}}), "materials isn't an export edit of this asset"),
            (
                image,
                json!({"layout": "Normal"}),
                "A layout is passThrough, ORM, MaskMap, Roughness, Gloss or Metallic",
            ),
            (image, json!({"role": "Albedo"}), "\"Albedo\" isn't a texture role"),
            (
                image,
                json!({"normalConvention": "dx"}),
                "A normal-map convention is opengl or directx",
            ),
            (
                model,
                json!({"materials": {"Body": {"name": "red paint"}}}),
                "\"red paint\" isn't PascalCase: a game name starts with a capital letter, then only letters and digits",
            ),
            (
                model,
                json!({"materials": {"Body": {"surface": "transparent"}}}),
                "A material's surface is opaque or cutout",
            ),
            (
                model,
                json!({"materials": {"Body": {"textures": {"Albedo": null}}}}),
                "Albedo isn't a texture role",
            ),
            (
                model,
                json!({"materials": {"Body": {"textures": {"Normal": 3}}}}),
                "A material's Normal is an image's id, or null for none",
            ),
            (
                model,
                json!({"materials": {"Body": {"rig": "generic"}}}),
                "rig isn't an edit of a material",
            ),
        ] {
            assert_eq!(write(&pool, id, &patch(value)).await, Err(error.to_string()));
        }
        assert_eq!(rows(&pool).await, []);
    }

    #[tokio::test]
    async fn a_model_in_no_pack_has_no_image_to_pick() {
        let pool = memory_library_pool().await;
        for path in ["Loose/rock.fbx", "Loose/T_Rock_D.png"] {
            insert_path(&pool, path).await;
        }
        let pick =
            json!({"materials": {"Rock": {"textures": {"BaseColor": "Loose/T_Rock_D.png"}}}});
        assert_eq!(
            write(&pool, "Loose/rock.fbx", &patch(pick)).await,
            Err(
                "The model is in no pack, so it has no images to pick: mark its folder as a pack"
                    .into()
            )
        );
        assert_eq!(rows(&pool).await, []);
    }

    #[tokio::test]
    async fn a_stored_pick_rewritten_with_its_slot_passes_though_its_image_left() {
        use crate::export::test_fixture::{fixture, id_of};
        let f = fixture().await;
        let pool = &f.library.pool;
        let model = id_of(pool, "Dungeon Props/Models/SM_Crate.fbx").await;
        let normal = id_of(pool, "Dungeon Props/Textures/4K/T_Crate_N.png").await;
        let slot = |extra: Value| {
            let mut planks = json!({"textures": {"Normal": normal}});
            planks
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            patch(json!({"materials": {"Planks": planks}}))
        };
        write(pool, &model, &slot(json!({}))).await.unwrap();
        sqlx::query("DELETE FROM assets WHERE id = ?")
            .bind(&normal)
            .execute(pool)
            .await
            .unwrap();
        // Accepting cutout re-sends the slot, its stale pick included, which is kept
        write(pool, &model, &slot(json!({"surface": "cutout"})))
            .await
            .unwrap();
        let all = read_all(pool).await.unwrap();
        let stored = &slot_edits(&all[0].parsed())["Planks"];
        assert_eq!(stored.textures[&Role::Normal], Some(normal.clone()));
        assert_eq!(stored.surface, Some(Surface::Cutout));
        // A new pick of a missing image is still refused
        let other = patch(json!({"materials": {"Planks": {"textures": {"BaseColor": "gone"}}}}));
        assert_eq!(
            write(pool, &model, &other).await,
            Err("No asset has the id gone".into())
        );
    }

    #[tokio::test]
    async fn a_layout_reset_takes_your_role_with_it() {
        let pool = memory_library_pool().await;
        insert_path(&pool, "Pack/T_Rock_R.png").await;
        let id = "Pack/T_Rock_R.png";
        // A Roughness map made one role, then its layout reset to the inferred Roughness
        let one_role = patch(json!({"layout": "passThrough", "role": "Height"}));
        write(&pool, id, &one_role).await.unwrap();
        let both = r#"{"layout":"passThrough","role":"Height"}"#.to_string();
        assert_eq!(rows(&pool).await, [(id.to_string(), both)]);
        write(&pool, id, &patch(json!({"layout": null})))
            .await
            .unwrap();
        assert_eq!(rows(&pool).await, [], "no role is left behind");

        // Your other edits stay
        let edits =
            json!({"layout": "passThrough", "role": "Height", "normalConvention": "directx"});
        write(&pool, id, &patch(edits)).await.unwrap();
        write(&pool, id, &patch(json!({"layout": null})))
            .await
            .unwrap();
        let left = r#"{"normalConvention":"directx"}"#.to_string();
        assert_eq!(rows(&pool).await, [(id.to_string(), left)]);
    }

    #[tokio::test]
    async fn a_source_edit_touches_the_models_of_its_pack() {
        use crate::export::test_fixture::{fixture, id_of};
        let f = fixture().await;
        let pool = &f.library.pool;
        let metallic = id_of(pool, "Dungeon Props/Textures/T_Iron_M.png").await;
        let one_role = patch(json!({"layout": "passThrough", "role": "BaseColor"}));
        let written = write(pool, &metallic, &one_role).await.unwrap();
        let ids = touched(pool, &metallic, &one_role, written.clone())
            .await
            .unwrap();
        assert!(ids.starts_with(&written));
        // SM_Gate's materials may now bind it; SM_Chest is listed as its group's main item
        let gate = id_of(pool, "Dungeon Props/Models/SM_Gate.fbx").await;
        let chest = id_of(pool, "Dungeon Props/Models/SM_Chest.fbx").await;
        let chest_obj = id_of(pool, "Dungeon Props/Models/SM_Chest.obj").await;
        assert!(ids.contains(&gate));
        assert!(ids.contains(&chest));
        assert!(!ids.contains(&chest_obj), "a member is never a Check's row");
        // Models of another pack are untouched
        let log = id_of(pool, "Unity Moss/Models/SM_Log.fbx").await;
        assert!(!ids.contains(&log));

        // A name reaches only what it wrote
        let name = patch(json!({"assetName": "Iron"}));
        let written = write(pool, &metallic, &name).await.unwrap();
        assert_eq!(
            touched(pool, &metallic, &name, written.clone())
                .await
                .unwrap(),
            written
        );
    }

    #[tokio::test]
    async fn a_models_rig_and_facing_reach_its_group_and_reset_on_their_own() {
        let pool = car_kit().await;
        write(
            &pool,
            SEDAN[1],
            &patch(json!({"rig": "generic", "facingQuarterTurns": 3})),
        )
        .await
        .unwrap();
        write(&pool, SEDAN[0], &patch(json!({"assetName": "Sedan"})))
            .await
            .unwrap();
        let all = read_all(&pool).await.unwrap();
        for id in SEDAN {
            let edits = of(&all, id);
            assert_eq!(rig(&edits), Some(Rig::Generic), "{id}");
            assert_eq!(facing_quarter_turns(&edits), Some(3), "{id}");
        }

        write(&pool, SEDAN[2], &patch(json!({"facingQuarterTurns": null})))
            .await
            .unwrap();
        let all = read_all(&pool).await.unwrap();
        let edits = of(&all, SEDAN[0]);
        assert_eq!(facing_quarter_turns(&edits), None);
        assert_eq!(rig(&edits), Some(Rig::Generic));
        assert_eq!(asset_name(&edits).as_deref(), Some("Sedan"));
        assert_eq!(of(&all, "gone"), Edits::new());
    }

    /// Validation and the readers share one parser each, so every value a write accepts is read
    /// back, whatever rigs or turns are added later.
    #[tokio::test]
    async fn every_rig_and_turn_a_write_accepts_is_read_back() {
        let pool = car_kit().await;
        for rig in Rig::ALL {
            let edit = json!({ "rig": rig });
            write(&pool, SEDAN[0], &patch(edit)).await.unwrap();
            let edits = of(&read_all(&pool).await.unwrap(), SEDAN[0]);
            assert_eq!(super::rig(&edits), Some(rig));
        }
        for turns in 0..=MAX_QUARTER_TURNS {
            let edit = json!({ "facingQuarterTurns": turns });
            write(&pool, SEDAN[0], &patch(edit)).await.unwrap();
            let edits = of(&read_all(&pool).await.unwrap(), SEDAN[0]);
            assert_eq!(facing_quarter_turns(&edits), Some(turns));
        }
        let edit = json!({ "facingQuarterTurns": 260 });
        assert!(write(&pool, SEDAN[0], &patch(edit)).await.is_err());
    }

    #[tokio::test]
    async fn an_image_has_no_rig_or_facing() {
        let pool = memory_library_pool().await;
        insert_path(&pool, "Pack/T_Rock_D.png").await;
        structure::rebuild(&pool, unmounted()).await.unwrap();
        for value in [json!({"rig": "none"}), json!({"facingQuarterTurns": 1})] {
            let key = value.as_object().unwrap().keys().next().unwrap().clone();
            assert_eq!(
                write(&pool, "Pack/T_Rock_D.png", &patch(value)).await,
                Err(format!("{key} isn't an export edit of this asset"))
            );
        }
        assert_eq!(rows(&pool).await, []);
    }
}
