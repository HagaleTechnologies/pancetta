//! PAN-357: `rig.ptt.method` is refused at validation unless it has a keying
//! path, and `serial_rts` needs a separate, positive-polarity PTT port.

use super::*;

fn rig_with(method: PttMethod, enabled: bool) -> RigConfig {
    let mut rig = RigConfig::default();
    rig.interface.enabled = enabled;
    rig.interface.port = "/dev/ttyUSB0".to_string();
    rig.ptt.method = method;
    rig
}

fn validation_message(rig: &RigConfig) -> String {
    match rig.validate_section() {
        Err(ConfigError::Validation(message)) => message,
        other => panic!("expected ConfigError::Validation, got {other:?}"),
    }
}

fn unimplemented_sentence(name: &str) -> String {
    format!(
        "rig.ptt.method = \"{name}\" is not implemented. Use \"cat\" or \"none\" to key through \
         rigctld, or \"serial_rts\" with rig.ptt.device to key the RTS line of a separate serial \
         port."
    )
}

const ALL_METHODS: [PttMethod; 9] = [
    PttMethod::None,
    PttMethod::Cat,
    PttMethod::Serial,
    PttMethod::Parallel,
    PttMethod::Usb,
    PttMethod::SoundCard,
    PttMethod::Vox,
    PttMethod::Network,
    PttMethod::SerialRts,
];

#[test]
fn unimplemented_methods_are_refused_while_rig_control_is_on() {
    for (method, name) in [
        (PttMethod::Vox, "vox"),
        (PttMethod::Parallel, "parallel"),
        (PttMethod::Usb, "usb"),
        (PttMethod::SoundCard, "sound_card"),
        (PttMethod::Network, "network"),
    ] {
        let rig = rig_with(method.clone(), true);
        assert_eq!(
            validation_message(&rig),
            unimplemented_sentence(name),
            "{method:?} must be refused with the not-implemented sentence"
        );
        let err = rig.validate_section().unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("Validation error: {}", unimplemented_sentence(name))
        );
    }
}

#[test]
fn plain_serial_is_refused_and_points_to_serial_rts() {
    let rig = rig_with(PttMethod::Serial, true);
    assert_eq!(
        validation_message(&rig),
        "rig.ptt.method = \"serial\" is not implemented: it does not say which line keys the \
         rig. Use \"serial_rts\" with rig.ptt.device."
    );
}

#[test]
fn none_and_cat_still_validate() {
    for method in [PttMethod::None, PttMethod::Cat] {
        let rig = rig_with(method.clone(), true);
        assert!(
            rig.validate_section().is_ok(),
            "{method:?} must validate with rig control on: {:?}",
            rig.validate_section()
        );
    }
}

#[test]
fn methods_are_not_checked_while_rig_control_is_off() {
    for method in ALL_METHODS {
        let rig = rig_with(method.clone(), false);
        assert!(
            rig.validate_section().is_ok(),
            "{method:?} must validate with rig control off: {:?}",
            rig.validate_section()
        );
    }
}

#[test]
fn serial_rts_accepts_the_ticket_spelling_and_writes_snake_case() {
    for spelling in ["serial_rts", "serial-rts"] {
        let ptt: PttConfig = toml::from_str(&format!(
            "method = \"{spelling}\"\ndevice = \"/dev/ttyUSB1\"\n"
        ))
        .unwrap_or_else(|e| panic!("{spelling} must parse: {e}"));
        assert_eq!(ptt.method, PttMethod::SerialRts, "{spelling}");
        let written = toml::to_string(&ptt).expect("PttConfig serializes");
        assert!(
            written.contains("method = \"serial_rts\""),
            "a save writes the snake_case name, got:\n{written}"
        );
        assert!(!written.contains("serial-rts"), "{written}");
    }
    assert_eq!(PttMethod::SerialRts.config_name(), "serial_rts");
}

#[test]
fn serial_rts_needs_a_device() {
    let sentence = "rig.ptt.method = \"serial_rts\" needs rig.ptt.device, the serial port whose \
                    RTS line keys the rig.";
    for device in [None, Some(""), Some("   ")] {
        let mut rig = rig_with(PttMethod::SerialRts, true);
        rig.ptt.device = device.map(str::to_string);
        assert_eq!(validation_message(&rig), sentence, "device {device:?}");
    }
}

#[test]
fn serial_rts_refuses_the_cat_port() {
    let mut rig = rig_with(PttMethod::SerialRts, true);
    rig.ptt.device = Some(" /dev/ttyUSB0 ".to_string());
    assert_eq!(
        validation_message(&rig),
        "rig.ptt.device = \"/dev/ttyUSB0\" is the CAT port (rig.interface.port). rigctld owns \
         that port; serial_rts needs a separate PTT port."
    );
}

#[test]
fn serial_rts_refuses_negative_polarity() {
    let mut rig = rig_with(PttMethod::SerialRts, true);
    rig.ptt.device = Some("/dev/ttyUSB1".to_string());
    rig.ptt.polarity = PttPolarity::Negative;
    assert_eq!(
        validation_message(&rig),
        "rig.ptt.polarity = \"negative\" is not implemented for serial_rts: an inverted RTS line \
         would key the rig whenever pancetta is not holding it."
    );
}

#[test]
fn serial_rts_with_a_separate_device_validates() {
    let mut rig = rig_with(PttMethod::SerialRts, true);
    rig.ptt.device = Some("/dev/ttyUSB1".to_string());
    assert!(
        rig.validate_section().is_ok(),
        "{:?}",
        rig.validate_section()
    );
    assert_eq!(rig.ptt.refusal(&rig.interface), None);
}
