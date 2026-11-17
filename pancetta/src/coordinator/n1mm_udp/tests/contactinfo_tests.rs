// PAN-382. The pure `<contactinfo>` builder: document shape, values, escaping,
// band tokens and skip reasons.
use super::*;
use chrono::{TimeZone, Utc};
use pancetta_qso::{GridSquares, SignalReports};
use quick_xml::events::Event;
use quick_xml::Reader;

/// The Design element list, in order.
pub(super) const ELEMENTS: [&str; 45] = [
    "app",
    "contestname",
    "contestnr",
    "timestamp",
    "mycall",
    "band",
    "rxfreq",
    "txfreq",
    "operator",
    "mode",
    "call",
    "countryprefix",
    "wpxprefix",
    "stationprefix",
    "continent",
    "snt",
    "sntnr",
    "rcv",
    "rcvnr",
    "gridsquare",
    "exchange1",
    "section",
    "comment",
    "qth",
    "name",
    "power",
    "misctext",
    "zone",
    "prec",
    "ck",
    "ismultiplier1",
    "ismultiplier2",
    "ismultiplier3",
    "points",
    "radionr",
    "run1run2",
    "RoverLocation",
    "RadioInterfaced",
    "NetworkedCompNr",
    "IsOriginal",
    "NetBiosName",
    "IsRunQSO",
    "StationName",
    "ID",
    "IsClaimedQso",
];

/// Every `QsoMetadata` field named, so a new field fails to compile here.
/// K5ARH works K1ABC on 14 074 200.4 Hz, FT8, reports -10/-8.
pub(super) fn metadata() -> QsoMetadata {
    let start = Utc.with_ymd_and_hms(2020, 10, 30, 12, 34, 40).unwrap();
    let end = Utc.with_ymd_and_hms(2020, 10, 30, 12, 34, 56).unwrap();
    QsoMetadata {
        qso_id: pancetta_qso::QsoId::new_v4(),
        our_callsign: "K5ARH".to_string(),
        their_callsign: Some("K1ABC".to_string()),
        frequency: 14_074_200.4,
        completed_rf_frequency_hz: None,
        mode: "FT8".to_string(),
        first_answer_at: None,
        start_time: start,
        end_time: Some(end),
        reports: SignalReports {
            sent: Some(-10),
            received: Some(-8),
        },
        grids: GridSquares {
            ours: Some("EM12".to_string()),
            theirs: Some("FN42".to_string()),
        },
        their_state: None,
        contest_info: None,
        tags: std::collections::HashMap::new(),
        adif_field_types: std::collections::HashMap::new(),
        notes: None,
        tx_parity: None,
        initiated_by: Default::default(),
        role: QsoRole::Caller,
        call_count: 0,
        first_call_at: None,
        last_call_at: None,
        progressed_this_cycle: false,
        stall_cycles: 0,
        last_known_good_offset_hz: None,
        advance_generation: 0,
        pre_switch_offset: None,
        hound: false,
        partner_freq: None,
        pending_freq_drift: None,
        hound_qsyed: false,
        remote_origin: false,
        remote_client_key_id: None,
        tx_parity_provisional: false,
    }
}

/// The parsed document: whether the XML declaration was seen, the names of
/// the root elements, and each child element with its unescaped text.
pub(super) struct Parsed {
    pub declaration_seen: bool,
    pub root_names: Vec<String>,
    pub elements: Vec<(String, String)>,
}

impl Parsed {
    pub fn value(&self, name: &str) -> &str {
        self.elements
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("element {name} missing"))
    }
}

/// Parse a `<contactinfo>` document; any reader error fails the test.
pub(super) fn parse(xml: &str) -> Parsed {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut parsed = Parsed {
        declaration_seen: false,
        root_names: Vec::new(),
        elements: Vec::new(),
    };
    let mut depth = 0usize;
    let mut current: Option<(String, String)> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Decl(_)) => parsed.declaration_seen = true,
            Ok(Event::Start(e)) => {
                let name = e.name().as_ref().to_string();
                if depth == 0 {
                    parsed.root_names.push(name);
                } else if depth == 1 {
                    current = Some((name, String::new()));
                } else {
                    panic!("unexpected nested element {name}");
                }
                depth += 1;
            }
            Ok(Event::Empty(e)) => {
                let name = e.name().as_ref().to_string();
                assert_eq!(depth, 1, "empty element {name} outside the root");
                parsed.elements.push((name, String::new()));
            }
            Ok(Event::Text(e)) => {
                if let Some((_, text)) = current.as_mut() {
                    text.push_str(&e.xml10_content());
                } else {
                    assert!(
                        e.chars().all(char::is_whitespace),
                        "text outside an element"
                    );
                }
            }
            Ok(Event::GeneralRef(e)) => {
                let name: &str = &e;
                let resolved = quick_xml::escape::resolve_predefined_entity(name)
                    .unwrap_or_else(|| panic!("unknown entity &{name};"));
                current
                    .as_mut()
                    .expect("entity outside an element")
                    .1
                    .push_str(resolved);
            }
            Ok(Event::End(_)) => {
                depth -= 1;
                if depth == 1 {
                    parsed
                        .elements
                        .push(current.take().expect("end without start"));
                }
            }
            Ok(Event::Eof) => break,
            Ok(other) => panic!("unexpected XML event {other:?}"),
            Err(e) => panic!("XML reader error at {}: {e}", reader.error_position()),
        }
    }
    assert_eq!(depth, 0, "unclosed elements");
    parsed
}

fn render(meta: &QsoMetadata, power: u32, attribution: bool) -> String {
    let adif = AdifProcessor::new()
        .with_station_power_watts(power)
        .with_attribution_comment(attribution)
        .qso_to_adif(meta, meta.contest_info.as_ref());
    contactinfo_xml(meta, &adif, power).expect("contactinfo builds")
}

#[test]
fn contactinfo_is_one_well_formed_xml_document() {
    let xml = render(&metadata(), 100, false);
    assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n"));
    let parsed = parse(&xml);
    assert!(parsed.declaration_seen);
    assert_eq!(parsed.root_names, vec!["contactinfo".to_string()]);
    let names: Vec<&str> = parsed.elements.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ELEMENTS.to_vec());
}

#[test]
fn contactinfo_carries_call_band_mode_time_reports_and_grid() {
    let meta = metadata();
    let parsed = parse(&render(&meta, 100, false));
    assert_eq!(parsed.value("app"), "N1MM");
    assert_eq!(parsed.value("call"), "K1ABC");
    assert_eq!(parsed.value("band"), "14");
    assert_eq!(parsed.value("rxfreq"), "1407420");
    assert_eq!(parsed.value("txfreq"), "1407420");
    assert_eq!(parsed.value("mode"), "FT8");
    assert_eq!(parsed.value("timestamp"), "2020-10-30 12:34:40");
    assert_eq!(parsed.value("snt"), "-10");
    assert_eq!(parsed.value("rcv"), "-08");
    assert_eq!(parsed.value("gridsquare"), "FN42");
    assert_eq!(parsed.value("mycall"), "K5ARH");
    assert_eq!(parsed.value("operator"), "K5ARH");
    assert_eq!(parsed.value("stationprefix"), "K5ARH");
    assert_eq!(parsed.value("continent"), "NA");
    assert_eq!(parsed.value("power"), "100");
    let id = meta.qso_id.simple().to_string();
    assert_eq!(id.len(), 32);
    assert_eq!(parsed.value("ID"), id);
}

#[test]
fn contactinfo_uses_the_first_answer_time() {
    let mut meta = metadata();
    meta.first_answer_at = Some(Utc.with_ymd_and_hms(2020, 10, 30, 12, 34, 47).unwrap());
    let parsed = parse(&render(&meta, 100, false));
    assert_eq!(parsed.value("timestamp"), "2020-10-30 12:34:47");
}

#[test]
fn contactinfo_marks_a_qso_we_called_cq_for_as_a_run_qso() {
    let mut meta = metadata();
    meta.role = QsoRole::Cqer;
    assert_eq!(parse(&render(&meta, 100, false)).value("IsRunQSO"), "1");
    meta.role = QsoRole::Caller;
    assert_eq!(parse(&render(&meta, 100, false)).value("IsRunQSO"), "0");
}

#[test]
fn missing_reports_grid_and_power_are_empty_elements() {
    let mut meta = metadata();
    meta.reports = SignalReports {
        sent: None,
        received: None,
    };
    meta.grids.theirs = None;
    let parsed = parse(&render(&meta, 0, false));
    for name in ["snt", "rcv", "gridsquare", "power"] {
        assert_eq!(parsed.value(name), "", "{name} must be empty");
    }
}

#[test]
fn contactinfo_escapes_every_text_value() {
    let mut meta = metadata();
    meta.notes = Some("a<b>&\"c\"'d'\u{7}".to_string());
    meta.hound = true;
    let adif = AdifProcessor::new()
        .with_station_power_watts(100)
        .with_attribution_comment(true)
        .qso_to_adif(&meta, None);
    let expected = adif.comment.clone().expect("comment").replace('\u{7}', "");
    let xml = contactinfo_xml(&meta, &adif, 100).expect("builds");
    let parsed = parse(&xml);
    assert_eq!(parsed.value("comment"), expected);
    assert!(parsed.value("comment").contains("Using cqdx.io & Pancetta"));
    assert!(parsed.value("comment").contains("a<b>&\"c\"'d'"));
    assert!(!parsed.value("comment").contains('\u{7}'));
    assert!(xml.contains("&amp;"));
    assert!(!xml.contains("<b>"));
    assert!(!xml.contains('\u{7}'));
}

#[test]
fn contactinfo_fits_one_unfragmented_datagram() {
    let mut meta = metadata();
    meta.notes = Some("n".repeat(60));
    let xml = render(&meta, 100, true);
    assert!(
        xml.len() <= 1472,
        "contactinfo is {} bytes, over one unfragmented datagram",
        xml.len()
    );
}

#[test]
fn band_token_for_every_known_band() {
    use pancetta_core::Band;
    let expected = [
        (Band::Band160m, "1.8"),
        (Band::Band80m, "3.5"),
        (Band::Band60m, "5.3"),
        (Band::Band40m, "7"),
        (Band::Band30m, "10"),
        (Band::Band20m, "14"),
        (Band::Band17m, "18"),
        (Band::Band15m, "21"),
        (Band::Band12m, "24"),
        (Band::Band10m, "28"),
        (Band::Band6m, "50"),
        (Band::Band2m, "144"),
        (Band::Band70cm, "432"),
    ];
    assert_eq!(expected.len(), Band::all().len());
    for (band, token) in expected {
        let (low, high) = band.frequency_range();
        let mid = (low + high) as f64 / 2.0;
        assert_eq!(n1mm_band_token(mid), Some(token), "{band:?} midpoint");
        assert_eq!(
            n1mm_band_token(low as f64),
            Some(token),
            "{band:?} low edge"
        );
        assert_eq!(
            n1mm_band_token(high as f64),
            Some(token),
            "{band:?} high edge"
        );
    }
}

#[test]
fn no_contactinfo_without_their_callsign() {
    let mut meta = metadata();
    meta.their_callsign = None;
    let adif = AdifProcessor::new().qso_to_adif(&meta, None);
    assert_eq!(
        contactinfo_xml(&meta, &adif, 100),
        Err(ContactinfoSkip::NoContraCallsign)
    );
}

#[test]
fn no_contactinfo_when_frequency_is_not_in_a_known_band() {
    for freq in [1500.0, 0.0, f64::NAN, 222_100_000.0] {
        let mut meta = metadata();
        meta.frequency = freq;
        let adif = AdifProcessor::new().qso_to_adif(&meta, None);
        match contactinfo_xml(&meta, &adif, 100) {
            Err(ContactinfoSkip::FrequencyNotInBand { .. }) => {}
            other => panic!("{freq}: expected FrequencyNotInBand, got {other:?}"),
        }
    }
}
