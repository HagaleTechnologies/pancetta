//! Versioned local ADIF contest evidence. Derived scores are never persisted.
use super::scoring::{
    ContestContactFact, ContestPeriod, ContestScoringContext, ContestScoringStamp,
};
use crate::states::{ContestInfo, ContestSerials, QsoMetadata};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const INSTANCE: &str = "APP_PANCETTA_CONTEST_INSTANCE";
pub const RULES: &str = "APP_PANCETTA_CONTEST_RULES";
pub const CONTEXT: &str = "APP_PANCETTA_CONTEST_CONTEXT";
pub const CONTACT_ID: &str = "APP_PANCETTA_CONTEST_CONTACT_ID";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContestEvidence {
    pub their_region: Option<String>,
    pub their_county: Option<String>,
    pub our_county: Option<String>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoggedContestContext {
    pub version: u32,
    pub profile_id: String,
    pub operator_callsign: String,
    pub operator_region: String,
    pub periods: Vec<ContestPeriod>,
    pub evidence: ContestEvidence,
    /// Exact live start time; standard ADIF TIME_ON has only whole seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_start: Option<chrono::DateTime<chrono::Utc>>,
}

/// Decode only supported, complete evidence. Callers must keep raw tags even
/// on error, so an unknown version survives import/export without alteration.
pub fn decode(tags: &HashMap<String, String>) -> Result<Option<LoggedContestContext>, String> {
    if ![INSTANCE, RULES, CONTEXT, CONTACT_ID]
        .iter()
        .any(|k| tags.contains_key(*k))
    {
        return Ok(None);
    }
    let required = |key: &str| {
        tags.get(key)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| format!("contest: missing {key}"))
    };
    required(INSTANCE)?;
    required(RULES)?;
    uuid::Uuid::parse_str(required(CONTACT_ID)?).map_err(|_| "contest: invalid contact id")?;
    let value: serde_json::Value =
        serde_json::from_str(required(CONTEXT)?).map_err(|_| "contest: malformed context")?;
    if value.get("version").and_then(|v| v.as_u64()) != Some(1) {
        return Err("contest: contest rules are unsupported".into());
    }
    let logged: LoggedContestContext =
        serde_json::from_value(value).map_err(|_| "contest: malformed context")?;
    if logged.operator_callsign.trim().is_empty() || logged.periods.is_empty() {
        return Err("contest: malformed context".into());
    }
    let context = ContestScoringContext::new(
        required(INSTANCE)?,
        &logged.profile_id,
        required(RULES)?,
        &logged.operator_region,
        logged.periods.clone(),
    )
    .map_err(|e| e.to_string())?;
    if context.rules().map_err(|e| e.to_string())?.profile_id != logged.profile_id {
        return Err("contest: unsupported profile".into());
    }
    Ok(Some(logged))
}

/// Interpret a CQ enrollment stamp using the latched first answer. Imported
/// records have no first_answer_at and retain their exact logged timestamp.
/// Only the original creation stamp may move; conflicting evidence still fails
/// validation in fact(). Export and indexed facts must use the same context.
pub(crate) fn contact_context(
    metadata: &QsoMetadata,
) -> Result<Option<LoggedContestContext>, String> {
    let mut logged = decode(&metadata.tags)?;
    if let Some(logged) = logged.as_mut() {
        if metadata.first_answer_at.is_some() && logged.contact_start == Some(metadata.start_time) {
            logged.contact_start = Some(metadata.contact_started_at());
        }
    }
    Ok(logged)
}

fn normalized(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_uppercase)
}

/// Band evidence for a contest fact: the explicit logged BAND tag (an
/// imported record may carry BAND with no FREQ), else the band the logged
/// RF frequency falls in. A value outside every named band — pancetta
/// without CAT logs the audio offset, which formats as the `0MHZ`
/// placeholder — is unresolvable evidence rather than a disallowed band,
/// so it stays `None` and the reducer keeps the contact pending.
fn fact_band(tags: &HashMap<String, String>, frequency_hz: f64) -> Option<String> {
    normalized(tags.get("BAND").map(String::as_str))
        .or_else(|| normalized(Some(&crate::adif::freq_hz_to_band(frequency_hz))))
        .filter(|band| !band.ends_with("MHZ"))
}

/// Prefer app evidence, filling missing fields from explicit standard facts.
/// Any contradiction leaves all location evidence unresolved. Keep original
/// app JSON and standard fields in the tags so this remains lossless.
pub fn resolved_evidence(
    logged: &LoggedContestContext,
    tags: &HashMap<String, String>,
    state: Option<&str>,
) -> ContestEvidence {
    let mut evidence = logged.evidence.clone();
    evidence.their_region = normalized(evidence.their_region.as_deref());
    evidence.their_county = normalized(evidence.their_county.as_deref());
    evidence.our_county = normalized(evidence.our_county.as_deref());
    let state = normalized(state);
    let county = tags
        .get("CNTY")
        .and_then(|s| s.split_once(','))
        .map(|(region, county)| (normalized(Some(region)), normalized(Some(county))));
    let mut conflict = false;
    for region in [
        state,
        county.as_ref().and_then(|(region, _)| region.clone()),
    ]
    .into_iter()
    .flatten()
    {
        if evidence
            .their_region
            .as_ref()
            .is_some_and(|current| current != &region)
        {
            conflict = true;
        } else {
            evidence.their_region = Some(region);
        }
    }
    if let Some((_, Some(county))) = county {
        if evidence
            .their_county
            .as_ref()
            .is_some_and(|current| current != &county)
        {
            conflict = true;
        } else {
            evidence.their_county = Some(county);
        }
    }
    if conflict {
        return ContestEvidence {
            source: Some("conflict".into()),
            ..Default::default()
        };
    }
    evidence
}

/// Recover family identity even when ADIF deliberately omits CONTEST_ID.
pub fn imported_info(
    tags: &HashMap<String, String>,
    standard_id: Option<&str>,
    serials: ContestSerials,
) -> Option<ContestInfo> {
    let logged = decode(tags).ok().flatten()?;
    Some(ContestInfo {
        contest_name: logged.profile_id,
        adif_contest_id: standard_id.map(str::to_string),
        category: String::new(),
        serials,
        points: 0,
        multiplier: None,
        scoring: Some(ContestScoringStamp {
            instance_id: tags[INSTANCE].clone(),
            rules_id: tags[RULES].clone(),
        }),
    })
}

/// Add immutable instance data to a new live manual contact. No history is
/// enrolled retrospectively and no standard contest id is invented.
pub fn attach(metadata: &mut QsoMetadata, context: &ContestScoringContext) {
    let logged = LoggedContestContext {
        version: 1,
        profile_id: context.profile_id.clone(),
        operator_callsign: metadata.our_callsign.clone(),
        operator_region: context.operator_region.clone(),
        periods: context.periods.clone(),
        evidence: ContestEvidence::default(),
        contact_start: Some(metadata.contact_started_at()),
    };
    metadata
        .tags
        .insert(INSTANCE.into(), context.instance_id.clone());
    metadata.tags.insert(RULES.into(), context.rules_id.clone());
    metadata
        .tags
        .insert(CONTACT_ID.into(), metadata.qso_id.to_string());
    // This struct contains only strings, timestamps and nullable strings.
    metadata.tags.insert(
        CONTEXT.into(),
        serde_json::to_string(&logged).expect("serializable contest context"),
    );
    metadata.contest_info = imported_info(
        &metadata.tags,
        None,
        ContestSerials {
            sent: None,
            received: None,
        },
    );
}

/// Validate immutable identity before period filtering, including older rows
/// with the same instance id outside today's configured periods.
pub fn fact(
    metadata: &QsoMetadata,
    context: &ContestScoringContext,
    operator_callsign: &str,
) -> Result<ContestContactFact, String> {
    let logged = contact_context(metadata)?.ok_or("contest: missing context")?;
    if metadata.tags[INSTANCE] != context.instance_id
        || metadata.tags[RULES] != context.rules_id
        || logged.profile_id != context.profile_id
        || !logged
            .operator_callsign
            .trim()
            .eq_ignore_ascii_case(operator_callsign.trim())
        || logged.operator_region.trim().to_ascii_uppercase() != context.operator_region
        || logged.periods != context.periods
    {
        return Err("contest: instance_id conflicts with logged contest settings".into());
    }
    let start = logged
        .contact_start
        .unwrap_or(metadata.contact_started_at());
    if start.timestamp() != metadata.contact_started_at().timestamp() {
        return Err("contest: contact start conflicts with logged QSO time".into());
    }
    let evidence = resolved_evidence(&logged, &metadata.tags, metadata.their_state.as_deref());
    Ok(ContestContactFact {
        contact_id: metadata.tags[CONTACT_ID].clone(),
        instance_id: context.instance_id.clone(),
        callsign: metadata
            .their_callsign
            .clone()
            .ok_or("contest: missing callsign")?,
        band: fact_band(&metadata.tags, metadata.frequency),
        mode: metadata.mode.clone(),
        start,
        failed: false,
        their_region: evidence.their_region,
        their_county: evidence.their_county,
        our_county: evidence.our_county,
    })
}

/// Snapshot envelope for all UI states. A single producer owns generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContestScoreState {
    Disabled,
    Loading,
    Ready(super::scoring::ContestScoreSnapshot),
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContestScoreUpdate {
    pub generation: u64,
    pub instance_id: String,
    pub rules_id: String,
    pub display_name: String,
    pub outside_period: bool,
    pub state: ContestScoreState,
}
