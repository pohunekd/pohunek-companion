//! The command line grammar. Parsing, `--help`, usage errors (exit code 2) and
//! the completion scripts all derive from this one definition.

use clap::builder::{PossibleValue, TypedValueParser};
use clap::{Parser, Subcommand, ValueEnum};
use pohunek_companion_core::{DialogHost, INTENTS, Intent, decode_dialog_hosts};

use crate::launch::{ASSISTANT_COMMAND, NEW_SESSION_COMMAND};
use crate::{assistant, new_session};

/// Subcommand that installs or removes the desktop entry.
pub const DESKTOP_COMMAND: &str = "desktop";

/// Without a subcommand the sidebar opens.
#[derive(Debug, Parser)]
#[command(name = "pohunek-companion", version, args_override_self = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open the new-session dialog.
    #[command(name = NEW_SESSION_COMMAND)]
    NewSession(new_session::Args),
    /// Open the assistant dialog.
    #[command(name = ASSISTANT_COMMAND)]
    Assistant(assistant::Args),
    /// Install or remove the desktop entry and icons (Linux only).
    #[command(name = DESKTOP_COMMAND, hide = !cfg!(target_os = "linux"))]
    Desktop { action: DesktopAction },
    /// Print a shell completion script to stdout.
    Completions {
        /// Shell to generate the script for.
        shell: CompletionShell,
        /// Print a script that completes `--host`, `--project` and `--group`
        /// from the local daemon and files, instead of the static script.
        #[arg(long)]
        dynamic: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DesktopAction {
    /// Write the desktop entry and icons.
    Install,
    /// Remove them.
    Uninstall,
}

/// Shells with a completion script.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}

/// Rejects an empty value, which no flag of the dialogs accepts.
pub fn non_empty(value: &str) -> Result<String, String> {
    if value.is_empty() {
        Err("a value is required".to_owned())
    } else {
        Ok(value.to_owned())
    }
}

/// Decodes the `--known-hosts` JSON the sidebar passes to a dialog.
pub fn dialog_hosts_json(json: &str) -> Result<Vec<DialogHost>, String> {
    decode_dialog_hosts(json).map_err(|err| err.to_string())
}

/// Parses an assistant intent name and offers the names as completions.
#[derive(Debug, Clone, Copy)]
pub struct IntentParser;

impl TypedValueParser for IntentParser {
    type Value = Intent;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<Intent, clap::Error> {
        let names: Vec<_> = INTENTS.into_iter().map(Intent::as_str).collect();
        let parsed = clap::builder::PossibleValuesParser::new(names).parse_ref(cmd, arg, value)?;
        INTENTS
            .into_iter()
            .find(|intent| intent.as_str() == parsed)
            .ok_or_else(|| clap::Error::new(clap::error::ErrorKind::InvalidValue).with_cmd(cmd))
    }

    fn possible_values(&self) -> Option<Box<dyn Iterator<Item = PossibleValue> + '_>> {
        Some(Box::new(
            INTENTS
                .into_iter()
                .map(|intent| PossibleValue::new(intent.as_str())),
        ))
    }
}

#[cfg(test)]
mod tests {
    use pohunek_companion_core::encode_dialog_hosts;

    use super::*;

    fn hosts() -> Vec<DialogHost> {
        vec![DialogHost {
            target: "netbird:peer~a".to_owned(),
            label: "a".to_owned(),
            unavailable: None,
            route: Some("netbird:peer~a@1".to_owned()),
        }]
    }

    fn parse(subcommand: &str) -> Command {
        let json = encode_dialog_hosts(&hosts()).expect("encode hosts");
        Cli::try_parse_from(["pohunek-companion", subcommand, "--known-hosts", &json])
            .expect("parse the dialog command line")
            .command
            .expect("a subcommand")
    }

    #[test]
    fn dialogs_read_known_hosts_from_one_json_word() {
        match parse(NEW_SESSION_COMMAND) {
            Command::NewSession(args) => assert_eq!(args.known_hosts, Some(hosts())),
            other => panic!("unexpected command {other:?}"),
        }
        match parse(ASSISTANT_COMMAND) {
            Command::Assistant(args) => assert_eq!(args.known_hosts, Some(hosts())),
            other => panic!("unexpected command {other:?}"),
        }
    }
}
