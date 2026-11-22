// PAN-359. The operator's own activation reference.
use super::*;

/// PAN-359 AC4: values are trimmed, blanks become `None`, and case is kept.
#[test]
fn new_trims_and_drops_blank_values() {
    let r = ActivationReference::new(Some("  POTA "), Some("\tK-1234\n"));
    assert_eq!(r.my_sig.as_deref(), Some("POTA"));
    assert_eq!(r.my_sig_info.as_deref(), Some("K-1234"));
    assert!(!r.is_empty());

    let r = ActivationReference::new(Some("pota"), Some("k-1234"));
    assert_eq!(r.my_sig.as_deref(), Some("pota"));
    assert_eq!(r.my_sig_info.as_deref(), Some("k-1234"));

    for (sig, info) in [
        (None, None),
        (Some(""), Some("")),
        (Some("   "), Some("\t")),
    ] {
        let r = ActivationReference::new(sig, info);
        assert_eq!(r, ActivationReference::default(), "{sig:?} {info:?}");
        assert!(r.is_empty());
        let mut tags = HashMap::new();
        r.stamp(&mut tags);
        assert!(tags.is_empty(), "{sig:?} {info:?} wrote {tags:?}");
    }

    let r = ActivationReference::new(Some("SOTA"), Some("  "));
    assert_eq!(r.my_sig.as_deref(), Some("SOTA"));
    assert_eq!(r.my_sig_info, None);
}

/// PAN-359 D2: stamping adds each field on its own and never replaces a
/// tag already present; a blank value set directly on the struct writes
/// nothing.
#[test]
fn stamp_never_overwrites_and_skips_blank() {
    let r = ActivationReference::new(Some("POTA"), Some("K-1234"));
    let mut tags = HashMap::from([("MY_SIG_INFO".to_string(), "K-9999".to_string())]);
    r.stamp(&mut tags);
    assert_eq!(tags.get("MY_SIG").map(String::as_str), Some("POTA"));
    assert_eq!(tags.get("MY_SIG_INFO").map(String::as_str), Some("K-9999"));

    let r = ActivationReference {
        my_sig: Some("  ".to_string()),
        my_sig_info: Some(" K-1 ".to_string()),
    };
    let mut tags = HashMap::new();
    r.stamp(&mut tags);
    assert_eq!(tags.len(), 1);
    assert_eq!(tags.get("MY_SIG_INFO").map(String::as_str), Some("K-1"));

    let worked = WorkedReference {
        sig: "POTA".to_string(),
        sig_info: "DA-0123".to_string(),
    };
    let mut tags = HashMap::from([("SIG".to_string(), "WWFF".to_string())]);
    worked.stamp(&mut tags);
    assert_eq!(tags.get("SIG").map(String::as_str), Some("WWFF"));
    assert_eq!(tags.get("SIG_INFO").map(String::as_str), Some("DA-0123"));
}
