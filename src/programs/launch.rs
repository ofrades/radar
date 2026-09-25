//! Turning a program into something we can actually spawn.

use serde::{Deserialize, Serialize};

use super::agents;
use super::Program;

/// How a program should be started.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchOptions {
    /// Never add permission-skipping flags.
    pub safe: bool,
    /// Extra arguments appended to the program's own.
    pub extra_args: Vec<String>,
    /// An initial prompt, for agents that take one.
    pub prompt: Option<String>,
}

/// A resolved command line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    /// argv[0] is the program to execute.
    pub argv: Vec<String>,
    /// Environment variables to remove before launching.
    pub env_unset: Vec<String>,
}

impl CommandSpec {
    pub fn program(&self) -> &str {
        self.argv.first().map(String::as_str).unwrap_or("")
    }

    /// Human readable form, for logs and tooltips.
    pub fn display(&self) -> String {
        self.argv.join(" ")
    }
}

/// Build the command line for `program`.
pub fn command_spec(program: &Program, options: &LaunchOptions) -> CommandSpec {
    // omarchy's wrapper knows how its default agent wants to be started, and
    // keeps working when those flags change. Only used when the user has not
    // asked for safe mode.
    if agents::use_omarchy_launcher(program, options) {
        let mut argv = vec!["omarchy".to_string(), "agent".to_string(), "--inline".to_string()];
        if let Some(prompt) = options.prompt.as_deref() {
            if !prompt.is_empty() {
                argv.push("--prompt".to_string());
                argv.push(prompt.to_string());
            }
        }
        return CommandSpec {
            argv,
            env_unset: Vec::new(),
        };
    }

    let mut argv = Vec::with_capacity(8);
    argv.push(program.command.clone());
    argv.extend(program.args.iter().cloned());
    if !options.safe {
        argv.extend(program.auto_args.iter().cloned());
    }
    if options.prompt.as_deref().is_some_and(|p| !p.is_empty()) {
        argv.push(options.prompt.clone().unwrap_or_default());
    }
    argv.extend(options.extra_args.iter().cloned());

    CommandSpec {
        argv,
        env_unset: program.env_unset.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::programs::Kind;

    fn program(id: &str, command: &str) -> Program {
        Program {
            id: id.to_string(),
            name: id.to_string(),
            command: command.to_string(),
            kind: Kind::Agent,
            description: String::new(),
            args: Vec::new(),
            auto_args: Vec::new(),
            env_unset: Vec::new(),
            external: false,
            omarchy: false,
            priority: 0,
        }
    }

    #[test]
    fn a_plain_program_is_just_its_command() {
        let spec = command_spec(&program("nvim", "nvim"), &LaunchOptions::default());
        assert_eq!(spec.argv, vec!["nvim"]);
        assert_eq!(spec.program(), "nvim");
    }

    #[test]
    fn auto_flags_are_added_unless_safe() {
        let agent = program("claude", "claude").with_auto_args(&["--permission-mode", "auto"]);
        let auto = command_spec(&agent, &LaunchOptions::default());
        assert_eq!(auto.argv, vec!["claude", "--permission-mode", "auto"]);

        let safe = command_spec(
            &agent,
            &LaunchOptions {
                safe: true,
                ..Default::default()
            },
        );
        assert_eq!(safe.argv, vec!["claude"]);
    }

    #[test]
    fn always_on_args_come_before_auto_flags() {
        let hunk = program("hunk", "hunk").with_args(&["diff", "--watch"]).with_auto_args(&["--x"]);
        let spec = command_spec(&hunk, &LaunchOptions::default());
        assert_eq!(spec.argv, vec!["hunk", "diff", "--watch", "--x"]);
    }

    #[test]
    fn extra_args_are_appended_last() {
        let spec = command_spec(
            &program("hunk", "hunk"),
            &LaunchOptions {
                extra_args: vec!["--mode".into(), "split".into()],
                ..Default::default()
            },
        );
        assert_eq!(spec.argv, vec!["hunk", "--mode", "split"]);
    }

    #[test]
    fn a_prompt_is_passed_through() {
        let spec = command_spec(
            &program("codex", "codex"),
            &LaunchOptions {
                prompt: Some("review this".into()),
                ..Default::default()
            },
        );
        assert_eq!(spec.argv, vec!["codex", "review this"]);
    }

    #[test]
    fn an_empty_prompt_is_ignored() {
        let spec = command_spec(
            &program("codex", "codex"),
            &LaunchOptions {
                prompt: Some(String::new()),
                ..Default::default()
            },
        );
        assert_eq!(spec.argv, vec!["codex"]);
    }

    #[test]
    fn env_unset_is_carried_along() {
        let hermes = program("hermes", "hermes").with_env_unset(&["HERMES_SESSION_SOURCE"]);
        let spec = command_spec(&hermes, &LaunchOptions::default());
        assert_eq!(spec.env_unset, vec!["HERMES_SESSION_SOURCE"]);
    }

    #[test]
    fn display_shows_the_whole_command_line() {
        let spec = command_spec(
            &program("hunk", "hunk"),
            &LaunchOptions {
                extra_args: vec!["--watch".into()],
                ..Default::default()
            },
        );
        assert_eq!(spec.display(), "hunk --watch");
    }
}
