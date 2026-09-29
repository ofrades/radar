//! The board store: a project's lanes and cards, owned by the daemon.
//!
//! Cards are records, not markdown lines. A lane carries a name and a kind
//! (`todo`, `in_progress`, `review`, `done`, or `custom`); a card carries a
//! stable id, its lane and position, a title, a markdown body, a claim, a done
//! flag, timestamps and a revision. A card is done exactly while it sits in a
//! done-kind lane — one definition, with no column-heading-versus-checkbox
//! ambiguity.
//!
//! The daemon is the only writer; clients read a snapshot and send mutations,
//! which are revision-checked so two agents cannot silently clobber one card.
//! The thread for a card is the project's activity journal filtered by the
//! card's id; it is not stored here.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use serde::{Deserialize, Serialize};

const MAX_TITLE: usize = 200;
const MAX_BODY: usize = 64 * 1024;
const MAX_CLAIM: usize = 200;

/// The four lanes a project starts with: (name, kind).
const DEFAULT_LANES: [(&str, &str); 4] = [
    ("Todo", "todo"),
    ("In progress", "in_progress"),
    ("Review", "review"),
    ("Done", "done"),
];

/// Bump when the tables below change; add the next step to `SCHEMA_STEPS`.
const SCHEMA_VERSION: i64 = 2;
const SCHEMA_STEPS: [&str; 2] = [
    r#"
    CREATE TABLE IF NOT EXISTS board_projects (
        project_id INTEGER PRIMARY KEY
    );
    CREATE TABLE IF NOT EXISTS board_lanes (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        project_id INTEGER NOT NULL,
        name TEXT NOT NULL,
        kind TEXT NOT NULL,
        position INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS board_lanes_project
        ON board_lanes(project_id, position);
    CREATE TABLE IF NOT EXISTS board_cards (
        id TEXT PRIMARY KEY,
        project_id INTEGER NOT NULL,
        lane_id INTEGER NOT NULL,
        position INTEGER NOT NULL,
        title TEXT NOT NULL,
        body TEXT NOT NULL DEFAULT '',
        claim TEXT,
        done INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        revision INTEGER NOT NULL DEFAULT 1
    );
    CREATE INDEX IF NOT EXISTS board_cards_project
        ON board_cards(project_id, lane_id, position);
"#,
    r#"
    -- Rename the first lane from Backlog to Todo. The kind moves too, so the
    -- lane reads as `todo` everywhere; only the label of a lane still called
    -- "Backlog" is rewritten, so a deliberately renamed lane keeps its name.
    UPDATE board_lanes SET kind = 'todo' WHERE kind = 'backlog';
    UPDATE board_lanes SET name = 'Todo' WHERE kind = 'todo' AND name = 'Backlog';
"#,
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lane {
    pub id: i64,
    pub name: String,
    /// `todo` | `in_progress` | `review` | `done` | `custom`.
    pub kind: String,
    pub position: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCard {
    pub id: String,
    pub project_id: i64,
    pub lane_id: i64,
    /// The lane's name, joined for the client's convenience.
    pub lane: String,
    pub done: bool,
    pub position: i64,
    pub title: String,
    pub body: String,
    pub claim: Option<String>,
    pub revision: u64,
    pub created_at_millis: i64,
    pub updated_at_millis: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardState {
    pub project_id: i64,
    pub lanes: Vec<Lane>,
    pub cards: Vec<StoredCard>,
}

/// What a mutation changed, so the daemon can publish a `BoardChanged` event
/// without re-reading the board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardChange {
    pub card: StoredCard,
    /// `added` | `updated` | `moved` | `claimed` | `released` | `completed`
    /// | `reopened` | `removed`.
    pub action: String,
    pub from_lane: Option<String>,
}

pub struct BoardStore {
    inner: Mutex<Connection>,
}

impl BoardStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("opening board store {}", path.display()))?;
        Self::from_connection(connection)
    }

    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self> {
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.busy_timeout(std::time::Duration::from_secs(2))?;
        super::schema::migrate(&connection, SCHEMA_VERSION, &SCHEMA_STEPS)?;
        Ok(Self {
            inner: Mutex::new(connection),
        })
    }

    /// Has this project's board been initialized in the store yet?
    pub fn is_initialized(&self, project_id: i64) -> Result<bool> {
        let inner = self.inner.lock();
        has_cards(&inner, project_id)
    }

    pub fn state(&self, project_id: i64) -> Result<BoardState> {
        let mut inner = self.inner.lock();
        let tx = inner.transaction()?;
        ensure_project(&tx, project_id)?;
        let state = read_state(&tx, project_id)?;
        tx.commit()?;
        Ok(state)
    }

    pub fn add_card(
        &self,
        project_id: i64,
        lane: Option<&str>,
        title: &str,
        body: &str,
        claim: Option<&str>,
    ) -> Result<BoardChange> {
        let title = validate_title(title)?;
        validate_body(body)?;
        if let Some(claim) = claim {
            validate_claim(claim)?;
        }
        let mut inner = self.inner.lock();
        let tx = inner.transaction()?;
        ensure_project(&tx, project_id)?;
        let lane = resolve_lane(&tx, project_id, lane)?;
        let position = next_position(&tx, project_id, lane.id)?;
        let now = now_millis();
        let id = new_card_id();
        tx.execute(
            "INSERT INTO board_cards
                (id, project_id, lane_id, position, title, body, claim, done,
                 created_at, updated_at, revision)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, 1)",
            params![
                id,
                project_id,
                lane.id,
                position,
                title,
                body,
                claim,
                lane.done(),
                now,
            ],
        )?;
        let card = read_card(&tx, &id)?.context("card vanished after insert")?;
        tx.commit()?;
        Ok(BoardChange {
            card,
            action: "added".to_string(),
            from_lane: None,
        })
    }

    pub fn update_card(
        &self,
        project_id: i64,
        card_id: &str,
        title: Option<&str>,
        body: Option<&str>,
        expected_revision: Option<u64>,
    ) -> Result<BoardChange> {
        if let Some(title) = title {
            validate_title(title)?;
        }
        if let Some(body) = body {
            validate_body(body)?;
        }
        let mut inner = self.inner.lock();
        let tx = inner.transaction()?;
        let card = require_card(&tx, project_id, card_id)?;
        check_revision(&card, expected_revision)?;
        let title = title.map(str::to_string).unwrap_or(card.title);
        let body = body.map(str::to_string).unwrap_or(card.body);
        bump(&tx, card_id, Some(&title), Some(&body), None, None)?;
        let card = read_card(&tx, card_id)?.context("card vanished after update")?;
        tx.commit()?;
        Ok(BoardChange {
            card,
            action: "updated".to_string(),
            from_lane: None,
        })
    }

    pub fn move_card(
        &self,
        project_id: i64,
        card_id: &str,
        lane: &str,
        expected_revision: Option<u64>,
    ) -> Result<BoardChange> {
        let mut inner = self.inner.lock();
        let tx = inner.transaction()?;
        ensure_project(&tx, project_id)?;
        let card = require_card(&tx, project_id, card_id)?;
        check_revision(&card, expected_revision)?;
        let target = resolve_lane(&tx, project_id, Some(lane))?;
        let from_lane = Some(card.lane.clone());
        if card.lane_id != target.id {
            let position = next_position(&tx, project_id, target.id)?;
            // Moving a card drops its claim: a card handed to another column is
            // by definition no longer the worker's.
            bump(
                &tx,
                card_id,
                None,
                None,
                Some((target.id, position, target.done())),
                Some(None),
            )?;
        }
        let card = read_card(&tx, card_id)?.context("card vanished after move")?;
        tx.commit()?;
        Ok(BoardChange {
            card,
            action: "moved".to_string(),
            from_lane,
        })
    }

    pub fn claim_card(
        &self,
        project_id: i64,
        card_id: &str,
        claim: Option<&str>,
        expected_revision: Option<u64>,
    ) -> Result<BoardChange> {
        if let Some(claim) = claim {
            validate_claim(claim)?;
        }
        let mut inner = self.inner.lock();
        let tx = inner.transaction()?;
        let card = require_card(&tx, project_id, card_id)?;
        check_revision(&card, expected_revision)?;
        bump(&tx, card_id, None, None, None, Some(claim))?;
        let card = read_card(&tx, card_id)?.context("card vanished after claim")?;
        tx.commit()?;
        Ok(BoardChange {
            card,
            action: if claim.is_some() {
                "claimed".to_string()
            } else {
                "released".to_string()
            },
            from_lane: None,
        })
    }

    /// Mark done: move to the done lane (and set the flag).
    pub fn complete_card(
        &self,
        project_id: i64,
        card_id: &str,
        expected_revision: Option<u64>,
    ) -> Result<BoardChange> {
        self.to_done(project_id, card_id, true, expected_revision)
    }

    /// Reopen: move back to the first non-done lane and clear the flag.
    pub fn reopen_card(
        &self,
        project_id: i64,
        card_id: &str,
        expected_revision: Option<u64>,
    ) -> Result<BoardChange> {
        self.to_done(project_id, card_id, false, expected_revision)
    }

    fn to_done(
        &self,
        project_id: i64,
        card_id: &str,
        done: bool,
        expected_revision: Option<u64>,
    ) -> Result<BoardChange> {
        let mut inner = self.inner.lock();
        let tx = inner.transaction()?;
        ensure_project(&tx, project_id)?;
        let card = require_card(&tx, project_id, card_id)?;
        check_revision(&card, expected_revision)?;
        let target = if done {
            resolve_lane(&tx, project_id, Some("Done"))?
        } else {
            first_open_lane(&tx, project_id)?
        };
        let from_lane = Some(card.lane.clone());
        if card.lane_id != target.id || card.done != done {
            let position = if card.lane_id == target.id {
                card.position
            } else {
                next_position(&tx, project_id, target.id)?
            };
            bump(
                &tx,
                card_id,
                None,
                None,
                Some((target.id, position, target.done())),
                None,
            )?;
        }
        let card = read_card(&tx, card_id)?.context("card vanished after done")?;
        tx.commit()?;
        Ok(BoardChange {
            card,
            action: if done {
                "completed".to_string()
            } else {
                "reopened".to_string()
            },
            from_lane,
        })
    }

    pub fn remove_card(&self, project_id: i64, card_id: &str) -> Result<BoardChange> {
        let mut inner = self.inner.lock();
        let tx = inner.transaction()?;
        let card = require_card(&tx, project_id, card_id)?;
        tx.execute(
            "DELETE FROM board_cards WHERE project_id = ?1 AND id = ?2",
            params![project_id, card_id],
        )?;
        tx.commit()?;
        Ok(BoardChange {
            card,
            action: "removed".to_string(),
            from_lane: None,
        })
    }

    /// Claim the first unclaimed, not-done card, optionally within one lane —
    /// the store's `card next`. Returns `None` when there is no work.
    pub fn next_card(
        &self,
        project_id: i64,
        who: &str,
        lane: Option<&str>,
    ) -> Result<Option<BoardChange>> {
        validate_claim(who)?;
        let mut inner = self.inner.lock();
        let tx = inner.transaction()?;
        ensure_project(&tx, project_id)?;
        let filter = match lane {
            Some(name) => Some(resolve_lane(&tx, project_id, Some(name))?.id),
            None => None,
        };
        let found: Option<String> = tx
            .query_row(
                "SELECT c.id FROM board_cards c
                 JOIN board_lanes l ON l.id = c.lane_id
                 WHERE c.project_id = ?1
                   AND c.done = 0 AND c.claim IS NULL
                   AND l.kind <> 'done'
                   AND (?2 IS NULL OR c.lane_id = ?2)
                   AND TRIM(c.title) <> ''
                 ORDER BY l.position, c.position
                 LIMIT 1",
                params![project_id, filter],
                |row| row.get(0),
            )
            .optional()?;
        let Some(card_id) = found else {
            tx.commit()?;
            return Ok(None);
        };
        bump(&tx, &card_id, None, None, None, Some(Some(who)))?;
        let card = read_card(&tx, &card_id)?.context("card vanished after next")?;
        tx.commit()?;
        Ok(Some(BoardChange {
            card,
            action: "claimed".to_string(),
            from_lane: None,
        }))
    }
}

impl Lane {
    fn done(&self) -> bool {
        self.kind == "done"
    }
}

fn has_cards(connection: &Connection, project_id: i64) -> Result<bool> {
    let cards: i64 = connection.query_row(
        "SELECT COUNT(*) FROM board_cards WHERE project_id = ?1",
        params![project_id],
        |row| row.get(0),
    )?;
    Ok(cards > 0)
}

fn read_state(tx: &Transaction<'_>, project_id: i64) -> Result<BoardState> {
    let mut lanes = Vec::new();
    {
        let mut statement = tx.prepare(
            "SELECT id, name, kind, position FROM board_lanes
             WHERE project_id = ?1 ORDER BY position, id",
        )?;
        let rows = statement.query_map(params![project_id], |row| {
            Ok(Lane {
                id: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
                position: row.get(3)?,
            })
        })?;
        for row in rows {
            lanes.push(row?);
        }
    }
    let mut cards = Vec::new();
    let mut statement = tx.prepare(
        "SELECT c.id, c.project_id, c.lane_id, l.name, c.done, c.position, c.title,
                c.body, c.claim, c.revision, c.created_at, c.updated_at
         FROM board_cards c JOIN board_lanes l ON l.id = c.lane_id
         WHERE c.project_id = ?1
         ORDER BY l.position, c.position, c.id",
    )?;
    let rows = statement.query_map(params![project_id], card_from_row)?;
    for row in rows {
        cards.push(row?);
    }
    Ok(BoardState {
        project_id,
        lanes,
        cards,
    })
}

fn read_card(tx: &Transaction<'_>, card_id: &str) -> Result<Option<StoredCard>> {
    tx.query_row(
        "SELECT c.id, c.project_id, c.lane_id, l.name, c.done, c.position, c.title,
                c.body, c.claim, c.revision, c.created_at, c.updated_at
         FROM board_cards c JOIN board_lanes l ON l.id = c.lane_id
         WHERE c.id = ?1",
        params![card_id],
        card_from_row,
    )
    .optional()
    .map_err(Into::into)
}

fn card_from_row(row: &Row<'_>) -> rusqlite::Result<StoredCard> {
    Ok(StoredCard {
        id: row.get(0)?,
        project_id: row.get(1)?,
        lane_id: row.get(2)?,
        lane: row.get(3)?,
        done: row.get::<_, i64>(4)? != 0,
        position: row.get(5)?,
        title: row.get(6)?,
        body: row.get(7)?,
        claim: row.get(8)?,
        revision: row.get::<_, i64>(9)? as u64,
        created_at_millis: row.get(10)?,
        updated_at_millis: row.get(11)?,
    })
}

fn require_card(tx: &Transaction<'_>, project_id: i64, card_id: &str) -> Result<StoredCard> {
    read_card(tx, card_id)?
        .filter(|card| card.project_id == project_id)
        .with_context(|| format!("no card {card_id} in project {project_id}"))
}

fn check_revision(card: &StoredCard, expected: Option<u64>) -> Result<()> {
    if let Some(expected) = expected {
        if card.revision != expected {
            bail!(
                "card {} changed (revision {}, expected {expected})",
                card.id,
                card.revision
            );
        }
    }
    Ok(())
}

/// Apply a card update: any of title/body, a lane move, or a claim change.
/// Everything absent stays as it was; the revision and timestamp advance.
fn bump(
    tx: &Transaction<'_>,
    card_id: &str,
    title: Option<&str>,
    body: Option<&str>,
    lane: Option<(i64, i64, bool)>,
    claim: Option<Option<&str>>,
) -> Result<()> {
    let now = now_millis();
    if let Some((lane_id, position, done)) = lane {
        tx.execute(
            "UPDATE board_cards SET
                lane_id = ?2, position = ?3, done = ?4,
                title = COALESCE(?5, title), body = COALESCE(?6, body),
                revision = revision + 1, updated_at = ?7
             WHERE id = ?1",
            params![card_id, lane_id, position, done, title, body, now],
        )?;
    } else if title.is_some() || body.is_some() {
        tx.execute(
            "UPDATE board_cards SET
                title = COALESCE(?2, title), body = COALESCE(?3, body),
                revision = revision + 1, updated_at = ?4
             WHERE id = ?1",
            params![card_id, title, body, now],
        )?;
    }
    if let Some(claim) = claim {
        tx.execute(
            "UPDATE board_cards SET claim = ?2, revision = revision + 1, updated_at = ?3
             WHERE id = ?1",
            params![card_id, claim, now],
        )?;
    }
    Ok(())
}

fn ensure_project(tx: &Transaction<'_>, project_id: i64) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO board_projects (project_id) VALUES (?1)",
        params![project_id],
    )?;
    let lanes: i64 = tx.query_row(
        "SELECT COUNT(*) FROM board_lanes WHERE project_id = ?1",
        params![project_id],
        |row| row.get(0),
    )?;
    if lanes == 0 {
        for (index, (name, kind)) in DEFAULT_LANES.iter().enumerate() {
            tx.execute(
                "INSERT INTO board_lanes (project_id, name, kind, position)
                 VALUES (?1, ?2, ?3, ?4)",
                params![project_id, name, kind, index as i64],
            )?;
        }
    }
    Ok(())
}

fn resolve_lane(tx: &Transaction<'_>, project_id: i64, name: Option<&str>) -> Result<Lane> {
    let name = name.unwrap_or(DEFAULT_LANES[0].0);
    tx.query_row(
        "SELECT id, name, kind, position FROM board_lanes
         WHERE project_id = ?1 AND name = ?2",
        params![project_id, name],
        |row| {
            Ok(Lane {
                id: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
                position: row.get(3)?,
            })
        },
    )
    .optional()?
    .with_context(|| format!("no lane named “{name}”"))
}

fn first_open_lane(tx: &Transaction<'_>, project_id: i64) -> Result<Lane> {
    tx.query_row(
        "SELECT id, name, kind, position FROM board_lanes
         WHERE project_id = ?1 AND kind <> 'done'
         ORDER BY position, id LIMIT 1",
        params![project_id],
        |row| {
            Ok(Lane {
                id: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
                position: row.get(3)?,
            })
        },
    )
    .optional()?
    .context("the board has no open lane")
}

fn next_position(tx: &Transaction<'_>, project_id: i64, lane_id: i64) -> Result<i64> {
    let max: Option<i64> = tx.query_row(
        "SELECT MAX(position) FROM board_cards WHERE project_id = ?1 AND lane_id = ?2",
        params![project_id, lane_id],
        |row| row.get(0),
    )?;
    Ok(max.map_or(0, |value| value + 1))
}

fn validate_title(title: &str) -> Result<String> {
    let title = title.trim();
    if title.is_empty() {
        bail!("a card needs a title");
    }
    if title.len() > MAX_TITLE {
        bail!("card title exceeds {MAX_TITLE} bytes");
    }
    Ok(title.to_string())
}

fn validate_body(body: &str) -> Result<()> {
    if body.len() > MAX_BODY {
        bail!("card body exceeds {MAX_BODY} bytes");
    }
    Ok(())
}

fn validate_claim(claim: &str) -> Result<()> {
    let claim = claim.trim();
    if claim.is_empty() || claim.len() > MAX_CLAIM {
        bail!("a claim must contain 1..{MAX_CLAIM} bytes");
    }
    if !claim
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
    {
        bail!("a claim may only contain letters, digits, '-', '_' or '.'");
    }
    Ok(())
}

fn new_card_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let count = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = now_millis() as u128;
    let pid = std::process::id();
    format!("card-{nanos:x}-{pid:x}-{count:x}")
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> BoardStore {
        BoardStore::open_in_memory().unwrap()
    }

    fn lane_cards(state: &BoardState, lane: &str) -> Vec<String> {
        state
            .cards
            .iter()
            .filter(|card| card.lane == lane)
            .map(|card| card.title.clone())
            .collect()
    }

    #[test]
    fn a_new_project_seeds_the_default_lanes() {
        let store = store();
        let state = store.state(7).unwrap();
        let names: Vec<&str> = state.lanes.iter().map(|lane| lane.name.as_str()).collect();
        assert_eq!(names, vec!["Todo", "In progress", "Review", "Done"]);
        assert!(state.cards.is_empty());
        assert!(!store.is_initialized(7).unwrap());
    }

    #[test]
    fn add_move_and_complete_follow_the_lane_kind() {
        let store = store();
        let added = store
            .add_card(1, None, "Fix login", "the 302 loop", None)
            .unwrap();
        assert_eq!(added.action, "added");
        assert_eq!(added.card.lane, "Todo");
        assert!(!added.card.done);

        let moved = store
            .move_card(1, &added.card.id, "In progress", None)
            .unwrap();
        assert_eq!(moved.action, "moved");
        assert_eq!(moved.from_lane.as_deref(), Some("Todo"));
        assert_eq!(moved.card.lane, "In progress");
        assert!(!moved.card.done);

        let done = store.complete_card(1, &added.card.id, None).unwrap();
        assert_eq!(done.action, "completed");
        assert_eq!(done.card.lane, "Done");
        assert!(done.card.done);

        let reopened = store.reopen_card(1, &added.card.id, None).unwrap();
        assert_eq!(reopened.action, "reopened");
        assert_eq!(reopened.card.lane, "Todo");
        assert!(!reopened.card.done);
    }

    #[test]
    fn moving_a_card_drops_its_claim() {
        let store = store();
        let card = store
            .add_card(1, None, "Task", "", Some("claude-abc"))
            .unwrap()
            .card;
        assert_eq!(card.claim.as_deref(), Some("claude-abc"));
        let moved = store.move_card(1, &card.id, "Review", None).unwrap();
        assert_eq!(moved.card.claim, None);
    }

    #[test]
    fn next_claims_the_first_open_card_and_respects_a_lane_filter() {
        let store = store();
        store.add_card(1, Some("Todo"), "first", "", None).unwrap();
        store
            .add_card(1, Some("In progress"), "second", "", None)
            .unwrap();

        let claimed = store.next_card(1, "codex-1", None).unwrap().unwrap();
        assert_eq!(claimed.card.title, "first");
        assert_eq!(claimed.card.claim.as_deref(), Some("codex-1"));
        assert_eq!(claimed.action, "claimed");

        let in_progress = store
            .next_card(1, "codex-1", Some("In progress"))
            .unwrap()
            .unwrap();
        assert_eq!(in_progress.card.title, "second");
        // Both are claimed now: nothing left in Todo.
        assert!(store
            .next_card(1, "codex-2", Some("Todo"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_stale_revision_is_refused() {
        let store = store();
        let card = store.add_card(1, None, "Task", "", None).unwrap().card;
        // Fresh revision: succeeds.
        store
            .update_card(1, &card.id, Some("Renamed"), None, Some(card.revision))
            .unwrap();
        // Stale revision: refused, nothing written.
        let error = store
            .update_card(1, &card.id, Some("Again"), None, Some(card.revision))
            .unwrap_err();
        assert!(error.to_string().contains("changed"), "{error}");
        let state = store.state(1).unwrap();
        assert_eq!(state.cards[0].title, "Renamed");
    }

    #[test]
    fn cards_keep_their_order_within_a_lane() {
        let store = store();
        store.add_card(1, Some("Todo"), "one", "", None).unwrap();
        store.add_card(1, Some("Todo"), "two", "", None).unwrap();
        store.add_card(1, Some("Todo"), "three", "", None).unwrap();
        let state = store.state(1).unwrap();
        assert_eq!(lane_cards(&state, "Todo"), vec!["one", "two", "three"]);
    }

    #[test]
    fn empty_titles_and_bad_claims_are_refused() {
        let store = store();
        assert!(store.add_card(1, None, "   ", "", None).is_err());
        assert!(store
            .add_card(1, None, "ok", "", Some("bad claim!"))
            .is_err());
        assert!(store.next_card(1, "", None).is_err());
    }

    #[test]
    fn the_schema_is_versioned() {
        let store = BoardStore::open_in_memory().unwrap();
        let inner = store.inner.lock();
        let version: i64 = inner
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }
}
