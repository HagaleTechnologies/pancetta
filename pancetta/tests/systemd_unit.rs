//! Shipped service settings are tested by section, without a running systemd.
use std::collections::BTreeMap;

const UNIT: &str = include_str!("../../packaging/systemd/pancetta.service");
const RUNBOOK: &str = include_str!("../../docs/RUNBOOK.md");

fn settings(input: &str) -> BTreeMap<(String, String), String> {
    let mut section = "";
    let mut values = BTreeMap::new();
    for line in input.lines().map(str::trim) {
        if line.starts_with('[') && line.ends_with(']') {
            section = &line[1..line.len() - 1];
        } else if !line.starts_with('#') {
            if let Some((key, value)) = line.split_once('=') {
                assert!(values
                    .insert((section.into(), key.into()), value.into())
                    .is_none());
            }
        }
    }
    values
}

#[test]
fn unit_has_notify_watchdog_and_shutdown_contract() {
    let values = settings(UNIT);
    for (section, key, expected) in [
        ("Unit", "StartLimitIntervalSec", "60"),
        ("Unit", "StartLimitBurst", "5"),
        ("Service", "Type", "notify"),
        ("Service", "NotifyAccess", "main"),
        ("Service", "WatchdogSec", "60"),
        ("Service", "WatchdogSignal", "SIGTERM"),
        ("Service", "TimeoutStartSec", "120"),
        ("Service", "TimeoutStopSec", "45"),
        ("Service", "KillMode", "mixed"),
        ("Service", "Restart", "always"),
        ("Service", "RestartSec", "2"),
        ("Service", "ExecStart", "%h/.cargo/bin/pancetta --headless"),
        ("Service", "NoNewPrivileges", "true"),
        ("Service", "OOMScoreAdjust", "-200"),
    ] {
        assert_eq!(
            values
                .get(&(section.into(), key.into()))
                .map(String::as_str),
            Some(expected),
            "{section}.{key}"
        );
    }
    assert!(!values.contains_key(&("Service".into(), "StartLimitBurst".into())));
}

#[test]
fn runbook_matches_service_contract() {
    for setting in [
        "Type=notify",
        "NotifyAccess=main",
        "WatchdogSec=60",
        "WatchdogSignal=SIGTERM",
        "TimeoutStartSec=120",
        "TimeoutStopSec=45",
        "KillMode=mixed",
        "Restart=always",
        "RestartSec=2",
    ] {
        assert!(RUNBOOK.contains(setting), "missing setting {setting}");
    }
    for instruction in [
        "systemctl --user daemon-reload",
        "systemctl --user restart pancetta.service",
        "READY=1",
        "SIGSTOP",
        "NRestarts",
        "MainPID",
        "network services",
        "log disk space",
        "microphone permission",
    ] {
        assert!(
            RUNBOOK.contains(instruction),
            "missing operator instruction {instruction}"
        );
    }
    assert!(RUNBOOK.contains("Real systemd restart and macOS TCC qualification remain pending"));
    assert!(RUNBOOK.contains("cannot guarantee physical unkey"));
}
