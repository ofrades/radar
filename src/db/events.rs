//! A small append-only log of what happened, so the UI can show recents and
//! history without guessing.

use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{now, Db};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub at: i64,
    pub project_id: Option<i64>,
    pub kind: String,
    pub data: Option<serde_json::Value>,
}

impl Db {
    /// Record an event. Logging must never be the reason an action fails, so
    /// callers may ignore the result, but the signature keeps errors visible.
    pub fn log_event(
        &self,
        kind: &str,
        project_id: Option<i64>,
        data: &serde_json::Value,
    ) -> Result<()> {
        let encoded = if data.is_null() {
            None
        } else {
            Some(serde_json::to_string(data)?)
        };
        self.conn().execute(
            "INSERT INTO events (at, project_id, kind, data) VALUES (?1, ?2, ?3, ?4)",
            params![now(), project_id, kind, encoded],
        )?;
        Ok(())
    }

    /// Newest first.
    pub fn events(&self, limit: usize) -> Result<Vec<Event>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, at, project_id, kind, data FROM events ORDER BY at DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], event_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn events_for_project(&self, project_id: i64, limit: usize) -> Result<Vec<Event>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, at, project_id, kind, data FROM events \
             WHERE project_id = ?1 ORDER BY at DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![project_id, limit as i64], event_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    let data: Option<String> = row.get(4)?;
    Ok(Event {
        id: row.get(0)?,
        at: row.get(1)?,
        project_id: row.get(2)?,
        kind: row.get(3)?,
        data: data.and_then(|raw| serde_json::from_str(&raw).ok()),
    })
}

#[cfg(test)]
mod tests {
    use crate::db::Db;

    #[test]
    fn adding_a_project_is_logged() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        let project = db.add_project(dir.path()).unwrap();
        let events = db.events(10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "project_added");
        assert_eq!(events[0].project_id, Some(project.id));
        assert!(events[0].data.as_ref().unwrap()["path"].is_string());
    }

    #[test]
    fn events_are_newest_first_and_scoped() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        let a = db.add_project(first.path()).unwrap();
        let b = db.add_project(second.path()).unwrap();
        db.log_event(
            "opened",
            Some(a.id),
            &serde_json::json!({ "tab": "editor" }),
        )
        .unwrap();
        db.log_event("opened", Some(b.id), &serde_json::Value::Null)
            .unwrap();

        let all = db.events(10).unwrap();
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].project_id, Some(b.id));

        let only_a = db.events_for_project(a.id, 10).unwrap();
        assert_eq!(only_a.len(), 2);
        assert!(only_a.iter().all(|e| e.project_id == Some(a.id)));
    }

    #[test]
    fn null_data_is_stored_as_nothing() {
        let db = Db::open_in_memory().unwrap();
        db.log_event("ping", None, &serde_json::Value::Null)
            .unwrap();
        let event = &db.events(1).unwrap()[0];
        assert_eq!(event.kind, "ping");
        assert!(event.data.is_none());
    }
}
