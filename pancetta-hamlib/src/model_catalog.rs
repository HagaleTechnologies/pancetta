//! Offline Hamlib model resolution (PAN-340).
//!
//! [`resolve_model`] maps a `rig.model` value to the Hamlib model number
//! `rigctld -m` takes, using only the vendored [`HAMLIB_MODEL_CATALOG`]
//! snapshot plus the curated [`crate::models::SUPPORTED_RIGS`] aliases. The
//! lookup is synchronous and offline: it never constructs a client, spawns
//! a process, probes a daemon or touches the network, so
//! `pancetta-config` can run it during validation.
//!
//! Resolution order:
//!
//! 1. **Numeric IDs win.** An all-ASCII-digit value (after trimming) is a
//!    numeric Hamlib model ID, `1..=2147483647`, leading zeroes allowed.
//!    This is why the all-digit catalog names (`Barrett 950` and friends)
//!    must be written manufacturer-qualified: bare `"950"` means model ID
//!    950, `"Barrett 950"` means 32002.
//! 2. **Exact names.** A trimmed, case-insensitive match against catalog
//!    model names and curated aliases. Checked before relaxed matching so
//!    a broader normalization cannot defeat a uniquely spelled name.
//! 3. **Relaxed names.** Catalog models, `"Manufacturer Model"` forms and
//!    curated aliases compared after [`crate::models::normalize_rig_model`]
//!    (case, hyphens and spaces ignored).
//!
//! A value that matches several backends is an
//! [`ModelResolutionError::Ambiguous`] requiring the numeric ID; a value
//! that matches none is [`ModelResolutionError::Unknown`] with the three
//! nearest catalog names.

use std::fmt;

pub use crate::model_catalog_generated::{CatalogEntry, HAMLIB_MODEL_CATALOG};
use crate::models::{normalize_rig_model, SUPPORTED_RIGS};

/// The largest Hamlib model ID `rigctld -m` accepts (a C `int`).
pub const MAX_HAMLIB_MODEL_ID: u32 = 2_147_483_647;

/// Why a `rig.model` value did not resolve to a Hamlib model ID.
///
/// The [`fmt::Display`] text is operator-facing: configuration validation,
/// `pancetta doctor` and the managed-spawn warning all show the same body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelResolutionError {
    /// The value looked like a numeric Hamlib ID but was not a decimal
    /// integer from 1 to [`MAX_HAMLIB_MODEL_ID`] — zero, signed, overflowed,
    /// or a numeric value with trailing command fragments.
    InvalidNumeric {
        /// The offending `rig.model` value, trimmed.
        input: String,
    },
    /// No catalog entry or curated alias matched the value.
    Unknown {
        /// The offending `rig.model` value, trimmed.
        input: String,
        /// The nearest catalog models, `"Model (id)"`, closest first.
        suggestions: Vec<String>,
    },
    /// Several backends matched the value; a numeric ID is required.
    Ambiguous {
        /// The offending `rig.model` value, trimmed.
        input: String,
        /// Every matching backend, `"Manufacturer Model (id)"`.
        candidates: Vec<String>,
    },
}

impl fmt::Display for ModelResolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModelResolutionError::InvalidNumeric { input } => write!(
                f,
                "Invalid rig.model '{input}'. Numeric Hamlib IDs must be decimal integers \
                 from 1 to {MAX_HAMLIB_MODEL_ID}. Run `rigctl -l` to find your model ID."
            ),
            ModelResolutionError::Unknown { input, suggestions } => {
                write!(f, "Unknown rig.model '{input}'.")?;
                if !suggestions.is_empty() {
                    write!(f, " Closest matches: {}.", suggestions.join(", "))?;
                }
                write!(
                    f,
                    " Run `rigctl -l` to find your model, then set rig.model to its numeric \
                     ID, for example \"1045\"."
                )
            }
            ModelResolutionError::Ambiguous { input, candidates } => write!(
                f,
                "Ambiguous rig.model '{input}': {}. Set rig.model to the numeric ID from \
                 `rigctl -l`.",
                candidates.join(", ")
            ),
        }
    }
}

impl std::error::Error for ModelResolutionError {}

/// Resolve a `rig.model` value to its Hamlib model number.
///
/// Offline and deterministic; see the [module docs](self) for the matching
/// order. The input is trimmed first, so `" 1045 "` resolves like `"1045"`.
pub fn resolve_model(input: &str) -> Result<u32, ModelResolutionError> {
    let trimmed = input.trim();
    resolve_trimmed(trimmed)
}

fn resolve_trimmed(trimmed: &str) -> Result<u32, ModelResolutionError> {
    // Numeric precedence: an all-digit value is a model ID, never a name.
    if !trimmed.is_empty() && trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return match trimmed.parse::<u64>() {
            Ok(n) if (1..=MAX_HAMLIB_MODEL_ID as u64).contains(&n) => Ok(n as u32),
            _ => Err(ModelResolutionError::InvalidNumeric {
                input: trimmed.to_string(),
            }),
        };
    }
    // A value whose first token *starts as* a numeric value but is not a
    // bare decimal integer — a sign, or trailing arguments like
    // `"1045 -t 4532"` — is a malformed ID. Reject it as such instead of
    // falling through to name matching, so a pasted command fragment can
    // never be read as a name. Digit-leading names that are not numeric
    // tokens (`"6xxx"`, `"505DSP"`) stay names.
    let first_token = trimmed
        .split([' ', '\t', ';', ','])
        .next()
        .unwrap_or_default();
    let token_is_all_digits =
        !first_token.is_empty() && first_token.bytes().all(|b| b.is_ascii_digit());
    let token_is_signed_integer = first_token.len() > 1
        && first_token.starts_with(['+', '-'])
        && first_token[1..].bytes().all(|b| b.is_ascii_digit());
    if token_is_signed_integer || (token_is_all_digits && trimmed != first_token) {
        return Err(ModelResolutionError::InvalidNumeric {
            input: trimmed.to_string(),
        });
    }

    // Stage A: exact (trimmed, case-insensitive) catalog names.
    if let Some(resolved) = exact_stage(
        HAMLIB_MODEL_CATALOG
            .iter()
            .filter(|e| trimmed.eq_ignore_ascii_case(e.model))
            .map(|e| (e.id, label_from(e.id, e.manufacturer, e.model))),
        trimmed,
    ) {
        return resolved;
    }
    // Manufacturer-qualified catalog names ("Barrett 950", "Icom IC-7300").
    if let Some(resolved) = exact_stage(
        HAMLIB_MODEL_CATALOG
            .iter()
            .filter(|e| trimmed.eq_ignore_ascii_case(&format!("{} {}", e.manufacturer, e.model)))
            .map(|e| (e.id, label_from(e.id, e.manufacturer, e.model))),
        trimmed,
    ) {
        return resolved;
    }
    // Stage B: exact curated aliases ("IC-7851" for the IC-7850/7851 row,
    // "FT-991A", "FT-817ND", ...). Aliases are references to catalog
    // records, never a second hand-written name-to-number table.
    if let Some(resolved) = exact_stage(
        SUPPORTED_RIGS
            .iter()
            .filter(|rig| {
                rig.config_names
                    .iter()
                    .any(|n| trimmed.eq_ignore_ascii_case(n))
            })
            .map(|rig| {
                (
                    rig.hamlib_id,
                    label_from(rig.hamlib_id, rig.manufacturer, rig.model),
                )
            }),
        trimmed,
    ) {
        return resolved;
    }

    // Stage C: relaxed matching (case, hyphens and spaces ignored) over
    // catalog names, qualified names and curated aliases together.
    let wanted = normalize_rig_model(trimmed);
    let is_match = |name: &str| normalize_rig_model(name) == wanted;
    let mut hits: Vec<(u32, String)> = HAMLIB_MODEL_CATALOG
        .iter()
        .filter(|e| is_match(e.model) || is_match(&format!("{} {}", e.manufacturer, e.model)))
        .map(|e| (e.id, label_from(e.id, e.manufacturer, e.model)))
        .chain(
            SUPPORTED_RIGS
                .iter()
                .filter(|rig| rig.config_names.iter().any(|n| is_match(n)) || is_match(rig.model))
                .map(|rig| {
                    (
                        rig.hamlib_id,
                        label_from(rig.hamlib_id, rig.manufacturer, rig.model),
                    )
                }),
        )
        .collect();
    dedup_by_id(&mut hits);
    match hits.len() {
        1 => Ok(hits.remove(0).0),
        0 => Err(ModelResolutionError::Unknown {
            input: trimmed.to_string(),
            suggestions: suggest(trimmed),
        }),
        _ => Err(ModelResolutionError::Ambiguous {
            input: trimmed.to_string(),
            candidates: hits.into_iter().map(|(_, label)| label).collect(),
        }),
    }
}

/// Exact-stage evaluation shared by every source: zero hits means "try the
/// next stage", one hit resolves, several distinct IDs are ambiguous.
fn exact_stage<I>(hits: I, trimmed: &str) -> Option<Result<u32, ModelResolutionError>>
where
    I: Iterator<Item = (u32, String)>,
{
    let mut hits: Vec<(u32, String)> = hits.collect();
    dedup_by_id(&mut hits);
    match hits.len() {
        0 => None,
        1 => Some(Ok(hits.remove(0).0)),
        _ => Some(Err(ModelResolutionError::Ambiguous {
            input: trimmed.to_string(),
            candidates: hits.into_iter().map(|(_, label)| label).collect(),
        })),
    }
}

fn dedup_by_id(hits: &mut Vec<(u32, String)>) {
    hits.sort_by_key(|(id, _)| *id);
    hits.dedup_by(|a, b| a.0 == b.0);
}

fn label_from(id: u32, manufacturer: &str, model: &str) -> String {
    format!("{manufacturer} {model} ({id})")
}

/// The three nearest catalog models, `"Model (id)"`, ranked by Levenshtein
/// distance on the normalized model name, then the lowercase model name,
/// then the ID — deterministic, and no fuzzy-match dependency.
fn suggest(trimmed: &str) -> Vec<String> {
    let wanted = normalize_rig_model(trimmed);
    let mut ranked: Vec<(usize, &str, u32)> = HAMLIB_MODEL_CATALOG
        .iter()
        .map(|e| {
            (
                levenshtein(&wanted, &normalize_rig_model(e.model)),
                e.model,
                e.id,
            )
        })
        .collect();
    ranked.sort_by_key(|&(dist, model, id)| (dist, model.to_lowercase(), id));
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for (_, model, id) in ranked {
        if seen.insert((model, id)) && out.len() < 3 {
            out.push(format!("{model} ({id})"));
        }
        if out.len() == 3 {
            break;
        }
    }
    out
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0_usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j + 1] + 1)
                .min(cur[j] + 1)
                .min(prev[j] + (ca != cb) as usize);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Compile-time Hamlib model ID for an upstream `RIG_MODEL_*` macro name,
/// looked up in the vendored catalog. `None` when the macro is not in the
/// pinned snapshot.
pub const fn catalog_id_by_macro(macro_name: &str) -> Option<u32> {
    let mut i = 0;
    while i < HAMLIB_MODEL_CATALOG.len() {
        let entry = &HAMLIB_MODEL_CATALOG[i];
        if const_str_eq(entry.macro_name, macro_name) {
            return Some(entry.id);
        }
        i += 1;
    }
    None
}

/// [`catalog_id_by_macro`] that refuses to compile (or, in a runtime call,
/// panics) when the macro is missing, so curated tables cannot silently
/// fall back to a placeholder ID.
pub const fn catalog_id(macro_name: &str) -> u32 {
    match catalog_id_by_macro(macro_name) {
        Some(id) => id,
        None => panic!("RIG_MODEL macro not present in the vendored Hamlib catalog"),
    }
}

const fn const_str_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Parse a `rigctl -l` snapshot with the header's column boundaries (model
/// names contain spaces, so token splitting cannot work). The generator
/// script owns the canonical parser; this independent copy exists so a test
/// can check the generated table against the raw snapshot itself.
#[cfg(test)]
pub(crate) fn parse_snapshot(text: &str) -> Result<Vec<(u32, String, String)>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let Some(header) = lines.first() else {
        return Err("snapshot is empty".to_string());
    };
    let col = |label: &str| {
        header
            .find(label)
            .ok_or_else(|| format!("header column {label:?} not found"))
    };
    let id_start = col("Rig #")?;
    let mfg_start = col("Mfg")?;
    let model_start = col("Model")?;
    let ver_start = col("Version")?;
    let status_start = col("Status")?;
    let macro_start = col("Macro")?;
    let mut rows = Vec::new();
    for (n, line) in lines.iter().enumerate().skip(1) {
        if line.trim().is_empty() {
            continue;
        }
        let slice = |start: usize, end: usize| -> String {
            line.get(start..end).unwrap_or("").trim().to_string()
        };
        let id: u32 = slice(id_start, mfg_start)
            .parse()
            .map_err(|_| format!("line {}: Rig # is not an integer", n + 1))?;
        let row = (
            id,
            slice(mfg_start, model_start),
            slice(model_start, ver_start),
        );
        let _ = (
            &slice(ver_start, status_start),
            slice(status_start, macro_start),
        );
        if row.1.is_empty() || row.2.is_empty() {
            return Err(format!(
                "line {}: empty manufacturer or model column",
                n + 1
            ));
        }
        rows.push(row);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_ids_are_explicit() {
        // Catalog IDs resolve to themselves...
        assert_eq!(resolve_model("1045"), Ok(1045));
        assert_eq!(resolve_model("3073"), Ok(3073));
        // ...and so do positive IDs the catalog does not carry.
        assert_eq!(resolve_model("123456"), Ok(123456));
        assert_eq!(resolve_model("2147483647"), Ok(2147483647));
        // Whitespace and leading zeroes are accepted; the ID is canonical.
        assert_eq!(resolve_model(" 1045 "), Ok(1045));
        assert_eq!(resolve_model("0007"), Ok(7));
        // Malformed numeric values never fall through to name matching.
        for bad in [
            "0",
            "-5",
            "+5",
            "9999999999",
            "1045 -t 4532",
            "1045;power 50",
        ] {
            let err = resolve_model(bad).unwrap_err();
            assert_eq!(
                err,
                ModelResolutionError::InvalidNumeric {
                    input: bad.trim().to_string()
                },
                "{bad:?}"
            );
            let text = err.to_string();
            assert!(
                text.contains(&format!("Invalid rig.model '{bad}'")),
                "{text}"
            );
            assert!(
                text.contains("decimal integers from 1 to 2147483647"),
                "{text}"
            );
        }
    }

    #[test]
    fn catalog_name_examples_resolve() {
        // Names from `rigctl -l`, with normalization.
        assert_eq!(resolve_model("IC-7300"), Ok(3073));
        assert_eq!(resolve_model("ic 7300"), Ok(3073));
        assert_eq!(resolve_model("KX3"), Ok(2045)); // rejected before PAN-340
        assert_eq!(resolve_model("FTDX-10"), Ok(1042));
        assert_eq!(resolve_model("FTdx10"), Ok(1042));
        assert_eq!(resolve_model("mcHF QRP"), Ok(1045));
        assert_eq!(resolve_model("mchfqrp"), Ok(1045));
        assert_eq!(resolve_model("IC-7850/7851"), Ok(3075));
        assert_eq!(resolve_model("NET rigctl"), Ok(2));
        assert_eq!(resolve_model("Dummy No VFO"), Ok(6));
        // Manufacturer-qualified names.
        assert_eq!(resolve_model("Icom IC-7300"), Ok(3073));
        assert_eq!(resolve_model("Elecraft K3S"), Ok(2043));
        assert_eq!(resolve_model("Video4Linux2 SW/FM radio"), Ok(26002));
        // All-digit catalog names resolve through the qualified form;
        // bare digits stay numeric IDs.
        assert_eq!(resolve_model("Barrett 950"), Ok(32002));
        assert_eq!(resolve_model("Barrett 2050"), Ok(32001));
        assert_eq!(resolve_model("Barrett 4050"), Ok(32003));
        assert_eq!(resolve_model("950"), Ok(950));
        // Curated aliases are references to catalog records.
        assert_eq!(resolve_model("IC-7851"), Ok(3075));
        assert_eq!(resolve_model("FT-991A"), Ok(1035));
        assert_eq!(resolve_model("FT-817ND"), Ok(1020));
        assert_eq!(resolve_model("TS-590"), Ok(2031));
        assert_eq!(resolve_model("ts 590 sg"), Ok(2037));
        // Placeholders are not special to the resolver; config treats them.
        assert!(resolve_model("Generic").is_err());
        assert!(resolve_model("").is_err());
    }

    #[test]
    fn every_catalog_row_resolves_or_reports_ambiguity() {
        for entry in HAMLIB_MODEL_CATALOG {
            let all_digits =
                !entry.model.is_empty() && entry.model.bytes().all(|b| b.is_ascii_digit());
            let bare = resolve_model(entry.model);
            if all_digits {
                // Numeric precedence: bare digits are the ID, not the name.
                assert_eq!(bare, Ok(entry.model.parse().unwrap()), "{}", entry.model);
            } else {
                match bare {
                    Ok(id) => assert_eq!(id, entry.id, "{}", entry.model),
                    Err(ModelResolutionError::Ambiguous { .. }) => {}
                    other => panic!("{} did not resolve: {other:?}", entry.model),
                }
            }
            // The manufacturer-qualified form is unique for every row.
            let qualified = format!("{} {}", entry.manufacturer, entry.model);
            assert_eq!(
                resolve_model(&qualified),
                Ok(entry.id),
                "{qualified:?} must resolve to {}",
                entry.id
            );
            // The upstream macro key finds a row carrying it. Upstream
            // reuses one macro for two rows (FT-450 1027 / FT-450D 1046
            // share RIG_MODEL_FT450), so the lookup may answer with either.
            if entry.macro_name.starts_with("RIG_MODEL_") {
                let found = catalog_id_by_macro(entry.macro_name)
                    .unwrap_or_else(|| panic!("{} missing from the catalog", entry.macro_name));
                assert!(
                    HAMLIB_MODEL_CATALOG
                        .iter()
                        .any(|e| { e.macro_name == entry.macro_name && e.id == found }),
                    "{} resolved to unrelated ID {found}",
                    entry.macro_name
                );
            }
        }
    }

    #[test]
    fn ambiguous_model_requires_numeric_id() {
        // The one real normalized-name collision in the 4.5.5 catalog.
        let err = resolve_model("SW/FM radio").unwrap_err();
        assert_eq!(
            err,
            ModelResolutionError::Ambiguous {
                input: "SW/FM radio".into(),
                candidates: vec![
                    "Video4Linux SW/FM radio (26001)".into(),
                    "Video4Linux2 SW/FM radio (26002)".into(),
                ],
            }
        );
        assert_eq!(
            err.to_string(),
            "Ambiguous rig.model 'SW/FM radio': Video4Linux SW/FM radio (26001), \
             Video4Linux2 SW/FM radio (26002). Set rig.model to the numeric ID from \
             `rigctl -l`."
        );
        // Relaxed spellings are ambiguous too (normalization keeps '/' —
        // the slash is part of the catalog name).
        assert!(matches!(
            resolve_model("sw/fm radio"),
            Err(ModelResolutionError::Ambiguous { .. })
        ));
        assert!(matches!(
            resolve_model("SW/FM RADIO"),
            Err(ModelResolutionError::Ambiguous { .. })
        ));
        // Synthetic collision fixture: same model name, two backends.
        let synthetic = [
            CatalogEntry {
                id: 9001,
                manufacturer: "TestA",
                model: "Twin 99",
                version: "1",
                status: "Stable",
                macro_name: "RIG_MODEL_TEST_A",
            },
            CatalogEntry {
                id: 9002,
                manufacturer: "TestB",
                model: "Twin 99",
                version: "1",
                status: "Stable",
                macro_name: "RIG_MODEL_TEST_B",
            },
        ];
        // The exact/relaxed stages are shared, so drive them directly.
        let hits: Vec<(u32, String)> = synthetic
            .iter()
            .filter(|e| "Twin 99".eq_ignore_ascii_case(e.model))
            .map(|e| (e.id, label_from(e.id, e.manufacturer, e.model)))
            .collect();
        assert_eq!(hits.len(), 2);
        let resolved = exact_stage(hits.into_iter(), "Twin 99").unwrap();
        assert!(matches!(
            resolved,
            Err(ModelResolutionError::Ambiguous { .. })
        ));
        // Qualified names disambiguate the synthetic pair as well.
        assert_eq!(
            exact_stage(
                synthetic
                    .iter()
                    .filter(|e| "TestB Twin 99"
                        .eq_ignore_ascii_case(&format!("{} {}", e.manufacturer, e.model)))
                    .map(|e| (e.id, label_from(e.id, e.manufacturer, e.model))),
                "TestB Twin 99"
            ),
            Some(Ok(9002))
        );
    }

    #[test]
    fn unknown_names_get_ranked_suggestions() {
        let err = resolve_model("FT-9999").unwrap_err();
        let ModelResolutionError::Unknown { input, suggestions } = &err else {
            panic!("FT-9999 must be Unknown, got {err:?}");
        };
        assert_eq!(input, "FT-9999");
        assert_eq!(
            suggestions,
            &["FT-990 (1016)", "FT-991 (1035)", "FT-890 (1015)"]
        );
        assert_eq!(
            err.to_string(),
            "Unknown rig.model 'FT-9999'. Closest matches: FT-990 (1016), FT-991 (1035), \
             FT-890 (1015). Run `rigctl -l` to find your model, then set rig.model to its \
             numeric ID, for example \"1045\"."
        );
        // Deterministic across repeat calls and orderings.
        assert_eq!(resolve_model("FT-9999").unwrap_err(), err);
        // No suggestions sentence when nothing is near... (every input ranks
        // something, so assert the bounded list shape instead).
        let long = resolve_model("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").unwrap_err();
        assert!(long.to_string().starts_with("Unknown rig.model 'zzzz"));
        // A bare manufacturer is unknown, not ambiguous.
        assert!(matches!(
            resolve_model("Yaesu"),
            Err(ModelResolutionError::Unknown { .. })
        ));
    }

    /// AC6, part one: the checked-in catalog covers every row of the pinned
    /// `rigctl -l` snapshot, with no dropped row and no duplicate ID.
    #[test]
    fn catalog_snapshot_is_complete() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/data/rigctl-4.5.5.txt");
        let raw = std::fs::read_to_string(path).expect("vendored rigctl snapshot");
        let rows = parse_snapshot(&raw).expect("snapshot parses");
        assert_eq!(rows.len(), 283, "the pinned 4.5.5 snapshot has 283 rows");
        let mut ids: Vec<u32> = rows.iter().map(|(id, _, _)| *id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 283, "283 distinct IDs");
        // Every raw row matches its generated entry field for field.
        assert_eq!(HAMLIB_MODEL_CATALOG.len(), rows.len());
        for (entry, (id, mfg, model)) in HAMLIB_MODEL_CATALOG.iter().zip(&rows) {
            assert_eq!(&entry.id, id);
            assert_eq!(&entry.manufacturer, mfg);
            assert_eq!(&entry.model, model);
        }
        // Key IDs pinned independently of the generated table.
        for (macro_name, id) in [
            ("RIG_MODEL_FTDX10", 1042),
            ("RIG_MODEL_MCHFQRP", 1045),
            ("RIG_MODEL_KX3", 2045),
            ("RIG_MODEL_IC7300", 3073),
        ] {
            assert_eq!(catalog_id_by_macro(macro_name), Some(id));
        }
    }

    /// AC6, part two: the generated file regenerates byte for byte without
    /// Hamlib or network access (the generator's `--check` mode).
    #[test]
    fn catalog_generation_is_current() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let script = format!("{manifest}/scripts/generate_model_catalog.py");
        let status = std::process::Command::new("python3")
            .arg(script)
            .arg("--check")
            .current_dir(manifest)
            .status()
            .expect("python3 must be installed to run the generator check");
        assert!(
            status.success(),
            "model_catalog_generated.rs is stale; rerun \
             python3 pancetta-hamlib/scripts/generate_model_catalog.py"
        );
    }
}
