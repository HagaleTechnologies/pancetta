//! PAN-270: coordinator-owned session choices — the operator's last live
//! TX-frequency mode (`f` / `o`) and decode-effort (`e`) preset, restored at
//! startup so the first decode slot and the first TUI frame already run at
//! the restored values instead of the compiled-in defaults.
//!
//! The whole policy lives here as pure functions (path, [`load`], [`resolve`],
//! [`save`]) so every ticket scenario is a unit test without a coordinator.
//! The TUI is a client and can be absent or remote, which is why this file —
//! not `tui_state.json` — is the source of truth: the restored values drive
//! the decoder budget and the autonomous service, not just the console chip.
//!
//! ## On-disk format
//!
//! `~/.pancetta/session_state.json`, flat and hand-inspectable (same style as
//! `tui_state.json`):
//!
//! ```json
//! {"tx_freq_mode":"AUTO","decode_effort":"MAX","decode_effort_config_at_save":"AUTO"}
//! ```
//!
//! Absent keys mean "never chosen" and restore nothing. Unknown labels,
//! malformed JSON, and unreadable files make the whole file [`SessionLoad::Ignored`]
//! — defaults apply, startup continues, and the operator is warned (the
//! console banner path config-load warnings use).
//!
//! ## Precedence against `pancetta.toml` (ticket's last scenario)
//!
//! At startup the config's current `[decoder].effort` is compared with
//! `decode_effort_config_at_save` — the config value the station was RUNNING
//! when the choice was saved (the shared config snapshot, because
//! `decoder.effort` never hot-applies). If they differ, the config edit is
//! newer: the station runs the config value and the file is rewritten so the
//! config value becomes the saved choice. Per-field records (not one
//! whole-file timestamp) mean an `f` press never refreshes the effort
//! baseline and cannot mask a later config edit.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pancetta_config::DecodeEffort;
use pancetta_core::TxFreqMode;
use tracing::warn;

/// The saved decode-effort choice: the operator's last live preset plus the
/// config `[decoder].effort` value the station was running when that choice
/// was made (the baseline the "config edit is newer" rule compares against).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EffortChoice {
    pub(crate) choice: DecodeEffort,
    pub(crate) config_at_save: DecodeEffort,
}

/// The canonical session record. `None` fields mean "never chosen" and are
/// omitted from the on-disk file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SessionRecord {
    /// Last live TX-frequency mode choice (`f` toggle / `o` set-clear).
    pub(crate) tx_freq_mode: Option<TxFreqMode>,
    /// Last live decode-effort choice (`e` cycle) with its config baseline.
    pub(crate) decode_effort: Option<EffortChoice>,
}

/// Result of reading the session file. [`SessionLoad::Missing`] (a first run)
/// is distinct from [`SessionLoad::Ignored`] (unreadable/corrupt/unknown —
/// the ticket's fifth scenario warns only for this one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionLoad {
    /// No file: a first run, nothing to restore, no warning, no rewrite.
    Missing,
    /// A structurally valid file (possibly with absent keys).
    Record(SessionRecord),
    /// Unreadable, malformed, or carrying an unknown label: defaults apply
    /// and the operator is warned. The payload is the diagnostic reason.
    Ignored(String),
}

/// The resolved startup state: the effective values to seed the coordinator
/// with, what to report, and the canonical record from here on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartupChoices {
    /// Effective TX-frequency mode (restored choice, else Hold).
    pub(crate) tx_freq_mode: TxFreqMode,
    /// Effective decode effort (restored choice, superseding config value,
    /// else the config value).
    pub(crate) decode_effort: DecodeEffort,
    /// The restored TX-frequency mode, if a saved choice applied.
    pub(crate) restored_tx_freq_mode: Option<TxFreqMode>,
    /// The restored decode effort, if a saved choice applied.
    pub(crate) restored_decode_effort: Option<DecodeEffort>,
    /// The saved effort a newer config edit superseded (for the status line).
    pub(crate) superseded_saved_effort: Option<DecodeEffort>,
    /// Why the saved choices were ignored, when they were.
    pub(crate) ignored_reason: Option<String>,
    /// The canonical record from here on (the supersede case already folds
    /// the config value in, so a rewrite writes exactly this).
    pub(crate) record: SessionRecord,
}

/// Operator-facing startup facts, composed once so the log lines (headless
/// visibility) and the TUI messages (console visibility) can never diverge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SessionReport {
    /// The `StatusUpdate` line (`Session: restored … from the last session`
    /// / the superseded variant), when there is something to say.
    pub(crate) status: Option<String>,
    /// The corrupt-file warning for the error banner, when choices were
    /// ignored.
    pub(crate) error: Option<String>,
}

/// One live mutation to persist: exactly the field the keystroke changed, so
/// a mode-only press never refreshes the effort record's config baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionChoiceUpdate {
    TxFreqMode(TxFreqMode),
    DecodeEffort {
        choice: DecodeEffort,
        config_at_save: DecodeEffort,
    },
}

/// Strict label parser for [`TxFreqMode`] (labels match `TxFreqMode::label`).
/// Neither crate exports a string parser, so the helpers live here.
fn parse_tx_freq_mode(label: &str) -> Option<TxFreqMode> {
    match label {
        "HOLD" => Some(TxFreqMode::Hold),
        "AUTO" => Some(TxFreqMode::Auto),
        _ => None,
    }
}

/// Strict label parser for [`DecodeEffort`] (labels match
/// `DecodeEffort::label`).
fn parse_decode_effort(label: &str) -> Option<DecodeEffort> {
    match label {
        "ECO" => Some(DecodeEffort::Eco),
        "STANDARD" => Some(DecodeEffort::Standard),
        "DEEP" => Some(DecodeEffort::Deep),
        "MAX" => Some(DecodeEffort::Max),
        "AUTO" => Some(DecodeEffort::Auto),
        _ => None,
    }
}

/// Path of the session state file: `<home>/.pancetta/session_state.json`.
/// `home_override` is the test-only override (the coordinator's
/// `pancetta_home` parameter, mirroring `qso.rs`'s `pancetta_home_override`);
/// `None` in production resolves the real home directory. `None` is returned
/// only when neither resolves — no home, no persistence, defaults apply.
///
/// Test builds (`#[cfg(test)]`) never resolve the real home (PAN-194
/// discipline, same as `pancetta-tui`'s `tui_state_path`): without an
/// explicit override they return `None`, so a test constructing a
/// coordinator with no home sees a first run — deterministic whatever the
/// developer saved in their real `~/.pancetta/session_state.json` — and can
/// never write to it either.
pub(crate) fn session_state_path(home_override: Option<&Path>) -> Option<PathBuf> {
    #[cfg(test)]
    if home_override.is_none() {
        return None;
    }
    let base = home_override
        .map(Path::to_path_buf)
        .or_else(dirs::home_dir)?;
    Some(base.join(".pancetta").join("session_state.json"))
}

/// Read and strictly parse the session file. `NotFound` is
/// [`SessionLoad::Missing`]; any I/O error, malformed JSON, or unknown label
/// is [`SessionLoad::Ignored`] with a reason. Absent keys parse as `None`
/// choices (a structurally valid file warns about nothing).
pub(crate) fn load(path: &Path) -> SessionLoad {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return SessionLoad::Missing,
        Err(e) => {
            return SessionLoad::Ignored(format!("{}: unreadable ({e})", path.display()));
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(e) => {
            return SessionLoad::Ignored(format!("{}: malformed JSON ({e})", path.display()));
        }
    };
    let mut record = SessionRecord::default();
    if let Some(label) = value.get("tx_freq_mode").and_then(|v| v.as_str()) {
        match parse_tx_freq_mode(label) {
            Some(mode) => record.tx_freq_mode = Some(mode),
            None => {
                return SessionLoad::Ignored(format!(
                    "{}: unknown tx_freq_mode {label:?}",
                    path.display()
                ));
            }
        }
    }
    if let Some(label) = value.get("decode_effort").and_then(|v| v.as_str()) {
        let choice = match parse_decode_effort(label) {
            Some(choice) => choice,
            None => {
                return SessionLoad::Ignored(format!(
                    "{}: unknown decode_effort {label:?}",
                    path.display()
                ));
            }
        };
        // The baseline is essential to the precedence rule: without a valid
        // one the "config edit is newer" comparison cannot run, so the
        // half-record is rejected rather than silently mis-resolved.
        let config_at_save = value
            .get("decode_effort_config_at_save")
            .and_then(|v| v.as_str())
            .and_then(parse_decode_effort);
        match config_at_save {
            Some(config_at_save) => {
                record.decode_effort = Some(EffortChoice {
                    choice,
                    config_at_save,
                });
            }
            None => {
                return SessionLoad::Ignored(format!(
                    "{}: decode_effort without a valid decode_effort_config_at_save",
                    path.display()
                ));
            }
        }
    }
    SessionLoad::Record(record)
}

/// Resolve the startup values. `config_effort` is the config
/// `[decoder].effort` value currently in effect (the shared snapshot, not the
/// file on disk — `decoder.effort` never hot-applies, so the snapshot is
/// exactly the value the station was running at save time too).
pub(crate) fn resolve(config_effort: DecodeEffort, load: SessionLoad) -> StartupChoices {
    match load {
        SessionLoad::Missing | SessionLoad::Ignored(_) => StartupChoices {
            tx_freq_mode: TxFreqMode::default(),
            decode_effort: config_effort,
            restored_tx_freq_mode: None,
            restored_decode_effort: None,
            superseded_saved_effort: None,
            ignored_reason: match &load {
                SessionLoad::Ignored(reason) => Some(reason.clone()),
                _ => None,
            },
            // A first run writes nothing; an ignored file keeps the
            // operator's next live choice as the fresh starting record.
            record: SessionRecord::default(),
        },
        SessionLoad::Record(record) => {
            let restored_tx_freq_mode = record.tx_freq_mode;
            let (effort, restored, superseded, canonical) = match record.decode_effort {
                None => (config_effort, None, None, None),
                Some(choice) if choice.config_at_save != config_effort => {
                    // The config edit is newer than the saved choice: the
                    // config value wins and becomes the saved choice.
                    (
                        config_effort,
                        None,
                        Some(choice.choice),
                        Some(EffortChoice {
                            choice: config_effort,
                            config_at_save: config_effort,
                        }),
                    )
                }
                Some(choice) => (choice.choice, Some(choice.choice), None, Some(choice)),
            };
            StartupChoices {
                tx_freq_mode: restored_tx_freq_mode.unwrap_or_default(),
                decode_effort: effort,
                restored_tx_freq_mode,
                restored_decode_effort: restored,
                superseded_saved_effort: superseded,
                ignored_reason: None,
                record: SessionRecord {
                    tx_freq_mode: record.tx_freq_mode,
                    decode_effort: canonical,
                },
            }
        }
    }
}

/// Compose the operator-facing startup facts from the resolved choices — one
/// source of truth for the log lines and the TUI messages alike.
pub(crate) fn startup_report(choices: &StartupChoices) -> SessionReport {
    if choices.ignored_reason.is_some() {
        return SessionReport {
            status: None,
            error: Some(
                "saved session choices ignored (corrupt file or unknown value); using defaults"
                    .to_string(),
            ),
        };
    }
    let mut restored = Vec::new();
    if let Some(mode) = choices.restored_tx_freq_mode {
        restored.push(format!("TX frequency mode {}", mode.label()));
    }
    if let Some(effort) = choices.restored_decode_effort {
        restored.push(format!("decode effort {}", effort.label()));
    }
    if !restored.is_empty() {
        return SessionReport {
            status: Some(format!(
                "Session: restored {} from the last session",
                restored.join(" and ")
            )),
            error: None,
        };
    }
    if let Some(old) = choices.superseded_saved_effort {
        return SessionReport {
            status: Some(format!(
                "Session: decode effort {} from config (superseded saved {})",
                choices.decode_effort.label(),
                old.label()
            )),
            error: None,
        };
    }
    SessionReport::default()
}

/// On-disk serialization shape. Field order is the documented flat format
/// (`tx_freq_mode` first); `None` choices serialize as `null`, which
/// [`load`] reads back as absent.
#[derive(serde::Serialize)]
struct SessionRecordOnDisk<'a> {
    tx_freq_mode: Option<&'a str>,
    decode_effort: Option<&'a str>,
    decode_effort_config_at_save: Option<&'a str>,
}

/// Atomically write the record: create the parent directory, write a tmp
/// sibling, rename over the target.
pub(crate) fn save(path: &Path, record: &SessionRecord) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let on_disk = SessionRecordOnDisk {
        tx_freq_mode: record.tx_freq_mode.map(|m| m.label()),
        decode_effort: record.decode_effort.map(|c| c.choice.label()),
        decode_effort_config_at_save: record.decode_effort.map(|c| c.config_at_save.label()),
    };
    let text = serde_json::to_string(&on_disk)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Apply one live mutation to the shared record and rewrite the file
/// best-effort — the single write seam every relay keystroke site (`e`
/// cycle, `f` toggle, `o` set/clear) goes through. A save failure is
/// logged and swallowed: the keystroke still took effect in memory and the
/// echo still goes out; persistence just catches up on the next press.
///
/// Future seam (PAN-270, documented not wired): a hot-apply path for
/// `decoder.effort`, if one is ever added to `config_reload`, must update
/// the record through this same save (choice = applied config value,
/// baseline = the same value) so a live reload behaves like a restart-time
/// config edit.
pub(crate) fn persist_session_choice(
    record: &Arc<std::sync::Mutex<SessionRecord>>,
    path: &Option<PathBuf>,
    update: SessionChoiceUpdate,
) {
    let mut guard = record.lock().unwrap_or_else(|e| e.into_inner());
    match update {
        SessionChoiceUpdate::TxFreqMode(mode) => guard.tx_freq_mode = Some(mode),
        SessionChoiceUpdate::DecodeEffort {
            choice,
            config_at_save,
        } => {
            guard.decode_effort = Some(EffortChoice {
                choice,
                config_at_save,
            });
        }
    }
    let Some(path) = path else { return };
    if let Err(e) = save(path, &guard) {
        warn!(
            "Session: failed to save session choices to {}: {e}",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a session file into a fresh temp dir and return its path
    /// (PAN-194 discipline: every state path in tests resolves a temp dir,
    /// never the real `~/.pancetta`).
    fn session_file(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".pancetta").join("session_state.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        (dir, path)
    }

    #[test]
    fn save_then_load_round_trips_the_record() {
        let (dir, path) = session_file("");
        std::fs::remove_file(&path).unwrap();
        let record = SessionRecord {
            tx_freq_mode: Some(TxFreqMode::Auto),
            decode_effort: Some(EffortChoice {
                choice: DecodeEffort::Max,
                config_at_save: DecodeEffort::Auto,
            }),
        };
        save(&path, &record).unwrap();
        assert_eq!(load(&path), SessionLoad::Record(record));
        drop(dir);
    }

    #[test]
    fn load_on_a_missing_path_is_missing_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session_state.json");
        assert_eq!(load(&path), SessionLoad::Missing);
    }

    #[test]
    fn unreadable_file_is_ignored() {
        // A directory at the path makes read_to_string fail with an I/O
        // error other than NotFound.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session_state.json");
        std::fs::create_dir(&path).unwrap();
        match load(&path) {
            SessionLoad::Ignored(reason) => {
                assert!(reason.contains("unreadable"), "reason: {reason}");
            }
            other => panic!("expected Ignored, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_is_ignored() {
        let (_dir, path) = session_file("{not json");
        match load(&path) {
            SessionLoad::Ignored(reason) => {
                assert!(reason.contains("malformed"), "reason: {reason}");
            }
            other => panic!("expected Ignored, got {other:?}"),
        }
    }

    #[test]
    fn unknown_tx_freq_mode_label_is_ignored() {
        // Ticket scenario 5: a saved value that names something unknown
        // falls back to defaults without failing startup.
        let (_dir, path) = session_file(
            r#"{"tx_freq_mode":"TURBO","decode_effort":"MAX","decode_effort_config_at_save":"AUTO"}"#,
        );
        match load(&path) {
            SessionLoad::Ignored(reason) => {
                assert!(reason.contains("TURBO"), "reason: {reason}");
            }
            other => panic!("expected Ignored, got {other:?}"),
        }
    }

    #[test]
    fn unknown_decode_effort_label_is_ignored() {
        let (_dir, path) =
            session_file(r#"{"decode_effort":"TURBO","decode_effort_config_at_save":"AUTO"}"#);
        match load(&path) {
            SessionLoad::Ignored(reason) => {
                assert!(reason.contains("TURBO"), "reason: {reason}");
            }
            other => panic!("expected Ignored, got {other:?}"),
        }
    }

    #[test]
    fn decode_effort_without_a_valid_baseline_is_ignored() {
        let (_dir, path) = session_file(r#"{"decode_effort":"MAX"}"#);
        match load(&path) {
            SessionLoad::Ignored(reason) => {
                assert!(
                    reason.contains("decode_effort_config_at_save"),
                    "reason: {reason}"
                );
            }
            other => panic!("expected Ignored, got {other:?}"),
        }
    }

    #[test]
    fn structurally_valid_file_with_absent_keys_is_a_record_of_nones() {
        let (_dir, path) = session_file("{}");
        assert_eq!(load(&path), SessionLoad::Record(SessionRecord::default()));
    }

    #[test]
    fn resolve_first_run_uses_defaults_and_writes_nothing() {
        // Ticket scenario 4: no saved session → Hold + config effort, nothing
        // restored.
        let choices = resolve(DecodeEffort::Auto, SessionLoad::Missing);
        assert_eq!(choices.tx_freq_mode, TxFreqMode::Hold);
        assert_eq!(choices.decode_effort, DecodeEffort::Auto);
        assert_eq!(choices.restored_tx_freq_mode, None);
        assert_eq!(choices.restored_decode_effort, None);
        assert_eq!(choices.superseded_saved_effort, None);
        assert_eq!(choices.ignored_reason, None);
        assert_eq!(choices.record, SessionRecord::default());
    }

    #[test]
    fn resolve_restores_saved_auto_tx_and_max_effort_when_baseline_matches() {
        // Ticket scenarios 1 and 2: the last session's choices come back.
        let record = SessionRecord {
            tx_freq_mode: Some(TxFreqMode::Auto),
            decode_effort: Some(EffortChoice {
                choice: DecodeEffort::Max,
                config_at_save: DecodeEffort::Auto,
            }),
        };
        let choices = resolve(DecodeEffort::Auto, SessionLoad::Record(record));
        assert_eq!(choices.tx_freq_mode, TxFreqMode::Auto);
        assert_eq!(choices.decode_effort, DecodeEffort::Max);
        assert_eq!(choices.restored_tx_freq_mode, Some(TxFreqMode::Auto));
        assert_eq!(choices.restored_decode_effort, Some(DecodeEffort::Max));
        assert_eq!(choices.superseded_saved_effort, None);
        // The canonical record keeps the saved choice unchanged.
        assert_eq!(
            choices.record.decode_effort,
            Some(EffortChoice {
                choice: DecodeEffort::Max,
                config_at_save: DecodeEffort::Auto,
            })
        );
    }

    #[test]
    fn resolve_lets_a_newer_config_effort_win_and_folds_it_in() {
        // Ticket scenario 6: the config edit is newer than the saved MAX;
        // DEEP runs and becomes the saved choice.
        let record = SessionRecord {
            tx_freq_mode: Some(TxFreqMode::Hold),
            decode_effort: Some(EffortChoice {
                choice: DecodeEffort::Max,
                config_at_save: DecodeEffort::Auto,
            }),
        };
        let choices = resolve(DecodeEffort::Deep, SessionLoad::Record(record));
        assert_eq!(choices.decode_effort, DecodeEffort::Deep);
        assert_eq!(choices.restored_decode_effort, None);
        assert_eq!(choices.superseded_saved_effort, Some(DecodeEffort::Max));
        assert_eq!(
            choices.record.decode_effort,
            Some(EffortChoice {
                choice: DecodeEffort::Deep,
                config_at_save: DecodeEffort::Deep,
            }),
            "the canonical record must become DEEP/DEEP"
        );
    }

    #[test]
    fn resolve_restores_hold_tx_and_max_effort_independently() {
        let record = SessionRecord {
            tx_freq_mode: Some(TxFreqMode::Hold),
            decode_effort: Some(EffortChoice {
                choice: DecodeEffort::Max,
                config_at_save: DecodeEffort::Auto,
            }),
        };
        let choices = resolve(DecodeEffort::Auto, SessionLoad::Record(record));
        assert_eq!(choices.tx_freq_mode, TxFreqMode::Hold);
        assert_eq!(choices.restored_tx_freq_mode, Some(TxFreqMode::Hold));
        assert_eq!(choices.restored_decode_effort, Some(DecodeEffort::Max));
        assert_eq!(choices.superseded_saved_effort, None);
    }

    #[test]
    fn resolve_an_ignored_file_falls_back_to_defaults() {
        // Ticket scenario 5: corrupt or unknown saved values → Hold + the
        // config effort, startup completes, the caller warns.
        let choices = resolve(
            DecodeEffort::Auto,
            SessionLoad::Ignored("some path: malformed JSON".to_string()),
        );
        assert_eq!(choices.tx_freq_mode, TxFreqMode::Hold);
        assert_eq!(choices.decode_effort, DecodeEffort::Auto);
        assert!(choices.ignored_reason.is_some());
        assert_eq!(choices.record, SessionRecord::default());
    }

    #[test]
    fn save_writes_a_tmp_sibling_then_renames_with_no_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".pancetta").join("session_state.json");
        save(
            &path,
            &SessionRecord {
                tx_freq_mode: Some(TxFreqMode::Auto),
                decode_effort: Some(EffortChoice {
                    choice: DecodeEffort::Max,
                    config_at_save: DecodeEffort::Auto,
                }),
            },
        )
        .unwrap();
        let parent = path.parent().unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp leftovers: {leftovers:?}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            r#"{"tx_freq_mode":"AUTO","decode_effort":"MAX","decode_effort_config_at_save":"AUTO"}"#
        );
    }

    #[test]
    fn save_overwrites_a_previous_record_in_place() {
        let (dir, path) = session_file(
            r#"{"tx_freq_mode":"AUTO","decode_effort":"MAX","decode_effort_config_at_save":"AUTO"}"#,
        );
        save(
            &path,
            &SessionRecord {
                tx_freq_mode: Some(TxFreqMode::Hold),
                decode_effort: None,
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            r#"{"tx_freq_mode":"HOLD","decode_effort":null,"decode_effort_config_at_save":null}"#
        );
        drop(dir);
    }

    #[test]
    fn session_state_path_joins_the_override_home() {
        let base = Path::new("/tmp/pancetta-test-home");
        assert_eq!(
            session_state_path(Some(base)),
            Some(base.join(".pancetta").join("session_state.json"))
        );
    }

    #[test]
    fn session_state_path_without_an_override_is_none_in_test_builds() {
        // PAN-194 discipline: a test with no explicit home override must
        // never resolve (or write) the developer's real ~/.pancetta.
        assert_eq!(session_state_path(None), None);
    }

    #[test]
    fn startup_report_composes_the_restored_line_from_what_restored() {
        // Ticket scenario 3: the startup status names both restored values.
        let choices = resolve(
            DecodeEffort::Auto,
            SessionLoad::Record(SessionRecord {
                tx_freq_mode: Some(TxFreqMode::Auto),
                decode_effort: Some(EffortChoice {
                    choice: DecodeEffort::Max,
                    config_at_save: DecodeEffort::Auto,
                }),
            }),
        );
        let report = startup_report(&choices);
        assert_eq!(
            report.status.as_deref(),
            Some("Session: restored TX frequency mode AUTO and decode effort MAX from the last session")
        );
        assert!(report.error.is_none());
    }

    #[test]
    fn startup_report_effort_only_and_mode_only_variants() {
        let effort_only = resolve(
            DecodeEffort::Auto,
            SessionLoad::Record(SessionRecord {
                tx_freq_mode: None,
                decode_effort: Some(EffortChoice {
                    choice: DecodeEffort::Max,
                    config_at_save: DecodeEffort::Auto,
                }),
            }),
        );
        assert_eq!(
            startup_report(&effort_only).status.as_deref(),
            Some("Session: restored decode effort MAX from the last session")
        );
        let mode_only = resolve(
            DecodeEffort::Auto,
            SessionLoad::Record(SessionRecord {
                tx_freq_mode: Some(TxFreqMode::Auto),
                decode_effort: None,
            }),
        );
        assert_eq!(
            startup_report(&mode_only).status.as_deref(),
            Some("Session: restored TX frequency mode AUTO from the last session")
        );
    }

    #[test]
    fn startup_report_names_the_superseding_config() {
        let choices = resolve(
            DecodeEffort::Deep,
            SessionLoad::Record(SessionRecord {
                tx_freq_mode: None,
                decode_effort: Some(EffortChoice {
                    choice: DecodeEffort::Max,
                    config_at_save: DecodeEffort::Auto,
                }),
            }),
        );
        assert_eq!(
            startup_report(&choices).status.as_deref(),
            Some("Session: decode effort DEEP from config (superseded saved MAX)")
        );
    }

    #[test]
    fn startup_report_on_a_first_run_and_an_ignored_file() {
        let first_run = resolve(DecodeEffort::Auto, SessionLoad::Missing);
        assert!(startup_report(&first_run) == SessionReport::default());
        let ignored = resolve(
            DecodeEffort::Auto,
            SessionLoad::Ignored("x: bad".to_string()),
        );
        let report = startup_report(&ignored);
        assert!(report.status.is_none());
        assert_eq!(
            report.error.as_deref(),
            Some("saved session choices ignored (corrupt file or unknown value); using defaults")
        );
    }

    // ------------------------------------------------------------------
    // Phase 3: the relay's single write seam
    // ------------------------------------------------------------------

    fn shared_record(record: SessionRecord) -> Arc<std::sync::Mutex<SessionRecord>> {
        Arc::new(std::sync::Mutex::new(record))
    }

    fn read_record(path: &Path) -> SessionRecord {
        match load(path) {
            SessionLoad::Record(record) => record,
            other => panic!("expected a parseable file after save, got {other:?}"),
        }
    }

    #[test]
    fn persist_a_tx_freq_mode_toggle_writes_the_new_mode() {
        // Ticket scenario 1's Given: an `f` press during a session leaves
        // tx_freq_mode AUTO on disk.
        let (dir, path) = session_file("");
        std::fs::remove_file(&path).unwrap();
        let record = shared_record(SessionRecord::default());
        persist_session_choice(
            &record,
            &Some(path.clone()),
            SessionChoiceUpdate::TxFreqMode(TxFreqMode::Auto),
        );
        assert_eq!(read_record(&path).tx_freq_mode, Some(TxFreqMode::Auto));
        drop(dir);
    }

    #[test]
    fn persist_offset_set_and_clear_write_hold_then_auto() {
        let (dir, path) = session_file("");
        std::fs::remove_file(&path).unwrap();
        let record = shared_record(SessionRecord::default());
        persist_session_choice(
            &record,
            &Some(path.clone()),
            SessionChoiceUpdate::TxFreqMode(TxFreqMode::Hold),
        );
        assert_eq!(read_record(&path).tx_freq_mode, Some(TxFreqMode::Hold));
        persist_session_choice(
            &record,
            &Some(path.clone()),
            SessionChoiceUpdate::TxFreqMode(TxFreqMode::Auto),
        );
        assert_eq!(read_record(&path).tx_freq_mode, Some(TxFreqMode::Auto));
        drop(dir);
    }

    #[test]
    fn persist_an_effort_cycle_records_the_choice_and_the_running_config() {
        // `e` from AUTO cycles to ECO; the baseline recorded is the relay's
        // RUNNING config effort (passed in by the handler), not the file.
        let (dir, path) = session_file("");
        std::fs::remove_file(&path).unwrap();
        let record = shared_record(SessionRecord::default());
        persist_session_choice(
            &record,
            &Some(path.clone()),
            SessionChoiceUpdate::DecodeEffort {
                choice: DecodeEffort::Eco,
                config_at_save: DecodeEffort::Auto,
            },
        );
        assert_eq!(
            read_record(&path).decode_effort,
            Some(EffortChoice {
                choice: DecodeEffort::Eco,
                config_at_save: DecodeEffort::Auto,
            })
        );
        drop(dir);
    }

    #[test]
    fn a_mode_only_persist_leaves_the_saved_effort_record_untouched() {
        // The baseline must survive an `f`: per-field updates, never a
        // whole-file refresh.
        let (dir, path) = session_file(
            r#"{"tx_freq_mode":"HOLD","decode_effort":"MAX","decode_effort_config_at_save":"AUTO"}"#,
        );
        let record = shared_record(SessionRecord {
            tx_freq_mode: Some(TxFreqMode::Hold),
            decode_effort: Some(EffortChoice {
                choice: DecodeEffort::Max,
                config_at_save: DecodeEffort::Auto,
            }),
        });
        persist_session_choice(
            &record,
            &Some(path.clone()),
            SessionChoiceUpdate::TxFreqMode(TxFreqMode::Auto),
        );
        let saved = read_record(&path);
        assert_eq!(saved.tx_freq_mode, Some(TxFreqMode::Auto));
        assert_eq!(
            saved.decode_effort,
            Some(EffortChoice {
                choice: DecodeEffort::Max,
                config_at_save: DecodeEffort::Auto,
            }),
            "an f press must not refresh the effort baseline"
        );
        drop(dir);
    }

    #[test]
    fn a_save_failure_updates_the_record_but_never_panics() {
        // The session path under an unwritable root: the keystroke still
        // took effect (the shared record updated) and the call returned.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session_state.json");
        // A directory AT the path: create_dir_all succeeds (parent exists),
        // the tmp write lands at session_state.json.tmp, and the rename
        // fails because the target is a non-empty dir — or the write itself
        // fails; either way save errors and must be swallowed.
        std::fs::create_dir(&path).unwrap();
        let record = shared_record(SessionRecord::default());
        persist_session_choice(
            &record,
            &Some(path.clone()),
            SessionChoiceUpdate::TxFreqMode(TxFreqMode::Auto),
        );
        assert_eq!(
            record.lock().unwrap().tx_freq_mode,
            Some(TxFreqMode::Auto),
            "the in-memory record must reflect the keystroke even when the save fails"
        );
    }

    #[test]
    fn persist_without_a_path_only_updates_the_record() {
        // No home resolvable (session_state_path → None): the keystroke
        // still works, nothing is written.
        let record = shared_record(SessionRecord::default());
        persist_session_choice(
            &record,
            &None,
            SessionChoiceUpdate::DecodeEffort {
                choice: DecodeEffort::Max,
                config_at_save: DecodeEffort::Auto,
            },
        );
        assert_eq!(
            record.lock().unwrap().decode_effort,
            Some(EffortChoice {
                choice: DecodeEffort::Max,
                config_at_save: DecodeEffort::Auto,
            })
        );
    }
}
