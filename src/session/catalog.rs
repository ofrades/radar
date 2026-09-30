//! Durable per-project session catalog, owned by the session daemon.
//!
//! The registry owns live PTYs and forgets them; this catalog is the history
//! that survives. One row per (project, provider, provider session): rows are
//! created when radar spawns a session, imported from a provider's own
//! session store, reconciled against live daemon state, and archived by the
//! user. Archiving is presentation only — nothing here deletes a
//! conversation or another program's data.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::registry::Status;

/// Which slice of the catalog a list should return.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogFilter {
    /// Never archived.
    Active,
    /// Archived only.
    Archived,
    All,
}

/// One durable session record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: i64,
    pub project_id: i64,
    /// The program that owns the conversation (opencode, claude, …).
    pub provider: String,
    pub provider_session_id: String,
    /// The radar daemon session currently or last attached to this row.
    pub radar_session_id: Option<String>,
    /// Stable todo association, independent of mutable board claims.
    #[serde(default)]
    pub card_id: Option<String>,
    /// `radar` when radar spawned it, `provider` when imported from history.
    pub source: String,
    pub title: Option<String>,
    pub cwd: PathBuf,
    pub created_at: i64,
    pub last_activity_at: i64,
    pub ended_at: Option<i64>,
    /// `running`, `ended`, or `failed`.
    pub lifecycle: String,
    pub archived_at: Option<i64>,
}

/// A provider conversation's activity is stale after this interval. Stale
/// ended/failed rows are archived automatically on the next catalog refresh;
/// live rows remain visible until the daemon confirms that they ended.
pub const ARCHIVE_AFTER_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// A conversation as a provider's own session store reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    pub id: String,
    pub title: Option<String>,
    /// Milliseconds since the epoch.
    pub created_ms: i64,
    /// Provider's last update stamp, or its creation stamp when unavailable.
    pub last_activity_ms: i64,
}

pub struct SessionCatalog {
    /// rusqlite connections are `Send` but not `Sync`; daemon worker threads
    /// share one catalog behind a parking-lot mutex.
    conn: Mutex<rusqlite::Connection>,
}

/// A provider creation stamp younger than this is treated as `ms`, older as
/// seconds. Providers have mixed conventions; 10^12 ms is 2001-09-09.
fn normalize_created_ms(value: i64) -> i64 {
    if value >= 1_000_000_000_000 {
        value
    } else if value > 0 {
        value.saturating_mul(1000)
    } else {
        0
    }
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}
pub(crate) fn is_generic_session_title(
    title: &str,
    program: Option<&crate::programs::Program>,
) -> bool {
    let title = title.trim();
    title.is_empty()
        || [
            "terminal",
            "foot",
            "ghostty",
            "kitty",
            "alacritty",
            "wezterm",
            "xterm",
            "radar",
        ]
        .iter()
        .any(|generic| title.eq_ignore_ascii_case(generic))
        || program.is_some_and(|program| {
            title.eq_ignore_ascii_case(&program.name)
                || title.eq_ignore_ascii_case(&program.command)
        })
}

/// Bump when the table below changes; add the next step to `SCHEMA_STEPS`.
const SCHEMA_VERSION: i64 = 2;
const SCHEMA_STEPS: [&str; 2] = [
    r#"
    CREATE TABLE IF NOT EXISTS sessions (
        id                 INTEGER PRIMARY KEY,
        project_id         INTEGER NOT NULL,
        provider           TEXT NOT NULL,
        provider_session_id TEXT NOT NULL,
        radar_session_id   TEXT,
        source             TEXT NOT NULL,
        title              TEXT,
        cwd                TEXT NOT NULL,
        created_at         INTEGER NOT NULL,
        last_activity_at   INTEGER NOT NULL,
        ended_at           INTEGER,
        lifecycle          TEXT NOT NULL,
        archived_at        INTEGER,
        UNIQUE(project_id, provider, provider_session_id)
    );
    CREATE INDEX IF NOT EXISTS sessions_activity
        ON sessions(project_id, last_activity_at DESC);
"#,
    "ALTER TABLE sessions ADD COLUMN card_id TEXT;",
];

impl SessionCatalog {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = rusqlite::Connection::open(path)
            .with_context(|| format!("opening session catalog {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        Self::initialize(conn)
    }

    /// A catalog without a file: daemon protocol tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn =
            rusqlite::Connection::open_in_memory().context("opening in-memory session catalog")?;
        Self::initialize(conn)
    }

    fn initialize(conn: rusqlite::Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        super::schema::migrate(&conn, SCHEMA_VERSION, &SCHEMA_STEPS)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Record a session radar spawned. Called from the daemon's Create path;
    /// idempotent, and a reuse of an ended id (the registry allows re-creating
    /// an ended session) runs the conversation again.
    pub fn record_radar(
        &self,
        project_id: i64,
        radar_id: &str,
        program: &str,
        cwd: &Path,
        now_ms: i64,
    ) -> Result<()> {
        self.record_radar_with_card(project_id, radar_id, program, cwd, now_ms, None)
    }

    pub fn record_radar_with_card(
        &self,
        project_id: i64,
        radar_id: &str,
        program: &str,
        cwd: &Path,
        now_ms: i64,
        card_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO sessions (project_id, provider, provider_session_id, radar_session_id,
                                   source, title, cwd, created_at, last_activity_at,
                                    ended_at, lifecycle, archived_at, card_id)
             VALUES (?1, ?2, ?3, ?3, 'radar', NULL, ?4, ?5, ?5, NULL, 'running', NULL, ?6)
             ON CONFLICT(project_id, provider, provider_session_id) DO UPDATE SET
                 radar_session_id = excluded.radar_session_id,
                 lifecycle = 'running',
                 ended_at = NULL,
                  last_activity_at = excluded.last_activity_at,
                  card_id = excluded.card_id",
            params![
                project_id,
                program,
                radar_id,
                cwd.to_string_lossy(),
                now_ms,
                card_id
            ],
        )?;
        Ok(())
    }

    /// Pull catalog rows toward live daemon truth: titles follow the
    /// terminal's title (a title change is activity), and a running row whose
    /// session is gone — stopped, forgotten, or lost to a daemon restart —
    /// ends. Registry-retained ended/failed lifecycles are copied over.
    pub fn reconcile(&self, live: &[Status], now_ms: i64) -> Result<()> {
        let conn = self.conn.lock();
        let live_by_id: std::collections::HashMap<&str, &Status> = live
            .iter()
            .map(|status| (status.id.as_str(), status))
            .collect();
        let mut programs = None;
        let stale = conn
            .prepare(
                "SELECT id, radar_session_id, title, lifecycle, provider FROM sessions
                 WHERE radar_session_id IS NOT NULL",
            )?
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (id, radar_id, title, lifecycle, provider) in stale {
            let Some(status) = live_by_id.get(radar_id.as_str()) else {
                if lifecycle == "running" {
                    conn.execute(
                        "UPDATE sessions SET lifecycle = 'ended', ended_at = ?2 WHERE id = ?1",
                        params![id, now_ms],
                    )?;
                }
                continue;
            };
            if let Some(fresh_title) = status
                .title
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
            {
                if Some(fresh_title) != title.as_deref() {
                    let programs = programs.get_or_insert_with(crate::programs::registry);
                    let program = programs.iter().find(|program| program.id == provider);
                    let replaces_real_title = is_generic_session_title(fresh_title, program)
                        && title
                            .as_deref()
                            .is_some_and(|current| !is_generic_session_title(current, program));
                    if !replaces_real_title {
                        conn.execute(
                            "UPDATE sessions SET title = ?2, last_activity_at = ?3 WHERE id = ?1",
                            params![id, fresh_title, now_ms],
                        )?;
                    }
                }
            }
            let record = match &status.lifecycle {
                super::registry::Lifecycle::Running => {
                    if lifecycle != "running" {
                        Some(("running".to_string(), Option::<i64>::None))
                    } else {
                        None
                    }
                }
                super::registry::Lifecycle::Exited(_) => {
                    if lifecycle != "ended" {
                        Some(("ended".to_string(), Some(now_ms)))
                    } else {
                        None
                    }
                }
                super::registry::Lifecycle::Failed(_) => {
                    if lifecycle != "failed" {
                        Some(("failed".to_string(), Some(now_ms)))
                    } else {
                        None
                    }
                }
            };
            if let Some((next, ended_at)) = record {
                conn.execute(
                    "UPDATE sessions SET lifecycle = ?2, ended_at = ?3 WHERE id = ?1",
                    params![id, next, ended_at],
                )?;
            }
        }
        Ok(())
    }

    /// Adopt a provider conversation for a radar-spawned row: the row keeps
    /// its identity but gains the provider's durable id. An imported row for
    /// the same conversation absorbs the live link instead of duplicating it.
    pub fn bind_provider(
        &self,
        radar_id: &str,
        provider: &str,
        provider_session_id: &str,
        now_ms: i64,
    ) -> Result<()> {
        let mut connection = self.conn.lock();
        let conn = connection.transaction()?;
        let Some((id, project_id, title, lifecycle, card_id)) = conn
            .query_row(
                "SELECT id, project_id, title, lifecycle, card_id FROM sessions
                  WHERE radar_session_id = ?1 AND source = 'radar'
                  ORDER BY (provider_session_id = radar_session_id) DESC, created_at DESC LIMIT 1",
                params![radar_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(());
        };
        let merged = conn
            .execute(
                "UPDATE sessions SET radar_session_id = ?3, last_activity_at = MAX(last_activity_at, ?4),
                                     card_id = COALESCE(?6, card_id)
                  WHERE project_id = ?1 AND provider = ?2 AND provider_session_id = ?5
                    AND id != ?7",
                params![project_id, provider, radar_id, now_ms, provider_session_id, card_id, id],
            )
            ? > 0;
        if merged {
            conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        } else {
            conn.execute(
                "UPDATE sessions SET provider = ?2, provider_session_id = ?3,
                                       title = COALESCE(title, ?4)
                 WHERE radar_session_id = ?1",
                params![radar_id, provider, provider_session_id, title],
            )?;
        }
        let _ = lifecycle;
        conn.commit()?;
        Ok(())
    }

    /// Upsert one provider-history page. A conversation that matches a young
    /// running radar row (the agent registered its session at startup) binds
    /// to that row instead of inserting a duplicate.
    pub fn import_provider(
        &self,
        project_id: i64,
        provider: &str,
        cwd: &Path,
        sessions: &[Imported],
        now_ms: i64,
    ) -> Result<usize> {
        let cwd_text = cwd.to_string_lossy().into_owned();
        let conn = self.conn.lock();
        let mut imported = 0;
        let mut program = None;
        let mut program_loaded = false;
        for session in sessions {
            let created = normalize_created_ms(session.created_ms);
            let activity = normalize_created_ms(session.last_activity_ms);
            // Provider history fills a generic terminal title, but leaves meaningful live
            // titles authoritative.
            let bound = conn
                .query_row(
                    "UPDATE sessions SET provider_session_id = ?4,
                                         last_activity_at = MAX(last_activity_at, ?5),
                                         cwd = CASE WHEN cwd = '' THEN ?6 ELSE cwd END
                     WHERE project_id = ?1 AND provider = ?2 AND source = 'radar'
                       AND lifecycle = 'running'
                       AND (provider_session_id = radar_session_id OR provider_session_id = ?4)
                       AND ABS(created_at - ?3) < 15000
                     RETURNING id, title",
                    params![project_id, provider, created, session.id, activity, cwd_text],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
                )
                .optional()?;
            if let Some((id, existing_title)) = bound {
                let replace_title = session.title.is_some()
                    && match existing_title.as_deref() {
                        None => true,
                        Some(title) if is_generic_session_title(title, None) => true,
                        Some(title) => {
                            if !program_loaded {
                                program = crate::programs::by_id(provider);
                                program_loaded = true;
                            }
                            is_generic_session_title(title, program.as_ref())
                        }
                    };
                if replace_title {
                    conn.execute(
                        "UPDATE sessions SET title = ?2 WHERE id = ?1",
                        params![id, session.title],
                    )?;
                }
                imported += 1;
                continue;
            }
            let changed = conn.execute(
                "INSERT INTO sessions (project_id, provider, provider_session_id, radar_session_id,
                                       source, title, cwd, created_at, last_activity_at,
                                       ended_at, lifecycle, archived_at)
                 VALUES (?1, ?2, ?3, NULL, 'provider', ?4, ?7, ?5, ?6, NULL, 'ended', NULL)
                 ON CONFLICT(project_id, provider, provider_session_id) DO UPDATE SET
                     title = COALESCE(excluded.title, sessions.title),
                     last_activity_at = MAX(sessions.last_activity_at, excluded.last_activity_at),
                     cwd = CASE WHEN sessions.cwd = '' THEN excluded.cwd ELSE sessions.cwd END",
                params![
                    project_id,
                    provider,
                    session.id,
                    session.title,
                    created,
                    activity,
                    cwd_text
                ],
            )?;
            imported += changed;
        }
        Self::archive_stale_locked(&conn, now_ms)?;
        Ok(imported)
    }

    /// Archive inactive history without touching live sessions.
    pub fn archive_stale(&self, now_ms: i64) -> Result<usize> {
        let conn = self.conn.lock();
        Self::archive_stale_locked(&conn, now_ms)
    }

    fn archive_stale_locked(conn: &rusqlite::Connection, now_ms: i64) -> Result<usize> {
        let cutoff = now_ms.saturating_sub(ARCHIVE_AFTER_MS);
        Ok(conn.execute(
            "UPDATE sessions SET archived_at = ?1
             WHERE archived_at IS NULL
               AND lifecycle != 'running'
               AND last_activity_at < ?2",
            params![now_ms, cutoff],
        )?)
    }

    pub fn list(
        &self,
        project_ids: &[i64],
        filter: CatalogFilter,
        query: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Entry>> {
        let mut sql = format!(
            "SELECT id, project_id, provider, provider_session_id, radar_session_id, source,
                     title, cwd, created_at, last_activity_at, ended_at, lifecycle, archived_at, card_id
             FROM sessions WHERE project_id IN ({})",
            vec!["?"; project_ids.len().max(1)].join(",")
        );
        match filter {
            CatalogFilter::Active => sql.push_str(" AND archived_at IS NULL"),
            CatalogFilter::Archived => sql.push_str(" AND archived_at IS NOT NULL"),
            CatalogFilter::All => {}
        }
        if query.is_some() {
            sql.push_str(
                " AND (title LIKE '%' || ? || '%' OR provider LIKE '%' || ? || '%'
                      OR provider_session_id LIKE '%' || ? || '%')",
            );
        }
        sql.push_str(" ORDER BY last_activity_at DESC, created_at DESC LIMIT ?");

        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let mut parameters: Vec<Box<dyn rusqlite::types::ToSql>> = project_ids
            .iter()
            .map(|id| Box::new(*id) as Box<dyn rusqlite::types::ToSql>)
            .collect();
        if project_ids.is_empty() {
            parameters.push(Box::new(-1_i64));
        }
        if let Some(query) = query {
            let like = query.trim();
            parameters.push(Box::new(like.to_string()));
            parameters.push(Box::new(like.to_string()));
            parameters.push(Box::new(like.to_string()));
        }
        parameters.push(Box::new(limit));

        let rows = stmt
            .query_map(
                parameters
                    .iter()
                    .map(|parameter| parameter.as_ref())
                    .collect::<Vec<_>>()
                    .as_slice(),
                |row| {
                    Ok(Entry {
                        id: row.get(0)?,
                        project_id: row.get(1)?,
                        provider: row.get(2)?,
                        provider_session_id: row.get(3)?,
                        radar_session_id: row.get(4)?,
                        source: row.get(5)?,
                        title: row.get(6)?,
                        cwd: PathBuf::from(row.get::<_, String>(7)?),
                        created_at: row.get(8)?,
                        last_activity_at: row.get(9)?,
                        ended_at: row.get(10)?,
                        lifecycle: row.get(11)?,
                        archived_at: row.get(12)?,
                        card_id: row.get(13)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn archive(&self, id: i64, archived: bool, now_ms: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE sessions SET archived_at = ?2 WHERE id = ?1",
            params![id, if archived { Some(now_ms) } else { None }],
        )?;
        Ok(())
    }
}

/// Parse the project id out of a stable radar session id
/// (`project-17-agent-2-opencode` / `web-project-17-shell-…`).
pub fn project_of_session_id(session_id: &str) -> Option<i64> {
    let remainder = session_id
        .strip_prefix("project-")
        .or_else(|| session_id.strip_prefix("web-project-"))?;
    let project_id = remainder.split('-').next()?.parse().ok()?;
    (project_id > 0).then_some(project_id)
}

/// The program that owns a stable session id, for catalog row identity.
pub fn provider_of_session_id(session_id: &str) -> Option<String> {
    if session_id.starts_with("web-project-") {
        return Some("shell".to_string());
    }
    session_id.splitn(5, '-').nth(4).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_binding_survives_exit_restart_import_merge_and_resume() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        let radar_id = "project-3-agent-0-opencode";
        let now = 1_800_000_000_000;
        {
            let catalog = SessionCatalog::open(&path).unwrap();
            catalog
                .import_provider(
                    3,
                    "opencode",
                    Path::new("/w"),
                    &[Imported {
                        id: "ses-exact".into(),
                        title: Some("Work".into()),
                        created_ms: now,
                        last_activity_ms: now,
                    }],
                    now,
                )
                .unwrap();
            catalog
                .record_radar_with_card(
                    3,
                    radar_id,
                    "opencode",
                    Path::new("/w"),
                    now,
                    Some("todo-1"),
                )
                .unwrap();
            catalog
                .bind_provider(radar_id, "opencode", "ses-exact", now)
                .unwrap();
            catalog.reconcile(&[], now + 1).unwrap();
        }
        let catalog = SessionCatalog::open(&path).unwrap();
        let rows = catalog.list(&[3], CatalogFilter::All, None, 100).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].card_id.as_deref(), Some("todo-1"));
        assert_eq!(rows[0].provider_session_id, "ses-exact");
        assert_eq!(rows[0].lifecycle, "ended");
        // Reopen that exact conversation on a new pane, preserving one row.
        let resumed = "project-3-agent-2-opencode";
        catalog
            .record_radar_with_card(
                3,
                resumed,
                "opencode",
                Path::new("/w"),
                now + 2,
                Some("todo-1"),
            )
            .unwrap();
        catalog
            .bind_provider(resumed, "opencode", "ses-exact", now + 2)
            .unwrap();
        let rows = catalog.list(&[3], CatalogFilter::All, None, 100).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].card_id.as_deref(), Some("todo-1"));
        assert_eq!(rows[0].radar_session_id.as_deref(), Some(resumed));
    }

    #[test]
    fn v1_catalog_migrates_without_inventing_todo_links() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        super::super::schema::migrate(&conn, 1, &SCHEMA_STEPS[..1]).unwrap();
        conn.execute("INSERT INTO sessions (project_id, provider, provider_session_id, source, cwd,
            created_at, last_activity_at, lifecycle) VALUES (3, 'opencode', 'ses-old', 'provider', '/w', 1, 1, 'ended')", []).unwrap();
        drop(conn);
        let catalog = SessionCatalog::open(&path).unwrap();
        let rows = catalog.list(&[3], CatalogFilter::All, None, 100).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider_session_id, "ses-old");
        assert_eq!(rows[0].card_id, None);
    }

    #[test]
    fn a_reused_pane_without_a_todo_does_not_inherit_the_old_binding() {
        let catalog = SessionCatalog::open_in_memory().unwrap();
        let id = "project-3-agent-0-opencode";
        catalog
            .record_radar_with_card(3, id, "opencode", Path::new("/w"), 1, Some("todo-old"))
            .unwrap();
        catalog.reconcile(&[], 2).unwrap();
        catalog
            .record_radar_with_card(3, id, "opencode", Path::new("/w"), 3, None)
            .unwrap();
        let rows = catalog.list(&[3], CatalogFilter::All, None, 100).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].card_id, None);
    }
    use crate::session::registry::Lifecycle;

    #[test]
    fn the_schema_is_versioned() {
        let catalog = SessionCatalog::open_in_memory().unwrap();
        let conn = catalog.conn.lock();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    fn status(id: &str, lifecycle: Lifecycle, title: Option<&str>) -> Status {
        Status {
            id: id.to_string(),
            cwd: PathBuf::from("/tmp"),
            pid: Some(1),
            lifecycle,
            title: title.map(str::to_string),
            stream_closed: false,
        }
    }

    fn catalog() -> SessionCatalog {
        let path = std::env::temp_dir().join(format!(
            "radar-catalog-test-{}-{}.sqlite",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        SessionCatalog::open(&path).unwrap()
    }

    #[test]
    fn created_stamps_normalize_to_milliseconds() {
        assert_eq!(normalize_created_ms(1_700_000_000_123), 1_700_000_000_123);
        assert_eq!(normalize_created_ms(1_700_000_000), 1_700_000_000_000);
        assert_eq!(normalize_created_ms(0), 0);
        assert_eq!(normalize_created_ms(-5), 0);
    }

    #[test]
    fn radar_rows_record_reconcile_and_end() {
        let catalog = catalog();
        catalog
            .record_radar(
                3,
                "project-3-agent-0-opencode",
                "opencode",
                Path::new("/w"),
                1000,
            )
            .unwrap();

        // Live and running: reconciling without the session ends it.
        catalog.reconcile(&[], 2000).unwrap();
        let rows = catalog.list(&[3], CatalogFilter::Active, None, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].lifecycle, "ended");
        assert_eq!(rows[0].ended_at, Some(2000));

        // A re-create runs the conversation again.
        catalog
            .record_radar(
                3,
                "project-3-agent-0-opencode",
                "opencode",
                Path::new("/w"),
                3000,
            )
            .unwrap();
        catalog
            .reconcile(
                &[status(
                    "project-3-agent-0-opencode",
                    Lifecycle::Running,
                    Some("Planning"),
                )],
                3100,
            )
            .unwrap();
        let rows = catalog.list(&[3], CatalogFilter::Active, None, 10).unwrap();
        assert_eq!(rows[0].lifecycle, "running");
        assert_eq!(rows[0].title.as_deref(), Some("Planning"));
        assert_eq!(rows[0].last_activity_at, 3100);

        // An exited registry lifecycle is copied over.
        catalog
            .reconcile(
                &[status(
                    "project-3-agent-0-opencode",
                    Lifecycle::Exited(crate::session::ExitInfo {
                        code: 0,
                        signal: None,
                    }),
                    None,
                )],
                3200,
            )
            .unwrap();
        let rows = catalog.list(&[3], CatalogFilter::Active, None, 10).unwrap();
        assert_eq!(rows[0].lifecycle, "ended");
        assert_eq!(rows[0].ended_at, Some(3200));
    }
    #[test]
    fn reconcile_keeps_conversation_title_over_generic_live_title() {
        let catalog = catalog();
        let radar_id = "project-3-agent-0-opencode";
        let created_at = 1_700_000_050_000;
        catalog
            .record_radar(3, radar_id, "opencode", Path::new("/w"), created_at)
            .unwrap();
        catalog
            .import_provider(
                3,
                "opencode",
                Path::new("/w"),
                &[Imported {
                    id: "ses_42".into(),
                    title: Some("Earlier work".into()),
                    created_ms: created_at + 2_000,
                    last_activity_ms: created_at + 2_000,
                }],
                created_at + 2_000,
            )
            .unwrap();

        catalog
            .reconcile(
                &[status(radar_id, Lifecycle::Running, Some("OpenCode"))],
                created_at + 3_000,
            )
            .unwrap();
        let rows = catalog.list(&[3], CatalogFilter::Active, None, 10).unwrap();
        assert_eq!(rows[0].title.as_deref(), Some("Earlier work"));

        catalog
            .reconcile(
                &[status(radar_id, Lifecycle::Running, None)],
                created_at + 4_000,
            )
            .unwrap();
        let rows = catalog.list(&[3], CatalogFilter::Active, None, 10).unwrap();
        assert_eq!(rows[0].title.as_deref(), Some("Earlier work"));

        catalog
            .reconcile(
                &[status(radar_id, Lifecycle::Running, Some("New task"))],
                created_at + 5_000,
            )
            .unwrap();
        let rows = catalog.list(&[3], CatalogFilter::Active, None, 10).unwrap();
        assert_eq!(rows[0].title.as_deref(), Some("New task"));
    }
    #[test]
    fn provider_import_replaces_generic_but_preserves_real_live_title() {
        let catalog = catalog();
        let radar_id = "project-3-agent-0-opencode";
        let created_at = 1_700_000_050_000;
        catalog
            .record_radar(3, radar_id, "opencode", Path::new("/w"), created_at)
            .unwrap();
        catalog
            .reconcile(
                &[status(radar_id, Lifecycle::Running, Some("OpenCode"))],
                created_at + 1_000,
            )
            .unwrap();
        catalog
            .import_provider(
                3,
                "opencode",
                Path::new("/w"),
                &[Imported {
                    id: "ses_42".into(),
                    title: Some("Earlier work".into()),
                    created_ms: created_at + 2_000,
                    last_activity_ms: created_at + 2_000,
                }],
                created_at + 2_000,
            )
            .unwrap();

        let rows = catalog.list(&[3], CatalogFilter::Active, None, 10).unwrap();
        assert_eq!(rows[0].title.as_deref(), Some("Earlier work"));
        catalog
            .reconcile(
                &[status(radar_id, Lifecycle::Running, Some("New task"))],
                created_at + 3_000,
            )
            .unwrap();
        catalog
            .import_provider(
                3,
                "opencode",
                Path::new("/w"),
                &[Imported {
                    id: "ses_42".into(),
                    title: Some("Earlier work".into()),
                    created_ms: created_at + 2_000,
                    last_activity_ms: created_at + 3_000,
                }],
                created_at + 3_000,
            )
            .unwrap();
        let rows = catalog.list(&[3], CatalogFilter::Active, None, 10).unwrap();
        assert_eq!(rows[0].title.as_deref(), Some("New task"));
    }

    #[test]
    fn importing_binds_a_young_running_radar_row_instead_of_duplicating() {
        let catalog = catalog();
        let radar_id = "project-3-agent-0-opencode";
        catalog
            .record_radar(3, radar_id, "opencode", Path::new("/w"), 1_700_000_050_000)
            .unwrap();
        catalog
            .import_provider(
                3,
                "opencode",
                Path::new("/w"),
                &[Imported {
                    id: "ses_new".into(),
                    title: Some("Fresh conversation".into()),
                    created_ms: 1_700_000_052_000,
                    last_activity_ms: 1_700_000_052_000,
                }],
                1_700_000_052_000,
            )
            .unwrap();
        let rows = catalog.list(&[3], CatalogFilter::Active, None, 10).unwrap();
        assert_eq!(rows.len(), 1, "the running row adopts the provider id");
        assert_eq!(rows[0].provider_session_id, "ses_new");
        assert_eq!(rows[0].radar_session_id.as_deref(), Some(radar_id));
        assert_eq!(rows[0].lifecycle, "running");

        // An older conversation imports as plain history.
        catalog
            .import_provider(
                3,
                "opencode",
                Path::new("/w"),
                &[Imported {
                    id: "ses_old".into(),
                    title: None,
                    created_ms: 1_690_000_000_000,
                    last_activity_ms: 1_690_000_000_000,
                }],
                1_700_000_052_000,
            )
            .unwrap();
        let rows = catalog.list(&[3], CatalogFilter::All, None, 10).unwrap();
        assert_eq!(rows.len(), 2);
        let old = rows
            .iter()
            .find(|r| r.provider_session_id == "ses_old")
            .unwrap();
        assert_eq!(old.source, "provider");
        assert_eq!(old.lifecycle, "ended");
        assert_eq!(old.created_at, 1_690_000_000_000);
        assert!(old.archived_at.is_some(), "stale history is archived");
    }

    #[test]
    fn importing_never_overwrites_an_exact_bind() {
        let catalog = catalog();
        let radar_id = "project-3-agent-0-opencode";
        catalog
            .record_radar(3, radar_id, "opencode", Path::new("/w"), 1_700_000_050_000)
            .unwrap();
        // Radar named the conversation at launch: the row already has a real
        // provider id, not the placeholder.
        catalog
            .bind_provider(radar_id, "opencode", "ses_exact", 1_700_000_050_100)
            .unwrap();
        // A different session created in the same window must not steal it.
        catalog
            .import_provider(
                3,
                "opencode",
                Path::new("/w"),
                &[Imported {
                    id: "ses_other".into(),
                    title: None,
                    created_ms: 1_700_000_052_000,
                    last_activity_ms: 1_700_000_052_000,
                }],
                1_700_000_052_000,
            )
            .unwrap();
        let rows = catalog.list(&[3], CatalogFilter::All, None, 10).unwrap();
        let live = rows
            .iter()
            .find(|row| row.radar_session_id.as_deref() == Some(radar_id))
            .unwrap();
        assert_eq!(live.provider_session_id, "ses_exact");
        assert!(rows
            .iter()
            .any(|row| row.provider_session_id == "ses_other" && row.source == "provider"));
    }

    #[test]
    fn binding_to_an_imported_row_merges_the_live_link() {
        let catalog = catalog();
        catalog
            .import_provider(
                3,
                "opencode",
                Path::new("/w"),
                &[Imported {
                    id: "ses_hist".into(),
                    title: Some("Earlier work".into()),
                    created_ms: 5_000,
                    last_activity_ms: 5_000,
                }],
                5_000,
            )
            .unwrap();
        catalog
            .record_radar(
                3,
                "project-3-agent-0-opencode",
                "opencode",
                Path::new("/w"),
                50_000,
            )
            .unwrap();
        catalog
            .bind_provider("project-3-agent-0-opencode", "opencode", "ses_hist", 51_000)
            .unwrap();
        let rows = catalog.list(&[3], CatalogFilter::Active, None, 10).unwrap();
        assert_eq!(rows.len(), 1, "the provisional row is absorbed");
        assert_eq!(rows[0].provider_session_id, "ses_hist");
        assert_eq!(
            rows[0].radar_session_id.as_deref(),
            Some("project-3-agent-0-opencode")
        );
        assert_eq!(rows[0].title.as_deref(), Some("Earlier work"));
    }

    #[test]
    fn lists_order_by_activity_and_filter_archive_and_query() {
        let catalog = catalog();
        catalog
            .import_provider(
                3,
                "opencode",
                Path::new("/w"),
                &[
                    Imported {
                        id: "a".into(),
                        title: Some("Alpha".into()),
                        created_ms: 1_000,
                        last_activity_ms: 1_000,
                    },
                    Imported {
                        id: "b".into(),
                        title: Some("Beta".into()),
                        created_ms: 2_000,
                        last_activity_ms: 2_000,
                    },
                    Imported {
                        id: "c".into(),
                        title: Some("Gamma".into()),
                        created_ms: 3_000,
                        last_activity_ms: 3_000,
                    },
                ],
                3_000,
            )
            .unwrap();
        let rows = catalog.list(&[3], CatalogFilter::Active, None, 10).unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| r.provider_session_id.as_str())
                .collect::<Vec<_>>(),
            ["c", "b", "a"],
            "newest activity first"
        );

        catalog.archive(rows[0].id, true, 4_000).unwrap();
        assert!(
            catalog
                .list(&[3], CatalogFilter::Active, None, 10)
                .unwrap()
                .len()
                == 2
        );
        let archived = catalog
            .list(&[3], CatalogFilter::Archived, None, 10)
            .unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].provider_session_id, "c");
        catalog.archive(archived[0].id, false, 5_000).unwrap();
        assert_eq!(
            catalog
                .list(&[3], CatalogFilter::All, None, 10)
                .unwrap()
                .len(),
            3
        );

        let hit = catalog
            .list(&[3], CatalogFilter::All, Some("bet"), 10)
            .unwrap();
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].provider_session_id, "b");
        let by_provider = catalog
            .list(&[3], CatalogFilter::All, Some("opencode"), 10)
            .unwrap();
        assert_eq!(by_provider.len(), 3);
        let limited = catalog.list(&[3], CatalogFilter::All, None, 2).unwrap();
        assert_eq!(limited.len(), 2);
    }

    #[test]
    fn stale_history_archives_on_refresh_but_running_rows_remain_visible() {
        let catalog = catalog();
        let now = 2_000_000_000_000;
        let stale = now - ARCHIVE_AFTER_MS - 1;

        catalog
            .record_radar(
                3,
                "project-3-agent-0-opencode",
                "opencode",
                Path::new("/w"),
                stale,
            )
            .unwrap();
        catalog
            .reconcile(
                &[status(
                    "project-3-agent-0-opencode",
                    Lifecycle::Exited(crate::session::ExitInfo {
                        code: 0,
                        signal: None,
                    }),
                    None,
                )],
                stale + 1,
            )
            .unwrap();
        catalog.archive_stale(now).unwrap();

        catalog
            .record_radar(
                3,
                "project-3-agent-1-opencode",
                "opencode",
                Path::new("/w"),
                stale,
            )
            .unwrap();
        catalog.archive_stale(now).unwrap();

        let active = catalog.list(&[3], CatalogFilter::Active, None, 10).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(
            active[0].radar_session_id.as_deref(),
            Some("project-3-agent-1-opencode")
        );
        let archived = catalog
            .list(&[3], CatalogFilter::Archived, None, 10)
            .unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(
            archived[0].radar_session_id.as_deref(),
            Some("project-3-agent-0-opencode")
        );
    }

    #[test]
    fn recent_provider_activity_overrides_old_creation_and_boundary_is_inclusive() {
        let catalog = catalog();
        let now = 2_000_000_000_000;
        let exactly_at_cutoff = now - ARCHIVE_AFTER_MS;

        catalog
            .import_provider(
                3,
                "opencode",
                Path::new("/w"),
                &[
                    Imported {
                        id: "recent-update".into(),
                        title: Some("Recently updated".into()),
                        created_ms: exactly_at_cutoff - 90 * 24 * 60 * 60 * 1000,
                        last_activity_ms: now - 100,
                    },
                    Imported {
                        id: "at-cutoff".into(),
                        title: Some("At cutoff".into()),
                        created_ms: exactly_at_cutoff,
                        last_activity_ms: exactly_at_cutoff,
                    },
                ],
                now,
            )
            .unwrap();

        let active = catalog.list(&[3], CatalogFilter::Active, None, 10).unwrap();
        assert_eq!(active.len(), 2);
        assert!(active.iter().all(|entry| entry.archived_at.is_none()));
        assert_eq!(catalog.archive_stale(now).unwrap(), 0);
    }

    #[test]
    fn stable_session_ids_reveal_project_and_provider() {
        assert_eq!(project_of_session_id("project-17-agent-2-claude"), Some(17));
        assert_eq!(project_of_session_id("web-project-4-shell-99"), Some(4));
        assert_eq!(project_of_session_id("unknown"), None);
        assert_eq!(
            provider_of_session_id("project-17-agent-2-claude-code").as_deref(),
            Some("claude-code")
        );
        assert_eq!(
            provider_of_session_id("web-project-4-shell-99").as_deref(),
            Some("shell")
        );
    }
}
