//! PAN-381 AC3: both decode-listener keys are restart-required.

use crate::coordinator::config_reload::ReloadApplyPlan;
use pancetta_config::Config;

#[test]
fn accept_decodes_change_is_restart_required() {
    let old = Config::default();
    let mut new = old.clone();
    new.network.wsjtx_udp.accept_decodes = !old.network.wsjtx_udp.accept_decodes;
    let plan = ReloadApplyPlan::decide(&old, &new, false);
    assert_eq!(
        plan.restart_required_keys,
        vec!["network.wsjtx_udp.accept_decodes".to_string()]
    );
    assert!(plan.apply_security.is_empty());
}

#[test]
fn decode_listen_change_is_restart_required() {
    let old = Config::default();
    let mut new = old.clone();
    new.network.wsjtx_udp.decode_listen = "127.0.0.1:2239".to_string();
    let plan = ReloadApplyPlan::decide(&old, &new, false);
    assert_eq!(
        plan.restart_required_keys,
        vec!["network.wsjtx_udp.decode_listen".to_string()]
    );
    assert!(plan.apply_security.is_empty());
}
