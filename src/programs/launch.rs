//! Turning a program into something we can actually spawn.

use serde::{Deserialize, Serialize};

use super::agents;
use super::Kind;
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
    /// Reopen the program's own last conversation instead of starting
    /// fresh — the agent's resume flags, when the registry knows them.
    pub resume: bool,
    /// Reopen one exact conversation by the CLI's own session id, when
    /// radar has a binding for it (see `db::agent_sessions`).
    pub session: Option<String>,
    /// Create a fresh conversation under this exact CLI session id, when the
    /// CLI can (see [`Program::create_session`]). OpenCode creates one if the
    /// id is new, so a fresh launch knows its conversation up front and the
    /// board claim links to it exactly — no post-exit guess against whatever
    /// other session in the project happened to be newest.
    #[serde(default)]
    pub create_session: Option<String>,
    /// A unique instance name for an agent launch, folded into `RADAR_AGENT`
    /// (see [`command_spec`]). Irrelevant for non-agents.
    pub agent_instance: Option<String>,
    /// The board card this launch is attached to, if any. Reaches the program
    /// as `RADAR_CARD_ID`, so its comments and claims default to that card.
    pub card: Option<String>,
}

/// A resolved command line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    /// argv[0] is the program to execute.
    pub argv: Vec<String>,
    /// Environment variables to remove before launching.
    pub env_unset: Vec<String>,
    /// Environment variables to add before launching.
    pub env_set: Vec<(String, String)>,
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
    // Agents launched by radar carry their claim name: `$RADAR_AGENT` is what
    // the board's skill and guard use to tell "who is asking" apart, so two
    // instances of the same agent never hold each other's cards. Unique per
    // launch (see [`agent_instance_name`]); the `omarchy` wrapper execs the
    // agent, so the variable reaches it either way.
    let mut env_set = match (program.kind, &options.agent_instance) {
        (Kind::Agent, Some(instance)) => vec![(
            "RADAR_AGENT".to_string(),
            format!("{}-{instance}", program.id),
        )],
        _ => Vec::new(),
    };
    if let Some(card) = &options.card {
        env_set.push(("RADAR_CARD_ID".to_string(), card.clone()));
    }

    // A resumed launch bypasses the omarchy wrapper (it always starts a
    // fresh agent) and the permission flags: the conversation being
    // reopened had its own. An exact session id wins over "the last one".
    // A prompt (and extra args) still append after the resume flags, so a
    // resumed agent is handed the card message that woke it.
    let append_tail = |argv: &mut Vec<String>| {
        if let Some(prompt) = options
            .prompt
            .as_deref()
            .filter(|prompt| !prompt.is_empty())
        {
            argv.push(prompt.to_string());
        }
        argv.extend(options.extra_args.iter().cloned());
    };
    // A fresh launch that names its own conversation: the CLI creates one
    // under this id (OpenCode's `--session` does). Radar knows the exact
    // conversation from the first frame, so the board claim links to it
    // without any post-exit guess. This is a *fresh* start, so it keeps the
    // permission flags and the program's own arguments; only the wrapper is
    // skipped (it cannot carry a session id).
    if let Some(id) = options
        .create_session
        .as_deref()
        .filter(|id| !id.is_empty())
    {
        if program.create_session && !program.resume_session.is_empty() {
            let mut argv = vec![program.command.clone()];
            argv.extend(program.args.iter().cloned());
            if !options.safe {
                argv.extend(program.auto_args.iter().cloned());
            }
            argv.extend(
                program
                    .resume_session
                    .replace("{id}", id)
                    .split_whitespace()
                    .map(str::to_string),
            );
            append_tail(&mut argv);
            return CommandSpec {
                argv,
                env_unset: program.env_unset.clone(),
                env_set,
            };
        }
    }
    let resumed_argv = || {
        let mut argv = vec![program.command.clone()];
        argv.extend(program.args.iter().cloned());
        if let Some(id) = &options.session {
            if !program.resume_session.is_empty() {
                argv.extend(
                    program
                        .resume_session
                        .replace("{id}", id)
                        .split_whitespace()
                        .map(str::to_string),
                );
                append_tail(&mut argv);
                return Some(argv);
            }
            return None;
        }
        if options.resume && !program.resume_args.is_empty() {
            argv.extend(program.resume_args.iter().cloned());
            append_tail(&mut argv);
            return Some(argv);
        }
        None
    };
    if let Some(argv) = resumed_argv() {
        return CommandSpec {
            argv,
            env_unset: Vec::new(),
            env_set,
        };
    }
    if agents::use_omarchy_launcher(program, options) {
        let mut argv = vec![
            "omarchy".to_string(),
            "agent".to_string(),
            "--inline".to_string(),
        ];
        if let Some(prompt) = options.prompt.as_deref() {
            if !prompt.is_empty() {
                argv.push("--prompt".to_string());
                argv.push(prompt.to_string());
            }
        }
        return CommandSpec {
            argv,
            env_unset: Vec::new(),
            env_set,
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
        env_set,
    }
}

/// A short base-36 stamp of a moment: the unique suffix in a `RADAR_AGENT`
/// claim name (`claude-mx7k2b1f`) — short enough to read on a board line,
/// unique enough that two instances of the same agent never hold the same
/// card, and stable across restarts so stale claims stay attributable.
pub fn instance_stamp(millis: u128) -> String {
    let mut n = millis;
    let mut stamp = Vec::new();
    while n > 0 {
        let digit = (n % 36) as u32;
        stamp.push(char::from_digit(digit, 36).unwrap_or('0'));
        n /= 36;
    }
    stamp.iter().rev().collect()
}

/// Milliseconds since the epoch, saturating rather than panicking.
pub fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// The stamp for *now*.
pub fn now_stamp() -> String {
    instance_stamp(now_millis())
}

/// The exact CLI conversation id a fresh launch is given when the CLI can
/// create one (see [`Program::create_session`]). Derived from the launch's
/// instance stamp, so it is unique per launch and still names the same
/// conversation in the board claim that carries that stamp.
pub fn provider_session_id(project_id: i64, stamp: &str) -> String {
    format!("ses_radar{project_id}{stamp}")
}

/// The `RADAR_AGENT` a process carries, read from `/proc/<pid>/environ`:
/// how a board claim is matched to the exact agent tab that owns it —
/// two instances of the same program are told apart by their stamps.
/// Linux only, and only while the process lives; `None` otherwise.
pub fn radar_agent_of(pid: u32) -> Option<String> {
    radar_env_of(pid, "RADAR_AGENT")
}

/// The `RADAR_CARD_ID` a process carries: the board to-do it was launched for.
/// Stable for the life of the process, unlike the card's claim (which moves to
/// whoever holds the card now).
pub fn radar_card_of(pid: u32) -> Option<String> {
    radar_env_of(pid, "RADAR_CARD_ID")
}

fn radar_env_of(pid: u32, key: &str) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/environ"))
        .ok()
        .and_then(|environ| {
            environ.split('\0').find_map(|entry| {
                let (name, value) = entry.split_once('=')?;
                (name == key).then(|| value.to_string())
            })
        })
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
            resume_args: Vec::new(),
            resume_session: String::new(),
            create_session: false,
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
        let hunk = program("hunk", "hunk")
            .with_args(&["diff", "--watch"])
            .with_auto_args(&["--x"]);
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
    fn an_agent_launch_sets_its_claim_name() {
        let agent = program("claude", "claude");
        let options = LaunchOptions {
            agent_instance: Some("mx7k2b1f".into()),
            ..Default::default()
        };
        let spec = command_spec(&agent, &options);
        assert_eq!(
            spec.env_set,
            vec![("RADAR_AGENT".to_string(), "claude-mx7k2b1f".to_string())]
        );

        // Editors get nothing, and neither do agents without an instance.
        let mut editor = program("nvim", "nvim");
        editor.kind = Kind::Editor;
        assert!(command_spec(&editor, &options).env_set.is_empty());
        assert!(command_spec(&agent, &LaunchOptions::default())
            .env_set
            .is_empty());
    }

    #[test]
    fn stamps_are_short_base36_and_ordered() {
        assert_eq!(instance_stamp(0), "");
        assert_eq!(instance_stamp(36), "10");
        let a = instance_stamp(1_769_420_000_000);
        let b = instance_stamp(1_769_420_000_001);
        assert_ne!(a, b);
        assert!(a.len() <= 8, "{a} is too long for a board line");
    }

    #[test]
    fn a_named_provider_session_is_unique_per_launch_and_project() {
        let a = provider_session_id(2, "abc");
        let b = provider_session_id(2, "abd");
        let c = provider_session_id(3, "abc");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("ses_"), "{a}");
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

    #[test]
    fn a_live_process_read_s_its_own_agent_name() {
        // This very process: whatever the environment says, the /proc read
        // must agree — an agent launched by radar carries its claim name,
        // a test run by hand carries none.
        assert_eq!(
            radar_agent_of(std::process::id()),
            std::env::var("RADAR_AGENT").ok()
        );
        // A pid that cannot exist: no environ, no claim.
        assert_eq!(radar_agent_of(u32::MAX - 1), None);
    }

    #[test]
    fn a_resume_launch_reopens_the_last_conversation() {
        let mut opencode = program("opencode", "opencode");
        opencode.resume_args = vec!["--continue".into()];
        let options = LaunchOptions {
            resume: true,
            agent_instance: Some("mx7k2b1f".into()),
            ..Default::default()
        };
        let spec = command_spec(&opencode, &options);
        // No auto flags, no omarchy wrapper: the resumed conversation had
        // its own permission context, and the wrapper cannot carry resume.
        assert_eq!(spec.argv, vec!["opencode", "--continue"]);
        // The relaunch is still a radar agent with its own claim name.
        assert_eq!(
            spec.env_set,
            vec![("RADAR_AGENT".to_string(), "opencode-mx7k2b1f".to_string())]
        );

        // Without resume knowledge, a resume launch is just a fresh start
        // with the program's own arguments.
        let fresh = command_spec(&program("crush", "crush"), &options);
        assert_eq!(fresh.argv, vec!["crush"]);
    }

    #[test]
    fn a_stored_session_reopens_that_exact_conversation() {
        let mut opencode = program("opencode", "opencode");
        opencode.resume_session = "--session {id}".to_string();
        let options = LaunchOptions {
            session: Some("ses_abc123".into()),
            ..Default::default()
        };
        let spec = command_spec(&opencode, &options);
        assert_eq!(spec.argv, vec!["opencode", "--session", "ses_abc123"]);

        // A program with no template for ids starts fresh instead of
        // resuming some other conversation by accident.
        let mut crush = program("crush", "crush");
        crush.resume_session = String::new();
        assert_eq!(command_spec(&crush, &options).argv, vec!["crush"]);
    }

    #[test]
    fn a_named_fresh_session_is_created_with_the_auto_flags() {
        let mut opencode = program("opencode", "opencode");
        opencode.auto_args = vec!["--auto".into()];
        opencode.resume_session = "--session {id}".into();
        opencode.create_session = true;
        let options = LaunchOptions {
            create_session: Some("ses_radar7abc".into()),
            prompt: Some("work the card".into()),
            card: Some("card-1".into()),
            agent_instance: Some("abc".into()),
            ..Default::default()
        };
        let spec = command_spec(&opencode, &options);
        // A fresh create keeps the permission flags and the prompt; it only
        // skips the omarchy wrapper, which cannot carry a session id.
        assert_eq!(
            spec.argv,
            vec![
                "opencode",
                "--auto",
                "--session",
                "ses_radar7abc",
                "work the card"
            ]
        );
        assert!(spec
            .env_set
            .iter()
            .any(|(key, value)| key == "RADAR_AGENT" && value == "opencode-abc"));

        // A CLI that cannot create a session ignores the request rather than
        // resuming some other conversation under a made-up id.
        let mut crush = program("crush", "crush");
        crush.resume_session = "--resume {id}".into();
        let ignored = command_spec(
            &crush,
            &LaunchOptions {
                create_session: Some("ses_radar7abc".into()),
                ..Default::default()
            },
        );
        assert_eq!(ignored.argv, vec!["crush"]);
    }

    #[test]
    fn supported_agents_resume_last_or_exact_provider_session() {
        let by_id = |id: &str| crate::programs::by_id(id).unwrap();
        for id in ["opencode", "omp", "cursor-agent"] {
            let program = by_id(id);
            let resumed = command_spec(
                &program,
                &LaunchOptions {
                    resume: true,
                    ..Default::default()
                },
            );
            assert_eq!(
                resumed.argv,
                vec![program.command.clone(), "--continue".to_string()],
                "{id}"
            );
        }

        let exact = LaunchOptions {
            session: Some("session-42".into()),
            ..Default::default()
        };
        assert_eq!(
            command_spec(&by_id("opencode"), &exact).argv,
            vec!["opencode", "--session", "session-42"]
        );
        assert_eq!(
            command_spec(&by_id("omp"), &exact).argv,
            vec!["omp", "--resume=session-42"]
        );
        assert_eq!(
            command_spec(&by_id("cursor-agent"), &exact).argv,
            vec!["cursor-agent", "--resume", "session-42"]
        );
    }

    #[test]
    fn a_resumed_launch_carries_the_card_prompt_and_card_env() {
        let mut agent = program("opencode", "opencode");
        agent.resume_session = "--session {id}".to_string();
        let options = LaunchOptions {
            session: Some("ses_1".into()),
            prompt: Some("a message from the human".into()),
            card: Some("card-1".into()),
            agent_instance: Some("stamp".into()),
            ..Default::default()
        };
        let spec = command_spec(&agent, &options);
        assert_eq!(
            spec.argv,
            vec!["opencode", "--session", "ses_1", "a message from the human"]
        );
        assert!(spec
            .env_set
            .iter()
            .any(|(key, value)| key == "RADAR_CARD_ID" && value == "card-1"));
        assert!(spec
            .env_set
            .iter()
            .any(|(key, value)| key == "RADAR_AGENT" && value == "opencode-stamp"));
    }
}
