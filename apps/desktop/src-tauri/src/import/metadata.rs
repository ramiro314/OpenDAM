//! The metadata step (plan pack-import, Pack metadata): writes a pack's `datapackage.json` from
//! its preset and the fields set at import, as the owner's `gen_datapackage.py` and
//! `gen_kenney_datapackage.py` write it.
//!
//! - **Unity and Fab.** The pack folder's name is the title, the publisher is the one set at
//!   import (else the folder above the pack), and the license is the Unity Asset Store EULA or
//!   the Fab Standard License.
//! - **Kenney.** The title and version come from the license header (`Car Kit (3.1)`, or
//!   `Fish Pack 2.0` in older packs), and CC0 is written only when the license text names it.
//!   With `lookup_store_page`, one or two HEAD requests to `https://kenney.nl/assets/<slug>`
//!   find the store page, written as the source only on a 200. The answer is kept in
//!   `import_packs.store_page`, outside the recipe (`build::store_page`).
//! - **None.** The title, the publisher and whatever fields were set.
//!
//! A field set at import is a step option, so every rebuild writes it again. The file's one
//! resource is the pack's first model by path ignoring case, else its first file.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures_util::future::{BoxFuture, FutureExt};
use serde::{Deserialize, Serialize};

use super::unitypackage::ExtractedFile;
use crate::models::AssetKind;

/// The file the step writes, at the pack root.
pub const FILE: &str = "datapackage.json";
const SCHEMA: &str = "https://datapackage.org/profiles/2.0/datapackage.json";
const STORE: &str = "https://kenney.nl/assets/";
const KENNEY: &str = "Kenney";
const KENNEY_HOME: &str = "https://kenney.nl/";
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    Unity,
    Fab,
    Kenney,
    None,
}

/// A license as `datapackage.json` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct License {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// The `license` option: a license name (`CC-BY-4.0`), or a whole license entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LicenseOption {
    Name(String),
    Entry(License),
}

/// A source as `datapackage.json` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub path: String,
}

/// The metadata step's options.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataStep {
    pub preset: Preset,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub publisher: Option<String>,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub license: Option<LicenseOption>,
    #[serde(default)]
    pub sources: Vec<Source>,
    /// Look the Kenney store page up (the Kenney preset only).
    #[serde(default)]
    pub lookup_store_page: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Contributor {
    pub title: String,
    pub roles: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Resource {
    pub name: String,
    pub path: String,
}

/// A `datapackage.json`, its fields in the scripts' order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Package {
    #[serde(rename = "$schema")]
    pub schema: &'static str,
    pub name: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contributors: Vec<Contributor>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub licenses: Vec<License>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<Source>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<Resource>,
}

impl Package {
    /// The file's bytes, as the scripts write them (`json.dumps(indent=2, ensure_ascii=False)`
    /// and a newline).
    pub fn render(&self) -> String {
        serde_json::to_string_pretty(self).expect("a package serializes") + "\n"
    }
}

/// What a Kenney license file's header says.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Header {
    pub title: Option<String>,
    pub version: Option<String>,
    /// Whether the text names CC0.
    pub cc0: bool,
}

/// A Kenney pack's license file: its name at the pack root and its header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KenneyLicense {
    pub file: String,
    pub header: Header,
}

fn trimmed(text: &Option<String>) -> Option<&str> {
    text.as_deref().map(str::trim).filter(|t| !t.is_empty())
}

impl MetadataStep {
    /// The pack's title: the one set, else the license header's, else the pack folder's name.
    pub fn title(&self, folder: &str, kenney: Option<&KenneyLicense>) -> String {
        trimmed(&self.title)
            .map(str::to_string)
            .or_else(|| kenney.and_then(|k| k.header.title.clone()))
            .unwrap_or_else(|| folder.to_string())
    }

    /// The `datapackage.json` of the pack at `target` (relative to the library root) holding
    /// `files`, with its Kenney license and the store page found, if any.
    pub fn package(
        &self,
        target: &str,
        files: &[ExtractedFile],
        kenney: Option<&KenneyLicense>,
        store_page: Option<&str>,
    ) -> Package {
        let (parent, folder) = match target.rsplit_once('/') {
            Some((parent, folder)) => (parent.rsplit('/').next(), folder),
            None => (None, target),
        };
        let title = self.title(folder, kenney);
        let version = trimmed(&self.version)
            .map(str::to_string)
            .or_else(|| kenney.and_then(|k| k.header.version.clone()));
        let publisher = trimmed(&self.publisher).or(match self.preset {
            Preset::Unity | Preset::Fab => parent,
            Preset::Kenney => Some(KENNEY),
            Preset::None => None,
        });
        let mut authors: Vec<&str> = self
            .authors
            .iter()
            .map(|a| a.trim())
            .filter(|a| !a.is_empty())
            .collect();
        if self.preset == Preset::Kenney && authors.is_empty() {
            authors.extend(publisher);
        }
        let mut contributors: Vec<Contributor> = Vec::new();
        for author in authors {
            contributors.push(Contributor {
                title: author.to_string(),
                roles: vec!["author"],
            });
        }
        if let Some(publisher) = publisher {
            match contributors.iter_mut().find(|c| c.title == publisher) {
                Some(contributor) => contributor.roles.push("publisher"),
                None => contributors.push(Contributor {
                    title: publisher.to_string(),
                    roles: vec!["publisher"],
                }),
            }
        }
        let preset_license = match self.preset {
            Preset::Unity => Some(License {
                name: "LicenseRef-Unity-Asset-Store-EULA".into(),
                title: Some("Standard Unity Asset Store EULA".into()),
                path: Some("https://unity.com/legal/as-terms".into()),
            }),
            Preset::Fab => Some(License {
                name: "LicenseRef-Fab-Standard-License".into(),
                title: Some("Fab Standard License".into()),
                path: Some("https://www.fab.com/eula".into()),
            }),
            Preset::Kenney => kenney.filter(|k| k.header.cc0).map(|k| License {
                name: "CC0-1.0".into(),
                title: Some("Creative Commons Zero v1.0 Universal".into()),
                path: Some(k.file.clone()),
            }),
            Preset::None => None,
        };
        let license = match &self.license {
            Some(LicenseOption::Name(name)) if !name.trim().is_empty() => Some(License {
                name: name.trim().to_string(),
                title: None,
                path: None,
            }),
            Some(LicenseOption::Entry(entry)) => Some(entry.clone()),
            _ => preset_license,
        };
        let mut sources = self.sources.clone();
        if let Some(url) = store_page.filter(|_| self.looks_up()) {
            if !sources.iter().any(|s| s.path == url) {
                sources.push(Source {
                    title: Some(KENNEY.into()),
                    path: url.to_string(),
                });
            }
        }
        Package {
            schema: SCHEMA,
            name: slug(&title),
            title,
            version,
            contributors,
            licenses: license.into_iter().collect(),
            sources,
            homepage: (self.preset == Preset::Kenney).then_some(KENNEY_HOME),
            resources: resource(files).into_iter().collect(),
        }
    }

    /// Whether the step looks the Kenney store page up.
    pub fn looks_up(&self) -> bool {
        self.preset == Preset::Kenney && self.lookup_store_page
    }
}

/// The scripts' `slug`: lowercase, every run of characters other than `a-z`, `0-9`, `.`, `_`
/// and `-` (and of `-`) one `-`, with no `-` at either end.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.to_lowercase().chars() {
        let keep = c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_');
        if keep {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// The pack's one resource: its first model by path ignoring case, else its first file,
/// leaving out dot files, files in dot folders and `datapackage.json`.
fn resource(files: &[ExtractedFile]) -> Option<Resource> {
    let mut paths: Vec<&str> = files
        .iter()
        .map(|f| f.path.as_str())
        .filter(|p| !p.split('/').any(|part| part.starts_with('.')))
        .filter(|p| p.to_lowercase() != FILE)
        .collect();
    paths.sort_by_cached_key(|p| (p.to_lowercase(), p.to_string()));
    let is_model = |p: &&str| {
        let name = p.rsplit('/').next().unwrap_or(p);
        std::path::Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| AssetKind::from_extension(e) == AssetKind::Model3d)
    };
    let path = paths
        .iter()
        .copied()
        .find(is_model)
        .or_else(|| paths.first().copied())?
        .to_string();
    let name = slug(path.rsplit('/').next().unwrap_or(&path));
    Some(Resource { name, path })
}

/// A file the original put where the step writes `datapackage.json`, in any ASCII case.
pub fn clash(files: &[ExtractedFile]) -> Option<&str> {
    files
        .iter()
        .map(|f| f.path.as_str())
        .find(|p| crate::structure::is_package_file(p))
}

/// The pack's Kenney license file: the first by name of the files at its root named
/// `License*.txt` or `Licence*.txt`, ignoring case.
pub fn license_file(files: &[ExtractedFile]) -> Option<&str> {
    files
        .iter()
        .map(|f| f.path.as_str())
        .filter(|p| !p.contains('/'))
        .filter(|p| {
            let name = p.to_lowercase();
            (name.starts_with("license") || name.starts_with("licence"))
                && name[7..].ends_with(".txt")
        })
        .min()
}

/// The title and version a Kenney license header gives, and whether the text names CC0.
pub fn header(text: &str) -> Header {
    // Python's `str.splitlines`, then `strip`, dropping blank lines.
    let lines: Vec<&str> = text
        .split([
            '\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\u{85}', '\u{2028}', '\u{2029}',
        ])
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let mut found = Header::default();
    if let [first, second, ..] = lines[..] {
        let by = second.to_lowercase();
        if by.starts_with("created/distributed by kenney") || by.starts_with("by kenney") {
            let (title, version) = title_and_version(first);
            found.title = Some(title.trim())
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            found.version = version.map(str::to_string);
        }
    }
    let lower = text.to_lowercase();
    found.cc0 = lower.contains("cc0") || lower.contains("publicdomain/zero");
    found
}

/// `Car Kit (3.1)` or `Fish Pack 2.0` as its title and version: the script's
/// `(.*?)(?:\s*\(([\d.]+)\)|\s+v?(\d+(?:\.\d+)+))?` over the whole line, with the shortest title
/// whose rest is a version.
fn title_and_version(line: &str) -> (&str, Option<&str>) {
    for (at, _) in line.char_indices() {
        if let Some(version) = version_suffix(&line[at..]) {
            return (&line[..at], Some(version));
        }
    }
    (line, None)
}

/// The version in `rest` when all of it is ` (3.1)` or ` v2.0`.
fn version_suffix(rest: &str) -> Option<&str> {
    let after_space = rest.trim_start();
    let in_parens = after_space
        .strip_prefix('(')
        .and_then(|r| r.strip_suffix(')'))
        .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.'));
    if in_parens.is_some() {
        return in_parens;
    }
    if after_space.len() == rest.len() {
        return None;
    }
    let dotted = |v: &str| {
        let parts: Vec<&str> = v.split('.').collect();
        parts.len() >= 2
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    };
    [after_space.strip_prefix('v'), Some(after_space)]
        .into_iter()
        .flatten()
        .find(|v| dotted(v))
}

/// The store page URLs to try, by each name's slug, without repeats. A name whose slug is empty
/// (no `a-z`, `0-9`, `.` or `_` in it) has none: it would ask for the store's index page.
pub fn store_urls(names: &[&str]) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    for slug in names
        .iter()
        .map(|name| slug(name))
        .filter(|s| !s.is_empty())
    {
        let url = format!("{STORE}{slug}");
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    urls
}

/// A store page lookup's answer, as `import_packs.store_page` keeps it (JSON): the URLs it
/// tried and the page found among them, if any. It holds only while a build would try the same
/// URLs, so a changed title looks the page up again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorePage {
    pub urls: Vec<String>,
    pub page: Option<String>,
}

impl StorePage {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a store page serializes")
    }
}

/// A HEAD request: the status it got, or why it got none.
pub type Head = Arc<dyn Fn(String) -> BoxFuture<'static, Result<u16, String>> + Send + Sync>;

/// What looking the store page up found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// The first URL that answered 200.
    Found(String),
    /// Every URL answered that it has no page (404 or 410).
    Miss,
    /// No URL answered 200, and one got no answer or another status (a 503, a 429), so another
    /// try could find it.
    Unknown,
}

/// Tries each of `urls` in order with `head` until one answers 200.
pub async fn look_up(head: &Head, urls: &[String]) -> Lookup {
    let mut answered = true;
    for url in urls {
        match head(url.clone()).await {
            Ok(200) => return Lookup::Found(url.clone()),
            Ok(404 | 410) => {}
            Ok(status) => {
                tracing::info!("{url} answered {status}");
                answered = false;
            }
            Err(e) => {
                tracing::info!("No answer from {url}: {e}");
                answered = false;
            }
        }
    }
    if answered {
        Lookup::Miss
    } else {
        Lookup::Unknown
    }
}

/// A HEAD request over HTTPS, with the script's 15 s timeout and user agent.
pub fn http_head() -> Head {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    Arc::new(|url: String| {
        async move {
            let client = CLIENT
                .get_or_init(|| {
                    // reqwest's rustls needs a process-wide provider; installing it twice is
                    // harmless
                    let _ = rustls::crypto::ring::default_provider().install_default();
                    reqwest::Client::builder()
                        .timeout(LOOKUP_TIMEOUT)
                        .user_agent("Mozilla/5.0")
                        .build()
                        .map_err(|e| format!("Could not set up HTTPS: {e}"))
                })
                .as_ref()
                .map_err(Clone::clone)?;
            let response = client.head(&url).send().await.map_err(|e| e.to_string())?;
            Ok(response.status().as_u16())
        }
        .boxed()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_the_scripts() {
        assert_eq!(slug("Car Kit"), "car-kit");
        assert_eq!(
            slug("POLYGON City Pack - Art by Synty"),
            "polygon-city-pack-art-by-synty"
        );
        assert_eq!(slug("  -Ünïcode_v1.2!- "), "n-code_v1.2");
        assert_eq!(slug("SM_Log.fbx"), "sm_log.fbx");
    }

    #[test]
    fn a_kenney_header_gives_its_title_and_version() {
        let text = "\n\n\tCar Kit (3.1)\n\n\tCreated/distributed by Kenney (www.kenney.nl)\n\
                    \tLicense: (Creative Commons Zero, CC0)\n";
        assert_eq!(
            header(text),
            Header {
                title: Some("Car Kit".into()),
                version: Some("3.1".into()),
                cc0: true
            }
        );
        let old = "Fish Pack 2.0\r\nby Kenney\r\nhttp://creativecommons.org/publicdomain/zero/1.0/";
        assert_eq!(
            header(old),
            Header {
                title: Some("Fish Pack".into()),
                version: Some("2.0".into()),
                cc0: true
            }
        );
        let v = header("Space Kit v1.0.2\nBy Kenney\nAll rights reserved");
        assert_eq!(
            (v.title.as_deref(), v.version.as_deref(), v.cc0),
            (Some("Space Kit"), Some("1.0.2"), false)
        );
        // No version, and a single number isn't one.
        let plain = header("Tiles 2\nby Kenney\n");
        assert_eq!(
            (plain.title.as_deref(), plain.version),
            (Some("Tiles 2"), None)
        );
        // Not a Kenney header: neither title nor version.
        let other = header("Some Pack (1.0)\nby someone else\nCC0");
        assert_eq!((other.title, other.version, other.cc0), (None, None, true));
    }

    fn files(paths: &[&str]) -> Vec<ExtractedFile> {
        paths
            .iter()
            .map(|p| ExtractedFile {
                path: p.to_string(),
                source_path: vec![p.to_string()],
                size: 1,
            })
            .collect()
    }

    #[test]
    fn the_resource_is_the_first_model_else_the_first_file() {
        let pack = files(&["b.png", "Models/Zed.glb", "models/apple.FBX", ".x/a.fbx"]);
        assert_eq!(resource(&pack).unwrap().path, "models/apple.FBX");
        let flat = files(&["Tiles/b.png", "License.txt", "datapackage.json", ".hidden"]);
        let first = resource(&flat).unwrap();
        assert_eq!(
            (first.name.as_str(), first.path.as_str()),
            ("license.txt", "License.txt")
        );
        assert_eq!(resource(&[]), None);
    }

    #[test]
    fn an_original_s_own_datapackage_json_at_the_root_clashes() {
        assert_eq!(clash(&files(&["Sub/datapackage.json", "a.txt"])), None);
        assert_eq!(
            clash(&files(&["a.txt", "DataPackage.JSON"])),
            Some("DataPackage.JSON")
        );
    }

    #[test]
    fn the_license_file_is_at_the_pack_root() {
        let pack = files(&[
            "Sub/License.txt",
            "LICENCE-cc0.TXT",
            "License.txt",
            "Licenses",
        ]);
        assert_eq!(license_file(&pack), Some("LICENCE-cc0.TXT"));
        assert_eq!(license_file(&files(&["Sub/License.txt"])), None);
    }

    #[test]
    fn fields_set_win_and_merge_with_the_preset() {
        let step: MetadataStep = serde_json::from_value(serde_json::json!({
            "preset": "fab",
            "publisher": "Leartes Studios",
            "authors": ["Ann", "Leartes Studios"],
            "version": "2.1",
            "license": "CC-BY-4.0",
            "sources": [{"path": "https://example.com/pack"}],
        }))
        .unwrap();
        let package = step.package("Stylarts/Ruins", &files(&["a.fbx"]), None, None);
        assert_eq!(package.title, "Ruins");
        assert_eq!(package.version.as_deref(), Some("2.1"));
        assert_eq!(
            package.contributors,
            [
                Contributor {
                    title: "Ann".into(),
                    roles: vec!["author"]
                },
                Contributor {
                    title: "Leartes Studios".into(),
                    roles: vec!["author", "publisher"]
                }
            ]
        );
        assert_eq!(package.licenses[0].name, "CC-BY-4.0");
        assert_eq!(package.sources[0].path, "https://example.com/pack");

        // Without a publisher, Unity and Fab name the folder above the pack, as the script does.
        let bare: MetadataStep =
            serde_json::from_value(serde_json::json!({"preset": "unity"})).unwrap();
        let package = bare.package("Unity/Synty/City", &files(&["a.fbx"]), None, None);
        assert_eq!(package.contributors[0].title, "Synty");
        // A store page is written only for a Kenney lookup.
        let package = bare.package("City", &files(&["a.fbx"]), None, Some("https://x"));
        assert!(package.contributors.is_empty() && package.sources.is_empty());
    }

    #[test]
    fn a_kenney_pack_gets_cc0_only_when_its_license_names_it() {
        let step: MetadataStep =
            serde_json::from_value(serde_json::json!({"preset": "kenney"})).unwrap();
        let license = |cc0| KenneyLicense {
            file: "License.txt".into(),
            header: Header {
                title: Some("Car Kit".into()),
                version: Some("3.1".into()),
                cc0,
            },
        };
        let pack = files(&["License.txt", "car.glb"]);
        let named = step.package("3D/Car Kit", &pack, Some(&license(true)), None);
        assert_eq!(
            named.licenses,
            [License {
                name: "CC0-1.0".into(),
                title: Some("Creative Commons Zero v1.0 Universal".into()),
                path: Some("License.txt".into()),
            }]
        );
        let unnamed = step.package("3D/Car Kit", &pack, Some(&license(false)), None);
        assert!(unnamed.licenses.is_empty(), "{:?}", unnamed.licenses);
        assert_eq!(unnamed.title, "Car Kit");
        assert_eq!(unnamed.version.as_deref(), Some("3.1"));
        // Without a license file there's no header and no license.
        let none = step.package("3D/Car Kit", &files(&["car.glb"]), None, None);
        assert!(none.licenses.is_empty());
        assert_eq!((none.title.as_str(), none.version), ("Car Kit", None));
    }

    #[test]
    fn the_none_preset_writes_only_what_was_set() {
        let pack = files(&["Docs/readme.txt", "a.fbx"]);
        let bare: MetadataStep =
            serde_json::from_value(serde_json::json!({"preset": "none"})).unwrap();
        let package = bare.package("Pub/Pack", &pack, None, None);
        assert_eq!(
            package.render(),
            "{\n  \"$schema\": \"https://datapackage.org/profiles/2.0/datapackage.json\",\n  \
             \"name\": \"pack\",\n  \"title\": \"Pack\",\n  \"resources\": [\n    {\n      \
             \"name\": \"a.fbx\",\n      \"path\": \"a.fbx\"\n    }\n  ]\n}\n",
            "no publisher from the folder above, no license, no homepage, no author"
        );

        let set: MetadataStep = serde_json::from_value(serde_json::json!({
            "preset": "none",
            "publisher": "Pub Co",
            "license": "MIT",
        }))
        .unwrap();
        let package = set.package("Pub/Pack", &pack, None, Some("https://kenney.nl/x"));
        assert_eq!(
            package.contributors,
            [Contributor {
                title: "Pub Co".into(),
                roles: vec!["publisher"]
            }]
        );
        assert_eq!(
            package.licenses,
            [License {
                name: "MIT".into(),
                title: None,
                path: None
            }]
        );
        assert_eq!(package.homepage, None);
        assert!(package.sources.is_empty(), "no store page outside Kenney");
    }

    #[test]
    fn unknown_options_are_refused() {
        let step = serde_json::from_value::<MetadataStep>(serde_json::json!({
            "preset": "kenney", "lookup": true
        }));
        assert!(step.is_err());
        let preset = serde_json::from_value::<MetadataStep>(serde_json::json!({"preset": "x"}));
        assert!(preset.is_err());
    }

    #[tokio::test]
    async fn a_lookup_needs_an_answer_to_be_a_miss() {
        let urls = store_urls(&["Car Kit", "car kit", "Car-Kit 2"]);
        assert_eq!(
            urls,
            [
                "https://kenney.nl/assets/car-kit",
                "https://kenney.nl/assets/car-kit-2"
            ]
        );
        // A name with no slug asks for no page, not for the store's index.
        assert_eq!(
            store_urls(&["車キット", "Car Kit"]),
            ["https://kenney.nl/assets/car-kit"]
        );
        assert!(store_urls(&["車キット", " -!- "]).is_empty());
        let answers = |first: Result<u16, String>, second: Result<u16, String>| -> Head {
            Arc::new(move |url: String| {
                let answer = if url.ends_with("-2") {
                    second.clone()
                } else {
                    first.clone()
                };
                async move { answer }.boxed()
            })
        };
        assert_eq!(
            look_up(&answers(Ok(404), Ok(200)), &urls).await,
            Lookup::Found(urls[1].clone())
        );
        assert_eq!(
            look_up(&answers(Ok(404), Ok(404)), &urls).await,
            Lookup::Miss
        );
        assert_eq!(
            look_up(&answers(Ok(410), Ok(404)), &urls).await,
            Lookup::Miss
        );
        assert_eq!(
            look_up(&answers(Err("offline".into()), Ok(404)), &urls).await,
            Lookup::Unknown
        );
        // A server error or a rate limit is no answer about the page.
        for status in [503, 500, 429, 408, 403] {
            assert_eq!(
                look_up(&answers(Ok(404), Ok(status)), &urls).await,
                Lookup::Unknown,
                "{status}"
            );
        }
    }
}
