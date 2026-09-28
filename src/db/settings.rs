//! Preferences: which program fills each slot, and how the window behaves.

use std::path::PathBuf;

use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{now, Db, Slot};

const PREFERENCES_KEY: &str = "preferences";
const UI_KEY: &str = "ui";

/// The user's preferred program per slot — "preferred editor / agent / diff".
///
/// Values are program ids from the registry (`nvim`, `claude`, `lazygit`, …).
/// `None` means "pick the best installed one", so a fresh install still works.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub editor: Option<String>,
    pub agent: Option<String>,
    pub diff: Option<String>,
    pub shell: Option<String>,
    /// Pass agents their "don't stop to ask" flags, the way omarchy's
    /// keybinding does. Off means each agent asks for permission itself.
    pub agent_auto_flags: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Preferences {
            editor: None,
            agent: None,
            diff: None,
            shell: None,
            agent_auto_flags: true,
        }
    }
}

impl Preferences {
    pub fn get(&self, slot: Slot) -> Option<&str> {
        match slot {
            Slot::Editor => self.editor.as_deref(),
            Slot::Agent => self.agent.as_deref(),
            Slot::Diff => self.diff.as_deref(),
            Slot::Shell => self.shell.as_deref(),
            Slot::Board | Slot::Custom => None,
        }
    }

    pub fn set(&mut self, slot: Slot, value: Option<String>) {
        match slot {
            Slot::Editor => self.editor = value,
            Slot::Agent => self.agent = value,
            Slot::Diff => self.diff = value,
            Slot::Shell => self.shell = value,
            Slot::Board | Slot::Custom => {}
        }
    }
}

/// What a brand-new workspace opens with, before it has stored panes of its
/// own. The agent leads in every preset; the rest is scope. Picked on the
/// home panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NewWorkspaceLayout {
    Agent,
    AgentChanges,
    Classic,
    Everything,
}

impl NewWorkspaceLayout {
    pub const ALL: [NewWorkspaceLayout; 4] = [
        NewWorkspaceLayout::Agent,
        NewWorkspaceLayout::AgentChanges,
        NewWorkspaceLayout::Classic,
        NewWorkspaceLayout::Everything,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            NewWorkspaceLayout::Agent => "Agent",
            NewWorkspaceLayout::AgentChanges => "Agent + Changes",
            NewWorkspaceLayout::Classic => "Agent + Changes + Commands",
            NewWorkspaceLayout::Everything => "Everything",
        }
    }

    /// The primitives the preset opens, in layout order.
    pub fn slots(self) -> &'static [Slot] {
        use Slot::{Agent, Board, Diff, Editor, Shell};
        match self {
            NewWorkspaceLayout::Agent => &[Agent],
            NewWorkspaceLayout::AgentChanges => &[Agent, Diff],
            NewWorkspaceLayout::Classic => &[Agent, Diff, Shell],
            NewWorkspaceLayout::Everything => &[Agent, Diff, Board, Shell, Editor],
        }
    }
}

/// Window and session preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiPrefs {
    pub sidebar_width: i32,
    /// Project to select on launch.
    pub last_project: Option<i64>,
    /// Directory the "add project" picker starts in.
    pub add_root: Option<PathBuf>,
    /// Reopen the tabs a project had last time.
    pub restore_tabs: bool,
    /// Primitives a brand-new workspace opens with.
    pub layout: Option<NewWorkspaceLayout>,
}

impl Default for UiPrefs {
    fn default() -> Self {
        UiPrefs {
            sidebar_width: 300,
            last_project: None,
            add_root: None,
            restore_tabs: true,
            layout: Some(NewWorkspaceLayout::Agent),
        }
    }
}

impl UiPrefs {
    /// The directory to browse when adding a project.
    pub fn resolved_add_root(&self) -> PathBuf {
        self.add_root
            .clone()
            .filter(|path| path.is_dir())
            .unwrap_or_else(crate::config::default_project_root)
    }
}

impl Db {
    /// Read a JSON setting, or `None` when unset or unreadable.
    fn setting_json<T: for<'de> Deserialize<'de>>(&self, key: &str) -> Result<Option<T>> {
        let raw: Option<String> = self
            .conn()
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .ok();
        let Some(raw) = raw else { return Ok(None) };
        match serde_json::from_str(&raw) {
            Ok(value) => Ok(Some(value)),
            Err(_) => Ok(None),
        }
    }

    fn set_setting_json<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let encoded = serde_json::to_string(value)?;
        self.conn().execute(
            "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, encoded, now()],
        )?;
        Ok(())
    }

    pub fn preferences(&self) -> Result<Preferences> {
        Ok(self.setting_json(PREFERENCES_KEY)?.unwrap_or_default())
    }

    pub fn set_preferences(&self, preferences: &Preferences) -> Result<()> {
        self.set_setting_json(PREFERENCES_KEY, preferences)
    }

    /// Set one slot's preferred program.
    pub fn set_preference(&self, slot: Slot, program_id: Option<&str>) -> Result<Preferences> {
        let mut preferences = self.preferences()?;
        preferences.set(slot, program_id.map(|s| s.to_string()));
        self.set_preferences(&preferences)?;
        Ok(preferences)
    }

    pub fn ui_prefs(&self) -> Result<UiPrefs> {
        Ok(self.setting_json(UI_KEY)?.unwrap_or_default())
    }

    pub fn set_ui_prefs(&self, prefs: &UiPrefs) -> Result<()> {
        self.set_setting_json(UI_KEY, prefs)
    }

    pub fn remember_last_project(&self, project_id: Option<i64>) -> Result<UiPrefs> {
        let mut prefs = self.ui_prefs()?;
        prefs.last_project = project_id;
        self.set_ui_prefs(&prefs)?;
        Ok(prefs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Slot;

    #[test]
    fn preferences_default_to_unset() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.preferences().unwrap(), Preferences::default());
        assert!(db.preferences().unwrap().agent.is_none());
    }

    #[test]
    fn preferences_round_trip_and_are_per_slot() {
        let db = Db::open_in_memory().unwrap();
        db.set_preference(Slot::Editor, Some("nvim")).unwrap();
        db.set_preference(Slot::Agent, Some("claude")).unwrap();
        db.set_preference(Slot::Diff, Some("hunk")).unwrap();

        let prefs = db.preferences().unwrap();
        assert_eq!(prefs.get(Slot::Editor), Some("nvim"));
        assert_eq!(prefs.get(Slot::Agent), Some("claude"));
        assert_eq!(prefs.get(Slot::Diff), Some("hunk"));
        assert_eq!(prefs.get(Slot::Custom), None);

        db.set_preference(Slot::Editor, Some("helix")).unwrap();
        assert_eq!(db.preferences().unwrap().editor.as_deref(), Some("helix"));

        db.set_preference(Slot::Editor, None).unwrap();
        assert!(db.preferences().unwrap().editor.is_none());
        assert_eq!(
            db.preferences().unwrap().agent.as_deref(),
            Some("claude"),
            "clearing one slot must not touch the others"
        );
    }

    #[test]
    fn ui_prefs_have_sane_defaults() {
        let db = Db::open_in_memory().unwrap();
        let prefs = db.ui_prefs().unwrap();
        assert_eq!(prefs.sidebar_width, 300);
        assert!(prefs.restore_tabs);
        assert!(prefs.last_project.is_none());
    }

    #[test]
    fn the_layout_preset_defaults_to_the_agent_and_round_trips() {
        let db = Db::open_in_memory().unwrap();
        let prefs = db.ui_prefs().unwrap();
        assert_eq!(prefs.layout, Some(NewWorkspaceLayout::Agent));
        assert_eq!(prefs.layout.unwrap().slots(), &[Slot::Agent][..]);

        let mut prefs = prefs;
        prefs.layout = Some(NewWorkspaceLayout::Classic);
        db.set_ui_prefs(&prefs).unwrap();
        assert_eq!(
            db.ui_prefs().unwrap().layout,
            Some(NewWorkspaceLayout::Classic),
            "the layout must survive a round trip"
        );
    }

    #[test]
    fn remember_last_project_updates_only_that_field() {
        let db = Db::open_in_memory().unwrap();
        let prefs = crate::db::UiPrefs {
            sidebar_width: 360,
            ..Default::default()
        };
        db.set_ui_prefs(&prefs).unwrap();

        db.remember_last_project(Some(7)).unwrap();
        let prefs = db.ui_prefs().unwrap();
        assert_eq!(prefs.last_project, Some(7));
        assert_eq!(prefs.sidebar_width, 360);
    }

    #[test]
    fn storing_a_setting_twice_updates_it() {
        let db = Db::open_in_memory().unwrap();
        db.remember_last_project(Some(1)).unwrap();
        db.remember_last_project(Some(2)).unwrap();
        assert_eq!(db.ui_prefs().unwrap().last_project, Some(2));
        let rows: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM settings WHERE key = 'ui'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1, "settings must upsert, not accumulate");
    }

    #[test]
    fn corrupt_settings_fall_back_to_defaults() {
        let db = Db::open_in_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO settings (key, value, updated_at) VALUES ('preferences', 'not json', 0)",
                [],
            )
            .unwrap();
        assert_eq!(db.preferences().unwrap(), Preferences::default());
    }

    #[test]
    fn add_root_ignores_a_directory_that_vanished() {
        let prefs = UiPrefs {
            add_root: Some(PathBuf::from("/definitely/not/here")),
            ..Default::default()
        };
        assert_eq!(
            prefs.resolved_add_root(),
            crate::config::default_project_root()
        );
    }
}
