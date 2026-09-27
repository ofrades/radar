//! Durable, per-project presentation state for the GUI workspace.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{now, Db, Slot, TabKey};

/// A pane group: one pane can show several tabs as chips.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceGroup {
    /// Members in header order.
    pub slots: Vec<TabKey>,
    /// The chip that was active when the workspace was saved.
    pub active: TabKey,
}

/// The direction a split divides its region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceAxis {
    Horizontal,
    Vertical,
}

/// A leaf refers to a group by its anchor tab; splits preserve the user's tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspaceLayout {
    Pane {
        group: TabKey,
    },
    Split {
        axis: WorkspaceAxis,
        ratio: f64,
        key: String,
        first: Box<WorkspaceLayout>,
        second: Box<WorkspaceLayout>,
    },
}

/// Everything needed to reconstruct one project's visible workspace.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceState {
    pub groups: Vec<WorkspaceGroup>,
    pub layout: Option<WorkspaceLayout>,
    /// Last chosen program for each primitive, including primitives currently
    /// hidden from the workspace.
    pub programs: HashMap<Slot, String>,
    /// Divider positions are in pixels, matching GTK's paned widget.
    pub positions: HashMap<String, i32>,
    /// The pane being shown full-screen by the zoom action, if any.
    pub zoomed: Option<TabKey>,
}

impl Db {
    /// Read a project's saved GUI state. Corrupt or older values are ignored.
    pub fn workspace_state(&self, project_id: i64) -> Result<Option<WorkspaceState>> {
        let raw: Option<String> = self
            .conn()
            .query_row(
                "SELECT state FROM workspace_state WHERE project_id = ?1",
                params![project_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(raw.and_then(|value| serde_json::from_str(&value).ok()))
    }

    /// Replace a project's saved GUI state atomically.
    pub fn set_workspace_state(&self, project_id: i64, state: &WorkspaceState) -> Result<()> {
        self.conn().execute(
            "INSERT INTO workspace_state (project_id, state, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(project_id) DO UPDATE SET state = excluded.state, updated_at = excluded.updated_at",
            params![project_id, serde_json::to_string(state)?, now()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_state_round_trips_and_is_per_project() {
        let db = Db::open_in_memory().unwrap();
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let first = db.add_project(first_dir.path()).unwrap();
        let second = db.add_project(second_dir.path()).unwrap();
        let state = WorkspaceState {
            groups: vec![
                WorkspaceGroup {
                    slots: vec![TabKey::first(Slot::Agent), TabKey::first(Slot::Diff)],
                    active: TabKey::first(Slot::Diff),
                },
                WorkspaceGroup {
                    slots: vec![TabKey::first(Slot::Shell)],
                    active: TabKey::first(Slot::Shell),
                },
            ],
            layout: Some(WorkspaceLayout::Split {
                axis: WorkspaceAxis::Horizontal,
                ratio: 0.42,
                key: "main".to_string(),
                first: Box::new(WorkspaceLayout::Pane { group: TabKey::first(Slot::Agent) }),
                second: Box::new(WorkspaceLayout::Pane { group: TabKey::first(Slot::Shell) }),
            }),
            programs: HashMap::from([
                (Slot::Agent, "claude".to_string()),
                (Slot::Shell, "fish".to_string()),
            ]),
            positions: HashMap::from([("main".to_string(), 640)]),
            zoomed: Some(TabKey::first(Slot::Agent)),
        };

        db.set_workspace_state(first.id, &state).unwrap();

        assert_eq!(db.workspace_state(first.id).unwrap(), Some(state));
        assert_eq!(db.workspace_state(second.id).unwrap(), None);
    }

    #[test]
    fn removing_a_project_cascades_its_workspace_state() {
        let db = Db::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let project = db.add_project(dir.path()).unwrap();
        db.set_workspace_state(project.id, &WorkspaceState::default())
            .unwrap();

        db.remove_project(project.id).unwrap();

        assert_eq!(db.workspace_state(project.id).unwrap(), None);
    }
}
