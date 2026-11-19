// PAN-376. Formerly a test in the inline module `tests`.
use super::*;

/// PAN-376: only a live station takes the data directory's instance
/// lock; subcommands, `--wav` and `--replay` never do.
#[test]
fn instance_lock_is_taken_only_by_a_live_station() {
    let parse = |args: &[&str]| {
        Cli::try_parse_from(std::iter::once("pancetta").chain(args.iter().copied()))
            .unwrap_or_else(|e| panic!("{args:?}: {e}"))
    };
    for args in [
        &[][..],
        &["run"],
        &["--headless", "--no-audio"],
        &["--data-dir", "/tmp/pancetta-40m", "run"],
    ] {
        assert!(takes_instance_lock(&parse(args)), "{args:?}");
    }
    for args in [
        &["--wav", "x.wav"][..],
        &["--replay", "d"],
        &["export"],
        &["import", "f.adi"],
        &["doctor"],
        &["setup"],
        &["config", "--validate"],
    ] {
        assert!(!takes_instance_lock(&parse(args)), "{args:?}");
    }
}
