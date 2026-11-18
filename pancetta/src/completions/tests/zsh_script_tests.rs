// PAN-377.
use super::*;
use clap::CommandFactory;
#[test]
fn every_subcommand_and_long_flag_is_offered() {
    let mut cmd = crate::Cli::command();
    cmd.build();
    let s = script(Shell::Zsh, cmd.clone());
    for child in cmd.get_subcommands().filter(|c| !c.is_hide_set()) {
        assert!(s.contains(&format!("'{}:", child.get_name())));
        let block = s
            .split(&format!("({})\n", child.get_name()))
            .nth(1)
            .unwrap()
            .split(";;")
            .next()
            .unwrap();
        for arg in child.get_arguments().filter(|a| !a.is_hide_set()) {
            if let Some(long) = arg.get_long() {
                assert!(
                    block.contains(&format!("--{long}")),
                    "missing {long} from {}",
                    child.get_name()
                );
            }
        }
    }
    assert!(s.contains("'completions:"));
    assert!(s.contains("'help:"));
}
#[test]
fn descriptions_are_one_escaped_sentence() {
    assert_eq!(first_sentence("Go somewhere. Then stop"), "Go somewhere");
    assert_eq!(
        first_sentence("e.g. panino — read-only, no network calls"),
        "e.g. panino — read-only, no network calls"
    );
    assert_eq!(
        escape(r"a\b 'c' [d] e:f $g `h`"),
        r"a\\b '\''c'\'' \[d\] e\:f \$g \`h\`"
    );
    let s = script(Shell::Zsh, crate::Cli::command());
    assert!(s.contains("as if it were live audio]:REPLAY:_files'"));
    assert!(s.contains(
        "Inject a single TransmitRequest after startup, then shutdown when it completes"
    ));
    assert!(s.contains(
        "Show this station's agent identity and fingerprint words"
            .replace("'", "'\\''")
            .as_str()
    ));
    assert!(s.contains("Measure receive gain for 1..3600 seconds (600 when omitted)"));
    // A dotted abbreviation before a capital is not a sentence end.
    assert_eq!(
        first_sentence("Only QSOs in this mode, e.g. FT8 (MODE or SUBMODE, ignoring case)"),
        "Only QSOs in this mode, e.g. FT8 (MODE or SUBMODE, ignoring case)"
    );
    assert_eq!(
        first_sentence("Pick one, i.e. FT8. Then stop"),
        "Pick one, i.e. FT8"
    );
    assert!(s.contains(
        "'--mode=[Only QSOs in this mode, e.g. FT8 (MODE or SUBMODE, ignoring case)]:MODE:'"
    ));
    assert!(
        s.contains("--since=[Only export QSOs on or after this date]:YYYY-MM-DD:")
            || s.contains(":YYYY-MM-DD:")
    );
}
#[test]
fn argument_kinds_map_to_zsh_specs() {
    use clap::{Arg, ArgAction, ValueHint};
    let cmd = Command::new("toy")
        .arg(
            Arg::new("config")
                .short('c')
                .long("config")
                .value_parser(clap::value_parser!(std::path::PathBuf))
                .help("Config file"),
        )
        .arg(Arg::new("dir").long("dir").value_hint(ValueHint::DirPath))
        .arg(
            Arg::new("quiet")
                .short('q')
                .long("quiet")
                .action(ArgAction::SetTrue),
        )
        .arg(Arg::new("verbose").short('v').action(ArgAction::Count))
        .arg(Arg::new("tag").long("tag").action(ArgAction::Append))
        .arg(Arg::new("seconds").long("seconds").num_args(0..=1))
        .arg(
            Arg::new("format")
                .long("format")
                .value_parser(["text", "json"]),
        )
        .arg(Arg::new("secret").long("secret").hide(true))
        .subcommand(
            Command::new("go")
                .about("Go somewhere. Then stop")
                .arg(Arg::new("target").required(true))
                .arg(Arg::new("rest").num_args(1..)),
        )
        .subcommand(Command::new("hidden").hide(true));
    let s = script(Shell::Zsh, cmd);
    for spec in [
        "'-c+[Config file]:CONFIG:_files'",
        "'--dir=:DIR:_files -/'",
        "'-q'",
        "'*-v'",
        "'*--tag=:TAG:'",
        "'--seconds=::SECONDS:'",
        "'--format=:FORMAT:(text json)'",
        "':TARGET:'",
        "'*::REST:'",
        "'go:Go somewhere'",
    ] {
        assert!(s.contains(spec), "missing {spec}");
    }
    assert!(!s.contains("secret"));
    assert!(!s.contains("'hidden:"));
    assert!(!s.contains('\x1b'));
}
