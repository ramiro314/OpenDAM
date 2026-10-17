//! The run engine: one permit pool per provider hands queued items out round-robin across that
//! provider's running runs, each item is written through the write protocol, and progress goes
//! to the UI as throttled `autotag:updated` events. Runs can be paused, resumed and cancelled,
//! and provider failures are retried, pause the run or fail the item by their class. A Prefer
//! existing run collects new tag names and ends waiting for one review.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

use super::provider::{AssetInfo, ErrorClass, Mode, TagProvider, TagRequest, TagResult, Thumbnail};
use super::review::{self, ProposalDecision, ReviewProposal};
use super::settings::{self, AiSettings};
use super::store::{self, Item, NewRun, PendingPair, RunSummary};
use super::vocabulary::Vocabulary;
use crate::db::{resolve_path, Database};

/// The most `autotag:updated` events per second, about.
const EVENT_INTERVAL: Duration = Duration::from_millis(250);
/// Thumbnails are downscaled to this on their long edge, about 350 image tokens.
const THUMBNAIL_EDGE: u32 = 512;
/// The waits before each retry of a retryable error. After the last, the item fails.
const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(8),
    Duration::from_secs(30),
];
/// The longest `retry-after` hint honoured, so one bad header can't hold a request slot for
/// hours.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(300);

/// The payload of `autotag:updated`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutotagUpdate {
    pub runs: Vec<RunSummary>,
    /// Assets whose tags changed since the last event.
    pub changed_asset_ids: Vec<String>,
}

pub type EventSink = Arc<dyn Fn(AutotagUpdate) + Send + Sync>;

/// A desktop notification.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub title: String,
    pub body: String,
}

pub type Notifier = Arc<dyn Fn(Notice) + Send + Sync>;

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

/// What a run covers: chosen assets, or a library's untagged assets (every asset with
/// `all_assets`).
#[derive(Debug, Clone, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RunScope {
    Assets {
        asset_ids: Vec<String>,
    },
    Library {
        library_id: String,
        all_assets: bool,
    },
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

/// Why an item stopped before its result was written.
#[derive(Debug, PartialEq)]
enum Stop {
    /// Only this item: it becomes `failed` with the message.
    Fail(String),
    /// The whole run would fail the same way: it is paused with the reason, and the item goes
    /// back to the queue.
    Pause(String),
    /// The run was paused or cancelled while the item waited to retry: the item goes back to
    /// the queue, or is skipped.
    Release,
}

impl From<String> for Stop {
    fn from(message: String) -> Self {
        Stop::Fail(message)
    }
}

impl From<&str> for Stop {
    fn from(message: &str) -> Self {
        Stop::Fail(message.to_string())
    }
}

/// The wait before retry number `retry` (0 for the first): the provider's hint when it gave one,
/// else the backoff schedule. None once the schedule is used up.
fn retry_delay(delays: &[Duration], retry: usize, hint: Option<Duration>) -> Option<Duration> {
    let scheduled = *delays.get(retry)?;
    Some(hint.map_or(scheduled, |hint| hint.min(MAX_RETRY_AFTER)))
}

pub struct Engine {
    db: Arc<Database>,
    pools: Vec<ProviderPool>,
    changed_assets: Mutex<HashSet<String>>,
    events_wake: Notify,
    sink: EventSink,
    notifier: Notifier,
    retry_delays: Vec<Duration>,
    /// Woken when a run is paused or cancelled, so items waiting to retry let go of their
    /// provider permit at once instead of at the end of the wait.
    run_stopped: Notify,
    /// Test-only fault: the library write of every item fails, as a NAS lock timeout would.
    #[cfg(test)]
    fail_library_writes: std::sync::atomic::AtomicBool,
    /// Test-only fault: `resume` can't run its finish check, as with a locked master DB.
    #[cfg(test)]
    fail_finishing: std::sync::atomic::AtomicBool,
}

impl Engine {
    pub fn new(
        db: Arc<Database>,
        providers: Vec<Arc<dyn TagProvider>>,
        sink: EventSink,
        notifier: Notifier,
    ) -> Arc<Self> {
        Self::with_retry_delays(db, providers, sink, notifier, RETRY_DELAYS.to_vec())
    }

    fn with_retry_delays(
        db: Arc<Database>,
        providers: Vec<Arc<dyn TagProvider>>,
        sink: EventSink,
        notifier: Notifier,
        retry_delays: Vec<Duration>,
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
            notifier,
            retry_delays,
            run_stopped: Notify::new(),
            #[cfg(test)]
            fail_library_writes: Default::default(),
            #[cfg(test)]
            fail_finishing: Default::default(),
        })
    }

    /// Pauses the runs a previous session left unfinished ("app restarted") and puts their
    /// running items back in the queue, replays writes still waiting for their library, then
    /// starts one scheduler per provider and the event loop.
    pub async fn start(self: &Arc<Self>) {
        if let Err(e) = store::recover_after_restart(&self.db.master_pool).await {
            tracing::error!("Could not pause the auto-tag runs of the last session: {e}");
        }
        self.replay(None).await;
        self.events_wake.notify_one();
        for index in 0..self.pools.len() {
            self.pools[index].wake.notify_one();
            tokio::spawn(self.clone().schedule(index));
        }
        tokio::spawn(self.clone().emit_events());
    }

    /// The writes waiting for `library_id`, read while it is still offline. Libraries open at
    /// startup, which `start` covers, and when adding a folder reopens the offline library it
    /// already is (`commands::library::open_library`), which reads these first, then opens the
    /// library, then hands them to `library_opened`.
    pub async fn waiting_writes(&self, library_id: &str) -> Result<Vec<PendingPair>, String> {
        store::pending_rows(&self.db, Some(library_id))
            .await
            .map_err(|e| e.to_string())
    }

    /// Replays `waiting`, read by `waiting_writes` before the library opened. Only those rows:
    /// once the library is open a scheduler can claim one of its items, whose own `pending`
    /// rows replay must leave to it, and other libraries' rows may belong to items in flight.
    pub async fn library_opened(&self, waiting: Vec<PendingPair>) {
        self.assets_changed(store::replay_rows(&self.db, waiting).await);
    }

    async fn replay(&self, library: Option<&str>) {
        match store::replay_pending(&self.db, library).await {
            Ok(changed) => self.assets_changed(changed),
            Err(e) => tracing::error!("Could not replay pending auto-tag writes: {e}"),
        }
    }

    /// Stops handing out the run's items. Items in flight finish.
    pub async fn pause_run(&self, run_id: &str) -> Result<(), String> {
        let paused = store::pause_run(&self.db.master_pool, run_id, None)
            .await
            .map_err(|e| e.to_string())?;
        if !paused {
            return Err("Only a running run can be paused".into());
        }
        self.run_stopped.notify_waiters();
        self.events_wake.notify_one();
        Ok(())
    }

    /// Hands out a paused run's items again, if its provider is ready.
    pub async fn resume_run(&self, run_id: &str) -> Result<(), String> {
        let paused = store::paused_runs(&self.db.master_pool)
            .await
            .map_err(|e| e.to_string())?;
        let (_, provider) = paused
            .into_iter()
            .find(|(id, _)| id == run_id)
            .ok_or("Only a paused run can be resumed")?;
        if let Some(reason) = self.unavailable_reason(&provider).await? {
            return Err(reason);
        }
        self.resume(&[run_id.to_string()]).await?;
        Ok(())
    }

    /// Resumes every paused run whose provider is ready, in one update so the schedulers see
    /// them all at once. Returns how many it resumed; the others stay paused.
    pub async fn resume_all(&self) -> Result<usize, String> {
        let paused = store::paused_runs(&self.db.master_pool)
            .await
            .map_err(|e| e.to_string())?;
        let mut ready = std::collections::HashMap::new();
        let mut run_ids = Vec::new();
        for (id, provider) in paused {
            if !ready.contains_key(&provider) {
                let reason = self.unavailable_reason(&provider).await?;
                ready.insert(provider.clone(), reason.is_none());
            }
            if ready[&provider] {
                run_ids.push(id);
            }
        }
        self.resume(&run_ids).await
    }

    async fn resume(&self, run_ids: &[String]) -> Result<usize, String> {
        let master = &self.db.master_pool;
        let resumed = store::resume_runs(master, run_ids)
            .await
            .map_err(|e| e.to_string())?;
        // A run paused after its last item was claimed has nothing left to hand out. The runs
        // are `running` already, so a failure here is logged, never returned: the schedulers
        // and the UI must still hear about them.
        for run_id in &resumed {
            #[cfg(test)]
            if self.fail_finishing.load(Ordering::SeqCst) {
                tracing::warn!("Test fault: not finishing resumed run {run_id}");
                continue;
            }
            self.finish_if_complete(run_id).await;
        }
        for pool in &self.pools {
            pool.wake.notify_one();
        }
        self.events_wake.notify_one();
        Ok(resumed.len())
    }

    /// Cancels the run: its queued items become `skipped`, and the tags it already applied stay.
    pub async fn cancel_run(&self, run_id: &str) -> Result<(), String> {
        let cancelled = store::cancel_run(&self.db.master_pool, run_id)
            .await
            .map_err(|e| e.to_string())?;
        if !cancelled {
            return Err("Only a running or paused run can be cancelled".into());
        }
        self.run_stopped.notify_waiters();
        self.events_wake.notify_one();
        Ok(())
    }

    /// Why a run of `provider_id` can't go on, when it can't. Reads the keyring off the async
    /// workers.
    async fn unavailable_reason(&self, provider_id: &str) -> Result<Option<String>, String> {
        let Ok(pool) = self.pool(provider_id) else {
            return Ok(Some(format!(
                "The {provider_id} provider isn't available in this session"
            )));
        };
        let provider = pool.provider.clone();
        tokio::task::spawn_blocking(move || provider.unavailable_reason())
            .await
            .map_err(|e| e.to_string())
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

    /// Queues a run over `scope`. Nothing is created, and nothing is sent, unless the provider
    /// is set up and can do `mode`.
    pub async fn start_run(
        &self,
        scope: RunScope,
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
        if mode == Mode::CreateFreely {
            return Err("Create freely runs aren't available yet".into());
        }

        let (items, requested, run_label) = match scope {
            RunScope::Assets { asset_ids } => {
                if asset_ids.is_empty() {
                    return Err("No assets to tag".into());
                }
                let items = store::locate_assets(&self.db, &asset_ids)
                    .await
                    .map_err(|e| e.to_string())?;
                if items.is_empty() {
                    return Err("None of these assets is in an open library".into());
                }
                let run_label = match items.len() {
                    1 => "1 asset".to_string(),
                    n => format!("{n} assets"),
                };
                (items, asset_ids.len(), run_label)
            }
            RunScope::Library {
                library_id,
                all_assets,
            } => {
                let lib = self
                    .db
                    .get_library_handle(&library_id)
                    .await
                    .ok_or("This library is offline")?;
                let asset_ids = store::library_assets(&lib.pool, all_assets)
                    .await
                    .map_err(|e| e.to_string())?;
                if asset_ids.is_empty() {
                    return Err(if all_assets {
                        format!("{} has no assets", lib.name)
                    } else {
                        format!("Every asset in {} already has a tag", lib.name)
                    });
                }
                let run_label = if all_assets {
                    format!("{}: all assets", lib.name)
                } else {
                    format!("{}: untagged assets", lib.name)
                };
                let n = asset_ids.len();
                let items = asset_ids
                    .into_iter()
                    .map(|id| (id, lib.id.clone()))
                    .collect();
                (items, n, run_label)
            }
        };
        let settings = settings::load(&self.db.master_pool)
            .await
            .map_err(|e| e.to_string())?;
        let id = uuid::Uuid::new_v4().to_string();
        store::create_run(
            &self.db.master_pool,
            NewRun {
                id: &id,
                label: &run_label,
                provider: provider_id,
                model: pool.provider.model(&settings).as_deref(),
                mode: mode.as_str(),
                options: serde_json::json!({
                    "maxTags": pool.provider.max_tags(&settings),
                    "maxProposals": pool.provider.max_proposals(&settings),
                    "threshold": pool.provider.threshold(&settings),
                }),
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
            skipped: requested - items.len(),
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
        let master = &self.db.master_pool;
        let recorded = match self.tag_item(provider, &item).await {
            Ok(changed) => {
                self.assets_changed(changed);
                Ok(())
            }
            Err(Stop::Fail(error)) => {
                tracing::warn!("Auto-tag item {} failed: {error}", item.asset_id);
                store::fail_item(master, &item, &error).await
            }
            Err(Stop::Pause(reason)) => {
                tracing::warn!("Auto-tag run {} paused: {reason}", item.run_id);
                match store::pause_run(master, &item.run_id, Some(&reason)).await {
                    Ok(_) => {
                        self.run_stopped.notify_waiters();
                        store::release_item(master, &item).await
                    }
                    Err(e) => Err(e),
                }
            }
            Err(Stop::Release) => store::release_item(master, &item).await,
        };
        if let Err(e) = recorded {
            tracing::error!("Could not record auto-tag item {}: {e}", item.asset_id);
        }
        self.finish_if_complete(&item.run_id).await;
        self.events_wake.notify_one();
    }

    /// Ends the run once nothing is left to hand out, and tells you about a run of several
    /// assets: it needs review, or it finished. Only the call that ends the run notifies, so
    /// each run notifies once.
    async fn finish_if_complete(&self, run_id: &str) {
        let master = &self.db.master_pool;
        let status = match store::finish_run_if_complete(master, run_id).await {
            Ok(Some(status)) => status,
            Ok(None) => return,
            Err(e) => {
                tracing::error!("Could not finish auto-tag run {run_id}: {e}");
                return;
            }
        };
        let run = match store::run_summary(master, run_id).await {
            Ok(Some(run)) => run,
            Ok(None) => return,
            Err(e) => {
                tracing::error!("Could not read finished auto-tag run {run_id}: {e}");
                return;
            }
        };
        if run.inline || run.total < 2 {
            return;
        }
        (self.notifier)(finish_notice(&run, &status));
    }

    /// The run's new tag names waiting for review, each with its assets.
    pub async fn proposals(&self, run_id: &str) -> Result<Vec<ReviewProposal>, String> {
        review::load(&self.db, run_id)
            .await
            .map_err(|e| e.to_string())
    }

    /// Applies the review of a run's new tag names. A run waiting for review becomes `done`
    /// once none is left; a cancelled run stays `cancelled`.
    pub async fn review_proposals(
        &self,
        run_id: &str,
        decisions: Vec<ProposalDecision>,
    ) -> Result<String, String> {
        let applied = review::apply(&self.db, run_id, decisions).await?;
        tracing::info!("Applied the review of auto-tag run {run_id}");
        self.assets_changed(applied.changed_assets);
        self.events_wake.notify_one();
        Ok(applied.status)
    }

    /// Asks the provider about one asset and writes the result through the write protocol.
    /// Returns the assets whose tags changed.
    async fn tag_item(&self, provider: &dyn TagProvider, item: &Item) -> Result<Vec<String>, Stop> {
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

        let image = match thumbnail.filter(|_| provider.sees_images()) {
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
        let max_tags = item.options["maxTags"].as_u64().unwrap_or(8) as usize;
        let mode: Mode = serde_json::from_value(serde_json::Value::String(item.mode.clone()))
            .map_err(|_| format!("Unknown mode {}", item.mode))?;
        let run_proposals = if mode == Mode::PreferExisting {
            store::pending_proposals(&self.db.master_pool, &item.run_id)
                .await
                .map_err(|e| e.to_string())?
        } else {
            Vec::new()
        };
        let request = TagRequest {
            run_id: item.run_id.clone(),
            mode,
            model: item.model.clone(),
            max_tags: max_tags as u32,
            max_proposals: item.options["maxProposals"].as_u64().unwrap_or(0) as u32,
            threshold: item.options["threshold"].as_f64(),
            asset: AssetInfo {
                file_name,
                relative_path,
                kind,
                extension,
                metadata: serde_json::from_str(&metadata).unwrap_or_default(),
            },
            image,
            vocabulary: vocabulary.clone(),
            run_proposals,
        };

        let result = self.call_provider(provider, &request, item).await?;
        let accepted = vocabulary.accepted_tags(&result, max_tags);
        let proposals = if mode == Mode::PreferExisting {
            vocabulary.new_proposals(&result, request.max_proposals as usize)
        } else {
            Vec::new()
        };

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
        let summary = serde_json::json!({ "tags": accepted, "proposed": proposals });
        store::record_intent(
            &self.db.master_pool,
            item,
            &new_tags,
            &proposals,
            &summary,
            &result.usage,
        )
        .await
        .map_err(|e| e.to_string())?;

        // Steps 3 and 4. A failure leaves the rows pending for replay; the item stays done.
        let write = async {
            #[cfg(test)]
            if self.fail_library_writes.load(Ordering::SeqCst) {
                return Err(sqlx::Error::PoolTimedOut);
            }
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

    /// Calls the provider, retrying a retryable error after each wait in `retry_delays`. A
    /// fatal error pauses the run, and an item error, or a retryable one that outlasts the
    /// retries, fails the item.
    async fn call_provider(
        &self,
        provider: &dyn TagProvider,
        request: &TagRequest,
        item: &Item,
    ) -> Result<TagResult, Stop> {
        let master = &self.db.master_pool;
        let mut retry = 0;
        loop {
            let error = match provider.tag(request).await {
                Ok(result) => return Ok(result),
                Err(error) => error,
            };
            tracing::debug!(
                "{} error ({:?}): {}",
                provider.id(),
                error.class,
                error.message
            );
            match error.class {
                ErrorClass::Item => return Err(Stop::Fail(error.message)),
                ErrorClass::Fatal => return Err(Stop::Pause(error.message)),
                ErrorClass::Retryable => {}
            }
            let Some(delay) = retry_delay(&self.retry_delays, retry, error.retry_after) else {
                return Err(Stop::Fail(format!(
                    "{} (gave up after {} tries)",
                    error.message,
                    retry + 1
                )));
            };
            retry += 1;
            tracing::info!(
                "Retrying auto-tag item {} in {delay:?}: {}",
                item.asset_id,
                error.message
            );
            let still_running = self
                .wait_to_retry(item, delay)
                .await
                .map_err(|e| e.to_string())?;
            if !still_running {
                return Err(Stop::Release);
            }
            store::count_attempt(master, item)
                .await
                .map_err(|e| e.to_string())?;
        }
    }

    /// Waits `delay` before a retry, ending early when the item's run is paused or cancelled.
    /// Returns whether the run is still running.
    async fn wait_to_retry(&self, item: &Item, delay: Duration) -> sqlx::Result<bool> {
        let master = &self.db.master_pool;
        let deadline = tokio::time::Instant::now() + delay;
        loop {
            // Listen before reading the status, so a stop landing between the two still wakes
            // the wait. Another run stopping wakes it too: it reads the status and waits on.
            let stopped = self.run_stopped.notified();
            tokio::pin!(stopped);
            stopped.as_mut().enable();
            if store::run_status(master, &item.run_id).await?.as_deref() != Some("running") {
                return Ok(false);
            }
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                _ = &mut stopped => {}
            }
        }
        Ok(store::run_status(master, &item.run_id).await?.as_deref() == Some("running"))
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

fn plural(n: i64, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// What the notification says when a run ends `status`.
fn finish_notice(run: &RunSummary, status: &str) -> Notice {
    if status == "awaiting_review" {
        return Notice {
            title: "Tagging run needs review".into(),
            body: format!(
                "{}: {} to review",
                run.label,
                plural(run.pending_proposals, "new tag")
            ),
        };
    }
    let mut body = format!("{}: {} tagged", run.label, plural(run.done, "asset"));
    if run.failed > 0 {
        body.push_str(&format!(", {} failed", run.failed));
    }
    Notice {
        title: "Tagging run finished".into(),
        body,
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
    use crate::autotag::provider::{ProposedTag, ProviderError, TagFuture};
    use crate::db::{
        insert_asset, insert_tag, memory_database, memory_library, memory_library_pool,
    };
    use sqlx::{Pool, Sqlite};
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    /// Counts calls and requests in flight, and answers every asset with `existing`, plus the
    /// new names `proposed` holds for its file name. Each call first takes the next of
    /// `failures` and returns it, while there is one. With `hang`, no call ever returns, like a
    /// request in flight when the app is killed.
    #[derive(Default)]
    struct Recording {
        existing: Vec<String>,
        proposed: std::collections::HashMap<String, Vec<ProposedTag>>,
        /// The run's earlier proposals each call was given, in call order.
        run_proposals_seen: Mutex<Vec<Vec<String>>>,
        failures: Mutex<VecDeque<ProviderError>>,
        hang: bool,
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
        fn max_proposals(&self, settings: &AiSettings) -> u32 {
            settings.anthropic.max_proposals
        }
        fn unavailable_reason(&self) -> Option<String> {
            self.unavailable.clone()
        }
        fn tag<'a>(&'a self, request: &'a TagRequest) -> TagFuture<'a> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(request.run_id.clone());
                self.run_proposals_seen.lock().unwrap().push(
                    request
                        .run_proposals
                        .iter()
                        .map(|p| p.name.clone())
                        .collect(),
                );
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_in_flight.fetch_max(now, Ordering::SeqCst);
                if self.hang {
                    std::future::pending::<()>().await;
                }
                tokio::time::sleep(self.delay).await;
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                if let Some(error) = self.failures.lock().unwrap().pop_front() {
                    return Err(error);
                }
                Ok(TagResult {
                    existing: self.existing.clone(),
                    proposed: self
                        .proposed
                        .get(&request.asset.file_name)
                        .cloned()
                        .unwrap_or_default(),
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
        notices: Arc<Mutex<Vec<Notice>>>,
    }

    type Recorded = (
        Arc<Engine>,
        Arc<Recording>,
        Arc<Mutex<Vec<AutotagUpdate>>>,
        Arc<Mutex<Vec<Notice>>>,
    );

    async fn setup(provider: Recording, assets: &[&str]) -> Setup {
        setup_with_delays(provider, assets, vec![Duration::from_millis(1); 3]).await
    }

    async fn setup_with_delays(
        provider: Recording,
        assets: &[&str],
        retry_delays: Vec<Duration>,
    ) -> Setup {
        let lib_pool = memory_library_pool().await;
        for id in assets {
            insert_asset(&lib_pool, id).await;
        }
        let db = Arc::new(memory_database(vec![memory_library("lib", lib_pool.clone())]).await);
        let (engine, provider, updates, notices) = engine_with_delays(db, provider, retry_delays);
        Setup {
            engine,
            provider,
            lib_pool,
            updates,
            notices,
        }
    }

    /// An engine over `db` whose retries wait a millisecond.
    fn engine_on(db: Arc<Database>, provider: Recording) -> Recorded {
        engine_with_delays(db, provider, vec![Duration::from_millis(1); 3])
    }

    fn engine_with_delays(
        db: Arc<Database>,
        provider: Recording,
        retry_delays: Vec<Duration>,
    ) -> Recorded {
        let provider = Arc::new(provider);
        let updates = Arc::new(Mutex::new(Vec::new()));
        let recorded = updates.clone();
        let notices = Arc::new(Mutex::new(Vec::new()));
        let noticed = notices.clone();
        let engine = Engine::with_retry_delays(
            db,
            vec![provider.clone()],
            Arc::new(move |update| recorded.lock().unwrap().push(update)),
            Arc::new(move |notice| noticed.lock().unwrap().push(notice)),
            retry_delays,
        );
        (engine, provider, updates, notices)
    }

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn scope(names: &[&str]) -> RunScope {
        RunScope::Assets {
            asset_ids: ids(names),
        }
    }

    fn failures(errors: Vec<ProviderError>) -> Mutex<VecDeque<ProviderError>> {
        Mutex::new(errors.into())
    }

    async fn wait_for(engine: &Engine, done: impl Fn(&[RunSummary]) -> bool) -> Vec<RunSummary> {
        for _ in 0..500 {
            let runs = engine.list_runs().await.unwrap();
            if done(&runs) {
                return runs;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("runs never got there: {:?}", engine.list_runs().await);
    }

    async fn wait_until_done(engine: &Engine) -> Vec<RunSummary> {
        wait_for(engine, |runs| runs.iter().all(|r| r.status == "done")).await
    }

    async fn item(master: &Pool<Sqlite>, asset: &str) -> (String, Option<String>, i64) {
        sqlx::query_as("SELECT status, error, attempts FROM tag_run_items WHERE asset_id = ?")
            .bind(asset)
            .fetch_one(master)
            .await
            .unwrap()
    }

    async fn count(pool: &Pool<Sqlite>, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
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
            .start_run(scope(&["a1"]), "recording", Mode::ExistingOnly)
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
            .start_run(scope(&assets[..4]), "recording", Mode::ExistingOnly)
            .await
            .unwrap();
        let second = s
            .engine
            .start_run(scope(&assets[4..]), "recording", Mode::ExistingOnly)
            .await
            .unwrap();

        // Startup pauses both runs; Resume all sets them running together.
        s.engine.start().await;
        assert_eq!(s.engine.resume_all().await.unwrap(), 2);
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
            .start_run(scope(&["a1"]), "recording", Mode::ExistingOnly)
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
            .start_run(scope(&["a1"]), "recording", Mode::CreateFreely)
            .await
            .unwrap_err();

        assert!(error.contains("Create freely"), "{error}");
        assert!(s.engine.list_runs().await.unwrap().is_empty());
    }

    fn proposal(name: &str, parent: Option<&str>) -> ProposedTag {
        ProposedTag {
            name: name.into(),
            parent_id: parent.map(Into::into),
        }
    }

    async fn proposals_of(master: &Pool<Sqlite>) -> Vec<(String, Option<String>, String)> {
        sqlx::query_as(
            "SELECT p.name, p.parent_id, group_concat(pa.asset_id, ',')
             FROM tag_run_proposals p JOIN tag_run_proposal_assets pa ON pa.proposal_id = p.id
             WHERE p.status = 'pending' GROUP BY p.id ORDER BY p.rowid",
        )
        .fetch_all(master)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn prefer_existing_merges_rock_and_rock_into_one_proposal_and_ends_awaiting_review() {
        let s = setup(
            Recording {
                proposed: [
                    ("a1.glb", vec![proposal("Rock", Some("t-nature"))]),
                    (
                        "a2.glb",
                        vec![
                            proposal(" rock", None),
                            proposal("Moss", Some("t-ghost")),
                            proposal("Fern", None),
                        ],
                    ),
                    // A proposal naming a catalog tag counts as that tag.
                    ("a3.glb", vec![proposal("TREE", None)]),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
                ..Default::default()
            },
            &["a1", "a2", "a3"],
        )
        .await;
        let master = &s.engine.db.master_pool;
        insert_tag(master, "t-nature", "Nature").await;
        insert_tag(master, "t-tree", "Tree").await;
        settings::update(
            master,
            serde_json::json!({"anthropic": {"concurrency": 1, "maxProposals": 2}}),
        )
        .await
        .unwrap();

        s.engine.start().await;
        let started = s
            .engine
            .start_run(
                scope(&["a1", "a2", "a3"]),
                "recording",
                Mode::PreferExisting,
            )
            .await
            .unwrap();
        let runs = wait_for(&s.engine, |runs| runs[0].status == "awaiting_review").await;

        assert_eq!((runs[0].done, runs[0].pending_proposals), (3, 2));
        assert!(runs[0].finished_at.is_some());
        assert_eq!(
            proposals_of(master).await,
            [
                ("Rock".into(), Some("t-nature".into()), "a1,a2".into()),
                ("Moss".into(), None, "a2".into()),
            ],
            "merged by normalized name, the per-asset cap of 2 drops Fern, an unknown parent \
             is cleared, and Tree is no proposal"
        );
        let seen = s.provider.run_proposals_seen.lock().unwrap().clone();
        assert_eq!(seen[0], Vec::<String>::new());
        assert_eq!(seen[1], ["Rock"], "later assets share the run's proposals");
        assert_eq!(seen[2], ["Rock", "Moss"]);
        let tags: Vec<(String, String)> =
            sqlx::query_as("SELECT asset_id, tag_id FROM asset_tags ORDER BY asset_id")
                .fetch_all(&s.lib_pool)
                .await
                .unwrap();
        assert_eq!(tags, [("a3".into(), "t-tree".into())]);
        let review = s.engine.proposals(&started.run.id).await.unwrap();
        assert_eq!(
            review[0].parent_id.as_deref(),
            Some("t-nature"),
            "the review preselects the suggested parent"
        );
        assert_eq!(
            *s.notices.lock().unwrap(),
            [Notice {
                title: "Tagging run needs review".into(),
                body: "3 assets: 2 new tags to review".into(),
            }]
        );
    }

    #[tokio::test]
    async fn a_run_with_nothing_to_review_ends_done_and_notifies_once_unless_it_is_one_asset() {
        let s = setup(
            Recording {
                existing: ids(&["t-rock"]),
                ..Default::default()
            },
            &["a1", "a2", "a3"],
        )
        .await;
        insert_tag(&s.engine.db.master_pool, "t-rock", "Rock").await;
        s.engine.start().await;

        s.engine
            .start_run(scope(&["a1", "a2"]), "recording", Mode::PreferExisting)
            .await
            .unwrap();
        wait_until_done(&s.engine).await;
        s.engine
            .start_run(scope(&["a3"]), "recording", Mode::ExistingOnly)
            .await
            .unwrap();
        wait_for(&s.engine, |runs| {
            runs.len() == 2 && runs.iter().all(|r| r.status == "done")
        })
        .await;
        tokio::time::sleep(Duration::from_millis(30)).await;

        assert_eq!(
            *s.notices.lock().unwrap(),
            [Notice {
                title: "Tagging run finished".into(),
                body: "2 assets: 2 assets tagged".into(),
            }],
            "the one-asset run sends none"
        );
    }

    #[tokio::test]
    async fn a_fifty_asset_run_over_two_libraries_notifies_once_and_its_review_applies_everywhere()
    {
        let a = memory_library_pool().await;
        let b = memory_library_pool().await;
        let assets: Vec<String> = (0..50).map(|i| format!("x{i:02}")).collect();
        for (i, id) in assets.iter().enumerate() {
            insert_asset(if i < 30 { &a } else { &b }, id).await;
        }
        let db = Arc::new(
            memory_database(vec![
                memory_library("a-lib", a.clone()),
                memory_library("b-lib", b.clone()),
            ])
            .await,
        );
        insert_tag(&db.master_pool, "t-tree", "Tree").await;
        let proposed = assets
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let rock = if i % 2 == 0 { "Rock" } else { "rock" };
                let mut names = vec![proposal(rock, None)];
                if i % 10 == 0 {
                    names.push(proposal("Moss", None));
                }
                (format!("{id}.glb"), names)
            })
            .collect();
        let (engine, provider, _, notices) = engine_on(
            db.clone(),
            Recording {
                existing: ids(&["t-tree"]),
                proposed,
                ..Default::default()
            },
        );
        engine.start().await;

        let started = engine
            .start_run(
                RunScope::Assets {
                    asset_ids: assets.clone(),
                },
                "recording",
                Mode::PreferExisting,
            )
            .await
            .unwrap();
        let runs = wait_for(&engine, |runs| runs[0].status == "awaiting_review").await;

        assert_eq!((runs[0].done, runs[0].pending_proposals), (50, 2));
        assert_eq!(provider.calls.lock().unwrap().len(), 50);
        assert_eq!(
            count(
                &a,
                "SELECT COUNT(*) FROM asset_tags WHERE tag_id = 't-tree'"
            )
            .await
                + count(
                    &b,
                    "SELECT COUNT(*) FROM asset_tags WHERE tag_id = 't-tree'"
                )
                .await,
            50,
            "matches are applied while the run goes"
        );
        assert_eq!(notices.lock().unwrap().len(), 1);

        // Accept both, unticking x01 (library A) and x31 (library B) for Rock.
        let review = engine.proposals(&started.run.id).await.unwrap();
        let decisions = review
            .iter()
            .map(|p| ProposalDecision::Accept {
                proposal_id: p.id.clone(),
                name: p.name.clone(),
                parent_id: p.parent_id.clone(),
                asset_ids: p
                    .assets
                    .iter()
                    .map(|a| a.asset_id.clone())
                    .filter(|a| a != "x01" && a != "x31")
                    .collect(),
            })
            .collect();
        let status = engine
            .review_proposals(&started.run.id, decisions)
            .await
            .unwrap();

        assert_eq!(status, "done");
        let rock_and_moss = "SELECT COUNT(*) FROM tags WHERE name IN ('Rock', 'Moss')";
        for pool in [&db.master_pool, &a, &b] {
            assert_eq!(
                count(pool, rock_and_moss).await,
                2,
                "master and both mirrors"
            );
        }
        let tagged = |pool, tag| async move {
            let sql = format!(
                "SELECT COUNT(*) FROM asset_tags at JOIN tags t ON t.id = at.tag_id
                 WHERE t.name = '{tag}'"
            );
            count(pool, &sql).await
        };
        assert_eq!(tagged(&a, "Rock").await, 29);
        assert_eq!(tagged(&b, "Rock").await, 19);
        assert_eq!(tagged(&a, "Moss").await, 3);
        assert_eq!(tagged(&b, "Moss").await, 2);
        assert_eq!(
            count(
                &db.master_pool,
                "SELECT COUNT(*) FROM tag_run_applied WHERE state = 'applied'"
            )
            .await,
            50 + 48 + 5,
            "every pair the run added is recorded for undo"
        );
        let runs = engine.list_runs().await.unwrap();
        assert_eq!(
            (runs[0].status.as_str(), runs[0].pending_proposals),
            ("done", 0)
        );
        assert_eq!(notices.lock().unwrap().len(), 1, "still one notification");
    }

    async fn items_of(engine: &Engine, run_id: &str) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT asset_id, library_id FROM tag_run_items WHERE run_id = ? ORDER BY asset_id",
        )
        .bind(run_id)
        .fetch_all(&engine.db.master_pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_library_run_covers_its_untagged_assets_or_every_asset_with_all_assets() {
        let lib_pool = memory_library_pool().await;
        for id in ["a3", "a1", "a2"] {
            insert_asset(&lib_pool, id).await;
        }
        insert_tag(&lib_pool, "t-rock", "Rock").await;
        sqlx::query("INSERT INTO asset_tags (asset_id, tag_id) VALUES ('a2', 't-rock')")
            .execute(&lib_pool)
            .await
            .unwrap();
        let other = memory_library_pool().await;
        insert_asset(&other, "b1").await;
        let db = memory_database(vec![
            memory_library("lib", lib_pool),
            memory_library("other", other),
        ])
        .await;
        let engine = Engine::new(
            Arc::new(db),
            vec![Arc::new(Recording::default())],
            Arc::new(|_| {}),
            Arc::new(|_| {}),
        );
        let library = |all_assets| RunScope::Library {
            library_id: "lib".into(),
            all_assets,
        };

        let untagged = engine
            .start_run(library(false), "recording", Mode::ExistingOnly)
            .await
            .unwrap();
        let all = engine
            .start_run(library(true), "recording", Mode::ExistingOnly)
            .await
            .unwrap();

        let pair = |id: &str| (id.to_string(), "lib".to_string());
        assert_eq!(
            items_of(&engine, &untagged.run.id).await,
            [pair("a1"), pair("a3")]
        );
        assert_eq!((untagged.queued, untagged.skipped), (2, 0));
        assert_eq!(untagged.run.label, "lib: untagged assets");
        assert_eq!(
            items_of(&engine, &all.run.id).await,
            [pair("a1"), pair("a2"), pair("a3")]
        );
        assert_eq!(all.run.label, "lib: all assets");
    }

    #[tokio::test]
    async fn a_library_run_with_nothing_to_tag_is_refused() {
        let s = setup(Recording::default(), &["a1"]).await;
        insert_tag(&s.lib_pool, "t-rock", "Rock").await;
        sqlx::query("INSERT INTO asset_tags (asset_id, tag_id) VALUES ('a1', 't-rock')")
            .execute(&s.lib_pool)
            .await
            .unwrap();
        let start = |library_id: &str| {
            s.engine.start_run(
                RunScope::Library {
                    library_id: library_id.into(),
                    all_assets: false,
                },
                "recording",
                Mode::ExistingOnly,
            )
        };

        assert_eq!(
            start("lib").await.unwrap_err(),
            "Every asset in lib already has a tag"
        );
        assert_eq!(
            start("offline").await.unwrap_err(),
            "This library is offline"
        );
        assert!(s.engine.list_runs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_all_assets_run_on_an_empty_library_is_refused() {
        let s = setup(Recording::default(), &[]).await;

        let error = s
            .engine
            .start_run(
                RunScope::Library {
                    library_id: "lib".into(),
                    all_assets: true,
                },
                "recording",
                Mode::ExistingOnly,
            )
            .await
            .unwrap_err();

        assert_eq!(error, "lib has no assets");
        assert!(s.engine.list_runs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_missing_asset_fails_its_item_and_the_run_still_finishes() {
        let s = setup(Recording::default(), &["a1", "a2"]).await;
        let started = s
            .engine
            .start_run(
                scope(&["a1", "a2", "nowhere"]),
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
        s.engine.resume_all().await.unwrap();
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
    async fn retryable_errors_retry_three_times_then_fail_the_item() {
        let s = setup(
            Recording {
                existing: ids(&["t-rock"]),
                failures: failures(vec![
                    ProviderError::new(ErrorClass::Retryable, "Overloaded");
                    4
                ]),
                ..Default::default()
            },
            &["a1"],
        )
        .await;
        let master = &s.engine.db.master_pool;
        insert_tag(master, "t-rock", "Rock").await;
        s.engine.start().await;

        s.engine
            .start_run(scope(&["a1"]), "recording", Mode::ExistingOnly)
            .await
            .unwrap();
        let runs = wait_until_done(&s.engine).await;

        assert_eq!((runs[0].done, runs[0].failed), (0, 1));
        assert_eq!(
            s.provider.calls.lock().unwrap().len(),
            4,
            "one try, three retries"
        );
        assert_eq!(
            item(master, "a1").await,
            (
                "failed".into(),
                Some("Overloaded (gave up after 4 tries)".into()),
                4
            )
        );
        assert_eq!(
            count(master, "SELECT COUNT(*) FROM tag_run_applied").await,
            0
        );
        assert_eq!(
            count(&s.lib_pool, "SELECT COUNT(*) FROM asset_tags").await,
            0
        );
    }

    #[tokio::test]
    async fn a_retryable_error_that_clears_lets_the_item_finish() {
        let s = setup(
            Recording {
                existing: ids(&["t-rock"]),
                failures: failures(vec![
                    ProviderError::new(ErrorClass::Retryable, "Rate limited")
                        .with_retry_after(Duration::from_millis(5)),
                    ProviderError::new(ErrorClass::Retryable, "Server error"),
                ]),
                ..Default::default()
            },
            &["a1"],
        )
        .await;
        let master = &s.engine.db.master_pool;
        insert_tag(master, "t-rock", "Rock").await;
        s.engine.start().await;

        s.engine
            .start_run(scope(&["a1"]), "recording", Mode::ExistingOnly)
            .await
            .unwrap();
        let runs = wait_until_done(&s.engine).await;

        assert_eq!((runs[0].done, runs[0].failed), (1, 0));
        assert_eq!(s.provider.calls.lock().unwrap().len(), 3);
        assert_eq!(item(master, "a1").await, ("done".into(), None, 3));
        assert_eq!(
            count(&s.lib_pool, "SELECT COUNT(*) FROM asset_tags").await,
            1
        );
    }

    #[test]
    fn retries_wait_2_8_and_30_seconds_unless_the_provider_says_otherwise() {
        let wait = |retry, hint| retry_delay(&RETRY_DELAYS, retry, hint);
        let secs = Duration::from_secs;

        assert_eq!(wait(0, None), Some(secs(2)));
        assert_eq!(wait(1, None), Some(secs(8)));
        assert_eq!(wait(2, None), Some(secs(30)));
        assert_eq!(wait(3, None), None, "the item fails after the third retry");
        assert_eq!(
            wait(0, Some(secs(5))),
            Some(secs(5)),
            "retry-after is honoured"
        );
        assert_eq!(wait(2, Some(secs(1))), Some(secs(1)));
        assert_eq!(wait(0, Some(secs(3600))), Some(MAX_RETRY_AFTER));
        assert_eq!(wait(3, Some(secs(5))), None, "a hint never adds a retry");
    }

    #[tokio::test]
    async fn a_fatal_error_pauses_the_run_with_its_reason_and_requeues_the_item() {
        let s = setup(
            Recording {
                existing: ids(&["t-rock"]),
                failures: failures(vec![ProviderError::new(
                    ErrorClass::Fatal,
                    "Anthropic rejected the API key",
                )]),
                ..Default::default()
            },
            &["a1", "a2"],
        )
        .await;
        let master = &s.engine.db.master_pool;
        insert_tag(master, "t-rock", "Rock").await;
        settings::update(master, serde_json::json!({"anthropic": {"concurrency": 1}}))
            .await
            .unwrap();
        s.engine.start().await;

        let started = s
            .engine
            .start_run(scope(&["a1", "a2"]), "recording", Mode::ExistingOnly)
            .await
            .unwrap();
        let runs = wait_for(&s.engine, |runs| {
            runs[0].status == "paused" && runs[0].running == 0
        })
        .await;

        assert_eq!(
            runs[0].pause_reason.as_deref(),
            Some("Anthropic rejected the API key")
        );
        assert_eq!((runs[0].queued, runs[0].done, runs[0].failed), (2, 0, 0));
        assert_eq!(item(master, "a1").await.0, "queued");
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            s.provider.calls.lock().unwrap().len(),
            1,
            "no call while paused"
        );

        s.engine.resume_run(&started.run.id).await.unwrap();
        let runs = wait_until_done(&s.engine).await;

        assert_eq!((runs[0].done, runs[0].failed), (2, 0));
        assert_eq!(runs[0].pause_reason, None);
        assert_eq!(s.provider.calls.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn an_item_error_fails_only_that_item() {
        let s = setup(
            Recording {
                failures: failures(vec![ProviderError::new(ErrorClass::Item, "Refused")]),
                ..Default::default()
            },
            &["a1", "a2"],
        )
        .await;
        let master = &s.engine.db.master_pool;
        settings::update(master, serde_json::json!({"anthropic": {"concurrency": 1}}))
            .await
            .unwrap();
        s.engine.start().await;

        s.engine
            .start_run(scope(&["a1", "a2"]), "recording", Mode::ExistingOnly)
            .await
            .unwrap();
        let runs = wait_until_done(&s.engine).await;

        assert_eq!((runs[0].done, runs[0].failed), (1, 1));
        assert_eq!(s.provider.calls.lock().unwrap().len(), 2, "no retry");
        assert_eq!(
            item(master, "a1").await,
            ("failed".into(), Some("Refused".into()), 1)
        );
    }

    /// A run over `n` assets, one request at a time, each taking 20 ms.
    async fn slow_run(n: usize) -> (Setup, String) {
        let assets: Vec<String> = (0..n).map(|i| format!("a{i}")).collect();
        let names: Vec<&str> = assets.iter().map(String::as_str).collect();
        let s = setup(
            Recording {
                existing: ids(&["t-rock"]),
                delay: Duration::from_millis(20),
                ..Default::default()
            },
            &names,
        )
        .await;
        let master = &s.engine.db.master_pool;
        insert_tag(master, "t-rock", "Rock").await;
        settings::update(master, serde_json::json!({"anthropic": {"concurrency": 1}}))
            .await
            .unwrap();
        s.engine.start().await;
        let started = s
            .engine
            .start_run(
                RunScope::Assets { asset_ids: assets },
                "recording",
                Mode::ExistingOnly,
            )
            .await
            .unwrap();
        wait_for(&s.engine, |runs| runs[0].done >= 1).await;
        (s, started.run.id)
    }

    #[tokio::test]
    async fn pause_stops_handing_out_items_and_resume_finishes_the_run() {
        let (s, run_id) = slow_run(6).await;

        s.engine.pause_run(&run_id).await.unwrap();
        let paused = wait_for(&s.engine, |runs| runs[0].running == 0).await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        let still = s.engine.list_runs().await.unwrap();

        assert_eq!(still[0].status, "paused");
        assert_eq!(
            still[0].pause_reason, None,
            "a pause you ask for has no reason"
        );
        assert_eq!(
            still[0].done, paused[0].done,
            "no item finishes while paused"
        );
        assert!(still[0].queued > 0);
        assert!(s.engine.pause_run(&run_id).await.is_err());

        s.engine.resume_run(&run_id).await.unwrap();
        let runs = wait_until_done(&s.engine).await;

        assert_eq!(runs[0].done, 6);
        assert_eq!(s.provider.calls.lock().unwrap().len(), 6);
        assert!(s.engine.resume_run(&run_id).await.is_err());
    }

    #[tokio::test]
    async fn cancel_skips_the_remaining_items_and_keeps_applied_tags() {
        let (s, run_id) = slow_run(6).await;
        let master = &s.engine.db.master_pool;

        s.engine.cancel_run(&run_id).await.unwrap();
        let runs = wait_for(&s.engine, |runs| runs[0].running == 0).await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        let run = &s.engine.list_runs().await.unwrap()[0];

        assert_eq!(run.status, "cancelled");
        assert!(run.finished_at.is_some());
        assert_eq!((run.done, run.skipped), (runs[0].done, runs[0].skipped));
        assert_eq!(run.done + run.skipped, 6);
        assert!(run.skipped > 0, "{run:?}");
        assert_eq!(s.provider.calls.lock().unwrap().len() as i64, run.done);
        assert_eq!(
            count(&s.lib_pool, "SELECT COUNT(*) FROM asset_tags").await,
            run.done,
            "the tags already applied stay"
        );
        assert_eq!(
            count(
                master,
                "SELECT COUNT(*) FROM tag_run_applied WHERE state = 'applied'"
            )
            .await,
            run.done,
            "and stay undoable"
        );
        for refused in [
            s.engine.cancel_run(&run_id).await,
            s.engine.pause_run(&run_id).await,
            s.engine.resume_run(&run_id).await,
        ] {
            assert!(refused.is_err());
        }
    }

    #[tokio::test]
    async fn a_paused_run_can_be_cancelled() {
        let (s, run_id) = slow_run(4).await;
        s.engine.pause_run(&run_id).await.unwrap();

        s.engine.cancel_run(&run_id).await.unwrap();
        let runs = wait_for(&s.engine, |runs| runs[0].running == 0).await;

        assert_eq!(runs[0].status, "cancelled");
        assert_eq!(runs[0].queued, 0);
        assert_eq!(runs[0].done + runs[0].skipped, 4);
    }

    /// A one-asset run whose first call fails with a retryable error, then waits a minute
    /// before its retry. Returns once that wait has begun.
    async fn run_waiting_to_retry() -> (Setup, String) {
        let s = setup_with_delays(
            Recording {
                existing: ids(&["t-rock"]),
                failures: failures(vec![ProviderError::new(
                    ErrorClass::Retryable,
                    "Overloaded",
                )]),
                ..Default::default()
            },
            &["a1"],
            vec![Duration::from_secs(60); 3],
        )
        .await;
        insert_tag(&s.engine.db.master_pool, "t-rock", "Rock").await;
        s.engine.start().await;
        let started = s
            .engine
            .start_run(scope(&["a1"]), "recording", Mode::ExistingOnly)
            .await
            .unwrap();
        for _ in 0..100 {
            if !s.provider.calls.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(s.provider.calls.lock().unwrap().len(), 1);
        (s, started.run.id)
    }

    #[tokio::test]
    async fn a_pause_during_a_retry_wait_requeues_the_item_without_retrying() {
        let (s, run_id) = run_waiting_to_retry().await;
        let master = &s.engine.db.master_pool;

        let paused_at = std::time::Instant::now();
        s.engine.pause_run(&run_id).await.unwrap();
        let runs = wait_for(&s.engine, |runs| runs[0].running == 0).await;

        assert!(
            paused_at.elapsed() < Duration::from_secs(2),
            "the item lets go of its permit at once, not after the minute's wait"
        );
        assert_eq!(runs[0].status, "paused");
        assert_eq!(item(master, "a1").await, ("queued".into(), None, 1));
        assert_eq!(
            s.provider.calls.lock().unwrap().len(),
            1,
            "no retry once paused"
        );

        s.engine.resume_run(&run_id).await.unwrap();
        let runs = wait_until_done(&s.engine).await;

        assert_eq!((runs[0].done, runs[0].failed), (1, 0));
        assert_eq!(s.provider.calls.lock().unwrap().len(), 2);
        assert_eq!(
            count(&s.lib_pool, "SELECT COUNT(*) FROM asset_tags").await,
            1
        );
    }

    #[tokio::test]
    async fn a_cancel_during_a_retry_wait_skips_the_item_without_retrying() {
        let (s, run_id) = run_waiting_to_retry().await;
        let master = &s.engine.db.master_pool;

        let cancelled_at = std::time::Instant::now();
        s.engine.cancel_run(&run_id).await.unwrap();
        let runs = wait_for(&s.engine, |runs| runs[0].running == 0).await;

        assert!(
            cancelled_at.elapsed() < Duration::from_secs(2),
            "the item lets go of its permit at once, not after the minute's wait"
        );
        assert_eq!(runs[0].status, "cancelled");
        assert_eq!((runs[0].skipped, runs[0].queued, runs[0].failed), (1, 0, 0));
        assert_eq!(item(master, "a1").await.0, "skipped");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            s.provider.calls.lock().unwrap().len(),
            1,
            "no retry once cancelled"
        );
        assert_eq!(
            count(&s.lib_pool, "SELECT COUNT(*) FROM asset_tags").await,
            0
        );
    }

    #[tokio::test]
    async fn a_fatal_error_ends_another_items_retry_wait_in_the_same_run() {
        // Two items in flight: the first call fails retryably and waits a minute to retry, the
        // second fails fatally and pauses the run.
        let s = setup_with_delays(
            Recording {
                failures: failures(vec![
                    ProviderError::new(ErrorClass::Retryable, "Overloaded"),
                    ProviderError::new(ErrorClass::Fatal, "Anthropic rejected the API key"),
                ]),
                delay: Duration::from_millis(20),
                ..Default::default()
            },
            &["a1", "a2"],
            vec![Duration::from_secs(60); 3],
        )
        .await;
        let master = &s.engine.db.master_pool;
        settings::update(master, serde_json::json!({"anthropic": {"concurrency": 2}}))
            .await
            .unwrap();
        s.engine.start().await;

        let started_at = std::time::Instant::now();
        s.engine
            .start_run(scope(&["a1", "a2"]), "recording", Mode::ExistingOnly)
            .await
            .unwrap();
        let runs = wait_for(&s.engine, |runs| {
            runs[0].status == "paused" && runs[0].running == 0
        })
        .await;

        assert!(started_at.elapsed() < Duration::from_secs(2));
        assert_eq!(
            runs[0].pause_reason.as_deref(),
            Some("Anthropic rejected the API key")
        );
        assert_eq!((runs[0].queued, runs[0].done, runs[0].failed), (2, 0, 0));
        assert_eq!(
            s.provider.calls.lock().unwrap().len(),
            2,
            "no retry once paused"
        );
    }

    #[tokio::test]
    async fn a_run_still_being_created_can_be_cancelled_but_not_paused() {
        let s = setup(Recording::default(), &["a1"]).await;
        let master = &s.engine.db.master_pool;
        s.engine.start().await;
        // create_run has committed one batch and is inserting the next.
        sqlx::query(
            "INSERT INTO tag_runs (id, label, provider, mode, status, created_at)
             VALUES ('creating', 'run', 'recording', 'existing_only', 'queued', 0)",
        )
        .execute(master)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tag_run_items (run_id, asset_id, library_id) VALUES ('creating', 'a1', 'lib')",
        )
        .execute(master)
        .await
        .unwrap();

        assert!(s.engine.pause_run("creating").await.is_err());
        assert_eq!(status_of(master, "creating").await, ("queued".into(), None));

        s.engine.cancel_run("creating").await.unwrap();
        assert_eq!(status_of(master, "creating").await.0, "cancelled");
        assert!(s.provider.calls.lock().unwrap().is_empty());
    }

    async fn insert_paused_run(master: &Pool<Sqlite>, id: &str, provider: &str, asset: &str) {
        sqlx::query(
            "INSERT INTO tag_runs (id, label, provider, mode, status, pause_reason, created_at)
             VALUES (?, 'run', ?, 'existing_only', 'paused', 'app restarted', 0)",
        )
        .bind(id)
        .bind(provider)
        .execute(master)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tag_run_items (run_id, asset_id, library_id) VALUES (?, ?, 'lib')",
        )
        .bind(id)
        .bind(asset)
        .execute(master)
        .await
        .unwrap();
    }

    async fn status_of(master: &Pool<Sqlite>, run_id: &str) -> (String, Option<String>) {
        sqlx::query_as("SELECT status, pause_reason FROM tag_runs WHERE id = ?")
            .bind(run_id)
            .fetch_one(master)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn resume_refuses_a_run_whose_provider_is_not_set_up() {
        let s = setup(
            Recording {
                unavailable: Some("Set up Recording in Settings".into()),
                ..Default::default()
            },
            &["a1"],
        )
        .await;
        let master = &s.engine.db.master_pool;
        insert_paused_run(master, "run", "recording", "a1").await;
        s.engine.start().await;

        let error = s.engine.resume_run("run").await.unwrap_err();
        let resumed = s.engine.resume_all().await.unwrap();

        assert_eq!(error, "Set up Recording in Settings");
        assert_eq!(resumed, 0);
        assert_eq!(
            status_of(master, "run").await,
            ("paused".into(), Some("app restarted".into()))
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(s.provider.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn resume_all_resumes_only_runs_whose_provider_is_ready() {
        let s = setup(Recording::default(), &["a1", "a2"]).await;
        let master = &s.engine.db.master_pool;
        insert_paused_run(master, "ready", "recording", "a1").await;
        // A Fake run, reopened in a session started without OPENDAM_AUTOTAG_FAKE.
        insert_paused_run(master, "gone", "fake", "a2").await;
        s.engine.start().await;

        assert_eq!(s.engine.resume_all().await.unwrap(), 1);
        wait_for(&s.engine, |runs| {
            runs.iter().any(|r| r.id == "ready" && r.status == "done")
        })
        .await;

        assert_eq!(
            status_of(master, "gone").await,
            ("paused".into(), Some("app restarted".into()))
        );
        assert_eq!(
            s.engine.resume_run("gone").await.unwrap_err(),
            "The fake provider isn't available in this session"
        );
        assert_eq!(s.provider.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn resuming_a_run_with_nothing_left_to_hand_out_finishes_it() {
        let s = setup(Recording::default(), &["a1", "a2", "a3", "a4"]).await;
        let master = &s.engine.db.master_pool;
        // Paused while its last items were in flight, which then finished.
        insert_paused_run(master, "one", "recording", "a1").await;
        insert_paused_run(master, "two", "recording", "a2").await;
        sqlx::query("UPDATE tag_run_items SET status = 'done'")
            .execute(master)
            .await
            .unwrap();
        insert_paused_run(master, "all", "recording", "a3").await;
        sqlx::query(
            "INSERT INTO tag_run_items (run_id, asset_id, library_id) VALUES ('all', 'a4', 'lib')",
        )
        .execute(master)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE tag_run_items SET status = 'failed' WHERE run_id = 'all' AND asset_id = 'a3'",
        )
        .execute(master)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE tag_run_items SET status = 'done' WHERE run_id = 'all' AND asset_id = 'a4'",
        )
        .execute(master)
        .await
        .unwrap();
        s.engine.start().await;

        s.engine.resume_run("one").await.unwrap();
        assert_eq!(s.engine.resume_all().await.unwrap(), 2);

        for run in ["one", "two", "all"] {
            let (status, reason, finished_at): (String, Option<String>, Option<i64>) =
                sqlx::query_as(
                    "SELECT status, pause_reason, finished_at FROM tag_runs WHERE id = ?",
                )
                .bind(run)
                .fetch_one(master)
                .await
                .unwrap();
            assert_eq!((status.as_str(), reason), ("done", None), "{run}");
            assert!(finished_at.is_some(), "{run}");
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(s.provider.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failed_finish_check_on_resume_still_wakes_the_scheduler() {
        let s = setup(Recording::default(), &["a1", "a2"]).await;
        let master = &s.engine.db.master_pool;
        insert_paused_run(master, "run", "recording", "a1").await;
        sqlx::query(
            "INSERT INTO tag_run_items (run_id, asset_id, library_id) VALUES ('run', 'a2', 'lib')",
        )
        .execute(master)
        .await
        .unwrap();
        s.engine.start().await;
        s.engine.fail_finishing.store(true, Ordering::SeqCst);

        s.engine.resume_run("run").await.unwrap();
        let runs = wait_until_done(&s.engine).await;

        assert_eq!((runs[0].done, runs[0].failed), (2, 0));
        assert_eq!(s.provider.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_restart_pauses_unfinished_runs_and_resume_finishes_them() {
        let assets = ["a1", "a2", "a3", "a4"];
        let lib_pool = memory_library_pool().await;
        for id in assets {
            insert_asset(&lib_pool, id).await;
        }
        let db = Arc::new(memory_database(vec![memory_library("lib", lib_pool.clone())]).await);
        insert_tag(&db.master_pool, "t-rock", "Rock").await;
        settings::update(
            &db.master_pool,
            serde_json::json!({"anthropic": {"concurrency": 2}}),
        )
        .await
        .unwrap();

        // The first session gets two requests in flight, then is killed: they never return.
        let (first, _, _, _) = engine_on(
            db.clone(),
            Recording {
                hang: true,
                ..Default::default()
            },
        );
        first.start().await;
        let started = first
            .start_run(scope(&assets), "recording", Mode::ExistingOnly)
            .await
            .unwrap();
        wait_for(&first, |runs| runs[0].running == 2).await;
        sqlx::query(
            "INSERT INTO tag_runs (id, label, provider, mode, status, created_at)
             VALUES ('half-made', 'run', 'recording', 'existing_only', 'queued', 0)",
        )
        .execute(&db.master_pool)
        .await
        .unwrap();

        // The next session.
        let (engine, provider, updates, _) = engine_on(
            db.clone(),
            Recording {
                existing: ids(&["t-rock"]),
                ..Default::default()
            },
        );
        engine.start().await;
        let runs = engine.list_runs().await.unwrap();

        for run in &runs {
            assert_eq!(run.status, "paused", "{run:?}");
        }
        let run = runs.iter().find(|r| r.id == started.run.id).unwrap();
        assert_eq!(run.pause_reason.as_deref(), Some(store::APP_RESTARTED));
        let half_made = runs.iter().find(|r| r.id == "half-made").unwrap();
        assert_eq!(
            half_made.pause_reason.as_deref(),
            Some(
                "app restarted while the run was starting: it holds only 0 assets, \
                 and any others were never added"
            ),
            "a run cut off while its items went in says it may be incomplete"
        );
        assert_eq!((run.running, run.queued), (0, 4));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(provider.calls.lock().unwrap().is_empty());
        for _ in 0..100 {
            if !updates.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let first_update = updates.lock().unwrap()[0].clone();
        assert!(
            first_update.runs.iter().all(|r| r.status == "paused"),
            "the UI hears about the paused runs"
        );

        engine.resume_run(&started.run.id).await.unwrap();
        let runs = wait_for(&engine, |runs| {
            runs.iter()
                .any(|r| r.id == started.run.id && r.status == "done")
        })
        .await;

        let run = runs.iter().find(|r| r.id == started.run.id).unwrap();
        assert_eq!((run.done, run.failed), (4, 0));
        assert_eq!(provider.calls.lock().unwrap().len(), 4);
        assert_eq!(count(&lib_pool, "SELECT COUNT(*) FROM asset_tags").await, 4);
    }

    #[tokio::test]
    async fn a_failed_library_write_waits_for_replay_and_the_model_is_not_asked_again() {
        let s = setup(
            Recording {
                existing: ids(&["t-rock"]),
                ..Default::default()
            },
            &["a1"],
        )
        .await;
        let master = &s.engine.db.master_pool;
        insert_tag(master, "t-rock", "Rock").await;
        s.engine.fail_library_writes.store(true, Ordering::SeqCst);
        s.engine.start().await;

        s.engine
            .start_run(scope(&["a1"]), "recording", Mode::ExistingOnly)
            .await
            .unwrap();
        let runs = wait_until_done(&s.engine).await;

        assert_eq!((runs[0].done, runs[0].failed), (1, 0), "the item is done");
        assert_eq!(
            count(&s.lib_pool, "SELECT COUNT(*) FROM asset_tags").await,
            0
        );
        assert_eq!(
            count(
                master,
                "SELECT COUNT(*) FROM tag_run_applied WHERE state = 'pending'"
            )
            .await,
            1
        );

        // A restart, then the library opening, replay it.
        s.engine.fail_library_writes.store(false, Ordering::SeqCst);
        store::recover_after_restart(master).await.unwrap();
        let waiting = s.engine.waiting_writes("lib").await.unwrap();
        s.engine.library_opened(waiting).await;

        let tags: Vec<(String, String)> = sqlx::query_as("SELECT asset_id, tag_id FROM asset_tags")
            .fetch_all(&s.lib_pool)
            .await
            .unwrap();
        assert_eq!(tags, [("a1".into(), "t-rock".into())]);
        assert_eq!(
            count(
                master,
                "SELECT COUNT(*) FROM tag_run_applied WHERE state = 'applied'"
            )
            .await,
            1
        );
        assert_eq!(item(master, "a1").await.0, "done");
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(s.provider.calls.lock().unwrap().len(), 1);
        tokio::time::sleep(EVENT_INTERVAL * 2).await;
        assert!(s
            .updates
            .lock()
            .unwrap()
            .iter()
            .any(|u| u.changed_asset_ids == ["a1"]));
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
            .start_run(
                RunScope::Assets {
                    asset_ids: assets.clone(),
                },
                "recording",
                Mode::ExistingOnly,
            )
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
    fn the_run_scope_reads_the_shape_the_frontend_sends() {
        let assets: RunScope =
            serde_json::from_str(r#"{"kind":"assets","assetIds":["a1"]}"#).unwrap();
        let library: RunScope =
            serde_json::from_str(r#"{"kind":"library","libraryId":"lib","allAssets":true}"#)
                .unwrap();

        assert!(matches!(assets, RunScope::Assets { asset_ids } if asset_ids == ["a1"]));
        assert!(matches!(
            library,
            RunScope::Library { library_id, all_assets: true } if library_id == "lib"
        ));
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
