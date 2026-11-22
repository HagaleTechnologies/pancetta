//! N1MM Logger+ `<contactinfo>` UDP broadcast of logged QSOs (PAN-382).
//!
//! When `[logging] n1mm_udp` holds an IP address and port, one
//! `<contactinfo>` XML datagram is sent there for every `QsoEvent::QsoCompleted`.
//! The element set and order follow the N1MM+ "External UDP Broadcasts"
//! appendix (<https://n1mmwp.hamdocs.com/appendices/external-udp-broadcasts/>).
//!
//! The datagram is built from the same `AdifQso` record that goes into
//! `qsos.adi` (`AdifProcessor::qso_to_adif` with the station power and
//! attribution settings), so call, band, reports, grid, zone and comment are
//! never formatted a second way. `<power>` is always empty: in N1MM it is the
//! worked station's power, which Pancetta does not know; our own watts stay in
//! ADIF `TX_PWR` (PAN-444). `QsoCompleted` is emitted only once the
//! closing frame aired (PAN-280), so a QSO whose 73 never aired is never
//! broadcast.
//!
//! Lifetime: the emitter task ends when the QSO-event channel closes, never on
//! the shutdown flag (the PAN-232/PAN-274 rule), so a QSO that completes after
//! the flag is set is still broadcast while the process runs. Shutdown does
//! not wait for the task: a UDP send has nothing to flush. It never runs under
//! `--replay`.

use std::fmt;
use std::net::SocketAddr;

use pancetta_config::LoggingConfig;
use pancetta_qso::{AdifProcessor, AdifQso, QsoEvent, QsoMetadata, QsoRole};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// The `<app>` value. Consumers written for N1MM may filter on it.
pub(crate) const N1MM_APP: &str = "N1MM";

/// Why a completed QSO produced no `<contactinfo>` datagram.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ContactinfoSkip {
    /// The contra callsign is unknown.
    NoContraCallsign,
    /// The frequency is outside every known amateur band (for example, the
    /// rig dial is unknown and the event still carries the audio offset).
    FrequencyNotInBand { frequency_hz: f64 },
}

impl fmt::Display for ContactinfoSkip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContactinfoSkip::NoContraCallsign => write!(f, "no contra callsign"),
            ContactinfoSkip::FrequencyNotInBand { frequency_hz } => write!(
                f,
                "frequency {frequency_hz} Hz is not in a known amateur band (is the rig dial unknown?)"
            ),
        }
    }
}

/// The N1MM band token (MHz) for an RF frequency in Hz, or `None` when the
/// frequency is not finite, not positive, or outside every known band.
pub(crate) fn n1mm_band_token(frequency_hz: f64) -> Option<&'static str> {
    use pancetta_core::Band;
    if !frequency_hz.is_finite() || frequency_hz <= 0.0 {
        return None;
    }
    let band = Band::all().iter().find(|band| {
        let (low, high) = band.frequency_range();
        frequency_hz >= low as f64 && frequency_hz <= high as f64
    })?;
    Some(match band {
        Band::Band160m => "1.8",
        Band::Band80m => "3.5",
        Band::Band60m => "5.3",
        Band::Band40m => "7",
        Band::Band30m => "10",
        Band::Band20m => "14",
        Band::Band17m => "18",
        Band::Band15m => "21",
        Band::Band12m => "24",
        Band::Band10m => "28",
        Band::Band6m => "50",
        Band::Band2m => "144",
        Band::Band70cm => "432",
        Band::Custom(_, _) => return None,
    })
}

/// XML text for one element value: characters XML 1.0 cannot carry (C0
/// controls other than tab, LF and CR, plus U+FFFE and U+FFFF) are dropped,
/// then the five XML special characters are escaped.
fn xml_text(raw: &str) -> String {
    let allowed: String = raw
        .chars()
        .filter(|&c| {
            matches!(c, '\t' | '\n' | '\r')
                || ((c as u32) > 0x1F && c != '\u{FFFE}' && c != '\u{FFFF}')
        })
        .collect();
    quick_xml::escape::escape(allowed.as_str()).into_owned()
}

/// Build the `<contactinfo>` document for one logged QSO.
///
/// `adif` must be the record `qso_to_adif` produced for `metadata`; the
/// reports, grid, zone and comment come from it so the datagram matches
/// `qsos.adi`.
pub(crate) fn contactinfo_xml(
    metadata: &QsoMetadata,
    adif: &AdifQso,
) -> Result<String, ContactinfoSkip> {
    let call = metadata
        .their_callsign
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or(ContactinfoSkip::NoContraCallsign)?;
    let band = n1mm_band_token(metadata.frequency).ok_or(ContactinfoSkip::FrequencyNotInBand {
        frequency_hz: metadata.frequency,
    })?;
    // N1MM frequencies are in 10 Hz units.
    let freq_10hz = format!("{}", (metadata.frequency / 10.0).round() as u64);
    let timestamp = metadata
        .contact_started_at()
        .strftime("%Y-%m-%d %H:%M:%S")
        .to_string();
    let continent = pancetta_core::dxcc::continent_for_callsign(call)
        .map(|c| c.code())
        .unwrap_or("");
    // A stored CQZ tag wins over the inferred zone: `qso_to_adif` then leaves
    // `cqz` empty and writes the stored value from `additional_fields`.
    let zone = adif
        .cqz
        .map(u32::from)
        .or_else(|| {
            adif.additional_fields
                .get("CQZ")
                .and_then(|stored| stored.trim().parse::<u32>().ok())
        })
        .unwrap_or(0)
        .to_string();
    let is_run = if metadata.role == QsoRole::Cqer {
        "1"
    } else {
        "0"
    };
    let id = metadata.qso_id.simple().to_string();
    let ours = metadata.our_callsign.as_str();

    let elements: [(&str, &str); 45] = [
        ("app", N1MM_APP),
        ("contestname", "DX"),
        ("contestnr", "1"),
        ("timestamp", &timestamp),
        ("mycall", ours),
        ("band", band),
        ("rxfreq", &freq_10hz),
        ("txfreq", &freq_10hz),
        ("operator", ours),
        ("mode", &metadata.mode),
        ("call", call),
        ("countryprefix", ""),
        ("wpxprefix", ""),
        ("stationprefix", ours),
        ("continent", continent),
        ("snt", adif.rst_sent.as_deref().unwrap_or("")),
        ("sntnr", "0"),
        ("rcv", adif.rst_rcvd.as_deref().unwrap_or("")),
        ("rcvnr", "0"),
        ("gridsquare", adif.gridsquare.as_deref().unwrap_or("")),
        ("exchange1", ""),
        ("section", ""),
        ("comment", adif.comment.as_deref().unwrap_or("")),
        ("qth", ""),
        ("name", ""),
        // N1MM's `power` is the worked station's power, which Pancetta never
        // knows, so it stays empty (PAN-444). Our own watts go only to the
        // ADIF `TX_PWR` field in `qsos.adi`.
        ("power", ""),
        ("misctext", ""),
        ("zone", &zone),
        ("prec", ""),
        ("ck", "0"),
        ("ismultiplier1", "0"),
        ("ismultiplier2", "0"),
        ("ismultiplier3", "0"),
        ("points", "1"),
        ("radionr", "1"),
        ("run1run2", "1"),
        ("RoverLocation", ""),
        ("RadioInterfaced", "1"),
        ("NetworkedCompNr", "0"),
        ("IsOriginal", "True"),
        ("NetBiosName", ""),
        ("IsRunQSO", is_run),
        ("StationName", ""),
        ("ID", &id),
        ("IsClaimedQso", "1"),
    ];

    let mut xml = String::with_capacity(1280);
    xml.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<contactinfo>\n");
    for (name, value) in elements {
        xml.push_str("  <");
        xml.push_str(name);
        xml.push('>');
        xml.push_str(&xml_text(value));
        xml.push_str("</");
        xml.push_str(name);
        xml.push_str(">\n");
    }
    xml.push_str("</contactinfo>\n");
    Ok(xml)
}

/// The N1MM destination for this run: `None` under `--replay` (replayed QSOs
/// are not real contacts) or when `[logging] n1mm_udp` is absent or blank.
pub(crate) fn n1mm_udp_destination(logging: &LoggingConfig, replay: bool) -> Option<SocketAddr> {
    if replay {
        return None;
    }
    logging.n1mm_udp_destination()
}

/// Spawn the emitter: one `<contactinfo>` datagram per `QsoCompleted` on
/// `events`, sent to `destination`. The task ends when the event channel
/// closes. A failed send only warns; it never fails the QSO or its log.
pub(crate) fn start_n1mm_udp_emitter(
    destination: SocketAddr,
    station_power_watts: u32,
    attribution_comment: bool,
    mut events: broadcast::Receiver<QsoEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let bind_addr: SocketAddr = if destination.is_ipv4() {
            SocketAddr::from(([0, 0, 0, 0], 0))
        } else {
            SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0))
        };
        let socket = match tokio::net::UdpSocket::bind(bind_addr).await {
            Ok(socket) => socket,
            Err(e) => {
                warn!(
                    "N1MM UDP: failed to open socket ({e}); contactinfo broadcast disabled for this run"
                );
                return;
            }
        };
        if destination.is_ipv4() {
            if let Err(e) = socket.set_broadcast(true) {
                warn!("N1MM UDP: could not enable broadcast on the socket ({e})");
            }
        }
        let processor = AdifProcessor::new()
            .with_station_power_watts(station_power_watts)
            .with_attribution_comment(attribution_comment);

        loop {
            match events.recv().await {
                Ok(QsoEvent::QsoCompleted {
                    qso_id, metadata, ..
                }) => {
                    let adif = processor.qso_to_adif(&metadata, metadata.contest_info.as_ref());
                    match contactinfo_xml(&metadata, &adif) {
                        Ok(xml) => match socket.send_to(xml.as_bytes(), destination).await {
                            Ok(_) => debug!(
                                "N1MM UDP: sent contactinfo for {} ({} bytes) to {}",
                                adif.call,
                                xml.len(),
                                destination
                            ),
                            Err(e) => warn!(
                                "N1MM UDP: contactinfo send for QSO {qso_id} failed: {e}"
                            ),
                        },
                        Err(skip) => {
                            warn!("N1MM UDP: skipping contactinfo for QSO {qso_id}: {skip}")
                        }
                    }
                }
                Ok(_) => {}
                Err(RecvError::Lagged(n)) => warn!(
                    "N1MM UDP: QSO-event stream lagged by {n} events; those completions were not broadcast"
                ),
                Err(RecvError::Closed) => break,
            }
        }
    })
}

#[cfg(test)]
#[path = "n1mm_udp/tests/contactinfo_tests.rs"]
mod contactinfo_tests;

#[cfg(test)]
#[path = "n1mm_udp/tests/emitter_tests.rs"]
mod emitter_tests;
