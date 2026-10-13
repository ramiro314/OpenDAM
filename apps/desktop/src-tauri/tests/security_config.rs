//! Checks the shipped configuration files, which no unit test exercises.

use std::path::Path;

fn cargo_manifest() -> toml::Table {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    std::fs::read_to_string(path).unwrap().parse().unwrap()
}

/// `panic = "abort"` would kill the app on a decoder panic instead of letting
/// `catch_unwind` fail that one image.
#[test]
fn release_profile_unwinds_on_panic() {
    let manifest = cargo_manifest();
    let panic = manifest
        .get("profile")
        .and_then(|p| p.get("release"))
        .and_then(|r| r.get("panic"))
        .and_then(|v| v.as_str());
    assert_ne!(
        panic,
        Some("abort"),
        "[profile.release] sets panic = \"abort\""
    );
}
