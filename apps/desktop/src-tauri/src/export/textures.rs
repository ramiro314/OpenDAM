//! Texture roles, what an image's name says about it, and how a texture set's sources become the
//! files their roles need (plan game-export C7 step 3, C8). A source that is ready for URP Lit
//! passes through byte for byte. Every other one is converted in Rust with the `image` crate, at
//! its own resolution and bit depth, into a PNG, and each operation is a recorded step (D33).

use std::io::Cursor;

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{DynamicImage, GenericImageView, ImageBuffer, ImageFormat, ImageReader, Primitive};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

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

/// A layout as your edit sets it (C4): one role passed through as it is, whose role is an edit
/// of its own, or a layout that fixes its roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutEdit {
    PassThrough,
    /// ORM, MaskMap, Roughness, Gloss or Metallic, never `Layout::Role`
    Fixed(Layout),
}

impl LayoutEdit {
    /// What `layout` is as an edit names it.
    pub fn of(layout: Layout) -> LayoutEdit {
        match layout {
            Layout::Role(_) => LayoutEdit::PassThrough,
            layout => LayoutEdit::Fixed(layout),
        }
    }

    /// Its name in an edit: `passThrough`, else the fixed layout's label.
    pub fn name(self) -> &'static str {
        match self {
            LayoutEdit::PassThrough => PASS_THROUGH,
            LayoutEdit::Fixed(layout) => layout.label(),
        }
    }

    /// The layout edit named `name`.
    pub fn parse(name: &str) -> Option<LayoutEdit> {
        if name == PASS_THROUGH {
            return Some(LayoutEdit::PassThrough);
        }
        [
            Layout::Orm,
            Layout::MaskMap,
            Layout::Roughness,
            Layout::Gloss,
            Layout::Metallic,
        ]
        .into_iter()
        .find(|l| l.label() == name)
        .map(LayoutEdit::Fixed)
    }
}

/// Your edits to one source image (C4): its layout, its role when that layout passes one role
/// through, and its normal-map convention. A layout with several outputs (ORM, MaskMap) or one
/// merged with other sources (Roughness, Gloss, Metallic) fixes its roles, so a role edit then
/// counts for nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceEdits {
    pub layout: Option<LayoutEdit>,
    pub role: Option<Role>,
    pub convention: Option<Convention>,
}

impl SourceEdits {
    /// The role a source inferred as `inferred` passes through as when you set none: the
    /// inferred layout's, else BaseColor. The export and the Game tab both take it from here.
    pub fn inferred_role(inferred: Layout) -> Role {
        inferred.role().unwrap_or(Role::BaseColor)
    }

    /// The layout of a source inferred as `inferred`, with these edits: your layout, else the
    /// inferred one. When it passes one role through, the role is yours, else `inferred_role`.
    pub fn layout(&self, inferred: Layout) -> Layout {
        match self.layout.unwrap_or(LayoutEdit::of(inferred)) {
            LayoutEdit::PassThrough => {
                Layout::Role(self.role.unwrap_or(SourceEdits::inferred_role(inferred)))
            }
            LayoutEdit::Fixed(layout) => layout,
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

/// `<N>` in `opendam-textures/<N>` (D33), which the export cache keys on (C11). Bump it whenever
/// an operation's pixels or encoding change, so no cached file made by older code is reused.
pub const VERSION: u32 = 1;

/// The `toolVersion` of OpenDAM's texture steps.
pub fn tool_version() -> String {
    format!("opendam-textures/{VERSION}")
}

/// The formats a bundle takes as they are.
const BUNDLE_FORMATS: [&str; 4] = ["png", "tga", "psd", "tif"];
/// The formats C8 turns into PNG (D25).
const TO_PNG_FORMATS: [&str; 5] = ["jpg", "jpeg", "bmp", "webp", "gif"];

/// One source of a texture set, as `plan` reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Relative to its pack folder, as the steps' settings name it (D17)
    pub source_path: String,
    /// Its extension, any case
    pub extension: String,
    pub layout: Layout,
    pub convention: Convention,
}

impl Source {
    fn extension(&self) -> String {
        self.extension.to_ascii_lowercase()
    }
}

/// Where a merged MetallicSmoothness takes its smoothness from (C8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Smoothness {
    /// `1 - roughness`, from the first input
    Roughness,
    /// The first input as it is
    Gloss,
    /// URP Lit's default, 0.5, for a lone metallic map
    Half,
}

/// The pixel work that makes one output from its inputs, in order (C8's table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recipe {
    /// The input's own pixels, its green channel flipped when `flip_green`
    Same { flip_green: bool },
    /// An ORM map's AO (R) in R, G and B
    OrmOcclusion,
    /// An ORM map's metallic (B) in R, G and B, and `1 - roughness` (G) in A
    OrmMetallicSmoothness,
    /// A MaskMap's AO (G) in R, G and B
    MaskOcclusion,
    /// A MaskMap's metallic (R) in R, G and B, and its smoothness (A) in A
    MaskMetallicSmoothness,
    /// Metallic in R, G and B (0 without a metallic map) and smoothness in A. The inputs are the
    /// roughness or gloss map first, unless `smoothness` is `Half`, then the metallic map when
    /// `metallic`
    Merge {
        smoothness: Smoothness,
        metallic: bool,
    },
}

/// How an output is made.
#[derive(Debug, Clone, PartialEq)]
pub enum Work {
    /// Its one input copied byte for byte, under the bundle extension `extension`
    PassThrough { extension: &'static str },
    /// A PNG made by `recipe`, recorded as `steps` (D33)
    Convert { recipe: Recipe, steps: Vec<Value> },
}

/// One texture a set exports: a role and the sources it is made from.
#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    pub role: Role,
    /// Indices into the sources given to `plan`. The first is the asset's `sourcePath` (D17)
    pub inputs: Vec<usize>,
    pub work: Work,
}

impl Output {
    /// The extension of its file in the bundle.
    pub fn extension(&self) -> &'static str {
        match &self.work {
            Work::PassThrough { extension } => extension,
            Work::Convert { .. } => "png",
        }
    }
}

/// Sources a set can't export, by index, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub sources: Vec<usize>,
    pub message: String,
}

/// What a source of layout `layout` and convention `convention` needs besides a format change,
/// in words, for the PSD refusal.
fn operations(layout: Layout, convention: Convention) -> Option<String> {
    match layout {
        Layout::Role(Role::Normal) if convention == Convention::DirectX => {
            Some("its DirectX normal map's green channel flipped".into())
        }
        Layout::Role(_) => None,
        Layout::Orm | Layout::MaskMap => Some(format!(
            "its {} channels split into URP roles",
            layout.label()
        )),
        Layout::Roughness | Layout::Gloss | Layout::Metallic => Some(format!(
            "its {} map merged into a MetallicSmoothness",
            layout.label()
        )),
    }
}

/// One output's steps (C8): `to-png` for each input that isn't in a bundle format, then
/// `operation`, then `invert-roughness` and `flip-normal-green` when `recipe` does them. Every
/// step's settings name the first input's layout and the output role, plus `extra`.
fn steps(
    sources: &[Source],
    inputs: &[usize],
    role: Role,
    recipe: Recipe,
    operation: Option<&str>,
    extra: &[(&str, Value)],
) -> Vec<Value> {
    let first = &sources[inputs[0]];
    let mut settings = serde_json::Map::new();
    settings.insert("layout".into(), first.layout.label().into());
    settings.insert("role".into(), role.as_str().into());
    for (key, value) in extra {
        settings.insert((*key).into(), value.clone());
    }
    let step = |by: &str| {
        json!({
            "tool": "OpenDAM",
            "toolVersion": tool_version(),
            "by": by,
            "from": first.extension(),
            "settings": settings,
        })
    };
    let mut steps: Vec<Value> = Vec::new();
    for input in inputs {
        let source = &sources[*input];
        if TO_PNG_FORMATS.contains(&source.extension().as_str()) {
            let mut to_png = step("to-png");
            to_png["from"] = source.extension().into();
            steps.push(to_png);
        }
    }
    steps.extend(operation.map(step));
    let inverts = matches!(
        recipe,
        Recipe::OrmMetallicSmoothness
            | Recipe::Merge {
                smoothness: Smoothness::Roughness,
                ..
            }
    );
    if inverts {
        steps.push(step("invert-roughness"));
    }
    if recipe == (Recipe::Same { flip_green: true }) {
        steps.push(step("flip-normal-green"));
    }
    steps
}

/// The outputs of one texture set's `sources` (C8): each source with one role gives that role,
/// passing through when it needs nothing, an ORM or MaskMap map gives an Occlusion and a
/// MetallicSmoothness, and the set's Roughness, Gloss and Metallic maps merge into one
/// MetallicSmoothness. A dds, exr or hdr source, and a PSD that needs any operation, is refused
/// for good (D4, D25), and so are a set's merge maps when it has two smoothness or two metallic
/// maps.
pub fn plan(sources: &[Source]) -> (Vec<Output>, Vec<Refused>) {
    let mut outputs = Vec::new();
    let mut refused = Vec::new();
    let mut smooth: Vec<usize> = Vec::new();
    let mut metallic: Vec<usize> = Vec::new();
    for (i, source) in sources.iter().enumerate() {
        let extension = source.extension();
        let bundled = match extension.as_str() {
            "tiff" => Some("tif"),
            ext => BUNDLE_FORMATS.iter().copied().find(|f| *f == ext),
        };
        if bundled.is_none() && !TO_PNG_FORMATS.contains(&extension.as_str()) {
            refused.push(Refused {
                sources: vec![i],
                message: format!(
                    "a .{extension} texture can't be exported: use png, tga, psd or tif (jpg, jpeg, bmp, webp and gif become PNG)"
                ),
            });
            continue;
        }
        if bundled == Some("psd") {
            if let Some(needs) = operations(source.layout, source.convention) {
                refused.push(Refused {
                    sources: vec![i],
                    message: format!(
                        "a PSD that needs converting can't be exported (it needs {needs})"
                    ),
                });
                continue;
            }
        }
        let mut output = |role, recipe, operation: Option<&str>| {
            let work = match (bundled, recipe) {
                (Some(extension), Recipe::Same { flip_green: false }) => {
                    Work::PassThrough { extension }
                }
                _ => Work::Convert {
                    recipe,
                    steps: steps(sources, &[i], role, recipe, operation, &[]),
                },
            };
            outputs.push(Output {
                role,
                inputs: vec![i],
                work,
            });
        };
        match source.layout {
            Layout::Role(role) => {
                let flip_green = role == Role::Normal && source.convention == Convention::DirectX;
                output(role, Recipe::Same { flip_green }, None);
            }
            Layout::Orm => {
                output(Role::Occlusion, Recipe::OrmOcclusion, Some("repack-orm"));
                let ms = Recipe::OrmMetallicSmoothness;
                output(Role::MetallicSmoothness, ms, Some("repack-orm"));
            }
            Layout::MaskMap => {
                output(Role::Occlusion, Recipe::MaskOcclusion, Some("repack-mask"));
                let ms = Recipe::MaskMetallicSmoothness;
                output(Role::MetallicSmoothness, ms, Some("repack-mask"));
            }
            Layout::Roughness | Layout::Gloss => smooth.push(i),
            Layout::Metallic => metallic.push(i),
        }
    }

    let mut clash = |maps: &[usize], what: &str| {
        let paths: Vec<&str> = maps
            .iter()
            .map(|i| sources[*i].source_path.as_str())
            .collect();
        refused.push(Refused {
            sources: maps.to_vec(),
            message: format!(
                "its texture set has {} {what} maps, {}, and merges one into its MetallicSmoothness: remove or rename the others",
                maps.len(),
                paths.join(" and ")
            ),
        });
    };
    if smooth.len() > 1 || metallic.len() > 1 {
        if smooth.len() > 1 {
            clash(&smooth, "roughness or gloss");
        }
        if metallic.len() > 1 {
            clash(&metallic, "metallic");
        }
    } else if !smooth.is_empty() || !metallic.is_empty() {
        let smoothness = match smooth.first().map(|i| sources[*i].layout) {
            Some(Layout::Roughness) => Smoothness::Roughness,
            Some(_) => Smoothness::Gloss,
            None => Smoothness::Half,
        };
        let inputs: Vec<usize> = smooth.iter().chain(&metallic).copied().collect();
        let recipe = Recipe::Merge {
            smoothness,
            metallic: !metallic.is_empty(),
        };
        let mut extra = Vec::new();
        match (smoothness, metallic.first()) {
            (Smoothness::Half, _) => extra.push(("smoothness", json!(0.5))),
            (_, Some(m)) => extra.push(("metallicSource", sources[*m].source_path.clone().into())),
            (_, None) => extra.push(("metallic", json!(0))),
        }
        let role = Role::MetallicSmoothness;
        outputs.push(Output {
            role,
            work: Work::Convert {
                recipe,
                steps: steps(
                    sources,
                    &inputs,
                    role,
                    recipe,
                    Some("merge-metallic-smoothness"),
                    &extra,
                ),
            },
            inputs,
        });
    }
    (outputs, refused)
}

/// Whether `image` has more than 8 bits per channel, so its output is 16 bit.
fn is_wide(image: &DynamicImage) -> bool {
    let color = image.color();
    color.bytes_per_pixel() > color.channel_count()
}

/// Whether `recipe`'s output has an alpha channel, given its first input.
fn has_alpha(recipe: Recipe, first: &DynamicImage) -> bool {
    match recipe {
        Recipe::Same { .. } => first.color().has_alpha(),
        Recipe::OrmOcclusion | Recipe::MaskOcclusion => false,
        Recipe::OrmMetallicSmoothness | Recipe::MaskMetallicSmoothness | Recipe::Merge { .. } => {
            true
        }
    }
}

/// `recipe`'s pixels from `inputs`, all of one size, as RGB or RGBA (`alpha`) samples.
fn pixels<T: Primitive>(recipe: Recipe, inputs: &[&[T]], alpha: bool) -> Vec<T> {
    let max = T::DEFAULT_MAX_VALUE;
    let half = max / (T::one() + T::one()) + T::one();
    let zero = T::zero();
    let count = inputs[0].len() / 4;
    let channels = if alpha { 4 } else { 3 };
    let mut out = Vec::with_capacity(count * channels);
    for p in 0..count {
        let at = |i: usize| -> &[T] { &inputs[i][p * 4..p * 4 + 4] };
        let [r, g, b, a] = match recipe {
            Recipe::Same { flip_green } => {
                let s = at(0);
                [s[0], if flip_green { max - s[1] } else { s[1] }, s[2], s[3]]
            }
            Recipe::OrmOcclusion => {
                let ao = at(0)[0];
                [ao, ao, ao, max]
            }
            Recipe::OrmMetallicSmoothness => {
                let s = at(0);
                [s[2], s[2], s[2], max - s[1]]
            }
            Recipe::MaskOcclusion => {
                let ao = at(0)[1];
                [ao, ao, ao, max]
            }
            Recipe::MaskMetallicSmoothness => {
                let s = at(0);
                [s[0], s[0], s[0], s[3]]
            }
            Recipe::Merge {
                smoothness,
                metallic,
            } => {
                let (smooth, next) = match smoothness {
                    Smoothness::Roughness => (max - at(0)[0], 1),
                    Smoothness::Gloss => (at(0)[0], 1),
                    Smoothness::Half => (half, 0),
                };
                let metal = if metallic { at(next)[0] } else { zero };
                [metal, metal, metal, smooth]
            }
        };
        out.extend_from_slice(&[r, g, b, a][..channels]);
    }
    out
}

/// Makes `recipe`'s output from `inputs`, at their resolution and at 16 bit when any of them
/// has more than 8 bits per channel, else 8 bit. Inputs of different sizes are refused: nothing
/// is resized (D25). A plain format change keeps the input's own channels.
pub fn apply(recipe: Recipe, inputs: &[DynamicImage]) -> Result<DynamicImage, String> {
    let first = inputs.first().ok_or("no input")?;
    let (width, height) = first.dimensions();
    if let Some(other) = inputs.iter().find(|i| i.dimensions() != (width, height)) {
        let (w, h) = other.dimensions();
        return Err(format!(
            "its maps differ in size ({width}x{height} and {w}x{h}), and nothing is resized"
        ));
    }
    if recipe == (Recipe::Same { flip_green: false }) {
        return Ok(match first {
            DynamicImage::ImageLuma8(_)
            | DynamicImage::ImageLumaA8(_)
            | DynamicImage::ImageRgb8(_)
            | DynamicImage::ImageRgba8(_)
            | DynamicImage::ImageLuma16(_)
            | DynamicImage::ImageLumaA16(_)
            | DynamicImage::ImageRgb16(_)
            | DynamicImage::ImageRgba16(_) => first.clone(),
            other => DynamicImage::ImageRgba16(other.to_rgba16()),
        });
    }
    let alpha = has_alpha(recipe, first);
    let built = if inputs.iter().any(is_wide) {
        let buffers: Vec<_> = inputs.iter().map(DynamicImage::to_rgba16).collect();
        let raw: Vec<&[u16]> = buffers.iter().map(|b| b.as_raw().as_slice()).collect();
        let samples = pixels(recipe, &raw, alpha);
        if alpha {
            ImageBuffer::from_raw(width, height, samples).map(DynamicImage::ImageRgba16)
        } else {
            ImageBuffer::from_raw(width, height, samples).map(DynamicImage::ImageRgb16)
        }
    } else {
        let buffers: Vec<_> = inputs.iter().map(DynamicImage::to_rgba8).collect();
        let raw: Vec<&[u8]> = buffers.iter().map(|b| b.as_raw().as_slice()).collect();
        let samples = pixels(recipe, &raw, alpha);
        if alpha {
            ImageBuffer::from_raw(width, height, samples).map(DynamicImage::ImageRgba8)
        } else {
            ImageBuffer::from_raw(width, height, samples).map(DynamicImage::ImageRgb8)
        }
    };
    built.ok_or_else(|| "its pixels don't fill the image".into())
}

/// `image` as PNG, with fixed encoder settings and no metadata, so the same pixels always give
/// the same bytes.
pub fn encode_png(image: &DynamicImage) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let encoder =
        PngEncoder::new_with_quality(&mut bytes, CompressionType::Default, FilterType::Adaptive);
    image
        .write_with_encoder(encoder)
        .map_err(|e| format!("it can't be encoded as PNG: {e}"))?;
    Ok(bytes)
}

/// Decodes `inputs`, each a file name and its bytes, and makes `recipe`'s output from them, as
/// PNG bytes. A name's extension gives the format of a file whose bytes don't tell it (TGA).
pub fn convert(recipe: Recipe, inputs: &[(&str, &[u8])]) -> Result<Vec<u8>, String> {
    let mut images = Vec::with_capacity(inputs.len());
    for (name, bytes) in inputs {
        let mut reader = ImageReader::new(Cursor::new(bytes));
        if let Ok(format) = ImageFormat::from_path(name) {
            reader.set_format(format);
        }
        let image = reader
            .with_guessed_format()
            .map_err(|e| format!("{name} can't be read: {e}"))?
            .decode()
            .map_err(|e| format!("{name} can't be decoded: {e}"))?;
        images.push(image);
    }
    encode_png(&apply(recipe, &images)?)
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
            layout: layout.map(|l| LayoutEdit::parse(l).unwrap()),
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
        assert_eq!(LayoutEdit::parse("Normal"), None);
        assert_eq!(LayoutEdit::of(Layout::Orm).name(), "ORM");
        assert_eq!(LayoutEdit::of(normal), LayoutEdit::PassThrough);
        assert_eq!(LayoutEdit::PassThrough.name(), PASS_THROUGH);
        assert_eq!(SourceEdits::inferred_role(Layout::Orm), Role::BaseColor);
        assert_eq!(SourceEdits::inferred_role(normal), Role::Normal);
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

    fn source(path: &str, layout: Layout, convention: Convention) -> Source {
        Source {
            source_path: path.into(),
            extension: path.rsplit_once('.').unwrap().1.into(),
            layout,
            convention,
        }
    }

    fn gl(path: &str, layout: Layout) -> Source {
        source(path, layout, Convention::OpenGl)
    }

    fn step(by: &str, from: &str, settings: Value) -> Value {
        json!({
            "tool": "OpenDAM",
            "toolVersion": "opendam-textures/1",
            "by": by,
            "from": from,
            "settings": settings,
        })
    }

    fn steps_of(output: &Output) -> &[Value] {
        match &output.work {
            Work::Convert { steps, .. } => steps,
            Work::PassThrough { .. } => &[],
        }
    }

    #[test]
    fn ready_textures_pass_through_and_a_tiff_is_renamed_tif() {
        let base = Layout::Role(Role::BaseColor);
        for (path, bundled) in [
            ("a.png", "png"),
            ("a.TGA", "tga"),
            ("a.psd", "psd"),
            ("a.tiff", "tif"),
            ("a.tif", "tif"),
        ] {
            let (outputs, refused) = plan(&[gl(path, base)]);
            assert_eq!(refused, []);
            assert_eq!(
                outputs,
                [Output {
                    role: Role::BaseColor,
                    inputs: vec![0],
                    work: Work::PassThrough { extension: bundled }
                }],
                "{path}"
            );
        }
        // An OpenGL normal map, and a DirectX convention on a role that isn't Normal
        let (outputs, _) = plan(&[
            gl("n.png", Layout::Role(Role::Normal)),
            source("e.png", Layout::Role(Role::Emission), Convention::DirectX),
        ]);
        assert!(outputs
            .iter()
            .all(|o| matches!(o.work, Work::PassThrough { .. })));
    }

    #[test]
    fn each_operation_is_one_step_in_order_naming_layout_role_and_extra_inputs() {
        let ms = Role::MetallicSmoothness;
        let (outputs, refused) = plan(&[
            gl("Rocks/T_Rock_R.jpg", Layout::Roughness),
            gl("Rocks/T_Rock_M.png", Layout::Metallic),
        ]);
        assert_eq!(refused, []);
        let merge = json!({"layout": "Roughness", "role": "MetallicSmoothness", "metallicSource": "Rocks/T_Rock_M.png"});
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].role, ms);
        assert_eq!(outputs[0].inputs, [0, 1], "the roughness map comes first");
        assert_eq!(outputs[0].extension(), "png");
        assert_eq!(
            steps_of(&outputs[0]),
            [
                step("to-png", "jpg", merge.clone()),
                step("merge-metallic-smoothness", "jpg", merge.clone()),
                step("invert-roughness", "jpg", merge),
            ]
        );

        let (outputs, _) = plan(&[gl("T_Slate_ORM.jpeg", Layout::Orm)]);
        let roles: Vec<Role> = outputs.iter().map(|o| o.role).collect();
        assert_eq!(roles, [Role::Occlusion, ms]);
        let occlusion = json!({"layout": "ORM", "role": "Occlusion"});
        let smoothness = json!({"layout": "ORM", "role": "MetallicSmoothness"});
        assert_eq!(
            steps_of(&outputs[0]),
            [
                step("to-png", "jpeg", occlusion.clone()),
                step("repack-orm", "jpeg", occlusion),
            ]
        );
        assert_eq!(
            steps_of(&outputs[1]),
            [
                step("to-png", "jpeg", smoothness.clone()),
                step("repack-orm", "jpeg", smoothness.clone()),
                step("invert-roughness", "jpeg", smoothness),
            ]
        );

        let (outputs, _) = plan(&[gl("T_Bricks_MaskMap.png", Layout::MaskMap)]);
        assert_eq!(
            steps_of(&outputs[1]),
            [step(
                "repack-mask",
                "png",
                json!({"layout": "MaskMap", "role": "MetallicSmoothness"})
            )]
        );

        let (outputs, _) = plan(&[source(
            "T_Rock_N_DX.bmp",
            Layout::Role(Role::Normal),
            Convention::DirectX,
        )]);
        let normal = json!({"layout": "Normal", "role": "Normal"});
        assert_eq!(
            steps_of(&outputs[0]),
            [
                step("to-png", "bmp", normal.clone()),
                step("flip-normal-green", "bmp", normal),
            ]
        );

        let (outputs, _) = plan(&[gl("T_Wall_D.jpg", Layout::Role(Role::BaseColor))]);
        assert_eq!(
            steps_of(&outputs[0]),
            [step(
                "to-png",
                "jpg",
                json!({"layout": "BaseColor", "role": "BaseColor"})
            )]
        );
    }

    #[test]
    fn a_lone_metallic_gets_half_smoothness_and_a_lone_smoothness_map_no_metal() {
        let (outputs, _) = plan(&[gl("T_Ore_M.png", Layout::Metallic)]);
        let settings =
            json!({"layout": "Metallic", "role": "MetallicSmoothness", "smoothness": 0.5});
        assert_eq!(
            steps_of(&outputs[0]),
            [step("merge-metallic-smoothness", "png", settings)]
        );
        let Work::Convert { recipe, .. } = outputs[0].work else {
            panic!("a merge converts")
        };
        assert_eq!(
            recipe,
            Recipe::Merge {
                smoothness: Smoothness::Half,
                metallic: true
            }
        );

        let (outputs, _) = plan(&[gl("T_Ore_G.png", Layout::Gloss)]);
        let settings = json!({"layout": "Gloss", "role": "MetallicSmoothness", "metallic": 0});
        assert_eq!(
            steps_of(&outputs[0]),
            [step("merge-metallic-smoothness", "png", settings)],
            "gloss is not inverted"
        );
    }

    #[test]
    fn unsupported_formats_psds_needing_work_and_two_merge_maps_are_refused() {
        let base = Layout::Role(Role::BaseColor);
        for ext in ["dds", "exr", "hdr", "ktx", "svg"] {
            let (outputs, refused) = plan(&[gl(&format!("a.{ext}"), base)]);
            assert_eq!(outputs, []);
            assert_eq!(refused.len(), 1, "{ext}");
            assert!(refused[0]
                .message
                .starts_with(&format!("a .{ext} texture can't be exported")));
        }
        for layout in [Layout::Orm, Layout::Roughness, Layout::Metallic] {
            let (outputs, refused) = plan(&[gl("a.psd", layout)]);
            assert_eq!(outputs, [], "{layout:?}");
            assert!(refused[0]
                .message
                .starts_with("a PSD that needs converting can't be exported"));
        }
        let normal = Layout::Role(Role::Normal);
        let (_, refused) = plan(&[source("n.psd", normal, Convention::DirectX)]);
        assert_eq!(
            refused[0].message,
            "a PSD that needs converting can't be exported (it needs its DirectX normal map's green channel flipped)"
        );

        let (outputs, refused) = plan(&[
            gl("T_Rock_D.png", base),
            gl("T_Rock_R.png", Layout::Roughness),
            gl("T_Rock_Gloss.png", Layout::Gloss),
            gl("T_Rock_M.png", Layout::Metallic),
        ]);
        assert_eq!(outputs.len(), 1, "the base color still exports");
        assert_eq!(
            refused,
            [Refused {
                sources: vec![1, 2],
                message: "its texture set has 2 roughness or gloss maps, T_Rock_R.png and T_Rock_Gloss.png, and merges one into its MetallicSmoothness: remove or rename the others".into()
            }]
        );
    }

    fn rgba8(pixels: &[[u8; 4]]) -> DynamicImage {
        let raw = pixels.concat();
        DynamicImage::ImageRgba8(ImageBuffer::from_raw(pixels.len() as u32, 1, raw).unwrap())
    }

    fn luma8(pixels: &[u8]) -> DynamicImage {
        DynamicImage::ImageLuma8(
            ImageBuffer::from_raw(pixels.len() as u32, 1, pixels.to_vec()).unwrap(),
        )
    }

    fn rgb16(pixels: &[[u16; 3]]) -> DynamicImage {
        let raw = pixels.concat();
        DynamicImage::ImageRgb16(ImageBuffer::from_raw(pixels.len() as u32, 1, raw).unwrap())
    }

    /// `recipe` applied to `inputs`, through PNG bytes and back, as the bundle file holds it.
    fn round_trip(recipe: Recipe, inputs: &[DynamicImage]) -> DynamicImage {
        let png = encode_png(&apply(recipe, inputs).unwrap()).unwrap();
        image::load_from_memory_with_format(&png, image::ImageFormat::Png).unwrap()
    }

    #[test]
    fn an_orm_map_splits_into_occlusion_and_metallic_smoothness_pixel_exact() {
        let orm = rgba8(&[[10, 20, 30, 255], [200, 100, 50, 7]]);
        let occlusion = round_trip(Recipe::OrmOcclusion, std::slice::from_ref(&orm));
        assert_eq!(occlusion.color(), image::ColorType::Rgb8);
        assert_eq!(occlusion.as_bytes(), [10, 10, 10, 200, 200, 200]);
        let ms = round_trip(Recipe::OrmMetallicSmoothness, &[orm]);
        assert_eq!(ms.color(), image::ColorType::Rgba8);
        assert_eq!(ms.as_bytes(), [30, 30, 30, 235, 50, 50, 50, 155]);
    }

    #[test]
    fn sixteen_bit_sources_give_sixteen_bit_outputs() {
        let orm = rgb16(&[[1000, 2000, 3000]]);
        let ms = round_trip(Recipe::OrmMetallicSmoothness, &[orm]);
        let DynamicImage::ImageRgba16(ms) = ms else {
            panic!("16 bit in, 16 bit out")
        };
        assert_eq!(ms.as_raw(), &[3000, 3000, 3000, 63535]);

        let metallic = DynamicImage::ImageLuma16(ImageBuffer::from_raw(1, 1, vec![40000]).unwrap());
        let lone = Recipe::Merge {
            smoothness: Smoothness::Half,
            metallic: true,
        };
        let DynamicImage::ImageRgba16(ms) = round_trip(lone, &[metallic]) else {
            panic!("16 bit in, 16 bit out")
        };
        assert_eq!(ms.as_raw(), &[40000, 40000, 40000, 32768]);
    }

    #[test]
    fn a_mask_map_splits_into_metallic_smoothness_and_occlusion_pixel_exact() {
        let mask = rgba8(&[[10, 20, 30, 40], [250, 5, 0, 255]]);
        let ms = round_trip(Recipe::MaskMetallicSmoothness, std::slice::from_ref(&mask));
        assert_eq!(ms.as_bytes(), [10, 10, 10, 40, 250, 250, 250, 255]);
        let occlusion = round_trip(Recipe::MaskOcclusion, &[mask]);
        assert_eq!(occlusion.as_bytes(), [20, 20, 20, 5, 5, 5]);
    }

    #[test]
    fn roughness_is_inverted_gloss_is_not_and_a_lone_metallic_gets_half_smoothness() {
        let roughness = luma8(&[0, 255, 100]);
        let metallic = luma8(&[255, 0, 7]);
        let merged = round_trip(
            Recipe::Merge {
                smoothness: Smoothness::Roughness,
                metallic: true,
            },
            &[roughness.clone(), metallic.clone()],
        );
        assert_eq!(
            merged.as_bytes(),
            [255, 255, 255, 255, 0, 0, 0, 0, 7, 7, 7, 155]
        );
        let rough_only = round_trip(
            Recipe::Merge {
                smoothness: Smoothness::Roughness,
                metallic: false,
            },
            &[roughness],
        );
        assert_eq!(
            rough_only.as_bytes(),
            [0, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 155]
        );
        let gloss = round_trip(
            Recipe::Merge {
                smoothness: Smoothness::Gloss,
                metallic: true,
            },
            &[luma8(&[0, 255, 100]), metallic.clone()],
        );
        assert_eq!(
            gloss.as_bytes(),
            [255, 255, 255, 0, 0, 0, 0, 255, 7, 7, 7, 100]
        );
        let lone = round_trip(
            Recipe::Merge {
                smoothness: Smoothness::Half,
                metallic: true,
            },
            &[metallic],
        );
        assert_eq!(
            lone.as_bytes(),
            [255, 255, 255, 128, 0, 0, 0, 128, 7, 7, 7, 128]
        );
    }

    #[test]
    fn a_directx_normal_map_has_its_green_flipped_pixel_exact() {
        let normal = rgba8(&[[128, 127, 255, 255], [0, 0, 10, 99]]);
        let flipped = round_trip(Recipe::Same { flip_green: true }, &[normal]);
        assert_eq!(flipped.color(), image::ColorType::Rgba8, "alpha is kept");
        assert_eq!(flipped.as_bytes(), [128, 128, 255, 255, 0, 255, 10, 99]);

        let rgb = rgb16(&[[1, 2, 3]]);
        let DynamicImage::ImageRgb16(flipped) =
            round_trip(Recipe::Same { flip_green: true }, &[rgb])
        else {
            panic!("16 bit RGB in, 16 bit RGB out")
        };
        assert_eq!(flipped.as_raw(), &[1, 65533, 3]);
    }

    #[test]
    fn a_jpg_becomes_a_png_of_the_same_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let source = rgba8(&[[200, 10, 30, 255], [5, 250, 90, 255]]).to_rgb8();
        let jpg = dir.path().join("T_Wall_D.jpg");
        source.save(&jpg).unwrap();
        let decoded = image::open(&jpg).unwrap();

        let bytes = std::fs::read(&jpg).unwrap();
        let png = convert(
            Recipe::Same { flip_green: false },
            &[("T_Wall_D.jpg", &bytes)],
        )
        .unwrap();
        let output = image::load_from_memory_with_format(&png, image::ImageFormat::Png).unwrap();
        assert_eq!(output.color(), decoded.color());
        assert_eq!(output.as_bytes(), decoded.as_bytes());
    }

    #[test]
    fn png_output_is_deterministic() {
        let orm = rgba8(&[[10, 20, 30, 255], [200, 100, 50, 7], [1, 2, 3, 4]]);
        let first =
            encode_png(&apply(Recipe::OrmMetallicSmoothness, std::slice::from_ref(&orm)).unwrap());
        let second = encode_png(&apply(Recipe::OrmMetallicSmoothness, &[orm]).unwrap());
        assert_eq!(first, second);
    }

    #[test]
    fn maps_of_different_sizes_are_refused_not_resized() {
        let recipe = Recipe::Merge {
            smoothness: Smoothness::Roughness,
            metallic: true,
        };
        assert_eq!(
            apply(recipe, &[luma8(&[1, 2]), luma8(&[1, 2, 3])]).unwrap_err(),
            "its maps differ in size (2x1 and 3x1), and nothing is resized"
        );
    }

    #[test]
    fn a_set_with_two_metallic_maps_merges_neither() {
        let (outputs, refused) = plan(&[
            gl("T_Rock_R.png", Layout::Roughness),
            gl("T_Rock_M.png", Layout::Metallic),
            gl("T_Rock_Metallic.png", Layout::Metallic),
        ]);
        assert!(outputs.iter().all(|o| o.role != Role::MetallicSmoothness));
        assert_eq!(
            refused,
            [Refused {
                sources: vec![1, 2],
                message: "its texture set has 2 metallic maps, T_Rock_M.png and T_Rock_Metallic.png, and merges one into its MetallicSmoothness: remove or rename the others".into()
            }]
        );
    }

    #[test]
    fn a_tga_is_decoded_by_its_name_and_converted() {
        let normal = rgba8(&[[128, 127, 255, 255], [0, 0, 10, 99]]);
        let mut tga = Vec::new();
        normal
            .write_to(&mut Cursor::new(&mut tga), ImageFormat::Tga)
            .unwrap();
        // TGA bytes start with no magic number, so only the name tells the format
        assert!(image::guess_format(&tga).is_err());

        let png = convert(
            Recipe::Same { flip_green: true },
            &[("T_Wall_N_DX.tga", &tga)],
        )
        .unwrap();
        let output = image::load_from_memory_with_format(&png, ImageFormat::Png).unwrap();
        assert_eq!(output.color(), image::ColorType::Rgba8);
        assert_eq!(output.as_bytes(), [128, 128, 255, 255, 0, 255, 10, 99]);
    }

    #[test]
    fn an_undecodable_input_is_an_error_naming_it() {
        let broken: &[u8] = b"not a png";
        let error = convert(
            Recipe::Same { flip_green: true },
            &[("T_Rock_R.png", broken)],
        )
        .unwrap_err();
        assert!(
            error.starts_with("T_Rock_R.png can't be decoded"),
            "{error}"
        );
    }
}
