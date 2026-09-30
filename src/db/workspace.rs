//! Durable, per-project presentation state for the GUI workspace.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{now, Db, Slot, TabKey};

/// A saved panel: one header over one tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspacePanel {
    /// The tab this panel shows.
    pub slot: TabKey,
}

/// The direction a split divides its region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceAxis {
    Horizontal,
    Vertical,
}

/// A leaf refers to a panel by its tab; splits preserve the user's tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspaceLayout {
    Pane {
        panel: TabKey,
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
    pub panels: Vec<WorkspacePanel>,
    pub layout: Option<WorkspaceLayout>,
    /// Legacy board presentation flag, retained for reading older snapshots.
    /// Boards now live in Home rather than in a tool workspace.
    pub board_open: bool,
    /// Last chosen program for each primitive, including primitives currently
    /// hidden from the workspace.
    pub programs: HashMap<Slot, String>,
    /// Divider positions are in pixels, matching GTK's paned widget.
    pub positions: HashMap<String, i32>,
    /// The pane being shown full-screen by the zoom action, if any.
    pub zoomed: Option<TabKey>,
}

/// Reconciled presentation state for the tools that actually exist. This is
/// independent of widgets and processes: callers supply the available tab keys.
#[derive(Debug, PartialEq)]
pub struct WorkspaceRestorePlan {
    pub panels: Vec<WorkspacePanel>,
    pub layout: Option<WorkspaceLayout>,
    pub layout_anchors: Vec<TabKey>,
    pub zoomed: Option<TabKey>,
}

/// Preserve panel order and valid splits, remove retired/missing/duplicate
/// leaves, and collapse empty split branches. A retired Board is never a tool,
/// even when an older snapshot forgot to set its board presentation flag.
pub fn workspace_restore_plan(
    state: Option<&WorkspaceState>,
    wanted: &[TabKey],
) -> WorkspaceRestorePlan {
    let mut panels = Vec::new();
    let mut assigned = HashSet::new();
    for key in state
        .into_iter()
        .flat_map(|state| state.panels.iter().map(|panel| panel.slot))
        .chain(wanted.iter().copied())
    {
        if key.slot != Slot::Board && wanted.contains(&key) && assigned.insert(key) {
            panels.push(WorkspacePanel { slot: key });
        }
    }
    let zoomed = state
        .and_then(|state| state.zoomed)
        .filter(|key| panels.len() > 1 && assigned.contains(key));
    let layout = state
        .and_then(|state| state.layout.as_ref())
        .and_then(|layout| reconcile_layout(layout, &assigned, &mut HashSet::new()));
    let layout_anchors = panels.iter().map(|panel| panel.slot).collect();
    WorkspaceRestorePlan {
        panels,
        layout,
        layout_anchors,
        zoomed,
    }
}

fn reconcile_layout(
    node: &WorkspaceLayout,
    available: &HashSet<TabKey>,
    seen: &mut HashSet<TabKey>,
) -> Option<WorkspaceLayout> {
    match node {
        WorkspaceLayout::Pane { panel } => {
            (available.contains(panel) && seen.insert(*panel)).then(|| node.clone())
        }
        WorkspaceLayout::Split {
            axis,
            ratio,
            key,
            first,
            second,
        } => {
            match (
                reconcile_layout(first, available, seen),
                reconcile_layout(second, available, seen),
            ) {
                (Some(first), Some(second)) => Some(WorkspaceLayout::Split {
                    axis: *axis,
                    ratio: if ratio.is_finite() {
                        ratio.clamp(0.05, 0.95)
                    } else {
                        0.5
                    },
                    key: key.clone(),
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(remaining), None) | (None, Some(remaining)) => Some(remaining),
                (None, None) => None,
            }
        }
    }
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

    fn pane(slot: Slot) -> WorkspaceLayout {
        WorkspaceLayout::Pane {
            panel: TabKey::first(slot),
        }
    }

    fn split(first: WorkspaceLayout, second: WorkspaceLayout) -> WorkspaceLayout {
        WorkspaceLayout::Split {
            axis: WorkspaceAxis::Horizontal,
            ratio: 0.42,
            key: "main".into(),
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[test]
    fn restoration_removes_all_board_instances_without_legacy_flags() {
        let agent = TabKey::first(Slot::Agent);
        let board = TabKey {
            slot: Slot::Board,
            instance: 2,
        };
        let state = WorkspaceState {
            panels: vec![
                WorkspacePanel { slot: board },
                WorkspacePanel { slot: agent },
            ],
            layout: Some(split(
                pane(Slot::Agent),
                WorkspaceLayout::Pane { panel: board },
            )),
            zoomed: Some(board),
            ..WorkspaceState::default()
        };
        let plan = workspace_restore_plan(Some(&state), &[agent, board]);
        assert_eq!(plan.panels, vec![WorkspacePanel { slot: agent }]);
        assert_eq!(plan.layout, Some(pane(Slot::Agent)));
        assert_eq!(plan.zoomed, None);
    }

    #[test]
    fn restoration_preserves_order_and_collapses_missing_and_duplicate_leaves() {
        let agent = TabKey::first(Slot::Agent);
        let diff = TabKey::first(Slot::Diff);
        let shell = TabKey::first(Slot::Shell);
        let state = WorkspaceState {
            panels: vec![
                WorkspacePanel { slot: diff },
                WorkspacePanel { slot: agent },
                WorkspacePanel { slot: diff },
                WorkspacePanel { slot: shell },
            ],
            layout: Some(split(
                pane(Slot::Diff),
                split(
                    pane(Slot::Shell),
                    split(pane(Slot::Agent), pane(Slot::Diff)),
                ),
            )),
            zoomed: Some(shell),
            ..WorkspaceState::default()
        };
        let plan = workspace_restore_plan(Some(&state), &[agent, diff, agent]);
        assert_eq!(plan.layout_anchors, vec![diff, agent]);
        assert_eq!(
            plan.layout,
            Some(split(pane(Slot::Diff), pane(Slot::Agent)))
        );
        assert_eq!(plan.zoomed, None);
    }

    #[test]
    fn restoration_adds_new_tools_and_keeps_valid_zoom() {
        let agent = TabKey::first(Slot::Agent);
        let shell = TabKey::first(Slot::Shell);
        let state = WorkspaceState {
            panels: vec![WorkspacePanel { slot: agent }],
            layout: Some(pane(Slot::Agent)),
            zoomed: Some(agent),
            ..WorkspaceState::default()
        };
        let plan = workspace_restore_plan(Some(&state), &[agent, shell]);
        assert_eq!(plan.layout_anchors, vec![agent, shell]);
        assert_eq!(plan.zoomed, Some(agent));
        let single = workspace_restore_plan(Some(&state), &[agent]);
        assert_eq!(single.zoomed, None);
        let empty = workspace_restore_plan(Some(&state), &[]);
        assert!(empty.panels.is_empty());
        assert_eq!(empty.layout, None);
        assert_eq!(empty.zoomed, None);
    }

    #[test]
    fn restoration_sanitizes_split_ratios() {
        for (ratio, expected) in [(f64::NAN, 0.5), (-1.0, 0.05), (2.0, 0.95)] {
            let mut layout = split(pane(Slot::Agent), pane(Slot::Diff));
            if let WorkspaceLayout::Split { ratio: value, .. } = &mut layout {
                *value = ratio;
            }
            let state = WorkspaceState {
                layout: Some(layout),
                ..WorkspaceState::default()
            };
            let plan = workspace_restore_plan(
                Some(&state),
                &[TabKey::first(Slot::Agent), TabKey::first(Slot::Diff)],
            );
            let Some(WorkspaceLayout::Split { ratio, .. }) = plan.layout else {
                panic!("missing split")
            };
            assert_eq!(ratio, expected);
        }
    }

    #[test]
    fn workspace_state_round_trips_and_is_per_project() {
        let db = Db::open_in_memory().unwrap();
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let first = db.add_project(first_dir.path()).unwrap();
        let second = db.add_project(second_dir.path()).unwrap();
        let state = WorkspaceState {
            panels: vec![
                WorkspacePanel {
                    slot: TabKey::first(Slot::Agent),
                },
                WorkspacePanel {
                    slot: TabKey::first(Slot::Shell),
                },
            ],
            layout: Some(WorkspaceLayout::Split {
                axis: WorkspaceAxis::Horizontal,
                ratio: 0.42,
                key: "main".to_string(),
                first: Box::new(WorkspaceLayout::Pane {
                    panel: TabKey::first(Slot::Agent),
                }),
                second: Box::new(WorkspaceLayout::Pane {
                    panel: TabKey::first(Slot::Shell),
                }),
            }),
            programs: HashMap::from([
                (Slot::Agent, "claude".to_string()),
                (Slot::Shell, "fish".to_string()),
            ]),
            positions: HashMap::from([("main".to_string(), 640)]),
            zoomed: Some(TabKey::first(Slot::Agent)),
            board_open: true,
        };

        db.set_workspace_state(first.id, &state).unwrap();

        assert_eq!(db.workspace_state(first.id).unwrap(), Some(state));
        let legacy: WorkspaceState = serde_json::from_str(r#"{"panels":[]}"#).unwrap();
        assert!(!legacy.board_open);
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
