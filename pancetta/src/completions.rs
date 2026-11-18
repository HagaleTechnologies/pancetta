//! PAN-377: generate zsh completions from clap without clap_complete.
use clap::{Arg, ArgAction, Command, ValueHint};
use std::io::{self, Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Shell {
    Zsh,
}

pub(crate) fn script(shell: Shell, cmd: Command) -> String {
    match shell {
        Shell::Zsh => zsh(cmd),
    }
}

pub(crate) fn write_script(out: &mut impl Write, script: &str) -> io::Result<()> {
    match out.write_all(script.as_bytes()).and_then(|()| out.flush()) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

fn first_sentence(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let bytes = collapsed.as_bytes();
    let cut = (0..bytes.len().saturating_sub(2))
        .find(|&i| bytes[i] == b'.' && bytes[i + 1] == b' ' && bytes[i + 2].is_ascii_uppercase())
        .unwrap_or(bytes.len());
    collapsed[..cut].trim_end_matches('.').to_owned()
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\'', "'\\''")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace(':', "\\:")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

fn description(text: Option<&clap::builder::StyledStr>) -> String {
    text.map(|s| format!("[{}]", escape(&first_sentence(&s.to_string()))))
        .unwrap_or_default()
}

fn value_action(arg: &Arg) -> String {
    let values: Vec<_> = arg
        .get_possible_values()
        .into_iter()
        .filter(|v| !v.is_hide_set())
        .map(|v| {
            escape(v.get_name())
                .replace(' ', "\\ ")
                .replace('(', "\\(")
                .replace(')', "\\)")
        })
        .collect();
    if !values.is_empty() {
        return format!("({})", values.join(" "));
    }
    match arg.get_value_hint() {
        ValueHint::AnyPath | ValueHint::FilePath => "_files",
        ValueHint::DirPath => "_files -/",
        ValueHint::ExecutablePath => "_absolute_command_paths",
        ValueHint::CommandName => "_command_names -e",
        ValueHint::Hostname => "_hosts",
        ValueHint::Url => "_urls",
        ValueHint::Username => "_users",
        ValueHint::EmailAddress => "_email_addresses",
        _ => "",
    }
    .to_owned()
}

fn arg_specs(arg: &Arg) -> Vec<String> {
    if arg.is_hide_set() {
        return vec![];
    }
    let desc = description(arg.get_help());
    let name = arg
        .get_value_names()
        .and_then(|n| n.first())
        .map(|n| n.to_string())
        .unwrap_or_else(|| arg.get_id().as_str().to_ascii_uppercase());
    let action = value_action(arg);
    if arg.is_positional() {
        let repeat = arg.get_num_args().is_some_and(|n| n.max_values() > 1)
            || matches!(arg.get_action(), ArgAction::Append);
        let prefix = if repeat {
            "*::"
        } else if arg.is_required_set() {
            ":"
        } else {
            "::"
        };
        let help = arg
            .get_help()
            .map(|s| format!(" -- {}", escape(&first_sentence(&s.to_string()))))
            .unwrap_or_default();
        return vec![format!("'{prefix}{name}{help}:{action}'")];
    }
    let mut names = Vec::new();
    if let Some(short) = arg.get_short() {
        names.push(format!("-{short}"));
    }
    if let Some(long) = arg.get_long() {
        names.push(format!("--{long}"));
    }
    names.extend(
        arg.get_visible_short_aliases()
            .unwrap_or_default()
            .iter()
            .map(|v| format!("-{v}")),
    );
    names.extend(
        arg.get_visible_aliases()
            .unwrap_or_default()
            .iter()
            .map(|v| format!("--{v}")),
    );
    names
        .into_iter()
        .map(|flag| {
            let repeat = if matches!(arg.get_action(), ArgAction::Count | ArgAction::Append) {
                "*"
            } else {
                ""
            };
            if arg.get_action().takes_values() {
                let separator = if flag.starts_with('-') && !flag.starts_with("--") {
                    "+"
                } else {
                    "="
                };
                let colon = if arg.get_num_args().is_some_and(|n| n.min_values() == 0) {
                    "::"
                } else {
                    ":"
                };
                format!("'{repeat}{flag}{separator}{desc}{colon}{name}:{action}'")
            } else {
                format!("'{repeat}{flag}{desc}'")
            }
        })
        .collect()
}

fn function_name(path: &[String]) -> String {
    format!(
        "_{}",
        path.iter()
            .map(|p| p.replace('-', "_"))
            .collect::<Vec<_>>()
            .join("__")
    )
}

fn command_block(cmd: &Command, path: &[String], out: &mut String) {
    out.push_str("_arguments \"${_arguments_options[@]}\" : \\\n");
    let mut args: Vec<_> = cmd.get_arguments().filter(|a| !a.is_positional()).collect();
    let mut positional: Vec<_> = cmd.get_arguments().filter(|a| a.is_positional()).collect();
    positional.sort_by_key(|a| a.get_index());
    args.extend(positional);
    for arg in args {
        for spec in arg_specs(arg) {
            out.push_str(&format!("{spec} \\\n"));
        }
    }
    let children: Vec<_> = cmd.get_subcommands().filter(|c| !c.is_hide_set()).collect();
    if !children.is_empty() {
        out.push_str(&format!(
            "\":: :{}_commands\" \\\n\"*::: :->{}\" \\\n",
            function_name(path),
            path.last().unwrap()
        ));
    }
    out.push_str("&& ret=0\n");
    if children.is_empty() {
        return;
    }
    out.push_str(&format!("    case $state in\n    ({})\n        words=($line[1] \"${{words[@]}}\")\n        (( CURRENT += 1 ))\n        curcontext=\"${{curcontext%:*:*}}:{}-command-$line[1]:\"\n        case $line[1] in\n", path.last().unwrap(), path.join("-")));
    for child in children {
        let mut child_path = path.to_vec();
        child_path.push(child.get_name().to_owned());
        out.push_str(&format!("            ({})\n", child.get_name()));
        command_block(child, &child_path, out);
        out.push_str(";;\n");
    }
    out.push_str("        esac\n    ;;\nesac\n");
}

fn describe_functions(cmd: &Command, path: &[String], out: &mut String) {
    let children: Vec<_> = cmd.get_subcommands().filter(|c| !c.is_hide_set()).collect();
    if !children.is_empty() {
        let name = format!("{}_commands", function_name(path));
        out.push_str(&format!(
            "(( $+functions[{name}] )) ||\n{name}() {{\n    local commands; commands=(\n"
        ));
        for child in &children {
            let desc = child
                .get_about()
                .map(|a| escape(&first_sentence(&a.to_string())))
                .unwrap_or_default();
            out.push_str(&format!("'{}:{}' \\\n", child.get_name(), desc));
        }
        out.push_str(&format!(
            "    )\n    _describe -t commands '{} commands' commands \"$@\"\n}}\n",
            path.join(" ")
        ));
    }
    for child in children {
        let mut p = path.to_vec();
        p.push(child.get_name().to_owned());
        describe_functions(child, &p, out);
    }
}

fn zsh(mut cmd: Command) -> String {
    cmd.build();
    let bin = cmd.get_bin_name().unwrap_or(cmd.get_name()).to_owned();
    let mut out = format!("#compdef {bin}\n\nautoload -U is-at-least\n\n_{}() {{\n    typeset -A opt_args\n    typeset -a _arguments_options\n    local ret=1\n\n    if is-at-least 5.2; then\n        _arguments_options=(-s -S -C)\n    else\n        _arguments_options=(-s -C)\n    fi\n\n    local context curcontext=\"$curcontext\" state line\n", bin.replace('-', "_"));
    command_block(&cmd, &[bin.clone()], &mut out);
    out.push_str("}\n\n");
    describe_functions(&cmd, &[bin.clone()], &mut out);
    out.push_str(&format!("\nif [ \"$funcstack[1]\" = \"_{}\" ]; then\n    _{} \"$@\"\nelse\n    compdef _{} {}\nfi\n", bin, bin, bin, bin));
    out
}

#[cfg(test)]
#[path = "completions/tests/zsh_script_tests.rs"]
mod zsh_script_tests;
