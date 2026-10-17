//! The run engine: one permit pool per provider hands queued items out round-robin across that
//! provider's running runs, each item is written through the write protocol, and progress goes
//! to the UI as throttled `autotag:updated` events.

use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

use super::provider::{AssetInfo, Mode, TagProvider, TagRequest, Thumbnail};
use super::settings::{self, AiSettings};
use super::store::{self, Item, NewRun, RunSummary};
use super::vocabulary::Vocabulary;
use crate::db::{resolve_path, Database};

/// The most `autotag:updated` events per second, about.
const EVENT_INTERVAL: Duration = Duration::from_millis(250);
/// Thumbnails are downscaled to this on their long edge, about 350 image tokens.
const THUMBNAIL_EDGE: u32 = 512;

/// The payload of `autotag:updated`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutotagUpdate {
    pub runs: Vec<RunSummary>,
    /// Assets whose tags changed since the last event.
    pub changed_asset_ids: Vec<String>,
}

pub type EventSink = Arc<dyn Fn(AutotagUpdate) + Send + Sync>;

/// A provider as the run dialog shows it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderOption {
    pub id: &'static str,
    pub label: &'static str,
    pub modes: Vec<Mode>,
    /// Why no run can start with it, when none can.
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartedRun {
    pub run: RunSummary,
    pub queued: usize,
    /// Assets left out because no open library holds them.
    pub skipped: usize,
}

/// One provider's share of the engine: its requests in flight never exceed its concurrency
/// setting, whatever the number of runs.
struct ProviderPool {
    provider: Arc<dyn TagProvider>,
    in_flight: AtomicU32,
    /// Woken when a run starts and when a request ends.
    wake: Notify,
}

pub struct Engine {
    db: Arc<Database>,
    pools: Vec<ProviderPool>,
    changed_assets: Mutex<HashSet<String>>,
    events_wake: Notify,
    sink: EventSink,
}

impl Engine {
    pub fn new(
        db: Arc<Database>,
        providers: Vec<Arc<dyn TagProvider>>,
        sink: EventSink,
    ) -> Arc<Self> {
        let pools = providers
            .into_iter()
            .map(|provider| ProviderPool {
                provider,
                in_flight: AtomicU32::new(0),
                wake: Notify::new(),
            })
            .collect();
        Arc::new(Self {
            db,
            pools,
            changed_assets: Mutex::new(HashSet::new()),
            events_wake: Notify::new(),
            sink,
        })
    }

    /// Puts items a previous session left running back in the queue, replays writes still
    /// waiting for their library, then starts one scheduler per provider and the event loop.
    pub async fn start(self: &Arc<Self>) {
        if let Err(e) = store::requeue_running_items(&self.db.master_pool).await {
            tracing::error!("Could not requeue auto-tag items: {e}");
        }
        match store::replay_pending(&self.db).await {
            Ok(changed) => self.assets_changed(changed),
            Err(e) => tracing::error!("Could not replay pending auto-tag writes: {e}"),
        }
        for index in 0..self.pools.len() {
            self.pools[index].wake.notify_one();
            tokio::spawn(self.clone().schedule(index));
        }
        tokio::spawn(self.clone().emit_events());
    }

    fn pool(&self, provider_id: &str) -> Result<&ProviderPool, String> {
        self.pools
            .iter()
            .find(|p| p.provider.id() == provider_id)
            .ok_or_else(|| format!("Unknown tagging provider {provider_id}"))
    }

    /// Every provider, with why it can't be picked. Blocks on the keyring: call it off async
    /// workers.
    pub fn provider_options(&self) -> Vec<ProviderOption> {
        self.pools
            .iter()
            .map(|p| ProviderOption {
                id: p.provider.id(),
                label: p.provider.label(),
                modes: p.provider.modes().to_vec(),
                unavailable_reason: p.provider.unavailable_reason(),
            })
            .collect()
    }

    pub async fn list_runs(&self) -> sqlx::Result<Vec<RunSummary>> {
        let mut runs = store::list_runs(&self.db.master_pool).await?;
        for run in &mut runs {
            run.provider_label = self
                .pool(&run.provider)
                .map_or(run.provider.clone(), |p| p.provider.label().to_string());
        }
        Ok(runs)
    }

    /// Queues a run over `asset_ids`. Nothing is created, and nothing is sent, unless the
    /// provider is set up and can do `mode`.
    pub async fn start_run(
        &self,
        asset_ids: Vec<String>,
        provider_id: &str,
        mode: Mode,
    ) -> Result<StartedRun, String> {
        let pool = self.pool(provider_id)?;
        let provider = pool.provider.clone();
        let label = provider.label();
        let unavailable = tokio::task::spawn_blocking(move || provider.unavailable_reason())
            .await
            .map_err(|e| e.to_string())?;
        if let Some(reason) = unavailable {
            return Err(reason);
        }
        if !pool.provider.modes().contains(&mode) {
            return Err(format!("{label} can't do {}", mode.as_str()));
        }
        if mode != Mode::ExistingOnly {
            return Err("Only Existing only runs are available so far".into());
        }
        if asset_ids.is_empty() {
            return Err("No assets to tag".into());
        }

        let items = store::locate_assets(&self.db, &asset_ids)
            .await
            .map_err(|e| e.to_string())?;
        if items.is_empty() {
            return Err("None of these assets is in an open library".into());
        }
        let settings = settings::load(&self.db.master_pool)
            .await
            .map_err(|e| e.to_string())?;
        let id = uuid::Uuid::new_v4().to_string();
        let run_label = match items.len() {
            1 => "1 asset".to_string(),
            n => format!("{n} assets"),
        };
        store::create_run(
            &self.db.master_pool,
            NewRun {
                id: &id,
                label: &run_label,
                provider: provider_id,
                model: pool.provider.model(&settings).as_deref(),
                mode: mode.as_str(),
                options: serde_json::json!({ "maxTags": pool.provider.max_tags(&settings) }),
            },
            &items,
        )
        .await
        .map_err(|e| e.to_string())?;
        tracing::info!("Started auto-tag run {id}: {} with {label}", run_label);

        pool.wake.notify_one();
        self.events_wake.notify_one();
        let run = self
            .list_runs()
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|r| r.id == id)
            .ok_or("The new run is missing")?;
        Ok(StartedRun {
            run,
            queued: items.len(),
            skipped: asset_ids.len() - items.len(),
        })
    }

    /// Hands out permits for one provider: while a request slot is free, it claims the next
    /// queued item, taking one from each running run in turn. It sleeps on the pool's `Notify`.
    async fn schedule(self: Arc<Self>, index: usize) {
        let pool = &self.pools[index];
        let mut last_run: Option<String> = None;
        loop {
            let limit = match settings::load(&self.db.master_pool).await {
                Ok(settings) => pool.provider.concurrency(&settings),
                Err(_) => pool.provider.concurrency(&AiSettings::default()),
            }
            .max(1);
            if pool.in_flight.load(Ordering::SeqCst) >= limit {
                pool.wake.notified().await;
                continue;
            }
            match self.next_item(pool.provider.id(), &mut last_run).await {
                Ok(Some(item)) => {
                    pool.in_flight.fetch_add(1, Ordering::SeqCst);
                    let engine = self.clone();
                    tokio::spawn(async move {
                        let pool = &engine.pools[index];
                        engine.process(pool.provider.as_ref(), item).await;
                        pool.in_flight.fetch_sub(1, Ordering::SeqCst);
                        pool.wake.notify_one();
                    });
                }
                Ok(None) => pool.wake.notified().await,
                Err(e) => {
                    tracing::error!("Auto-tag scheduler for {}: {e}", pool.provider.id());
                    pool.wake.notified().await;
                }
            }
        }
    }

    /// The next item round-robin: from the first run with work after `last_run`.
    async fn next_item(
        &self,
        provider_id: &str,
        last_run: &mut Option<String>,
    ) -> sqlx::Result<Option<Item>> {
        let runs = store::runs_with_work(&self.db.master_pool, provider_id).await?;
        let start = last_run
            .as_ref()
            .and_then(|last| runs.iter().position(|r| r == last))
            .map_or(0, |i| i + 1);
        for offset in 0..runs.len() {
            let run_id = &runs[(start + offset) % runs.len()];
            if let Some(item) = store::claim_item(&self.db.master_pool, run_id).await? {
                *last_run = Some(run_id.clone());
                return Ok(Some(item));
            }
        }
        Ok(None)
    }

    async fn process(&self, provider: &dyn TagProvider, item: Item) {
        match self.tag_item(provider, &item).await {
            Ok(changed) => self.assets_changed(changed),
            Err(error) => {
                tracing::warn!("Auto-tag item {} failed: {error}", item.asset_id);
                if let Err(e) = store::fail_item(&self.db.master_pool, &item, &error).await {
                    tracing::error!("Could not record the failed item {}: {e}", item.asset_id);
                }
            }
        }
        if let Err(e) = store::finish_run_if_complete(&self.db.master_pool, &item.run_id).await {
            tracing::error!("Could not finish auto-tag run {}: {e}", item.run_id);
        }
        self.events_wake.notify_one();
    }

    /// Asks the provider about one asset and writes the result through the write protocol.
    /// Returns the assets whose tags changed. An error fails the item.
    async fn tag_item(
        &self,
        provider: &dyn TagProvider,
        item: &Item,
    ) -> Result<Vec<String>, String> {
        let lib = self
            .db
            .get_library_handle(&item.library_id)
            .await
            .ok_or("The asset's library is offline")?;
        let row: Option<(String, String, String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT file_name, file_path, kind, extension, metadata, thumbnail_path
             FROM assets WHERE id = ?",
        )
        .bind(&item.asset_id)
        .fetch_optional(&lib.pool)
        .await
        .map_err(|e| e.to_string())?;
        let (file_name, relative_path, kind, extension, metadata, thumbnail) =
            row.ok_or("The asset is gone")?;

        let image = match thumbnail {
            Some(thumb) => {
                let path = resolve_path(&lib.root_path, &thumb);
                tokio::task::spawn_blocking(move || load_thumbnail(Path::new(&path)))
                    .await
                    .map_err(|e| e.to_string())?
            }
            None => None,
        };
        let vocabulary = Arc::new(
            Vocabulary::load(&self.db.master_pool)
                .await
                .map_err(|e| e.to_string())?,
        );
        let mode: Mode = serde_json::from_value(serde_json::Value::String(item.mode.clone()))
            .map_err(|_| format!("Unknown mode {}", item.mode))?;
        let request = TagRequest {
            run_id: item.run_id.clone(),
            mode,
            asset: AssetInfo {
                file_name,
                relative_path,
                kind,
                extension,
                metadata: serde_json::from_str(&metadata).unwrap_or_default(),
            },
            image,
            vocabulary: vocabulary.clone(),
        };

        // Every class fails the item for now; retries and pausing the run come later.
        let result = provider.tag(&request).await.map_err(|e| {
            tracing::debug!("{} error ({:?}): {}", provider.id(), e.class, e.message);
            e.message
        })?;
        let max_tags = item.options["maxTags"].as_u64().unwrap_or(8) as usize;
        let accepted = vocabulary.accepted_tags(&result, max_tags);

        // Step 1: record every pair the library lacks before the library sees it.
        let present: HashSet<String> =
            sqlx::query_scalar::<_, String>("SELECT tag_id FROM asset_tags WHERE asset_id = ?")
                .bind(&item.asset_id)
                .fetch_all(&lib.pool)
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .collect();
        let new_tags: Vec<String> = accepted
            .iter()
            .filter(|id| !present.contains(*id))
            .cloned()
            .collect();
        let summary = serde_json::json!({ "tags": accepted, "proposed": result.proposed });
        store::record_intent(
            &self.db.master_pool,
            item,
            &new_tags,
            &summary,
            &result.usage,
        )
        .await
        .map_err(|e| e.to_string())?;

        // Steps 3 and 4. A failure leaves the rows pending for replay; the item stays done.
        let write = async {
            let pending = store::pending_pairs_of(&self.db.master_pool, item).await?;
            store::apply_pairs(&self.db, &lib, &pending, store::Applier::Item).await
        };
        match write.await {
            Ok(changed) => Ok(changed),
            Err(e) => {
                tracing::warn!(
                    "Tags for {} wait for replay, the library write failed: {e}",
                    item.asset_id
                );
                Ok(Vec::new())
            }
        }
    }

    fn assets_changed(&self, assets: Vec<String>) {
        if assets.is_empty() {
            return;
        }
        self.changed_assets.lock().unwrap().extend(assets);
        self.events_wake.notify_one();
    }

    /// Sends `autotag:updated` at most every `EVENT_INTERVAL`, coalescing what happened since.
    async fn emit_events(self: Arc<Self>) {
        loop {
            self.events_wake.notified().await;
            let changed: Vec<String> = self.changed_assets.lock().unwrap().drain().collect();
            match self.list_runs().await {
                Ok(runs) => (self.sink)(AutotagUpdate {
                    runs,
                    changed_asset_ids: changed,
                }),
                Err(e) => tracing::error!("Could not list auto-tag runs: {e}"),
            }
            tokio::time::sleep(EVENT_INTERVAL).await;
        }
    }
}

/// The thumbnail as a PNG at most `THUMBNAIL_EDGE` px on its long edge, or None when it can't
/// be read: the request is then text only.
fn load_thumbnail(path: &Path) -> Option<Thumbnail> {
    let image = image::open(path)
        .map_err(|e| tracing::debug!("No thumbnail for tagging from {path:?}: {e}"))
        .ok()?;
    let image = if image.width().max(image.height()) > THUMBNAIL_EDGE {
        image.thumbnail(THUMBNAIL_EDGE, THUMBNAIL_EDGE)
    } else {
        image
    };
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).ok()?;
    Some(Thumbnail {
        media_type: "image/png",
        data: png.into_inner(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autotag::provider::{ErrorClass, ProviderError, TagFuture, TagResult};
    use crate::db::{
        insert_asset, insert_tag, memory_database, memory_library, memory_library_pool,
    };
    use sqlx::{Pool, Sqlite};
    use std::sync::atomic::AtomicUsize;

    /// Counts calls and requests in flight, and answers every asset with `existing`, or with
    /// `error` when it is set.
    #[derive(Default)]
    struct Recording {
        existing: Vec<String>,
        error: Option<ProviderError>,
        delay: Duration,
        unavailable: Option<String>,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        calls: Mutex<Vec<String>>,
    }

    impl TagProvider for Recording {
        fn id(&self) -> &'static str {
            "recording"
        }
        fn label(&self) -> &'static str {
            "Recording"
        }
        fn modes(&self) -> &'static [Mode] {
            &Mode::ALL
        }
        fn concurrency(&self, settings: &AiSettings) -> u32 {
            settings.anthropic.concurrency
        }
        fn max_tags(&self, settings: &AiSettings) -> u32 {
            settings.anthropic.max_tags
        }
        fn unavailable_reason(&self) -> Option<String> {
            self.unavailable.clone()
        }
        fn tag<'a>(&'a self, request: &'a TagRequest) -> TagFuture<'a> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(request.run_id.clone());
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_in_flight.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(self.delay).await;
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                if let Some(error) = &self.error {
                    return Err(error.clone());
                }
                Ok(TagResult {
                    existing: self.existing.clone(),
                    ..Default::default()
                })
            })
        }
    }

    struct Setup {
        engine: Arc<Engine>,
        provider: Arc<Recording>,
        lib_pool: Pool<Sqlite>,
        updates: Arc<Mutex<Vec<AutotagUpdate>>>,
    }

    async fn setup(provider: Recording, assets: &[&str]) -> Setup {
        let lib_pool = memory_library_pool().await;
        for id in assets {
            insert_asset(&lib_pool, id).await;
        }
        let db = Arc::new(memory_database(vec![memory_library("lib", lib_pool.clone())]).await);
        let provider = Arc::new(provider);
        let updates = Arc::new(Mutex::new(Vec::new()));
        let recorded = updates.clone();
        let engine = Engine::new(
            db,
            vec![provider.clone()],
            Arc::new(move |update| recorded.lock().unwrap().push(update)),
        );
        Setup {
            engine,
            provider,
            lib_pool,
            updates,
        }
    }

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    async fn wait_until_done(engine: &Engine) -> Vec<RunSummary> {
        for _ in 0..500 {
            let runs = engine.list_runs().await.unwrap();
            if runs.iter().all(|r| r.status == "done") {
                return runs;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("runs never finished: {:?}", engine.list_runs().await);
    }

    #[tokio::test]
    async fn additive_apply_records_only_newly_inserted_pairs() {
        let s = setup(
            Recording {
                existing: ids(&["t-rock", "t-tree", "t-ghost"]),
                ..Default::default()
            },
            &["a1"],
        )
        .await;
        let master = &s.engine.db.master_pool;
        insert_tag(master, "t-rock", "Rock").await;
        insert_tag(master, "t-tree", "Tree").await;
        insert_tag(master, "t-moss", "Moss").await;
        insert_tag(&s.lib_pool, "t-moss", "Moss").await;
        insert_tag(&s.lib_pool, "t-rock", "Rock").await;
        for tag in ["t-rock", "t-moss"] {
            sqlx::query("INSERT INTO asset_tags (asset_id, tag_id) VALUES ('a1', ?)")
                .bind(tag)
                .execute(&s.lib_pool)
                .await
                .unwrap();
        }

        s.engine.start().await;
        s.engine
            .start_run(ids(&["a1"]), "recording", Mode::ExistingOnly)
            .await
            .unwrap();
        let runs = wait_until_done(&s.engine).await;

        assert_eq!((runs[0].done, runs[0].failed), (1, 0));
        let tags: Vec<String> = sqlx::query_scalar(
            "SELECT tag_id FROM asset_tags WHERE asset_id = 'a1' ORDER BY tag_id",
        )
        .fetch_all(&s.lib_pool)
        .await
        .unwrap();
        assert_eq!(tags, ["t-moss", "t-rock", "t-tree"], "added, never removed");
        let recorded: Vec<(String, String)> =
            sqlx::query_as("SELECT tag_id, state FROM tag_run_applied")
                .fetch_all(master)
                .await
                .unwrap();
        assert_eq!(recorded, [("t-tree".into(), "applied".into())]);

        tokio::time::sleep(EVENT_INTERVAL * 2).await;
        let updates = s.updates.lock().unwrap();
        assert!(updates.iter().any(|u| u.changed_asset_ids == ["a1"]));
        assert_eq!(updates.last().unwrap().runs[0].status, "done");
    }

    #[tokio::test]
    async fn two_runs_at_concurrency_two_never_exceed_two_in_flight() {
        let assets = ["a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7"];
        let s = setup(
            Recording {
                delay: Duration::from_millis(30),
                ..Default::default()
            },
            &assets,
        )
        .await;
        settings::update(
            &s.engine.db.master_pool,
            serde_json::json!({"anthropic": {"concurrency": 2}}),
        )
        .await
        .unwrap();
        let first = s
            .engine
            .start_run(ids(&assets[..4]), "recording", Mode::ExistingOnly)
            .await
            .unwrap();
        let second = s
            .engine
            .start_run(ids(&assets[4..]), "recording", Mode::ExistingOnly)
            .await
            .unwrap();

        s.engine.start().await;
        let runs = wait_until_done(&s.engine).await;

        assert!(runs.iter().all(|r| r.done == 4));
        assert_eq!(s.provider.max_in_flight.load(Ordering::SeqCst), 2);
        let calls = s.provider.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 8);
        let runs_called = |calls: &[String], id: &str| calls.iter().filter(|c| *c == id).count();
        assert_eq!(runs_called(&calls[..4], &first.run.id), 2, "{calls:?}");
        assert_eq!(runs_called(&calls[..4], &second.run.id), 2, "{calls:?}");
        assert!(
            calls.windows(2).take(3).all(|w| w[0] != w[1]),
            "round-robin across the two runs: {calls:?}"
        );
    }

    #[tokio::test]
    async fn a_start_with_nothing_configured_makes_zero_provider_calls() {
        let s = setup(
            Recording {
                unavailable: Some("Set up Recording in Settings".into()),
                ..Default::default()
            },
            &["a1"],
        )
        .await;
        s.engine.start().await;

        let error = s
            .engine
            .start_run(ids(&["a1"]), "recording", Mode::ExistingOnly)
            .await
            .unwrap_err();

        assert_eq!(error, "Set up Recording in Settings");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(s.provider.calls.lock().unwrap().is_empty());
        assert!(s.engine.list_runs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn modes_not_built_yet_are_refused() {
        let s = setup(Recording::default(), &["a1"]).await;

        let error = s
            .engine
            .start_run(ids(&["a1"]), "recording", Mode::PreferExisting)
            .await
            .unwrap_err();

        assert!(error.contains("Existing only"), "{error}");
        assert!(s.engine.list_runs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_missing_asset_fails_its_item_and_the_run_still_finishes() {
        let s = setup(Recording::default(), &["a1", "a2"]).await;
        let started = s
            .engine
            .start_run(
                ids(&["a1", "a2", "nowhere"]),
                "recording",
                Mode::ExistingOnly,
            )
            .await
            .unwrap();
        assert_eq!((started.queued, started.skipped), (2, 1));
        sqlx::query("DELETE FROM assets WHERE id = 'a2'")
            .execute(&s.lib_pool)
            .await
            .unwrap();

        s.engine.start().await;
        let runs = wait_until_done(&s.engine).await;

        assert_eq!((runs[0].done, runs[0].failed), (1, 1));
        let error: String =
            sqlx::query_scalar("SELECT error FROM tag_run_items WHERE asset_id = 'a2'")
                .fetch_one(&s.engine.db.master_pool)
                .await
                .unwrap();
        assert_eq!(error, "The asset is gone");
    }

    #[tokio::test]
    async fn a_provider_error_of_every_class_fails_the_item() {
        for class in [ErrorClass::Retryable, ErrorClass::Fatal, ErrorClass::Item] {
            let message = format!("{class:?} trouble");
            let s = setup(
                Recording {
                    existing: ids(&["t-rock"]),
                    error: Some(ProviderError::new(class, &message)),
                    ..Default::default()
                },
                &["a1"],
            )
            .await;
            let master = &s.engine.db.master_pool;
            insert_tag(master, "t-rock", "Rock").await;

            s.engine.start().await;
            s.engine
                .start_run(ids(&["a1"]), "recording", Mode::ExistingOnly)
                .await
                .unwrap();
            let runs = wait_until_done(&s.engine).await;

            assert_eq!((runs[0].done, runs[0].failed), (0, 1), "{class:?}");
            assert_eq!(
                s.provider.calls.lock().unwrap().len(),
                1,
                "{class:?}: no retry"
            );
            let (status, error): (String, String) =
                sqlx::query_as("SELECT status, error FROM tag_run_items WHERE asset_id = 'a1'")
                    .fetch_one(master)
                    .await
                    .unwrap();
            assert_eq!((status.as_str(), error), ("failed", message), "{class:?}");
            let applied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tag_run_applied")
                .fetch_one(master)
                .await
                .unwrap();
            let tagged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM asset_tags")
                .fetch_one(&s.lib_pool)
                .await
                .unwrap();
            assert_eq!((applied, tagged), (0, 0), "{class:?}");
        }
    }

    #[tokio::test]
    async fn updates_are_throttled_to_one_per_event_interval() {
        let assets: Vec<String> = (0..40).map(|i| format!("a{i}")).collect();
        let names: Vec<&str> = assets.iter().map(String::as_str).collect();
        let s = setup(Recording::default(), &names).await;
        settings::update(
            &s.engine.db.master_pool,
            serde_json::json!({"anthropic": {"concurrency": 16}}),
        )
        .await
        .unwrap();
        s.engine.start().await;

        let started = std::time::Instant::now();
        s.engine
            .start_run(assets.clone(), "recording", Mode::ExistingOnly)
            .await
            .unwrap();
        wait_until_done(&s.engine).await;
        tokio::time::sleep(EVENT_INTERVAL * 2).await;
        let elapsed = started.elapsed();

        // 40 items finish, each waking the event loop, but sends are spaced EVENT_INTERVAL apart.
        let sent = s.updates.lock().unwrap().len() as u128;
        let allowed = elapsed.as_millis() / EVENT_INTERVAL.as_millis() + 1;
        assert!(sent >= 1 && sent <= allowed, "{sent} events in {elapsed:?}");
        assert!(sent < 40, "{sent} events for 40 items");
        assert_eq!(s.updates.lock().unwrap().last().unwrap().runs[0].done, 40);
    }

    #[test]
    fn thumbnails_are_downscaled_to_512_px_on_the_long_edge() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thumb.png");
        image::RgbImage::new(1024, 256).save(&path).unwrap();

        let thumbnail = load_thumbnail(&path).unwrap();

        let decoded = image::load_from_memory(&thumbnail.data).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (512, 128));
        assert_eq!(thumbnail.media_type, "image/png");
        assert!(load_thumbnail(&dir.path().join("missing.png")).is_none());
    }
}
