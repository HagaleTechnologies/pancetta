//! PAN-89: station readiness — durable blockers, acknowledgeable warnings and
//! short-lived notices, kept apart from the transient status line.
//!
//! Everything here is derived from typed `App` state every frame, so no
//! status-line write (a decode, a `StatusUpdate`, a key echo) can erase an
//! item. The coordinator relay pushes a [`StationReadiness`] snapshot on its
//! 2 s tick (only on change); the rest comes from `PipelineHealth`,
//! `AudioOutputDefault`, `AudioInputFallback` and sticky error [`Alert`]s.
//!
//! Severity rules:
//! - **Blocker** (red): stays until the condition resolves; `!` can't hide it.
//! - **Warning** (yellow): stays until resolved or acknowledged with `!`. An
//!   ack lasts for the current episode only — once the condition clears the
//!   ack is pruned, so a recurrence shows again.
//! - **Notice** (cyan): short-lived "waiting on the rig" states.

use chrono::{DateTime, Utc};

use crate::app::{App, RigConnDisplay};

/// Most sticky error alerts kept at once; the oldest is evicted past this.
pub const ALERT_CAP: usize = 8;

/// One TX hard-mute cause, mirroring the coordinator's `tx_mute_causes`
/// (itself parity-tested against `tx_hard_mute_reason`). Declared in the
/// gate's priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TxMuteCause {
    RigRestarting,
    RigNotReady,
    RigDisconnected,
    AudioOutputDead,
    RigStatePending,
    RigCommandInFlight,
    ReceiveGainUnconfirmed,
    PolicyDisabled,
}

impl TxMuteCause {
    /// Every cause, in the gate's priority order.
    pub const ALL: [TxMuteCause; 8] = [
        TxMuteCause::RigRestarting,
        TxMuteCause::RigNotReady,
        TxMuteCause::RigDisconnected,
        TxMuteCause::AudioOutputDead,
        TxMuteCause::RigStatePending,
        TxMuteCause::RigCommandInFlight,
        TxMuteCause::ReceiveGainUnconfirmed,
        TxMuteCause::PolicyDisabled,
    ];

    /// A fault that holds TX until something is fixed (not just "waiting").
    fn is_blocking(self) -> bool {
        matches!(
            self,
            TxMuteCause::RigRestarting
                | TxMuteCause::RigDisconnected
                | TxMuteCause::AudioOutputDead
                | TxMuteCause::ReceiveGainUnconfirmed
        )
    }
}

/// The coordinator clock-skew monitor's state (PAN-114 monitor, PAN-89 adds
/// the states that used to collapse to "no warning").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClockState {
    /// No monitor runs (e.g. `--replay`).
    #[default]
    NotMonitored,
    /// First probe not answered yet.
    Pending,
    InSpec,
    Skewed,
    /// pool.ntp.org unreachable — the clock is unverified.
    Unreachable,
    /// A clock step invalidated the last verdict; waiting for the next probe.
    Rechecking,
}

/// What is wrong with one logbook-upload destination (PAN-57).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UploadIssueKind {
    /// The service refused the account; uploads are held until a restart.
    LoginRejected,
    /// The service refused some records; they are held until the next start.
    Rejected,
    /// Still failing after the quick retries; retried on the slow interval.
    Retrying { every_mins: u64 },
}

/// One upload warning for the readiness strip (PAN-57). Never a Blocker:
/// upload trouble never holds TX.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadIssue {
    /// Service name, e.g. "QRZ Logbook".
    pub service: String,
    /// Where its credentials live, e.g. "[network.qrz_logbook]".
    pub config_section: String,
    pub kind: UploadIssueKind,
    /// Uploads affected (held, or queued).
    pub count: usize,
    /// The latest failure text (≤ 160 chars, never a credential).
    pub last_error: String,
    /// The contacted station of the latest affected upload, when known.
    pub last_call: Option<String>,
}

/// PAN-93: a cqdx.io needed list as the strip sees it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NeedListHealth {
    /// Not configured, loading, loaded, or not offered by cqdx.io.
    #[default]
    NoWarning,
    /// No list has loaded this session. `retrying` is false when only a
    /// config fix and restart can help. `error` is ≤ 160 chars, never a credential.
    Unavailable { error: String, retrying: bool },
}

/// Coordinator → TUI readiness snapshot (relay, 2 s tick, pushed on change).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StationReadiness {
    /// Every active TX hard-mute cause, in the gate's priority order. Empty ⇔ gate open.
    pub tx_mute_causes: Vec<TxMuteCause>,
    /// Hamlib teardown could not confirm PTT off; cleared only when the rig
    /// accepts a later PTT-off.
    pub ptt_off_unconfirmed: bool,
    pub clock: ClockState,
    /// Supervisor degraded-components summary, `None` when nothing failed.
    pub degraded: Option<String>,
    pub rf_no_decode: bool,
    pub input_silent: bool,
    /// Per-destination logbook-upload warnings (PAN-57).
    pub upload_issues: Vec<UploadIssue>,
    /// New uploads refused this session because the queue was full.
    pub upload_queue_refused: usize,
    /// PAN-93: the cqdx.io needed-entity list.
    pub cqdx_dxcc_needs: NeedListHealth,
    /// PAN-93: the cqdx.io needed-grid list.
    pub cqdx_grid_needs: NeedListHealth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Blocker,
    Warning,
    Notice,
}

/// Identity of a readiness condition; acknowledgements are keyed by it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReadinessKey {
    PttOffUnconfirmed,
    RigRestarting,
    RigLinkLost,
    TxAudioDead,
    RxAudioDead,
    ClockSkewed,
    ClockUnverified,
    DecoderStub,
    TxOutputDefault,
    RxFallback,
    InputSilent,
    RfNoDecodes,
    /// Keyed by the summary text, so a changed summary shows again.
    Degraded(String),
    Alert(u64),
    RigNotReady,
    RigCmdQueued,
    RigCmdApplying,
    ReceiveGainUnconfirmed,
    /// A logbook-upload warning, keyed by service and kind (not count), so
    /// an ack survives a count change (PAN-57).
    Upload(String, UploadIssueKind),
    UploadQueueFull,
    /// The cqdx.io needed-entity list has not loaded (PAN-93). Carries no
    /// error text, so an ack survives a changed error.
    CqdxNeeds,
    /// Only the cqdx.io needed-grid list has not loaded (PAN-93).
    CqdxGridNeeds,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadinessItem {
    pub key: ReadinessKey,
    pub severity: Severity,
    /// Strip chip text (glyph included, no padding).
    pub label: String,
    /// Shift+S detail line.
    pub detail: String,
    pub acknowledged: bool,
}

impl ReadinessItem {
    /// Shown on the strip: everything except acknowledged items.
    pub fn is_visible(&self) -> bool {
        !self.acknowledged
    }

    /// `!` can acknowledge it.
    pub fn is_acknowledgeable(&self) -> bool {
        self.severity == Severity::Warning
    }
}

/// The strip's TX verdict chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Checking,
    Held,
    RxOnly,
    Wait,
    Open,
    Ready,
}

impl Verdict {
    /// Chip text (glyph included, no padding).
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Checking => "… CHECKING",
            Verdict::Held => "✗ TX HELD",
            Verdict::RxOnly => "■ RX ONLY",
            Verdict::Wait => "◌ TX WAIT",
            Verdict::Open => "⚠ TX OPEN",
            Verdict::Ready => "✓ TX READY",
        }
    }
}

/// A sticky error alert from `TuiMessage::Error` (no "resolved" signal, so it
/// stays until acknowledged).
#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub id: u64,
    pub text: String,
    pub count: u32,
    pub first_at: DateTime<Utc>,
    pub acknowledged: bool,
}

/// PAN-114 clock-skew text for a signed SNTP offset (positive = local clock
/// behind UTC = SLOW). Two decimals below 10 s, whole seconds from 10 s up
/// (a Pi booted without an RTC can be a day off).
pub fn clock_skew_label(offset_s: f64) -> String {
    let mag = offset_s.abs();
    let mag = if mag < 10.0 {
        format!("{mag:.2}s")
    } else {
        format!("{mag:.0}s")
    };
    let dir = if offset_s > 0.0 { "SLOW" } else { "FAST" };
    format!("⚠ CLOCK {mag} {dir}")
}

/// Shift+S wording for a signed clock offset, e.g. `1.20 s SLOW`.
pub fn clock_skew_detail(offset_s: f64) -> String {
    let mag = offset_s.abs();
    let mag = if mag < 10.0 {
        format!("{mag:.2} s")
    } else {
        format!("{mag:.0} s")
    };
    let dir = if offset_s > 0.0 { "SLOW" } else { "FAST" };
    format!("{mag} {dir}")
}

/// `""` for one, `"s"` otherwise.
fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// The strip item for one upload issue.
fn upload_item(i: &UploadIssue) -> ReadinessItem {
    let service_upper = i.service.to_uppercase();
    let n = i.count;
    let (label, detail) = match i.kind {
        UploadIssueKind::LoginRejected => (
            format!("⚠ {service_upper} LOGIN REJECTED"),
            format!(
                "{} login rejected — {n} upload{} held: {}. Fix {} and restart pancetta; held uploads retry at startup.",
                i.service,
                plural(n),
                i.last_error,
                i.config_section
            ),
        ),
        UploadIssueKind::Rejected => {
            let last = i
                .last_call
                .as_ref()
                .map(|c| format!("last {c}: "))
                .unwrap_or_default();
            let stays = if n == 1 {
                "the QSO stays"
            } else {
                "the QSOs stay"
            };
            (
                format!(
                    "⚠ {service_upper} REJECTED {n} QSO{}",
                    plural(n).to_uppercase()
                ),
                format!(
                    "{} rejected {n} QSO{} — {last}{}. Held until the next start; {stays} in qsos.adi.",
                    i.service,
                    plural(n),
                    i.last_error
                ),
            )
        }
        UploadIssueKind::Retrying { every_mins } => (
            format!("⚠ {service_upper} UPLOAD RETRYING · {n} QUEUED"),
            format!(
                "{} upload retrying — {n} queued, trying every {every_mins} min: {}. Queued uploads survive a restart.",
                i.service, i.last_error
            ),
        ),
    };
    ReadinessItem {
        key: ReadinessKey::Upload(i.service.clone(), i.kind),
        severity: Severity::Warning,
        label,
        detail,
        acknowledged: false,
    }
}

/// Shift+S wording for one cqdx.io needed list that has not loaded. `list`
/// is `entity` or `grid`; `effect` says what is missing until it loads.
fn cqdx_need_detail(list: &str, error: &str, retrying: bool, effect: &str) -> String {
    let error = error.trim_end_matches('.');
    let (action, until) = if retrying {
        ("pancetta keeps retrying.", "Until it loads")
    } else {
        (
            "Fix [network.cqdx] token and restart pancetta.",
            "Until then",
        )
    };
    format!("cqdx.io needed-{list} list not loaded: {error}. {action} {until}, {effect}")
}

/// PAN-93: at most one strip item for cqdx.io needed lists that have not
/// loaded. The grid chip shows only while the entity list is fine, so one
/// outage gives one chip. Always a Warning: it never changes the TX verdict.
fn cqdx_need_item(r: &StationReadiness) -> Option<ReadinessItem> {
    let grids_down = matches!(r.cqdx_grid_needs, NeedListHealth::Unavailable { .. });
    let (key, label, detail) = match (&r.cqdx_dxcc_needs, &r.cqdx_grid_needs) {
        (NeedListHealth::Unavailable { error, retrying }, _) => {
            let mut detail = cqdx_need_detail(
                "entity",
                error,
                *retrying,
                "no station shows the + needed marker and ranking guesses from your home and logged prefixes.",
            );
            if grids_down {
                detail.push_str(" Needed grids are unavailable too.");
            }
            (ReadinessKey::CqdxNeeds, "⚠ CQDX NEEDS UNKNOWN", detail)
        }
        (NeedListHealth::NoWarning, NeedListHealth::Unavailable { error, retrying }) => (
            ReadinessKey::CqdxGridNeeds,
            "⚠ CQDX GRID NEEDS UNKNOWN",
            cqdx_need_detail(
                "grid",
                error,
                *retrying,
                "needed grids add nothing to autonomous scoring.",
            ),
        ),
        (NeedListHealth::NoWarning, NeedListHealth::NoWarning) => return None,
    };
    Some(ReadinessItem {
        key,
        severity: Severity::Warning,
        label: label.to_string(),
        detail,
        acknowledged: false,
    })
}

fn item(key: ReadinessKey, severity: Severity, label: &str, detail: &str) -> ReadinessItem {
    ReadinessItem {
        key,
        severity,
        label: label.to_string(),
        detail: detail.to_string(),
        acknowledged: false,
    }
}

/// Blocker and Notice items derived from the snapshot's mute causes and the
/// PTT latch.
fn cause_items(
    r: &StationReadiness,
    blockers: &mut Vec<ReadinessItem>,
    notices: &mut Vec<ReadinessItem>,
) {
    if r.ptt_off_unconfirmed {
        blockers.push(item(
            ReadinessKey::PttOffUnconfirmed,
            Severity::Blocker,
            "⚠ PTT MAY STILL BE KEYED — check radio",
            "PTT may still be keyed — Hamlib teardown could not confirm PTT off. Check the radio; clears when the rig confirms PTT off.",
        ));
    }
    for cause in &r.tx_mute_causes {
        match cause {
            TxMuteCause::RigRestarting => blockers.push(item(
                ReadinessKey::RigRestarting,
                Severity::Blocker,
                "⚠ RIG CONTROL RESTARTING",
                "Rig control restarting — TX held until the new Hamlib session is up.",
            )),
            TxMuteCause::RigDisconnected => blockers.push(item(
                ReadinessKey::RigLinkLost,
                Severity::Blocker,
                "⚠ RIG CAT LINK LOST",
                "Rig CAT link lost — polls failing; pancetta keeps redialling rigctld.",
            )),
            TxMuteCause::AudioOutputDead => blockers.push(item(
                ReadinessKey::TxAudioDead,
                Severity::Blocker,
                "⚠ TX AUDIO OUTPUT DEAD — press d",
                "TX audio output dead — the output device stopped calling back; press d to pick a device.",
            )),
            TxMuteCause::RigNotReady => notices.push(item(
                ReadinessKey::RigNotReady,
                Severity::Notice,
                "◌ RIG CONTROL NOT READY",
                "Rig control not ready — TX held until the Hamlib command loop is up.",
            )),
            TxMuteCause::RigStatePending => notices.push(item(
                ReadinessKey::RigCmdQueued,
                Severity::Notice,
                "◌ RIG CMD QUEUED",
                "Rig command queued — frequency/split not yet delivered to the rig.",
            )),
            TxMuteCause::RigCommandInFlight => notices.push(item(
                ReadinessKey::RigCmdApplying,
                Severity::Notice,
                "◌ RIG APPLYING CMD",
                "Rig applying command — waiting for the rig to confirm frequency/split.",
            )),
            TxMuteCause::ReceiveGainUnconfirmed => blockers.push(item(
                ReadinessKey::ReceiveGainUnconfirmed,
                Severity::Blocker,
                "receive gain change is in progress or unconfirmed",
                "receive gain change is in progress or unconfirmed",
            )),
            // The verdict chip (`■ RX ONLY`) already says it; no item.
            TxMuteCause::PolicyDisabled => {}
        }
    }
}

/// Blockers and Warnings from the pipeline/audio/clock/supervisor state.
fn condition_items(
    app: &App,
    blockers: &mut Vec<ReadinessItem>,
    warnings: &mut Vec<ReadinessItem>,
) {
    let readiness = app.station_readiness.as_ref();
    if let Some(h) = app.pipeline_health.as_ref() {
        if !h.audio_alive {
            blockers.push(item(
                ReadinessKey::RxAudioDead,
                Severity::Blocker,
                "⚠ AUDIO INPUT DEAD — press d",
                "Audio input dead — no samples from the input device; press d.",
            ));
        }
        if let Some(offset) = h.clock_skew_warning_s {
            warnings.push(ReadinessItem {
                key: ReadinessKey::ClockSkewed,
                severity: Severity::Warning,
                label: clock_skew_label(offset),
                detail: format!(
                    "Clock {} vs pool.ntp.org — past FT8's 0.3 s margin; enable NTP.",
                    clock_skew_detail(offset)
                ),
                acknowledged: false,
            });
        }
    }
    if readiness.is_some_and(|r| r.clock == ClockState::Unreachable) {
        warnings.push(item(
            ReadinessKey::ClockUnverified,
            Severity::Warning,
            "⚠ CLOCK UNVERIFIED",
            "Clock unverified — pool.ntp.org unreachable; FT8 needs the clock within 0.3 s.",
        ));
    }
    if app
        .pipeline_health
        .as_ref()
        .is_some_and(|h| !h.ft8lib_available)
    {
        warnings.push(item(
            ReadinessKey::DecoderStub,
            Severity::Warning,
            "⚠ DECODER STUB",
            "Decoder stub — ft8_lib not compiled in.",
        ));
    }
    if app.tx_output_default {
        warnings.push(item(
            ReadinessKey::TxOutputDefault,
            Severity::Warning,
            "⚠ TX AUDIO → SYSTEM DEFAULT",
            "TX audio goes to the system default output, not the rig.",
        ));
    }
    if app.input_fallback {
        warnings.push(item(
            ReadinessKey::RxFallback,
            Severity::Warning,
            "⚠ RX ON FALLBACK DEVICE",
            "RX is capturing from a fallback device, not the configured input.",
        ));
    }
    if let Some(r) = readiness {
        if r.input_silent {
            warnings.push(item(
                ReadinessKey::InputSilent,
                Severity::Warning,
                "⚠ INPUT SILENT",
                "Input silent (RMS≈0) — check the input device, mic permission, and that nothing else holds the CODEC.",
            ));
        }
        if r.rf_no_decode {
            warnings.push(item(
                ReadinessKey::RfNoDecodes,
                Severity::Warning,
                "⚠ RF BUT NO DECODES",
                "RF present but no decodes — check mode and clock.",
            ));
        }
        if let Some(summary) = r.degraded.as_ref() {
            warnings.push(ReadinessItem {
                key: ReadinessKey::Degraded(summary.clone()),
                severity: Severity::Warning,
                label: format!("⚠ DEGRADED: {summary}"),
                detail: format!("Degraded — {summary}"),
                acknowledged: false,
            });
        }
        // PAN-57: upload warnings. Always Warnings, never Blockers, so they
        // can never change the TX verdict.
        warnings.extend(r.upload_issues.iter().map(upload_item));
        if r.upload_queue_refused > 0 {
            let n = r.upload_queue_refused;
            warnings.push(ReadinessItem {
                key: ReadinessKey::UploadQueueFull,
                severity: Severity::Warning,
                label: "⚠ UPLOAD QUEUE FULL".to_string(),
                detail: format!(
                    "Upload queue full — {n} new upload{} not queued this session; those QSOs are in qsos.adi. Resolve the failing service to drain the queue.",
                    plural(n)
                ),
                acknowledged: false,
            });
        }
        // PAN-93: a configured cqdx.io whose needed list has not loaded.
        warnings.extend(cqdx_need_item(r));
    }
    for warning in warnings.iter_mut() {
        warning.acknowledged = app.acknowledged_readiness.contains(&warning.key);
    }
}

/// Sticky error alerts, oldest first.
fn alert_items(app: &App) -> Vec<ReadinessItem> {
    app.alerts
        .iter()
        .map(|a| ReadinessItem {
            key: ReadinessKey::Alert(a.id),
            severity: Severity::Warning,
            label: if a.count > 1 {
                format!("⚠ {} ×{}", a.text, a.count)
            } else {
                format!("⚠ {}", a.text)
            },
            detail: format!(
                "{} (×{}, first {})",
                a.text,
                a.count,
                a.first_at.format("%H:%M:%S")
            ),
            acknowledged: a.acknowledged,
        })
        .collect()
}

/// Every readiness item, acknowledged ones included, in display order:
/// Blockers, then Warnings, then alerts, then Notices.
pub fn readiness_items(app: &App) -> Vec<ReadinessItem> {
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();
    let mut notices = Vec::new();
    if let Some(r) = app.station_readiness.as_ref() {
        cause_items(r, &mut blockers, &mut notices);
    }
    condition_items(app, &mut blockers, &mut warnings);
    let mut items = blockers;
    items.extend(warnings);
    items.extend(alert_items(app));
    items.extend(notices);
    items
}

/// The TX verdict (first match wins). `Held`/`RxOnly`/`Wait` show exactly
/// when the coordinator's TX gate would refuse PTT.
pub fn verdict(app: &App) -> Verdict {
    let Some(r) = app.station_readiness.as_ref() else {
        return Verdict::Checking;
    };
    if r.tx_mute_causes.iter().any(|c| c.is_blocking()) {
        Verdict::Held
    } else if r.tx_mute_causes.contains(&TxMuteCause::PolicyDisabled) {
        Verdict::RxOnly
    } else if !r.tx_mute_causes.is_empty() {
        Verdict::Wait
    } else if readiness_items(app)
        .iter()
        .any(|i| i.severity == Severity::Blocker)
    {
        Verdict::Open
    } else {
        Verdict::Ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::PipelineHealth;
    use crate::config::Config;

    async fn app() -> App {
        App::new(Config::default(), None).await.unwrap()
    }

    fn health(audio_alive: bool, clock_skew_warning_s: Option<f64>) -> PipelineHealth {
        PipelineHealth {
            audio_alive,
            dsp_windows: 10,
            last_rms: 0.1,
            ft8lib_available: true,
            total_decodes: 0,
            last_decode_elapsed_ms: 0,
            last_decode_budget_exhausted: false,
            tx_attempts: 0,
            tx_defers: 0,
            decode_panic_count: 0,
            wdt_panic_count: 0,
            clock_skew_warning_s,
        }
    }

    fn snapshot(causes: &[TxMuteCause]) -> StationReadiness {
        StationReadiness {
            tx_mute_causes: causes.to_vec(),
            ..StationReadiness::default()
        }
    }

    fn labels(app: &App) -> Vec<String> {
        readiness_items(app).into_iter().map(|i| i.label).collect()
    }

    fn visible_labels(app: &App) -> Vec<String> {
        readiness_items(app)
            .into_iter()
            .filter(ReadinessItem::is_visible)
            .map(|i| i.label)
            .collect()
    }

    #[tokio::test]
    async fn no_snapshot_yields_checking_verdict() {
        let app = app().await;
        assert_eq!(verdict(&app), Verdict::Checking);
    }

    #[tokio::test]
    async fn gate_open_without_blockers_is_tx_ready() {
        let mut app = app().await;
        app.station_readiness = Some(StationReadiness::default());
        app.pipeline_health = Some(health(true, None));
        assert_eq!(verdict(&app), Verdict::Ready);
        assert!(readiness_items(&app).is_empty());
    }

    #[tokio::test]
    async fn blocking_causes_are_tx_held_and_every_cause_is_listed() {
        let mut app = app().await;
        app.station_readiness = Some(snapshot(&[
            TxMuteCause::RigDisconnected,
            TxMuteCause::AudioOutputDead,
        ]));
        assert_eq!(verdict(&app), Verdict::Held);
        let labels = labels(&app);
        assert!(
            labels.contains(&"⚠ RIG CAT LINK LOST".to_string()),
            "{labels:?}"
        );
        assert!(
            labels.contains(&"⚠ TX AUDIO OUTPUT DEAD — press d".to_string()),
            "{labels:?}"
        );
    }

    #[tokio::test]
    async fn policy_disabled_is_rx_only() {
        let mut app = app().await;
        app.station_readiness = Some(snapshot(&[TxMuteCause::PolicyDisabled]));
        assert_eq!(verdict(&app), Verdict::RxOnly);
        assert!(readiness_items(&app).is_empty());
    }

    #[tokio::test]
    async fn receive_gain_transition_is_a_typed_tx_blocker() {
        let mut app = app().await;
        app.station_readiness = Some(snapshot(&[TxMuteCause::ReceiveGainUnconfirmed]));
        assert_eq!(verdict(&app), Verdict::Held);
        let items = readiness_items(&app);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].severity, Severity::Blocker);
        assert_eq!(
            items[0].label,
            "receive gain change is in progress or unconfirmed"
        );
        app.station_readiness = Some(StationReadiness::default());
        assert_eq!(verdict(&app), Verdict::Ready);
        assert!(readiness_items(&app).is_empty());
    }

    #[tokio::test]
    async fn waiting_causes_are_tx_wait_notices() {
        let mut app = app().await;
        app.station_readiness = Some(snapshot(&[
            TxMuteCause::RigNotReady,
            TxMuteCause::RigStatePending,
            TxMuteCause::RigCommandInFlight,
        ]));
        assert_eq!(verdict(&app), Verdict::Wait);
        let items = readiness_items(&app);
        assert!(items.iter().all(|i| i.severity == Severity::Notice));
        assert_eq!(
            labels(&app),
            vec![
                "◌ RIG CONTROL NOT READY",
                "◌ RIG CMD QUEUED",
                "◌ RIG APPLYING CMD"
            ]
        );
    }

    #[tokio::test]
    async fn ptt_unconfirmed_with_open_gate_is_tx_open() {
        let mut app = app().await;
        app.station_readiness = Some(StationReadiness {
            ptt_off_unconfirmed: true,
            ..StationReadiness::default()
        });
        assert_eq!(verdict(&app), Verdict::Open);
        assert_eq!(labels(&app), vec!["⚠ PTT MAY STILL BE KEYED — check radio"]);
    }

    #[tokio::test]
    async fn verdict_is_held_wait_or_rx_only_iff_causes_nonempty() {
        let mut app = app().await;
        for mask in 0u32..(1 << TxMuteCause::ALL.len()) {
            let causes: Vec<TxMuteCause> = TxMuteCause::ALL
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, c)| *c)
                .collect();
            app.station_readiness = Some(snapshot(&causes));
            let v = verdict(&app);
            let refuses = matches!(v, Verdict::Held | Verdict::RxOnly | Verdict::Wait);
            assert_eq!(refuses, !causes.is_empty(), "causes {causes:?} → {v:?}");
        }
    }

    #[tokio::test]
    async fn audio_dead_and_clock_skew_are_both_listed() {
        let mut app = app().await;
        app.pipeline_health = Some(health(false, Some(1.2)));
        let labels = labels(&app);
        assert!(
            labels.contains(&"⚠ AUDIO INPUT DEAD — press d".to_string()),
            "{labels:?}"
        );
        assert!(
            labels.contains(&"⚠ CLOCK 1.20s SLOW".to_string()),
            "{labels:?}"
        );
    }

    #[tokio::test]
    async fn clock_unverified_is_an_acknowledgeable_warning() {
        let mut app = app().await;
        app.station_readiness = Some(StationReadiness {
            clock: ClockState::Unreachable,
            ..StationReadiness::default()
        });
        let items = readiness_items(&app);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "⚠ CLOCK UNVERIFIED");
        assert_eq!(items[0].severity, Severity::Warning);
        assert_eq!(app.acknowledge_readiness(), 1);
        assert!(visible_labels(&app).is_empty());
        assert!(readiness_items(&app)[0].acknowledged);
    }

    #[tokio::test]
    async fn blockers_cannot_be_acknowledged() {
        let mut app = app().await;
        app.station_readiness = Some(StationReadiness {
            ptt_off_unconfirmed: true,
            tx_mute_causes: vec![TxMuteCause::RigDisconnected],
            ..StationReadiness::default()
        });
        assert_eq!(app.acknowledge_readiness(), 0);
        assert_eq!(visible_labels(&app).len(), 2);
    }

    #[tokio::test]
    async fn acknowledged_warning_hides_until_condition_clears_then_reappears() {
        let mut app = app().await;
        let unverified = StationReadiness {
            clock: ClockState::Unreachable,
            ..StationReadiness::default()
        };
        app.station_readiness = Some(unverified.clone());
        assert_eq!(app.acknowledge_readiness(), 1);
        assert!(visible_labels(&app).is_empty());

        // Still unverified on the next push: the ack holds.
        app.station_readiness = Some(unverified.clone());
        app.prune_acknowledged_readiness();
        assert!(visible_labels(&app).is_empty());

        // Clock verifies: the episode ends and the ack is forgotten.
        app.station_readiness = Some(StationReadiness {
            clock: ClockState::InSpec,
            ..StationReadiness::default()
        });
        app.prune_acknowledged_readiness();
        assert!(app.acknowledged_readiness.is_empty());

        // Unreachable again: shows again.
        app.station_readiness = Some(unverified);
        app.prune_acknowledged_readiness();
        assert_eq!(visible_labels(&app), vec!["⚠ CLOCK UNVERIFIED"]);
    }

    #[tokio::test]
    async fn degraded_ack_is_keyed_by_summary() {
        let mut app = app().await;
        app.station_readiness = Some(StationReadiness {
            degraded: Some("DxCluster: restarted too many times".to_string()),
            ..StationReadiness::default()
        });
        assert_eq!(app.acknowledge_readiness(), 1);
        assert!(visible_labels(&app).is_empty());

        app.station_readiness = Some(StationReadiness {
            degraded: Some("Hamlib: restarted too many times".to_string()),
            ..StationReadiness::default()
        });
        app.prune_acknowledged_readiness();
        assert_eq!(
            visible_labels(&app),
            vec!["⚠ DEGRADED: Hamlib: restarted too many times"]
        );
    }

    #[tokio::test]
    async fn alerts_dedupe_by_text_count_repeats_and_cap_at_eight() {
        let mut app = app().await;
        for _ in 0..3 {
            app.add_error_message(
                "config: [audio] input_device \"USB CODEC\" not found".to_string(),
            );
        }
        assert_eq!(app.alerts.len(), 1);
        assert_eq!(app.alerts[0].count, 3);
        assert_eq!(
            labels(&app),
            vec!["⚠ config: [audio] input_device \"USB CODEC\" not found ×3"]
        );

        for i in 0..10 {
            app.add_error_message(format!("error {i}"));
        }
        assert_eq!(app.alerts.len(), ALERT_CAP);
        assert_eq!(app.alerts.back().unwrap().text, "error 9");
        assert_eq!(app.alerts.front().unwrap().text, "error 2");
    }

    #[tokio::test]
    async fn acknowledge_marks_alerts_and_returns_count() {
        let mut app = app().await;
        app.add_error_message("Rig connect failed".to_string());
        app.add_error_message("Audio failed".to_string());
        app.station_readiness = Some(StationReadiness {
            clock: ClockState::Unreachable,
            ..StationReadiness::default()
        });
        assert_eq!(app.acknowledge_readiness(), 3);
        assert!(app.alerts.iter().all(|a| a.acknowledged));
        assert!(visible_labels(&app).is_empty());
        assert_eq!(app.acknowledge_readiness(), 0);

        // The same text after an ack is a new alert, not a repeat.
        app.add_error_message("Rig connect failed".to_string());
        assert_eq!(visible_labels(&app), vec!["⚠ Rig connect failed"]);
    }

    // ----- PAN-57: upload warnings. -----------------------------------------

    fn issue(service: &str, section: &str, kind: UploadIssueKind, count: usize) -> UploadIssue {
        UploadIssue {
            service: service.to_string(),
            config_section: section.to_string(),
            kind,
            count,
            last_error: "HTTP 503 Service Unavailable".to_string(),
            last_call: Some("K1ABC".to_string()),
        }
    }

    fn qrz_login(count: usize) -> UploadIssue {
        issue(
            "QRZ Logbook",
            "[network.qrz_logbook]",
            UploadIssueKind::LoginRejected,
            count,
        )
    }

    fn with_uploads(issues: Vec<UploadIssue>, refused: usize) -> StationReadiness {
        StationReadiness {
            upload_issues: issues,
            upload_queue_refused: refused,
            ..StationReadiness::default()
        }
    }

    #[tokio::test]
    async fn upload_login_rejected_is_an_acknowledgeable_warning() {
        let mut app = app().await;
        app.station_readiness = Some(with_uploads(vec![qrz_login(3)], 0));
        let items = readiness_items(&app);
        assert_eq!(items.len(), 1);
        let i = &items[0];
        assert_eq!(i.label, "⚠ QRZ LOGBOOK LOGIN REJECTED");
        assert_eq!(i.severity, Severity::Warning);
        assert!(i.is_acknowledgeable());
        assert!(
            i.detail
                .starts_with("QRZ Logbook login rejected — 3 uploads held: "),
            "{}",
            i.detail
        );
        assert!(i.detail.contains("Fix [network.qrz_logbook] and restart"));
    }

    #[tokio::test]
    async fn upload_rejected_label_counts_qsos() {
        let mut app = app().await;
        let clublog = |n| issue("ClubLog", "[network.clublog]", UploadIssueKind::Rejected, n);
        app.station_readiness = Some(with_uploads(vec![clublog(1)], 0));
        assert_eq!(labels(&app), vec!["⚠ CLUBLOG REJECTED 1 QSO"]);
        let detail = readiness_items(&app)[0].detail.clone();
        assert!(detail.contains("last K1ABC: "), "{detail}");
        assert!(detail.contains("the QSO stays in qsos.adi"), "{detail}");
        app.station_readiness = Some(with_uploads(vec![clublog(2)], 0));
        assert_eq!(labels(&app), vec!["⚠ CLUBLOG REJECTED 2 QSOS"]);
    }

    #[tokio::test]
    async fn upload_retrying_label_shows_the_queue() {
        let mut app = app().await;
        app.station_readiness = Some(with_uploads(
            vec![issue(
                "eQSL",
                "[network.eqsl]",
                UploadIssueKind::Retrying { every_mins: 5 },
                4,
            )],
            0,
        ));
        assert_eq!(labels(&app), vec!["⚠ EQSL UPLOAD RETRYING · 4 QUEUED"]);
        assert!(readiness_items(&app)[0]
            .detail
            .contains("trying every 5 min"));
    }

    #[tokio::test]
    async fn upload_queue_full_is_a_warning() {
        let mut app = app().await;
        app.station_readiness = Some(with_uploads(vec![], 2));
        let items = readiness_items(&app);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "⚠ UPLOAD QUEUE FULL");
        assert_eq!(items[0].severity, Severity::Warning);
    }

    #[tokio::test]
    async fn upload_warnings_never_change_the_tx_verdict() {
        let mut app = app().await;
        let all = vec![
            qrz_login(3),
            issue("ClubLog", "[network.clublog]", UploadIssueKind::Rejected, 1),
            issue(
                "eQSL",
                "[network.eqsl]",
                UploadIssueKind::Retrying { every_mins: 5 },
                4,
            ),
        ];
        app.station_readiness = Some(with_uploads(all.clone(), 1));
        assert_eq!(verdict(&app), Verdict::Ready);
        app.station_readiness = Some(StationReadiness {
            tx_mute_causes: vec![TxMuteCause::RigDisconnected],
            ..with_uploads(all, 1)
        });
        assert_eq!(verdict(&app), Verdict::Held);
        app.station_readiness = Some(snapshot(&[TxMuteCause::RigDisconnected]));
        assert_eq!(verdict(&app), Verdict::Held);
    }

    #[tokio::test]
    async fn upload_ack_survives_a_count_change_and_clears_when_resolved() {
        let mut app = app().await;
        app.station_readiness = Some(with_uploads(vec![qrz_login(3)], 0));
        assert_eq!(app.acknowledge_readiness(), 1);
        assert!(visible_labels(&app).is_empty());

        app.station_readiness = Some(with_uploads(vec![qrz_login(4)], 0));
        app.prune_acknowledged_readiness();
        assert!(visible_labels(&app).is_empty());

        app.station_readiness = Some(with_uploads(vec![], 0));
        app.prune_acknowledged_readiness();
        assert!(app.acknowledged_readiness.is_empty());

        app.station_readiness = Some(with_uploads(vec![qrz_login(1)], 0));
        app.prune_acknowledged_readiness();
        let items = readiness_items(&app);
        assert!(!items[0].acknowledged);
        assert_eq!(visible_labels(&app), vec!["⚠ QRZ LOGBOOK LOGIN REJECTED"]);
    }

    #[tokio::test]
    async fn upload_detail_lead_splits_for_the_ackd_marker() {
        let mut app = app().await;
        app.station_readiness = Some(with_uploads(
            vec![
                qrz_login(1),
                issue("ClubLog", "[network.clublog]", UploadIssueKind::Rejected, 1),
                issue(
                    "eQSL",
                    "[network.eqsl]",
                    UploadIssueKind::Retrying { every_mins: 5 },
                    1,
                ),
            ],
            1,
        ));
        let items = readiness_items(&app);
        assert_eq!(items.len(), 4);
        for i in items {
            assert!(i.detail.split_once(" — ").is_some(), "{}", i.detail);
        }
    }

    // ----- PAN-93: cqdx.io needed lists. ------------------------------------

    fn unavailable(error: &str, retrying: bool) -> NeedListHealth {
        NeedListHealth::Unavailable {
            error: error.to_string(),
            retrying,
        }
    }

    fn with_needs(dxcc: NeedListHealth, grids: NeedListHealth) -> StationReadiness {
        StationReadiness {
            cqdx_dxcc_needs: dxcc,
            cqdx_grid_needs: grids,
            ..StationReadiness::default()
        }
    }

    #[tokio::test]
    async fn cqdx_needs_unknown_is_an_acknowledgeable_warning() {
        let mut app = app().await;
        app.station_readiness = Some(with_needs(
            unavailable("HTTP request failed", true),
            NeedListHealth::NoWarning,
        ));
        assert_eq!(labels(&app), vec!["⚠ CQDX NEEDS UNKNOWN"]);
        let items = readiness_items(&app);
        let i = &items[0];
        assert_eq!(i.severity, Severity::Warning);
        assert!(i.is_acknowledgeable());
        assert_eq!(
            i.detail,
            "cqdx.io needed-entity list not loaded: HTTP request failed. pancetta keeps retrying. Until it loads, no station shows the + needed marker and ranking guesses from your home and logged prefixes."
        );
        assert!(app
            .station_readiness
            .as_ref()
            .is_some_and(|r| r.tx_mute_causes.is_empty()));
        assert_eq!(verdict(&app), Verdict::Ready);
    }

    #[tokio::test]
    async fn cqdx_needs_token_problem_says_fix_and_restart() {
        let mut app = app().await;
        app.station_readiness = Some(with_needs(
            unavailable("Authentication failed: invalid or expired PAT.", false),
            NeedListHealth::NoWarning,
        ));
        let detail = readiness_items(&app)[0].detail.clone();
        assert!(
            detail.contains("Fix [network.cqdx] token and restart pancetta."),
            "{detail}"
        );
        assert_eq!(
            detail,
            "cqdx.io needed-entity list not loaded: Authentication failed: invalid or expired PAT. Fix [network.cqdx] token and restart pancetta. Until then, no station shows the + needed marker and ranking guesses from your home and logged prefixes."
        );
        assert!(!detail.contains("keeps retrying"), "{detail}");
    }

    #[tokio::test]
    async fn cqdx_grid_chip_only_when_entity_list_is_fine() {
        let mut app = app().await;
        app.station_readiness = Some(with_needs(
            NeedListHealth::NoWarning,
            unavailable("Server error: 503", true),
        ));
        assert_eq!(labels(&app), vec!["⚠ CQDX GRID NEEDS UNKNOWN"]);
        assert_eq!(
            readiness_items(&app)[0].detail,
            "cqdx.io needed-grid list not loaded: Server error: 503. pancetta keeps retrying. Until it loads, needed grids add nothing to autonomous scoring."
        );
        app.station_readiness = Some(with_needs(
            NeedListHealth::NoWarning,
            unavailable("Authentication failed", false),
        ));
        assert_eq!(
            readiness_items(&app)[0].detail,
            "cqdx.io needed-grid list not loaded: Authentication failed. Fix [network.cqdx] token and restart pancetta. Until then, needed grids add nothing to autonomous scoring."
        );

        app.station_readiness = Some(with_needs(
            unavailable("HTTP request failed", true),
            unavailable("HTTP request failed", true),
        ));
        assert_eq!(labels(&app), vec!["⚠ CQDX NEEDS UNKNOWN"]);
        let detail = readiness_items(&app)[0].detail.clone();
        assert!(
            detail.ends_with("Needed grids are unavailable too."),
            "{detail}"
        );
    }

    #[tokio::test]
    async fn cqdx_needs_ack_survives_error_change_and_is_pruned_on_recovery() {
        let mut app = app().await;
        app.station_readiness = Some(with_needs(
            unavailable("HTTP request failed", true),
            NeedListHealth::NoWarning,
        ));
        assert_eq!(app.acknowledge_readiness(), 1);
        assert!(visible_labels(&app).is_empty());

        // A different error is the same outage: the ack holds.
        app.station_readiness = Some(with_needs(
            unavailable("Server error: 503", true),
            NeedListHealth::NoWarning,
        ));
        app.prune_acknowledged_readiness();
        assert!(visible_labels(&app).is_empty());
        assert!(readiness_items(&app)[0].acknowledged);

        // The list loads: the episode ends and the ack is forgotten.
        app.station_readiness = Some(with_needs(
            NeedListHealth::NoWarning,
            NeedListHealth::NoWarning,
        ));
        app.prune_acknowledged_readiness();
        assert!(app.acknowledged_readiness.is_empty());
        assert!(readiness_items(&app).is_empty());

        // A later outage shows again.
        app.station_readiness = Some(with_needs(
            unavailable("HTTP request failed", true),
            NeedListHealth::NoWarning,
        ));
        app.prune_acknowledged_readiness();
        assert_eq!(visible_labels(&app), vec!["⚠ CQDX NEEDS UNKNOWN"]);
    }
}
