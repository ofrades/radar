//! Projects: the sidebar's contents.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{abbreviate, normalize_path, now, Db};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: i64,
    pub path: PathBuf,
    pub name: String,
    pub pinned: bool,
    pub sort_order: i64,
    pub added_at: i64,
    pub last_opened_at: Option<i64>,
    pub open_count: i64,
    pub archived: bool,
}

impl Project {
    /// `~/Work/api-server`
    pub fn display_path(&self) -> String {
        abbreviate(&self.path)
    }

    /// Has the directory gone away since it was added?
    pub fn is_missing(&self) -> bool {
        !self.path.is_dir()
    }

    /// One line for the sidebar's second row.
    pub fn subtitle(&self) -> String {
        if self.is_missing() {
            return format!("{} · missing", self.display_path());
        }
        self.display_path()
    }
}

const COLUMNS: &str = "id, path, name, pinned, sort_order, added_at, last_opened_at, \
                       open_count, archived";

fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        path: PathBuf::from(row.get::<_, String>(1)?),
        name: row.get(2)?,
        pinned: row.get::<_, i64>(3)? != 0,
        sort_order: row.get(4)?,
        added_at: row.get(5)?,
        last_opened_at: row.get(6)?,
        open_count: row.get(7)?,
        archived: row.get(8)?,
    })
}

impl Db {
    /// Every project, in sidebar order: pinned first, then manual order, then
    /// name. Archived projects are left out.
    pub fn projects(&self) -> Result<Vec<Project>> {
        let sql = format!(
            "SELECT {COLUMNS} FROM projects WHERE archived = 0 \
             ORDER BY pinned DESC, sort_order ASC, name COLLATE NOCASE ASC"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map([], from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Most recently opened first.
    pub fn recent_projects(&self, limit: usize) -> Result<Vec<Project>> {
        let sql = format!(
            "SELECT {COLUMNS} FROM projects WHERE archived = 0 AND last_opened_at IS NOT NULL \
             ORDER BY last_opened_at DESC LIMIT ?1"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(params![limit as i64], from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn project_by_path(&self, path: impl AsRef<Path>) -> Result<Option<Project>> {
        let path = normalize_path(path)?;
        self.project_by_normalized(&path)
    }

    fn project_by_normalized(&self, path: &Path) -> Result<Option<Project>> {
        let sql = format!("SELECT {COLUMNS} FROM projects WHERE path = ?1");
        let project = self
            .conn()
            .query_row(&sql, params![path.to_string_lossy()], from_row)
            .optional()?;
        Ok(project)
    }

    pub fn project(&self, id: i64) -> Result<Option<Project>> {
        let sql = format!("SELECT {COLUMNS} FROM projects WHERE id = ?1");
        Ok(self
            .conn()
            .query_row(&sql, params![id], from_row)
            .optional()?)
    }

    /// Add a directory, or return the existing entry for it.
    ///
    /// Errors when the path is not a directory: a project the sidebar cannot
    /// open is worse than a clear message.
    pub fn add_project(&self, path: impl AsRef<Path>) -> Result<Project> {
        let path = normalize_path(path)?;
        if !path.is_dir() {
            bail!("not a directory: {}", path.display());
        }
        if let Some(existing) = self.project_by_normalized(&path)? {
            return Ok(existing);
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| path.display().to_string());
        let next_order: i64 = self.conn().query_row(
            "SELECT COALESCE(MAX(sort_order), 0) + 1 FROM projects",
            [],
            |row| row.get(0),
        )?;
        self.conn().execute(
            "INSERT INTO projects (path, name, sort_order, added_at) VALUES (?1, ?2, ?3, ?4)",
            params![path.to_string_lossy(), name, next_order, now()],
        )?;
        let id = self.conn().last_insert_rowid();
        self.log_event(
            "project_added",
            Some(id),
            &serde_json::json!({ "path": path }),
        )?;
        self.project(id)?
            .context("project vanished right after being inserted")
    }

    /// Remove a project from the list. Never touches the filesystem.
    pub fn remove_project(&self, id: i64) -> Result<bool> {
        let removed = self
            .conn()
            .execute("DELETE FROM projects WHERE id = ?1", params![id])?;
        if removed > 0 {
            self.log_event("project_removed", None, &serde_json::json!({ "id": id }))?;
        }
        Ok(removed > 0)
    }

    pub fn rename_project(&self, id: i64, name: &str) -> Result<()> {
        let name = name.trim();
        let name = if name.is_empty() {
            let project = self.project(id)?.context("unknown project")?;
            project
                .path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| project.path.display().to_string())
        } else {
            name.to_string()
        };
        self.conn().execute(
            "UPDATE projects SET name = ?2 WHERE id = ?1",
            params![id, name],
        )?;
        Ok(())
    }

    pub fn set_pinned(&self, id: i64, pinned: bool) -> Result<()> {
        self.conn().execute(
            "UPDATE projects SET pinned = ?2 WHERE id = ?1",
            params![id, pinned as i64],
        )?;
        Ok(())
    }

    pub fn set_archived(&self, id: i64, archived: bool) -> Result<()> {
        self.conn().execute(
            "UPDATE projects SET archived = ?2 WHERE id = ?1",
            params![id, archived as i64],
        )?;
        Ok(())
    }

    /// Mark a project as opened just now.
    pub fn touch_project(&self, id: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE projects SET last_opened_at = ?2, open_count = open_count + 1 WHERE id = ?1",
            params![id, now()],
        )?;
        Ok(())
    }

    /// Move a project up or down the manual order. Pinned projects form their
    /// own block, and a move is clamped inside it, so sorting never fights the
    /// pinned flag.
    pub fn move_project(&self, id: i64, delta: i64) -> Result<()> {
        let projects = self.projects()?;
        let Some(from) = projects.iter().position(|p| p.id == id) else {
            return Ok(());
        };
        let pinned = projects[from].pinned;
        let block: Vec<usize> = projects
            .iter()
            .enumerate()
            .filter(|(_, p)| p.pinned == pinned)
            .map(|(index, _)| index)
            .collect();
        let Some(pos_in_block) = block.iter().position(|index| *index == from) else {
            return Ok(());
        };
        let target_pos = (pos_in_block as i64 + delta).clamp(0, block.len() as i64 - 1) as usize;
        if target_pos == pos_in_block {
            return Ok(());
        }
        let target = block[target_pos];
        let mut order: Vec<i64> = projects.iter().map(|p| p.id).collect();
        let moved = order.remove(from);
        order.insert(target, moved);
        for (index, project_id) in order.iter().enumerate() {
            self.conn().execute(
                "UPDATE projects SET sort_order = ?2 WHERE id = ?1",
                params![project_id, index as i64],
            )?;
        }
        Ok(())
    }

    /// Drop projects whose directory no longer exists.
    pub fn prune_missing(&self) -> Result<Vec<Project>> {
        let missing: Vec<Project> = self
            .projects()?
            .into_iter()
            .filter(|p| p.is_missing())
            .collect();
        for project in &missing {
            self.remove_project(project.id)?;
        }
        Ok(missing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_with_tmpdir() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Db::open_in_memory().unwrap(), dir)
    }

    #[test]
    fn add_project_is_idempotent() {
        let (db, dir) = db_with_tmpdir();
        let first = db.add_project(dir.path()).unwrap();
        let second = db.add_project(dir.path()).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(db.projects().unwrap().len(), 1);
    }

    #[test]
    fn add_project_rejects_files_and_missing_paths() {
        let (db, dir) = db_with_tmpdir();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hi").unwrap();
        assert!(db.add_project(&file).is_err());
        assert!(db.add_project(dir.path().join("nope")).is_err());
    }

    #[test]
    fn add_project_names_it_after_the_directory() {
        let (db, dir) = db_with_tmpdir();
        let project = db.add_project(dir.path()).unwrap();
        let expected = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(project.name, expected);
    }

    #[test]
    fn projects_are_ordered_pinned_first_then_manual() {
        let (db, dir) = db_with_tmpdir();
        let mut ids = vec![];
        for name in ["alpha", "beta", "gamma"] {
            let path = dir.path().join(name);
            std::fs::create_dir(&path).unwrap();
            ids.push(db.add_project(&path).unwrap().id);
        }
        db.set_pinned(ids[2], true).unwrap();
        let names: Vec<String> = db.projects().unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["gamma", "alpha", "beta"]);
    }

    #[test]
    fn move_project_reorders() {
        let (db, dir) = db_with_tmpdir();
        let mut ids = vec![];
        for name in ["a", "b", "c"] {
            let path = dir.path().join(name);
            std::fs::create_dir(&path).unwrap();
            ids.push(db.add_project(&path).unwrap().id);
        }
        db.move_project(ids[0], 2).unwrap();
        let names: Vec<String> = db.projects().unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["b", "c", "a"]);
        db.move_project(ids[0], -1).unwrap();
        let names: Vec<String> = db.projects().unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["b", "a", "c"]);
    }

    #[test]
    fn move_project_cannot_cross_the_pinned_block() {
        let (db, dir) = db_with_tmpdir();
        let mut ids = vec![];
        for name in ["a", "b"] {
            let path = dir.path().join(name);
            std::fs::create_dir(&path).unwrap();
            ids.push(db.add_project(&path).unwrap().id);
        }
        db.set_pinned(ids[1], true).unwrap();
        db.move_project(ids[0], -1).unwrap();
        let names: Vec<String> = db.projects().unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["b", "a"], "unpinned must not jump above pinned");
    }

    #[test]
    fn remove_project_keeps_the_directory() {
        let (db, dir) = db_with_tmpdir();
        let project = db.add_project(dir.path()).unwrap();
        assert!(db.remove_project(project.id).unwrap());
        assert!(dir.path().is_dir(), "removing must never delete files");
        assert!(db.projects().unwrap().is_empty());
        assert!(!db.remove_project(project.id).unwrap());
    }

    #[test]
    fn removing_a_project_removes_its_tabs() {
        let (db, dir) = db_with_tmpdir();
        let project = db.add_project(dir.path()).unwrap();
        db.set_tabs(
            project.id,
            &[crate::db::Tab::new(crate::db::Slot::Editor, "nvim")],
        )
        .unwrap();
        assert_eq!(db.tabs(project.id).unwrap().len(), 1);
        db.remove_project(project.id).unwrap();
        let orphans: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM tabs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(orphans, 0, "tabs must cascade");
    }

    #[test]
    fn touch_project_tracks_recency() {
        let (db, dir) = db_with_tmpdir();
        let project = db.add_project(dir.path()).unwrap();
        assert!(db.recent_projects(5).unwrap().is_empty());
        db.touch_project(project.id).unwrap();
        db.touch_project(project.id).unwrap();
        let recent = db.recent_projects(5).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].open_count, 2);
        assert!(recent[0].last_opened_at.is_some());
    }

    #[test]
    fn prune_removes_only_vanished_directories() {
        let (db, dir) = db_with_tmpdir();
        let keep = dir.path().join("keep");
        let gone = dir.path().join("gone");
        std::fs::create_dir(&keep).unwrap();
        std::fs::create_dir(&gone).unwrap();
        let gone_canonical = std::fs::canonicalize(&gone).unwrap();
        db.add_project(&keep).unwrap();
        db.add_project(&gone).unwrap();
        std::fs::remove_dir(&gone).unwrap();
        let pruned = db.prune_missing().unwrap();
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].path, gone_canonical);
        assert_eq!(db.projects().unwrap().len(), 1);
    }

    #[test]
    fn rename_falls_back_to_the_directory_name() {
        let (db, dir) = db_with_tmpdir();
        let project = db.add_project(dir.path()).unwrap();
        let base = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        db.rename_project(project.id, "  ").unwrap();
        assert_eq!(db.project(project.id).unwrap().unwrap().name, base);
        db.rename_project(project.id, "My API").unwrap();
        assert_eq!(db.project(project.id).unwrap().unwrap().name, "My API");
    }

    #[test]
    fn subtitle_shows_missing_directories() {
        let (db, dir) = db_with_tmpdir();
        let gone = dir.path().join("gone");
        std::fs::create_dir(&gone).unwrap();
        let project = db.add_project(&gone).unwrap();
        std::fs::remove_dir(&gone).unwrap();
        assert!(project.is_missing());
        assert!(project.subtitle().contains("missing"));
    }
}
