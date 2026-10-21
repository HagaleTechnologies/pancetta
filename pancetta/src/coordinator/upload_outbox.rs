//! PAN-57: the process-wide pending-upload worker for per-QSO logbook uploads
//! (ClubLog, QRZ Logbook, eQSL, cqdx.io).
//!
//! The QSO upload subscriber hands each completed QSO's uploads to this
//! worker as a batch. The worker writes them to `~/.pancetta/upload_outbox.json`
//! before the first attempt, then delivers them one at a time per destination
//! ("lane"): transient failures retry on the `[network.reliability.retry]`
//! schedule and then every 5 minutes; a login rejection stops that destination
//! for the session; a record rejection holds that one entry until the next
//! start. The queue is drained again at every start.
//!
//! An upload is never sent before it is on disk: while the queue file cannot
//! be written (disk full, directory unwritable), new uploads stay held in
//! memory, the write is retried with capped backoff, and they are sent once
//! a write succeeds.
//!
//! There is exactly one worker per process (owned by the coordinator, reused
//! across Qso restarts), because a Qso crash-restart can briefly leave two
//! upload subscribers alive, and two writers on one file must never happen.
//!
//! State changes are published as [`UploadHealth`] for the readiness strip and
//! emitted as one Diagnostics row each. LoTW keeps its single tqsl attempt in
//! the subscriber and never enters the queue.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use chrono::Utc;
use pancetta_dx::upload_outbox::{
    Lane, NewUpload, OutboxStore, RetryDelay, RetrySchedule, UploadDestination, OUTBOX_MAX_ENTRIES,
};
use pancetta_dx::UploadFailureClass;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::tx::emit_diagnostic_full;
use crate::message_bus::{ComponentId, DiagnosticLevel, MessageBus};

/// Longest failure text in a Diagnostics row or a readiness detail.
pub(crate) const UPLOAD_TEXT_MAX_CHARS: usize = 160;

/// What one delivery attempt came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// The service took it (`duplicate`: it already had it).
    Accepted { duplicate: bool },
    /// The service refused it, or could not be reached.
    Failed {
        class: UploadFailureClass,
        text: String,
    },
}

/// Sends one queued payload to one destination.
pub(crate) trait OutboxTransport: Send + Sync + 'static {
    /// The destinations this transport can deliver to (the enabled ones).
    fn destinations(&self) -> Vec<UploadDestination>;
    /// Deliver `payload` to `d`.
    fn deliver(
        &self,
        d: UploadDestination,
        payload: String,
    ) -> impl Future<Output = Delivery> + Send;
}

/// The real logbook clients.
pub(crate) struct LogbookClients {
    pub clublog: Option<Arc<pancetta_dx::ClubLogClient>>,
    pub qrz: Option<Arc<pancetta_dx::QrzLogbookClient>>,
    pub eqsl: Option<Arc<pancetta_dx::EqslClient>>,
    pub cqdx: Option<Arc<pancetta_cqdx::CqdxClient>>,
}

fn failed(e: &pancetta_dx::DxError) -> Delivery {
    Delivery::Failed {
        class: e.upload_failure_class(),
        text: e.upload_failure_text(),
    }
}

/// How a cqdx.io client error maps onto the upload failure classes.
pub(crate) fn classify_cqdx_error(e: &pancetta_cqdx::CqdxError) -> UploadFailureClass {
    use pancetta_cqdx::CqdxError;
    match e {
        CqdxError::Unauthorized
        | CqdxError::InvalidToken(_)
        | CqdxError::InvalidBaseUrl(_)
        | CqdxError::NotConfigured
        | CqdxError::HttpInit(_) => UploadFailureClass::LoginRejected,
        CqdxError::Server { status, .. } => pancetta_dx::upload_failure_class_for_status(*status),
        CqdxError::ResponseTooLarge(_) => UploadFailureClass::RecordRejected,
        CqdxError::Http(_) | CqdxError::Json(_) => UploadFailureClass::Transient,
    }
}

impl OutboxTransport for LogbookClients {
    fn destinations(&self) -> Vec<UploadDestination> {
        UploadDestination::ALL
            .into_iter()
            .filter(|d| match d {
                UploadDestination::ClubLog => self.clublog.is_some(),
                UploadDestination::QrzLogbook => self.qrz.is_some(),
                UploadDestination::Eqsl => self.eqsl.is_some(),
                UploadDestination::Cqdx => self.cqdx.is_some(),
            })
            .collect()
    }

    async fn deliver(&self, d: UploadDestination, payload: String) -> Delivery {
        let missing = || Delivery::Failed {
            class: UploadFailureClass::LoginRejected,
            text: format!("{}: upload not configured", d.label()),
        };
        match d {
            UploadDestination::ClubLog => match &self.clublog {
                Some(c) => match c.upload_adif(&payload).await {
                    Ok(()) => Delivery::Accepted { duplicate: false },
                    Err(e) => failed(&e),
                },
                None => missing(),
            },
            UploadDestination::QrzLogbook => match &self.qrz {
                Some(c) => match c.upload_adif(&payload).await {
                    Ok(pancetta_dx::QrzInsertOutcome::Inserted { .. }) => {
                        Delivery::Accepted { duplicate: false }
                    }
                    Ok(pancetta_dx::QrzInsertOutcome::Duplicate { .. }) => {
                        Delivery::Accepted { duplicate: true }
                    }
                    Err(e) => failed(&e),
                },
                None => missing(),
            },
            UploadDestination::Eqsl => match &self.eqsl {
                Some(c) => match c.upload_adif(&payload).await {
                    Ok(pancetta_dx::QsoUploadOutcome::Logged) => {
                        Delivery::Accepted { duplicate: false }
                    }
                    Ok(pancetta_dx::QsoUploadOutcome::Duplicate) => {
                        Delivery::Accepted { duplicate: true }
                    }
                    Err(e) => failed(&e),
                },
                None => missing(),
            },
            UploadDestination::Cqdx => match &self.cqdx {
                Some(c) => {
                    let record = match serde_json::from_str::<pancetta_cqdx::QsoRecord>(&payload) {
                        Ok(r) => r,
                        Err(e) => {
                            return Delivery::Failed {
                                class: UploadFailureClass::RecordRejected,
                                text: format!("cqdx.io: queued record unreadable: {e}"),
                            }
                        }
                    };
                    match c.log_qso(record).await {
                        Ok(pancetta_cqdx::QsoUploadOutcome::Logged) => {
                            Delivery::Accepted { duplicate: false }
                        }
                        Ok(pancetta_cqdx::QsoUploadOutcome::Duplicate) => {
                            Delivery::Accepted { duplicate: true }
                        }
                        Err(e) => Delivery::Failed {
                            class: classify_cqdx_error(&e),
                            text: e.to_string(),
                        },
                    }
                }
                None => missing(),
            },
        }
    }
}

/// Build the clients for every enabled queue destination, logging each one
/// that is on. A cqdx.io client that fails to build is logged once and left
/// out.
pub(crate) fn build_logbook_clients(
    clublog_cfg: &pancetta_config::network::ClubLogConfig,
    qrz_cfg: &pancetta_config::network::QrzLogbookConfig,
    eqsl_cfg: &pancetta_config::network::EqslConfig,
    cqdx_cfg: &pancetta_config::network::CqdxConfig,
    our_callsign: &str,
) -> LogbookClients {
    let clublog = if clublog_cfg.enabled {
        // Fall back to the QSO's own call when no station call is configured.
        let callsign = if clublog_cfg.callsign.is_empty() {
            our_callsign.to_string()
        } else {
            clublog_cfg.callsign.clone()
        };
        // An empty configured key falls back to Pancetta's own compiled-in
        // ClubLog application key (empty on a source build) — resolved here,
        // at point of use, rather than baked into the config value itself,
        // so a rotated compiled key takes effect immediately with no config
        // file changes and a config resave never freezes in a stale one.
        let api_key = if clublog_cfg.api_key.is_empty() {
            pancetta_config::default_clublog_api_key()
        } else {
            clublog_cfg.api_key.clone()
        };
        info!(target: "qso.upload", "ClubLog per-QSO upload enabled");
        Some(Arc::new(pancetta_dx::ClubLogClient::new(
            clublog_cfg.email.clone(),
            clublog_cfg.password.clone(),
            callsign,
            api_key,
        )))
    } else {
        None
    };

    let qrz = if qrz_cfg.enabled {
        info!(target: "qso.upload", "QRZ Logbook per-QSO upload enabled");
        Some(Arc::new(pancetta_dx::QrzLogbookClient::new(
            qrz_cfg.api_key.clone(),
        )))
    } else {
        None
    };

    // eQSL.cc client. Opt-in: enabled + username/password (config validation
    // already rejects enabled-without-creds). QTH nickname is optional.
    let eqsl = if eqsl_cfg.enabled {
        let nick = if eqsl_cfg.qth_nickname.is_empty() {
            None
        } else {
            Some(eqsl_cfg.qth_nickname.clone())
        };
        info!(target: "qso.upload", "eQSL.cc per-QSO upload enabled");
        Some(Arc::new(pancetta_dx::EqslClient::new(
            eqsl_cfg.username.clone(),
            eqsl_cfg.password.clone(),
            nick,
        )))
    } else {
        None
    };

    // cqdx.io logbook client. Opt-in: enabled + a non-empty PAT token. A
    // malformed token (CqdxClient::new validation) is logged once at WARN and
    // simply disables the cqdx upload.
    let cqdx = if cqdx_cfg.enabled {
        match cqdx_cfg.token.as_ref().filter(|t| !t.is_empty()) {
            Some(token) => {
                match pancetta_cqdx::CqdxClient::new(cqdx_cfg.base_url.clone(), token.clone()) {
                    Ok(c) => {
                        info!(target: "qso.upload", "cqdx.io per-QSO logbook upload enabled");
                        Some(Arc::new(c))
                    }
                    Err(e) => {
                        // Token value is wrapped/redacted; the error never prints it.
                        warn!(
                            target: "qso.upload",
                            "cqdx.io upload disabled — client init failed: {}", e
                        );
                        None
                    }
                }
            }
            None => None,
        }
    } else {
        None
    };

    LogbookClients {
        clublog,
        qrz,
        eqsl,
        cqdx,
    }
}

/// The queue destinations a run uses. `--replay` never uploads, so it never
/// opens the queue.
pub(crate) fn outbox_destinations(
    clublog_enabled: bool,
    qrz_enabled: bool,
    eqsl_enabled: bool,
    cqdx_upload_enabled: bool,
    replay_mode: bool,
) -> Vec<UploadDestination> {
    if replay_mode {
        return Vec::new();
    }
    UploadDestination::ALL
        .into_iter()
        .filter(|d| match d {
            UploadDestination::ClubLog => clublog_enabled,
            UploadDestination::QrzLogbook => qrz_enabled,
            UploadDestination::Eqsl => eqsl_enabled,
            UploadDestination::Cqdx => cqdx_upload_enabled,
        })
        .collect()
}

/// The uploads for one completed QSO: the rendered ADIF record for ClubLog,
/// QRZ Logbook and eQSL (credentials are added at send time, never stored),
/// and the cqdx.io `QsoRecord` JSON when there is one.
pub(crate) fn outbox_batch(
    qso_id: Uuid,
    call: &str,
    record: &str,
    cqdx_payload: Option<String>,
    destinations: &[UploadDestination],
) -> Vec<NewUpload> {
    destinations
        .iter()
        .filter_map(|&d| {
            let payload = match d {
                UploadDestination::Cqdx => cqdx_payload.clone()?,
                _ => record.to_string(),
            };
            Some(NewUpload {
                qso_id,
                destination: d,
                call: call.to_string(),
                payload,
            })
        })
        .collect()
}

/// Worker timing that is not operator configuration.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OutboxTiming {
    /// The slow lane, after the quick retries are used up.
    pub slow_retry: Duration,
    /// Gap between consecutive sends to one destination.
    pub drain_spacing: Duration,
    /// How often the worker wakes up to look for due work.
    pub tick: Duration,
    /// After shutdown, how long the worker keeps taking new batches (a
    /// subscriber may already be handling a completed QSO) before it closes
    /// the channel. Ends early once every handle is dropped.
    pub shutdown_grace: Duration,
    /// First retry delay after a failed queue write; doubles per failure.
    pub persist_retry: Duration,
    /// Longest delay between retries of a failing queue write.
    pub persist_retry_cap: Duration,
}

impl OutboxTiming {
    pub(crate) const PRODUCTION: Self = Self {
        slow_retry: Duration::from_secs(300),
        drain_spacing: Duration::from_secs(1),
        tick: Duration::from_secs(1),
        shutdown_grace: Duration::from_secs(2),
        persist_retry: Duration::from_secs(1),
        persist_retry_cap: Duration::from_secs(60),
    };

    /// The wait before the next write attempt after `failures` failures.
    fn persist_backoff(&self, failures: u32) -> Duration {
        let shift = failures.saturating_sub(1).min(16);
        self.persist_retry
            .saturating_mul(1u32 << shift)
            .min(self.persist_retry_cap)
    }

    /// The slow-lane interval in whole minutes (at least 1), for display.
    pub(crate) fn slow_retry_mins(&self) -> u64 {
        self.slow_retry.as_secs().div_ceil(60).max(1)
    }
}

/// What the readiness strip shows about uploads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct UploadHealth {
    /// Destinations with something to report, in `UploadDestination::ALL` order.
    pub destinations: Vec<DestinationHealth>,
    /// New uploads refused this session because the queue was full.
    pub refused_full: usize,
}

/// One destination's upload state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DestinationHealth {
    pub destination: UploadDestination,
    pub pending: usize,
    pub held: usize,
    /// Why the service refused the account, when it did (session-long).
    pub login_rejected: Option<String>,
    /// The quick retries are used up and the destination is on the slow lane.
    pub retrying: bool,
    pub last_error: Option<String>,
    pub last_held_call: Option<String>,
    pub last_held_error: Option<String>,
}

/// The coordinator's handle on the worker. Cloned into each upload subscriber.
#[derive(Clone)]
pub(crate) struct UploadOutboxHandle {
    tx: mpsc::Sender<Vec<NewUpload>>,
    destinations: Arc<[UploadDestination]>,
}

impl UploadOutboxHandle {
    /// The destinations the worker delivers to.
    pub(crate) fn destinations(&self) -> &[UploadDestination] {
        &self.destinations
    }

    /// Queue a batch. Waits for room in the channel. Once the worker sees
    /// shutdown it closes the channel, then saves what is already in it; a
    /// batch sent after the close (or after the worker is gone) is not queued
    /// and is logged at warn.
    pub(crate) async fn enqueue(&self, batch: Vec<NewUpload>) {
        if batch.is_empty() {
            return;
        }
        if let Err(mpsc::error::SendError(batch)) = self.tx.send(batch).await {
            let call = batch.first().map_or("?", |u| u.call.as_str());
            warn!(
                target: "qso.upload",
                "upload queue is closed — {} upload(s) for {} not queued",
                batch.len(),
                call
            );
        }
    }

    /// Whether the worker has stopped taking batches (it closed its inbox).
    #[cfg(test)]
    pub(crate) fn worker_stopped(&self) -> bool {
        self.tx.is_closed()
    }

    /// Both handles talk to the same worker.
    #[cfg(test)]
    pub(crate) fn same_worker(&self, other: &Self) -> bool {
        self.tx.same_channel(&other.tx)
    }
}

/// Start the one process-wide upload worker.
pub(crate) fn start_upload_outbox<T: OutboxTransport>(
    path: PathBuf,
    transport: Arc<T>,
    schedule: RetrySchedule,
    timing: OutboxTiming,
    health: Arc<std::sync::Mutex<UploadHealth>>,
    bus: Option<MessageBus>,
    shutdown: Arc<AtomicBool>,
) -> (UploadOutboxHandle, tokio::task::JoinHandle<()>) {
    spawn_upload_outbox(
        path,
        OUTBOX_MAX_ENTRIES,
        transport,
        schedule,
        timing,
        health,
        bus,
        shutdown,
    )
}

/// How long coordinator shutdown waits for the worker beyond its grace
/// window: time to save the batches it already took and record attempts
/// that finish.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);

/// Coordinator shutdown: drop the coordinator's own handle (so the worker's
/// grace window can end once the subscribers are gone too) and wait for the
/// worker to drain and save, bounded by the grace window plus
/// [`SHUTDOWN_DRAIN`]. Awaiting it here keeps runtime teardown from
/// cancelling the worker mid-save. The shutdown flag must already be set.
pub(crate) async fn stop_upload_outbox(
    handle: Option<UploadOutboxHandle>,
    task: Option<tokio::task::JoinHandle<()>>,
    timing: OutboxTiming,
) {
    drop(handle);
    let Some(task) = task else {
        return;
    };
    let limit = timing.shutdown_grace + SHUTDOWN_DRAIN;
    match tokio::time::timeout(limit, task).await {
        Ok(Ok(())) => debug!(target: "qso.upload", "upload queue worker drained"),
        Ok(Err(e)) => warn!(target: "qso.upload", "upload queue worker ended abnormally: {e}"),
        Err(_) => warn!(
            target: "qso.upload",
            "upload queue worker still saving after {} — shutdown continues without it",
            format_wait(limit)
        ),
    }
}

/// [`start_upload_outbox`] with an explicit queue capacity.
// rationale: the same explicit inputs as `start_upload_outbox` plus the
// capacity tests shrink; bundling them into a struct would only move the
// fields.
#[allow(clippy::too_many_arguments)]
fn spawn_upload_outbox<T: OutboxTransport>(
    path: PathBuf,
    capacity: usize,
    transport: Arc<T>,
    schedule: RetrySchedule,
    timing: OutboxTiming,
    health: Arc<std::sync::Mutex<UploadHealth>>,
    bus: Option<MessageBus>,
    shutdown: Arc<AtomicBool>,
) -> (UploadOutboxHandle, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(256);
    let destinations: Arc<[UploadDestination]> = transport.destinations().into();
    let (results_tx, results_rx) = mpsc::unbounded_channel();
    let worker = Worker {
        path,
        capacity,
        destinations: destinations.to_vec(),
        transport,
        schedule,
        timing,
        health,
        bus,
        shutdown,
        lanes: HashMap::new(),
        refused_full: 0,
        persist_blocked: false,
        persist_error_reported: false,
        unsaved: HashSet::new(),
        persist_failures: 0,
        persist_retry_at: None,
        results_tx,
    };
    let task = tokio::spawn(worker.run(rx, results_rx));
    (UploadOutboxHandle { tx, destinations }, task)
}

type DeliveryResult = (Uuid, UploadDestination, Delivery);

struct Worker<T: OutboxTransport> {
    path: PathBuf,
    capacity: usize,
    destinations: Vec<UploadDestination>,
    transport: Arc<T>,
    schedule: RetrySchedule,
    timing: OutboxTiming,
    health: Arc<std::sync::Mutex<UploadHealth>>,
    bus: Option<MessageBus>,
    shutdown: Arc<AtomicBool>,
    lanes: HashMap<UploadDestination, Lane>,
    refused_full: usize,
    /// The unreadable queue file could not be moved aside: never overwrite it.
    persist_blocked: bool,
    persist_error_reported: bool,
    /// Entries added since the last successful queue write: never sent
    /// until a write succeeds.
    unsaved: HashSet<Uuid>,
    /// Consecutive failed queue writes.
    persist_failures: u32,
    /// When the next write retry is due; `Some` while the file on disk is
    /// behind the queue in memory.
    persist_retry_at: Option<Instant>,
    results_tx: mpsc::UnboundedSender<DeliveryResult>,
}

/// A value in [-1, 1] for the retry jitter, without a `rand` dependency.
fn jitter_unit() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let h = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    (h as f64 / u64::MAX as f64) * 2.0 - 1.0
}

/// `1s`, `2.5s`, `300ms`.
fn format_wait(d: Duration) -> String {
    if d >= Duration::from_secs(1) {
        let s = d.as_secs_f64();
        if (s - s.round()).abs() < 0.05 {
            format!("{}s", s.round() as u64)
        } else {
            format!("{s:.1}s")
        }
    } else {
        format!("{}ms", d.as_millis())
    }
}

/// The prefix of today's success/duplicate log lines.
fn log_prefix(d: UploadDestination) -> &'static str {
    match d {
        UploadDestination::QrzLogbook => "QRZ",
        other => other.label(),
    }
}

impl<T: OutboxTransport> Worker<T> {
    async fn run(
        mut self,
        mut inbox: mpsc::Receiver<Vec<NewUpload>>,
        mut results: mpsc::UnboundedReceiver<DeliveryResult>,
    ) {
        let (mut store, quarantine) =
            OutboxStore::open_with_capacity(self.path.clone(), self.capacity);
        if let Some(q) = quarantine {
            self.persist_blocked = q.rename_failed;
            warn!(
                target: "qso.upload",
                "Upload queue file was unreadable ({}) — moved to {}; starting with an empty queue",
                q.reason,
                q.moved_to.display()
            );
            let text = if q.rename_failed {
                format!(
                    "Upload queue file was unreadable and could not be moved aside ({}) — new uploads are held, not sent, this session (the QSOs are in qsos.adi)",
                    q.reason
                )
            } else {
                format!(
                    "Upload queue file was unreadable — moved to {}; starting with an empty queue",
                    q.moved_to.display()
                )
            };
            self.diag(DiagnosticLevel::Error, text, None).await;
        }
        let released = store.release_held();
        if !store.is_empty() {
            let mut parts = Vec::new();
            for d in UploadDestination::ALL {
                let n = store
                    .entries()
                    .iter()
                    .filter(|e| e.destination == d)
                    .count();
                if n > 0 {
                    parts.push(format!("{} {}", d.label(), n));
                }
            }
            info!(
                target: "qso.upload",
                "upload queue: {} waiting ({}), {} held from the last run released — sending now",
                store.len(),
                parts.join(", "),
                released
            );
        }
        if released > 0 {
            self.persist(&store).await;
        }
        self.publish(&store);

        let mut inbox_open = true;
        while !self.shutdown.load(Ordering::Acquire) {
            if self.persist_retry_at.is_some_and(|at| Instant::now() >= at) {
                self.persist(&store).await;
            }
            self.dispatch(&store);
            tokio::select! {
                batch = inbox.recv(), if inbox_open => match batch {
                    Some(batch) => self.on_batch(&mut store, batch).await,
                    None => inbox_open = false,
                },
                Some(result) = results.recv() => self.on_result(&mut store, result).await,
                _ = tokio::time::sleep(self.timing.tick) => {}
            }
        }
        // Shutdown: start no new attempts. A subscriber that was already
        // handling a completed QSO when the flag flipped enqueues just after
        // it, so keep saving batches (and recording attempts that finish) for
        // a short grace window, or until every handle is gone.
        let grace = tokio::time::sleep(self.timing.shutdown_grace);
        tokio::pin!(grace);
        while inbox_open {
            tokio::select! {
                batch = inbox.recv() => match batch {
                    Some(batch) => self.on_batch(&mut store, batch).await,
                    None => inbox_open = false,
                },
                Some(result) = results.recv() => self.on_result(&mut store, result).await,
                _ = &mut grace => break,
            }
        }
        // Then save every batch the channel already took. Closing first
        // makes a later `enqueue` fail at once instead of leaving its batch
        // unread in the channel.
        inbox.close();
        while let Some(batch) = inbox.recv().await {
            self.on_batch(&mut store, batch).await;
        }
        // A last try to save anything still only in memory.
        if self.persist_retry_at.is_some() {
            self.persist(&store).await;
        }
        debug!(target: "qso.upload", "upload queue worker stopped; {} uploads stay queued", store.len());
    }

    /// Start one delivery for every ready destination with something queued.
    fn dispatch(&mut self, store: &OutboxStore) {
        let now = Instant::now();
        for &d in &self.destinations {
            let lane = self.lanes.entry(d).or_default();
            if !lane.ready(now) {
                continue;
            }
            let Some(entry) = store.next_for(d) else {
                continue;
            };
            if self.unsaved.contains(&entry.id) {
                // Not on disk yet: held until a queue write succeeds.
                continue;
            }
            lane.started();
            let id = entry.id;
            let payload = entry.payload.clone();
            let transport = self.transport.clone();
            let tx = self.results_tx.clone();
            tokio::spawn(async move {
                let delivery = transport.deliver(d, payload).await;
                let _ = tx.send((id, d, delivery));
            });
        }
    }

    async fn on_batch(&mut self, store: &mut OutboxStore, batch: Vec<NewUpload>) {
        let before = store.len();
        let r = store.add(batch, Utc::now());
        if r.added > 0 {
            // Held until a write succeeds; with `persist_blocked` that is
            // never this session, so they wait for the next start.
            self.unsaved
                .extend(store.entries()[before..].iter().map(|e| e.id));
            self.persist(store).await;
        }
        if r.refused_full > 0 {
            self.refused_full += r.refused_full;
            warn!(
                target: "qso.upload",
                "upload queue full ({} entries) — {} new upload(s) not queued; the QSO is in qsos.adi",
                store.len(),
                r.refused_full
            );
        }
        self.publish(store);
    }

    async fn on_result(&mut self, store: &mut OutboxStore, (id, d, delivery): DeliveryResult) {
        let now = Instant::now();
        let entry = store.get(id).cloned();
        let call = entry
            .as_ref()
            .map(|e| e.call.clone())
            .unwrap_or_else(|| "?".to_string());
        let qso_id = entry.as_ref().map(|e| e.qso_id.to_string());
        let attempts = entry.as_ref().map(|e| e.attempts + 1).unwrap_or(1);
        let label = d.label();
        let lane = self.lanes.entry(d).or_default();
        let mut row: Option<(DiagnosticLevel, String)> = None;
        match delivery {
            Delivery::Accepted { duplicate } => {
                store.delivered(id);
                let recovered = lane.delivered(now, self.timing.drain_spacing);
                if duplicate {
                    info!(
                        target: "qso.upload",
                        "{}: QSO with {} already logged (duplicate, skipped)",
                        log_prefix(d),
                        call
                    );
                } else {
                    info!(target: "qso.upload", "{}: uploaded QSO with {}", log_prefix(d), call);
                }
                if recovered {
                    let left = store.counts(d).pending;
                    row = Some((
                        DiagnosticLevel::Info,
                        format!("{label} upload recovered — {left} queued uploads left to send"),
                    ));
                }
            }
            Delivery::Failed {
                class: UploadFailureClass::Transient,
                text,
            } => {
                store.record_failure(id, Utc::now(), &text);
                let was_slow = lane.is_retrying_slowly();
                match lane.failed(now, &self.schedule, jitter_unit()) {
                    RetryDelay::Burst(wait) => warn!(
                        target: "qso.upload",
                        "{}: upload failed for {} (attempt {}, retry in {}): {}",
                        label,
                        call,
                        attempts,
                        format_wait(wait),
                        text
                    ),
                    RetryDelay::Slow(_) => {
                        let mins = self.timing.slow_retry_mins();
                        warn!(
                            target: "qso.upload",
                            "{}: upload failed for {} (attempt {}, queued — retrying every {} min): {}",
                            label,
                            call,
                            attempts,
                            mins,
                            text
                        );
                        if !was_slow {
                            // The lane's count, not this entry's: the burst
                            // spans every entry of the destination.
                            let failures = lane.consecutive_failures();
                            let queued = store.counts(d).pending;
                            row = Some((
                                DiagnosticLevel::Warn,
                                format!(
                                    "{label} upload still failing after {failures} attempts — {queued} queued, retrying every {mins} min: {}",
                                    clip(&text)
                                ),
                            ));
                        }
                    }
                }
            }
            Delivery::Failed {
                class: UploadFailureClass::LoginRejected,
                text,
            } => {
                store.record_failure(id, Utc::now(), &text);
                lane.login_rejected(text.clone());
                warn!(
                    target: "qso.upload",
                    "{}: login rejected — holding queued uploads until pancetta restarts: {}",
                    label,
                    text
                );
                row = Some((
                    DiagnosticLevel::Warn,
                    format!(
                        "{label} login rejected — holding uploads until pancetta restarts: {}",
                        clip(&text)
                    ),
                ));
            }
            Delivery::Failed {
                class: UploadFailureClass::RecordRejected,
                text,
            } => {
                store.hold(id, Utc::now(), &text);
                lane.record_rejected(now, self.timing.drain_spacing);
                warn!(
                    target: "qso.upload",
                    "{}: upload for {} rejected — held until next start: {}",
                    label,
                    call,
                    text
                );
                row = Some((
                    DiagnosticLevel::Warn,
                    format!(
                        "{label} rejected upload — held until next start: {}",
                        clip(&text)
                    ),
                ));
            }
        }
        self.persist(store).await;
        self.publish(store);
        if let Some((level, text)) = row {
            self.diag(level, text, Some((qso_id, call))).await;
        }
    }

    /// Write the queue to disk (off the runtime threads) and wait for it.
    ///
    /// On success every held new upload becomes sendable. On failure they
    /// stay held and the write is retried with capped backoff.
    async fn persist(&mut self, store: &OutboxStore) {
        if self.persist_blocked {
            return;
        }
        let path = store.path().to_path_buf();
        let err = match store.to_bytes() {
            Ok(bytes) => {
                match tokio::task::spawn_blocking(move || OutboxStore::write_atomic(&path, &bytes))
                    .await
                {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(e.to_string()),
                    Err(e) => Some(e.to_string()),
                }
            }
            Err(e) => Some(e.to_string()),
        };
        let Some(err) = err else {
            let held = self.unsaved.len();
            self.unsaved.clear();
            self.persist_failures = 0;
            self.persist_retry_at = None;
            if self.persist_error_reported {
                self.persist_error_reported = false;
                info!(
                    target: "qso.upload",
                    "Upload queue saved to {} again — {} held upload(s) released",
                    store.path().display(),
                    held
                );
                self.diag(
                    DiagnosticLevel::Info,
                    format!("Upload queue saved again — {held} held upload(s) sending now"),
                    None,
                )
                .await;
            }
            return;
        };
        self.persist_failures = self.persist_failures.saturating_add(1);
        let wait = self.timing.persist_backoff(self.persist_failures);
        self.persist_retry_at = Some(Instant::now() + wait);
        warn!(
            target: "qso.upload",
            "Upload queue could not be saved to {} ({} upload(s) held, retry in {}): {}",
            store.path().display(),
            self.unsaved.len(),
            format_wait(wait),
            err
        );
        if !self.persist_error_reported {
            self.persist_error_reported = true;
            self.diag(
                DiagnosticLevel::Error,
                format!(
                    "Upload queue could not be saved: {} — new uploads are held, not sent, until it can be saved; retrying",
                    clip(&err)
                ),
                None,
            )
            .await;
        }
    }

    /// Publish the readiness view of the queue.
    fn publish(&self, store: &OutboxStore) {
        let mut h = UploadHealth {
            destinations: Vec::new(),
            refused_full: self.refused_full,
        };
        for d in UploadDestination::ALL {
            let c = store.counts(d);
            let lane = self.lanes.get(&d);
            let login_rejected = lane.and_then(|l| l.login_rejection().map(str::to_string));
            let retrying = lane.is_some_and(Lane::is_retrying_slowly);
            if c.pending == 0 && c.held == 0 && login_rejected.is_none() && !retrying {
                continue;
            }
            h.destinations.push(DestinationHealth {
                destination: d,
                pending: c.pending,
                held: c.held,
                login_rejected,
                retrying,
                last_error: c.last_error,
                last_held_call: c.last_held_call,
                last_held_error: c.last_held_error,
            });
        }
        *self.health.lock().unwrap_or_else(PoisonError::into_inner) = h;
    }

    async fn diag(
        &self,
        level: DiagnosticLevel,
        text: String,
        who: Option<(Option<String>, String)>,
    ) {
        let Some(bus) = self.bus.as_ref() else {
            return;
        };
        let (qso_id, call) = match who {
            Some((q, c)) => (q, Some(c)),
            None => (None, None),
        };
        emit_diagnostic_full(
            bus,
            ComponentId::Qso,
            "qso.upload",
            level,
            text,
            qso_id.as_deref(),
            call.as_deref(),
        )
        .await;
    }
}

/// Failure text cut for a Diagnostics row or readiness detail.
pub(crate) fn clip(text: &str) -> String {
    pancetta_dx::upload_outbox::truncate_chars(text, UPLOAD_TEXT_MAX_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pancetta_dx::upload_outbox::{EntryState, OUTBOX_FILE_NAME};
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use tokio::sync::Notify;

    type Call = (UploadDestination, String, Instant);

    #[derive(Default)]
    struct FakeTransport {
        enabled: Vec<UploadDestination>,
        script: Mutex<HashMap<UploadDestination, VecDeque<Delivery>>>,
        /// Per-payload scripts take precedence over the per-destination one.
        by_payload: Mutex<HashMap<String, Delivery>>,
        fallback: Mutex<HashMap<UploadDestination, Delivery>>,
        calls: Mutex<Vec<Call>>,
        block: Option<Arc<Notify>>,
    }

    impl FakeTransport {
        fn new(enabled: &[UploadDestination]) -> Self {
            Self {
                enabled: enabled.to_vec(),
                ..Self::default()
            }
        }
        fn script(self, d: UploadDestination, s: Vec<Delivery>) -> Self {
            self.script.lock().unwrap().insert(d, s.into());
            self
        }
        fn always(self, d: UploadDestination, r: Delivery) -> Self {
            self.fallback.lock().unwrap().insert(d, r);
            self
        }
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
        fn calls_to(&self, d: UploadDestination) -> usize {
            self.calls().iter().filter(|c| c.0 == d).count()
        }
    }

    impl OutboxTransport for FakeTransport {
        fn destinations(&self) -> Vec<UploadDestination> {
            self.enabled.clone()
        }

        async fn deliver(&self, d: UploadDestination, payload: String) -> Delivery {
            self.calls
                .lock()
                .unwrap()
                .push((d, payload.clone(), Instant::now()));
            if let Some(n) = &self.block {
                n.notified().await;
            }
            if let Some(r) = self.by_payload.lock().unwrap().get(&payload) {
                return r.clone();
            }
            if let Some(r) = self
                .script
                .lock()
                .unwrap()
                .get_mut(&d)
                .and_then(VecDeque::pop_front)
            {
                return r;
            }
            self.fallback
                .lock()
                .unwrap()
                .get(&d)
                .cloned()
                .unwrap_or(Delivery::Accepted { duplicate: false })
        }
    }

    fn transient() -> Delivery {
        Delivery::Failed {
            class: UploadFailureClass::Transient,
            text: "ClubLog upload failed: HTTP 503 Service Unavailable — ".to_string(),
        }
    }
    fn login() -> Delivery {
        Delivery::Failed {
            class: UploadFailureClass::LoginRejected,
            text: "QRZ Logbook insert rejected: invalid or insufficient API key (RESULT=AUTH)"
                .to_string(),
        }
    }
    fn record_rejected() -> Delivery {
        Delivery::Failed {
            class: UploadFailureClass::RecordRejected,
            text: "eQSL upload failed: HTTP 400 Bad Request — bad date".to_string(),
        }
    }
    fn ok() -> Delivery {
        Delivery::Accepted { duplicate: false }
    }

    const TIMING: OutboxTiming = OutboxTiming {
        slow_retry: Duration::from_millis(200),
        drain_spacing: Duration::from_millis(5),
        tick: Duration::from_millis(5),
        shutdown_grace: Duration::from_millis(100),
        persist_retry: Duration::from_millis(5),
        persist_retry_cap: Duration::from_millis(20),
    };

    fn schedule() -> RetrySchedule {
        RetrySchedule::new(3, 10, 40, 2.0, 0.0, TIMING.slow_retry)
    }

    struct Rig {
        _dir: tempfile::TempDir,
        path: PathBuf,
        transport: Arc<FakeTransport>,
        handle: UploadOutboxHandle,
        task: tokio::task::JoinHandle<()>,
        health: Arc<std::sync::Mutex<UploadHealth>>,
        shutdown: Arc<AtomicBool>,
    }

    impl Rig {
        fn entries(&self) -> Vec<pancetta_dx::upload_outbox::OutboxEntry> {
            OutboxStore::open(self.path.clone()).0.entries().to_vec()
        }
        fn health(&self) -> UploadHealth {
            self.health.lock().unwrap().clone()
        }
    }

    fn start_in(dir: tempfile::TempDir, transport: FakeTransport, bus: Option<MessageBus>) -> Rig {
        start_with_capacity(dir, OUTBOX_MAX_ENTRIES, transport, bus)
    }

    fn start_with_capacity(
        dir: tempfile::TempDir,
        capacity: usize,
        transport: FakeTransport,
        bus: Option<MessageBus>,
    ) -> Rig {
        let path = dir.path().join(OUTBOX_FILE_NAME);
        let transport = Arc::new(transport);
        let health = Arc::new(std::sync::Mutex::new(UploadHealth::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (handle, task) = spawn_upload_outbox(
            path.clone(),
            capacity,
            transport.clone(),
            schedule(),
            TIMING,
            health.clone(),
            bus,
            shutdown.clone(),
        );
        Rig {
            _dir: dir,
            path,
            transport,
            handle,
            task,
            health,
            shutdown,
        }
    }

    fn start(transport: FakeTransport) -> Rig {
        start_in(tempfile::tempdir().unwrap(), transport, None)
    }

    fn upload(d: UploadDestination, call: &str) -> NewUpload {
        NewUpload {
            qso_id: Uuid::new_v4(),
            destination: d,
            call: call.to_string(),
            payload: format!("<CALL:{}>{}<EOR>", call.len(), call),
        }
    }

    async fn wait_until(mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(Instant::now() < deadline, "condition not met within 5s");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    fn seed(path: &std::path::Path, uploads: Vec<NewUpload>, hold: usize) {
        let (mut store, _) = OutboxStore::open(path.to_path_buf());
        store.add(uploads, Utc::now());
        let ids: Vec<Uuid> = store.entries().iter().map(|e| e.id).collect();
        for id in ids.iter().take(hold) {
            store.hold(*id, Utc::now(), "HTTP 400");
        }
        OutboxStore::write_atomic(path, &store.to_bytes().unwrap()).unwrap();
    }

    use UploadDestination::{ClubLog, Cqdx, Eqsl, QrzLogbook};

    #[tokio::test]
    async fn transient_failures_retry_with_backoff_then_deliver() {
        let rig = start(
            FakeTransport::new(&[ClubLog]).script(ClubLog, vec![transient(), transient(), ok()]),
        );
        rig.handle.enqueue(vec![upload(ClubLog, "K1ABC")]).await;
        wait_until(|| rig.transport.calls_to(ClubLog) == 3).await;
        wait_until(|| rig.entries().is_empty() && rig.health().destinations.is_empty()).await;
        let calls = rig.transport.calls();
        assert!(calls[1].2 - calls[0].2 >= Duration::from_millis(10));
        assert!(calls[2].2 - calls[1].2 >= Duration::from_millis(20));
    }

    #[tokio::test]
    async fn exhausted_burst_moves_to_slow_lane_and_keeps_the_entry() {
        let rig = start(FakeTransport::new(&[ClubLog]).always(ClubLog, transient()));
        let started = Instant::now();
        rig.handle.enqueue(vec![upload(ClubLog, "K1ABC")]).await;
        wait_until(|| rig.transport.calls_to(ClubLog) == 3).await;
        wait_until(|| {
            rig.health()
                .destinations
                .first()
                .is_some_and(|h| h.retrying)
        })
        .await;
        if Instant::now() - started < Duration::from_millis(190) {
            assert_eq!(rig.transport.calls_to(ClubLog), 3);
        }
        let entries = rig.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].attempts, 3);
        assert!(entries[0]
            .last_error
            .as_deref()
            .unwrap()
            .contains("HTTP 503"));
        let h = rig.health();
        assert_eq!(h.destinations[0].destination, ClubLog);
        assert_eq!(h.destinations[0].pending, 1);
        wait_until(|| rig.transport.calls_to(ClubLog) == 4).await;
        let calls = rig.transport.calls();
        assert!(calls[3].2 - calls[2].2 >= Duration::from_millis(200));
    }

    #[tokio::test]
    async fn upload_is_on_disk_before_its_first_attempt() {
        let notify = Arc::new(Notify::new());
        let transport = FakeTransport {
            block: Some(notify.clone()),
            ..FakeTransport::new(&[ClubLog])
        };
        let rig = start(transport);
        rig.handle.enqueue(vec![upload(ClubLog, "K1ABC")]).await;
        wait_until(|| rig.transport.calls_to(ClubLog) == 1).await;
        let entries = rig.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].call, "K1ABC");
        notify.notify_one();
        wait_until(|| rig.entries().is_empty()).await;
    }

    #[tokio::test]
    async fn queued_entries_drain_at_startup_and_held_ones_are_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(OUTBOX_FILE_NAME);
        seed(
            &path,
            vec![
                upload(ClubLog, "A1A"),
                upload(ClubLog, "B1B"),
                upload(ClubLog, "C1C"),
            ],
            1,
        );
        let rig = start_in(dir, FakeTransport::new(&[ClubLog]), None);
        wait_until(|| rig.transport.calls_to(ClubLog) == 3).await;
        wait_until(|| rig.entries().is_empty()).await;
    }

    #[tokio::test]
    async fn login_rejection_stops_that_destination_for_the_session() {
        let rig = start(FakeTransport::new(&[ClubLog, QrzLogbook]).always(QrzLogbook, login()));
        for call in ["A1A", "B1B", "C1C"] {
            let qso = Uuid::new_v4();
            let mut batch = vec![upload(ClubLog, call), upload(QrzLogbook, call)];
            for u in &mut batch {
                u.qso_id = qso;
            }
            rig.handle.enqueue(batch).await;
        }
        wait_until(|| rig.transport.calls_to(ClubLog) == 3).await;
        wait_until(|| {
            rig.health()
                .destinations
                .iter()
                .any(|h| h.destination == QrzLogbook && h.login_rejected.is_some())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(rig.transport.calls_to(QrzLogbook), 1);
        let entries = rig.entries();
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|e| e.destination == QrzLogbook));
    }

    #[tokio::test]
    async fn record_rejection_holds_only_that_entry() {
        let rig =
            start(FakeTransport::new(&[Eqsl]).script(Eqsl, vec![record_rejected(), ok(), ok()]));
        rig.handle
            .enqueue(vec![
                upload(Eqsl, "A1A"),
                upload(Eqsl, "B1B"),
                upload(Eqsl, "C1C"),
            ])
            .await;
        wait_until(|| rig.transport.calls_to(Eqsl) == 3).await;
        wait_until(|| {
            rig.health()
                .destinations
                .first()
                .is_some_and(|h| h.held == 1 && h.pending == 0)
        })
        .await;
        let entries = rig.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].state, EntryState::Held);
        assert_eq!(entries[0].call, "A1A");
        let h = rig.health();
        assert_eq!(h.destinations[0].held, 1);
        assert_eq!(h.destinations[0].last_held_call.as_deref(), Some("A1A"));
    }

    #[tokio::test]
    async fn duplicate_counts_as_delivered() {
        let rig = start(
            FakeTransport::new(&[QrzLogbook])
                .always(QrzLogbook, Delivery::Accepted { duplicate: true }),
        );
        rig.handle.enqueue(vec![upload(QrzLogbook, "K1ABC")]).await;
        wait_until(|| rig.transport.calls_to(QrzLogbook) == 1).await;
        wait_until(|| rig.entries().is_empty()).await;
    }

    #[tokio::test]
    async fn a_poison_entry_does_not_block_the_rest() {
        let transport = FakeTransport::new(&[ClubLog]);
        let poison = upload(ClubLog, "A1A");
        transport
            .by_payload
            .lock()
            .unwrap()
            .insert(poison.payload.clone(), transient());
        let rig = start(transport);
        rig.handle.enqueue(vec![poison]).await;
        wait_until(|| rig.transport.calls_to(ClubLog) == 1).await;
        rig.handle.enqueue(vec![upload(ClubLog, "B1B")]).await;
        // B is sent while A is still failing (A is untried-last, B untried).
        wait_until(|| rig.transport.calls().iter().any(|c| c.1.contains("B1B"))).await;
        wait_until(|| {
            let e = rig.entries();
            e.len() == 1 && e[0].call == "A1A"
        })
        .await;
        assert!(rig.entries()[0].attempts >= 1);
    }

    #[tokio::test]
    async fn consecutive_sends_to_one_destination_are_spaced() {
        let rig = start(FakeTransport::new(&[ClubLog]));
        rig.handle
            .enqueue(vec![upload(ClubLog, "A1A"), upload(ClubLog, "B1B")])
            .await;
        wait_until(|| rig.transport.calls_to(ClubLog) == 2).await;
        let calls = rig.transport.calls();
        assert!(calls[1].2 - calls[0].2 >= TIMING.drain_spacing);
    }

    #[tokio::test]
    async fn entries_for_a_disabled_destination_are_kept_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(OUTBOX_FILE_NAME);
        seed(&path, vec![upload(ClubLog, "A1A")], 0);
        let rig = start_in(dir, FakeTransport::new(&[QrzLogbook]), None);
        rig.handle.enqueue(vec![upload(QrzLogbook, "B1B")]).await;
        wait_until(|| rig.transport.calls_to(QrzLogbook) == 1).await;
        wait_until(|| rig.entries().len() == 1).await;
        assert_eq!(rig.transport.calls_to(ClubLog), 0);
        assert_eq!(rig.entries()[0].destination, ClubLog);
    }

    #[tokio::test]
    async fn full_queue_refuses_and_reports_it() {
        let rig = start_with_capacity(
            tempfile::tempdir().unwrap(),
            1,
            FakeTransport::new(&[QrzLogbook]).always(QrzLogbook, transient()),
            None,
        );
        rig.handle.enqueue(vec![upload(QrzLogbook, "A1A")]).await;
        rig.handle.enqueue(vec![upload(QrzLogbook, "B1B")]).await;
        wait_until(|| rig.health().refused_full == 1).await;
        let entries = rig.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].call, "A1A");
    }

    #[tokio::test]
    async fn shutdown_stops_new_attempts_and_leaves_the_queue_on_disk() {
        let notify = Arc::new(Notify::new());
        let transport = FakeTransport {
            block: Some(notify.clone()),
            ..FakeTransport::new(&[ClubLog])
        };
        let rig = start(transport);
        rig.handle.enqueue(vec![upload(ClubLog, "K1ABC")]).await;
        wait_until(|| rig.transport.calls_to(ClubLog) == 1).await;
        rig.shutdown.store(true, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(1), rig.task)
            .await
            .expect("worker stops within 1s")
            .unwrap();
        let (store, _) = OutboxStore::open(rig.path.clone());
        assert_eq!(store.len(), 1);
    }

    #[tokio::test]
    async fn shutdown_saves_a_batch_already_in_the_channel() {
        // Current-thread runtime: the worker has not run yet, so the batch is
        // still in the channel when it first sees the shutdown flag.
        let rig = start(FakeTransport::new(&[ClubLog]));
        rig.shutdown.store(true, Ordering::Release);
        rig.handle.enqueue(vec![upload(ClubLog, "K1ABC")]).await;
        tokio::time::timeout(Duration::from_secs(1), rig.task)
            .await
            .expect("worker stops within 1s")
            .unwrap();
        let (store, _) = OutboxStore::open(rig.path.clone());
        assert_eq!(store.len(), 1);
        assert_eq!(store.entries()[0].call, "K1ABC");
        assert_eq!(rig.transport.calls_to(ClubLog), 0);
    }

    #[tokio::test]
    async fn shutdown_saves_a_batch_enqueued_just_after_the_flag() {
        // A subscriber still handling a completed QSO enqueues after the
        // worker has already seen shutdown: the batch is saved, not sent.
        let rig = start(FakeTransport::new(&[ClubLog]));
        rig.shutdown.store(true, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!rig.task.is_finished(), "worker waits out the grace window");
        rig.handle.enqueue(vec![upload(ClubLog, "K1ABC")]).await;
        tokio::time::timeout(Duration::from_secs(1), rig.task)
            .await
            .expect("worker stops within 1s")
            .unwrap();
        let (store, _) = OutboxStore::open(rig.path.clone());
        assert_eq!(store.len(), 1);
        assert_eq!(store.entries()[0].call, "K1ABC");
        assert_eq!(rig.transport.calls_to(ClubLog), 0);
    }

    #[tokio::test]
    async fn shutdown_ends_early_once_every_handle_is_dropped() {
        let rig = start(FakeTransport::new(&[ClubLog]));
        rig.shutdown.store(true, Ordering::Release);
        drop(rig.handle);
        tokio::time::timeout(Duration::from_millis(50), rig.task)
            .await
            .expect("worker stops before the grace window ends")
            .unwrap();
    }

    #[tokio::test]
    async fn unreadable_file_is_quarantined_and_the_worker_still_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(OUTBOX_FILE_NAME);
        std::fs::write(&path, b"{ broken").unwrap();
        let bus = MessageBus::new(16).unwrap();
        let (_s, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();
        let rig = start_in(dir, FakeTransport::new(&[ClubLog]), Some(bus));
        rig.handle.enqueue(vec![upload(ClubLog, "K1ABC")]).await;
        wait_until(|| rig.transport.calls_to(ClubLog) == 1).await;
        let quarantined = std::fs::read_dir(rig.path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("upload_outbox.json.corrupt-")
            });
        assert!(quarantined);
        let msg = receiver.try_recv().expect("diagnostic row");
        match msg.message_type {
            crate::message_bus::MessageType::DiagnosticEvent { level, text, .. } => {
                assert_eq!(level, DiagnosticLevel::Error);
                assert!(
                    text.starts_with("Upload queue file was unreadable"),
                    "{text}"
                );
            }
            other => panic!("expected DiagnosticEvent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn uploads_stay_held_when_the_unreadable_queue_cannot_be_moved_aside() {
        let bus = MessageBus::new(16).unwrap();
        let (_s, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();
        let (rig, q) = start_blockable(FakeTransport::new(&[ClubLog]), Some(bus));
        // Before the worker's first poll: the queue path's directory is a
        // file, so the open fails (not "missing") and so does the rename.
        std::fs::remove_dir(&q).unwrap();
        std::fs::write(&q, b"not a directory").unwrap();
        rig.handle.enqueue(vec![upload(ClubLog, "K1ABC")]).await;
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            rig.transport.calls_to(ClubLog),
            0,
            "sent with no durable copy"
        );
        let rows = diag_rows(&receiver);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].0, DiagnosticLevel::Error);
        assert!(
            rows[0].1.contains("could not be moved aside")
                && rows[0].1.contains("new uploads are held, not sent"),
            "{}",
            rows[0].1
        );
        // The unreadable "file" is never overwritten.
        assert_eq!(std::fs::read(&q).unwrap(), b"not a directory");
    }

    #[tokio::test]
    async fn stop_upload_outbox_waits_for_the_worker_to_save() {
        let rig = start(FakeTransport::new(&[ClubLog]));
        rig.shutdown.store(true, Ordering::Release);
        rig.handle.enqueue(vec![upload(ClubLog, "K1ABC")]).await;
        // Current-thread runtime: the worker has not run yet, so only
        // awaiting it gets the batch saved.
        stop_upload_outbox(Some(rig.handle), Some(rig.task), TIMING).await;
        let (store, _) = OutboxStore::open(rig.path.clone());
        assert_eq!(store.len(), 1);
        assert_eq!(rig.transport.calls_to(ClubLog), 0);
    }

    #[tokio::test]
    async fn login_rejection_emits_one_diagnostic_row() {
        let bus = MessageBus::new(16).unwrap();
        let (_s, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();
        let rig = start_in(
            tempfile::tempdir().unwrap(),
            FakeTransport::new(&[QrzLogbook]).always(QrzLogbook, login()),
            Some(bus),
        );
        rig.handle
            .enqueue(vec![
                upload(QrzLogbook, "A1A"),
                upload(QrzLogbook, "B1B"),
                upload(QrzLogbook, "C1C"),
            ])
            .await;
        wait_until(|| {
            rig.health()
                .destinations
                .iter()
                .any(|h| h.login_rejected.is_some())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut rows = Vec::new();
        while let Ok(msg) = receiver.try_recv() {
            if let crate::message_bus::MessageType::DiagnosticEvent {
                target,
                level,
                text,
                ..
            } = msg.message_type
            {
                rows.push((target, level, text));
            }
        }
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].0, "qso.upload");
        assert_eq!(rows[0].1, DiagnosticLevel::Warn);
        assert!(
            rows[0].2.starts_with("QRZ Logbook login rejected"),
            "{}",
            rows[0].2
        );
    }

    /// A worker whose queue file sits in the (empty) directory `<dir>/q/`,
    /// plus the `q` path. See [`block_writes`].
    fn start_blockable(transport: FakeTransport, bus: Option<MessageBus>) -> (Rig, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let q = dir.path().join("q");
        std::fs::create_dir(&q).unwrap();
        let path = q.join(OUTBOX_FILE_NAME);
        let transport = Arc::new(transport);
        let health = Arc::new(std::sync::Mutex::new(UploadHealth::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (handle, task) = spawn_upload_outbox(
            path.clone(),
            OUTBOX_MAX_ENTRIES,
            transport.clone(),
            schedule(),
            TIMING,
            health.clone(),
            bus,
            shutdown.clone(),
        );
        let rig = Rig {
            _dir: dir,
            path,
            transport,
            handle,
            task,
            health,
            shutdown,
        };
        (rig, q)
    }

    /// Swap the queue directory `q` for a regular file, so every queue write
    /// fails (even as root). Removing the file lets writes succeed again.
    ///
    /// Async so the worker runs first: `#[tokio::test]` is a current-thread
    /// runtime, and one yield lets the spawned worker open the queue (a
    /// synchronous step) before `q` stops being a directory; otherwise the
    /// open itself would fail and quarantine.
    async fn block_writes(q: &std::path::Path) {
        tokio::task::yield_now().await;
        std::fs::remove_dir(q).unwrap();
        std::fs::write(q, b"not a directory").unwrap();
    }

    fn diag_rows(
        receiver: &crossbeam_channel::Receiver<crate::message_bus::ComponentMessage>,
    ) -> Vec<(DiagnosticLevel, String)> {
        let mut rows = Vec::new();
        while let Ok(msg) = receiver.try_recv() {
            if let crate::message_bus::MessageType::DiagnosticEvent { level, text, .. } =
                msg.message_type
            {
                rows.push((level, text));
            }
        }
        rows
    }

    #[tokio::test]
    async fn failing_queue_write_holds_new_uploads_until_it_succeeds() {
        let bus = MessageBus::new(16).unwrap();
        let (_s, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();
        let notify = Arc::new(Notify::new());
        let transport = FakeTransport {
            block: Some(notify.clone()),
            ..FakeTransport::new(&[ClubLog])
        };
        let (rig, q) = start_blockable(transport, Some(bus));
        block_writes(&q).await;
        rig.handle.enqueue(vec![upload(ClubLog, "K1ABC")]).await;
        // Several retry periods pass: still nothing sent, nothing on disk.
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(rig.transport.calls_to(ClubLog), 0, "sent before saved");
        assert!(!rig.path.exists());
        let rows = diag_rows(&receiver);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].0, DiagnosticLevel::Error);
        assert!(
            rows[0].1.contains("new uploads are held, not sent"),
            "{}",
            rows[0].1
        );

        // The write recovers on a retry: the upload is on disk before its
        // first attempt, then delivered.
        std::fs::remove_file(&q).unwrap();
        wait_until(|| rig.transport.calls_to(ClubLog) == 1).await;
        let on_disk = rig.entries();
        assert_eq!(on_disk.len(), 1);
        assert_eq!(on_disk[0].call, "K1ABC");
        notify.notify_one();
        wait_until(|| rig.entries().is_empty()).await;
        assert_eq!(rig.transport.calls_to(ClubLog), 1);
        let rows = diag_rows(&receiver);
        assert!(
            rows.iter()
                .any(|(l, t)| *l == DiagnosticLevel::Info
                    && t.starts_with("Upload queue saved again")),
            "{rows:?}"
        );
    }

    #[test]
    fn queue_write_retries_back_off() {
        let timing = TIMING;
        assert_eq!(timing.persist_backoff(1), Duration::from_millis(5));
        assert_eq!(timing.persist_backoff(2), Duration::from_millis(10));
        assert_eq!(timing.persist_backoff(3), Duration::from_millis(20));
        assert_eq!(timing.persist_backoff(4), Duration::from_millis(20));
        assert_eq!(timing.persist_backoff(u32::MAX), Duration::from_millis(20));
        let p = OutboxTiming::PRODUCTION;
        assert_eq!(p.persist_backoff(1), Duration::from_secs(1));
        assert_eq!(p.persist_backoff(40), Duration::from_secs(60));
    }

    #[tokio::test]
    async fn held_upload_is_saved_at_shutdown_once_the_write_recovers() {
        let (rig, q) = start_blockable(FakeTransport::new(&[ClubLog]), None);
        block_writes(&q).await;
        rig.handle.enqueue(vec![upload(ClubLog, "K1ABC")]).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(rig.transport.calls_to(ClubLog), 0);
        rig.shutdown.store(true, Ordering::Release);
        std::fs::remove_file(&q).unwrap();
        drop(rig.handle);
        tokio::time::timeout(Duration::from_secs(1), rig.task)
            .await
            .expect("worker stops within 1s")
            .unwrap();
        let (store, _) = OutboxStore::open(rig.path.clone());
        assert_eq!(store.len(), 1, "held upload lost at shutdown");
        assert_eq!(store.entries()[0].call, "K1ABC");
        assert_eq!(rig.transport.calls_to(ClubLog), 0);
    }

    // ----- Coordinator helpers ----------------------------------------------

    #[test]
    fn outbox_destinations_follow_the_enable_flags() {
        assert_eq!(
            outbox_destinations(true, false, true, false, false),
            vec![ClubLog, Eqsl]
        );
        assert_eq!(
            outbox_destinations(false, true, false, false, false),
            vec![QrzLogbook]
        );
        assert!(outbox_destinations(false, false, false, false, false).is_empty());
    }

    #[test]
    fn replay_never_opens_the_outbox() {
        assert!(outbox_destinations(true, true, true, true, true).is_empty());
    }

    #[test]
    fn outbox_destinations_include_cqdx_when_the_token_is_set() {
        assert_eq!(
            outbox_destinations(false, false, false, true, false),
            vec![Cqdx]
        );
    }

    #[test]
    fn queued_payload_is_the_rendered_record_without_credentials() {
        let record = "<CALL:5>K1ABC <QSO_DATE:8>20261017 <EOR>";
        let qso = Uuid::new_v4();
        let batch = outbox_batch(
            qso,
            "K1ABC",
            record,
            None,
            &[ClubLog, QrzLogbook, Eqsl, Cqdx],
        );
        assert_eq!(batch.len(), 3, "no cqdx record → no cqdx upload");
        for u in &batch {
            assert_eq!(u.payload, record);
            assert_eq!(u.qso_id, qso);
            assert_eq!(u.call, "K1ABC");
            assert!(!u.payload.contains("EQSL_PSWD"));
            assert!(!u.payload.contains("EQSL_USER"));
        }
        let with_cqdx = outbox_batch(qso, "K1ABC", record, Some("{}".into()), &[Cqdx]);
        assert_eq!(with_cqdx.len(), 1);
        assert_eq!(with_cqdx[0].payload, "{}");
    }

    // ----- cqdx.io (Phase 5) -------------------------------------------------

    #[test]
    fn cqdx_errors_classify_like_the_http_table() {
        use pancetta_cqdx::CqdxError;
        let server = |status| CqdxError::Server {
            status,
            message: String::new(),
        };
        assert_eq!(
            classify_cqdx_error(&CqdxError::Unauthorized),
            UploadFailureClass::LoginRejected
        );
        assert_eq!(
            classify_cqdx_error(&server(503)),
            UploadFailureClass::Transient
        );
        assert_eq!(
            classify_cqdx_error(&server(422)),
            UploadFailureClass::RecordRejected
        );
        assert_eq!(
            classify_cqdx_error(&server(403)),
            UploadFailureClass::LoginRejected
        );
        assert_eq!(
            classify_cqdx_error(&CqdxError::ResponseTooLarge(1)),
            UploadFailureClass::RecordRejected
        );
        let json = serde_json::from_str::<u8>("x").unwrap_err();
        assert_eq!(
            classify_cqdx_error(&CqdxError::Json(json)),
            UploadFailureClass::Transient
        );
        for e in [
            CqdxError::InvalidToken("bad"),
            CqdxError::InvalidBaseUrl("x".into()),
            CqdxError::NotConfigured,
        ] {
            assert_eq!(classify_cqdx_error(&e), UploadFailureClass::LoginRejected);
        }
    }

    fn cqdx_record_json() -> String {
        let t = chrono::DateTime::parse_from_rfc3339("2026-10-17T14:02:11Z")
            .unwrap()
            .with_timezone(&Utc);
        serde_json::to_string(&pancetta_cqdx::QsoRecord {
            callsign: "K1ABC".into(),
            remote_grid: Some("FN42".into()),
            local_grid: Some("EM10".into()),
            frequency: 14_075_000,
            mode: "FT8".into(),
            rst_sent: Some("-10".into()),
            rst_received: Some("-05".into()),
            start_time: t,
            end_time: t,
        })
        .unwrap()
    }

    /// A raw HTTP server answering each connection with the next response.
    fn serve_sequence(responses: Vec<String>) -> (std::net::SocketAddr, Arc<Mutex<usize>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let count = Arc::new(Mutex::new(0usize));
        let seen = count.clone();
        std::thread::spawn(move || {
            for resp in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read headers, then the Content-Length body.
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                *seen.lock().unwrap() += 1;
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        (addr, count)
    }

    #[tokio::test]
    async fn cqdx_transient_then_created_is_delivered_by_the_real_client() {
        let body = r#"{"ok":true,"entity":"United States","band":"20m","duplicate":false}"#;
        let (addr, served) = serve_sequence(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
            format!(
                "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            ),
        ]);
        let client =
            pancetta_cqdx::CqdxClient::new(format!("http://{addr}"), "pat_0123456789abcdef".into())
                .unwrap();
        let clients = LogbookClients {
            clublog: None,
            qrz: None,
            eqsl: None,
            cqdx: Some(Arc::new(client)),
        };
        assert_eq!(clients.destinations(), vec![Cqdx]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(OUTBOX_FILE_NAME);
        let health = Arc::new(std::sync::Mutex::new(UploadHealth::default()));
        let handle = start_upload_outbox(
            path.clone(),
            Arc::new(clients),
            schedule(),
            TIMING,
            health,
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .0;
        handle
            .enqueue(vec![NewUpload {
                qso_id: Uuid::new_v4(),
                destination: Cqdx,
                call: "K1ABC".into(),
                payload: cqdx_record_json(),
            }])
            .await;
        wait_until(|| *served.lock().unwrap() == 2).await;
        wait_until(|| OutboxStore::open(path.clone()).0.is_empty()).await;
    }

    #[tokio::test]
    async fn cqdx_unreadable_queued_payload_is_held_not_dropped() {
        let client = pancetta_cqdx::CqdxClient::new(
            "http://127.0.0.1:9".to_string(),
            "pat_0123456789abcdef".into(),
        )
        .unwrap();
        let clients = LogbookClients {
            clublog: None,
            qrz: None,
            eqsl: None,
            cqdx: Some(Arc::new(client)),
        };
        match clients.deliver(Cqdx, "not json".into()).await {
            Delivery::Failed { class, text } => {
                assert_eq!(class, UploadFailureClass::RecordRejected);
                assert!(
                    text.starts_with("cqdx.io: queued record unreadable: "),
                    "{text}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(OUTBOX_FILE_NAME);
        let handle = start_upload_outbox(
            path.clone(),
            Arc::new(clients),
            schedule(),
            TIMING,
            Arc::new(std::sync::Mutex::new(UploadHealth::default())),
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .0;
        handle
            .enqueue(vec![NewUpload {
                qso_id: Uuid::new_v4(),
                destination: Cqdx,
                call: "K1ABC".into(),
                payload: "not json".into(),
            }])
            .await;
        wait_until(|| {
            OutboxStore::open(path.clone())
                .0
                .entries()
                .first()
                .is_some_and(|e| e.state == EntryState::Held)
        })
        .await;
    }
}
