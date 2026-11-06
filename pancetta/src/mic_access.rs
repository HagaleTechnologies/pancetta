//! Embedded macOS microphone identity and the pre-audio authorization gate.
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub(crate) const BUNDLE_ID: &str = "net.hagale.pancetta";
pub(crate) const USAGE_DESCRIPTION: &str =
    "pancetta listens to your radio's USB audio so it can decode FT8 and FT4 signals.";

#[derive(Debug)]
pub(crate) struct EmbeddedIdentity {
    pub(crate) bundle_id: String,
    pub(crate) has_usage_description: bool,
}

#[cfg(target_os = "macos")]
pub(crate) fn embedded_identity() -> Option<EmbeddedIdentity> {
    use objc2_foundation::{ns_string, NSBundle, NSString};
    let bundle = NSBundle::mainBundle();
    let id = bundle.objectForInfoDictionaryKey(ns_string!("CFBundleIdentifier"))?;
    let id = id.downcast_ref::<NSString>()?.to_string();
    let usage = bundle.objectForInfoDictionaryKey(ns_string!("NSMicrophoneUsageDescription"));
    Some(EmbeddedIdentity {
        bundle_id: id,
        has_usage_description: usage
            .as_ref()
            .and_then(|v| v.downcast_ref::<NSString>())
            .is_some_and(|v| !v.is_empty()),
    })
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn embedded_identity() -> Option<EmbeddedIdentity> {
    None
}

pub(crate) fn identity_line(identity: Option<&EmbeddedIdentity>) -> String {
    match identity.filter(|v| v.has_usage_description) {
        Some(v) => format!("Microphone identity: {} (embedded Info.plist)", v.bundle_id),
        None => "Microphone identity: missing (no embedded Info.plist; macOS uses the launching app's approval)".into(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MicrophoneAuthorization {
    Authorized,
    Denied,
    NotDetermined,
    Restricted,
    Unknown,
}

#[cfg(target_os = "macos")]
pub(crate) fn microphone_authorization() -> MicrophoneAuthorization {
    use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
    // SAFETY: querying authorization neither opens capture nor requests a prompt.
    let status =
        unsafe { AVCaptureDevice::authorizationStatusForMediaType(AVMediaTypeAudio.unwrap()) };
    match status {
        AVAuthorizationStatus::Authorized => MicrophoneAuthorization::Authorized,
        AVAuthorizationStatus::Denied => MicrophoneAuthorization::Denied,
        AVAuthorizationStatus::NotDetermined => MicrophoneAuthorization::NotDetermined,
        AVAuthorizationStatus::Restricted => MicrophoneAuthorization::Restricted,
        _ => MicrophoneAuthorization::Unknown,
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn microphone_authorization() -> MicrophoneAuthorization {
    MicrophoneAuthorization::Unknown
}

#[cfg(target_os = "macos")]
fn request_microphone_access() {
    use objc2_av_foundation::{AVCaptureDevice, AVMediaTypeAudio};
    tracing::debug!(
        usage = USAGE_DESCRIPTION,
        "Requesting macOS microphone approval"
    );
    let callback = block2::RcBlock::new(|granted: objc2::runtime::Bool| {
        tracing::debug!(
            granted = granted.as_bool(),
            "macOS microphone permission response"
        );
    });
    // SAFETY: AVFoundation copies the block; it captures no borrowed state and may run on any queue.
    unsafe {
        AVCaptureDevice::requestAccessForMediaType_completionHandler(
            AVMediaTypeAudio.unwrap(),
            &callback,
        );
    }
}

#[cfg(not(target_os = "macos"))]
fn request_microphone_access() {}

const APPROVED: &str = "pancetta: microphone access approved; starting the station.";

fn approval_notice(
    state: MicrophoneAuthorization,
    user: Option<&str>,
    interactive: bool,
) -> Option<Vec<String>> {
    use MicrophoneAuthorization::*;
    let first = match state {
        NotDetermined | Denied => "pancetta: microphone access needs a one-time approval at the Mac, by local login or Screen Sharing.",
        Restricted => "pancetta: microphone access is restricted on this Mac by an administrator or device management profile, so macOS cannot ask for approval.",
        Authorized | Unknown => return None,
    };
    let detail = match state {
        NotDetermined => format!("macOS was asked to show its microphone prompt for pancetta. At the Mac, logged in as {}, click Allow.", user.unwrap_or("this user")),
        Denied => "macOS has microphone access for pancetta turned off. At the Mac, open System Settings > Privacy & Security > Microphone and turn on pancetta. If it is already on, see \"macOS microphone access\" in docs/RUNBOOK.md.".into(),
        Restricted => format!("Ask the Mac's administrator to allow microphone access for pancetta ({BUNDLE_ID})."),
        _ => unreachable!(),
    };
    let mut waiting = "The station is waiting and starts as soon as access is allowed.".to_string();
    if interactive {
        waiting.push_str(" Press Ctrl-C to quit.");
    }
    Some(vec![first.into(), detail, waiting])
}

fn reminder_line(state: MicrophoneAuthorization) -> String {
    let label = match state {
        MicrophoneAuthorization::NotDetermined => "not yet decided",
        MicrophoneAuthorization::Denied => "turned off",
        MicrophoneAuthorization::Restricted => "restricted",
        _ => "unknown",
    };
    format!("pancetta: still waiting for microphone approval at the Mac (macOS reports: {label}).")
}

pub(crate) fn gate_required(
    outcome: &crate::responsibility::Outcome,
    station_live_audio: bool,
) -> bool {
    station_live_audio && outcome.identity_is_own()
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GateResult {
    Proceed,
    Stopped,
}

async fn wait_for_approval(
    mut query: impl FnMut() -> MicrophoneAuthorization,
    mut request: impl FnMut(),
    mut emit: impl FnMut(&str),
    shutdown: &AtomicBool,
    poll: Duration,
    remind_after_polls: usize,
) -> GateResult {
    let user = std::env::var("USER").ok();
    let interactive = std::io::stderr().is_terminal();
    let mut previous = None;
    let mut requested = false;
    let mut waited = false;
    let mut polls = 0;
    loop {
        if shutdown.load(Ordering::Acquire) {
            return GateResult::Stopped;
        }
        let state = query();
        if state == MicrophoneAuthorization::Authorized {
            if waited {
                emit(APPROVED);
            }
            return GateResult::Proceed;
        }
        if state == MicrophoneAuthorization::Unknown {
            emit("pancetta: could not query macOS microphone authorization; continuing with the launcher's microphone approval.");
            return GateResult::Proceed;
        }
        if state == MicrophoneAuthorization::NotDetermined && !requested {
            request();
            requested = true;
        }
        if previous != Some(state) {
            for line in approval_notice(state, user.as_deref(), interactive).unwrap_or_default() {
                emit(&line);
            }
            previous = Some(state);
            polls = 0;
        } else {
            polls += 1;
            if polls >= remind_after_polls.max(1) {
                emit(&reminder_line(state));
                polls = 0;
            }
        }
        waited = true;
        tokio::time::sleep(poll).await;
    }
}

pub(crate) async fn wait_for_station_approval(shutdown: &AtomicBool, headless: bool) -> GateResult {
    wait_for_approval(
        microphone_authorization,
        request_microphone_access,
        |line| {
            if !headless {
                eprintln!("{line}");
            }
            if line == APPROVED {
                tracing::info!("{line}");
            } else {
                tracing::warn!("{line}");
            }
        },
        shutdown,
        Duration::from_secs(2),
        300,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    const INFO_PLIST: &str = include_str!("../macos/Info.plist");
    fn plist_string<'a>(doc: &'a str, key: &str) -> Option<&'a str> {
        doc.split_once(&format!("<key>{key}</key>"))?
            .1
            .split_once("<string>")?
            .1
            .split_once("</string>")
            .map(|v| v.0)
    }
    #[test]
    fn info_plist_declares_identity_and_usage_description() {
        assert_eq!(
            plist_string(INFO_PLIST, "CFBundleIdentifier"),
            Some(BUNDLE_ID)
        );
        assert_eq!(plist_string(INFO_PLIST, "CFBundleName"), Some("pancetta"));
        assert_eq!(
            plist_string(INFO_PLIST, "NSMicrophoneUsageDescription"),
            Some(USAGE_DESCRIPTION)
        );
    }
    #[test]
    fn bundle_identifier_matches_launch_agent_label() {
        assert_eq!(
            plist_string(
                include_str!("../../packaging/launchd/net.hagale.pancetta.plist"),
                "Label"
            ),
            Some(BUNDLE_ID)
        );
    }
    #[test]
    fn identity_line_texts() {
        assert_eq!(
            identity_line(Some(&EmbeddedIdentity {
                bundle_id: BUNDLE_ID.into(),
                has_usage_description: true
            })),
            "Microphone identity: net.hagale.pancetta (embedded Info.plist)"
        );
        assert_eq!(
            identity_line(None),
            identity_line(Some(&EmbeddedIdentity {
                bundle_id: BUNDLE_ID.into(),
                has_usage_description: false
            }))
        );
        assert!(identity_line(None).contains("missing (no embedded Info.plist;"));
    }
    #[test]
    fn approval_notice_texts() {
        use MicrophoneAuthorization::*;
        for state in [NotDetermined, Denied, Restricted] {
            let lines = approval_notice(state, Some("thagale"), true).unwrap();
            assert_eq!(lines.len(), 3);
            assert!(lines[2].ends_with("Press Ctrl-C to quit."));
            assert!(!approval_notice(state, None, false).unwrap()[2].contains("Ctrl-C"));
        }
        assert!(approval_notice(NotDetermined, None, false).unwrap()[1]
            .contains("logged in as this user"));
        assert!(
            approval_notice(NotDetermined, Some("thagale"), false).unwrap()[1]
                .contains("logged in as thagale")
        );
        assert_eq!(
            approval_notice(NotDetermined, None, false).unwrap()[0],
            approval_notice(Denied, None, false).unwrap()[0]
        );
        assert!(approval_notice(Restricted, None, false).unwrap()[0].contains("restricted"));
        assert!(approval_notice(Authorized, None, true).is_none());
        assert!(approval_notice(Unknown, None, true).is_none());
    }
    #[test]
    fn approval_notices_match_operator_instructions() {
        use MicrophoneAuthorization::*;
        let first = "pancetta: microphone access needs a one-time approval at the Mac, by local login or Screen Sharing.";
        let waiting = "The station is waiting and starts as soon as access is allowed.";
        assert_eq!(approval_notice(NotDetermined, Some("thagale"), true).unwrap(), vec![
            first.to_string(),
            "macOS was asked to show its microphone prompt for pancetta. At the Mac, logged in as thagale, click Allow.".to_string(),
            format!("{waiting} Press Ctrl-C to quit."),
        ]);
        assert_eq!(approval_notice(Denied, None, false).unwrap(), vec![
            first,
            "macOS has microphone access for pancetta turned off. At the Mac, open System Settings > Privacy & Security > Microphone and turn on pancetta. If it is already on, see \"macOS microphone access\" in docs/RUNBOOK.md.",
            waiting,
        ]);
        assert_eq!(approval_notice(Restricted, None, false).unwrap(), vec![
            "pancetta: microphone access is restricted on this Mac by an administrator or device management profile, so macOS cannot ask for approval.",
            "Ask the Mac's administrator to allow microphone access for pancetta (net.hagale.pancetta).",
            waiting,
        ]);
    }

    #[test]
    fn reminder_texts() {
        for (state, label) in [
            (MicrophoneAuthorization::Denied, "turned off"),
            (MicrophoneAuthorization::Restricted, "restricted"),
            (MicrophoneAuthorization::NotDetermined, "not yet decided"),
        ] {
            assert_eq!(reminder_line(state), format!("pancetta: still waiting for microphone approval at the Mac (macOS reports: {label})."));
        }
        assert_eq!(
            APPROVED,
            "pancetta: microphone access approved; starting the station."
        );
    }
    #[test]
    fn gate_required_truth_table() {
        use crate::responsibility::Outcome::*;
        let outcomes = [
            NotMacOs,
            NotNeeded,
            DisabledByEnv,
            MissingUsageDescription,
            SymbolsMissing,
            LookupFailed("error".into()),
            AppLauncher("/Terminal.app/Contents/MacOS/Terminal".into()),
            SelfResponsible,
            RelaunchedChild { parent: 1 },
            RelaunchedButNotResponsible {
                responsible: 1,
                path: None,
            },
            SpawnFailed(1),
        ];
        for outcome in outcomes {
            assert!(!gate_required(&outcome, false));
            assert_eq!(
                gate_required(&outcome, true),
                matches!(outcome, SelfResponsible | RelaunchedChild { .. })
            );
        }
    }
    async fn run_states(
        states: &[MicrophoneAuthorization],
        stop: bool,
    ) -> (GateResult, usize, Vec<String>) {
        let mut index = 0;
        let mut requests = 0;
        let mut lines = vec![];
        let shutdown = AtomicBool::new(false);
        let result = wait_for_approval(
            || {
                let state = states[index.min(states.len() - 1)];
                index += 1;
                if stop && index == 2 {
                    shutdown.store(true, Ordering::Release);
                }
                state
            },
            || requests += 1,
            |line| lines.push(line.to_string()),
            &shutdown,
            Duration::from_millis(1),
            3,
        )
        .await;
        (result, requests, lines)
    }
    #[tokio::test(flavor = "current_thread")]
    async fn gate_proceeds_immediately_when_authorized() {
        let (r, n, l) = run_states(&[MicrophoneAuthorization::Authorized], false).await;
        assert_eq!(r, GateResult::Proceed);
        assert_eq!(n, 0);
        assert!(l.is_empty());
    }
    #[tokio::test(flavor = "current_thread")]
    async fn gate_requests_once_and_waits_until_authorized() {
        use MicrophoneAuthorization::*;
        let (r, n, l) = run_states(&[NotDetermined, NotDetermined, Authorized], false).await;
        assert_eq!(r, GateResult::Proceed);
        assert_eq!(n, 1);
        assert_eq!(l.len(), 4);
        assert_eq!(l[3], APPROVED);
    }
    #[tokio::test(flavor = "current_thread")]
    async fn gate_reprints_notice_when_state_changes() {
        use MicrophoneAuthorization::*;
        let (_, n, l) = run_states(&[NotDetermined, Denied, Authorized], false).await;
        assert_eq!(n, 1);
        assert_eq!(l.len(), 7);
        assert!(l[4].contains("turned off"));
    }
    #[tokio::test(flavor = "current_thread")]
    async fn gate_reminds_after_the_configured_polls() {
        use MicrophoneAuthorization::*;
        let (_, _, l) = run_states(
            &[Denied, Denied, Denied, Denied, Denied, Denied, Authorized],
            false,
        )
        .await;
        assert_eq!(l.iter().filter(|s| s.contains("still waiting")).count(), 1);
    }
    #[tokio::test(flavor = "current_thread")]
    async fn gate_returns_stopped_on_shutdown() {
        let (r, _, l) = run_states(&[MicrophoneAuthorization::Denied], true).await;
        assert_eq!(r, GateResult::Stopped);
        assert!(!l.iter().any(|s| s == APPROVED));
    }
    #[tokio::test(flavor = "current_thread")]
    async fn gate_does_not_block_on_unknown_status() {
        let (r, n, l) = run_states(&[MicrophoneAuthorization::Unknown], false).await;
        assert_eq!(r, GateResult::Proceed);
        assert_eq!(n, 0);
        assert_eq!(l.len(), 1);
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn embedded_identity_is_readable_from_the_running_binary() {
        let identity = embedded_identity().unwrap();
        assert_eq!(identity.bundle_id, BUNDLE_ID);
        assert!(identity.has_usage_description);
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn authorization_query_returns_without_requesting() {
        let _ = microphone_authorization();
    }
}
