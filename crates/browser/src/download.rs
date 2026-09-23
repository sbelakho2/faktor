//! Downloads (spec §9): **disabled by default, and native downloads are
//! always denied** — an enabled policy means "capture under a hard cap",
//! never "let Chromium write wherever it likes".
//!
//! # Enforcement model
//!
//! * The browser instance installs `Browser.setDownloadBehavior`
//!   `{ "behavior": "deny" }` on the root CDP session, so Chromium never
//!   writes a download file itself.
//! * `Browser.downloadWillBegin` / `Browser.downloadProgress` are
//!   **browser-domain** events (no session id): the instance owns them
//!   centrally (GUID -> url/context/page record, bounded), cancels every
//!   native download through `Browser.cancelDownload`, and fails closed on
//!   malformed or missing numeric fields — anything that cannot be proven
//!   under the byte cap is refused.
//! * A configured policy captures eligible responses (a
//!   `Content-Disposition: attachment` response) through response-stage
//!   `Fetch` interception: the body is streamed with
//!   `Fetch.takeResponseBodyAsStream` + `IO.read` into a profile-scoped temp
//!   file under the byte cap, aborted the moment the cap would be exceeded,
//!   fsynced, and atomically published only after complete success. A failed
//!   capture leaves no partial destination file behind.
//!
//! All filesystem work goes through [`faktor_fs::RootedDir`], so the download
//! subdirectory is a validated *relative* location under the profile root and
//! a directory entry swapped for a symlink can never redirect a write.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use faktor_core::cancellation::CancellationToken;
use faktor_core::time::Deadline;
use faktor_fs::RootedDir;

use crate::cdp::CdpClient;
use crate::error::BrowserError;
use crate::timeutil::deadline_in;

/// IO.read request size for a captured stream.
const READ_CHUNK: usize = 64 * 1024;
/// Hard bound on the number of IO.read round-trips for one captured stream
/// (a hostile peer returning empty non-eof chunks cannot spin forever).
const MAX_STREAM_READS_EXTRA: u64 = 64;
/// Bounded central GUID record ring.
const MAX_DOWNLOAD_RECORDS: usize = 256;
/// Maximum published filename length (bytes).
const MAX_FILENAME_BYTES: usize = 200;
/// Temp-file prefix inside the download directory.
const TEMP_PREFIX: &str = ".faktor-download-";

/// Download policy for one browser profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadPolicy {
    /// Disabled by default; enabling is an explicit operator decision.
    pub enabled: bool,
    /// Required when enabled: a validated **relative** subdirectory under the
    /// profile root (e.g. `"downloads"`), never an arbitrary path.
    pub directory: Option<String>,
    /// Hard byte cap enforced during capture (0 is refused).
    pub max_bytes: u64,
}

impl Default for DownloadPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            directory: None,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

impl DownloadPolicy {
    /// Validate the policy shape. Returns the validated relative
    /// subdirectory when the policy is enabled.
    pub fn validate(&self) -> Result<Option<PathBuf>, BrowserError> {
        if !self.enabled {
            return Ok(None);
        }
        let Some(directory) = &self.directory else {
            return Err(BrowserError::invalid_config(
                "downloads enabled without a profile-scoped directory",
            ));
        };
        if self.max_bytes == 0 {
            return Err(BrowserError::invalid_config(
                "download max_bytes must be > 0",
            ));
        }
        let rel = validate_relative_dir(directory)?;
        Ok(Some(rel))
    }

    /// The CDP command that installs the policy for the browser. Native
    /// downloads are denied unconditionally: capture is the only write path.
    pub fn set_behavior_command(&self) -> (&'static str, Value) {
        (
            "Browser.setDownloadBehavior",
            json!({ "behavior": "deny", "eventsEnabled": true }),
        )
    }

    /// The typed error for a policy-denied (native) download.
    pub fn denied_error(&self, url: &str) -> BrowserError {
        BrowserError::DownloadBlocked {
            url: url.to_string(),
        }
    }
}

/// Validate a policy-provided directory as a non-empty relative path of plain
/// components (`..`, rooted forms, prefixes and NULs are refused).
pub fn validate_relative_dir(directory: &str) -> Result<PathBuf, BrowserError> {
    if directory.is_empty() || directory.len() > 512 {
        return Err(BrowserError::invalid_config(
            "download directory must be a non-empty relative path",
        ));
    }
    let mut rel = PathBuf::new();
    for comp in Path::new(directory).components() {
        match comp {
            Component::Normal(name) => {
                if name.as_encoded_bytes().contains(&0) {
                    return Err(BrowserError::invalid_config(
                        "download directory component contains a NUL byte",
                    ));
                }
                rel.push(name);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(BrowserError::invalid_config(
                    "download directory must stay inside the profile (relative, no `..`)",
                ))
            }
        }
    }
    if rel.as_os_str().is_empty() {
        return Err(BrowserError::invalid_config(
            "download directory must name at least one component",
        ));
    }
    Ok(rel)
}

/// One central download record (`GUID -> context/page`), bounded and
/// fail-closed: fields that cannot be parsed are recorded as invalid rather
/// than dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRecord {
    pub guid: String,
    pub url: String,
    pub suggested_filename: String,
    pub frame_id: Option<String>,
    pub browser_context_id: Option<String>,
    pub page_target_id: Option<String>,
    pub received_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub state: DownloadState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadState {
    /// Announced, not yet resolved.
    Pending,
    /// Refused/cancelled (native downloads are always refused).
    Cancelled,
    /// Captured and atomically published.
    Captured,
    /// Refused because a field was malformed or the byte cap was exceeded.
    Rejected,
}

/// Bounded download counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DownloadStats {
    pub blocked_total: u64,
    pub captured_total: u64,
    pub captured_bytes: u64,
    pub rejected_total: u64,
    pub cancelled_over_bound: u64,
}

/// What a caller must do with a browser-domain download event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeDownloadDenial {
    pub guid: String,
    pub url: String,
}

/// The outcome of one response-stage paused request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadOutcome {
    /// The response was continued (not an attachment, or capture disabled).
    Continued,
    /// The download was refused (native denial, cap violation or malformed
    /// stream); the request was failed/aborted.
    Denied { url: String },
    /// A complete capture was published.
    Captured { path: PathBuf, bytes: u64 },
}

/// Central, profile-scoped download authority owned by one browser instance.
pub struct DownloadManager {
    policy: DownloadPolicy,
    root: RootedDir,
    /// Profile-relative download directory (`<profile>/<policy.directory>`).
    dir_rel: PathBuf,
    ready: AtomicBool,
    records: Mutex<BTreeMap<String, DownloadRecord>>,
    order: Mutex<VecDeque<String>>,
    last_error: Mutex<Option<BrowserError>>,
    stats: Mutex<DownloadStats>,
}

impl std::fmt::Debug for DownloadManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadManager")
            .field("enabled", &self.policy.enabled)
            .field("dir", &self.dir_rel)
            .field("records", &self.records.lock().unwrap().len())
            .finish()
    }
}

impl DownloadManager {
    /// Build the manager for one profile. When the policy is enabled the
    /// download directory is created (owner-only) through the rooted
    /// authority before any browser exists; a hostile symlink swap makes this
    /// fail typed.
    pub fn new(
        policy: DownloadPolicy,
        root: RootedDir,
        base: &Path,
    ) -> Result<Arc<Self>, BrowserError> {
        if base.as_os_str().is_empty() {
            return Err(BrowserError::invalid_config(
                "download manager requires a profile-relative base",
            ));
        }
        let subdir = policy
            .validate()?
            .unwrap_or_else(|| PathBuf::from("downloads"));
        let dir_rel = base.join(&subdir);
        let manager = Arc::new(Self {
            policy,
            root,
            dir_rel,
            ready: AtomicBool::new(false),
            records: Mutex::new(BTreeMap::new()),
            order: Mutex::new(VecDeque::new()),
            last_error: Mutex::new(None),
            stats: Mutex::new(DownloadStats::default()),
        });
        if manager.policy.enabled {
            manager.prepare()?;
        }
        Ok(manager)
    }

    /// Create (once) and restrict the download directory.
    fn prepare(&self) -> Result<(), BrowserError> {
        if self.ready.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.root.create_dir_all(&self.dir_rel).map_err(|e| {
            BrowserError::profile(format!(
                "cannot create download directory {:?}: {e}",
                self.dir_rel
            ))
        })?;
        self.root
            .restrict_owner_only(&self.dir_rel)
            .map_err(|e| BrowserError::profile(format!("cannot restrict download dir: {e}")))?;
        self.ready.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// The policy this manager enforces.
    pub fn policy(&self) -> &DownloadPolicy {
        &self.policy
    }

    /// The CDP command installing the native-deny behavior.
    pub fn behavior_command(&self) -> (&'static str, Value) {
        self.policy.set_behavior_command()
    }

    /// The Fetch patterns to install: request stage always, response stage
    /// when capture is enabled (downloads are identified at response time by
    /// `Content-Disposition: attachment`).
    pub fn fetch_patterns(&self) -> Value {
        let mut patterns = vec![json!({ "urlPattern": "*", "requestStage": "Request" })];
        if self.policy.enabled {
            patterns.push(json!({ "urlPattern": "*", "requestStage": "Response" }));
        }
        Value::Array(patterns)
    }

    /// The latest download failure for this profile, if any.
    pub fn last_error(&self) -> Option<BrowserError> {
        self.last_error.lock().unwrap().clone()
    }

    pub fn stats(&self) -> DownloadStats {
        self.stats.lock().unwrap().clone()
    }

    /// Bounded central snapshot in announcement order.
    pub fn records(&self) -> Vec<DownloadRecord> {
        let records = self.records.lock().unwrap();
        self.order
            .lock()
            .unwrap()
            .iter()
            .filter_map(|guid| records.get(guid).cloned())
            .collect()
    }

    /// Handle one browser-domain download event. Native downloads are always
    /// denied; the returned denial is the GUID the caller must cancel through
    /// `Browser.cancelDownload`. Fields that cannot be parsed fail closed: a
    /// malformed `receivedBytes`/`totalBytes` (or a missing GUID) is a
    /// rejection, never an excuse to let the download proceed.
    pub fn observe_native(&self, method: &str, params: &Value) -> Option<NativeDownloadDenial> {
        match method {
            "Browser.downloadWillBegin" => {
                let guid = match string_field(params, "guid") {
                    Ok(Some(guid)) if !guid.is_empty() => guid,
                    _ => {
                        self.reject("", "", "download announcement without a usable guid", false);
                        return None;
                    }
                };
                let url = string_field(params, "url")
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let suggested_filename = string_field(params, "suggestedFilename")
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                self.record(DownloadRecord {
                    guid: guid.clone(),
                    url: url.clone(),
                    suggested_filename,
                    frame_id: string_field(params, "frameId").ok().flatten(),
                    browser_context_id: string_field(params, "browserContextId").ok().flatten(),
                    page_target_id: string_field(params, "pageId").ok().flatten(),
                    received_bytes: None,
                    total_bytes: None,
                    state: DownloadState::Cancelled,
                });
                {
                    let mut stats = self.stats.lock().unwrap();
                    stats.blocked_total = stats.blocked_total.saturating_add(1);
                }
                *self.last_error.lock().unwrap() = Some(self.policy.denied_error(&url));
                Some(NativeDownloadDenial { guid, url })
            }
            "Browser.downloadProgress" => {
                let guid = match string_field(params, "guid") {
                    Ok(Some(guid)) if !guid.is_empty() => guid,
                    _ => {
                        self.reject("", "", "download progress without a usable guid", false);
                        return None;
                    }
                };
                let url = self
                    .records
                    .lock()
                    .unwrap()
                    .get(&guid)
                    .map(|record| record.url.clone())
                    .unwrap_or_default();
                let received = match fail_closed_u64(params.get("receivedBytes")) {
                    Ok(value) => value,
                    Err(MalformedNumber) => {
                        self.reject(&guid, &url, "malformed receivedBytes field", false);
                        return Some(NativeDownloadDenial { guid, url });
                    }
                };
                let total = match fail_closed_u64(params.get("totalBytes")) {
                    Ok(value) => value,
                    Err(MalformedNumber) => {
                        self.reject(&guid, &url, "malformed totalBytes field", false);
                        return Some(NativeDownloadDenial { guid, url });
                    }
                };
                {
                    let mut records = self.records.lock().unwrap();
                    if let Some(record) = records.get_mut(&guid) {
                        record.received_bytes = received;
                        record.total_bytes = total;
                    }
                }
                let over_bound = received.unwrap_or(0) > self.policy.max_bytes
                    || total.unwrap_or(0) > self.policy.max_bytes;
                if over_bound {
                    self.reject(
                        &guid,
                        &url,
                        "download exceeded the configured byte bound",
                        true,
                    );
                    return Some(NativeDownloadDenial { guid, url });
                }
                // Native downloads are denied even when capture is enabled:
                // any progress event is itself a policy violation.
                self.reject(&guid, &url, "native download is never permitted", false);
                Some(NativeDownloadDenial { guid, url })
            }
            _ => None,
        }
    }

    /// Handle one response-stage `Fetch.requestPaused` event.
    pub async fn handle_paused(
        &self,
        client: &CdpClient,
        session: &str,
        params: &Value,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<DownloadOutcome, BrowserError> {
        let request_id = params
            .get("requestId")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::cdp("Fetch.requestPaused without requestId"))?
            .to_string();
        let url = params
            .get("request")
            .and_then(|request| request.get("url"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let headers = params
            .get("responseHeaders")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let disposition = header_value(&headers, "content-disposition");
        let attachment = disposition
            .as_deref()
            .map(is_attachment_disposition)
            .unwrap_or(false);
        if !self.policy.enabled || !attachment {
            self.continue_response(client, session, &request_id, deadline, cancel)
                .await?;
            return Ok(DownloadOutcome::Continued);
        }
        if let Err(error) = self.prepare() {
            *self.last_error.lock().unwrap() = Some(error.clone());
            let _ = self
                .deny(client, session, &request_id, deadline, cancel)
                .await;
            return Err(error);
        }
        // Declared length is enforced before a single byte is streamed; an
        // unparseable Content-Length fails closed.
        if let Some(raw) = header_value(&headers, "content-length") {
            match raw.trim().parse::<u64>() {
                Ok(len) if len > self.policy.max_bytes => {
                    self.deny_capture(
                        &url,
                        &format!(
                            "declared length {len} exceeds the {} byte bound",
                            self.policy.max_bytes
                        ),
                        &request_id,
                        client,
                        session,
                        deadline,
                        cancel,
                    )
                    .await?;
                    return Ok(DownloadOutcome::Denied { url });
                }
                Ok(_) => {}
                Err(_) => {
                    self.deny_capture(
                        &url,
                        "malformed Content-Length on a download response",
                        &request_id,
                        client,
                        session,
                        deadline,
                        cancel,
                    )
                    .await?;
                    return Ok(DownloadOutcome::Denied { url });
                }
            }
        }
        let filename = download_filename(disposition.as_deref(), &url, &request_id);
        let dest_rel = self.dir_rel.join(&filename);
        let tmp_rel = self.dir_rel.join(format!(
            "{TEMP_PREFIX}{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        let taken = client
            .send(
                Some(session),
                "Fetch.takeResponseBodyAsStream",
                json!({ "requestId": request_id }),
                deadline,
                cancel,
            )
            .await;
        let handle = match taken {
            Ok(value) => match value.get("stream").and_then(Value::as_str) {
                Some(handle) => handle.to_string(),
                None => {
                    self.deny_capture(
                        &url,
                        "takeResponseBodyAsStream returned no stream",
                        &request_id,
                        client,
                        session,
                        deadline,
                        cancel,
                    )
                    .await?;
                    return Ok(DownloadOutcome::Denied { url });
                }
            },
            Err(error) => {
                self.reject(
                    &request_id,
                    &url,
                    &format!("cannot take the download stream: {error}"),
                    false,
                );
                return Err(error);
            }
        };
        let mut file = match self.root.open_create_new(&tmp_rel) {
            Ok(file) => file,
            Err(error) => {
                let _ = close_stream(client, session, &handle).await;
                self.reject(
                    &request_id,
                    &url,
                    &format!("cannot create the temp file: {error}"),
                    false,
                );
                return Err(BrowserError::profile(format!(
                    "cannot create download temp file: {error}"
                )));
            }
        };
        let max_reads = self
            .policy
            .max_bytes
            .div_ceil(READ_CHUNK as u64)
            .saturating_add(MAX_STREAM_READS_EXTRA);
        let mut written: u64 = 0;
        let mut reads: u64 = 0;
        loop {
            reads = reads.saturating_add(1);
            if reads > max_reads {
                return self
                    .abort_capture(
                        file,
                        &tmp_rel,
                        &handle,
                        client,
                        session,
                        &url,
                        &request_id,
                        "download stream did not terminate within the read bound",
                    )
                    .await;
            }
            let read = client
                .send(
                    Some(session),
                    "IO.read",
                    json!({ "handle": handle, "size": READ_CHUNK }),
                    deadline,
                    cancel,
                )
                .await;
            let value = match read {
                Ok(value) => value,
                Err(error) => {
                    drop(file);
                    let _ = close_stream(client, session, &handle).await;
                    let _ = self.root.remove_file(&tmp_rel);
                    self.reject(
                        &request_id,
                        &url,
                        &format!("stream read failed: {error}"),
                        false,
                    );
                    return Err(error);
                }
            };
            let (chunk, eof) = match decode_io_chunk(&value) {
                Ok(decoded) => decoded,
                Err(detail) => {
                    return self
                        .abort_capture(
                            file,
                            &tmp_rel,
                            &handle,
                            client,
                            session,
                            &url,
                            &request_id,
                            &detail,
                        )
                        .await;
                }
            };
            if !chunk.is_empty() {
                if written.saturating_add(chunk.len() as u64) > self.policy.max_bytes {
                    return self
                        .abort_capture(
                            file,
                            &tmp_rel,
                            &handle,
                            client,
                            session,
                            &url,
                            &request_id,
                            "download exceeded the configured byte bound",
                        )
                        .await;
                }
                if let Err(e) = file.write_all(&chunk) {
                    drop(file);
                    let _ = close_stream(client, session, &handle).await;
                    let _ = self.root.remove_file(&tmp_rel);
                    self.reject(&request_id, &url, &format!("temp write failed: {e}"), false);
                    return Err(BrowserError::profile(format!(
                        "cannot write download temp file: {e}"
                    )));
                }
                written = written.saturating_add(chunk.len() as u64);
            }
            if eof {
                break;
            }
        }
        if let Err(e) = file.sync_all() {
            drop(file);
            let _ = close_stream(client, session, &handle).await;
            let _ = self.root.remove_file(&tmp_rel);
            self.reject(&request_id, &url, &format!("fsync failed: {e}"), false);
            return Err(BrowserError::profile(format!(
                "cannot fsync download temp file: {e}"
            )));
        }
        drop(file);
        let _ = close_stream(client, session, &handle).await;
        if let Err(error) = self.root.atomic_publish(&tmp_rel, &dest_rel) {
            let _ = self.root.remove_file(&tmp_rel);
            self.reject(
                &request_id,
                &url,
                &format!("atomic publish failed: {error}"),
                false,
            );
            return Err(BrowserError::profile(format!(
                "cannot publish download: {error}"
            )));
        }
        let _ = self.root.sync_dir(&self.dir_rel);
        self.record(DownloadRecord {
            guid: request_id.clone(),
            url: url.clone(),
            suggested_filename: filename,
            frame_id: None,
            browser_context_id: None,
            page_target_id: None,
            received_bytes: Some(written),
            total_bytes: Some(written),
            state: DownloadState::Captured,
        });
        {
            let mut stats = self.stats.lock().unwrap();
            stats.captured_total = stats.captured_total.saturating_add(1);
            stats.captured_bytes = stats.captured_bytes.saturating_add(written);
        }
        Ok(DownloadOutcome::Captured {
            path: self.root.join(&dest_rel),
            bytes: written,
        })
    }

    /// Abort an in-flight capture: close the stream, delete the temp file,
    /// fail the request best-effort and record the typed rejection. The
    /// destination is never touched.
    #[allow(clippy::too_many_arguments)]
    async fn abort_capture(
        &self,
        file: std::fs::File,
        tmp_rel: &Path,
        handle: &str,
        client: &CdpClient,
        session: &str,
        url: &str,
        request_id: &str,
        detail: &str,
    ) -> Result<DownloadOutcome, BrowserError> {
        drop(file);
        let _ = close_stream(client, session, handle).await;
        let _ = self.root.remove_file(tmp_rel);
        self.reject(request_id, url, detail, true);
        let _ = self
            .deny(
                client,
                session,
                request_id,
                deadline_in(1_000),
                &CancellationToken::new(),
            )
            .await;
        Ok(DownloadOutcome::Denied {
            url: url.to_string(),
        })
    }

    /// Deny a capture before or at the response stage: record the typed
    /// rejection and fail the request.
    #[allow(clippy::too_many_arguments)]
    async fn deny_capture(
        &self,
        url: &str,
        detail: &str,
        request_id: &str,
        client: &CdpClient,
        session: &str,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<(), BrowserError> {
        self.reject(request_id, url, detail, false);
        let _ = self
            .deny(client, session, request_id, deadline, cancel)
            .await;
        Ok(())
    }

    fn reject(&self, guid: &str, url_hint: &str, detail: &str, over_bound: bool) {
        let url = if url_hint.is_empty() {
            self.records
                .lock()
                .unwrap()
                .get(guid)
                .map(|record| record.url.clone())
                .unwrap_or_default()
        } else {
            url_hint.to_string()
        };
        if !guid.is_empty() {
            self.record(DownloadRecord {
                guid: guid.to_string(),
                url: url.clone(),
                suggested_filename: String::new(),
                frame_id: None,
                browser_context_id: None,
                page_target_id: None,
                received_bytes: None,
                total_bytes: None,
                state: DownloadState::Rejected,
            });
        }
        {
            let mut stats = self.stats.lock().unwrap();
            stats.rejected_total = stats.rejected_total.saturating_add(1);
            if over_bound {
                stats.cancelled_over_bound = stats.cancelled_over_bound.saturating_add(1);
            }
        }
        *self.last_error.lock().unwrap() = Some(BrowserError::DownloadRejected {
            url,
            detail: detail.to_string(),
        });
    }

    fn record(&self, record: DownloadRecord) {
        let guid = record.guid.clone();
        {
            let mut records = self.records.lock().unwrap();
            if !records.contains_key(&guid) {
                let mut order = self.order.lock().unwrap();
                while order.len() >= MAX_DOWNLOAD_RECORDS {
                    if let Some(evicted) = order.pop_front() {
                        records.remove(&evicted);
                    }
                }
                order.push_back(guid.clone());
            }
            records.insert(guid, record);
        }
    }

    async fn continue_response(
        &self,
        client: &CdpClient,
        session: &str,
        request_id: &str,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<(), BrowserError> {
        client
            .send(
                Some(session),
                "Fetch.continueResponse",
                json!({ "requestId": request_id }),
                deadline,
                cancel,
            )
            .await
            .map(|_| ())
    }

    async fn deny(
        &self,
        client: &CdpClient,
        session: &str,
        request_id: &str,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<(), BrowserError> {
        client
            .send(
                Some(session),
                "Fetch.failRequest",
                json!({ "requestId": request_id, "errorReason": "BlockedByClient" }),
                deadline,
                cancel,
            )
            .await
            .map(|_| ())
    }
}

/// True when a paused `Fetch.requestPaused` event is at the response stage
/// (its params carry response fields; request-stage events do not).
pub fn is_response_stage(params: &Value) -> bool {
    params.get("responseStatusCode").is_some()
        || params.get("responseHeaders").is_some()
        || params.get("responseErrorReason").is_some()
}

/// Does a `Content-Disposition` header value mark an attachment?
fn is_attachment_disposition(value: &str) -> bool {
    value
        .split(';')
        .next()
        .map(|kind| kind.trim().eq_ignore_ascii_case("attachment"))
        .unwrap_or(false)
}

/// Case-insensitive header lookup in a CDP `responseHeaders` array.
pub fn header_value(headers: &[Value], name: &str) -> Option<String> {
    headers.iter().find_map(|header| {
        let header_name = header.get("name").and_then(Value::as_str)?;
        if header_name.eq_ignore_ascii_case(name) {
            header
                .get("value")
                .and_then(Value::as_str)
                .map(str::to_string)
        } else {
            None
        }
    })
}

/// A numeric field that could not be parsed as a byte count. The caller
/// fails closed (refuses the download).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MalformedNumber;

/// Parse a JSON numeric field fail-closed: `None` means absent/null; a
/// non-number, a non-finite value, a negative value or one that does not fit
/// `u64` is [`MalformedNumber`] (the caller refuses the download).
pub fn fail_closed_u64(value: Option<&Value>) -> Result<Option<u64>, MalformedNumber> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => {
            if let Some(value) = number.as_u64() {
                return Ok(Some(value));
            }
            match number.as_f64() {
                Some(value) if value.is_finite() && value >= 0.0 && value <= u64::MAX as f64 => {
                    Ok(Some(value as u64))
                }
                _ => Err(MalformedNumber),
            }
        }
        Some(_) => Err(MalformedNumber),
    }
}

/// String field extraction that treats a non-string as malformed.
fn string_field(params: &Value, name: &str) -> Result<Option<String>, ()> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(()),
    }
}

/// Decode one `IO.read` result. A missing `data` without `eof` is malformed
/// (fail closed).
fn decode_io_chunk(value: &Value) -> Result<(Vec<u8>, bool), String> {
    let eof = value.get("eof").and_then(Value::as_bool).unwrap_or(false);
    let Some(data) = value.get("data") else {
        return if eof {
            Ok((Vec::new(), true))
        } else {
            Err("IO.read returned no data and no eof".to_string())
        };
    };
    let Some(data) = data.as_str() else {
        return Err("IO.read data is not a string".to_string());
    };
    let encoded = value
        .get("base64Encoded")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if encoded {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| format!("invalid base64 IO.read data: {e}"))?;
        Ok((bytes, eof))
    } else {
        Ok((data.as_bytes().to_vec(), eof))
    }
}

async fn close_stream(client: &CdpClient, session: &str, handle: &str) -> Result<(), BrowserError> {
    client
        .send(
            Some(session),
            "IO.close",
            json!({ "handle": handle }),
            deadline_in(1_000),
            &CancellationToken::new(),
        )
        .await
        .map(|_| ())
}

/// The sanitized destination filename for one captured download: the
/// `Content-Disposition` filename when usable, else the URL's last path
/// segment, else a request-id-derived name. Path separators, control
/// characters and `.`/`..` are stripped; the result is a single bounded
/// component.
pub fn download_filename(disposition: Option<&str>, url: &str, request_id: &str) -> String {
    let candidate = disposition
        .and_then(disposition_filename)
        .or_else(|| url_filename(url))
        .unwrap_or_default();
    sanitize_filename(&candidate)
        .unwrap_or_else(|| format!("download-{}", sanitize_token(request_id)))
}

/// Extract `filename=` / `filename*=` from a Content-Disposition value.
fn disposition_filename(value: &str) -> Option<String> {
    for part in value.split(';').skip(1) {
        let part = part.trim();
        let (name, raw) = part.split_once('=')?;
        let name = name.trim();
        if !name.eq_ignore_ascii_case("filename") && !name.eq_ignore_ascii_case("filename*") {
            continue;
        }
        let raw = raw.trim().trim_matches('"').trim_matches('\'');
        // RFC 5987 form: charset'lang'percent-encoded
        let raw = raw.rsplit('\'').next().unwrap_or(raw);
        if name.eq_ignore_ascii_case("filename*") {
            if let Some(decoded) = percent_decode(raw) {
                return Some(decoded);
            }
        }
        if !raw.is_empty() {
            return Some(raw.to_string());
        }
    }
    None
}

/// Last path segment of a URL, ignoring query and fragment.
fn url_filename(url: &str) -> Option<String> {
    let without_fragment = url.split('#').next().unwrap_or(url);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    let segment = without_query.rsplit('/').next().unwrap_or_default();
    if segment.is_empty() {
        None
    } else {
        Some(percent_decode(segment).unwrap_or_else(|| segment.to_string()))
    }
}

/// Minimal percent-decode for filename display (fail-closed: malformed
/// escapes yield `None`).
fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex(*bytes.get(index + 1)?)?;
            let low = hex(*bytes.get(index + 2)?)?;
            out.push((high << 4) | low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Reduce a raw name to one safe path component, or `None` when nothing
/// usable remains.
fn sanitize_filename(raw: &str) -> Option<String> {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = last
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\' && *c != ':')
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    let bounded: String = trimmed.chars().take(MAX_FILENAME_BYTES).collect();
    if bounded.is_empty() || bounded == "." || bounded == ".." {
        None
    } else {
        Some(bounded)
    }
}

fn sanitize_token(raw: &str) -> String {
    let token: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(48)
        .collect();
    if token.is_empty() {
        "stream".to_string()
    } else {
        token
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rooted(tmp: &tempfile::TempDir, base: &str) -> (RootedDir, PathBuf) {
        let root = RootedDir::create(&tmp.path().join("profiles")).unwrap();
        root.create_dir_all(Path::new(base)).unwrap();
        (root, PathBuf::from(base))
    }

    #[test]
    fn downloads_are_denied_by_default_and_native_is_always_denied() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, base) = rooted(&tmp, "p1");
        let manager = DownloadManager::new(DownloadPolicy::default(), root, &base).unwrap();
        assert!(!manager.policy().enabled);
        let (method, params) = manager.behavior_command();
        assert_eq!(method, "Browser.setDownloadBehavior");
        assert_eq!(params["behavior"], "deny");
        // Native announcement is denied even though capture is disabled.
        let denial = manager
            .observe_native(
                "Browser.downloadWillBegin",
                &json!({
                    "guid": "g1",
                    "url": "https://first.test/file.pdf",
                    "suggestedFilename": "file.pdf",
                    "frameId": "f1"
                }),
            )
            .expect("denied");
        assert_eq!(denial.guid, "g1");
        assert_eq!(
            manager.last_error(),
            Some(BrowserError::DownloadBlocked {
                url: "https://first.test/file.pdf".to_string()
            })
        );
        let record = manager.records().pop().unwrap();
        assert_eq!(record.frame_id.as_deref(), Some("f1"));
        assert_eq!(record.state, DownloadState::Cancelled);
        assert_eq!(manager.stats().blocked_total, 1);
    }

    #[test]
    fn malformed_or_negative_progress_fields_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, base) = rooted(&tmp, "p1");
        let policy = DownloadPolicy {
            enabled: true,
            directory: Some("downloads".to_string()),
            max_bytes: 1024,
        };
        let manager = DownloadManager::new(policy, root, &base).unwrap();
        for hostile in [
            json!({"guid": "g1", "receivedBytes": "many"}),
            json!({"guid": "g1", "receivedBytes": -5}),
            json!({"guid": "g1", "receivedBytes": 1e300}),
            json!({"guid": "g1", "totalBytes": null, "receivedBytes": true}),
            json!({"receivedBytes": 10}),
        ] {
            let denial = manager.observe_native("Browser.downloadProgress", &hostile);
            assert!(
                denial.is_some() || hostile.get("guid").is_none(),
                "a malformed progress field must fail closed: {hostile}"
            );
            assert!(
                matches!(
                    manager.last_error(),
                    Some(BrowserError::DownloadRejected { .. })
                ),
                "{hostile}"
            );
        }
        // Over-bound fields cancel before any byte is written.
        let denial = manager
            .observe_native(
                "Browser.downloadProgress",
                &json!({"guid": "g1", "receivedBytes": 2048}),
            )
            .expect("denied");
        assert_eq!(denial.guid, "g1");
        assert_eq!(manager.stats().cancelled_over_bound, 1);
        // Valid, in-bound progress is still denied: native is never allowed.
        let denial = manager
            .observe_native(
                "Browser.downloadProgress",
                &json!({"guid": "g1", "receivedBytes": 10, "totalBytes": 20}),
            )
            .expect("native is always denied");
        assert_eq!(denial.guid, "g1");
    }

    #[test]
    fn policy_directories_are_relative_and_bounded() {
        let policy = DownloadPolicy {
            enabled: true,
            directory: None,
            max_bytes: 1,
        };
        assert!(policy.validate().is_err());
        let policy = DownloadPolicy {
            enabled: true,
            directory: Some("../outside".to_string()),
            max_bytes: 1,
        };
        assert!(policy.validate().is_err());
        let policy = DownloadPolicy {
            enabled: true,
            directory: Some("/absolute".to_string()),
            max_bytes: 1,
        };
        assert!(policy.validate().is_err());
        let policy = DownloadPolicy {
            enabled: true,
            directory: Some("downloads".to_string()),
            max_bytes: 0,
        };
        assert!(policy.validate().is_err());
        let policy = DownloadPolicy {
            enabled: true,
            directory: Some("nested/downloads".to_string()),
            max_bytes: 1,
        };
        assert_eq!(
            policy.validate().unwrap().unwrap(),
            PathBuf::from("nested/downloads")
        );
    }

    #[test]
    fn attachment_detection_and_filename_sanitization() {
        assert!(is_attachment_disposition("attachment"));
        assert!(is_attachment_disposition("ATTACHMENT; filename=\"a.bin\""));
        assert!(!is_attachment_disposition("inline; filename=\"a.html\""));
        assert!(!is_attachment_disposition(""));
        assert_eq!(
            download_filename(
                Some("attachment; filename=\"../../etc/passwd\""),
                "https://x.test/",
                "r1"
            ),
            "passwd"
        );
        assert_eq!(
            download_filename(Some("attachment; filename=.."), "https://x.test/", "r1"),
            "download-r1"
        );
        assert_eq!(
            download_filename(None, "https://x.test/a/report.pdf?token=x", "r1"),
            "report.pdf"
        );
        assert_eq!(
            download_filename(Some("attachment"), "https://x.test/", "r1"),
            "download-r1"
        );
        let long = "a".repeat(500);
        assert_eq!(
            download_filename(Some(&format!("attachment; filename={long}")), "", "r1").len(),
            MAX_FILENAME_BYTES
        );
    }

    #[test]
    fn response_stage_detection_and_headers() {
        assert!(!is_response_stage(&json!({
            "requestId": "r1",
            "request": {"url": "https://x.test/"}
        })));
        assert!(is_response_stage(&json!({
            "requestId": "r1",
            "responseStatusCode": 200,
            "responseHeaders": []
        })));
        let headers = vec![
            json!({"name": "Content-Type", "value": "application/pdf"}),
            json!({"name": "content-disposition", "value": "attachment; filename=\"a.pdf\""}),
        ];
        assert_eq!(
            header_value(&headers, "Content-Disposition").as_deref(),
            Some("attachment; filename=\"a.pdf\"")
        );
        assert_eq!(header_value(&headers, "content-length"), None);
    }

    #[test]
    fn fetch_patterns_include_response_stage_only_when_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, base) = rooted(&tmp, "p1");
        let disabled =
            DownloadManager::new(DownloadPolicy::default(), root.clone(), &base).unwrap();
        assert_eq!(disabled.fetch_patterns().as_array().unwrap().len(), 1);
        let enabled = DownloadManager::new(
            DownloadPolicy {
                enabled: true,
                directory: Some("downloads".to_string()),
                max_bytes: 1024,
            },
            root,
            &base,
        )
        .unwrap();
        let patterns = enabled.fetch_patterns();
        assert_eq!(patterns.as_array().unwrap().len(), 2);
        assert_eq!(patterns[1]["requestStage"], "Response");
    }

    #[test]
    fn fail_closed_numeric_parsing() {
        assert_eq!(fail_closed_u64(None), Ok(None));
        assert_eq!(fail_closed_u64(Some(&Value::Null)), Ok(None));
        assert_eq!(fail_closed_u64(Some(&json!(7))), Ok(Some(7)));
        assert_eq!(fail_closed_u64(Some(&json!(7.5))), Ok(Some(7)));
        assert_eq!(fail_closed_u64(Some(&json!(-1))), Err(MalformedNumber));
        assert_eq!(fail_closed_u64(Some(&json!("7"))), Err(MalformedNumber));
        assert_eq!(fail_closed_u64(Some(&json!(true))), Err(MalformedNumber));
        assert_eq!(fail_closed_u64(Some(&json!([7]))), Err(MalformedNumber));
    }

    #[test]
    fn decode_io_chunk_is_strict() {
        assert_eq!(
            decode_io_chunk(&json!({"data": "aGk=", "base64Encoded": true, "eof": true})).unwrap(),
            (b"hi".to_vec(), true)
        );
        assert!(decode_io_chunk(&json!({"eof": false})).is_err());
        assert!(decode_io_chunk(&json!({"data": 7})).is_err());
        assert!(decode_io_chunk(&json!({"data": "!!!", "base64Encoded": true})).is_err());
    }
}
