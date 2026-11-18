// PAN-376. Formerly a test in the inline module `wav_recording_gate_tests`.
use super::wav_recording_gate_tests::{home, rec};
use super::*;

#[test]
fn an_empty_recording_directory_uses_the_data_dir_and_tilde_uses_home() {
    // PAN-376: with `--data-dir D`, an empty directory records to
    // `D/recordings`, while `~` in the configured value still expands
    // against the home directory.
    let custom = std::path::PathBuf::from("/srv/pancetta-40m");
    for directory in ["", "   "] {
        assert_eq!(
            resolve_wav_recording(
                &rec(true, directory),
                false,
                false,
                Some(&home()),
                Some(&custom)
            ),
            WavRecording::Enabled {
                dir: custom.join("recordings")
            },
            "directory = {directory:?}"
        );
    }
    assert_eq!(
        resolve_wav_recording(
            &rec(true, "~/rec"),
            false,
            false,
            Some(&home()),
            Some(&custom)
        ),
        WavRecording::Enabled {
            dir: home().join("rec")
        }
    );
    // No data directory and an empty value: recording stays off.
    assert_eq!(
        resolve_wav_recording(&rec(true, ""), false, false, Some(&home()), None),
        WavRecording::NoHomeDir {
            directory: String::new()
        }
    );
}

#[test]
fn omitted_recording_directory_uses_each_stations_data_dir() {
    assert!(pancetta_config::RecordingConfig::default()
        .directory
        .is_empty());
    let root = tempfile::tempdir().unwrap();
    let home = home();
    for (station, directory) in [
        ("20m", ""),
        ("40m", ""),
        (
            "explicit",
            "directory = \"~/Documents/Pancetta/Recordings\"",
        ),
    ] {
        let data_dir = root.path().join(station);
        std::fs::create_dir(&data_dir).unwrap();
        let config_path = data_dir.join("config.toml");
        std::fs::write(
            &config_path,
            format!("[audio.recording]\nauto_record = true\n{directory}\n"),
        )
        .unwrap();
        let (config, _, files) =
            pancetta_config::Config::load_from_dir_with_warnings_and_files(&data_dir).unwrap();
        assert_eq!(files, [config_path]);
        let expected = if directory.is_empty() {
            data_dir.join("recordings")
        } else {
            home.join("Documents/Pancetta/Recordings")
        };
        assert_eq!(
            resolve_wav_recording(
                &config.audio.recording,
                false,
                false,
                Some(&home),
                Some(&data_dir),
            ),
            WavRecording::Enabled { dir: expected },
            "{station}",
        );
    }
}
