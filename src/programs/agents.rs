//! Agent CLIs.
//!
//! The ids, names and "don't stop to ask" flags mirror omarchy's own launcher
//! (`/usr/share/omarchy/bin/omarchy-agent`) so that `omarchy default agent`,
//! the keybinding, and radar all agree on how an agent should be started.
//! `omarchy default agent` is the source of truth for which one is preferred.

use crate::db::Preferences;

use super::{Kind, Program};

/// An agent as omarchy defines it.
struct AgentDef {
    id: &'static str,
    name: &'static str,
    command: &'static str,
    /// Arguments that skip permission prompts.
    auto: &'static [&'static str],
    /// Always-on arguments (subcommands).
    args: &'static [&'static str],
    /// Environment to unset.
    env_unset: &'static [&'static str],
    description: &'static str,
    /// Shipped by omarchy (so `omarchy default agent` can select it).
    omarchy: bool,
}

/// Agent providers Radar currently offers for new selections.
const SUPPORTED_AGENT_IDS: &[&str] = &["opencode", "omp", "cursor-agent"];

pub fn is_supported(id: &str) -> bool {
    SUPPORTED_AGENT_IDS.contains(&id)
}

/// omarchy's agent list, then a few the registry knows directly.
const AGENTS: &[AgentDef] = &[
    AgentDef {
        id: "opencode",
        name: "OpenCode",
        command: "opencode",
        auto: &["--auto"],
        // The TUI otherwise attaches to opencode's background service, whose
        // current conversation is whatever the service has open — not the
        // session id radar asked for. A private server per pane keeps exact
        // sessions exact.
        args: &["--standalone"],
        env_unset: &[],
        description: "open source agent",
        omarchy: true,
    },
    AgentDef {
        id: "claude",
        name: "Claude Code",
        command: "claude",
        auto: &["--permission-mode", "auto"],
        args: &[],
        env_unset: &[],
        description: "Anthropic",
        omarchy: true,
    },
    AgentDef {
        id: "codex",
        name: "Codex",
        command: "codex",
        auto: &["--approve-for-me"],
        args: &[],
        env_unset: &[],
        description: "OpenAI",
        omarchy: true,
    },
    AgentDef {
        id: "crush",
        name: "Crush",
        command: "crush",
        auto: &["--yolo"],
        args: &[],
        env_unset: &[],
        description: "Charm",
        omarchy: true,
    },
    AgentDef {
        id: "cursor-agent",
        name: "Cursor CLI",
        command: "cursor-agent",
        auto: &["--yolo", "--trust"],
        args: &[],
        env_unset: &[],
        description: "Cursor",
        omarchy: true,
    },
    AgentDef {
        id: "agy",
        name: "Antigravity",
        command: "agy",
        auto: &["--dangerously-skip-permissions"],
        args: &[],
        env_unset: &[],
        description: "Google",
        omarchy: true,
    },
    AgentDef {
        id: "copilot",
        name: "GitHub Copilot",
        command: "copilot",
        auto: &["--allow-all"],
        args: &[],
        env_unset: &[],
        description: "GitHub",
        omarchy: true,
    },
    AgentDef {
        id: "grok",
        name: "Grok",
        command: "grok",
        auto: &["--permission-mode", "bypassPermissions"],
        args: &[],
        env_unset: &[],
        description: "xAI",
        omarchy: true,
    },
    AgentDef {
        id: "hermes",
        name: "Hermes",
        command: "hermes",
        auto: &["--yolo"],
        args: &[],
        env_unset: &["HERMES_SESSION_SOURCE"],
        description: "Hermes",
        omarchy: true,
    },
    AgentDef {
        id: "muse",
        name: "Muse Code",
        command: "muse",
        auto: &["--approval-mode", "never"],
        args: &[],
        env_unset: &[],
        description: "Meta",
        omarchy: true,
    },
    AgentDef {
        id: "omp",
        name: "Oh My Pi",
        command: "omp",
        auto: &["--auto-approve"],
        args: &[],
        env_unset: &[],
        description: "pi fork",
        omarchy: true,
    },
    AgentDef {
        id: "ori",
        name: "Ori",
        command: "ori",
        auto: &[],
        args: &["code"],
        env_unset: &[],
        description: "OpenRouter",
        omarchy: true,
    },
    AgentDef {
        id: "pi",
        name: "Pi",
        command: "pi",
        auto: &[],
        args: &[],
        env_unset: &[],
        description: "Pi",
        omarchy: true,
    },
    AgentDef {
        id: "amp",
        name: "Amp",
        command: "amp",
        auto: &[],
        args: &[],
        env_unset: &[],
        description: "Sourcegraph",
        omarchy: false,
    },
    AgentDef {
        id: "gemini",
        name: "Gemini CLI",
        command: "gemini",
        auto: &["--yolo"],
        args: &[],
        env_unset: &[],
        description: "Google",
        omarchy: false,
    },
    AgentDef {
        id: "aider",
        name: "Aider",
        command: "aider",
        auto: &["--yes-always"],
        args: &[],
        env_unset: &[],
        description: "pair programming",
        omarchy: false,
    },
    AgentDef {
        id: "goose",
        name: "Goose",
        command: "goose",
        auto: &[],
        args: &[],
        env_unset: &[],
        description: "Block",
        omarchy: false,
    },
];

impl AgentDef {
    fn program(&self, priority: i32) -> Program {
        let mut program = Program {
            id: self.id.to_string(),
            name: self.name.to_string(),
            command: self.command.to_string(),
            kind: Kind::Agent,
            description: self.description.to_string(),
            args: self.args.iter().map(|s| s.to_string()).collect(),
            auto_args: self.auto.iter().map(|s| s.to_string()).collect(),
            resume_args: Vec::new(),
            resume_session: String::new(),
            create_session: false,
            env_unset: self.env_unset.iter().map(|s| s.to_string()).collect(),
            external: false,
            omarchy: self.omarchy,
            priority,
        };
        // How to reopen the agent's own last conversation — CLI-specific,
        // and radar's own knowledge: omarchy's wrapper always starts fresh.
        // A resumed launch reopens the project's last conversation, which
        // is the session a claimed card's agent was working in.
        let resume: &[&str] = match self.id {
            "opencode" | "claude" | "omp" | "cursor-agent" => &["--continue"],
            "codex" => &["resume", "--last"],
            _ => &[],
        };
        program.resume_args = resume.iter().map(|s| s.to_string()).collect();
        // How to reopen one exact conversation, when radar has stored the
        // session id a claim's agent had (db::agent_sessions).
        program.resume_session = match self.id {
            "opencode" => "--session {id}".to_string(),
            "claude" | "cursor-agent" => "--resume {id}".to_string(),
            "codex" => "resume {id}".to_string(),
            "omp" => "--resume={id}".to_string(),
            _ => String::new(),
        };
        // OpenCode's `--session` creates the conversation if the id is new, so
        // radar can name a fresh launch's conversation up front (an exact 1:1
        // link to the board card) instead of reading its store after the fact.
        // The other CLIs' flags only resume; a fresh launch there stays a guess.
        program.create_session = self.id == "opencode";
        // omarchy's default agent is the one users expect first everywhere.
        if omarchy_default().as_deref() == Some(self.id) {
            program.priority = -1;
        }
        program
    }
}

/// Every agent, installed or not, in preference order.
pub fn programs() -> Vec<Program> {
    let mut agents: Vec<Program> = AGENTS
        .iter()
        .enumerate()
        .map(|(index, def)| def.program(index as i32))
        .collect();
    agents.sort_by_key(|a| (a.priority, !a.installed(), a.name.clone()));

    // A project's agent tab may have been stored as `omarchy-default`; keep that
    // id resolvable so old tabs keep working.
    agents
}

/// Providers Radar offers as agent choices, including those not installed.
pub fn supported_programs() -> Vec<Program> {
    programs()
        .into_iter()
        .filter(|program| is_supported(&program.id))
        .collect()
}

/// Providers that can currently be selected to launch a new agent.
pub fn selectable_agents() -> Vec<Program> {
    supported_programs()
        .into_iter()
        .filter(Program::installed)
        .collect()
}

/// All known agents that are installed; retained for legacy program handling.
pub fn installable_agents() -> Vec<Program> {
    programs().into_iter().filter(|p| p.installed()).collect()
}

/// omarchy's chosen default agent, if it is set.
pub fn omarchy_default() -> Option<String> {
    let file = dirs::home_dir()?.join(".config/omarchy/defaults/agent");
    let text = std::fs::read_to_string(file).ok()?;
    let id = text.trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

/// Is `omarchy` available to run the default agent on our behalf?
pub fn have_omarchy() -> bool {
    crate::config::have("omarchy")
}

/// The agent a project should start: the preferred one if it is installed,
/// then omarchy's default, then the first installed agent.
///
/// Agents are terminal programs. radar gives them a terminal and stays out of
/// the way — no API adapters, no reimplemented session browsers.
pub fn preferred_agent(preferences: &Preferences) -> Option<Program> {
    if let Some(id) = preferences.agent.as_deref().filter(|id| is_supported(id)) {
        if let Some(program) = super::by_id(id) {
            if program.installed() {
                return Some(program);
            }
        }
    }
    if let Some(default_id) = omarchy_default().filter(|id| is_supported(id)) {
        if let Some(program) = super::by_id(&default_id) {
            if program.installed() {
                return Some(program);
            }
        }
    }
    selectable_agents().into_iter().next()
}

/// Run `omarchy agent --inline` instead of building the command line ourselves?
///
/// True for omarchy's default agent when auto flags are on and the `omarchy`
/// wrapper exists, so omarchy keeps ownership of the flags and prompts.
/// `$PWD` decides the starting directory, and the caller sets that to the
/// project.
pub fn use_omarchy_launcher(program: &Program, options: &super::LaunchOptions) -> bool {
    !options.safe
        && program.omarchy
        && have_omarchy()
        && omarchy_default().as_deref() == Some(program.id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omarchy_flags_are_mirrored() {
        let by_id = |id: &str| programs().into_iter().find(|p| p.id == id).unwrap();
        assert_eq!(by_id("claude").auto_args, vec!["--permission-mode", "auto"]);
        assert_eq!(by_id("codex").auto_args, vec!["--approve-for-me"]);
        assert_eq!(by_id("crush").auto_args, vec!["--yolo"]);
        assert_eq!(by_id("opencode").auto_args, vec!["--auto"]);
        assert_eq!(by_id("cursor-agent").auto_args, vec!["--yolo", "--trust"]);
        assert_eq!(by_id("ori").args, vec!["code"]);
        assert_eq!(by_id("hermes").env_unset, vec!["HERMES_SESSION_SOURCE"]);
    }

    #[test]
    fn supported_agents_declare_documented_resume_forms() {
        let by_id = |id: &str| programs().into_iter().find(|p| p.id == id).unwrap();
        for id in ["opencode", "omp", "cursor-agent"] {
            assert_eq!(by_id(id).resume_args, vec!["--continue"], "{id}");
            assert!(!by_id(id).resume_session.is_empty(), "{id}");
        }
        assert_eq!(by_id("omp").resume_session, "--resume={id}");
        assert_eq!(by_id("cursor-agent").resume_session, "--resume {id}");
        assert_eq!(by_id("opencode").resume_session, "--session {id}");
    }

    #[test]
    fn only_opencode_can_create_a_conversation_under_a_chosen_id() {
        let by_id = |id: &str| programs().into_iter().find(|p| p.id == id).unwrap();
        assert!(by_id("opencode").create_session);
        // The others' flags only resume an existing conversation, so a fresh
        // launch there must not be handed a made-up id.
        for id in ["omp", "cursor-agent", "claude", "codex", "crush"] {
            assert!(!by_id(id).create_session, "{id}");
        }
    }

    #[test]
    fn supported_agent_choices_are_exactly_the_three_providers() {
        let mut ids: Vec<_> = supported_programs()
            .into_iter()
            .map(|program| program.id)
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["cursor-agent", "omp", "opencode"]);
        assert!(selectable_agents()
            .iter()
            .all(|program| is_supported(&program.id)));
        assert!(!is_supported("claude"));
        assert!(!is_supported("cursor"));
    }

    #[test]
    fn unsupported_saved_preferences_never_resolve_to_another_agent() {
        let prefs = Preferences {
            agent: Some("claude".into()),
            ..Default::default()
        };
        assert!(preferred_agent(&prefs).is_none_or(|program| is_supported(&program.id)));
    }

    #[test]
    fn omarchy_default_is_read_from_the_omarchy_config() {
        // The test environment's answer is whatever the machine says; the
        // contract is just that a set default is a known agent id.
        if let Some(id) = omarchy_default() {
            assert!(
                AGENTS.iter().any(|a| a.id == id),
                "unknown default agent {id}"
            );
        }
    }

    #[test]
    fn installed_agents_are_a_subset() {
        let all: Vec<String> = programs().into_iter().map(|p| p.id).collect();
        for agent in installable_agents() {
            assert!(all.contains(&agent.id));
            assert!(agent.installed());
        }
    }

    #[test]
    fn the_default_agent_sorts_first_when_it_is_installed() {
        let Some(default_id) = omarchy_default() else {
            return;
        };
        let agents = installable_agents();
        if let Some(first) = agents.first() {
            if agents.iter().any(|a| a.id == default_id) {
                assert_eq!(
                    first.id, default_id,
                    "the default agent should lead the list"
                );
            }
        }
    }

    #[test]
    fn preferred_agent_prefers_the_preference() {
        let mut prefs = Preferences::default();
        let selectable = selectable_agents();
        let Some(some_agent) = selectable.first() else {
            return;
        };
        prefs.agent = Some(some_agent.id.clone());
        assert_eq!(preferred_agent(&prefs).unwrap().id, some_agent.id);
    }

    #[test]
    fn preferred_agent_falls_back_when_unset() {
        let prefs = Preferences::default();
        let chosen = preferred_agent(&prefs);
        if let Some(agent) = chosen {
            assert!(agent.installed());
        }
    }

    #[test]
    fn omarchy_launcher_is_only_used_for_the_omarchy_default() {
        let options = super::super::LaunchOptions::default();
        for agent in programs() {
            if use_omarchy_launcher(&agent, &options) {
                assert!(agent.omarchy);
                assert_eq!(omarchy_default().as_deref(), Some(agent.id.as_str()));
            }
        }
    }

    #[test]
    fn safe_mode_never_uses_the_omarchy_launcher() {
        let options = super::super::LaunchOptions {
            safe: true,
            ..Default::default()
        };
        for agent in programs() {
            assert!(!use_omarchy_launcher(&agent, &options));
        }
    }
}
