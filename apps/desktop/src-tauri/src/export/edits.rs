//! Your edits to what the game export infers (plan game-export C4, D8, D32). Each library's
//! `export_edits` table holds, per asset, a JSON object of only the values you set; everything
//! else is inferred again whenever it is shown or exported. A write reaches every member of the
//! asset's group (`structure::group_members`), and a texture's `assetName` every source of its
//! texture set (C8), in one transaction. When a group forms, the structure pass gives every
//! member the main item's edits (A7, `structure::apply_unions`).
//!
//! So far the edits are `assetName`, and a model's `rig` and `facingQuarterTurns`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::{Pool, Sqlite};

use crate::export::infer;
use crate::export::names::is_pascal;
use crate::export::textures::read_stem;
use crate::models::AssetKind;
use crate::structure;

/// One asset's edits: each key you set, with its value.
pub type Edits = Map<String, Value>;

pub const ASSET_NAME: &str = "assetName";
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
    /// What the export infers for a model: generic when it has an armature
    pub fn inferred(has_armature: bool) -> Self {
        if has_armature {
            Rig::Generic
        } else {
            Rig::None
        }
    }
}

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
    edits
        .get(RIG)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// The quarter turns about Y you set in `edits` (D13), if any: 0 to 3, each clockwise seen from
/// above, after the pack origin's default facing.
pub fn facing_quarter_turns(edits: &Edits) -> Option<u8> {
    edits
        .get(FACING_QUARTER_TURNS)
        .and_then(Value::as_u64)
        .filter(|turns| *turns <= 3)
        .map(|turns| turns as u8)
}

/// The edits of asset `asset_id` among `rows`, empty when it has none.
pub fn of(rows: &[Row], asset_id: &str) -> Edits {
    rows.iter()
        .find(|row| row.asset_id == asset_id)
        .map(Row::parsed)
        .unwrap_or_default()
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

/// Refuses a `patch` holding a key that isn't an edit, or a value an edit can't take: an
/// `assetName` must be a PascalCase string (the contract's `pascalName`), a `rig` none or
/// generic, and `facingQuarterTurns` 0 to 3, or each null to reset it. Only a model, never an
/// image, has a rig or a facing.
fn validate(patch: &Edits, image: bool) -> Result<(), String> {
    for (key, value) in patch {
        match (key.as_str(), value) {
            (RIG | FACING_QUARTER_TURNS, _) if image => {
                return Err(format!("{key} is a model's edit, and this is an image"))
            }
            (RIG | FACING_QUARTER_TURNS, Value::Null) => {}
            (RIG, Value::String(rig)) if matches!(rig.as_str(), "none" | "generic") => {}
            (RIG, _) => return Err("A rig must be none or generic".into()),
            (FACING_QUARTER_TURNS, Value::Number(turns))
                if turns.as_u64().is_some_and(|t| t <= 3) => {}
            (FACING_QUARTER_TURNS, _) => {
                return Err("facingQuarterTurns must be a whole number from 0 to 3".into())
            }
            (ASSET_NAME, Value::Null) => {}
            (ASSET_NAME, Value::String(name)) if is_pascal(name) => {}
            (ASSET_NAME, Value::String(name)) => {
                return Err(format!(
                    "{name:?} isn't PascalCase: a game name starts with a capital letter, then only letters and digits"
                ))
            }
            (ASSET_NAME, _) => return Err("An assetName must be text".into()),
            _ => return Err(format!("{key} isn't an export edit")),
        }
    }
    Ok(())
}

/// Applies `patch` to `edits`: each key set to its value, or removed when the value is null.
fn apply(edits: &mut Edits, patch: &Edits) {
    for (key, value) in patch {
        if value.is_null() {
            edits.remove(key);
        } else {
            edits.insert(key.clone(), value.clone());
        }
    }
}

/// Writes your edit `patch` to asset `asset_id` (C4): each key set to its value, or reset to
/// inferred when the value is null. A group's member stands for its main item. It reaches every
/// member of the item's group, and for an image, every source of its texture set and their
/// groups, in one transaction. A row left
/// with no edit is deleted, so the table holds only what you set. Returns the ids it wrote.
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
    validate(patch, is_image(&kind))?;
    let sources: Vec<String> = if is_image(&kind) {
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
    let mut targets = BTreeSet::new();
    for source in &sources {
        targets.extend(
            structure::group_members(&mut *tx, source)
                .await
                .map_err(|e| e.to_string())?,
        );
    }

    for id in &targets {
        let text: Option<String> =
            sqlx::query_scalar("SELECT edits FROM export_edits WHERE asset_id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        let mut edits = text.as_deref().map(parse).unwrap_or_default();
        apply(&mut edits, patch);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memory_library_pool;
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
            (json!({"surface": "cutout"}), "surface isn't an export edit"),
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

    #[tokio::test]
    async fn an_image_has_no_rig_or_facing() {
        let pool = memory_library_pool().await;
        insert_path(&pool, "Pack/T_Rock_D.png").await;
        structure::rebuild(&pool, unmounted()).await.unwrap();
        for value in [json!({"rig": "none"}), json!({"facingQuarterTurns": 1})] {
            let key = value.as_object().unwrap().keys().next().unwrap().clone();
            assert_eq!(
                write(&pool, "Pack/T_Rock_D.png", &patch(value)).await,
                Err(format!("{key} is a model's edit, and this is an image"))
            );
        }
        assert_eq!(rows(&pool).await, []);
    }
}
