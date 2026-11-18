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
fn decoded(slot_parity: Option<SlotParity>) -> DecodedView {
    let full = slot_parity.is_some();
    DecodedView {
        timestamp: stamp(),
        frequency_hz: 14_075_200.0,
        snr: -8,
        delta_time: 0.2,
        delta_freq: 1.0,
        call_sign: full.then(|| "D2UY".into()),
        grid_square: full.then(|| "JI64".into()),
        message: "CQ D2UY JI64".into(),
        slot_parity,
        is_directed_at_us: false,
        worked_before: false,
        needed: true,
        atno: false,
        priority_score: full.then_some(720),
    }
}
fn row(slot_parity: Option<SlotParity>, source: &str) -> DxRow {
    let full = slot_parity.is_some();
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
        slot_parity,
        last_seen: stamp(),
        source: source.into(),
    }
}
fn qso(tx_parity: Option<SlotParity>) -> QsoProgress {
    let full = tx_parity.is_some();
    QsoProgress {
        qso_id: "q1".into(),
        their_callsign: "D2UY".into(),
        state: "calling".into(),
        frequency_hz: 14_075_200.0,
        tx_parity,
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
fn pending(dx_parity: Option<SlotParity>) -> PendingCall {
    PendingCall {
        callsign: "D2UY".into(),
        dx_parity,
        waited_secs: 15,
    }
}
fn events() -> Vec<E> {
    vec![
        E::Decoded {
            decoded: decoded(Some(SlotParity::Even)),
        },
        E::Decoded {
            decoded: decoded(Some(SlotParity::Odd)),
        },
        E::Decoded {
            decoded: decoded(None),
        },
        E::DxHunter {
            rows: vec![
                row(Some(SlotParity::Odd), "local"),
                row(None, "network"),
                row(Some(SlotParity::Even), "both"),
            ],
        },
        E::ActiveQsos {
            qsos: vec![
                qso(Some(SlotParity::Even)),
                qso(Some(SlotParity::Odd)),
                qso(None),
            ],
            pending: vec![
                pending(Some(SlotParity::Odd)),
                pending(Some(SlotParity::Even)),
                pending(None),
            ],
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
) -> Result<BTreeMap<&'a str, (usize, &'a Value)>, String> {
    let mut out = BTreeMap::new();
    for (position, member) in s
        .pointer(&format!("/$defs/{family}/oneOf"))
        .and_then(Value::as_array)
        .ok_or("missing union")?
        .iter()
        .enumerate()
    {
        let (_, branch) =
            resolve(s, member).map_err(|error| format!("{family}/oneOf/{position}: {error}"))?;
        let properties = branch.get("properties").and_then(Value::as_object);
        let tag = properties
            .and_then(|properties| properties.get(tag_key))
            .and_then(|rule| rule.get("const"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                let constants: BTreeMap<_, _> = properties
                    .into_iter()
                    .flatten()
                    .filter_map(|(key, rule)| Some((key, rule.get("const")?.as_str()?)))
                    .collect();
                format!(
                    "{family}/oneOf/{position}: missing or invalid discriminator field {tag_key:?}; string constant fields {constants:?}"
                )
            })?;
        if out.insert(tag, (position, branch)).is_some() {
            return Err(format!("duplicate discriminator {tag}"));
        }
    }
    Ok(out)
}

fn check_schema(
    s: &Value,
    family: &str,
    tag_key: &str,
    samples: &[(String, Value, Value)],
) -> Result<(), String> {
    let branches = branches(s, family, tag_key)?;
    let mut by_tag: BTreeMap<&str, Vec<_>> = BTreeMap::new();
    for (tag, payload, frame) in samples {
        by_tag.entry(tag).or_default().push((payload, frame));
    }
    let sample_tags: BTreeSet<_> = by_tag.keys().copied().collect();
    let schema_tags: BTreeSet<_> = branches.keys().copied().collect();
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
    for (tag, samples) in by_tag {
        let (position, branch) = branches[tag];
        let branch_schema = compiler
            .compile(
                &format!("{id}#/$defs/{family}/oneOf/{position}"),
                &mut schemas,
            )
            .map_err(|e| e.to_string())?;
        let mut errors = Vec::new();
        let payloads: Vec<_> = samples
            .iter()
            .map(|&(payload, _)| (String::new(), payload))
            .collect();
        compare_shape(s, branch, &payloads, &mut errors);
        for (payload, frame) in samples {
            for (name, value, index) in [
                ("branch", payload, branch_schema),
                ("union", payload, union),
                ("frame", frame, frame_schema),
            ] {
                if let Err(e) = schemas.validate(value, index) {
                    errors.push(format!("{name}: {e:#}"));
                }
            }
        }
        if !errors.is_empty() {
            return Err(format!("{tag}: {}", errors.join("; ")));
        }
    }
    Ok(())
}
fn resolve<'a>(
    root: &'a Value,
    mut node: &'a Value,
) -> Result<(Option<&'a str>, &'a Value), String> {
    let mut seen = BTreeSet::new();
    let mut location = None;
    while let Some(reference) = node.get("$ref").and_then(Value::as_str) {
        if !seen.insert(reference) {
            return Err(format!("cyclic reference {reference:?}"));
        }
        location = reference.strip_prefix('#');
        node = location
            .and_then(|pointer| root.pointer(pointer))
            .ok_or_else(|| format!("unresolved local JSON Pointer {reference:?}"))?;
    }
    Ok((location, node))
}
// Values at the same schema location share a property set, including array items.
// Keep each instance path for per-object required-field diagnostics.
fn compare_shape(
    root: &Value,
    node: &Value,
    values: &[(String, &Value)],
    errors: &mut Vec<String>,
) {
    let Some((path, _)) = values.first() else {
        return;
    };
    let node = match resolve(root, node) {
        Ok((_, node)) => node,
        Err(error) => {
            errors.push(format!("{path}: {error}"));
            return;
        }
    };
    if let Some(props) = node.get("properties").and_then(Value::as_object) {
        let mut objects = values
            .iter()
            .filter_map(|(path, value)| Some((path, value.as_object()?)))
            .peekable();
        let Some(&(first_object_path, _)) = objects.peek() else {
            return;
        };
        let declared: BTreeSet<_> = props.keys().map(String::as_str).collect();
        let mut emitted = BTreeSet::new();
        let mut children: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for (path, object) in objects {
            for required in node
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !object.contains_key(required) {
                    errors.push(format!("{path}: missing required {required}"));
                }
            }
            for (key, value) in object {
                emitted.insert(key.as_str());
                if props.contains_key(key) {
                    children
                        .entry(key)
                        .or_default()
                        .push((format!("{path}/{key}"), value));
                }
            }
        }
        let missing: Vec<_> = declared.difference(&emitted).collect();
        let extra: Vec<_> = emitted.difference(&declared).collect();
        if !missing.is_empty() || !extra.is_empty() {
            let location = if first_object_path.is_empty() {
                "/"
            } else {
                first_object_path
            };
            errors.push(format!(
                "property set across samples (first object at {location}): declared but unemitted {missing:?}; emitted but undeclared {extra:?}"
            ));
        }
        for (key, values) in children {
            compare_shape(root, &props[key], &values, errors);
        }
    } else if let Some(items) = node.get("items") {
        let mut elements = Vec::new();
        for (path, value) in values {
            if let Some(array) = value.as_array() {
                for (index, element) in array.iter().enumerate() {
                    elements.push((format!("{path}/{index}"), element));
                }
            }
        }
        compare_shape(root, items, &elements, errors);
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
            dx_hunter: vec![row(Some(SlotParity::Odd), "local")],
            active_qsos: vec![qso(Some(SlotParity::Even))],
            pending_calls: vec![pending(Some(SlotParity::Odd))],
            recent_decodes: vec![decoded(Some(SlotParity::Even))],
        },
    });
    let v = serde_json::to_value(&frame).unwrap();
    schemas.validate(&v, server).unwrap();
    assert_eq!(serde_json::from_value::<ServerFrame>(v).unwrap(), frame);
}
fn branch_mut<'a>(s: &'a mut Value, family: &str, index: usize) -> &'a mut Value {
    let member = format!("/$defs/{family}/oneOf/{index}");
    let (location, _) = resolve(s, s.pointer(&member).unwrap()).unwrap();
    let target = location.map(str::to_owned).unwrap_or(member);
    s.pointer_mut(&target).unwrap()
}

fn reference_union_member(s: &mut Value, family: &str, key: &str, tag: &str) {
    let (index, _) = branches(s, family, key).unwrap()[tag];
    let member = &mut s["$defs"][family]["oneOf"][index];
    let definition = std::mem::replace(member, json!({"$ref": "#/$defs/driftTestAlias"}));
    s["$defs"]["driftTestAlias"] = json!({"$ref": "#/$defs/driftTestDefinition"});
    s["$defs"]["driftTestDefinition"] = definition;
}

fn rename(s: &mut Value, family: &str, key: &str, tag: &str, old: &str, new: &str, required: bool) {
    let (index, _) = branches(s, family, key).unwrap()[tag];
    let b = branch_mut(s, family, index);
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
    reference_union_member(&mut s, "serverEvent", "event", "frequency");
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
    reference_union_member(&mut s, "clientCommand", "cmd", "callStation");
    check_schema(&s, "clientCommand", "cmd", &command_samples()).unwrap();
    let (index, branch) = branches(&s, "clientCommand", "cmd").unwrap()["callStation"];
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
    let branch = branch_mut(&mut s, "clientCommand", index);
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
    reference_union_member(&mut s, "serverEvent", "event", "frequency");
    check_schema(&s, "serverEvent", "event", &event_samples()).unwrap();
    let (index, _) = branches(&s, "serverEvent", "event").unwrap()["frequency"];
    let b = branch_mut(&mut s, "serverEvent", index)
        .pointer_mut("/properties/event/const")
        .unwrap();
    assert_eq!(b, "frequency");
    *b = json!("dialFrequency");
    let e = check_schema(&s, "serverEvent", "event", &event_samples()).unwrap_err();
    assert!(
        e.contains("frequency") && e.contains("dialFrequency"),
        "{e}"
    );
}

#[test]
fn nested_event_samples_cover_both_parities_and_null() {
    let samples = event_samples();
    for (tag, array, key) in [
        ("decoded", None, "slotParity"),
        ("dxHunter", Some("rows"), "slotParity"),
        ("activeQsos", Some("qsos"), "txParity"),
        ("activeQsos", Some("pending"), "dxParity"),
    ] {
        let parities: BTreeSet<_> = samples
            .iter()
            .filter(|(sample_tag, _, _)| sample_tag == tag)
            .flat_map(|(_, payload, _)| match array {
                Some(array) => payload[array]
                    .as_array()
                    .unwrap()
                    .iter()
                    .collect::<Vec<_>>(),
                None => vec![&payload["decoded"]],
            })
            .map(|dto| dto.get(key).unwrap().as_str())
            .collect();
        assert_eq!(
            parities,
            BTreeSet::from([None, Some("even"), Some("odd")]),
            "{tag} {array:?} {key}"
        );
    }
}

#[test]
fn union_members_can_use_local_references() {
    let mut s = schema();
    reference_union_member(&mut s, "serverEvent", "event", "frequency");
    check_schema(&s, "serverEvent", "event", &event_samples()).unwrap();
}

#[test]
fn discriminator_field_rename_reports_variant_and_fields() {
    let mut s = schema();
    rename(
        &mut s,
        "serverEvent",
        "event",
        "frequency",
        "event",
        "kind",
        true,
    );
    let error = check_schema(&s, "serverEvent", "event", &event_samples()).unwrap_err();
    for part in ["\"frequency\"", "\"event\"", "\"kind\""] {
        assert!(error.contains(part), "{error}");
    }
}

#[test]
fn optional_properties_can_be_omitted_from_individual_instances() {
    let s = schema();
    let mut commands = command_samples();
    let (_, payload, frame) = commands
        .iter_mut()
        .find(|(tag, _, _)| tag == "callStation")
        .unwrap();
    assert!(payload
        .as_object_mut()
        .unwrap()
        .remove("dxParity")
        .is_some());
    frame["command"] = payload.clone();
    let command_result = check_schema(&s, "clientCommand", "cmd", &commands);

    let mut events = event_samples();
    let (_, payload, frame) = events
        .iter_mut()
        .find(|(tag, _, _)| tag == "dxHunter")
        .unwrap();
    assert!(payload["rows"][0]
        .as_object_mut()
        .unwrap()
        .remove("slotParity")
        .is_some());
    frame["event"] = payload.clone();
    let event_result = check_schema(&s, "serverEvent", "event", &events);
    assert!(
        command_result.is_ok() && event_result.is_ok(),
        "{command_result:?}; {event_result:?}"
    );

    let (_, payload, frame) = events
        .iter_mut()
        .find(|(tag, _, _)| tag == "dxHunter")
        .unwrap();
    assert!(payload["rows"][0]
        .as_object_mut()
        .unwrap()
        .remove("callSign")
        .is_some());
    frame["event"] = payload.clone();
    let error = check_schema(&s, "serverEvent", "event", &events).unwrap_err();
    assert!(
        error.contains("/rows/0: missing required callSign"),
        "{error}"
    );
}
