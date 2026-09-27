//! SQLite-backed state: projects, their tabs, preferences and an event log.
//!
//! One database, owned by this binary. Every front-end (GUI, CLI, a future
//! shell widget) goes through the same `Db` type, so nothing can disagree
//! about what a project is.

mod agent_sessions;
mod events;
mod projects;
mod settings;
mod tabs;
mod workspace;

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;

pub use events::Event;
pub use projects::Project;
pub use settings::{NewWorkspaceLayout, Preferences, UiPrefs};
pub use tabs::{Slot, Tab, TabKey};
pub use workspace::{WorkspaceAxis, WorkspaceGroup, WorkspaceLayout, WorkspaceState};

use crate::config::Paths;

/// Current schema version; bump with a migration below when changing tables.
const SCHEMA_VERSION: i64 = 3;

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open (creating if needed) the database at `paths.database()`.
    pub fn open(paths: &Paths) -> Result<Db> {
        paths.ensure().context("creating radar directories")?;
        let conn = Connection::open(paths.database())
            .with_context(|| format!("opening {}", paths.database().display()))?;
        Db::from_conn(conn)
    }

    /// An in-memory database, for tests and for `--dry-run`.
    pub fn open_in_memory() -> Result<Db> {
        Db::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Db> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let mut db = Db { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&mut self) -> Result<()> {
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version >= SCHEMA_VERSION {
            return Ok(());
        }
        if version < 1 {
            self.conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS projects (
                    id             INTEGER PRIMARY KEY AUTOINCREMENT,
                    path           TEXT    NOT NULL UNIQUE,
                    name           TEXT    NOT NULL,
                    pinned         INTEGER NOT NULL DEFAULT 0,
                    sort_order     INTEGER NOT NULL DEFAULT 0,
                    added_at       INTEGER NOT NULL,
                    last_opened_at INTEGER,
                    open_count     INTEGER NOT NULL DEFAULT 0,
                    archived       INTEGER NOT NULL DEFAULT 0
                );
                CREATE INDEX IF NOT EXISTS projects_order
                    ON projects(pinned DESC, sort_order ASC, name ASC);

                CREATE TABLE IF NOT EXISTS tabs (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                    slot        TEXT    NOT NULL,
                    program_id  TEXT    NOT NULL,
                    title       TEXT,
                    sort_order  INTEGER NOT NULL DEFAULT 0,
                    extra_args  TEXT    NOT NULL DEFAULT '[]',
                    created_at  INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS tabs_project ON tabs(project_id, sort_order);

                CREATE TABLE IF NOT EXISTS settings (
                    key        TEXT PRIMARY KEY,
                    value      TEXT NOT NULL,
                    updated_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS events (
                    id         INTEGER PRIMARY KEY AUTOINCREMENT,
                    at         INTEGER NOT NULL,
                    project_id INTEGER REFERENCES projects(id) ON DELETE SET NULL,
                    kind       TEXT NOT NULL,
                    data       TEXT
                );
                CREATE INDEX IF NOT EXISTS events_recent ON events(at DESC);
                "#,
            )?;
        }
        if version < 2 {
            self.conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS workspace_state (
                    project_id INTEGER PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
                    state      TEXT    NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                "#,
            )?;
        }
        if version < 3 {
            self.conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS agent_sessions (
                    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                    claim      TEXT    NOT NULL,
                    program_id TEXT    NOT NULL,
                    session_id TEXT    NOT NULL,
                    updated_at INTEGER NOT NULL,
                    PRIMARY KEY (project_id, claim)
                );
                "#,
            )?;
        }
        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    /// Escape hatch for tests and one-off queries.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

/// Seconds since the epoch, saturating rather than panicking on a clock skew.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Turn whatever the user typed into the canonical identity of a directory.
///
/// Expands `~`, makes the path absolute, and resolves symlinks when the path
/// exists so the same directory cannot be added twice under two names.
pub fn normalize_path(input: impl AsRef<Path>) -> Result<PathBuf> {
    let input = input.as_ref();
    let expanded = expand_tilde(input);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()?.join(expanded)
    };
    Ok(std::fs::canonicalize(&absolute).unwrap_or_else(|_| lexical_clean(&absolute)))
}

fn expand_tilde(path: &Path) -> PathBuf {
    let Some(home) = dirs::home_dir() else {
        return path.to_path_buf();
    };
    match path.to_str() {
        Some("~") => home,
        Some(rest) => match rest.strip_prefix("~/") {
            Some(tail) => home.join(tail),
            None => path.to_path_buf(),
        },
        None => path.to_path_buf(),
    }
}

/// Resolve `.` and `..` without touching the filesystem.
fn lexical_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Display helper: `~` for the home directory.
pub fn abbreviate(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            if rest.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_expands_tilde_and_cleans_dots() {
        let home = dirs::home_dir().unwrap();
        let normalized = normalize_path("~/some/../some/place").unwrap();
        assert_eq!(normalized, home.join("some/place"));
    }

    #[test]
    fn normalize_resolves_relative_paths() {
        let cwd = std::env::current_dir().unwrap();
        let normalized = normalize_path(".").unwrap();
        assert_eq!(normalized, std::fs::canonicalize(&cwd).unwrap_or(cwd));
    }

    #[test]
    fn normalize_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let once = normalize_path(dir.path()).unwrap();
        let twice = normalize_path(&once).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn abbreviate_shortens_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(abbreviate(&home), "~");
        assert_eq!(abbreviate(&home.join("Work/x")), "~/Work/x");
    }

    #[test]
    fn migration_is_idempotent() {
        let mut db = Db::open_in_memory().unwrap();
        db.migrate().unwrap();
        db.migrate().unwrap();
        let version: i64 = db
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }
}
