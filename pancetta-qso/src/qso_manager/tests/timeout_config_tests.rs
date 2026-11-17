use super::TimeoutConfig;

#[test]
fn timeout_config_default_qso_stall_switch_after_is_4() {
    let config = TimeoutConfig::default();
    assert_eq!(config.qso_stall_switch_after, 4);
}
