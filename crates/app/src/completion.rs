//! Shell completion for `pohunek-companion`.
//!
//! Static scripts come from the clap command tree. Dynamic scripts register
//! clap's environment completer, which also offers live values for `--host`,
//! `--project` and `--group`. Every lookup shares one short deadline and any
//! failure yields no candidates: pressing Tab must never print an error or
//! write anything.

use std::ffi::OsStr;
use std::future::Future;
use std::io::{self, Write};
use std::time::Duration;

use clap::CommandFactory;
use clap_complete::engine::ValueCompleter;
use clap_complete::{ArgValueCompleter, CompleteEnv, CompletionCandidate};
use pohunek_companion_core::{
    Config, LOCAL_HOST, LayoutStore, dialog_hosts, discover_records, layout_path, load_choices,
    plan_discovery,
};

use crate::cli::{Cli, CompletionShell};
use crate::launch::{ASSISTANT_COMMAND, NEW_SESSION_COMMAND};

/// Name the binary is installed under and the scripts complete.
const BIN_NAME: &str = "pohunek-companion";

/// Environment variable of clap's dynamic completion protocol.
const COMPLETE_ENV: &str = "POHUNEK_COMPANION_COMPLETE";

/// All daemon and file lookups of one Tab press share this deadline.
const COMPLETION_DEADLINE: Duration = Duration::from_millis(750);

impl CompletionShell {
    const fn aot(self) -> clap_complete::aot::Shell {
        match self {
            Self::Bash => clap_complete::aot::Shell::Bash,
            Self::Zsh => clap_complete::aot::Shell::Zsh,
            Self::Fish => clap_complete::aot::Shell::Fish,
        }
    }
}

/// Lets clap answer a dynamic completion request before normal parsing. A
/// no-op unless [`COMPLETE_ENV`] is set; in completion mode clap prints the
/// registration or the candidates and exits.
pub fn complete_env() {
    CompleteEnv::with_factory(dynamic_command)
        .var(COMPLETE_ENV)
        .bin(BIN_NAME)
        .complete();
}

/// Prints the completion script for `shell` to stdout.
pub fn run(shell: CompletionShell, dynamic: bool) -> io::Result<()> {
    io::stdout().write_all(&render_script(shell, dynamic))
}

fn render_script(shell: CompletionShell, dynamic: bool) -> Vec<u8> {
    if dynamic {
        return dynamic_bootstrap(shell).into_bytes();
    }
    let mut output = Vec::new();
    let mut command = without_hidden_args(&Cli::command());
    clap_complete::aot::generate(shell.aot(), &mut command, BIN_NAME, &mut output);
    match shell {
        CompletionShell::Bash => fix_bash_names(&output),
        CompletionShell::Zsh | CompletionShell::Fish => output,
    }
}

/// For a hyphenated binary name the bash generator spells the root state
/// `pohunek__companion` where it tracks the typed words but
/// `pohunek__subcmd__companion` in the per-command cases, so no subcommand's
/// flags would complete. Both places use the first spelling.
fn fix_bash_names(script: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(script)
        .replace("pohunek__subcmd__companion", "pohunek__companion")
        .into_bytes()
}

/// The static generators list hidden arguments and subcommands, so the
/// internal flags the sidebar passes to its dialogs and the platform-only
/// subcommands are dropped from the tree they see.
fn without_hidden_args(command: &clap::Command) -> clap::Command {
    let mut rebuilt = clap::Command::new(command.get_name().to_owned())
        .args(
            command
                .get_arguments()
                .filter(|arg| !arg.is_hide_set())
                .cloned(),
        )
        .subcommands(
            command
                .get_subcommands()
                .filter(|sub| !sub.is_hide_set())
                .map(without_hidden_args),
        );
    if let Some(version) = command.get_version() {
        rebuilt = rebuilt.version(version.to_owned());
    }
    if let Some(about) = command.get_about() {
        rebuilt = rebuilt.about(about.clone());
    }
    rebuilt.hide(command.is_hide_set())
}

fn dynamic_bootstrap(shell: CompletionShell) -> String {
    match shell {
        CompletionShell::Bash => format!("source <({COMPLETE_ENV}=bash {BIN_NAME})\n"),
        CompletionShell::Zsh => format!("source <({COMPLETE_ENV}=zsh {BIN_NAME})\n"),
        CompletionShell::Fish => format!("{COMPLETE_ENV}=fish {BIN_NAME} | source\n"),
    }
}

/// The command tree with live completers on the dialog flags.
fn dynamic_command() -> clap::Command {
    Cli::command()
        .mut_subcommand(NEW_SESSION_COMMAND, |command| {
            command
                .mut_arg("host", |arg| arg.add(ArgValueCompleter::new(HostCompleter)))
                .mut_arg("project", |arg| {
                    arg.add(ArgValueCompleter::new(ProjectCompleter))
                })
                .mut_arg("group", |arg| {
                    arg.add(ArgValueCompleter::new(GroupCompleter))
                })
        })
        .mut_subcommand(ASSISTANT_COMMAND, |command| {
            command
                .mut_arg("host", |arg| arg.add(ArgValueCompleter::new(HostCompleter)))
                .mut_arg("project", |arg| {
                    arg.add(ArgValueCompleter::new(ProjectCompleter))
                })
        })
}

/// `local`, the configured `[[hosts]]` targets, and the peers the local daemon
/// discovers.
#[derive(Clone, Copy, Debug)]
struct HostCompleter;

impl ValueCompleter for HostCompleter {
    fn complete(&self, current: &OsStr) -> Vec<CompletionCandidate> {
        let Some(current) = current.to_str() else {
            return Vec::new();
        };
        let targets = host_targets();
        with_prefix(current, targets.into_iter().map(|target| (target, None)))
    }
}

/// `local` and the configured hosts come from the file alone; only the peer
/// discovery asks the daemon, so a slow daemon still leaves them.
fn host_targets() -> Vec<String> {
    let mut targets = vec![LOCAL_HOST.to_owned()];
    let Ok(config) = Config::load() else {
        return targets;
    };
    let configured = config.hosts.clone();
    let daemon = config.daemon.clone();
    let discovered = run_bounded(async move {
        let records = discover_records(&daemon).await.ok()?;
        Some(plan_discovery(&records, &configured))
    })
    .unwrap_or_default();
    targets.extend(
        dialog_hosts(&config.hosts, &discovered)
            .into_iter()
            .filter(|host| host.unavailable.is_none())
            .map(|host| host.target),
    );
    targets
}

/// Project ids of the local daemon, with the project label as help.
#[derive(Clone, Copy, Debug)]
struct ProjectCompleter;

impl ValueCompleter for ProjectCompleter {
    fn complete(&self, current: &OsStr) -> Vec<CompletionCandidate> {
        let Some(current) = current.to_str() else {
            return Vec::new();
        };
        let projects = run_bounded(async {
            let config = Config::load().ok()?;
            let choices = load_choices(LOCAL_HOST, &config.daemon).await.ok()?;
            Some(choices.projects)
        })
        .unwrap_or_default();
        with_prefix(
            current,
            projects
                .into_iter()
                .map(|project| (project.id, Some(project.label))),
        )
    }
}

/// Ids of the My groups groups in the layout file, with the group name as
/// help. The file is only read.
#[derive(Clone, Copy, Debug)]
struct GroupCompleter;

impl ValueCompleter for GroupCompleter {
    fn complete(&self, current: &OsStr) -> Vec<CompletionCandidate> {
        let Some(current) = current.to_str() else {
            return Vec::new();
        };
        let Ok(path) = layout_path() else {
            return Vec::new();
        };
        let (store, error) = LayoutStore::open(path);
        if error.is_some() {
            return Vec::new();
        }
        with_prefix(
            current,
            store
                .layout()
                .ordered_groups()
                .into_iter()
                .map(|group| (group.id.clone(), Some(group.name.clone()))),
        )
    }
}

fn with_prefix(
    current: &str,
    values: impl Iterator<Item = (String, Option<String>)>,
) -> Vec<CompletionCandidate> {
    values
        .filter(|(value, _)| value.starts_with(current))
        .map(|(value, help)| CompletionCandidate::new(value).help(help.map(Into::into)))
        .collect()
}

/// Runs `future` on its own runtime and thread, giving up at
/// [`COMPLETION_DEADLINE`].
fn run_bounded<F, T>(future: F) -> Option<T>
where
    F: Future<Output = Option<T>> + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        runtime
            .block_on(async { tokio::time::timeout(COMPLETION_DEADLINE, future).await })
            .ok()
            .flatten()
    })
    .join()
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(shell: CompletionShell) -> String {
        String::from_utf8(render_script(shell, false)).expect("utf-8 script")
    }

    #[test]
    fn static_scripts_complete_version() {
        for shell in [
            CompletionShell::Bash,
            CompletionShell::Zsh,
            CompletionShell::Fish,
        ] {
            assert!(script(shell).contains("version"), "{shell:?}");
        }
    }

    #[test]
    fn bash_script_names_every_command_state_consistently() {
        assert!(!script(CompletionShell::Bash).contains("pohunek__subcmd__companion"));
    }

    #[test]
    fn static_scripts_leave_out_the_internal_known_hosts_flag() {
        for shell in [
            CompletionShell::Bash,
            CompletionShell::Zsh,
            CompletionShell::Fish,
        ] {
            let text = script(shell);
            assert!(!text.contains("known-hosts"), "{shell:?}");
            assert!(text.contains("new-session"), "{shell:?}");
        }
    }

    #[test]
    fn hidden_subcommands_are_dropped_from_the_static_tree() {
        let command = clap::Command::new("root")
            .subcommand(clap::Command::new("shown"))
            .subcommand(clap::Command::new("secret").hide(true));
        let names: Vec<_> = without_hidden_args(&command)
            .get_subcommands()
            .map(|sub| sub.get_name().to_owned())
            .collect();
        assert_eq!(names, ["shown"]);
    }
}
