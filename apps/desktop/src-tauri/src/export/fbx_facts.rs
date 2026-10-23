//! An FBX's export facts (plan game-export C12), read by the preview worker's `fbx_facts` from
//! the file's node tree in milliseconds: the kept meshes' material slots and their texture
//! references, the mesh names, the bones and the takes that animate something. The Check
//! classifies FBX files by them (C3), and the answers are cached per file size and mtime for
//! the session.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::export::collision::PATTERNS;
use crate::preview::WorkerRequest;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FbxFacts {
    /// The materials of kept meshes, in the file's material order. Two materials of one name
    /// are both listed.
    pub slots: Vec<Slot>,
    pub kept_meshes: Vec<String>,
    /// Collision meshes, which `collision::PATTERNS` match
    pub dropped_meshes: Vec<String>,
    pub has_kept_mesh: bool,
    /// Whether a kept mesh has no material, so the export adds one (C9 step 2)
    pub kept_mesh_without_material: bool,
    pub bones: Vec<String>,
    /// The takes that animate something, by name
    pub takes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Slot {
    pub name: String,
    /// By type: `baseColor`, `normal`, `metalness`, `roughness`, `occlusion`, `emissive`,
    /// `height`
    #[serde(default)]
    pub textures: BTreeMap<String, Vec<TextureRef>>,
}

/// A texture file as the FBX names it, never opened by the worker.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TextureRef {
    pub file: Option<String>,
    pub relative: Option<String>,
}

/// What an FBX exports as (C3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// It has a kept mesh
    Model,
    /// It has no kept mesh and at least one take
    Animation,
}

impl FbxFacts {
    /// A model or an animation, or `None` for an FBX with no kept mesh and no take, which has
    /// nothing to export.
    pub fn reading(&self) -> Option<Reading> {
        if self.has_kept_mesh {
            Some(Reading::Model)
        } else if !self.takes.is_empty() {
            Some(Reading::Animation)
        } else {
            None
        }
    }

    /// Each slot name held by more than one material, with how many, in slot order. Blender
    /// would rename all but one, so such an FBX is refused (C9 step 1).
    pub fn duplicate_slots(&self) -> Vec<(&str, usize)> {
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for slot in &self.slots {
            match counts.iter_mut().find(|(name, _)| *name == slot.name) {
                Some((_, n)) => *n += 1,
                None => counts.push((&slot.name, 1)),
            }
        }
        counts.retain(|(_, n)| *n > 1);
        counts
    }
}

/// The worker's `fbx_facts` request for the FBX at `source`, which it reads only inside
/// `library_root`.
pub fn request(id: &str, source: &Path, library_root: &Path) -> Value {
    json!({
        "id": id,
        "method": "fbx_facts",
        "params": {
            "source": source.to_string_lossy(),
            "library_root": library_root.to_string_lossy(),
            "patterns": PATTERNS,
        },
    })
}

/// The facts in the worker's reply, or why there are none.
pub fn parse_reply(reply: &Value) -> Result<FbxFacts, String> {
    if let Some(error) = reply.get("error") {
        return Err(error
            .as_str()
            .map_or_else(|| error.to_string(), str::to_string));
    }
    let result = reply
        .get("result")
        .ok_or("the preview engine's reply has no result")?;
    if result.get("ok").and_then(Value::as_bool) != Some(true) {
        let error = result.get("error").and_then(Value::as_str);
        return Err(error
            .unwrap_or("the preview engine gave no reason")
            .to_string());
    }
    serde_json::from_value(result.clone()).map_err(|e| format!("the facts are malformed: {e}"))
}

/// Reads an FBX's facts.
pub trait Reader: Sync {
    /// The facts of the FBX at `source`, an absolute path in the library at `library_root`.
    fn read(
        &self,
        source: &Path,
        library_root: &Path,
    ) -> impl Future<Output = Result<FbxFacts, String>> + Send;
}

/// The preview queue's worker, reached through its requests (`preview::WorkerRequest`).
pub struct Worker {
    requests: mpsc::Sender<WorkerRequest>,
}

impl Worker {
    pub fn new(requests: mpsc::Sender<WorkerRequest>) -> Self {
        Worker { requests }
    }
}

impl Reader for Worker {
    async fn read(&self, source: &Path, library_root: &Path) -> Result<FbxFacts, String> {
        let id = format!("fbx_facts-{}", uuid::Uuid::new_v4());
        let (reply, answer) = oneshot::channel();
        let request = request(&id, source, library_root);
        self.requests
            .send(WorkerRequest { request, reply })
            .await
            .map_err(|_| "the preview engine isn't running".to_string())?;
        let reply = answer
            .await
            .map_err(|_| "the preview engine stopped before it answered".to_string())??;
        parse_reply(&reply)
    }
}

/// When a file was read: its size and mtime.
type Stamp = (u64, Option<SystemTime>);

/// Another reader's answers, kept per file while its size and mtime stay the same, for the
/// session. A failed read isn't kept, so it is tried again.
pub struct Cached<R> {
    inner: R,
    entries: Mutex<HashMap<PathBuf, (Stamp, FbxFacts)>>,
}

impl<R: Reader> Cached<R> {
    pub fn new(inner: R) -> Self {
        Cached {
            inner,
            entries: Mutex::new(HashMap::new()),
        }
    }
}

fn stamp(source: &Path) -> Result<Stamp, String> {
    let meta = std::fs::metadata(source).map_err(|e| format!("it can't be read: {e}"))?;
    Ok((meta.len(), meta.modified().ok()))
}

impl<R: Reader + Send> Reader for Cached<R> {
    async fn read(&self, source: &Path, library_root: &Path) -> Result<FbxFacts, String> {
        // Stamped before the read, so a file changed during it is read again next time
        let stamp = stamp(source)?;
        let cached = self
            .entries
            .lock()
            .expect("facts cache")
            .get(source)
            .cloned();
        if let Some((at, facts)) = cached {
            if at == stamp {
                return Ok(facts);
            }
        }
        let facts = self.inner.read(source, library_root).await?;
        self.entries
            .lock()
            .expect("facts cache")
            .insert(source.to_path_buf(), (stamp, facts.clone()));
        Ok(facts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn slot(name: &str) -> Slot {
        Slot {
            name: name.into(),
            ..Slot::default()
        }
    }

    #[test]
    fn a_kept_mesh_makes_a_model_and_takes_alone_an_animation() {
        let model = FbxFacts {
            has_kept_mesh: true,
            takes: vec!["Wave".into()],
            ..FbxFacts::default()
        };
        assert_eq!(model.reading(), Some(Reading::Model));
        let animation = FbxFacts {
            dropped_meshes: vec!["UCX_Hull".into()],
            takes: vec!["Wave".into()],
            ..FbxFacts::default()
        };
        assert_eq!(animation.reading(), Some(Reading::Animation));
        let empty = FbxFacts {
            dropped_meshes: vec!["UCX_Hull".into()],
            ..FbxFacts::default()
        };
        assert_eq!(empty.reading(), None);
    }

    #[test]
    fn duplicate_slots_are_named_once_with_their_count() {
        let facts = FbxFacts {
            slots: ["Wood", "Iron", "Wood", "Cloth", "Iron", "Wood"]
                .map(slot)
                .to_vec(),
            ..FbxFacts::default()
        };
        assert_eq!(facts.duplicate_slots(), [("Wood", 3), ("Iron", 2)]);
        let distinct = FbxFacts {
            slots: vec![slot("Material"), slot("DefaultMaterial")],
            ..FbxFacts::default()
        };
        assert_eq!(distinct.duplicate_slots(), []);
    }

    #[test]
    fn the_request_carries_the_collision_patterns_and_the_library_root() {
        let req = request("r1", Path::new("/lib/Kit/a.fbx"), Path::new("/lib"));
        assert_eq!(
            req,
            json!({
                "id": "r1",
                "method": "fbx_facts",
                "params": {
                    "source": "/lib/Kit/a.fbx",
                    "library_root": "/lib",
                    "patterns": serde_json::to_value(&PATTERNS).unwrap(),
                },
            })
        );
    }

    #[test]
    fn replies_parse_into_facts_or_the_reason_there_are_none() {
        let reply = json!({"id": "r1", "result": {
            "ok": true,
            "slots": [{"name": "Stone", "textures": {"baseColor": [{"file": "/a/T_D.png", "relative": "T_D.png"}]}}],
            "kept_meshes": ["SM_Pillar_LOD0"], "dropped_meshes": ["UCX_SM_Pillar_00"],
            "has_kept_mesh": true, "kept_mesh_without_material": false,
            "bones": [], "takes": [],
        }});
        let facts = parse_reply(&reply).unwrap();
        assert_eq!(facts.slots[0].name, "Stone");
        assert_eq!(
            facts.slots[0].textures["baseColor"],
            [TextureRef {
                file: Some("/a/T_D.png".into()),
                relative: Some("T_D.png".into())
            }]
        );
        assert_eq!(facts.dropped_meshes, ["UCX_SM_Pillar_00"]);

        let refused = json!({"id": "r1", "result": {"ok": false, "error": "FBX version 6100 is older than FBX 7"}});
        assert_eq!(
            parse_reply(&refused),
            Err("FBX version 6100 is older than FBX 7".into())
        );
        // An older installed engine
        let unknown = json!({"id": "r1", "error": "Unknown method: fbx_facts"});
        assert_eq!(
            parse_reply(&unknown),
            Err("Unknown method: fbx_facts".into())
        );
    }

    #[tokio::test]
    async fn the_worker_reader_asks_the_queue_and_reads_its_reply() {
        let (sender, mut requests) = mpsc::channel(1);
        let worker = Worker::new(sender);
        let queue = tokio::spawn(async move {
            let WorkerRequest { request, reply } = requests.recv().await.unwrap();
            let answer = json!({"id": request["id"], "result": {
                "ok": true, "slots": [], "kept_meshes": [], "dropped_meshes": [],
                "has_kept_mesh": false, "kept_mesh_without_material": false,
                "bones": [], "takes": ["Idle"],
            }});
            reply.send(Ok(answer)).unwrap();
            request
        });
        let facts = worker
            .read(Path::new("/lib/a.fbx"), Path::new("/lib"))
            .await
            .unwrap();
        assert_eq!(facts.reading(), Some(Reading::Animation));
        let request = queue.await.unwrap();
        assert_eq!(request["method"], "fbx_facts");
        assert_eq!(request["params"]["source"], "/lib/a.fbx");
    }

    /// Counts its reads, and fails while `fail` is set.
    struct Counting {
        reads: AtomicUsize,
        fail: std::sync::atomic::AtomicBool,
    }

    impl Reader for Counting {
        async fn read(&self, _: &Path, _: &Path) -> Result<FbxFacts, String> {
            let n = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail.load(Ordering::SeqCst) {
                return Err("no engine".into());
            }
            Ok(FbxFacts {
                takes: vec![format!("read {n}")],
                ..FbxFacts::default()
            })
        }
    }

    #[tokio::test]
    async fn answers_are_cached_per_file_size_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let fbx = dir.path().join("a.fbx");
        std::fs::write(&fbx, b"one").unwrap();
        let cache = Cached::new(Counting {
            reads: AtomicUsize::new(0),
            fail: true.into(),
        });
        let takes = |facts: Result<FbxFacts, String>| facts.map(|f| f.takes);

        // A failure is tried again
        assert_eq!(
            takes(cache.read(&fbx, dir.path()).await),
            Err("no engine".into())
        );
        cache.inner.fail.store(false, Ordering::SeqCst);
        assert_eq!(
            takes(cache.read(&fbx, dir.path()).await),
            Ok(vec!["read 2".into()])
        );
        assert_eq!(
            takes(cache.read(&fbx, dir.path()).await),
            Ok(vec!["read 2".into()])
        );

        // A new size reads it again
        std::fs::write(&fbx, b"three").unwrap();
        assert_eq!(
            takes(cache.read(&fbx, dir.path()).await),
            Ok(vec!["read 3".into()])
        );
        // A new mtime at the same size, too
        let file = std::fs::File::options().write(true).open(&fbx).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(
            takes(cache.read(&fbx, dir.path()).await),
            Ok(vec!["read 4".into()])
        );
        assert_eq!(cache.inner.reads.load(Ordering::SeqCst), 4);

        // A missing file is an error, and the inner reader isn't asked
        let missing = dir.path().join("gone.fbx");
        assert!(cache
            .read(&missing, dir.path())
            .await
            .unwrap_err()
            .starts_with("it can't be read: "));
        assert_eq!(cache.inner.reads.load(Ordering::SeqCst), 4);
    }
}
