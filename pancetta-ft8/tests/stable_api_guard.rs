//! PAN-380: compile-time pin of the stable `pancetta-ft8` API (README,
//! "Stable API and compatibility policy").
//!
//! A change to a pinned signature, field or trait bound makes this file fail
//! to COMPILE, and rustc's error names the item. Change a pin only together
//! with a crate version bump, a CHANGELOG entry, the README list and a
//! dispensa proposal (AGENTS.md, cross-repo contracts).
use pancetta_ft8::{
    decode_cycle, ft8lib_is_available, CycleDecoder, DecodedMessage, Ft8Config, Ft8Decoder,
    Ft8Error, Ft8Result, Protocol, WINDOW_SAMPLES,
};

/// Coerces each function to its pinned fn-pointer type and records its name
/// for the README check. A drift in parameters or return type is E0308, whose
/// note ends in `{<item>}` and whose label underlines the pinned line.
macro_rules! pin_fns {
    ($($item:path : $ty:ty;)+) => {
        $(const _: $ty = $item;)+
        const PINNED_FNS: &[&str] = &[$(stringify!($item)),+];
    };
}

pin_fns! {
    // The window API the dispensa Q-0085 answer names.
    Ft8Decoder::new: fn(Ft8Config) -> Ft8Result<Ft8Decoder>;
    Ft8Decoder::decode_window: fn(&mut Ft8Decoder, &[f32]) -> Ft8Result<Vec<DecodedMessage>>;
    // The cycle API manta calls (dispensa ADR-0017).
    CycleDecoder::new: fn(Protocol) -> Ft8Result<CycleDecoder>;
    CycleDecoder::decode_cycle: fn(&mut CycleDecoder, &[f32]) -> Ft8Result<Vec<DecodedMessage>>;
    decode_cycle: fn(&[f32], Protocol) -> Ft8Result<Vec<DecodedMessage>>;
    ft8lib_is_available: fn() -> bool;
}

/// The non-function stable items, as the README names them.
const PINNED_OTHER: &[&str] = &[
    "Ft8Config::sample_rate",
    "Ft8Config::protocol",
    "Ft8Config::default",
    "DecodedMessage::text",
    "DecodedMessage::snr_db",
    "DecodedMessage::frequency_offset",
    "DecodedMessage::time_offset",
    "Protocol::Ft8",
    "Protocol::Ft4",
    "WINDOW_SAMPLES",
    "Ft8Result",
    "Ft8Error",
];

const _: usize = WINDOW_SAMPLES;
const _: fn(Ft8Error) -> Ft8Result<()> = Err;

fn assert_send<T: Send>() {}
fn assert_clone_debug<T: Clone + std::fmt::Debug>() {}
fn assert_copy_eq<T: Copy + Eq + std::hash::Hash + std::fmt::Debug>() {}

#[test]
fn trait_bounds_are_pinned() {
    assert_send::<CycleDecoder>(); // manta boxes it as `dyn CycleDecode: Send`
    assert_clone_debug::<Ft8Config>();
    assert_clone_debug::<DecodedMessage>();
    assert_copy_eq::<Protocol>();
}

#[test]
fn ft8_config_pinned_fields_and_struct_update_syntax() {
    // `Ft8Config { sample_rate, protocol, .. }` with a `Default` base. The
    // literal pins both field names and types, and fails (E0639) if the struct
    // ever becomes `#[non_exhaustive]`.
    let cfg = Ft8Config {
        sample_rate: 12_000u32,
        protocol: Protocol::Ft8,
        ..Ft8Config::default()
    };
    let _: u32 = cfg.sample_rate;
    let _: Protocol = cfg.protocol;
}

#[test]
fn decoded_message_pinned_fields() {
    fn read(m: &DecodedMessage) -> (&String, f32, f64, f64) {
        (&m.text, m.snr_db, m.frequency_offset, m.time_offset)
    }
    let _ = read;
}

#[test]
fn protocol_pinned_variants() {
    let _ = [Protocol::Ft8, Protocol::Ft4];
}

#[test]
fn pinned_surface_still_decodes() {
    let mut d = Ft8Decoder::new(Ft8Config::default()).expect("Ft8Decoder::new");
    let silence = vec![0.0f32; WINDOW_SAMPLES];
    assert!(d.decode_window(&silence).expect("decode_window").is_empty());
}

#[test]
fn readme_lists_every_pinned_item() {
    let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))
        .expect("pancetta-ft8/README.md is readable");
    let heading = "### Stable API and compatibility policy";
    let start = readme
        .find(heading)
        .unwrap_or_else(|| panic!("README must have a `{heading}` section"));
    let body = &readme[start + heading.len()..];
    let section = &body[..body.find("\n#").unwrap_or(body.len())];
    for item in PINNED_FNS.iter().chain(PINNED_OTHER) {
        assert!(
            section.contains(&format!("`{item}")),
            "README's stable API section must list `{item}`"
        );
    }
    for phrase in [
        "..Ft8Config::default()",
        "minor",
        "#[non_exhaustive]",
        "dispensa",
    ] {
        assert!(
            section.contains(phrase),
            "README's compatibility policy must mention {phrase:?}"
        );
    }
}
