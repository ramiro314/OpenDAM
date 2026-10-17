//! Anthropic with Claude-plan access: each asset is one headless run of your installed `claude`
//! CLI (docs/plans/ai-auto-tagging/plan.md, "Providers"). The app makes no HTTPS call of its
//! own on this path: `claude` talks to Anthropic, on your Claude plan.
//!
//! **Personal use only.** Anthropic doesn't let third-party products offer claude.ai login or
//! Claude plan limits. This app never handles the login: it only runs the owner's own `claude`,
//! which the owner logged in. Remove or disable this access mode before the app is ever
//! distributed.
//!
//! - **No API key is ever billed.** The child starts from an empty environment and gets only
//!   `CHILD_ENV`, so an `ANTHROPIC_API_KEY` or any other auth override set for the app never
//!   reaches it. `init` arrives only after the asset is sent, so its check is a second guard:
//!   an `apiKeySource` other than `none` kills the child and pauses the run.
//! - **Nothing large in argv.** Linux caps one argument at 128 KiB, and a catalog's vocabulary
//!   can be far bigger. The instructions and vocabulary go in the run's `system.txt`
//!   (`--system-prompt-file`), and the asset goes in on stdin. When the catalog changes, the
//!   run's one writer writes a temp file and renames it over `system.txt`, so a child reads one
//!   vocabulary in full, never a partial file.
//! - **Failures** are classified from the assistant `error` token, the `result` line and the
//!   exit, never from `subtype` alone (the plan's Failures table).

use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::anthropic::{self, ConnectionTest};
use super::provider::{ErrorClass, ProviderError, TagRequest, TagResult, Usage};

/// The only variables a `claude` child gets, each one only when it is set for the app.
pub const CHILD_ENV: [&str; 6] = [
    "HOME",
    "PATH",
    "USER",
    "LANG",
    "TMPDIR",
    "CLAUDE_CONFIG_DIR",
];

/// Variables that would make `claude` use something other than its own claude.ai login. None
/// is in `CHILD_ENV`: Settings names the ones set for the app, to show they're ignored.
pub const AUTH_OVERRIDES: [&str; 8] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_PROFILE",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// One headless run, from spawn to exit. A child still running then is killed and retried.
const RUN_TIMEOUT: Duration = Duration::from_secs(120);
/// `claude --version` and `claude auth status --json`.
const STATUS_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a child may take to exit once its output has ended.
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long stderr may stay open once the child is gone: a descendant of `claude` that inherited
/// it would otherwise hold the item, and its run, forever.
const STDERR_TIMEOUT: Duration = Duration::from_secs(2);
/// The run id, and run directory, of "Test connection".
const TEST_RUN: &str = "connection-test";
/// The CLI's own login, the only one a Claude-plan run may use.
const CLAUDE_AI: &str = "claude.ai";

type EnvLookup = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// What Settings shows about Claude-plan access. Nothing in it is secret.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudePlanStatus {
    /// Where `claude` was found on PATH.
    pub path: Option<String>,
    /// `claude --version`, such as "2.1.296 (Claude Code)".
    pub version: Option<String>,
    pub logged_in: bool,
    /// "claude.ai" when runs use your Claude plan.
    pub auth_method: Option<String>,
    /// The plan, such as "max" or "pro".
    pub subscription_type: Option<String>,
    /// Claude-plan runs can start.
    pub configured: bool,
    /// Why they can't, when they can't.
    pub problem: Option<String>,
    /// Auth overrides set for the app that `claude` never gets, such as `ANTHROPIC_API_KEY`.
    pub ignored_overrides: Vec<&'static str>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthStatus {
    #[serde(default)]
    logged_in: bool,
    #[serde(default)]
    auth_method: Option<String>,
    #[serde(default)]
    subscription_type: Option<String>,
}

/// One run's `system.txt`.
#[derive(Default)]
struct RunFile {
    /// The run's one writer, held from the temp write to the rename.
    writer: tokio::sync::Mutex<()>,
    /// The hash of the vocabulary `system.txt` holds now.
    current: Mutex<Option<u64>>,
}

pub struct ClaudeCli {
    /// The app data dir: every child's working directory, and home of `autotag/<run id>/`.
    data_dir: PathBuf,
    /// The app's environment, which children see only through `CHILD_ENV`.
    env: EnvLookup,
    runs: Mutex<HashMap<String, Arc<RunFile>>>,
    /// Test-only: the next vocabulary rewrite waits between its temp write and its rename.
    #[cfg(test)]
    hold_rename: Mutex<Option<Arc<RenameHold>>>,
}

impl ClaudeCli {
    /// Runs `claude` found on the app's PATH, with `data_dir` as its working directory.
    pub fn from_process_env(data_dir: PathBuf) -> Self {
        Self::with_env(data_dir, Box::new(|name| std::env::var(name).ok()))
    }

    fn with_env(data_dir: PathBuf, env: EnvLookup) -> Self {
        Self {
            data_dir,
            env,
            runs: Mutex::default(),
            #[cfg(test)]
            hold_rename: Mutex::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn new(
        data_dir: PathBuf,
        env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        Self::with_env(data_dir, Box::new(env))
    }

    /// A CLI that never finds `claude`, for tests of the API-key path.
    #[cfg(test)]
    pub(crate) fn unused() -> Self {
        Self::new(PathBuf::from("/nonexistent"), |_| None)
    }

    fn child_env(&self) -> Vec<(&'static str, String)> {
        CHILD_ENV
            .into_iter()
            .filter_map(|name| (self.env)(name).map(|value| (name, value)))
            .collect()
    }

    /// `claude` on the app's PATH.
    fn program(&self) -> Option<PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        let path = (self.env)("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join("claude"))
            .find(|candidate| {
                std::fs::metadata(candidate)
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
    }

    /// Runs `claude <args>` from the cleaned environment and returns its stdout, whatever its
    /// exit: `auth status` exits 1 when logged out. Blocks.
    fn output_blocking(&self, program: &Path, args: &[&str]) -> Result<String, String> {
        use std::io::Read;
        std::fs::create_dir_all(&self.data_dir).map_err(|e| e.to_string())?;
        let mut child = std::process::Command::new(program)
            .args(args)
            .env_clear()
            .envs(self.child_env())
            .current_dir(&self.data_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("could not start it: {e}"))?;
        // Its output is a few hundred bytes, far below what a pipe holds before it blocks
        let deadline = Instant::now() + STATUS_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "claude {} didn't answer in {} s",
                        args.join(" "),
                        STATUS_TIMEOUT.as_secs()
                    ));
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        let mut out = String::new();
        if let Some(mut stdout) = child.stdout.take() {
            stdout.read_to_string(&mut out).map_err(|e| e.to_string())?;
        }
        Ok(out)
    }

    /// Whether Claude-plan runs can start: `claude` found, its version, and its login, read
    /// from the same cleaned environment runs use. Only the claude.ai login counts. Blocks on
    /// two subprocesses: call it off async workers.
    pub fn status(&self) -> ClaudePlanStatus {
        let mut status = ClaudePlanStatus {
            ignored_overrides: AUTH_OVERRIDES
                .into_iter()
                .filter(|name| (self.env)(name).is_some_and(|v| !v.is_empty()))
                .collect(),
            ..Default::default()
        };
        let Some(program) = self.program() else {
            status.problem = Some(
                "claude isn't on PATH: install Claude Code and log in to use your Claude plan"
                    .into(),
            );
            return status;
        };
        status.path = Some(program.display().to_string());
        status.version = self
            .output_blocking(&program, &["--version"])
            .ok()
            .and_then(|out| out.lines().next().map(|l| l.trim().to_string()))
            .filter(|v| !v.is_empty());
        let auth = self
            .output_blocking(&program, &["auth", "status", "--json"])
            .and_then(|out| {
                serde_json::from_str::<AuthStatus>(&out)
                    .map_err(|e| format!("its output isn't the expected JSON: {e}"))
            });
        status.problem = match auth {
            Err(e) => Some(format!("Could not read claude's login: {e}")),
            Ok(auth) => {
                status.logged_in = auth.logged_in;
                status.auth_method = auth.auth_method;
                status.subscription_type = auth.subscription_type;
                if !status.logged_in {
                    Some("claude is logged out: run claude in a terminal and log in".into())
                } else if status.auth_method.as_deref() != Some(CLAUDE_AI) {
                    Some(format!(
                        "claude is logged in with {}, not with your Claude plan (claude.ai)",
                        status.auth_method.as_deref().unwrap_or("an unknown method")
                    ))
                } else {
                    None
                }
            }
        };
        status.configured = status.problem.is_none();
        status
    }

    fn run_dir(&self, run_id: &str) -> Result<PathBuf, String> {
        // Run ids are UUIDs: nothing else may name a directory under the app data dir
        if run_id.is_empty()
            || !run_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(format!("Unexpected run id {run_id:?}"));
        }
        Ok(self.data_dir.join("autotag").join(run_id))
    }

    /// The run's `system.txt`, holding `text` or a vocabulary renamed into place after it. A
    /// new `text` is written to a temp file in the same directory and renamed over
    /// `system.txt`, under the run's one writer. A child that opened the old file keeps reading
    /// it intact, and one started meanwhile reads the old file or the new one, in full.
    async fn system_file(&self, run_id: &str, text: &str) -> Result<PathBuf, String> {
        let dir = self.run_dir(run_id)?;
        let path = dir.join("system.txt");
        let hash = {
            let mut hasher = DefaultHasher::new();
            text.hash(&mut hasher);
            hasher.finish()
        };
        let run = self
            .runs
            .lock()
            .unwrap()
            .entry(run_id.to_string())
            .or_default()
            .clone();
        let is_current = || *run.current.lock().unwrap() == Some(hash) && path.is_file();
        if is_current() {
            return Ok(path);
        }
        let _writer = run.writer.lock().await;
        if is_current() {
            return Ok(path);
        }
        let write = async {
            tokio::fs::create_dir_all(&dir).await?;
            let temp = dir.join(format!("system.{}.tmp", uuid::Uuid::new_v4()));
            let mut written = tokio::fs::write(&temp, text).await;
            if written.is_ok() {
                #[cfg(test)]
                self.wait_before_rename().await;
                written = tokio::fs::rename(&temp, &path).await;
            }
            if written.is_err() {
                let _ = tokio::fs::remove_file(&temp).await;
            }
            written
        };
        write
            .await
            .map_err(|e| format!("Could not write the run's vocabulary file: {e}"))?;
        *run.current.lock().unwrap() = Some(hash);
        Ok(path)
    }

    /// Deletes the vocabulary file of every run in `ended`. Only those: a run missing from it
    /// may have been created since it was read, and be writing its first file.
    pub fn forget_runs(&self, ended: &HashSet<String>) {
        self.runs
            .lock()
            .unwrap()
            .retain(|id, _| !ended.contains(id));
        let Ok(entries) = std::fs::read_dir(self.data_dir.join("autotag")) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !ended.contains(&name) || !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                tracing::warn!("Could not delete the vocabulary file of run {name}: {e}");
            }
        }
    }

    fn forget_run(&self, run_id: &str) {
        self.runs.lock().unwrap().remove(run_id);
        if let Ok(dir) = self.run_dir(run_id) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// Tags one asset with one headless `claude` run on `model`.
    pub async fn tag(&self, request: &TagRequest, model: &str) -> Result<TagResult, ProviderError> {
        let fatal = |message: String| ProviderError::new(ErrorClass::Fatal, message);
        let program = self.program().ok_or_else(|| {
            fatal("claude isn't on PATH: install Claude Code and log in, then resume".into())
        })?;
        let system_file = self
            .system_file(&request.run_id, &anthropic::system_text(request))
            .await
            .map_err(fatal)?;
        // A few hundred bytes: the schema never lists tag ids
        let schema = anthropic::answer_schema(request.mode).to_string();
        let mut child = tokio::process::Command::new(&program)
            .args(["-p", "--input-format", "stream-json"])
            .args(["--output-format", "stream-json", "--verbose"])
            .arg("--json-schema")
            .arg(&schema)
            .arg("--model")
            .arg(model)
            .args(["--tools", "", "--safe-mode", "--no-session-persistence"])
            .arg("--system-prompt-file")
            .arg(&system_file)
            .env_clear()
            .envs(self.child_env())
            .current_dir(&self.data_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| fatal(format!("Could not start claude: {e}")))?;

        let message = json!({
            "type": "user",
            "message": { "role": "user", "content": anthropic::user_content(request) },
        })
        .to_string()
            + "\n";
        let mut stdin = child.stdin.take().expect("stdin is piped");
        // Written alongside the reads below; dropping stdin closes it, which ends the input
        let feed = tokio::spawn(async move { stdin.write_all(message.as_bytes()).await });
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let mut errors = tokio::spawn(async move {
            let mut head = Vec::new();
            let _ = (&mut stderr).take(16 * 1024).read_to_end(&mut head).await;
            let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
            String::from_utf8_lossy(&head).into_owned()
        });
        let stdout = child.stdout.take().expect("stdout is piped");

        let mut stream = Stream::default();
        let read = async {
            let mut lines = BufReader::new(stdout).lines();
            while let Some(line) = lines.next_line().await? {
                if let Some(stop) = stream.read(&line) {
                    return Ok(Some(stop));
                }
            }
            Ok::<_, std::io::Error>(None)
        };
        let stopped = match tokio::time::timeout(RUN_TIMEOUT, read).await {
            Err(_) => Some(ProviderError::new(
                ErrorClass::Retryable,
                format!("claude didn't finish in {} s", RUN_TIMEOUT.as_secs()),
            )),
            Ok(Err(e)) => Some(ProviderError::new(
                ErrorClass::Item,
                format!("Could not read claude's output: {e}"),
            )),
            Ok(Ok(stop)) => stop,
        };
        if stopped.is_some() {
            let _ = child.start_kill();
        }
        let exit = match tokio::time::timeout(EXIT_TIMEOUT, child.wait()).await {
            Ok(Ok(status)) => status.code(),
            _ => {
                let _ = child.start_kill();
                None
            }
        };
        feed.abort();
        let stderr = match tokio::time::timeout(STDERR_TIMEOUT, &mut errors).await {
            Ok(Ok(text)) => text,
            _ => {
                errors.abort();
                String::new()
            }
        };
        if !stderr.trim().is_empty() {
            tracing::debug!("claude stderr: {}", stderr.trim());
        }
        if let Some(error) = stopped {
            return Err(error);
        }
        stream.outcome(request, model, exit, &stderr)
    }

    /// Sends the "Test connection" request through `claude`, on your Claude plan.
    pub async fn test_connection(&self, model: &str) -> ConnectionTest {
        let request = anthropic::test_request(model);
        debug_assert_eq!(request.run_id, TEST_RUN);
        let outcome = self.tag(&request, model).await;
        self.forget_run(TEST_RUN);
        ConnectionTest::of(model, &request, outcome, true)
    }

    /// Test-only: the next rewrite of a vocabulary file waits between its temp write and its
    /// rename until the returned hold is released.
    #[cfg(test)]
    pub(crate) fn hold_next_rename(&self) -> Arc<RenameHold> {
        let hold = Arc::new(RenameHold {
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        });
        *self.hold_rename.lock().unwrap() = Some(hold.clone());
        hold
    }

    #[cfg(test)]
    async fn wait_before_rename(&self) {
        let hold = self.hold_rename.lock().unwrap().take();
        if let Some(hold) = hold {
            hold.reached.notify_one();
            let _ = hold.release.acquire().await;
        }
    }
}

#[cfg(test)]
pub(crate) struct RenameHold {
    /// Notified once the rewrite has written its temp file.
    pub reached: tokio::sync::Notify,
    /// The rename goes ahead once this has a permit.
    pub release: tokio::sync::Semaphore,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitInfo {
    /// `allowed`, `allowed_warning` or `rejected`.
    status: String,
    /// Unix seconds.
    #[serde(default)]
    resets_at: Option<i64>,
    /// 0-1.
    #[serde(default)]
    utilization: Option<f64>,
    /// Which limit: `five_hour`, `seven_day`, ...
    #[serde(default)]
    rate_limit_type: Option<String>,
    /// `credits_required` once the subscription's usage is exhausted.
    #[serde(default)]
    error_code: Option<String>,
}

/// "14:00" today, else "Sat 14:00", in local time.
fn reset_time(unix: i64) -> Option<String> {
    let at = Local.timestamp_opt(unix, 0).single()?;
    let format = if at.date_naive() == Local::now().date_naive() {
        "%H:%M"
    } else {
        "%a %H:%M"
    };
    Some(at.format(format).to_string())
}

impl RateLimitInfo {
    /// The pause reason of a `rejected` limit.
    fn limit_reached(&self) -> String {
        if self.error_code.as_deref() == Some("credits_required") {
            return "Claude plan usage exhausted".into();
        }
        match self.resets_at.and_then(reset_time) {
            Some(at) => format!("Claude plan limit reached, resets at {at}"),
            None => "Claude plan limit reached".into(),
        }
    }

    /// What the Runs panel shows for an `allowed_warning`, and nothing for any other status.
    fn warning(&self) -> Option<String> {
        if self.status != "allowed_warning" {
            return None;
        }
        let limit = match &self.rate_limit_type {
            Some(kind) => format!("the {} limit", kind.replace('_', "-")),
            None => "a limit".into(),
        };
        let mut text = match self.utilization {
            Some(used) => {
                let percent = if used <= 1.0 { used * 100.0 } else { used };
                format!("Claude plan: {percent:.0}% of {limit} used")
            }
            None => format!("Claude plan: close to {limit}"),
        };
        if let Some(at) = self.resets_at.and_then(reset_time) {
            text.push_str(&format!(", resets at {at}"));
        }
        Some(text)
    }
}

#[derive(Debug, Default, Deserialize)]
struct CliUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    cache_creation_input_tokens: i64,
    #[serde(default)]
    cache_read_input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
}

#[derive(Debug, Deserialize)]
struct ResultLine {
    #[serde(default)]
    subtype: String,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    api_error_status: Option<u16>,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    errors: Option<Vec<String>>,
    #[serde(default)]
    structured_output: Option<Value>,
    #[serde(default)]
    usage: Option<CliUsage>,
}

/// What a run's stdout said so far.
#[derive(Debug, Default)]
struct Stream {
    rate_limit: Option<RateLimitInfo>,
    /// The assistant `error` token, and the text that came with it.
    error: Option<(String, String)>,
    result: Option<ResultLine>,
}

/// The text blocks of an assistant message.
fn message_text(message: &Value) -> String {
    message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The plan's Failures class for an assistant `error` token, with a readable message.
fn token_error(token: &str, text: &str, model: &str) -> ProviderError {
    let class = match token {
        "rate_limit" | "overloaded" | "server_error" => ErrorClass::Retryable,
        "authentication_failed"
        | "oauth_org_not_allowed"
        | "account_on_hold"
        | "billing_error"
        | "model_not_found"
        | "cloud_credential_error" => ErrorClass::Fatal,
        // invalid_request, max_output_tokens, unknown, and any token this build doesn't know
        _ => ErrorClass::Item,
    };
    let text = if text.trim().is_empty() {
        token
    } else {
        text.trim()
    };
    let message = match token {
        "authentication_failed" => {
            format!("claude can't use your Claude plan: {text}. Log in with claude, then resume")
        }
        "model_not_found" => {
            format!("Unknown model {model}, or your Claude plan has no access to it")
        }
        _ => format!("claude: {text} ({token})"),
    };
    ProviderError::new(class, message)
}

impl Stream {
    /// Takes one stdout line in. Returns why the child must stop now, when it must.
    fn read(&mut self, line: &str) -> Option<ProviderError> {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return None;
        };
        match value["type"].as_str() {
            Some("system") if value["subtype"] == "init" => {
                // A missing source counts as one: the guard fails closed
                let source = value["apiKeySource"].as_str().unwrap_or("unknown");
                if source != "none" {
                    return Some(ProviderError::new(
                        ErrorClass::Fatal,
                        format!("Claude plan run would bill an API key ({source})"),
                    ));
                }
            }
            Some("rate_limit_event") => {
                let info: RateLimitInfo =
                    serde_json::from_value(value["rate_limit_info"].clone()).ok()?;
                if info.status == "rejected" {
                    return Some(ProviderError::new(ErrorClass::Fatal, info.limit_reached()));
                }
                self.rate_limit = Some(info);
            }
            Some("assistant") => {
                if let Some(token) = value["error"].as_str() {
                    self.error = Some((token.to_string(), message_text(&value["message"])));
                }
            }
            Some("result") => self.result = serde_json::from_value(value).ok(),
            _ => {}
        }
        None
    }

    /// The item's outcome once the child exited with `exit`.
    fn outcome(
        self,
        request: &TagRequest,
        model: &str,
        exit: Option<i32>,
        stderr: &str,
    ) -> Result<TagResult, ProviderError> {
        if let Some((token, text)) = &self.error {
            return Err(token_error(token, text, model));
        }
        let Some(result) = self.result else {
            let exit = exit.map_or("killed".to_string(), |code| format!("exit {code}"));
            let detail = stderr.lines().rev().find(|l| !l.trim().is_empty());
            return Err(ProviderError::new(
                ErrorClass::Item,
                match detail {
                    Some(detail) => format!("claude ended without a result ({exit}): {detail}"),
                    None => format!("claude ended without a result ({exit})"),
                },
            ));
        };
        if result.is_error || result.subtype.starts_with("error") {
            let class = match result.api_error_status {
                Some(429 | 529 | 500..=599) => ErrorClass::Retryable,
                Some(401 | 403) => ErrorClass::Fatal,
                _ => ErrorClass::Item,
            };
            let detail = match result.errors.filter(|e| !e.is_empty()) {
                Some(errors) => errors.join("; "),
                None => result.result.unwrap_or_default(),
            };
            let what = match result.api_error_status {
                _ if result.subtype.starts_with("error") => result.subtype,
                Some(status) => format!("API error {status}"),
                None => "error".to_string(),
            };
            return Err(ProviderError::new(
                class,
                format!("claude: {detail} ({what})"),
            ));
        }
        let Some(answer) = result.structured_output else {
            return Err(ProviderError::new(
                ErrorClass::Item,
                "claude returned no structured output",
            ));
        };
        let usage = result.usage.unwrap_or_default();
        Ok(TagResult {
            usage: Usage {
                input_tokens: usage.input_tokens
                    + usage.cache_creation_input_tokens
                    + usage.cache_read_input_tokens,
                output_tokens: usage.output_tokens,
                // On your plan: no cost of its own
                cost_usd: 0.0,
            },
            notice: self.rate_limit.as_ref().and_then(RateLimitInfo::warning),
            ..anthropic::read_answer(&answer.to_string(), request.mode, request.max_proposals)?
        })
    }
}

/// The plan's stub-`claude` tests: a shell script on a test `PATH` replays a recorded or
/// synthesized fixture and records its argv, environment, stdin and vocabulary file.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::autotag::anthropic::{mock, AnthropicProvider};
    use crate::autotag::credentials::{Credentials, MemoryKeyring};
    use crate::autotag::engine::{Engine, RunScope};
    use crate::autotag::provider::{AssetInfo, Mode, TagProvider, Thumbnail};
    use crate::autotag::settings;
    use crate::autotag::store::RunSummary;
    use crate::autotag::vocabulary::Vocabulary;
    use crate::db::TagRow;
    use crate::db::{
        insert_asset, insert_tag, memory_database, memory_library, memory_library_pool,
    };
    use sqlx::{Pool, Sqlite};

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/autotag/fixtures/claude")
            .join(name)
    }

    /// What one run of the stub saw.
    #[derive(Debug)]
    struct Call {
        args: Vec<String>,
        env: Vec<(String, String)>,
        stdin: String,
        vocabulary: Option<String>,
    }

    struct Stub {
        dir: tempfile::TempDir,
    }

    impl Stub {
        fn bin(&self) -> PathBuf {
            self.dir.path().join("bin")
        }

        fn log(&self) -> PathBuf {
            self.dir.path().join("log")
        }

        fn data_dir(&self) -> PathBuf {
            self.dir.path().join("data")
        }

        /// The stub prints `run` (a fixture name, or "" for no output) for headless runs, and
        /// `auth` for `claude auth status --json`. Runs sleep `sleep` seconds first.
        fn new(run: &str, auth: &str, sleep: f32) -> Self {
            let stub = Stub {
                dir: tempfile::tempdir().unwrap(),
            };
            std::fs::create_dir_all(stub.bin()).unwrap();
            std::fs::create_dir_all(stub.log()).unwrap();
            let print = if run.is_empty() {
                String::new()
            } else {
                format!("cat '{}'", fixture(run).display())
            };
            let script = format!(
                r#"#!/bin/sh
log='{log}'
case "$1" in
  --version) echo '2.1.296 (Claude Code)'; exit 0 ;;
  auth) env > "$log/auth-env.$$"; cat '{auth}'; exit 0 ;;
esac
printf '%s\0' "$@" > "$log/args.$$"
env > "$log/env.$$"
prev=
for arg in "$@"; do
  if [ "$prev" = --system-prompt-file ]; then cat "$arg" > "$log/vocab.$$"; fi
  prev=$arg
done
cat > "$log/stdin.$$"
sleep {sleep}
{print}
exit 1
"#,
                log = stub.log().display(),
                auth = fixture(auth).display(),
            );
            let path = stub.bin().join("claude");
            std::fs::write(&path, script).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            stub
        }

        /// A CLI that finds the stub on its PATH, with `extra` set for the app too.
        fn cli(&self, extra: &[(&str, &str)]) -> ClaudeCli {
            let mut env: HashMap<String, String> = [
                ("PATH", format!("{}:/usr/bin:/bin", self.bin().display())),
                ("HOME", self.dir.path().display().to_string()),
                ("USER", "tester".into()),
                ("LANG", "C.UTF-8".into()),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
            for (k, v) in extra {
                env.insert(k.to_string(), v.to_string());
            }
            ClaudeCli::new(self.data_dir(), move |name| env.get(name).cloned())
        }

        fn read(&self, prefix: &str) -> Vec<(String, Vec<u8>)> {
            let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(self.log())
                .unwrap()
                .flatten()
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let pid = name.strip_prefix(prefix)?.to_string();
                    Some((pid, std::fs::read(entry.path()).unwrap()))
                })
                .collect();
            files.sort();
            files
        }

        fn calls(&self) -> Vec<Call> {
            let text = |bytes: &[u8]| String::from_utf8(bytes.to_vec()).unwrap();
            let vocab: HashMap<String, Vec<u8>> = self.read("vocab.").into_iter().collect();
            let stdin: HashMap<String, Vec<u8>> = self.read("stdin.").into_iter().collect();
            let env: HashMap<String, Vec<u8>> = self.read("env.").into_iter().collect();
            self.read("args.")
                .into_iter()
                .map(|(pid, args)| Call {
                    args: args
                        .split(|b| *b == 0)
                        .map(text)
                        .collect::<Vec<_>>()
                        .split_last()
                        .map(|(_, args)| args.to_vec())
                        .unwrap_or_default(),
                    env: env_vars(&text(&env[&pid])),
                    stdin: stdin.get(&pid).map(|b| text(b)).unwrap_or_default(),
                    vocabulary: vocab.get(&pid).map(|b| text(b)),
                })
                .collect()
        }

        fn auth_envs(&self) -> Vec<Vec<(String, String)>> {
            self.read("auth-env.")
                .into_iter()
                .map(|(_, env)| env_vars(&String::from_utf8(env).unwrap()))
                .collect()
        }
    }

    fn env_vars(text: &str) -> Vec<(String, String)> {
        text.lines()
            .filter_map(|line| line.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn row(id: &str, name: &str, parent: Option<&str>, hint: &str) -> TagRow {
        (
            id.into(),
            name.into(),
            None,
            parent.map(Into::into),
            hint.into(),
        )
    }

    fn vocabulary(tags: &[(&str, &str)]) -> Arc<Vocabulary> {
        Arc::new(Vocabulary::from_rows(
            &tags
                .iter()
                .map(|(id, name)| row(id, name, None, ""))
                .collect::<Vec<_>>(),
        ))
    }

    fn request(image: bool) -> TagRequest {
        TagRequest {
            run_id: "11111111-2222-3333-4444-555555555555".into(),
            mode: Mode::ExistingOnly,
            access: Some("claude_plan".into()),
            model: Some("claude-haiku-5-5".into()),
            max_tags: 8,
            max_proposals: 0,
            threshold: None,
            asset: AssetInfo {
                file_name: "red_circle_icon.png".into(),
                relative_path: "ui/red_circle_icon.png".into(),
                kind: "texture".into(),
                extension: "png".into(),
                metadata: json!({}),
            },
            image: image.then(|| Thumbnail {
                media_type: "image/png",
                data: vec![1, 2, 3],
            }),
            vocabulary: vocabulary(&[("t3", "Red"), ("t5", "Circle")]),
            run_proposals: Vec::new(),
        }
    }

    #[tokio::test]
    async fn success_with_text_runs_the_production_flags_and_reads_the_structured_output() {
        let stub = Stub::new("text_allowed.jsonl", "auth_logged_in.json", 0.0);
        let cli = stub.cli(&[]);
        let request = request(false);

        let result = cli.tag(&request, "claude-haiku-5-5").await.unwrap();

        assert_eq!(result.existing, ["t3", "t5"]);
        assert_eq!(
            result.usage,
            Usage {
                input_tokens: 2 + 1450,
                output_tokens: 242,
                cost_usd: 0.0
            },
            "tokens recorded, no cost of its own"
        );
        assert_eq!(result.notice, None, "a plain `allowed` limit shows nothing");
        let calls = stub.calls();
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        let system_file = stub
            .data_dir()
            .join("autotag/11111111-2222-3333-4444-555555555555/system.txt");
        let schema = anthropic::answer_schema(Mode::ExistingOnly).to_string();
        let expected: Vec<&str> = vec![
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--json-schema",
            &schema,
            "--model",
            "claude-haiku-5-5",
            "--tools",
            "",
            "--safe-mode",
            "--no-session-persistence",
            "--system-prompt-file",
            system_file.to_str().unwrap(),
        ];
        assert_eq!(call.args, expected);
        assert_eq!(
            call.vocabulary.as_deref(),
            Some(anthropic::system_text(&request).as_str())
        );
        let message: Value = serde_json::from_str(call.stdin.trim()).unwrap();
        assert_eq!(message["type"], "user");
        assert_eq!(message["message"]["role"], "user");
        assert_eq!(
            message["message"]["content"],
            json!(anthropic::user_content(&request))
        );
    }

    #[tokio::test]
    async fn success_with_an_image_sends_the_image_block_and_shows_a_plan_warning() {
        let stub = Stub::new("image.jsonl", "auth_logged_in.json", 0.0);

        let result = stub
            .cli(&[])
            .tag(&request(true), "claude-haiku-5-5")
            .await
            .unwrap();

        assert_eq!(result.existing, ["t3", "t5"]);
        let notice = result.notice.unwrap();
        assert!(
            notice.starts_with("Claude plan: 76% of the seven-day limit used, resets at "),
            "{notice}"
        );
        let message: Value = serde_json::from_str(stub.calls()[0].stdin.trim()).unwrap();
        let content = &message["message"]["content"];
        assert_eq!(
            content[0],
            json!({
                "type": "image",
                "source": { "type": "base64", "media_type": "image/png", "data": "AQID" },
            })
        );
        assert_eq!(content[1]["type"], "text");
    }

    #[tokio::test]
    async fn each_failure_fixture_falls_in_its_plan_class() {
        let cases = [
            (
                "model_not_found.jsonl",
                ErrorClass::Fatal,
                "Unknown model claude-haiku-5-5",
            ),
            (
                "logged_out.jsonl",
                ErrorClass::Fatal,
                "Not logged in · Please run /login",
            ),
            (
                "authentication_failed.jsonl",
                ErrorClass::Fatal,
                "OAuth token revoked",
            ),
            (
                "inherited_api_key.jsonl",
                ErrorClass::Fatal,
                "Claude plan run would bill an API key (ANTHROPIC_API_KEY)",
            ),
            (
                "rate_limit_rejected.jsonl",
                ErrorClass::Fatal,
                "Claude plan limit reached, resets at ",
            ),
            (
                "rate_limit_credits_required.jsonl",
                ErrorClass::Fatal,
                "Claude plan usage exhausted",
            ),
            ("rate_limit.jsonl", ErrorClass::Retryable, "(rate_limit)"),
            ("overloaded.jsonl", ErrorClass::Retryable, "(overloaded)"),
            (
                "error_max_structured_output_retries.jsonl",
                ErrorClass::Item,
                "claude: Failed to provide valid structured output after 5 attempts \
                 (error_max_structured_output_retries)",
            ),
            (
                "",
                ErrorClass::Item,
                "claude ended without a result (exit 1)",
            ),
        ];
        for (name, class, message) in cases {
            let stub = Stub::new(name, "auth_logged_in.json", 0.0);

            let error = stub
                .cli(&[])
                .tag(&request(false), "claude-haiku-5-5")
                .await
                .unwrap_err();

            assert_eq!(error.class, class, "{name}: {}", error.message);
            assert!(error.message.contains(message), "{name}: {}", error.message);
        }
    }

    #[test]
    fn a_result_is_never_read_from_its_subtype_alone() {
        let mut stream = Stream::default();
        for line in std::fs::read_to_string(fixture("model_not_found.jsonl"))
            .unwrap()
            .lines()
        {
            assert!(stream.read(line).is_none());
        }
        let result = stream.result.as_ref().unwrap();
        assert_eq!(
            (result.subtype.as_str(), result.is_error),
            ("success", true)
        );

        // Without the assistant's token, `is_error` and `api_error_status` still decide
        stream.error = None;
        let error = stream
            .outcome(&request(false), "claude-haiku-5-5", Some(1), "")
            .unwrap_err();
        assert_eq!(error.class, ErrorClass::Item);
        assert!(
            error.message.contains("(API error 404)"),
            "{}",
            error.message
        );
    }

    #[test]
    fn without_an_error_token_the_api_status_of_the_result_line_decides() {
        for (status, class) in [
            (401, ErrorClass::Fatal),
            (403, ErrorClass::Fatal),
            (429, ErrorClass::Retryable),
            (529, ErrorClass::Retryable),
            (503, ErrorClass::Retryable),
            (400, ErrorClass::Item),
        ] {
            let mut stream = Stream::default();
            let line = json!({
                "type": "result",
                "subtype": "success",
                "is_error": true,
                "api_error_status": status,
                "result": "API Error",
            });
            assert!(stream.read(&line.to_string()).is_none());

            let error = stream
                .outcome(&request(false), "claude-haiku-5-5", Some(1), "")
                .unwrap_err();

            assert_eq!(error.class, class, "{status}: {}", error.message);
            assert!(
                error.message.contains(&format!("(API error {status})")),
                "{status}: {}",
                error.message
            );
        }
    }

    #[test]
    fn an_init_line_without_an_api_key_source_stops_the_child() {
        let mut stream = Stream::default();

        let error = stream
            .read(r#"{"type":"system","subtype":"init","model":"claude-haiku-5-5"}"#)
            .unwrap();

        assert_eq!(error.class, ErrorClass::Fatal);
        assert_eq!(
            error.message,
            "Claude plan run would bill an API key (unknown)"
        );
        assert!(stream
            .read(r#"{"type":"system","subtype":"init","apiKeySource":"none"}"#)
            .is_none());
    }

    #[tokio::test]
    async fn no_auth_override_reaches_the_child_for_runs_or_for_auth_status() {
        let stub = Stub::new("text.jsonl", "auth_logged_in.json", 0.0);
        let overrides: Vec<(&str, &str)> = AUTH_OVERRIDES
            .iter()
            .map(|name| (*name, "sk-ant-SENTINEL-override"))
            .chain([("TMPDIR", "/tmp"), ("OPENDAM_AUTOTAG_FAKE", "1")])
            .collect();
        let cli = stub.cli(&overrides);

        cli.tag(&request(false), "claude-haiku-5-5").await.unwrap();
        let status = cli.status();

        assert!(status.configured, "{status:?}");
        assert_eq!(status.ignored_overrides, AUTH_OVERRIDES.to_vec());
        let mut envs: Vec<Vec<(String, String)>> =
            stub.calls().into_iter().map(|c| c.env).collect();
        envs.extend(stub.auth_envs());
        assert_eq!(envs.len(), 2, "one run and one auth status");
        // `sh` adds PWD, SHLVL and `_` itself
        let allowed: HashSet<&str> = CHILD_ENV.into_iter().chain(["PWD", "SHLVL", "_"]).collect();
        for env in envs {
            for (name, value) in &env {
                assert!(allowed.contains(name.as_str()), "{name} reached the child");
                assert!(!value.contains("SENTINEL"), "{name}={value}");
            }
            let names: HashSet<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
            for name in ["HOME", "PATH", "USER", "LANG", "TMPDIR"] {
                assert!(names.contains(name), "{name} is passed through");
            }
            assert!(!names.contains("CLAUDE_CONFIG_DIR"), "only when set");
        }
    }

    #[test]
    fn only_the_claude_ai_login_counts_as_configured() {
        let logged_in = Stub::new("", "auth_logged_in.json", 0.0);
        let status = logged_in.cli(&[]).status();
        assert_eq!(
            status,
            ClaudePlanStatus {
                path: Some(logged_in.bin().join("claude").display().to_string()),
                version: Some("2.1.296 (Claude Code)".into()),
                logged_in: true,
                auth_method: Some("claude.ai".into()),
                subscription_type: Some("max".into()),
                configured: true,
                problem: None,
                ignored_overrides: Vec::new(),
            }
        );

        let logged_out = Stub::new("", "auth_logged_out.json", 0.0).cli(&[]).status();
        assert!(!logged_out.configured);
        assert_eq!(
            logged_out.problem.as_deref(),
            Some("claude is logged out: run claude in a terminal and log in")
        );

        let api_key = Stub::new("", "auth_api_key.json", 0.0).cli(&[]).status();
        assert!(!api_key.configured && api_key.logged_in);
        assert_eq!(
            api_key.problem.as_deref(),
            Some("claude is logged in with api_key, not with your Claude plan (claude.ai)")
        );

        let missing = ClaudeCli::new(PathBuf::from("/nonexistent"), |name| {
            (name == "PATH").then(|| "/nonexistent/bin".to_string())
        })
        .status();
        assert!(!missing.configured && missing.path.is_none());
        assert!(missing.problem.unwrap().starts_with("claude isn't on PATH"));
    }

    fn big_vocabulary(tags: usize, hint: &str) -> Arc<Vocabulary> {
        let rows: Vec<TagRow> = (0..tags)
            .map(|i| {
                row(
                    &format!("tag-{i:05}"),
                    &format!("Tag {i}"),
                    None,
                    &format!("{hint} {i}"),
                )
            })
            .collect();
        Arc::new(Vocabulary::from_rows(&rows))
    }

    #[tokio::test]
    async fn a_descendant_holding_stderr_open_never_holds_the_item() {
        let stub = Stub::new("", "auth_logged_in.json", 0.0);
        // claude prints an API-key init, which stops the run, and leaves a helper behind that
        // inherited its stderr
        std::fs::write(
            stub.bin().join("claude"),
            format!(
                "#!/bin/sh\ncat > /dev/null\nsleep 30 &\ncat '{}'\nsleep 30\n",
                fixture("inherited_api_key.jsonl").display()
            ),
        )
        .unwrap();

        let started = Instant::now();
        let error = stub
            .cli(&[])
            .tag(&request(false), "claude-haiku-5-5")
            .await
            .unwrap_err();

        assert_eq!(error.class, ErrorClass::Fatal, "{}", error.message);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_vocabulary_over_128_kib_goes_through_the_file_and_no_argument_is_large() {
        let stub = Stub::new("text.jsonl", "auth_logged_in.json", 0.0);
        let mut request = request(false);
        request.vocabulary = big_vocabulary(5_000, "A hint long enough to matter for each tag");
        let text = anthropic::system_text(&request);
        assert!(text.len() > 128 * 1024, "{} bytes", text.len());

        stub.cli(&[])
            .tag(&request, "claude-haiku-5-5")
            .await
            .unwrap();

        let call = &stub.calls()[0];
        assert_eq!(
            call.vocabulary.as_deref(),
            Some(text.as_str()),
            "read in full"
        );
        for arg in &call.args {
            assert!(arg.len() <= 4096, "a {}-byte argument", arg.len());
        }
    }

    #[tokio::test]
    async fn a_catalog_change_mid_flight_never_yields_a_partial_vocabulary() {
        let stub = Stub::new("text.jsonl", "auth_logged_in.json", 0.3);
        let cli = Arc::new(stub.cli(&[]));
        let old = {
            let mut r = request(false);
            r.vocabulary = big_vocabulary(3_000, "Old hint");
            r
        };
        let new = {
            let mut r = request(false);
            r.vocabulary = big_vocabulary(3_001, "New hint");
            r
        };
        let (old_text, new_text) = (anthropic::system_text(&old), anthropic::system_text(&new));

        // Four items in flight on the old vocabulary
        let tag = |r: &TagRequest| {
            let (cli, r) = (cli.clone(), r.clone());
            tokio::spawn(async move { cli.tag(&r, "claude-haiku-5-5").await })
        };
        let in_flight: Vec<_> = (0..4).map(|_| tag(&old)).collect();
        tokio::time::sleep(Duration::from_millis(100)).await;
        // The catalog changes: the rewrite stops between its temp write and its rename
        let hold = cli.hold_next_rename();
        let rewrite = tag(&new);
        hold.reached.notified().await;
        let temp: Vec<_> = std::fs::read_dir(
            stub.data_dir()
                .join("autotag/11111111-2222-3333-4444-555555555555"),
        )
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
        assert_eq!(
            temp.len(),
            1,
            "the new vocabulary is written, not renamed yet"
        );
        // A child started now reads the previous vocabulary, in full
        tag(&old).await.unwrap().unwrap();
        hold.release.add_permits(1);
        rewrite.await.unwrap().unwrap();
        for item in in_flight {
            item.await.unwrap().unwrap();
        }
        // And one started after the rename reads the new one
        tag(&new).await.unwrap().unwrap();

        let vocabularies: Vec<String> = stub
            .calls()
            .into_iter()
            .map(|c| c.vocabulary.unwrap())
            .collect();
        assert_eq!(vocabularies.len(), 7);
        for vocabulary in &vocabularies {
            assert!(
                *vocabulary == old_text || *vocabulary == new_text,
                "a partial or mixed vocabulary of {} bytes",
                vocabulary.len()
            );
        }
        assert_eq!(vocabularies.iter().filter(|v| **v == old_text).count(), 5);
        assert_eq!(vocabularies.iter().filter(|v| **v == new_text).count(), 2);
    }

    #[tokio::test]
    async fn only_the_files_of_ended_runs_are_deleted() {
        let stub = Stub::new("text.jsonl", "auth_logged_in.json", 0.0);
        let cli = stub.cli(&[]);
        let mut other = request(false);
        other.run_id = "99999999-0000-0000-0000-000000000000".into();
        cli.tag(&request(false), "claude-haiku-5-5").await.unwrap();
        // `other` stands for a run created after the ended runs were listed
        cli.tag(&other, "claude-haiku-5-5").await.unwrap();

        cli.forget_runs(&HashSet::from([
            "11111111-2222-3333-4444-555555555555".to_string()
        ]));

        let autotag = stub.data_dir().join("autotag");
        assert!(!autotag
            .join("11111111-2222-3333-4444-555555555555")
            .exists());
        assert!(autotag.join(&other.run_id).join("system.txt").is_file());

        let test = cli.test_connection("claude-haiku-5-5").await;
        assert!(test.ok && test.on_plan, "{test:?}");
        assert_eq!(test.tags, Vec::<String>::new(), "the stub answers t3, t5");
        assert!(
            !autotag.join(TEST_RUN).exists(),
            "the test's file is deleted"
        );
    }

    // ── Through the engine ──────────────────────────────────────────────

    struct PlanRun {
        engine: Arc<Engine>,
        db: Arc<crate::db::Database>,
        stub: Stub,
        api: mock::MockApi,
        lib_pool: Pool<Sqlite>,
    }

    /// An engine whose Anthropic provider has Claude-plan access through a stub replaying
    /// `run`, with an API stand-in that would record any HTTPS request, and one asset.
    async fn plan_engine(run: &str, auth: &str) -> PlanRun {
        let stub = Stub::new(run, auth, 0.0);
        let lib_pool = memory_library_pool().await;
        insert_asset(&lib_pool, "a1").await;
        let db = Arc::new(memory_database(vec![memory_library("lib", lib_pool.clone())]).await);
        insert_tag(&db.master_pool, "t3", "Red").await;
        insert_tag(&db.master_pool, "t5", "Circle").await;
        settings::update(
            &db.master_pool,
            json!({"anthropic": {"access": "claude_plan", "model": "claude-haiku-5-5"}}),
        )
        .await
        .unwrap();
        let api = mock::serve(200, &[], "{}").await;
        let credentials = Arc::new(Credentials::new(MemoryKeyring::default(), |name| {
            (name == "ANTHROPIC_API_KEY").then(|| "sk-ant-SENTINEL-app".to_string())
        }));
        let anthropic = Arc::new(AnthropicProvider::with_base_url(
            credentials,
            Arc::new(stub.cli(&[("ANTHROPIC_API_KEY", "sk-ant-SENTINEL-app")])),
            api.url.clone(),
        ));
        let engine = Engine::with_retry_delays(
            db.clone(),
            vec![anthropic as Arc<dyn TagProvider>],
            Arc::new(|_| {}),
            Arc::new(|_| {}),
            vec![Duration::from_millis(1); 3],
        );
        engine.start().await;
        PlanRun {
            engine,
            db,
            stub,
            api,
            lib_pool,
        }
    }

    impl PlanRun {
        async fn run(&self) -> (RunSummary, (String, Option<String>, i64)) {
            self.engine
                .start_run(
                    RunScope::Assets {
                        asset_ids: vec!["a1".into()],
                    },
                    "anthropic",
                    Mode::ExistingOnly,
                )
                .await
                .unwrap();
            for _ in 0..500 {
                let runs = self.engine.list_runs().await.unwrap();
                if runs[0].status != "running" {
                    let item = sqlx::query_as(
                        "SELECT status, error, attempts FROM tag_run_items WHERE asset_id = 'a1'",
                    )
                    .fetch_one(&self.db.master_pool)
                    .await
                    .unwrap();
                    assert!(self.api.received().is_empty(), "no HTTPS call from the app");
                    return (runs[0].clone(), item);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the run never stopped");
        }
    }

    #[tokio::test]
    async fn a_plan_run_tags_records_tokens_and_shows_the_warning_without_pausing() {
        let p = plan_engine("text.jsonl", "auth_logged_in.json").await;
        // As if a run were created, and wrote its first file, after the ended runs were listed
        const NEWER_RUN: &str = "22222222-0000-0000-0000-000000000000";
        std::fs::create_dir_all(p.stub.data_dir().join("autotag").join(NEWER_RUN)).unwrap();

        let (run, item) = p.run().await;

        assert_eq!(run.status, "done", "allowed_warning never pauses");
        assert_eq!(item.0, "done");
        assert_eq!(run.access.as_deref(), Some("claude_plan"));
        assert_eq!((run.input_tokens, run.output_tokens), (1452, 242));
        assert_eq!(run.cost_usd, 0.0);
        assert!(run.notice.unwrap().contains("76% of the seven-day limit"));
        let tags: Vec<String> =
            sqlx::query_scalar("SELECT tag_id FROM asset_tags WHERE asset_id = 'a1' ORDER BY 1")
                .fetch_all(&p.lib_pool)
                .await
                .unwrap();
        assert_eq!(tags, ["t3", "t5"]);
        assert!(
            !p.stub.data_dir().join("autotag").join(&run.id).exists(),
            "the vocabulary file is deleted with the finished run"
        );
        assert!(
            p.stub.data_dir().join("autotag").join(NEWER_RUN).is_dir(),
            "a run the database doesn't list as ended keeps its file"
        );
    }

    #[tokio::test]
    async fn fatal_cli_results_pause_the_run_with_a_message() {
        for (fixture, reason) in [
            ("model_not_found.jsonl", "Unknown model claude-haiku-5-5"),
            ("logged_out.jsonl", "Not logged in · Please run /login"),
            (
                "rate_limit_rejected.jsonl",
                "Claude plan limit reached, resets at ",
            ),
            (
                "inherited_api_key.jsonl",
                "Claude plan run would bill an API key",
            ),
        ] {
            let p = plan_engine(fixture, "auth_logged_in.json").await;

            let (run, item) = p.run().await;

            assert_eq!(run.status, "paused", "{fixture}");
            let pause_reason = run.pause_reason.unwrap();
            assert!(pause_reason.contains(reason), "{fixture}: {pause_reason}");
            assert_eq!(
                item.0, "queued",
                "{fixture}: the item goes back to the queue"
            );
            assert_eq!(p.stub.calls().len(), 1, "{fixture}: never retried");
            assert!(
                p.stub.data_dir().join("autotag").join(&run.id).exists(),
                "a paused run keeps its vocabulary file"
            );
        }
    }

    #[tokio::test]
    async fn a_rate_limit_retries_then_fails_the_item() {
        let p = plan_engine("rate_limit.jsonl", "auth_logged_in.json").await;

        let (run, item) = p.run().await;

        assert_eq!(run.status, "done");
        assert_eq!(item.0, "failed");
        assert!(item.1.unwrap().contains("gave up after 4 tries"));
        assert_eq!(p.stub.calls().len(), 4, "one call and three retries");
    }

    #[tokio::test]
    async fn exhausted_structured_output_retries_fail_the_item() {
        let p = plan_engine(
            "error_max_structured_output_retries.jsonl",
            "auth_logged_in.json",
        )
        .await;

        let (run, item) = p.run().await;

        assert_eq!((run.status.as_str(), item.0.as_str()), ("done", "failed"));
        assert!(item
            .1
            .unwrap()
            .contains("error_max_structured_output_retries"));
        assert_eq!(p.stub.calls().len(), 1, "not retried");
    }

    #[tokio::test]
    async fn a_logged_out_cli_starts_no_run_and_sends_nothing() {
        let p = plan_engine("text.jsonl", "auth_logged_out.json").await;

        let error = p
            .engine
            .start_run(
                RunScope::Assets {
                    asset_ids: vec!["a1".into()],
                },
                "anthropic",
                Mode::ExistingOnly,
            )
            .await
            .unwrap_err();

        assert_eq!(
            error,
            "claude is logged out: run claude in a terminal and log in"
        );
        assert!(p.engine.list_runs().await.unwrap().is_empty());
        assert!(p.stub.calls().is_empty() && p.api.received().is_empty());
        let options = p.engine.provider_options().await.unwrap();
        assert_eq!(options[0].access, Some("claude_plan"));
        assert_eq!(
            options[0].unavailable_reason.as_deref(),
            Some(error.as_str())
        );
    }
}
