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
