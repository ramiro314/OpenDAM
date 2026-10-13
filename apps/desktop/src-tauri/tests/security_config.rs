//! Checks the shipped Tauri config, capabilities and Cargo manifest for a wide-open security
//! surface: a missing CSP, an asset protocol that serves the whole disk, broad capabilities, or
//! devtools in release builds.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

fn crate_file(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(name)
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path:?}: {e}"))
}

fn tauri_conf() -> Value {
    read_json(&crate_file("tauri.conf.json"))
}

fn cargo_manifest() -> toml::Table {
    let text = fs::read_to_string(crate_file("Cargo.toml")).unwrap();
    text.parse().unwrap()
}

/// Every string anywhere inside `value`.
fn strings(value: &Value) -> Vec<&str> {
    match value {
        Value::String(s) => vec![s],
        Value::Array(items) => items.iter().flat_map(strings).collect(),
        Value::Object(map) => map.values().flat_map(strings).collect(),
        _ => vec![],
    }
}

#[test]
fn csp_is_a_real_policy_with_a_default_src() {
    let csp = &tauri_conf()["app"]["security"]["csp"];
    let has_default_src = match csp {
        Value::String(policy) => policy
            .split(';')
            .any(|directive| directive.trim_start().starts_with("default-src ")),
        Value::Object(directives) => directives.contains_key("default-src"),
        _ => false,
    };
    assert!(has_default_src, "CSP has no default-src: {csp}");
}

#[test]
fn asset_protocol_static_scope_is_not_the_whole_disk() {
    let scope = &tauri_conf()["app"]["security"]["assetProtocol"]["scope"];
    // The scope is either a list of paths or an object with `allow` and `deny` lists.
    let allowed = match scope {
        Value::Object(map) => map.get("allow").cloned().unwrap_or(Value::Null),
        other => other.clone(),
    };
    for path in strings(&allowed) {
        assert!(
            !["**", "**/*", "$HOME/**", "$DESKTOP/**"].contains(&path),
            "asset protocol scope allows {path}"
        );
    }
}

#[test]
fn capabilities_grant_no_filesystem_or_shell_access() {
    let mut checked = 0;
    for entry in fs::read_dir(crate_file("capabilities")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        checked += 1;
        let capability = read_json(&path);
        for permission in capability["permissions"].as_array().unwrap() {
            let identifier = match permission {
                Value::String(id) => id.as_str(),
                other => other["identifier"].as_str().unwrap(),
            };
            assert!(
                !identifier.starts_with("fs:") && !identifier.starts_with("shell:"),
                "{path:?} grants {identifier}"
            );
            for scoped in strings(permission) {
                assert!(!scoped.contains("**"), "{path:?} grants the path {scoped}");
            }
        }
    }
    assert!(checked > 0, "no capability files found");
}

#[test]
fn tauri_dependency_has_no_devtools_feature() {
    let manifest = cargo_manifest();
    let features = manifest["dependencies"]["tauri"]
        .get("features")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        !features.iter().any(|f| f.as_str() == Some("devtools")),
        "tauri is built with the devtools feature: {features:?}"
    );
}
