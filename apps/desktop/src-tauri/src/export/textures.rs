//! Texture roles, what an image's name says about it, and whether it can go into a bundle as it
//! is (plan game-export C7 step 3, C8). Only pass-through textures export so far: every texture
//! that needs an operation is refused by the "texture conversion not built yet" guard (plan
//! "Build order").

use serde::{Deserialize, Serialize};

/// A URP Lit texture role, as the contract names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum Role {
    BaseColor,
    Normal,
    MetallicSmoothness,
    Occlusion,
    Emission,
    Height,
}

impl Role {
    pub const ALL: [Role; 6] = [
        Role::BaseColor,
        Role::Normal,
        Role::MetallicSmoothness,
        Role::Occlusion,
        Role::Emission,
        Role::Height,
    ];

    /// The role the contract names `name`.
    pub fn parse(name: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.as_str() == name)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::BaseColor => "BaseColor",
            Role::Normal => "Normal",
            Role::MetallicSmoothness => "MetallicSmoothness",
            Role::Occlusion => "Occlusion",
            Role::Emission => "Emission",
            Role::Height => "Height",
        }
    }
}

/// What a source image holds, read from its role or layout suffix (C8's "Source layout").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// One role, ready for URP Lit as it is
    Role(Role),
    /// AO in R, roughness in G, metallic in B
    Orm,
    /// HDRP: metal R, AO G, detail B, smoothness A
    MaskMap,
    Roughness,
    Gloss,
    Metallic,
}

impl Layout {
    pub fn label(self) -> &'static str {
        match self {
            Layout::Role(role) => role.as_str(),
            Layout::Orm => "ORM",
            Layout::MaskMap => "MaskMap",
            Layout::Roughness => "Roughness",
            Layout::Gloss => "Gloss",
            Layout::Metallic => "Metallic",
        }
    }

    /// The one role a source of this layout passes through as, if it is one.
    pub fn role(self) -> Option<Role> {
        match self {
            Layout::Role(role) => Some(role),
            _ => None,
        }
    }

    /// The layout as your edit names it (C4): `passThrough` for any one role, whose role is then
    /// an edit of its own, else its label.
    pub fn edit_name(self) -> &'static str {
        match self {
            Layout::Role(_) => PASS_THROUGH,
            other => other.label(),
        }
    }

    /// The layout your edit names `name`. `passThrough` gives `Layout::Role(BaseColor)`, whose
    /// role `SourceEdits::layout` replaces.
    pub fn from_edit_name(name: &str) -> Option<Layout> {
        [
            Layout::Role(Role::BaseColor),
            Layout::Orm,
            Layout::MaskMap,
            Layout::Roughness,
            Layout::Gloss,
            Layout::Metallic,
        ]
        .into_iter()
        .find(|l| l.edit_name() == name)
    }

    /// The roles a source of this layout gives once converted (C8): an ORM or MaskMap map
    /// splits into Occlusion and MetallicSmoothness, and a Roughness, Gloss or Metallic map
    /// merges into its set's MetallicSmoothness.
    pub fn outputs(self) -> &'static [Role] {
        match self {
            Layout::Role(Role::BaseColor) => &[Role::BaseColor],
            Layout::Role(Role::Normal) => &[Role::Normal],
            Layout::Role(Role::MetallicSmoothness) => &[Role::MetallicSmoothness],
            Layout::Role(Role::Occlusion) => &[Role::Occlusion],
            Layout::Role(Role::Emission) => &[Role::Emission],
            Layout::Role(Role::Height) => &[Role::Height],
            Layout::Orm | Layout::MaskMap => &[Role::Occlusion, Role::MetallicSmoothness],
            Layout::Roughness | Layout::Gloss | Layout::Metallic => &[Role::MetallicSmoothness],
        }
    }
}

/// The edit name of a layout that passes one role through (C4).
pub const PASS_THROUGH: &str = "passThrough";

/// Your edits to one source image (C4): its layout, its role when that layout passes one role
/// through, and its normal-map convention. A layout with several outputs (ORM, MaskMap) or one
/// merged with other sources (Roughness, Gloss, Metallic) fixes its roles, so a role edit then
/// counts for nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceEdits {
    pub layout: Option<Layout>,
    pub role: Option<Role>,
    pub convention: Option<Convention>,
}

impl SourceEdits {
    /// The layout of a source inferred as `inferred`, with these edits: your layout, else the
    /// inferred one. When it passes one role through, the role is yours, else the inferred
    /// layout's, else BaseColor.
    pub fn layout(&self, inferred: Layout) -> Layout {
        match self.layout.unwrap_or(inferred) {
            Layout::Role(_) => {
                Layout::Role(self.role.or(inferred.role()).unwrap_or(Role::BaseColor))
            }
            layout => layout,
        }
    }
}

/// A normal map's green channel convention (plan D14). URP wants OpenGL, green up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(rename_all = "lowercase")]
pub enum Convention {
    #[serde(rename = "opengl")]
    OpenGl,
    #[serde(rename = "directx")]
    DirectX,
}

/// The role and layout suffixes of C7 step 3, matched as the stem's last `_` word, ignoring case.
const SUFFIXES: [(&str, Layout); 34] = [
    ("D", Layout::Role(Role::BaseColor)),
    ("BaseColor", Layout::Role(Role::BaseColor)),
    ("Albedo", Layout::Role(Role::BaseColor)),
    ("Diffuse", Layout::Role(Role::BaseColor)),
    ("Color", Layout::Role(Role::BaseColor)),
    ("N", Layout::Role(Role::Normal)),
    ("Normal", Layout::Role(Role::Normal)),
    ("NRM", Layout::Role(Role::Normal)),
    ("ORM", Layout::Orm),
    ("AORM", Layout::Orm),
    ("ARM", Layout::Orm),
    ("MaskMap", Layout::MaskMap),
    ("Mask", Layout::MaskMap),
    ("R", Layout::Roughness),
    ("Roughness", Layout::Roughness),
    ("Rough", Layout::Roughness),
    ("G", Layout::Gloss),
    ("Gloss", Layout::Gloss),
    ("Glossiness", Layout::Gloss),
    ("Smoothness", Layout::Gloss),
    ("M", Layout::Metallic),
    ("Metallic", Layout::Metallic),
    ("Metalness", Layout::Metallic),
    ("AO", Layout::Role(Role::Occlusion)),
    ("Occlusion", Layout::Role(Role::Occlusion)),
    ("E", Layout::Role(Role::Emission)),
    ("Emissive", Layout::Role(Role::Emission)),
    ("Emission", Layout::Role(Role::Emission)),
    ("H", Layout::Role(Role::Height)),
    ("Height", Layout::Role(Role::Height)),
    ("Disp", Layout::Role(Role::Height)),
    ("Displacement", Layout::Role(Role::Height)),
    ("MetallicSmoothness", Layout::Role(Role::MetallicSmoothness)),
    ("MS", Layout::Role(Role::MetallicSmoothness)),
];

/// What an image's file stem says: the stem its set and default name come from, its layout, and
/// a normal-map convention when the stem states one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StemInfo {
    /// The stem without its `_DX`/`_GL` and role or layout suffix: `T_Rock` for `T_Rock_N_DX`
    pub base: String,
    /// `None` when no suffix names one; the image is then a BaseColor (C3)
    pub layout: Option<Layout>,
    pub convention: Option<Convention>,
}

/// Reads a trailing `_DX` or `_GL` first, then the role or layout suffix before it (C7 step 3).
/// A suffix is dropped only when something is left before it.
pub fn read_stem(stem: &str) -> StemInfo {
    let mut base = stem;
    let mut convention = None;
    if let Some((head, last)) = base.rsplit_once('_').filter(|(head, _)| !head.is_empty()) {
        if last.eq_ignore_ascii_case("DX") {
            convention = Some(Convention::DirectX);
            base = head;
        } else if last.eq_ignore_ascii_case("GL") {
            convention = Some(Convention::OpenGl);
            base = head;
        }
    }
    let mut layout = None;
    if let Some((head, last)) = base.rsplit_once('_').filter(|(head, _)| !head.is_empty()) {
        if let Some((_, found)) = SUFFIXES.iter().find(|(s, _)| s.eq_ignore_ascii_case(last)) {
            layout = Some(*found);
            base = head;
        }
    }
    StemInfo {
        base: base.to_string(),
        layout,
        convention,
    }
}

/// The formats a bundle takes as they are.
const BUNDLE_FORMATS: [&str; 4] = ["png", "tga", "psd", "tif"];
/// The formats C8 turns into PNG (D25).
const TO_PNG_FORMATS: [&str; 5] = ["jpg", "jpeg", "bmp", "webp", "gif"];

/// Why a texture can't go into the bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Supported by the plan, refused until texture conversion is built (step 5's guard)
    NeedsConversion(String),
    /// Never exported (D4, D25)
    Unsupported(String),
}

/// A texture that passes through: its role and the extension it has in the bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassThrough {
    pub role: Role,
    pub extension: &'static str,
}

/// Whether a source with extension `extension` (any case), layout `layout` and normal-map
/// convention `convention` passes through byte for byte (a `.tiff` only renamed `.tif`), or why
/// not. A PSD that needs any operation is refused for good (D4); every other operation waits for
/// texture conversion.
pub fn pass_through(
    extension: &str,
    layout: Layout,
    convention: Convention,
) -> Result<PassThrough, Refusal> {
    let extension = extension.to_ascii_lowercase();
    let bundled = match extension.as_str() {
        "tiff" => Some("tif"),
        ext => BUNDLE_FORMATS.iter().copied().find(|f| *f == ext),
    };
    let converts = TO_PNG_FORMATS.contains(&extension.as_str());
    if bundled.is_none() && !converts {
        return Err(Refusal::Unsupported(format!(
            "a .{extension} texture can't be exported: use png, tga, psd or tif"
        )));
    }

    let mut operations = Vec::new();
    if converts {
        operations.push(format!("its .{extension} format to PNG"));
    }
    match layout {
        Layout::Role(Role::Normal) if convention == Convention::DirectX => {
            operations.push("its DirectX normal map's green channel flipped".into())
        }
        Layout::Role(_) => {}
        Layout::Orm | Layout::MaskMap => operations.push(format!(
            "its {} channels split into URP roles",
            layout.label()
        )),
        Layout::Roughness | Layout::Gloss | Layout::Metallic => operations.push(format!(
            "its {} map merged into a MetallicSmoothness",
            layout.label()
        )),
    }
    match (bundled, layout) {
        (Some(extension), Layout::Role(role)) if operations.is_empty() => {
            Ok(PassThrough { role, extension })
        }
        (Some("psd"), _) => Err(Refusal::Unsupported(format!(
            "a PSD that needs converting can't be exported (it needs {})",
            operations.join(" and ")
        ))),
        _ => Err(Refusal::NeedsConversion(format!(
            "texture conversion not built yet: it needs {}",
            operations.join(" and ")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(base: &str, layout: Option<Layout>, convention: Option<Convention>) -> StemInfo {
        StemInfo {
            base: base.into(),
            layout,
            convention,
        }
    }

    #[test]
    fn your_layout_wins_and_your_role_counts_only_when_one_role_passes_through() {
        let edits = |layout: Option<&str>, role: Option<Role>| SourceEdits {
            layout: layout.map(|l| Layout::from_edit_name(l).unwrap()),
            role,
            convention: None,
        };
        let normal = Layout::Role(Role::Normal);
        for (yours, inferred, expected) in [
            (edits(None, None), Layout::Orm, Layout::Orm),
            (edits(Some("ORM"), None), normal, Layout::Orm),
            // Your role replaces the inferred one
            (
                edits(None, Some(Role::Height)),
                normal,
                Layout::Role(Role::Height),
            ),
            // A layout with several outputs fixes its roles
            (
                edits(None, Some(Role::Height)),
                Layout::MaskMap,
                Layout::MaskMap,
            ),
            (
                edits(Some("Gloss"), Some(Role::Height)),
                normal,
                Layout::Gloss,
            ),
            // Passing through keeps the inferred role, else BaseColor
            (edits(Some(PASS_THROUGH), None), normal, normal),
            (
                edits(Some(PASS_THROUGH), None),
                Layout::Orm,
                Layout::Role(Role::BaseColor),
            ),
            (
                edits(Some(PASS_THROUGH), Some(Role::Emission)),
                Layout::Metallic,
                Layout::Role(Role::Emission),
            ),
        ] {
            assert_eq!(
                yours.layout(inferred),
                expected,
                "{yours:?} on {inferred:?}"
            );
        }
        assert_eq!(Layout::from_edit_name("Normal"), None);
        assert_eq!(Layout::Orm.edit_name(), "ORM");
        assert_eq!(normal.edit_name(), PASS_THROUGH);
    }

    #[test]
    fn suffixes_give_roles_and_layouts() {
        use Layout::*;
        for (stem, expected) in [
            (
                "T_Rock_D",
                info("T_Rock", Some(Role(super::Role::BaseColor)), None),
            ),
            (
                "T_Rock_n",
                info("T_Rock", Some(Role(super::Role::Normal)), None),
            ),
            ("T_desk_AORM", info("T_desk", Some(Orm), None)),
            ("T_Rock_R", info("T_Rock", Some(Roughness), None)),
            ("T_Rock_M", info("T_Rock", Some(Metallic), None)),
            ("T_Rock_MaskMap", info("T_Rock", Some(MaskMap), None)),
            (
                "T_Rune_AO",
                info("T_Rune", Some(Role(super::Role::Occlusion)), None),
            ),
            (
                "T_Rune_MS",
                info("T_Rune", Some(Role(super::Role::MetallicSmoothness)), None),
            ),
            ("mattress_01", info("mattress_01", None, None)),
            (
                "PolygonCity_Texture_01_A",
                info("PolygonCity_Texture_01_A", None, None),
            ),
        ] {
            assert_eq!(read_stem(stem), expected, "{stem}");
        }
    }

    #[test]
    fn a_convention_suffix_is_read_before_the_role() {
        assert_eq!(
            read_stem("T_Rock_N_DX"),
            info(
                "T_Rock",
                Some(Layout::Role(Role::Normal)),
                Some(Convention::DirectX)
            )
        );
        assert_eq!(
            read_stem("T_Rock_normal_gl"),
            info(
                "T_Rock",
                Some(Layout::Role(Role::Normal)),
                Some(Convention::OpenGl)
            )
        );
    }

    #[test]
    fn a_suffix_alone_is_kept_as_the_stem() {
        assert_eq!(read_stem("N"), info("N", None, None));
        assert_eq!(read_stem("_N"), info("_N", None, None));
        assert_eq!(
            read_stem("DX_N"),
            info("DX", Some(Layout::Role(Role::Normal)), None)
        );
    }

    #[test]
    fn ready_textures_pass_through() {
        let base = Layout::Role(Role::BaseColor);
        for (ext, bundled) in [
            ("png", "png"),
            ("TGA", "tga"),
            ("psd", "psd"),
            ("tiff", "tif"),
        ] {
            assert_eq!(
                pass_through(ext, base, Convention::OpenGl),
                Ok(PassThrough {
                    role: Role::BaseColor,
                    extension: bundled
                }),
                "{ext}"
            );
        }
        let normal = Layout::Role(Role::Normal);
        assert!(pass_through("png", normal, Convention::OpenGl).is_ok());
    }

    #[test]
    fn textures_needing_an_operation_wait_for_conversion() {
        let needs = |ext: &str, layout, convention| {
            matches!(
                pass_through(ext, layout, convention),
                Err(Refusal::NeedsConversion(m)) if m.starts_with("texture conversion not built yet")
            )
        };
        let base = Layout::Role(Role::BaseColor);
        for ext in ["jpg", "jpeg", "bmp", "webp", "gif"] {
            assert!(needs(ext, base, Convention::OpenGl), "{ext}");
        }
        for layout in [
            Layout::Orm,
            Layout::MaskMap,
            Layout::Roughness,
            Layout::Gloss,
            Layout::Metallic,
        ] {
            assert!(needs("png", layout, Convention::OpenGl), "{layout:?}");
        }
        assert!(needs(
            "png",
            Layout::Role(Role::Normal),
            Convention::DirectX
        ));
        // A DirectX convention means nothing to other roles
        assert!(pass_through("png", base, Convention::DirectX).is_ok());
    }

    #[test]
    fn unsupported_formats_and_psds_needing_work_are_refused_for_good() {
        let refused = |ext: &str, layout| {
            matches!(
                pass_through(ext, layout, Convention::DirectX),
                Err(Refusal::Unsupported(_))
            )
        };
        for ext in ["dds", "exr", "hdr", "ktx", "svg"] {
            assert!(refused(ext, Layout::Role(Role::BaseColor)), "{ext}");
        }
        assert!(refused("psd", Layout::Orm));
        assert!(refused("psd", Layout::Role(Role::Normal)));
    }
}
