//! The Dungeon Game asset contract (plan game-export, C1). `contract/asset-contract.schema.json`
//! is a byte copy of dungeon-game's schema, pinned by `contract/SOURCE.md` and changed only by
//! `scripts/sync-asset-contract.sh`. Manifests are validated against its root and
//! `conversion.json` files against `$defs/conversionFile`, as Draft 2020-12 with `format` checked.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use jsonschema::error::ValidationErrorKind;
use jsonschema::{ValidationError, Validator};
use serde_json::Value;

/// The `contractVersion` OpenDAM writes. A test keeps it equal to the pinned schema's.
pub const CONTRACT_VERSION: u64 = 1;

const SCHEMA: &str = include_str!("../../contract/asset-contract.schema.json");

static MANIFEST: LazyLock<Validator> = LazyLock::new(|| validator("manifest"));
static CONVERSION_FILE: LazyLock<Validator> = LazyLock::new(|| validator("conversionFile"));

/// Compiles the schema with its root `$ref` pointed at `$defs/<def>`, so every other `$ref` still
/// resolves inside the same document.
fn validator(def: &str) -> Validator {
    let mut schema: Value = serde_json::from_str(SCHEMA).expect("the pinned schema is JSON");
    schema["$ref"] = Value::String(format!("#/$defs/{def}"));
    jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&schema)
        .expect("the pinned schema compiles")
}

/// Each error as `<JSON pointer>: <message>`, the pointer `/` for the document itself.
fn errors(validator: &Validator, instance: &Value) -> Result<(), Vec<String>> {
    let found: Vec<ValidationError> = validator.iter_errors(instance).collect();
    let mut errors = Vec::new();
    describe(&found, &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn describe(errors: &[ValidationError], out: &mut Vec<String>) {
    for error in errors {
        let path = error.instance_path().as_str();
        match error.kind() {
            // A failed `oneOf` would print the whole value and no reason, so it gives the errors
            // of the branch that came closest instead: for an asset, the one its `kind` names.
            ValidationErrorKind::OneOfNotValid { context } => {
                let closest = context
                    .iter()
                    .filter(|b| !b.is_empty())
                    .min_by_key(|b| b.len());
                if let Some(closest) = closest {
                    describe(closest, out);
                    continue;
                }
            }
            // A failed `allOf` member evaluates no property, so `unevaluatedProperties` lists
            // every property it checked too. The member's own error says what is wrong.
            ValidationErrorKind::UnevaluatedProperties { .. }
                if errors.iter().any(|other| {
                    let other_path = other.instance_path().as_str();
                    !matches!(
                        other.kind(),
                        ValidationErrorKind::UnevaluatedProperties { .. }
                    ) && (other_path == path || other_path.starts_with(&format!("{path}/")))
                }) =>
            {
                continue
            }
            _ => {}
        }
        out.push(format!(
            "{}: {error}",
            if path.is_empty() { "/" } else { path }
        ));
    }
}

/// Checks a `manifest.json` against the schema. Only the schema: the importer's other checks
/// are `check_rules`, and the listed files are checked where the bundle is written.
pub fn validate_manifest(manifest: &Value) -> Result<(), Vec<String>> {
    errors(&MANIFEST, manifest)
}

/// The importer's manifest checks that the schema can't express (plan C1): unique pack ids, no
/// two models or animations with one `assetName`, no two textures with one `assetName` and
/// role, no two assets with one `file`, a known pack for every asset, every material texture
/// naming a texture asset with that role, and every `avatarFrom` naming a rigged model. Each
/// error names the asset by its JSON pointer. Run it beside `validate_manifest`: it skips what
/// the schema already refuses, such as an asset without a `kind`.
pub fn check_rules(manifest: &Value) -> Result<(), Vec<String>> {
    let empty = Vec::new();
    let list = |key: &str| manifest[key].as_array().unwrap_or(&empty);
    let text = |value: &Value| value.as_str().map(str::to_string);
    let mut errors = Vec::new();

    let mut packs = HashSet::new();
    for (i, pack) in list("packs").iter().enumerate() {
        if let Some(id) = text(&pack["id"]) {
            if !packs.insert(id.clone()) {
                errors.push(format!("/packs/{i}/id: another pack has the id {id}"));
            }
        }
    }

    // The first asset holding each key, by its index
    let mut names: HashMap<(bool, String, String), usize> = HashMap::new();
    let mut files: HashMap<String, usize> = HashMap::new();
    let mut textures = HashSet::new();
    let mut rigged = HashSet::new();
    let assets = list("assets");
    for (i, asset) in assets.iter().enumerate() {
        let (Some(kind), Some(name)) = (text(&asset["kind"]), text(&asset["assetName"])) else {
            continue;
        };
        let is_texture = kind == "texture";
        let role = text(&asset["role"]).unwrap_or_default();
        if is_texture {
            textures.insert((name.clone(), role.clone()));
        } else if kind == "model" && asset["rig"].as_str().is_some_and(|r| r != "none") {
            rigged.insert(name.clone());
        }
        let key = (
            is_texture,
            name.clone(),
            if is_texture {
                role.clone()
            } else {
                String::new()
            },
        );
        if let Some(first) = names.get(&key) {
            let what = if is_texture {
                format!("texture {name} with role {role}")
            } else {
                format!("model or animation named {name}")
            };
            errors.push(format!("/assets/{i}: /assets/{first} is already a {what}"));
        } else {
            names.insert(key, i);
        }
        if let Some(file) = text(&asset["file"]) {
            if let Some(first) = files.get(&file) {
                errors.push(format!(
                    "/assets/{i}/file: /assets/{first} is already {file}"
                ));
            } else {
                files.insert(file, i);
            }
        }
        if let Some(pack) = text(&asset["pack"]) {
            if !packs.contains(&pack) {
                errors.push(format!("/assets/{i}/pack: no pack has the id {pack}"));
            }
        }
    }

    for (i, asset) in assets.iter().enumerate() {
        let materials = asset["materials"].as_array().unwrap_or(&empty);
        for (m, material) in materials.iter().enumerate() {
            let Some(map) = material["textures"].as_object() else {
                continue;
            };
            for (role, name) in map {
                let Some(name) = name.as_str() else { continue };
                if !textures.contains(&(name.to_string(), role.clone())) {
                    errors.push(format!(
                        "/assets/{i}/materials/{m}/textures/{role}: no texture {name} has the role {role}"
                    ));
                }
            }
        }
        if let Some(avatar) = text(&asset["avatarFrom"]) {
            if !rigged.contains(&avatar) {
                errors.push(format!(
                    "/assets/{i}/avatarFrom: no rigged model is named {avatar}"
                ));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Checks a `conversion.json` against `$defs/conversionFile`.
pub fn validate_conversion_file(file: &Value) -> Result<(), Vec<String>> {
    errors(&CONVERSION_FILE, file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    const SOURCE: &str = include_str!("../../contract/SOURCE.md");

    fn source_field(name: &str) -> &'static str {
        let prefix = format!("- {name}: `");
        SOURCE
            .lines()
            .find_map(|l| l.strip_prefix(prefix.as_str())?.strip_suffix('`'))
            .unwrap_or_else(|| panic!("SOURCE.md names no {name}"))
    }

    #[test]
    fn schema_matches_the_pinned_sha256() {
        let sha: String = Sha256::digest(SCHEMA.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            sha,
            source_field("sha256"),
            "contract/asset-contract.schema.json was edited: re-copy it with \
             scripts/sync-asset-contract.sh instead"
        );
        assert_eq!(source_field("Repository"), "ramiro314/dungeon-game");
        assert_eq!(source_field("Path"), "docs/asset-contract.schema.json");
        assert_eq!(source_field("Commit").len(), 40);
    }

    #[test]
    fn contract_version_matches_the_schema() {
        let schema: Value = serde_json::from_str(SCHEMA).unwrap();
        for def in ["manifest", "conversionFile"] {
            assert_eq!(
                schema["$defs"][def]["properties"]["contractVersion"]["const"],
                json!(CONTRACT_VERSION),
                "{def}"
            );
        }
    }

    fn step() -> Value {
        json!({
            "tool": "OpenDAM",
            "toolVersion": "0.1.0",
            "by": "opendam-textures/1",
            "from": "png",
            "settings": { "flipGreen": true, "inputs": "T_Rock_R.png", "scale": 1, "note": null }
        })
    }

    fn texture() -> Value {
        json!({
            "kind": "texture",
            "assetName": "Rock",
            "file": "Textures/Rock_BaseColor.png",
            "sha256": "0".repeat(64),
            "pack": "stylarts-props",
            "sourcePath": "Stylarts/Textures/T_Rock_D.png",
            "conversion": [],
            "role": "BaseColor"
        })
    }

    fn model() -> Value {
        json!({
            "kind": "model",
            "assetName": "Rock",
            "file": "Models/Rock.fbx",
            "sha256": "a".repeat(64),
            "pack": "stylarts-props",
            "sourcePath": "Stylarts/Meshes/SM_Rock.fbx",
            "conversion": [step()],
            "bounds": { "min": [-0.5, 0, -0.5], "max": [0.5, 1, 0.5] },
            "rig": "none",
            "materials": [{ "slot": "M_Rock", "name": "Rock", "textures": { "BaseColor": "Rock" } }]
        })
    }

    fn manifest() -> Value {
        json!({
            "contractVersion": 1,
            "domain": "Dungeon",
            "exportedAt": "2026-10-22T12:00:00Z",
            "exporter": { "name": "OpenDAM", "version": "0.1.0" },
            "packs": [{
                "id": "stylarts-props",
                "name": "Stylarts Props",
                "licence": { "spdx": null, "name": "Unknown" },
                "origin": "generic"
            }],
            "assets": [model(), texture()]
        })
    }

    /// The errors for a manifest changed by `edit`, which must be invalid.
    fn manifest_errors(edit: impl FnOnce(&mut Value)) -> Vec<String> {
        let mut m = manifest();
        edit(&mut m);
        validate_manifest(&m).expect_err("the manifest should be invalid")
    }

    #[test]
    fn valid_manifests_pass() {
        assert_eq!(validate_manifest(&manifest()), Ok(()));
        let mut empty = manifest();
        empty["packs"] = json!([]);
        empty["assets"] = json!([]);
        assert_eq!(validate_manifest(&empty), Ok(()));
    }

    #[test]
    fn invalid_manifests_fail_with_their_path() {
        let errors = manifest_errors(|m| m["contractVersion"] = json!(2));
        assert!(
            errors.iter().any(|e| e.starts_with("/contractVersion: ")),
            "{errors:?}"
        );

        let errors = manifest_errors(|m| m["domain"] = json!("dungeon"));
        assert!(
            errors.iter().any(|e| e.starts_with("/domain: ")),
            "{errors:?}"
        );

        let errors = manifest_errors(|m| {
            m.as_object_mut().unwrap().remove("exporter");
        });
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with("/: ") && e.contains("exporter")),
            "{errors:?}"
        );

        let errors = manifest_errors(|m| m["packs"][0]["origin"] = json!("godot"));
        assert!(
            errors.iter().any(|e| e.starts_with("/packs/0/origin: ")),
            "{errors:?}"
        );
    }

    #[test]
    fn formats_are_checked() {
        let errors = manifest_errors(|m| m["exportedAt"] = json!("yesterday"));
        assert!(
            errors.iter().any(|e| e.starts_with("/exportedAt: ")),
            "{errors:?}"
        );
    }

    #[test]
    fn unevaluated_asset_properties_fail() {
        let errors = manifest_errors(|m| m["assets"][1]["bounds"] = model()["bounds"].clone());
        assert!(
            errors.iter().any(|e| e.starts_with("/assets/1")),
            "{errors:?}"
        );
        // A model's own properties don't leak into a texture, nor the reverse.
        manifest_errors(|m| m["assets"][0]["role"] = json!("BaseColor"));
        manifest_errors(|m| m["assets"][1]["file"] = json!("Textures/Rock_Albedo.png"));
        let errors = manifest_errors(|m| m["assets"][0]["sha256"] = json!("ABC"));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].starts_with("/assets/0/sha256: "), "{errors:?}");
    }

    #[test]
    fn valid_conversion_files_pass() {
        let file = json!({ "contractVersion": 1, "steps": [step(), { "tool": "Blender", "toolVersion": "5.2.2", "from": "uasset" }] });
        assert_eq!(validate_conversion_file(&file), Ok(()));
    }

    #[test]
    fn invalid_conversion_files_fail() {
        let invalid = [
            json!({ "contractVersion": 1, "steps": [] }),
            json!({ "contractVersion": 2, "steps": [step()] }),
            json!({ "contractVersion": 1, "steps": [step()], "notes": "x" }),
            json!({ "contractVersion": 1, "steps": [{ "tool": "Blender", "from": "uasset" }] }),
            json!({ "contractVersion": 1, "steps": [{ "tool": "Blender", "toolVersion": "5.2", "from": "fbx", "settings": { "axes": { "up": "Y" } } }] }),
        ];
        for file in invalid {
            assert!(validate_conversion_file(&file).is_err(), "{file}");
        }
        let errors = validate_conversion_file(&json!({ "contractVersion": 1, "steps": [{ "tool": "", "toolVersion": "5.2", "from": "fbx" }] }))
            .unwrap_err();
        assert!(
            errors.iter().any(|e| e.starts_with("/steps/0/tool: ")),
            "{errors:?}"
        );
        // A manifest is not a conversion file.
        assert!(validate_conversion_file(&manifest()).is_err());
    }

    #[test]
    fn a_valid_manifest_passes_the_importers_rules() {
        assert_eq!(check_rules(&manifest()), Ok(()));
        let mut animated = manifest();
        animated["assets"][0]["rig"] = json!("generic");
        animated["assets"].as_array_mut().unwrap().push(json!({
            "kind": "animation", "assetName": "RockWobble", "file": "Animations/RockWobble.fbx",
            "avatarFrom": "Rock", "clips": ["Wobble"]
        }));
        assert_eq!(check_rules(&animated), Ok(()));
    }

    /// The rule errors for a manifest changed by `edit`, which must break a rule.
    fn rule_errors(edit: impl FnOnce(&mut Value)) -> Vec<String> {
        let mut m = manifest();
        edit(&mut m);
        check_rules(&m).expect_err("the manifest should break a rule")
    }

    #[test]
    fn duplicate_names_and_files_break_the_rules() {
        let assets = |m: &mut Value| m["assets"].as_array_mut().unwrap().clone();
        let errors = rule_errors(|m| {
            let mut copy = assets(m)[1].clone();
            copy["file"] = json!("Textures/Rock_BaseColor.tga");
            m["assets"].as_array_mut().unwrap().push(copy);
        });
        assert_eq!(
            errors,
            ["/assets/2: /assets/1 is already a texture Rock with role BaseColor"]
        );

        let errors = rule_errors(|m| {
            let copy = assets(m)[0].clone();
            m["assets"].as_array_mut().unwrap().push(copy);
        });
        assert_eq!(
            errors,
            [
                "/assets/2: /assets/0 is already a model or animation named Rock",
                "/assets/2/file: /assets/0 is already Models/Rock.fbx"
            ]
        );

        // A model and its own textures share a name on purpose, and roles tell textures apart
        let mut ok = manifest();
        let mut normal = assets(&mut ok)[1].clone();
        normal["role"] = json!("Normal");
        normal["file"] = json!("Textures/Rock_Normal.png");
        ok["assets"].as_array_mut().unwrap().push(normal);
        assert_eq!(check_rules(&ok), Ok(()));
    }

    #[test]
    fn unknown_packs_and_textures_break_the_rules() {
        let errors = rule_errors(|m| m["assets"][1]["pack"] = json!("nope"));
        assert_eq!(errors, ["/assets/1/pack: no pack has the id nope"]);

        let errors = rule_errors(|m| {
            let pack = m["packs"][0].clone();
            m["packs"].as_array_mut().unwrap().push(pack);
        });
        assert_eq!(
            errors,
            ["/packs/1/id: another pack has the id stylarts-props"]
        );

        let errors = rule_errors(|m| m["assets"][1]["role"] = json!("Normal"));
        assert_eq!(
            errors,
            ["/assets/0/materials/0/textures/BaseColor: no texture Rock has the role BaseColor"]
        );
    }

    #[test]
    fn avatar_from_must_name_a_rigged_model() {
        let errors = rule_errors(|m| {
            m["assets"].as_array_mut().unwrap().push(json!({
                "kind": "animation", "assetName": "RockWobble", "file": "Animations/RockWobble.fbx",
                "avatarFrom": "Rock", "clips": ["Wobble"]
            }));
        });
        assert_eq!(
            errors,
            ["/assets/2/avatarFrom: no rigged model is named Rock"]
        );
    }
}
