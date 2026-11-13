//! Parse documented commands without running the station or touching devices.
use super::Cli;
use clap::{error::ErrorKind, CommandFactory, Parser};

const RUNBOOK: &str = include_str!("../../docs/RUNBOOK.md");
const CONFIG: &str = include_str!("../../docs/CONFIG.md");

fn commands(doc: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut shell = false;
    for line in doc.lines() {
        let line = line.trim();
        if line.starts_with("```") {
            shell = !shell && matches!(line, "```sh" | "```bash" | "```shell");
            continue;
        }
        if shell && (line == "pancetta" || line.starts_with("pancetta ")) {
            result.push(line.to_owned());
        }
        // Only inline code, not paths or executable names embedded in prose.
        for snippet in line.split('`').skip(1).step_by(2) {
            if snippet.starts_with("pancetta ") {
                result.push(snippet.to_owned());
            }
        }
    }
    result
}

fn tokens(snippet: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut started = false;
    for c in snippet.chars() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                token.push(c);
            }
        } else {
            match c {
                '\'' | '"' => {
                    started = true;
                    quote = Some(c);
                }
                '|' | ';' | '&' | '$' | '\\' => return Err("unsupported shell syntax".into()),
                c if c.is_whitespace() => {
                    if started {
                        args.push(std::mem::take(&mut token));
                        started = false;
                    }
                }
                _ => {
                    started = true;
                    token.push(c);
                }
            }
        }
    }
    if quote.is_some() {
        return Err("unclosed quote".into());
    }
    if started {
        args.push(token);
    }
    for arg in &mut args {
        if arg == "<file>" {
            *arg = "example.adi".into();
        } else if arg.contains(['<', '>']) {
            return Err("unknown placeholder".into());
        }
    }
    Ok(args)
}

fn check(doc: &str) -> Result<Vec<String>, String> {
    let snippets = commands(doc);
    if snippets.is_empty() {
        return Err("no documented commands found".into());
    }
    for snippet in &snippets {
        // These are explicit partial references; the complete import example is
        // checked separately. --undo requires a receipt ID in the real CLI.
        if matches!(
            snippet.as_str(),
            "pancetta import" | "pancetta import --undo"
        ) {
            continue;
        }
        let args = tokens(snippet).map_err(|e| format!("{snippet}: {e}"))?;
        if let Err(e) = Cli::try_parse_from(&args) {
            // clap stops parsing at help/version. Do not let a trailing typo
            // disappear behind that early return.
            let display = (e.kind() == ErrorKind::DisplayHelp
                && args.last().is_some_and(|a| a == "--help" || a == "-h"))
                || (e.kind() == ErrorKind::DisplayVersion
                    && args.last().is_some_and(|a| a == "--version" || a == "-V"));
            if !display {
                return Err(format!("{snippet}: {e}"));
            }
        }
    }
    Ok(snippets)
}

fn precedence_flags(doc: &str) -> Result<(), String> {
    let paragraph = doc
        .split("CLI flags (")
        .nth(1)
        .ok_or("missing CLI precedence")?
        .split("\n\n")
        .next()
        .ok_or("missing paragraph")?;
    let command = Cli::command();
    let mut count = 0;
    for flag in paragraph.split('`').skip(1).step_by(2) {
        let name = flag.strip_prefix("--").ok_or("expected long CLI flag")?;
        if !command
            .get_arguments()
            .any(|arg| arg.get_long() == Some(name))
        {
            return Err(format!("unknown precedence flag {flag}"));
        }
        count += 1;
    }
    if count == 0 {
        return Err("no precedence flags".into());
    }
    Ok(())
}

#[test]
fn documented_commands_parse() {
    let runbook = check(RUNBOOK).expect("docs/RUNBOOK.md");
    let config = check(CONFIG).expect("docs/CONFIG.md");
    for required in [
        "pancetta test-audio --list",
        "pancetta config --show",
        "pancetta config --validate",
    ] {
        assert!(runbook.iter().any(|s| s == required), "missing {required}");
    }
    assert!(config.iter().any(|s| s == "pancetta config --validate"));
    precedence_flags(CONFIG).expect("docs/CONFIG.md");
}

#[test]
fn extractor_and_parser_fixtures() {
    assert_eq!(
        commands("`pancetta info`\n```sh\npancetta config --show\n```\n`pancetta/src/main.rs`"),
        ["pancetta info", "pancetta config --show"]
    );
    assert_eq!(
        tokens("pancetta --audio-device 'USB CODEC' import <file>").unwrap(),
        [
            "pancetta",
            "--audio-device",
            "USB CODEC",
            "import",
            "example.adi"
        ]
    );
    assert_eq!(
        tokens("pancetta --audio-device \"USB CODEC\" --config ''").unwrap(),
        ["pancetta", "--audio-device", "USB CODEC", "--config", ""]
    );
    assert!(tokens("pancetta info | cat").is_err());
    assert!(tokens("pancetta 'unclosed").is_err());
    assert!(tokens("pancetta import <unknown>").is_err());
    assert!(check("`pancetta --unknown-flag`").is_err());
    assert!(check("`pancetta imaginary-command`").is_err());
    assert!(check("`pancetta --help --unknown-flag`").is_err());
    assert!(check("`pancetta --version imaginary-command`").is_err());
    assert!(check("`pancetta --help`").is_ok());
    assert!(check("`pancetta --version`").is_ok());
}

#[test]
fn actual_document_mutations_are_rejected() {
    assert!(RUNBOOK.contains("pancetta test-audio --list"));
    assert!(
        check(&RUNBOOK.replace("pancetta test-audio --list", "pancetta --list-audio")).is_err()
    );
    assert!(CONFIG.contains("CLI flags ("));
    assert!(precedence_flags(&CONFIG.replace("CLI flags (", "CLI flags (`--no-rig`, ")).is_err());
}
