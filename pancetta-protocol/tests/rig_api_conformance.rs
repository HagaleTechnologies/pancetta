//! Dispensa source: HagaleTechnologies/dispensa, contracts/rig/rig-api.v1.schema.json
//! at 869efaa7d565dcb15f81b4003643bd3da53012ca (v1.4), imported 2026-11-18.
//! Import: GitHub fetch_file(repository_full_name="hagaletechnologies/dispensa",
//! path="contracts/rig/rig-api.v1.schema.json", ref="869efaa") copied verbatim
//! to tests/fixtures/rig-api.v1.schema.json; local and upstream bytes compared.
use boon::{Compiler, Schemas};
use chrono::{TimeZone, Utc};
use pancetta_core::{slot::SlotParity, ResponseStep, TxPolicy};
use pancetta_protocol::{
    ClientCommand as C, ClientFrame, DecodedView, DxRow, Hello, PendingCall, Placement,
    PlacementSlice, QsoProgress, ServerEvent as E, ServerFrame, Spectrum, StateSnapshot, Welcome,
    PROTOCOL_VERSION,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

const RAW: &str = include_str!("fixtures/rig-api.v1.schema.json");
fn schema() -> Value {
    serde_json::from_str(RAW).unwrap()
}
fn stamp() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 7, 12, 0, 0).unwrap()
}
fn decoded(full: bool) -> DecodedView {
    DecodedView {
        timestamp: stamp(),
        frequency_hz: 14_075_200.0,
        snr: -8,
        delta_time: 0.2,
        delta_freq: 1.0,
        call_sign: full.then(|| "D2UY".into()),
        grid_square: full.then(|| "JI64".into()),
        message: "CQ D2UY JI64".into(),
        slot_parity: full.then_some(SlotParity::Even),
        is_directed_at_us: false,
        worked_before: false,
        needed: true,
        atno: false,
        priority_score: full.then_some(720),
    }
}
fn row(full: bool, source: &str) -> DxRow {
    DxRow {
        call_sign: "D2UY".into(),
        grid_square: full.then(|| "JI64".into()),
        frequency_hz: 14_075_200.0,
        mode: "FT8".into(),
        snr: -8,
        distance_km: full.then_some(1234.0),
        bearing: full.then_some(40.0),
        worked_before: false,
        needed: true,
        atno: false,
        priority: 720,
        entity_name: full.then(|| "Angola".into()),
        rarity_tier: full.then(|| "rare".into()),
        audio_offset_hz: full.then_some(1200),
        slot_parity: full.then_some(SlotParity::Odd),
        last_seen: stamp(),
        source: source.into(),
    }
}
fn qso(full: bool) -> QsoProgress {
    QsoProgress {
        qso_id: "q1".into(),
        their_callsign: "D2UY".into(),
        state: "calling".into(),
        frequency_hz: 14_075_200.0,
        tx_parity: full.then_some(SlotParity::Even),
        ladder_labels: vec!["grid".into()],
        ladder_ours: vec![true],
        ladder_index: 0,
        now_line: "CQ D2UY".into(),
        next_line: "D2UY K1ABC -08".into(),
        last_tx_text: full.then(|| "CQ D2UY".into()),
        last_rx_text: full.then(|| "CQ D2UY".into()),
        report_sent: full.then_some(-8),
        report_received: full.then_some(-10),
        dx_last_activity: full.then(|| "now".into()),
        started_at: stamp(),
    }
}
fn pending(full: bool) -> PendingCall {
    PendingCall {
        callsign: "D2UY".into(),
        dx_parity: full.then_some(SlotParity::Odd),
        waited_secs: 15,
    }
}
fn events() -> Vec<E> {
    vec![
        E::Decoded {
            decoded: decoded(true),
        },
        E::Decoded {
            decoded: decoded(false),
        },
        E::DxHunter {
            rows: vec![row(true, "local"), row(false, "network"), row(true, "both")],
        },
        E::ActiveQsos {
            qsos: vec![qso(true), qso(false)],
            pending: vec![pending(true), pending(false)],
        },
        E::Frequency {
            vfo: 0,
            frequency_hz: 14_074_000,
        },
        E::Split { tx_hz: 14_075_000 },
        E::Mode { mode: "FT8".into() },
        E::Mode { mode: "FT4".into() },
        E::Mode { mode: "FT2".into() },
        E::SignalStrength { db_over_s9: -4 },
        E::Spectrum {
            spectrum: Spectrum {
                bin_start_hz: 14_074_000.0,
                bin_width_hz: 3.125,
                mags_db: vec![-90.0, -80.0],
                seq: 1,
                timestamp: stamp(),
            },
        },
        E::Placement {
            placement: Placement {
                slices: vec![PlacementSlice {
                    offset_hz: 1200.0,
                    score: 0.8,
                    clear_first: true,
                    clear_second: false,
                }],
                openness: vec![1, 3],
                bin_hz: 3.125,
                range: (300.0, 3000.0),
            },
        },
        E::TxStatus { active: true },
        E::TxPolicy {
            policy: TxPolicy::Full,
        },
        E::TxPolicy {
            policy: TxPolicy::RespondOnly,
        },
        E::TxPolicy {
            policy: TxPolicy::Disabled,
        },
        E::ControlState {
            control_held_by_me: true,
            transmit_armed: false,
        },
        E::Status {
            component: "rig".into(),
            status: "ready".into(),
        },
        E::Error {
            component: "rig".into(),
            message: "offline".into(),
        },
    ]
}
fn commands() -> Vec<C> {
    let mut v = vec![
        C::SetFrequency {
            vfo: 0,
            frequency_hz: 14_074_000,
        },
        C::SetSplit {
            enabled: true,
            tx_frequency_hz: 14_075_000,
        },
        C::CallStation {
            callsign: "D2UY".into(),
            frequency_hz: 14_074_000,
            dx_parity: Some(SlotParity::Even),
        },
        C::CallStation {
            callsign: "D2UY".into(),
            frequency_hz: 14_074_000,
            dx_parity: None,
        },
        C::StartCq {
            frequency_offset_hz: 1500.0,
        },
        C::StopCq,
        C::TakeControl,
        C::ReleaseControl,
        C::SetTransmitArmed { armed: true },
    ];
    for step in [
        ResponseStep::Grid,
        ResponseStep::Report,
        ResponseStep::ReportAck,
        ResponseStep::Rr73,
        ResponseStep::SeventyThree,
    ] {
        v.push(C::AnswerCaller {
            callsign: "D2UY".into(),
            frequency_hz: 14_074_000,
            dx_parity: Some(SlotParity::Odd),
            step,
            snr: Some(-8.0),
        });
    }
    v.push(C::AnswerCaller {
        callsign: "D2UY".into(),
        frequency_hz: 14_074_000,
        dx_parity: None,
        step: ResponseStep::Grid,
        snr: None,
    });
    v
}
fn event_tag(e: &E) -> &'static str {
    match e {
        E::Decoded { .. } => "decoded",
        E::DxHunter { .. } => "dxHunter",
        E::ActiveQsos { .. } => "activeQsos",
        E::Frequency { .. } => "frequency",
        E::Split { .. } => "split",
        E::Mode { .. } => "mode",
        E::SignalStrength { .. } => "signalStrength",
        E::Spectrum { .. } => "spectrum",
        E::Placement { .. } => "placement",
        E::TxStatus { .. } => "txStatus",
        E::TxPolicy { .. } => "txPolicy",
        E::ControlState { .. } => "controlState",
        E::Status { .. } => "status",
        E::Error { .. } => "error",
    }
}
fn command_tag(c: &C) -> &'static str {
    match c {
        C::SetFrequency { .. } => "setFrequency",
        C::SetSplit { .. } => "setSplit",
        C::CallStation { .. } => "callStation",
        C::AnswerCaller { .. } => "answerCaller",
        C::StartCq { .. } => "startCq",
        C::StopCq => "stopCq",
        C::TakeControl => "takeControl",
        C::ReleaseControl => "releaseControl",
        C::SetTransmitArmed { .. } => "setTransmitArmed",
    }
}
fn branches<'a>(
    s: &'a Value,
    family: &str,
    tag_key: &str,
) -> Result<BTreeMap<String, &'a Value>, String> {
    let mut out = BTreeMap::new();
    for branch in s
        .pointer(&format!("/$defs/{family}/oneOf"))
        .and_then(Value::as_array)
        .ok_or("missing union")?
    {
        let tag = branch
            .pointer(&format!("/properties/{tag_key}/const"))
            .and_then(Value::as_str)
            .ok_or("unreadable discriminator")?;
        if out.insert(tag.into(), branch).is_some() {
            return Err(format!("duplicate discriminator {tag}"));
        }
    }
    Ok(out)
}
fn branch_index(s: &Value, family: &str, key: &str, tag: &str) -> usize {
    s.pointer(&format!("/$defs/{family}/oneOf"))
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .position(|b| {
            b.pointer(&format!("/properties/{key}/const"))
                .and_then(Value::as_str)
                == Some(tag)
        })
        .unwrap()
}
fn check_schema(
    s: &Value,
    family: &str,
    tag_key: &str,
    samples: &[(String, Value, Value)],
) -> Result<(), String> {
    let branches = branches(s, family, tag_key)?;
    let sample_tags: BTreeSet<_> = samples.iter().map(|(t, _, _)| t.clone()).collect();
    let schema_tags: BTreeSet<_> = branches.keys().cloned().collect();
    if sample_tags != schema_tags {
        return Err(format!(
            "{family} tag drift: missing samples {:?}; surplus samples {:?}",
            schema_tags.difference(&sample_tags).collect::<Vec<_>>(),
            sample_tags.difference(&schema_tags).collect::<Vec<_>>()
        ));
    }
    let id = s["$id"].as_str().ok_or("missing $id")?;
    let mut compiler = Compiler::new();
    compiler
        .add_resource(id, s.clone())
        .map_err(|e| e.to_string())?;
    let mut schemas = Schemas::new();
    let union = compiler
        .compile(&format!("{id}#/$defs/{family}"), &mut schemas)
        .map_err(|e| e.to_string())?;
    let frame_family = if family == "serverEvent" {
        "serverFrame"
    } else {
        "clientFrame"
    };
    let frame_schema = compiler
        .compile(&format!("{id}#/$defs/{frame_family}"), &mut schemas)
        .map_err(|e| e.to_string())?;
    for (tag, payload, frame) in samples {
        let branch = branches
            .get(tag)
            .ok_or_else(|| format!("missing branch {tag}"))?;
        let position = branch_index(s, family, tag_key, tag);
        let branch_schema = compiler
            .compile(
                &format!("{id}#/$defs/{family}/oneOf/{position}"),
                &mut schemas,
            )
            .map_err(|e| e.to_string())?;
        let mut errors = Vec::new();
        compare_shape(s, branch, payload, "", &mut errors);
        for (name, value, index) in [
            ("branch", payload, branch_schema),
            ("union", payload, union),
            ("frame", frame, frame_schema),
        ] {
            if let Err(e) = schemas.validate(value, index) {
                errors.push(format!("{name}: {e:#}"));
            }
        }
        if !errors.is_empty() {
            return Err(format!("{tag}: {}", errors.join("; ")));
        }
    }
    Ok(())
}
fn resolve<'a>(root: &'a Value, node: &'a Value) -> &'a Value {
    if let Some(r) = node.get("$ref").and_then(Value::as_str) {
        root.pointer(r.strip_prefix('#').unwrap()).unwrap()
    } else {
        node
    }
}
fn compare_shape(root: &Value, node: &Value, value: &Value, path: &str, errors: &mut Vec<String>) {
    let node = resolve(root, node);
    if let (Some(props), Some(obj)) = (
        node.get("properties").and_then(Value::as_object),
        value.as_object(),
    ) {
        let declared: BTreeSet<_> = props.keys().map(String::as_str).collect();
        let emitted: BTreeSet<_> = obj.keys().map(String::as_str).collect();
        let missing: Vec<_> = declared.difference(&emitted).collect();
        let extra: Vec<_> = emitted.difference(&declared).collect();
        if !missing.is_empty() || !extra.is_empty() {
            errors.push(format!(
                "{path}: declared but unemitted {missing:?}; emitted but undeclared {extra:?}"
            ));
        }
        for required in node
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !obj.contains_key(required) {
                errors.push(format!("{path}: missing required {required}"));
            }
        }
        for (k, v) in obj {
            if let Some(sub) = props.get(k) {
                compare_shape(root, sub, v, &format!("{path}/{k}"), errors);
            }
        }
    } else if let (Some(items), Some(array)) = (node.get("items"), value.as_array()) {
        for (i, v) in array.iter().enumerate() {
            compare_shape(root, items, v, &format!("{path}/{i}"), errors);
        }
    }
}
fn event_samples() -> Vec<(String, Value, Value)> {
    events()
        .into_iter()
        .map(|e| {
            let tag = event_tag(&e).to_string();
            let payload = serde_json::to_value(&e).unwrap();
            assert_eq!(payload["event"], tag);
            let frame = serde_json::to_value(ServerFrame::event(e.clone())).unwrap();
            assert_eq!(
                serde_json::from_value::<E>(payload.clone()).unwrap(),
                e,
                "{tag}"
            );
            assert_eq!(
                serde_json::from_value::<ServerFrame>(frame.clone()).unwrap(),
                ServerFrame::event(e),
                "{tag} frame"
            );
            (tag, payload, frame)
        })
        .collect()
}
fn command_samples() -> Vec<(String, Value, Value)> {
    commands()
        .into_iter()
        .map(|c| {
            let tag = command_tag(&c).to_string();
            let payload = serde_json::to_value(&c).unwrap();
            assert_eq!(payload["cmd"], tag);
            let frame = serde_json::to_value(ClientFrame::command(c.clone())).unwrap();
            assert_eq!(
                serde_json::from_value::<C>(payload.clone()).unwrap(),
                c,
                "{tag}"
            );
            assert_eq!(
                serde_json::from_value::<ClientFrame>(frame.clone()).unwrap(),
                ClientFrame::command(c),
                "{tag} frame"
            );
            (tag, payload, frame)
        })
        .collect()
}
#[test]
fn vendored_schema_is_v1_4() {
    assert_eq!(
        schema()["x-status"]
            .as_str()
            .unwrap()
            .split_whitespace()
            .next(),
        Some("v1.4")
    );
}
#[test]
fn server_events_conform_and_round_trip() {
    check_schema(&schema(), "serverEvent", "event", &event_samples()).unwrap();
}
#[test]
fn client_commands_conform_and_round_trip() {
    check_schema(&schema(), "clientCommand", "cmd", &command_samples()).unwrap();
}
#[test]
fn handshake_frames_conform_and_round_trip() {
    let s = schema();
    let id = s["$id"].as_str().unwrap();
    let mut c = Compiler::new();
    c.add_resource(id, s.clone()).unwrap();
    let mut schemas = Schemas::new();
    let client = c
        .compile(&format!("{id}#/$defs/clientFrame"), &mut schemas)
        .unwrap();
    let server = c
        .compile(&format!("{id}#/$defs/serverFrame"), &mut schemas)
        .unwrap();
    for token in [None, Some("fake-token".into())] {
        let frame = ClientFrame::hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "Panino".into(),
            client_version: "1.0".into(),
            gateway_token: token,
        });
        let v = serde_json::to_value(&frame).unwrap();
        schemas.validate(&v, client).unwrap();
        assert_eq!(serde_json::from_value::<ClientFrame>(v).unwrap(), frame);
    }
    let frame = ServerFrame::welcome(Welcome {
        protocol_version: PROTOCOL_VERSION,
        server_version: "0.9.6".into(),
        snapshot: StateSnapshot {
            frequency_hz: 14_074_000,
            split_tx_hz: 14_075_000,
            tx_policy: TxPolicy::Full,
            mode: "FT8".into(),
            dx_hunter: vec![row(true, "local")],
            active_qsos: vec![qso(true)],
            pending_calls: vec![pending(true)],
            recent_decodes: vec![decoded(true)],
        },
    });
    let v = serde_json::to_value(&frame).unwrap();
    schemas.validate(&v, server).unwrap();
    assert_eq!(serde_json::from_value::<ServerFrame>(v).unwrap(), frame);
}
fn rename(s: &mut Value, family: &str, key: &str, tag: &str, old: &str, new: &str, required: bool) {
    let b = s
        .pointer_mut(&format!("/$defs/{family}/oneOf"))
        .unwrap()
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|b| {
            b.pointer(&format!("/properties/{key}/const"))
                .and_then(Value::as_str)
                == Some(tag)
        })
        .unwrap();
    let props = b["properties"].as_object_mut().unwrap();
    let value = props.remove(old).unwrap();
    props.insert(new.into(), value);
    if required {
        for v in b["required"].as_array_mut().unwrap() {
            if v == old {
                *v = json!(new);
            }
        }
    }
}
#[test]
fn required_field_rename_reports_variant_and_fields() {
    let mut s = schema();
    check_schema(&s, "serverEvent", "event", &event_samples()).unwrap();
    rename(
        &mut s,
        "serverEvent",
        "event",
        "frequency",
        "frequencyHz",
        "freqHz",
        true,
    );
    let e = check_schema(&s, "serverEvent", "event", &event_samples()).unwrap_err();
    for part in ["frequency", "frequencyHz", "freqHz"] {
        assert!(e.contains(part), "{e}");
    }
}
#[test]
fn optional_field_rename_reports_variant_and_fields() {
    let mut s = schema();
    check_schema(&s, "clientCommand", "cmd", &command_samples()).unwrap();
    let branch = branches(&s, "clientCommand", "cmd").unwrap()["callStation"];
    assert!(!branch["required"]
        .as_array()
        .unwrap()
        .contains(&json!("dxParity")));
    rename(
        &mut s,
        "clientCommand",
        "cmd",
        "callStation",
        "dxParity",
        "slotParity",
        false,
    );
    let index = branch_index(&s, "clientCommand", "cmd", "callStation");
    let branch = s
        .pointer_mut(&format!("/$defs/clientCommand/oneOf/{index}"))
        .unwrap();
    branch["additionalProperties"] = json!(true);
    let id = s["$id"].as_str().unwrap();
    let mut compiler = Compiler::new();
    compiler.add_resource(id, s.clone()).unwrap();
    let mut schemas = Schemas::new();
    let index = compiler
        .compile(
            &format!("{id}#/$defs/clientCommand/oneOf/{index}"),
            &mut schemas,
        )
        .unwrap();
    let call = command_samples()
        .into_iter()
        .find(|(tag, _, _)| tag == "callStation")
        .unwrap();
    assert!(
        schemas.validate(&call.1, index).is_ok(),
        "Boon alone must accept an optional rename in an open branch"
    );
    let e = check_schema(&s, "clientCommand", "cmd", &command_samples()).unwrap_err();
    for part in ["callStation", "dxParity", "slotParity"] {
        assert!(e.contains(part), "{e}");
    }
}
#[test]
fn renamed_discriminator_reports_tag_drift() {
    let mut s = schema();
    check_schema(&s, "serverEvent", "event", &event_samples()).unwrap();
    let index = branch_index(&s, "serverEvent", "event", "frequency");
    let b = s
        .pointer_mut(&format!(
            "/$defs/serverEvent/oneOf/{index}/properties/event/const"
        ))
        .unwrap();
    assert_eq!(b, "frequency");
    *b = json!("dialFrequency");
    let e = check_schema(&s, "serverEvent", "event", &event_samples()).unwrap_err();
    assert!(
        e.contains("frequency") && e.contains("dialFrequency"),
        "{e}"
    );
}
