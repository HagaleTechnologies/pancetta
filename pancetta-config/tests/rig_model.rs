//! PAN-340: `rig.model` validation through the shared Hamlib resolver, at
//! the top-level [`Config::validate`] boundary the `pancetta config
//! --validate` CLI uses. The fixture is an otherwise-valid default station
//! so no earlier section validation masks the rig error.

use pancetta_config::Config;

fn config_with_model(model: &str) -> Config {
    let mut config = Config::default();
    config.rig.model = model.to_string();
    config
}

#[test]
fn config_validation_checks_rig_model() {
    // The fixture baseline: the default station validates cleanly.
    Config::default()
        .validate()
        .expect("default config is valid");

    // Accepted values.
    for model in ["Generic", "", "IC-7300", "KX3", "Barrett 950", "1045"] {
        config_with_model(model)
            .validate()
            .unwrap_or_else(|e| panic!("{model:?}: {e}"));
    }

    // Unknown name: the full resolver body reaches the top-level error.
    let error = config_with_model("FT-9999")
        .validate()
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with(
            "Validation error: Unknown rig.model 'FT-9999'. Closest matches: \
             FT-990 (1016), FT-991 (1035), FT-890 (1015)."
        ),
        "{error}"
    );
    assert!(
        error.contains(
            "Run `rigctl -l` to find your model, then set rig.model to its numeric ID, \
             for example \"1045\"."
        ),
        "{error}"
    );

    // Malformed numeric value and ambiguity, through the same boundary.
    let error = config_with_model("0").validate().unwrap_err().to_string();
    assert!(error.contains("Invalid rig.model '0'"), "{error}");
    let error = config_with_model("SW/FM radio")
        .validate()
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("Ambiguous rig.model 'SW/FM radio'"),
        "{error}"
    );

    // An enabled managed station needs a real model: the placeholder stays
    // loadable, and a valid catalog model keeps the whole config valid with
    // CAT on.
    let mut enabled = config_with_model("IC-7300");
    enabled.rig.interface.enabled = true;
    enabled
        .validate()
        .expect("enabled CAT with a catalog model");
    let mut unknown = config_with_model("FT-9999");
    unknown.rig.interface.enabled = true;
    assert!(unknown.validate().is_err());
}
