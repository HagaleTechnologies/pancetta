use pancetta_config::loader::ConfigLoader;
use serde_json::{json, Value};
use std::process::Command;

const KEYS: [&str; 9] = [
    "PANCETTA_CALLSIGN",
    "PANCETTA_GRID_SQUARE",
    "PANCETTA_POWER_WATTS",
    "PANCETTA_CAT_PORT",
    "PANCETTA_CAT_BAUD",
    "PANCETTA_AUDIO_INPUT",
    "PANCETTA_AUDIO_OUTPUT",
    "PANCETTA_SAMPLE_RATE",
    "PANCETTA_THEME",
];

#[test]
fn environment_overrides_change_only_target_fields() {
    if std::env::var_os("PAN460_CHILD").is_some() {
        let dir = std::path::PathBuf::from(std::env::var_os("PAN460_DIR").unwrap());
        let loader = ConfigLoader::with_search_paths(vec![dir.clone()]).unwrap();
        let config = loader.load().unwrap();
        assert!(
            loader.load_warnings().is_empty(),
            "{:?}",
            loader.load_warnings()
        );
        std::fs::write(
            dir.join("result.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pancetta.toml"),
        r#"
[station]
callsign = "K1FILE"
grid_square = "FN31pr"
tx_late_max_ms = 12000
[autonomous]
max_concurrent_qsos = 3
[logging]
attribution_comment = true
[network.psk_reporter]
enabled = true
"#,
    )
    .unwrap();

    let run = |overrides: &[(&str, &str)]| -> Value {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg("--exact")
            .arg("environment_overrides_change_only_target_fields")
            .env("PAN460_CHILD", "1")
            .env("PAN460_DIR", dir.path());
        for key in KEYS {
            command.env_remove(key);
        }
        for (key, value) in overrides {
            command.env(key, value);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let mut value: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("result.json")).unwrap())
                .unwrap();
        value.as_object_mut().unwrap().remove("metadata");
        value
    };

    let baseline = run(&[]);
    assert_eq!(baseline["station"]["callsign"], "K1FILE");
    assert_eq!(baseline["autonomous"]["max_concurrent_qsos"], 3);
    assert_eq!(baseline["logging"]["attribution_comment"], true);
    assert_eq!(baseline["network"]["psk_reporter"]["enabled"], true);

    let mut grid_expected = baseline.clone();
    grid_expected["station"]["grid_square"] = json!("EM10");
    assert_eq!(run(&[("PANCETTA_GRID_SQUARE", "EM10")]), grid_expected);

    let mut all_expected = baseline.clone();
    all_expected["station"]["callsign"] = json!("K1ENV");
    all_expected["station"]["grid_square"] = json!("EM10");
    all_expected["station"]["power_watts"] = json!(75);
    all_expected["rig"]["interface"]["port"] = json!("/dev/ttyENV");
    all_expected["rig"]["interface"]["baud_rate"] = json!(19200);
    all_expected["audio"]["input_device"] = json!("input-env");
    all_expected["audio"]["output_device"] = json!("output-env");
    all_expected["audio"]["sample_rate"] = json!(48000);
    all_expected["ui"]["theme"] = json!("light");
    assert_eq!(
        run(&[
            ("PANCETTA_CALLSIGN", "K1ENV"),
            ("PANCETTA_GRID_SQUARE", "EM10"),
            ("PANCETTA_POWER_WATTS", "75"),
            ("PANCETTA_CAT_PORT", "/dev/ttyENV"),
            ("PANCETTA_CAT_BAUD", "19200"),
            ("PANCETTA_AUDIO_INPUT", "input-env"),
            ("PANCETTA_AUDIO_OUTPUT", "output-env"),
            ("PANCETTA_SAMPLE_RATE", "48000"),
            ("PANCETTA_THEME", "light"),
        ]),
        all_expected
    );
    assert_eq!(
        run(&[
            ("PANCETTA_GRID_SQUARE", ""),
            ("PANCETTA_POWER_WATTS", "0"),
            ("PANCETTA_CAT_BAUD", "bad"),
            ("PANCETTA_SAMPLE_RATE", "0"),
        ]),
        baseline
    );
}
