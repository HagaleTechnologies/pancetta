// PAN-389. Waterfall palette and gain choices in the session file: persist,
// restore, per-field config precedence and bad-record isolation. Formerly
// part of the inline `tests` module in `session_state.rs`.
use super::tests::{read_record, session_file, shared_record};
use super::*;

#[test]
fn a_full_record_with_both_waterfall_pairs_round_trips() {
    let (dir, path) = session_file("");
    std::fs::remove_file(&path).unwrap();
    // Save the full record over an already-saved PAN-270 record, as the
    // round trip did when it lived in `save_then_load_round_trips_the_record`.
    save(
        &path,
        &SessionRecord {
            tx_freq_mode: Some(TxFreqMode::Auto),
            decode_effort: Some(EffortChoice {
                choice: DecodeEffort::Max,
                config_at_save: DecodeEffort::Auto,
            }),
            ..Default::default()
        },
    )
    .unwrap();
    let full = SessionRecord {
        tx_drive_by_band: Default::default(),
        tx_freq_mode: Some(TxFreqMode::Auto),
        decode_effort: Some(EffortChoice {
            choice: DecodeEffort::Max,
            config_at_save: DecodeEffort::Auto,
        }),
        waterfall_palette: Some(WaterfallPaletteChoice {
            choice: WaterfallPalette::Classic,
            config_at_save: WaterfallPalette::Viridis,
        }),
        waterfall_gain_db: Some(WaterfallGainChoice {
            choice: 8.0,
            config_at_save: 6.0,
        }),
    };
    save(&path, &full).unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        r#"{"tx_freq_mode":"AUTO","decode_effort":"MAX","decode_effort_config_at_save":"AUTO","waterfall_palette":"classic","waterfall_palette_config_at_save":"viridis","waterfall_gain_db":8.0,"waterfall_gain_db_config_at_save":6.0}"#
    );
    assert_eq!(load(&path), SessionLoad::Record(full));
    for gain in [6.0f32, 2.5, -0.1, 0.1, 11.9] {
        let record = SessionRecord {
            waterfall_gain_db: Some(WaterfallGainChoice {
                choice: gain,
                config_at_save: gain,
            }),
            ..Default::default()
        };
        save(&path, &record).unwrap();
        assert_eq!(load(&path), SessionLoad::Record(record), "gain {gain}");
    }
    drop(dir);
}

fn viridis_six() -> WaterfallDisplayConfig {
    WaterfallDisplayConfig {
        palette: WaterfallPalette::Viridis,
        gain_db: 6.0,
    }
}

fn effort_max_auto() -> Option<EffortChoice> {
    Some(EffortChoice {
        choice: DecodeEffort::Max,
        config_at_save: DecodeEffort::Auto,
    })
}

#[test]
fn persist_a_waterfall_gain_records_the_choice_and_the_running_config() {
    let (dir, path) = session_file("");
    std::fs::remove_file(&path).unwrap();
    let record = shared_record(SessionRecord::default());
    persist_session_choice(
        &record,
        &Some(path.clone()),
        SessionChoiceUpdate::WaterfallGain {
            choice: 7.0,
            config_at_save: 6.0,
        },
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains(r#""waterfall_gain_db":7.0,"waterfall_gain_db_config_at_save":6.0"#),
        "{text}"
    );
    assert_eq!(
        read_record(&path).waterfall_gain_db,
        Some(WaterfallGainChoice {
            choice: 7.0,
            config_at_save: 6.0,
        })
    );
    drop(dir);
}

#[test]
fn persist_a_waterfall_palette_records_the_choice_and_the_running_config() {
    let (dir, path) = session_file("");
    std::fs::remove_file(&path).unwrap();
    let record = shared_record(SessionRecord::default());
    persist_session_choice(
        &record,
        &Some(path.clone()),
        SessionChoiceUpdate::WaterfallPalette {
            choice: WaterfallPalette::Thermal,
            config_at_save: WaterfallPalette::Viridis,
        },
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains(
            r#""waterfall_palette":"thermal","waterfall_palette_config_at_save":"viridis""#
        ),
        "{text}"
    );
    assert_eq!(
        read_record(&path).waterfall_palette,
        Some(WaterfallPaletteChoice {
            choice: WaterfallPalette::Thermal,
            config_at_save: WaterfallPalette::Viridis,
        })
    );
    drop(dir);
}

#[test]
fn waterfall_persists_leave_the_other_records_untouched() {
    let (dir, path) = session_file("");
    std::fs::remove_file(&path).unwrap();
    let palette = Some(WaterfallPaletteChoice {
        choice: WaterfallPalette::Spectrum,
        config_at_save: WaterfallPalette::Classic,
    });
    let gain = Some(WaterfallGainChoice {
        choice: -3.0,
        config_at_save: 0.0,
    });
    let start = SessionRecord {
        tx_drive_by_band: Default::default(),
        tx_freq_mode: Some(TxFreqMode::Auto),
        decode_effort: effort_max_auto(),
        waterfall_palette: palette,
        waterfall_gain_db: gain,
    };
    let record = shared_record(start.clone());
    persist_session_choice(
        &record,
        &Some(path.clone()),
        SessionChoiceUpdate::WaterfallGain {
            choice: 4.0,
            config_at_save: 0.0,
        },
    );
    let saved = read_record(&path);
    assert_eq!(saved.tx_freq_mode, start.tx_freq_mode);
    assert_eq!(saved.decode_effort, start.decode_effort);
    assert_eq!(
        saved.waterfall_palette, palette,
        "a gain persist keeps the palette"
    );
    assert_eq!(saved.waterfall_gain_db.map(|g| g.choice), Some(4.0));

    persist_session_choice(
        &record,
        &Some(path.clone()),
        SessionChoiceUpdate::WaterfallPalette {
            choice: WaterfallPalette::Viridis,
            config_at_save: WaterfallPalette::Classic,
        },
    );
    let saved = read_record(&path);
    assert_eq!(saved.tx_freq_mode, start.tx_freq_mode);
    assert_eq!(saved.decode_effort, start.decode_effort);
    assert_eq!(
        saved.waterfall_gain_db,
        Some(WaterfallGainChoice {
            choice: 4.0,
            config_at_save: 0.0,
        }),
        "a palette persist keeps the gain"
    );
    assert_eq!(
        saved.waterfall_palette.map(|p| p.choice),
        Some(WaterfallPalette::Viridis)
    );
    drop(dir);
}

#[test]
fn persist_waterfall_gain_ignores_non_finite_and_clamps() {
    let (dir, path) = session_file("");
    std::fs::remove_file(&path).unwrap();
    let record = shared_record(SessionRecord::default());
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        persist_session_choice(
            &record,
            &Some(path.clone()),
            SessionChoiceUpdate::WaterfallGain {
                choice: bad,
                config_at_save: 0.0,
            },
        );
        assert_eq!(record.lock().unwrap().waterfall_gain_db, None, "{bad}");
        assert!(!path.exists(), "{bad}: nothing may be written");
    }
    persist_session_choice(
        &record,
        &Some(path.clone()),
        SessionChoiceUpdate::WaterfallGain {
            choice: 40.0,
            config_at_save: 0.0,
        },
    );
    assert_eq!(
        read_record(&path).waterfall_gain_db.map(|g| g.choice),
        Some(12.0)
    );
    persist_session_choice(
        &record,
        &Some(path.clone()),
        SessionChoiceUpdate::WaterfallGain {
            choice: -40.0,
            config_at_save: 0.0,
        },
    );
    assert_eq!(
        read_record(&path).waterfall_gain_db.map(|g| g.choice),
        Some(-12.0)
    );
    drop(dir);
}

#[test]
fn resolve_restores_saved_waterfall_choices_when_baselines_match() {
    let record = SessionRecord {
        waterfall_palette: Some(WaterfallPaletteChoice {
            choice: WaterfallPalette::Classic,
            config_at_save: WaterfallPalette::Viridis,
        }),
        waterfall_gain_db: Some(WaterfallGainChoice {
            choice: 8.0,
            config_at_save: 6.0,
        }),
        ..Default::default()
    };
    let choices = resolve(
        DecodeEffort::Auto,
        &viridis_six(),
        SessionLoad::Record(record.clone()),
    );
    assert_eq!(
        choices.waterfall,
        WaterfallDisplayConfig {
            palette: WaterfallPalette::Classic,
            gain_db: 8.0,
        }
    );
    assert_eq!(
        choices.restored_waterfall_palette,
        Some(WaterfallPalette::Classic)
    );
    assert_eq!(choices.restored_waterfall_gain_db, Some(8.0));
    assert!(!choices.superseded_saved_waterfall);
    assert!(!choices.needs_rewrite());
    assert_eq!(choices.record, record);
}

#[test]
fn resolve_lets_a_newer_config_waterfall_value_win_per_field() {
    let palette = Some(WaterfallPaletteChoice {
        choice: WaterfallPalette::Classic,
        config_at_save: WaterfallPalette::Viridis,
    });
    let record = SessionRecord {
        waterfall_palette: palette,
        waterfall_gain_db: Some(WaterfallGainChoice {
            choice: 8.0,
            config_at_save: 6.0,
        }),
        ..Default::default()
    };
    let config = WaterfallDisplayConfig {
        palette: WaterfallPalette::Viridis,
        gain_db: 3.0,
    };
    let choices = resolve(DecodeEffort::Auto, &config, SessionLoad::Record(record));
    assert_eq!(choices.waterfall.gain_db, 3.0, "the newer config gain wins");
    assert_eq!(
        choices.waterfall.palette,
        WaterfallPalette::Classic,
        "the matching palette record is still restored"
    );
    assert_eq!(choices.restored_waterfall_gain_db, None);
    assert_eq!(
        choices.restored_waterfall_palette,
        Some(WaterfallPalette::Classic)
    );
    assert!(choices.superseded_saved_waterfall);
    assert!(choices.needs_rewrite());
    assert_eq!(choices.record.waterfall_gain_db, None);
    assert_eq!(choices.record.waterfall_palette, palette);

    // A second restart keeps the config gain, now as config.
    let second = resolve(
        DecodeEffort::Auto,
        &config,
        SessionLoad::Record(choices.record),
    );
    assert_eq!(second.waterfall.gain_db, 3.0);
    assert_eq!(second.restored_waterfall_gain_db, None);
    assert!(!second.needs_rewrite());

    // A palette baseline mismatch supersedes the palette alone.
    let record = SessionRecord {
        waterfall_palette: palette,
        waterfall_gain_db: Some(WaterfallGainChoice {
            choice: 8.0,
            config_at_save: 6.0,
        }),
        ..Default::default()
    };
    let config = WaterfallDisplayConfig {
        palette: WaterfallPalette::Thermal,
        gain_db: 6.0,
    };
    let choices = resolve(DecodeEffort::Auto, &config, SessionLoad::Record(record));
    assert_eq!(choices.waterfall.palette, WaterfallPalette::Thermal);
    assert_eq!(choices.waterfall.gain_db, 8.0);
    assert_eq!(choices.record.waterfall_palette, None);
    assert!(choices.needs_rewrite());
}

#[test]
fn effective_waterfall_prefers_a_kept_saved_choice_over_config() {
    let record = SessionRecord {
        waterfall_palette: Some(WaterfallPaletteChoice {
            choice: WaterfallPalette::Thermal,
            config_at_save: WaterfallPalette::Viridis,
        }),
        waterfall_gain_db: Some(WaterfallGainChoice {
            choice: -2.0,
            config_at_save: 6.0,
        }),
        ..Default::default()
    };
    assert_eq!(
        effective_waterfall(&record, &viridis_six()),
        WaterfallDisplayConfig {
            palette: WaterfallPalette::Thermal,
            gain_db: -2.0,
        }
    );
    // One saved field, the other from config.
    let gain_only = SessionRecord {
        waterfall_gain_db: Some(WaterfallGainChoice {
            choice: -2.0,
            config_at_save: 6.0,
        }),
        ..Default::default()
    };
    assert_eq!(
        effective_waterfall(&gain_only, &viridis_six()),
        WaterfallDisplayConfig {
            palette: WaterfallPalette::Viridis,
            gain_db: -2.0,
        }
    );
}

#[test]
fn effective_waterfall_without_saved_choices_is_the_config() {
    let record = SessionRecord {
        tx_freq_mode: Some(TxFreqMode::Auto),
        decode_effort: effort_max_auto(),
        ..Default::default()
    };
    assert_eq!(effective_waterfall(&record, &viridis_six()), viridis_six());
    let choices = resolve(DecodeEffort::Auto, &viridis_six(), SessionLoad::Missing);
    assert_eq!(choices.waterfall, viridis_six());
    assert_eq!(
        effective_waterfall(&choices.record, &viridis_six()),
        choices.waterfall
    );
}

#[test]
fn startup_report_names_restored_waterfall_choices() {
    let waterfall_only = resolve(
        DecodeEffort::Auto,
        &viridis_six(),
        SessionLoad::Record(SessionRecord {
            tx_drive_by_band: Default::default(),
            waterfall_palette: Some(WaterfallPaletteChoice {
                choice: WaterfallPalette::Viridis,
                config_at_save: WaterfallPalette::Viridis,
            }),
            waterfall_gain_db: Some(WaterfallGainChoice {
                choice: 6.0,
                config_at_save: 6.0,
            }),
            ..Default::default()
        }),
    );
    assert_eq!(
        startup_report(&waterfall_only).status.as_deref(),
        Some("Session: restored waterfall palette viridis and waterfall gain +6 dB from the last session")
    );
    let all_four = resolve(
        DecodeEffort::Auto,
        &viridis_six(),
        SessionLoad::Record(SessionRecord {
            tx_drive_by_band: Default::default(),
            tx_freq_mode: Some(TxFreqMode::Auto),
            decode_effort: effort_max_auto(),
            waterfall_palette: Some(WaterfallPaletteChoice {
                choice: WaterfallPalette::Viridis,
                config_at_save: WaterfallPalette::Viridis,
            }),
            waterfall_gain_db: Some(WaterfallGainChoice {
                choice: 6.0,
                config_at_save: 6.0,
            }),
        }),
    );
    assert_eq!(
        startup_report(&all_four).status.as_deref(),
        Some("Session: restored TX frequency mode AUTO, decode effort MAX, waterfall palette viridis and waterfall gain +6 dB from the last session")
    );
}

#[test]
fn a_bad_waterfall_record_is_dropped_without_ignoring_the_file() {
    let base =
        r#""tx_freq_mode":"AUTO","decode_effort":"MAX","decode_effort_config_at_save":"AUTO""#;
    let good_palette =
        r#""waterfall_palette":"thermal","waterfall_palette_config_at_save":"classic""#;
    let good_gain = r#""waterfall_gain_db":3.0,"waterfall_gain_db_config_at_save":0.0"#;
    // (extra JSON, palette expected kept, gain expected kept)
    let cases = [
        (
            format!(
                r#"{base},"waterfall_palette":"inferno","waterfall_palette_config_at_save":"classic",{good_gain}"#
            ),
            false,
            true,
        ),
        (
            format!(
                r#"{base},{good_palette},"waterfall_gain_db":"loud","waterfall_gain_db_config_at_save":0.0"#
            ),
            true,
            false,
        ),
        (
            format!(
                r#"{base},{good_palette},"waterfall_gain_db":40.0,"waterfall_gain_db_config_at_save":0.0"#
            ),
            true,
            false,
        ),
        (
            format!(r#"{base},{good_palette},"waterfall_gain_db":3.0"#),
            true,
            false,
        ),
        (
            format!(r#"{base},"waterfall_palette":"thermal",{good_gain}"#),
            false,
            true,
        ),
        (
            format!(
                r#"{base},"waterfall_palette":7,"waterfall_palette_config_at_save":"classic",{good_gain}"#
            ),
            false,
            true,
        ),
    ];
    for (body, palette_kept, gain_kept) in cases {
        let json = format!("{{{body}}}");
        let (_dir, path) = session_file(&json);
        let record = match load(&path) {
            SessionLoad::Record(record) => record,
            other => {
                panic!("{json}: a bad waterfall value must not ignore the file: {other:?}")
            }
        };
        assert_eq!(record.tx_freq_mode, Some(TxFreqMode::Auto), "{json}");
        assert_eq!(record.decode_effort, effort_max_auto(), "{json}");
        assert_eq!(record.waterfall_palette.is_some(), palette_kept, "{json}");
        assert_eq!(record.waterfall_gain_db.is_some(), gain_kept, "{json}");
    }
}

#[test]
fn a_record_without_waterfall_choices_saves_the_pan270_format() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".pancetta").join("session_state.json");
    save(
        &path,
        &SessionRecord {
            tx_drive_by_band: Default::default(),
            tx_freq_mode: Some(TxFreqMode::Auto),
            decode_effort: None,
            waterfall_palette: None,
            waterfall_gain_db: None,
        },
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        r#"{"tx_freq_mode":"AUTO","decode_effort":null,"decode_effort_config_at_save":null}"#
    );
}
