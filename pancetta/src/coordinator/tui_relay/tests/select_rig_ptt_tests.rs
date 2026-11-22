//! PAN-357: the rig picker refuses PTT settings validation would reject.

use super::*;
use pancetta_config::rig::{PttMethod, RigConfig};

fn live(device: Option<&str>) -> RigConfig {
    let mut rig = RigConfig::default();
    rig.interface.enabled = true;
    rig.interface.port = "/dev/ttyUSB0".into();
    rig.ptt.method = PttMethod::Cat;
    rig.ptt.device = device.map(str::to_string);
    rig
}

#[test]
fn select_rig_ptt_refusal_names_the_rule() {
    assert_eq!(
        select_rig_ptt_refusal(&live(None), "/dev/ttyUSB0", &PttMethod::SerialRts).as_deref(),
        Some(
            "rig.ptt.method = \"serial_rts\" needs rig.ptt.device, the serial port whose \
             RTS line keys the rig."
        )
    );
    // Picking the PTT port as the CAT port collides with rig.ptt.device.
    assert_eq!(
        select_rig_ptt_refusal(
            &live(Some("/dev/ttyUSB1")),
            "/dev/ttyUSB1",
            &PttMethod::SerialRts
        )
        .as_deref(),
        Some(
            "rig.ptt.device = \"/dev/ttyUSB1\" is the CAT port (rig.interface.port). \
             rigctld owns that port; serial_rts needs a separate PTT port."
        )
    );
    assert!(
        select_rig_ptt_refusal(&live(None), "/dev/ttyUSB0", &PttMethod::Vox)
            .is_some_and(|r| r.contains("\"vox\" is not implemented"))
    );
}

#[test]
fn select_rig_ptt_refusal_allows_valid_choices() {
    for method in [PttMethod::Cat, PttMethod::None] {
        assert_eq!(
            select_rig_ptt_refusal(&live(None), "/dev/ttyUSB0", &method),
            None
        );
    }
    assert_eq!(
        select_rig_ptt_refusal(
            &live(Some("/dev/ttyUSB1")),
            "/dev/ttyUSB0",
            &PttMethod::SerialRts
        ),
        None
    );
    // With rig control off the method is not checked.
    let mut off = live(None);
    off.interface.enabled = false;
    assert_eq!(
        select_rig_ptt_refusal(&off, "/dev/ttyUSB0", &PttMethod::SerialRts),
        None
    );
}
