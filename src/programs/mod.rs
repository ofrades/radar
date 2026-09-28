//! The program registry: everything radar can put in a tab.
//!
//! Two kinds of program exist here. *Embedded* programs are terminal UIs
//! (nvim, hunk, an agent CLI) that run inside a tab's terminal. *External*
//! programs are GUI applications (Zed, VS Code) that radar launches as their
//! own window — you cannot draw Zed inside a terminal, so we do not pretend to.

pub mod agents;
pub mod launch;
pub mod sessions;

use serde::{Deserialize, Serialize};

use crate::db::{Preferences, Slot};
use crate::git;

pub use launch::{CommandSpec, LaunchOptions};

/// What a program is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Editor,
    Agent,
    Diff,
    Shell,
    /// Terminals, file managers, dashboards: useful, not fitting a slot.
    Tool,
}

impl Kind {
    pub const ALL: [Kind; 5] = [
        Kind::Editor,
        Kind::Agent,
        Kind::Diff,
        Kind::Shell,
        Kind::Tool,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Kind::Editor => "Editors",
            Kind::Agent => "Agents",
            Kind::Diff => "Diffs",
            Kind::Shell => "Shells",
            Kind::Tool => "Tools",
        }
    }

    /// Which slot a freshly added tab of this kind fills.
    pub const fn default_slot(self) -> Slot {
        match self {
            Kind::Editor => Slot::Editor,
            Kind::Agent => Slot::Agent,
            Kind::Diff => Slot::Diff,
            Kind::Shell => Slot::Shell,
            Kind::Tool => Slot::Custom,
        }
    }

    pub fn from_slot(slot: Slot) -> Option<Kind> {
        match slot {
            Slot::Editor => Some(Kind::Editor),
            Slot::Agent => Some(Kind::Agent),
            Slot::Diff => Some(Kind::Diff),
            Slot::Shell => Some(Kind::Shell),
            Slot::Board | Slot::Custom => None,
        }
    }
}

/// One runnable program.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Program {
    /// Stable id used in preferences and stored tabs.
    pub id: String,
    pub name: String,
    /// The binary to execute.
    pub command: String,
    pub kind: Kind,
    pub description: String,
    /// Always-on arguments, e.g. `["diff", "--watch"]` for hunk.
    pub args: Vec<String>,
    /// Arguments that skip a tool's permission prompts (agents only).
    pub auto_args: Vec<String>,
    /// Arguments that reopen the agent's own last conversation (agents
    /// only, e.g. `["--continue"]`). Empty when the CLI has none radar
    /// knows — a resumed launch then just starts fresh.
    pub resume_args: Vec<String>,
    /// How to reopen one exact conversation: a tiny template with `{id}`
    /// where the session id goes (e.g. `"--session {id}"`). Empty when
    /// the CLI has no such flag radar knows.
    pub resume_session: String,
    /// Environment variables to remove for this program.
    pub env_unset: Vec<String>,
    /// GUI program: launch it externally instead of embedding it.
    pub external: bool,
    /// Part of omarchy's curated agent list.
    pub omarchy: bool,
    /// Lower sorts first when picking a default for a kind.
    pub priority: i32,
}

impl Program {
    fn new(
        id: &str,
        name: &str,
        command: &str,
        kind: Kind,
        description: &str,
        priority: i32,
    ) -> Program {
        Program {
            id: id.to_string(),
            name: name.to_string(),
            command: command.to_string(),
            kind,
            description: description.to_string(),
            args: Vec::new(),
            auto_args: Vec::new(),
            resume_args: Vec::new(),
            resume_session: String::new(),
            env_unset: Vec::new(),
            external: false,
            omarchy: false,
            priority,
        }
    }

    pub fn with_args(mut self, args: &[&str]) -> Program {
        self.args = args.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn with_auto_args(mut self, args: &[&str]) -> Program {
        self.auto_args = args.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn with_env_unset(mut self, keys: &[&str]) -> Program {
        self.env_unset = keys.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn external(mut self) -> Program {
        self.external = true;
        self
    }

    pub fn omarchy(mut self) -> Program {
        self.omarchy = true;
        self
    }

    /// Is the binary on `PATH`?
    pub fn installed(&self) -> bool {
        crate::config::have(&self.command)
    }

    /// Full command line, honouring permission-flag policy and extra args.
    pub fn command_spec(&self, options: &LaunchOptions) -> CommandSpec {
        launch::command_spec(self, options)
    }

    /// Display form, for the "add tab" menu and the preferences dialog.
    pub fn detail(&self) -> String {
        if self.installed() {
            self.description.clone()
        } else {
            format!("{} · not installed ({})", self.description, self.command)
        }
    }
}

/// Every program radar knows about, installed or not.
///
/// Order within a kind is the preference order used when nothing is chosen.
pub fn registry() -> Vec<Program> {
    let mut programs = Vec::new();

    // ---- Editors (terminal only: a GUI editor cannot live in a tab) ----
    let editor_priority = 0;
    programs.push(
        Program::new(
            "nvim",
            "Neovim",
            "nvim",
            Kind::Editor,
            "the editor",
            editor_priority,
        )
        .with_args(&["."]),
    );
    programs.push(
        Program::new(
            "vim",
            "Vim",
            "vim",
            Kind::Editor,
            "classic vim",
            editor_priority + 1,
        )
        .with_args(&["."]),
    );
    programs.push(
        Program::new(
            "hx",
            "Helix",
            "hx",
            Kind::Editor,
            "modal editor in Rust",
            editor_priority + 2,
        )
        .with_args(&["."]),
    );
    programs.push(
        Program::new(
            "micro",
            "Micro",
            "micro",
            Kind::Editor,
            "friendly editor",
            editor_priority + 3,
        )
        .with_args(&["."]),
    );
    // Honour $EDITOR when it is something the list does not already cover, so a
    // kakoune or emacs user gets their editor without a second nvim entry.
    if let Some(editor) = crate::config::preferred_editor_from_env() {
        if !programs.iter().any(|p| p.command == editor) {
            programs.push(
                Program::new(
                    &editor,
                    &format!("{editor} ($EDITOR)"),
                    &editor,
                    Kind::Editor,
                    "your $EDITOR",
                    -1,
                )
                .with_args(&["."]),
            );
        }
    }

    // GUI editors: offered as "open externally", never as a tab.
    programs.push(
        Program::new(
            "zed",
            "Zed",
            "zed",
            Kind::Editor,
            "open the project in Zed",
            50,
        )
        .with_args(&["."])
        .external(),
    );
    programs.push(
        Program::new(
            "code",
            "VS Code",
            "code",
            Kind::Editor,
            "open the project in VS Code",
            51,
        )
        .with_args(&["."])
        .external(),
    );
    programs.push(
        Program::new(
            "cursor",
            "Cursor",
            "cursor",
            Kind::Editor,
            "open the project in Cursor",
            52,
        )
        .with_args(&["."])
        .external(),
    );

    // ---- Agents: omarchy's list, plus a few the registry knows directly ----
    programs.extend(agents::programs());

    // ---- Diffs ----
    // lazygit leads: it is the general git TUI, so it is what the Changes
    // primitive opens when nothing else is preferred. Hunk stays second as the
    // focused reviewer for a single changeset.
    programs.push(Program::new(
        "lazygit",
        "Lazygit",
        "lazygit",
        Kind::Diff,
        "git TUI",
        0,
    ));
    programs.push(
        Program::new(
            "hunk",
            "Hunk",
            "hunk",
            Kind::Diff,
            "live review of agent changes",
            1,
        )
        .with_args(&["diff", "--watch"]),
    );
    programs.push(Program::new(
        "gitui",
        "GitUI",
        "gitui",
        Kind::Diff,
        "git TUI",
        2,
    ));
    programs.push(Program::new(
        "tig",
        "Tig",
        "tig",
        Kind::Diff,
        "git browser",
        3,
    ));

    // ---- Shells ----
    let shell = crate::config::login_shell();
    let shell_name = shell.rsplit('/').next().unwrap_or("shell").to_string();
    programs.push(Program::new(
        "shell",
        &format!("Shell ({shell_name})"),
        &shell,
        Kind::Shell,
        "your login shell",
        0,
    ));
    for (index, candidate) in ["bash", "zsh", "fish"].iter().enumerate() {
        if crate::config::have(candidate) {
            programs.push(Program::new(
                candidate,
                candidate,
                candidate,
                Kind::Shell,
                "interactive shell",
                index as i32 + 1,
            ));
        }
    }

    // ---- Tools: a small, curated tray of terminal programs ----
    let tools: [(&str, &str, &str); 14] = [
        ("yazi", "Yazi", "file manager"),
        ("ranger", "Ranger", "file manager"),
        ("lf", "lf", "file manager"),
        ("lazydocker", "Lazydocker", "docker dashboard"),
        ("lazysql", "Lazysql", "database client"),
        ("k9s", "k9s", "kubernetes dashboard"),
        ("btop", "Btop", "system monitor"),
        ("htop", "Htop", "process viewer"),
        ("nvtop", "NVTop", "GPU monitor"),
        ("tmux", "Tmux", "terminal multiplexer"),
        ("herdr", "Herdr", "agent workspace manager"),
        ("dust", "Dust", "disk usage"),
        ("dua", "Dua", "disk usage"),
        ("gpg", "GPG", "key manager"),
    ];
    for (index, (command, name, description)) in tools.iter().enumerate() {
        programs.push(Program::new(
            command,
            name,
            command,
            Kind::Tool,
            description,
            index as i32,
        ));
    }

    // Ids address tabs and preferences, so they must be unique. Keep the first
    // occurrence: registries are ordered by preference, and editors/agents come
    // before tools.
    let mut seen = std::collections::HashSet::new();
    programs.retain(|program| seen.insert(program.id.clone()));
    programs
}

/// Ids that exist in the registry.
pub fn by_id(id: &str) -> Option<Program> {
    registry().into_iter().find(|p| p.id == id)
}

/// Programs of a kind, installed only, in preference order.
pub fn installed_of(kind: Kind) -> Vec<Program> {
    registry()
        .into_iter()
        .filter(|p| p.kind == kind && !p.external && p.installed())
        .collect()
}

/// Everything that can live in a tab (installed, not external).
pub fn embeddable() -> Vec<Program> {
    registry()
        .into_iter()
        .filter(|p| !p.external && p.installed())
        .collect()
}

/// External programs (GUI apps) that are installed.
pub fn external_programs() -> Vec<Program> {
    registry()
        .into_iter()
        .filter(|p| p.external && p.installed())
        .collect()
}

/// The program a slot should use: the preferred one when it is still
/// installed, otherwise the best available.
pub fn for_slot(slot: Slot, preferences: &Preferences) -> Option<Program> {
    let kind = Kind::from_slot(slot)?;
    if let Some(id) = preferences.get(slot) {
        if slot != Slot::Agent || agents::is_supported(id) {
            if let Some(program) = by_id(id) {
                if program.installed() && !program.external {
                    return Some(program);
                }
            }
        }
    }
    match slot {
        Slot::Agent => agents::preferred_agent(preferences),
        _ => installed_of(kind).into_iter().next(),
    }
}

/// Everything the user could pick for a slot: installed programs of that kind.
pub fn candidates_for_slot(slot: Slot, preferences: &Preferences) -> Vec<Program> {
    let Some(kind) = Kind::from_slot(slot) else {
        return embeddable();
    };
    let mut candidates = installed_of(kind);
    if slot == Slot::Agent {
        candidates = agents::selectable_agents();
    }
    let preferred = preferences.get(slot);
    candidates.sort_by_key(|p| (p.id != preferred.unwrap_or_default(), p.priority));
    candidates
}

/// Git status for a project, used by the sidebar.
pub fn project_status(path: impl AsRef<std::path::Path>) -> git::Status {
    git::status(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_ids_are_unique() {
        let programs = registry();
        let mut ids: Vec<&str> = programs.iter().map(|p| p.id.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate program ids in the registry");
    }

    #[test]
    fn editor_choice_can_come_from_the_environment() {
        // Whatever $EDITOR is, the registry must stay usable: unique ids, and
        // an editor candidate that is installed.
        let programs = registry();
        if let Some(editor) = crate::config::preferred_editor_from_env() {
            let matching: Vec<_> = programs
                .iter()
                .filter(|p| p.command == editor && p.kind == Kind::Editor)
                .collect();
            assert_eq!(matching.len(), 1, "$EDITOR should appear exactly once");
        }
        assert!(!installed_of(Kind::Editor).is_empty());
    }

    #[test]
    fn every_kind_has_a_candidate_on_this_machine() {
        assert!(!installed_of(Kind::Editor).is_empty());
        assert!(!installed_of(Kind::Shell).is_empty());
        assert!(!installed_of(Kind::Diff).is_empty());
    }

    #[test]
    fn agents_are_registered_and_flagged_from_omarchy() {
        let agents = agents::installable_agents();
        assert!(agents.len() > 8, "expected omarchy's agent list");
        assert!(agents.iter().any(|a| a.id == "claude"));
        assert!(agents.iter().all(|a| a.kind == Kind::Agent));
        assert!(agents.iter().any(|a| a.omarchy));
    }

    #[test]
    fn agent_choice_lists_only_supported_providers() {
        let prefs = Preferences {
            agent: Some("claude".into()),
            ..Default::default()
        };
        let candidates = candidates_for_slot(Slot::Agent, &prefs);
        assert!(candidates
            .iter()
            .all(|program| agents::is_supported(&program.id)));
        assert!(!candidates.iter().any(|program| program.id == "claude"));
    }

    #[test]
    fn old_unsupported_agent_preference_resolves_to_a_supported_fallback() {
        let prefs = Preferences {
            agent: Some("claude".into()),
            ..Default::default()
        };
        assert!(
            for_slot(Slot::Agent, &prefs).is_none_or(|program| agents::is_supported(&program.id))
        );
    }

    #[test]
    fn external_programs_are_never_offered_for_a_tab() {
        for program in embeddable() {
            assert!(!program.external);
        }
    }

    #[test]
    fn for_slot_honours_an_installed_preference() {
        let prefs = Preferences {
            editor: Some("nvim".to_string()),
            ..Default::default()
        };
        let chosen = for_slot(Slot::Editor, &prefs).expect("an editor");
        assert_eq!(chosen.id, "nvim");
    }

    #[test]
    fn for_slot_falls_back_when_the_preference_is_gone() {
        let prefs = Preferences {
            editor: Some("no-such-editor".to_string()),
            ..Default::default()
        };
        let chosen = for_slot(Slot::Editor, &prefs).expect("a fallback editor");
        assert_ne!(chosen.id, "no-such-editor");
        assert!(chosen.installed());
    }

    #[test]
    fn diff_kind_is_a_terminal_program() {
        for program in installed_of(Kind::Diff) {
            assert!(!program.external);
        }
    }

    #[test]
    fn the_changes_slot_defaults_to_lazygit() {
        // lazygit is the general git TUI, so it leads the Diff registry and
        // therefore fills the Changes primitive when nothing is preferred.
        if !crate::config::have("lazygit") {
            return;
        }
        let chosen = for_slot(Slot::Diff, &Preferences::default()).expect("a diff program");
        assert_eq!(chosen.id, "lazygit");
        assert_eq!(installed_of(Kind::Diff)[0].id, "lazygit");
    }

    #[test]
    fn candidates_for_a_slot_put_the_preferred_first() {
        let prefs = Preferences {
            editor: Some("vim".to_string()),
            ..Default::default()
        };
        let candidates = candidates_for_slot(Slot::Editor, &prefs);
        if candidates.iter().any(|p| p.id == "vim") {
            assert_eq!(candidates[0].id, "vim");
        }
    }
}
