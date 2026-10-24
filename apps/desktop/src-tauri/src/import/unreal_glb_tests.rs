//! The `export_unreal` step in builds (plan pack-import, Unreal conversion, A9): a stub Unreal
//! Editor, a shell script that exports the fixture project's `SM_*` assets `GLTF_BATCH` at a
//! time as the embedded script would, stands in for the real one, which CI doesn't have (A10 is
//! checked by hand).
//!
//! The tests sit in a `mod tests` block, as in every other file, so source scans that stop there
//! (`structure::tests::metadata_writers`) read none of them.

mod tests {
    use std::path::{Path, PathBuf};

    use serde_json::{json, Value};

    use crate::import::engine::tests::{setup, setup_with, Setup};
    use crate::import::engine::*;
    use crate::import::unitypackage::tests::{fixture, on_disk};
    use crate::import::unitypackage::SkipReason;
    use crate::import::unreal_glb::tests::{write_editor, write_version};

    const TARGET: &str = "Fab/unreal";

    /// A stub editor below `root`: each session logs itself and its `GLTF_BATCH` to
    /// `sessions.log`, keeps a copy of the `.uproject` it opened as `uproject.<n>`, then writes
    /// a GLB for at most `GLTF_BATCH` of the project's `SM_*.uasset` files that have none in
    /// `GLTF_OUT` yet, and the number left to `.gltf-remaining`. The session numbered in a
    /// `stall` file, and every one after it, writes no GLB. An asset named in a `fail` file (by
    /// its path below `Content/`) gets no GLB either: as the script does, the session counts it
    /// in its batch, lists it in `.gltf-failed`, and later sessions skip it.
    fn stub_editor(root: &Path) {
        let r = root.display();
        write_editor(
            root,
            &format!(
                r#"log='{r}/sessions.log'
n=$(( $(cat "$log" 2>/dev/null | wc -l) + 1 ))
echo "session $n batch=$GLTF_BATCH" >> "$log"
cp "$1" '{r}/uproject.'$n
case "$2" in -ExecutePythonScript=*) [ -f "${{2#-ExecutePythonScript=}}" ] || exit 3;; *) exit 4;; esac
stall=$(cat '{r}/stall' 2>/dev/null || echo 999)
content="$(dirname "$1")/Content"
made=0; left=0
for asset in $(cd "$content" && find . -name 'SM_*.uasset' | sort); do
  rel=${{asset#./}}; rel=${{rel%.uasset}}
  glb="$GLTF_OUT/$rel.glb"
  [ -e "$glb" ] && continue
  grep -qx "/Game/$rel" "$GLTF_OUT/.gltf-failed" 2>/dev/null && continue
  if [ "$n" -lt "$stall" ] && [ "$made" -lt "$GLTF_BATCH" ]; then
    made=$((made + 1))
    if grep -qx "$rel" '{r}/fail' 2>/dev/null; then
      echo "/Game/$rel" >> "$GLTF_OUT/.gltf-failed"
      echo "LogPython: Warning: [gltf] no file written for /Game/$rel"
    else
      mkdir -p "$(dirname "$glb")"
      printf 'glTF %s' "$rel" > "$glb"
      echo "LogPython: [gltf] exported /Game/$rel"
    fi
  else
    left=$((left + 1))
  fi
done
echo "LogPython: [gltf] attempted $made, remaining $left"
echo "$left" > "$GLTF_OUT/.gltf-remaining""#
            ),
        );
        write_version(root);
    }

    impl Setup {
        /// Points Settings at a stub editor (`stub_editor`) and returns its folder, with a user
        /// manager that accepts every scope and runs its command, logging it to `scopes.log`.
        async fn stub_unreal(&self) -> PathBuf {
            let bin = self.cache.path().join("bin");
            let root = bin.join("UnrealEngine");
            stub_editor(&root);
            crate::import::settings::update(
                &self.engine.db.master_pool,
                json!({ "unrealEditorRoot": root }),
            )
            .await
            .unwrap();
            let scopes = bin.join("scopes.log");
            write_script(&bin.join("systemctl"), "exit 0");
            write_script(
                &bin.join("systemd-run"),
                &format!(
                    "echo \"$*\" >> '{}'\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"",
                    scopes.display()
                ),
            );
            self.engine.set_systemd(crate::capped::Systemd {
                run: bin.join("systemd-run"),
                ctl: bin.join("systemctl"),
            });
            root
        }

        fn unreal_request(&self, name: &str) -> ImportRequest {
            ImportRequest {
                unreal: true,
                publisher: "Fab".into(),
                ..self.zip_request(name, None, None, TARGET)
            }
        }

        fn pack_dir(&self) -> PathBuf {
            self.dir.path().join(TARGET)
        }

        /// No work folder is left in the cache.
        fn no_work_folder(&self) {
            let left: Vec<_> = std::fs::read_dir(self.cache.path().join("work"))
                .map(|d| d.map(|e| e.unwrap().file_name()).collect())
                .unwrap_or_default();
            assert!(left.is_empty(), "{left:?}");
        }

        /// No staging, previous or failed folder is left beside the pack.
        fn nothing_beside_the_pack(&self) {
            for tag in ["import", "prev", "failed"] {
                let left = crate::fs_swap::leftovers(&self.pack_dir(), tag).unwrap();
                assert!(left.is_empty(), "{left:?}");
            }
        }

        fn cached_glbs(&self) -> usize {
            std::fs::read_dir(self.cache.path().join("glb"))
                .map(|d| d.count())
                .unwrap_or(0)
        }
    }

    fn write_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn sessions(root: &Path) -> Vec<String> {
        std::fs::read_to_string(root.join("sessions.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The zip's bytes and mtime, to show it untouched.
    fn stamp(path: &Path) -> (String, std::time::SystemTime) {
        (
            crate::export::hashes::sha256_file(path).unwrap(),
            std::fs::metadata(path).unwrap().modified().unwrap(),
        )
    }

    /// A9: the step runs the editor in batches, resumes from the cache after a session that
    /// made no progress failed it, and a rebuild after a finished export runs no editor. The
    /// work copy's `.uproject` has the plugins on, and the zip is untouched.
    #[tokio::test]
    async fn the_export_loops_in_batches_resumes_from_the_cache_and_stops_without_progress() {
        let s = setup_with(false).await;
        let root = s.stub_unreal().await;
        let zip = fixture("unreal.zip");
        let before = stamp(&zip);
        let run = s
            .engine
            .start_import(s.unreal_request("unreal.zip"))
            .await
            .unwrap();
        let pack_id = run.packs[0].pack_id.clone();
        assert_eq!(
            s.strings("SELECT kind || ' ' || options FROM import_steps ORDER BY position")
                .await,
            [
                r#"export_unreal {"batch":30}"#,
                "flatten {}",
                r#"hoist {"folder":"Meshes"}"#,
                r#"metadata {"preset":"fab","publisher":"Fab"}"#,
            ]
        );
        assert_eq!(
            s.strings("SELECT source FROM import_packs").await,
            ["Feif/Feif.uproject"]
        );
        // One asset per session, and the third session exports nothing.
        sqlx::query(
            "UPDATE import_steps SET options = '{\"batch\":1}' WHERE kind = 'export_unreal'",
        )
        .execute(&s.lib.pool)
        .await
        .unwrap();
        std::fs::write(root.join("stall"), "3").unwrap();
        tokio::spawn(s.engine.clone().start());

        let failed = s.wait_for(&run.id, "failed").await;
        let error = failed.error.unwrap();
        assert!(
            error.starts_with(
                "Exporting from Unreal failed: an editor session made no progress, with 1 assets \
                 left (exit status: 0): "
            ),
            "{error}"
        );
        assert_eq!(
            sessions(&root),
            [
                "session 1 batch=1",
                "session 2 batch=1",
                "session 3 batch=1"
            ]
        );
        // Each session's GLB was cached as it ended; nothing reached the library.
        assert_eq!(s.cached_glbs(), 2);
        assert!(!s.pack_dir().exists());
        s.no_work_folder();
        // The editor ran under the Unreal cap.
        let scopes = std::fs::read_to_string(root.parent().unwrap().join("scopes.log")).unwrap();
        assert!(
            scopes.contains("MemoryMax=20G") && scopes.contains("UnrealEditor"),
            "{scopes}"
        );

        // The work copy's .uproject has both plugins on, keeping what it listed.
        let uproject: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("uproject.1")).unwrap())
                .unwrap();
        let plugins: Vec<(&str, bool)> = uproject["Plugins"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p["Name"].as_str().unwrap(), p["Enabled"].as_bool().unwrap()))
            .collect();
        assert_eq!(
            plugins,
            [
                ("GLTFExporter", true),
                ("ModelingToolsEditorMode", true),
                ("PythonScriptPlugin", true),
            ]
        );
        assert_eq!(uproject["EngineAssociation"], "5.4");

        // A rebuild resumes: its one session finds the cached GLBs and exports only SM_C.
        std::fs::remove_file(root.join("stall")).unwrap();
        let run = s.engine.rebuild_pack("lib", &pack_id).await.unwrap();
        s.wait_for(&run.id, "done").await;
        assert_eq!(sessions(&root).len(), 4);
        let built = [
            "README.md",
            "Readme.txt",
            "SM_A.glb",
            "SM_B.glb",
            "SM_C.glb",
            "datapackage.json",
        ];
        assert_eq!(on_disk(&s.pack_dir()), built);
        assert_eq!(
            std::fs::read_to_string(s.pack_dir().join("SM_A.glb")).unwrap(),
            "glTF Fixture/Meshes/SM_A"
        );
        let sources: Vec<(String, String)> = sqlx::query_as(
            "SELECT o.path, o.origin || ' ' || o.source_path FROM import_outputs o
             JOIN import_packs p ON p.id = o.pack_id AND o.build_id = p.current_build
             ORDER BY o.path",
        )
        .fetch_all(&s.lib.pool)
        .await
        .unwrap();
        let sources: Vec<(&str, &str)> = sources
            .iter()
            .map(|(p, o)| (p.strip_prefix("Fab/unreal/").unwrap(), o.as_str()))
            .collect();
        assert_eq!(
            sources,
            [
                ("README.md", r#"extracted ["Feif/README.md"]"#),
                (
                    "Readme.txt",
                    r#"extracted ["Feif/Content/Fixture/Readme.txt"]"#
                ),
                ("SM_A.glb", r#"extracted ["/Game/Fixture/Meshes/SM_A"]"#),
                ("SM_B.glb", r#"extracted ["/Game/Fixture/Meshes/SM_B"]"#),
                ("SM_C.glb", r#"extracted ["/Game/Fixture/Meshes/SM_C"]"#),
                ("datapackage.json", "generated []"),
            ]
        );
        let package: Value = serde_json::from_str(
            &std::fs::read_to_string(s.pack_dir().join("datapackage.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(package["title"], "unreal", "{package}");
        assert_eq!(s.cached_glbs(), 3);
        s.no_work_folder();

        // Once an export finished, a rebuild unzips nothing and runs no editor.
        let run = s.engine.rebuild_pack("lib", &pack_id).await.unwrap();
        s.wait_for(&run.id, "done").await;
        assert_eq!(sessions(&root).len(), 4);
        assert_eq!(on_disk(&s.pack_dir()), built);

        // A GLB gone from the cache is exported again, alone, in one session.
        let glb_dir = s.cache.path().join("glb");
        let evicted = std::fs::read_dir(&glb_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        std::fs::remove_file(evicted.path()).unwrap();
        assert_eq!(s.cached_glbs(), 2);
        let run = s.engine.rebuild_pack("lib", &pack_id).await.unwrap();
        s.wait_for(&run.id, "done").await;
        assert_eq!(sessions(&root).len(), 5);
        assert_eq!(on_disk(&s.pack_dir()), built);
        assert_eq!(s.cached_glbs(), 3);
        s.no_work_folder();
        s.nothing_beside_the_pack();
        assert_eq!(stamp(&zip), before, "the zip is untouched");
    }

    /// An asset the exporter fails on gets no GLB and becomes a warning naming it, and the step
    /// goes on: a session whose only attempt failed still made progress.
    #[tokio::test]
    async fn an_asset_the_exporter_fails_on_is_a_warning_and_the_export_goes_on() {
        let s = setup_with(false).await;
        let root = s.stub_unreal().await;
        std::fs::write(root.join("fail"), "Fixture/Meshes/SM_B\n").unwrap();
        let run = s
            .engine
            .start_import(s.unreal_request("unreal.zip"))
            .await
            .unwrap();
        sqlx::query(
            "UPDATE import_steps SET options = '{\"batch\":1}' WHERE kind = 'export_unreal'",
        )
        .execute(&s.lib.pool)
        .await
        .unwrap();
        tokio::spawn(s.engine.clone().start());

        let done = s.wait_for(&run.id, "done").await;
        // SM_A, then SM_B failing alone, then SM_C.
        assert_eq!(sessions(&root).len(), 3);
        assert_eq!(
            on_disk(&s.pack_dir()),
            [
                "README.md",
                "Readme.txt",
                "SM_A.glb",
                "SM_C.glb",
                "datapackage.json"
            ]
        );
        let warned: Vec<(Vec<String>, SkipReason)> = done
            .warnings
            .iter()
            .map(|w| (w.source_path.clone(), w.reason))
            .collect();
        assert_eq!(
            warned,
            [(
                vec!["/Game/Fixture/Meshes/SM_B".to_string()],
                SkipReason::NotExported
            )]
        );
        assert_eq!(s.cached_glbs(), 2);
        s.no_work_folder();
    }

    /// A project with a `Source/` folder is refused with the reason: by the scan, which offers
    /// it unticked, and by an import, which records nothing.
    #[tokio::test]
    async fn a_project_with_source_is_refused_with_the_reason() {
        let s = setup().await;
        s.stub_unreal().await;
        let why = "The Unreal project Feif.uproject has a Source/ folder: the prebuilt editor \
                   can't compile its C++ code, so only a content-only project can be imported";
        let original = fixture("unreal-source.zip");
        let scan = s
            .engine
            .scan("lib", &original.to_string_lossy())
            .await
            .unwrap();
        let unreal = scan.offers.iter().find(|o| o.layout == "unreal").unwrap();
        let pack = &unreal.candidates[0];
        assert_eq!(pack.project.as_deref(), Some("Feif/Feif.uproject"));
        assert!(!pack.ticked);
        let tree = pack.tree.as_ref().unwrap();
        assert_eq!(tree.error.as_deref(), Some(why));
        assert!(tree.estimate);
        assert_eq!(scan.unavailable.get("export_unreal"), None);

        let refused = s
            .engine
            .start_import(s.unreal_request("unreal-source.zip"))
            .await;
        assert_eq!(refused.unwrap_err(), why);
        assert_eq!(s.count("SELECT COUNT(*) FROM imports").await, 0);
        assert!(!s.pack_dir().exists());

        // A zip without a project isn't an Unreal import.
        let refused = s.engine.start_import(s.unreal_request("kenney.zip")).await;
        assert_eq!(
            refused.unwrap_err(),
            "The zip holds no Unreal project (.uproject)"
        );
    }

    /// Without the editor, the scan and the pack's steps say why, and the build fails with it,
    /// changing nothing.
    #[tokio::test]
    async fn without_the_editor_the_export_fails_with_the_reason() {
        let s = setup().await;
        let why = "Exporting from Unreal needs the Unreal Editor: no editor at \
                   /nonexistent/opendam-test/UnrealEngine/Engine/Binaries/Linux/UnrealEditor \
                   (set its folder in Settings)";
        let original = fixture("unreal.zip");
        let scan = s
            .engine
            .scan("lib", &original.to_string_lossy())
            .await
            .unwrap();
        assert_eq!(
            scan.unavailable.get("export_unreal").map(String::as_str),
            Some(why)
        );
        let run = s
            .engine
            .start_import(s.unreal_request("unreal.zip"))
            .await
            .unwrap();
        let failed = s.wait_for(&run.id, "failed").await;
        assert_eq!(failed.error.as_deref(), Some(why));
        let steps = s
            .engine
            .pack_steps("lib", &run.packs[0].pack_id)
            .await
            .unwrap();
        assert_eq!(
            steps.unavailable.get("export_unreal").map(String::as_str),
            Some(why)
        );
        assert!(!s.pack_dir().exists());
        s.nothing_beside_the_pack();
        s.no_work_folder();
    }

    /// An editor killed with status 137 under the cap fails the step, naming the cap.
    #[tokio::test]
    async fn the_editor_killed_by_the_memory_cap_fails_the_step() {
        let s = setup().await;
        let root = s.stub_unreal().await;
        write_editor(&root, "exit 137");
        crate::import::settings::update(&s.engine.db.master_pool, json!({"unrealMemoryMax": "6G"}))
            .await
            .unwrap();
        let run = s
            .engine
            .start_import(s.unreal_request("unreal.zip"))
            .await
            .unwrap();
        let failed = s.wait_for(&run.id, "failed").await;
        assert_eq!(
            failed.error.as_deref(),
            Some(
                "Exporting from Unreal failed: the Unreal Editor was killed by the memory cap (6G)"
            )
        );
        assert!(!s.pack_dir().exists());
        s.no_work_folder();
    }
}
