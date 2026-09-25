//! Tabs: what runs inside a project's workspace.
//!
//! A tab is a *definition* — which slot it fills and which program it runs —
//! not a live process. The GUI turns definitions into terminals on open, and
//! stores them back when tabs are added, closed or reordered, so reopening a
//! project restores the workspace you left.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{now, Db};

/// The role a tab plays. The first four have a user preference attached, which
/// is how "my preferred editor/agent/diff" works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Slot {
    Editor,
    Agent,
    Diff,
    Shell,
    /// Anything else: any program the registry knows about.
    Custom,
}

impl Slot {
    pub const ALL: [Slot; 5] = [Slot::Editor, Slot::Agent, Slot::Diff, Slot::Shell, Slot::Custom];

    pub const fn as_str(self) -> &'static str {
        match self {
            Slot::Editor => "editor",
            Slot::Agent => "agent",
            Slot::Diff => "diff",
            Slot::Shell => "shell",
            Slot::Custom => "custom",
        }
    }

    pub fn parse(value: &str) -> Slot {
        match value {
            "editor" => Slot::Editor,
            "agent" => Slot::Agent,
            "diff" => Slot::Diff,
            "shell" => Slot::Shell,
            _ => Slot::Custom,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Slot::Editor => "Editor",
            Slot::Agent => "Agent",
            Slot::Diff => "Changes",
            Slot::Shell => "Commands",
            Slot::Custom => "Program",
        }
    }

    /// Key used in [`super::Preferences`] for this slot, if it has one.
    pub const fn preference_key(self) -> Option<&'static str> {
        match self {
            Slot::Editor => Some("editor"),
            Slot::Agent => Some("agent"),
            Slot::Diff => Some("diff"),
            Slot::Shell => Some("shell"),
            Slot::Custom => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    /// Zero until the tab is stored.
    pub id: i64,
    pub project_id: i64,
    pub slot: Slot,
    pub program_id: String,
    pub title: Option<String>,
    pub sort_order: i64,
    /// Extra arguments appended to the program's default argv.
    pub extra_args: Vec<String>,
    pub created_at: i64,
}

impl Tab {
    pub fn new(slot: Slot, program_id: impl Into<String>) -> Tab {
        Tab {
            id: 0,
            project_id: 0,
            slot,
            program_id: program_id.into(),
            title: None,
            sort_order: 0,
            extra_args: Vec::new(),
            created_at: now(),
        }
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Tab {
        self.title = Some(title.into());
        self
    }

    pub fn with_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Tab {
        self.extra_args = args.into_iter().map(Into::into).collect();
        self
    }

    /// What the tab bar shows.
    pub fn display_title(&self, program_name: &str) -> String {
        self.title.clone().unwrap_or_else(|| program_name.to_string())
    }
}

const COLUMNS: &str = "id, project_id, slot, program_id, title, sort_order, extra_args, created_at";

fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Tab> {
    let extra: String = row.get(6)?;
    Ok(Tab {
        id: row.get(0)?,
        project_id: row.get(1)?,
        slot: Slot::parse(&row.get::<_, String>(2)?),
        program_id: row.get(3)?,
        title: row.get(4)?,
        sort_order: row.get(5)?,
        extra_args: serde_json::from_str(&extra).unwrap_or_default(),
        created_at: row.get(7)?,
    })
}

impl Db {
    /// Tabs of a project, in bar order.
    pub fn tabs(&self, project_id: i64) -> Result<Vec<Tab>> {
        let sql = format!(
            "SELECT {COLUMNS} FROM tabs WHERE project_id = ?1 ORDER BY sort_order ASC, id ASC"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(params![project_id], from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Replace a project's tabs in one transaction.
    pub fn set_tabs(&self, project_id: i64, tabs: &[Tab]) -> Result<()> {
        let tx = self.conn().unchecked_transaction()?;
        tx.execute("DELETE FROM tabs WHERE project_id = ?1", params![project_id])?;
        for (index, tab) in tabs.iter().enumerate() {
            insert_tab(&tx, project_id, tab, index as i64)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Append one tab and return it with its new id.
    pub fn add_tab(&self, project_id: i64, tab: &Tab) -> Result<Tab> {
        let next: i64 = self.conn().query_row(
            "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM tabs WHERE project_id = ?1",
            params![project_id],
            |row| row.get(0),
        )?;
        insert_tab(self.conn(), project_id, tab, next)?;
        let id = self.conn().last_insert_rowid();
        let stored = self
            .conn()
            .query_row(
                &format!("SELECT {COLUMNS} FROM tabs WHERE id = ?1"),
                params![id],
                from_row,
            )
            .optional()?;
        stored.ok_or_else(|| anyhow::anyhow!("tab vanished right after being inserted"))
    }

    pub fn remove_tab(&self, tab_id: i64) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM tabs WHERE id = ?1", params![tab_id])?
            > 0)
    }

    pub fn rename_tab(&self, tab_id: i64, title: Option<&str>) -> Result<()> {
        let title = title.map(|t| t.trim()).filter(|t| !t.is_empty());
        self.conn().execute(
            "UPDATE tabs SET title = ?2 WHERE id = ?1",
            params![tab_id, title],
        )?;
        Ok(())
    }

    /// Reorder a project's tabs to match `order` (ids not listed keep their
    /// relative position at the end).
    pub fn reorder_tabs(&self, project_id: i64, order: &[i64]) -> Result<()> {
        let existing = self.tabs(project_id)?;
        let mut position = 0i64;
        for id in order {
            if existing.iter().any(|t| t.id == *id) {
                self.conn().execute(
                    "UPDATE tabs SET sort_order = ?2 WHERE id = ?1 AND project_id = ?3",
                    params![id, position, project_id],
                )?;
                position += 1;
            }
        }
        for tab in existing {
            if !order.contains(&tab.id) {
                self.conn().execute(
                    "UPDATE tabs SET sort_order = ?2 WHERE id = ?1",
                    params![tab.id, position],
                )?;
                position += 1;
            }
        }
        Ok(())
    }
}

fn insert_tab(
    conn: &rusqlite::Connection,
    project_id: i64,
    tab: &Tab,
    sort_order: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO tabs (project_id, slot, program_id, title, sort_order, extra_args, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            project_id,
            tab.slot.as_str(),
            tab.program_id,
            tab.title,
            sort_order,
            serde_json::to_string(&tab.extra_args)?,
            if tab.created_at > 0 { tab.created_at } else { now() },
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_with_tabs() -> (Db, i64, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        let project = db.add_project(dir.path()).unwrap();
        (db, project.id, dir)
    }

    #[test]
    fn slot_round_trips_through_text() {
        for slot in Slot::ALL {
            assert_eq!(Slot::parse(slot.as_str()), slot);
        }
        assert_eq!(Slot::parse("nonsense"), Slot::Custom);
    }

    #[test]
    fn set_tabs_replaces_and_orders() {
        let (db, project, _dir) = project_with_tabs();
        db.set_tabs(
            project,
            &[
                Tab::new(Slot::Editor, "nvim"),
                Tab::new(Slot::Agent, "claude"),
                Tab::new(Slot::Diff, "hunk"),
            ],
        )
        .unwrap();
        let tabs = db.tabs(project).unwrap();
        let ids: Vec<&str> = tabs.iter().map(|t| t.program_id.as_str()).collect();
        assert_eq!(ids, vec!["nvim", "claude", "hunk"]);

        db.set_tabs(project, &[Tab::new(Slot::Shell, "bash")]).unwrap();
        let tabs = db.tabs(project).unwrap();
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].slot, Slot::Shell);
    }

    #[test]
    fn extra_args_survive_a_round_trip() {
        let (db, project, _dir) = project_with_tabs();
        db.add_tab(
            project,
            &Tab::new(Slot::Diff, "hunk").with_args(["--mode", "split"]),
        )
        .unwrap();
        let tab = &db.tabs(project).unwrap()[0];
        assert_eq!(tab.extra_args, vec!["--mode", "split"]);
    }

    #[test]
    fn add_tab_appends_and_returns_the_stored_row() {
        let (db, project, _dir) = project_with_tabs();
        let first = db.add_tab(project, &Tab::new(Slot::Editor, "nvim")).unwrap();
        let second = db.add_tab(project, &Tab::new(Slot::Agent, "codex")).unwrap();
        assert!(first.id > 0);
        assert!(second.id > first.id);
        assert_eq!(second.sort_order, 1);
        assert_eq!(db.tabs(project).unwrap().len(), 2);
    }

    #[test]
    fn reorder_moves_tabs_and_keeps_the_rest() {
        let (db, project, _dir) = project_with_tabs();
        let a = db.add_tab(project, &Tab::new(Slot::Editor, "nvim")).unwrap();
        let b = db.add_tab(project, &Tab::new(Slot::Agent, "claude")).unwrap();
        let c = db.add_tab(project, &Tab::new(Slot::Diff, "hunk")).unwrap();
        db.reorder_tabs(project, &[c.id, a.id, b.id]).unwrap();
        let ids: Vec<i64> = db.tabs(project).unwrap().iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![c.id, a.id, b.id]);
        // A partial order must not lose tabs.
        db.reorder_tabs(project, &[b.id]).unwrap();
        let tabs = db.tabs(project).unwrap();
        assert_eq!(tabs.len(), 3);
        assert_eq!(tabs[0].id, b.id);
    }

    #[test]
    fn tab_titles_override_the_program_name() {
        let tab = Tab::new(Slot::Editor, "nvim").with_title("Frontend");
        assert_eq!(tab.display_title("Neovim"), "Frontend");
        assert_eq!(Tab::new(Slot::Editor, "nvim").display_title("Neovim"), "Neovim");
    }

    #[test]
    fn rename_and_remove() {
        let (db, project, _dir) = project_with_tabs();
        let tab = db.add_tab(project, &Tab::new(Slot::Shell, "bash")).unwrap();
        db.rename_tab(tab.id, Some("  build  ")).unwrap();
        assert_eq!(db.tabs(project).unwrap()[0].title.as_deref(), Some("build"));
        db.rename_tab(tab.id, Some("   ")).unwrap();
        assert_eq!(db.tabs(project).unwrap()[0].title, None);
        assert!(db.remove_tab(tab.id).unwrap());
        assert!(db.tabs(project).unwrap().is_empty());
    }

    #[test]
    fn tabs_survive_slots_having_preferences() {
        for slot in Slot::ALL {
            if let Some(key) = slot.preference_key() {
                assert!(!key.is_empty());
            } else {
                assert_eq!(slot, Slot::Custom);
            }
        }
    }
}
