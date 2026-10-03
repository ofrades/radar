//! Settings that belong to one project but live in Radar's global database.

use std::path::Path;

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};

use super::{Db, Preferences, Slot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSettings {
    /// Boards are enabled unless the user opts this project out.
    pub board_enabled: bool,
    /// `None` inherits Radar's global preferred program for that pane.
    pub editor: Option<String>,
    pub agent: Option<String>,
    pub diff: Option<String>,
    pub shell: Option<String>,
    /// Which agent reviews this project's finished cards; `None` inherits
    /// Radar's global reviewer.
    pub reviewer: Option<String>,
}

impl Default for ProjectSettings {
    fn default() -> Self {
        Self {
            board_enabled: true,
            editor: None,
            agent: None,
            diff: None,
            shell: None,
            reviewer: None,
        }
    }
}

impl ProjectSettings {
    pub fn apply_to(&self, global: &Preferences) -> Preferences {
        let mut effective = global.clone();
        for slot in [Slot::Editor, Slot::Agent, Slot::Diff, Slot::Shell] {
            if let Some(program) = self.get(slot) {
                effective.set(slot, Some(program.to_string()));
            }
        }
        if let Some(reviewer) = &self.reviewer {
            effective.reviewer = Some(reviewer.clone());
        }
        effective
    }

    pub fn get(&self, slot: Slot) -> Option<&str> {
        match slot {
            Slot::Editor => self.editor.as_deref(),
            Slot::Agent => self.agent.as_deref(),
            Slot::Diff => self.diff.as_deref(),
            Slot::Shell => self.shell.as_deref(),
            Slot::Board | Slot::Custom => None,
        }
    }
}

impl Db {
    /// Project settings default to board-enabled and inherit global pane defaults.
    pub fn project_settings(&self, project_id: i64) -> Result<ProjectSettings> {
        let settings = self.conn().query_row(
            "SELECT board_enabled, editor, agent, diff, shell, reviewer FROM project_settings WHERE project_id = ?1",
            params![project_id],
            |row| {
                Ok(ProjectSettings {
                    board_enabled: row.get::<_, i64>(0)? != 0,
                    editor: row.get(1)?,
                    agent: row.get(2)?,
                    diff: row.get(3)?,
                    shell: row.get(4)?,
                    reviewer: row.get(5)?,
                })
            },
        ).optional()?;
        Ok(settings.unwrap_or_default())
    }

    /// The agent that reviews this project's finished cards: its own
    /// override, else Radar's global reviewer; `None` means nobody is
    /// dispatched and review stays a human's step.
    pub fn reviewer(&self, project_id: i64) -> Result<Option<String>> {
        if let Some(reviewer) = self.project_settings(project_id)?.reviewer {
            return Ok(Some(reviewer));
        }
        Ok(self.preferences()?.reviewer)
    }

    /// Record one project's reviewer override (`None` clears back to global).
    pub fn set_project_reviewer(&self, project_id: i64, program_id: Option<&str>) -> Result<()> {
        if self.project(project_id)?.is_none() {
            anyhow::bail!("no project {project_id}");
        }
        self.conn().execute(
            "INSERT INTO project_settings (project_id, reviewer) VALUES (?1, ?2)
             ON CONFLICT(project_id) DO UPDATE SET reviewer = excluded.reviewer",
            params![project_id, program_id],
        )?;
        Ok(())
    }

    /// Unregistered project paths have no override and retain global defaults.
    pub fn project_settings_for_path(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<ProjectSettings> {
        match self.project_by_path(path)? {
            Some(project) => self.project_settings(project.id),
            None => Ok(ProjectSettings::default()),
        }
    }

    pub fn set_project_board_enabled(&self, project_id: i64, enabled: bool) -> Result<()> {
        self.conn().execute(
            "INSERT INTO project_settings (project_id, board_enabled) VALUES (?1, ?2)
             ON CONFLICT(project_id) DO UPDATE SET board_enabled = excluded.board_enabled",
            params![project_id, enabled as i64],
        )?;
        Ok(())
    }

    /// Whether Radar's board is enabled for a project. Unregistered paths have
    /// no override and retain the default-enabled behavior.
    pub fn board_enabled(&self, project: &Path) -> Result<bool> {
        Ok(self.project_settings_for_path(project)?.board_enabled)
    }

    /// Fail when this project's board is disabled in Radar's global settings.
    pub fn require_board_enabled(&self, project: &Path) -> Result<()> {
        if !self.board_enabled(project)? {
            bail!("the board is disabled for {}", project.display());
        }
        Ok(())
    }

    /// Persist a project's board preference in Radar's global database.
    pub fn set_board_enabled(&self, project: &Path, enabled: bool) -> Result<()> {
        let project = self
            .project_by_path(project)?
            .context("project is not registered in Radar")?;
        self.set_project_board_enabled(project.id, enabled)
    }

    /// Set a project override, or clear it to inherit Radar's global preference.
    pub fn set_project_preference(
        &self,
        project_id: i64,
        slot: Slot,
        program_id: Option<&str>,
    ) -> Result<()> {
        let column = match slot {
            Slot::Editor => "editor",
            Slot::Agent => "agent",
            Slot::Diff => "diff",
            Slot::Shell => "shell",
            Slot::Board | Slot::Custom => return Ok(()),
        };
        self.conn().execute(
            &format!(
                "INSERT INTO project_settings (project_id, {column}) VALUES (?1, ?2)
                 ON CONFLICT(project_id) DO UPDATE SET {column} = excluded.{column}"
            ),
            params![project_id, program_id],
        )?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;

    #[test]
    fn project_overrides_are_isolated_persistent_and_never_touch_project_files() {
        let root = tempfile::tempdir().unwrap();
        let project_path = root.path().join("project");
        std::fs::create_dir(&project_path).unwrap();
        let paths = Paths::with_root(root.path().join("radar-home"));
        let db = Db::open(&paths).unwrap();
        let project = db.add_project(&project_path).unwrap();
        let other_path = root.path().join("other");
        std::fs::create_dir(&other_path).unwrap();
        let other = db.add_project(&other_path).unwrap();

        assert!(db.project_settings(project.id).unwrap().board_enabled);
        assert_eq!(
            db.project_settings(project.id).unwrap().agent,
            None,
            "unset project app preferences inherit global choices"
        );
        db.set_project_board_enabled(project.id, false).unwrap();
        db.set_project_preference(project.id, Slot::Agent, Some("claude"))
            .unwrap();

        let project_settings = db.project_settings(project.id).unwrap();
        assert!(!project_settings.board_enabled);
        assert_eq!(project_settings.agent.as_deref(), Some("claude"));
        assert!(db.project_settings(other.id).unwrap().board_enabled);
        assert_eq!(db.project_settings(other.id).unwrap().agent, None);
        assert!(!project_path.join(".radar.toml").exists());

        drop(db);
        let reopened = Db::open(&paths).unwrap();
        let persisted = reopened.project_settings(project.id).unwrap();
        assert!(!persisted.board_enabled);
        assert_eq!(persisted.agent.as_deref(), Some("claude"));
    }

    #[test]
    fn unregistered_project_paths_have_global_defaults() {
        let db = Db::open_in_memory().unwrap();
        let path = tempfile::tempdir().unwrap();
        assert_eq!(
            db.project_settings_for_path(path.path()).unwrap(),
            ProjectSettings::default()
        );
    }

    #[test]
    fn project_preferences_override_only_configured_global_panes() {
        let global = Preferences {
            editor: Some("nvim".into()),
            agent: Some("codex".into()),
            diff: Some("delta".into()),
            shell: Some("foot".into()),
            ..Preferences::default()
        };
        let settings = ProjectSettings {
            agent: Some("claude".into()),
            ..ProjectSettings::default()
        };
        let effective = settings.apply_to(&global);
        assert_eq!(effective.editor.as_deref(), Some("nvim"));
        assert_eq!(effective.agent.as_deref(), Some("claude"));
        assert_eq!(effective.diff.as_deref(), Some("delta"));
        assert_eq!(effective.shell.as_deref(), Some("foot"));
    }
}
