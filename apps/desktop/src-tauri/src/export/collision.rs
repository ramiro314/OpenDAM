//! The collision rule (plan game-export C9 step 2): the meshes an export drops. It is this one
//! list, sent to the preview worker's `fbx_facts` in its request, and to the Blender script in
//! its job, so both keep the same meshes.

use serde::Serialize;

/// Name patterns, matched ignoring case.
#[derive(Debug, Serialize)]
pub struct Patterns {
    pub prefixes: &'static [&'static str],
    pub suffixes: &'static [&'static str],
}

/// A mesh is a collision mesh when its name starts with one of `prefixes` or ends with one of
/// `suffixes`. LOD meshes (`_LODn`) are kept.
pub const PATTERNS: Patterns = Patterns {
    prefixes: &["UCX_", "UBX_", "USP_", "UCP_"],
    suffixes: &["_Collision", "_COL"],
};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_patterns_go_to_the_worker_as_prefixes_and_suffixes() {
        assert_eq!(
            serde_json::to_value(&PATTERNS).unwrap(),
            json!({
                "prefixes": ["UCX_", "UBX_", "USP_", "UCP_"],
                "suffixes": ["_Collision", "_COL"],
            })
        );
    }
}
