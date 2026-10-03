//! Durable project activity and human attention, independent of PTY output.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

const MAX_REPLAY: usize = 200;
const WATCH_QUEUE: usize = 64;
const MAX_TEXT: usize = 16 * 1024;
const MAX_ID: usize = 200;

/// Bump when the tables below change; add the next step to `SCHEMA_STEPS`.
const SCHEMA_VERSION: i64 = 2;
const SCHEMA_STEPS: [&str; 2] = [
    r#"
    CREATE TABLE IF NOT EXISTS activity_events (
        project_id INTEGER NOT NULL,
        sequence INTEGER NOT NULL,
        event_id TEXT NOT NULL UNIQUE,
        data TEXT NOT NULL,
        PRIMARY KEY(project_id, sequence)
    );
    CREATE INDEX IF NOT EXISTS activity_recent
        ON activity_events(project_id, sequence DESC);
    CREATE TABLE IF NOT EXISTS attention_requests (
        project_id INTEGER NOT NULL,
        request_id TEXT NOT NULL UNIQUE,
        revision INTEGER NOT NULL,
        resolved INTEGER NOT NULL DEFAULT 0,
        data TEXT NOT NULL,
        PRIMARY KEY(project_id, request_id)
    );
    CREATE INDEX IF NOT EXISTS attention_unresolved
        ON attention_requests(project_id, resolved, request_id);
    CREATE TABLE IF NOT EXISTS activity_commands (
        project_id INTEGER NOT NULL,
        command_id TEXT NOT NULL,
        command_type TEXT NOT NULL,
        result TEXT NOT NULL,
        PRIMARY KEY(project_id, command_id)
    );
    CREATE TABLE IF NOT EXISTS board_snapshots (
        project_id INTEGER PRIMARY KEY,
        board_json TEXT
    );
"#,
    // The daemon no longer watches a markdown board; card transitions are
    // published by the store's mutations, so the baseline table is dead.
    r#"
    DROP TABLE IF EXISTS board_snapshots;
"#,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "kebab-case")]
pub enum AgentState {
    Unknown,
    Working,
    WaitingForInput,
    WaitingForApproval,
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "kebab-case")]
pub enum AttentionKind {
    Question,
    Approval,
    Failure,
    Review,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "kebab-case")]
pub enum AttentionActionKind {
    Answer,
    Approve,
    Deny,
    Dismiss,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", content = "value", rename_all = "snake_case")]
pub enum AttentionResponse {
    Answer(String),
    Approve,
    Deny,
    Dismiss,
}

impl AttentionResponse {
    fn kind(&self) -> AttentionActionKind {
        match self {
            Self::Answer(_) => AttentionActionKind::Answer,
            Self::Approve => AttentionActionKind::Approve,
            Self::Deny => AttentionActionKind::Deny,
            Self::Dismiss => AttentionActionKind::Dismiss,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    AgentStateChanged,
    Reported,
    AttentionRequested,
    AttentionSeen,
    AttentionAcknowledged,
    AttentionResolved,
    BoardChanged,
    SessionLifecycle,
    CommandResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ActivityPayload {
    AgentState {
        state: AgentState,
        message: Option<String>,
    },
    Message {
        text: String,
    },
    AttentionRequested {
        request_id: String,
        attention_kind: AttentionKind,
        reason: String,
        allowed_actions: Vec<AttentionActionKind>,
    },
    AttentionSeen {
        request_id: String,
        revision: u64,
    },
    AttentionAcknowledged {
        request_id: String,
        revision: u64,
    },
    AttentionResolved {
        request_id: String,
        revision: u64,
        response: AttentionResponse,
    },
    BoardChanged {
        action: String,
        card_id: Option<String>,
        title: Option<String>,
        /// Column names are stable across column insertion/reordering.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        column: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_column: Option<String>,
    },
    SessionLifecycle {
        state: String,
        detail: Option<String>,
    },
    CommandResult {
        command_id: String,
        ok: bool,
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEvent {
    pub id: String,
    pub project_id: i64,
    pub sequence: u64,
    pub at_millis: i64,
    pub session_id: Option<String>,
    pub card_id: Option<String>,
    pub kind: ActivityKind,
    pub payload: ActivityPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attention {
    pub id: String,
    pub source_event_id: String,
    pub project_id: i64,
    pub session_id: Option<String>,
    pub card_id: Option<String>,
    pub kind: AttentionKind,
    pub reason: String,
    pub allowed_actions: Vec<AttentionActionKind>,
    pub created_at_millis: i64,
    pub seen_at_millis: Option<i64>,
    pub acknowledged_at_millis: Option<i64>,
    pub resolved_at_millis: Option<i64>,
    pub resolution: Option<AttentionResponse>,
    pub revision: u64,
}

impl Attention {
    pub fn is_unresolved(&self) -> bool {
        self.resolved_at_millis.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivitySnapshot {
    pub project_id: i64,
    pub watermark: u64,
    /// Chronological order; the first event is the oldest in this page.
    pub events: Vec<ActivityEvent>,
    pub attention: Vec<Attention>,
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateAttentionResult {
    pub attention: Attention,
    pub event: ActivityEvent,
    pub duplicate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionMutationResult {
    pub attention: Attention,
    pub event: Option<ActivityEvent>,
    pub duplicate: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishActivity {
    pub project_id: i64,
    pub command_id: String,
    pub session_id: Option<String>,
    pub card_id: Option<String>,
    pub kind: ActivityKind,
    pub payload: ActivityPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAttention {
    pub project_id: i64,
    pub command_id: String,
    pub session_id: Option<String>,
    pub card_id: Option<String>,
    pub kind: AttentionKind,
    pub reason: String,
    pub allowed_actions: Vec<AttentionActionKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "change", content = "value", rename_all = "snake_case")]
pub enum AttentionChange {
    MarkSeen,
    Acknowledge,
    Respond(AttentionResponse),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeAttention {
    pub project_id: i64,
    pub request_id: String,
    pub command_id: String,
    pub expected_revision: u64,
    pub change: AttentionChange,
}

struct Subscriber {
    tx: SyncSender<ActivityEvent>,
    lagged: Arc<AtomicBool>,
    attached: Arc<AtomicBool>,
}

struct Inner {
    connection: Connection,
    subscribers: HashMap<i64, Vec<Subscriber>>,
}

/// One SQLite journal and independent bounded watchers, shared by daemon
/// request threads. Appends and watch registration take the same lock, so a
/// subscriber cannot miss an event between its replay watermark and live tail.
pub struct ActivityJournal {
    inner: Mutex<Inner>,
}

pub struct ActivitySubscription {
    rx: Receiver<ActivityEvent>,
    lagged: Arc<AtomicBool>,
    attached: Arc<AtomicBool>,
}

impl Drop for ActivitySubscription {
    fn drop(&mut self) {
        self.attached.store(false, Ordering::Release);
    }
}

impl ActivitySubscription {
    pub fn try_recv(&self) -> std::result::Result<ActivityEvent, ActivityReceiveError> {
        if self.lagged.load(Ordering::Acquire) {
            return Err(ActivityReceiveError::ResyncRequired);
        }
        let result = self.rx.try_recv().map_err(|error| match error {
            TryRecvError::Empty => ActivityReceiveError::Empty,
            TryRecvError::Disconnected => ActivityReceiveError::Closed,
        });
        if self.lagged.load(Ordering::Acquire) {
            return Err(ActivityReceiveError::ResyncRequired);
        }
        result
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ActivityReceiveError {
    Empty,
    ResyncRequired,
    Closed,
}

pub enum WatchResult {
    Ready(ActivitySnapshot, ActivitySubscription),
    ResyncRequired,
}

impl ActivityJournal {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("opening activity journal {}", path.display()))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.busy_timeout(std::time::Duration::from_secs(2))?;
        let journal = Self::from_connection(connection)?;
        #[cfg(unix)]
        if path.exists() {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(journal)
    }

    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self> {
        super::schema::migrate(&connection, SCHEMA_VERSION, &SCHEMA_STEPS)?;
        Ok(Self {
            inner: Mutex::new(Inner {
                connection,
                subscribers: HashMap::new(),
            }),
        })
    }

    pub fn publish(&self, input: PublishActivity) -> Result<ActivityEvent> {
        validate_identity(
            input.project_id,
            &input.command_id,
            input.session_id.as_deref(),
        )?;
        validate_card_id(input.card_id.as_deref())?;
        validate_activity(&input.kind, &input.payload)?;
        let mut inner = self.inner.lock();
        let tx = inner.connection.transaction()?;
        if let Some(event) =
            cached::<ActivityEvent>(&tx, input.project_id, &input.command_id, "publish")?
        {
            tx.commit()?;
            return Ok(event);
        }
        let event = append_event(
            &tx,
            input.project_id,
            input.session_id,
            input.card_id,
            input.kind,
            input.payload,
        )?;
        cache(&tx, input.project_id, &input.command_id, "publish", &event)?;
        tx.commit()?;
        notify(&mut inner, event.clone());
        Ok(event)
    }

    pub fn create_attention(&self, input: CreateAttention) -> Result<CreateAttentionResult> {
        validate_identity(
            input.project_id,
            &input.command_id,
            input.session_id.as_deref(),
        )?;
        let reason = input.reason.trim();
        if reason.is_empty() || reason.len() > MAX_TEXT {
            bail!("attention reason must contain 1..{MAX_TEXT} bytes");
        }
        if input.allowed_actions.is_empty() || input.allowed_actions.len() > 4 {
            bail!("attention must have 1..4 allowed actions");
        }
        validate_card_id(input.card_id.as_deref())?;
        if has_duplicates(&input.allowed_actions) {
            bail!("attention allowed actions must be unique");
        }

        let mut inner = self.inner.lock();
        let tx = inner.connection.transaction()?;
        if let Some(mut result) = cached::<CreateAttentionResult>(
            &tx,
            input.project_id,
            &input.command_id,
            "create_attention",
        )? {
            tx.commit()?;
            result.duplicate = true;
            return Ok(result);
        }

        let sequence = next_sequence(&tx, input.project_id)?;
        let event_id = format!("evt-{}-{sequence}", input.project_id);
        let request_id = format!("attention-{}-{sequence}", input.project_id);
        let attention = Attention {
            id: request_id.clone(),
            source_event_id: event_id,
            project_id: input.project_id,
            session_id: input.session_id.clone(),
            card_id: input.card_id.clone(),
            kind: input.kind,
            reason: reason.to_string(),
            allowed_actions: input.allowed_actions.clone(),
            created_at_millis: now_millis(),
            seen_at_millis: None,
            acknowledged_at_millis: None,
            resolved_at_millis: None,
            resolution: None,
            revision: 1,
        };
        let event = ActivityEvent {
            id: attention.source_event_id.clone(),
            project_id: input.project_id,
            sequence,
            at_millis: attention.created_at_millis,
            session_id: input.session_id,
            card_id: input.card_id,
            kind: ActivityKind::AttentionRequested,
            payload: ActivityPayload::AttentionRequested {
                request_id,
                attention_kind: input.kind,
                reason: reason.to_string(),
                allowed_actions: input.allowed_actions,
            },
        };
        write_event(&tx, &event)?;
        tx.execute(
            "INSERT INTO attention_requests(project_id, request_id, revision, resolved, data)
             VALUES (?1, ?2, ?3, 0, ?4)",
            params![
                input.project_id,
                attention.id,
                attention.revision as i64,
                serde_json::to_string(&attention)?
            ],
        )?;
        let result = CreateAttentionResult {
            attention,
            event: event.clone(),
            duplicate: false,
        };
        cache(
            &tx,
            input.project_id,
            &input.command_id,
            "create_attention",
            &result,
        )?;
        tx.commit()?;
        notify(&mut inner, event);
        Ok(result)
    }

    pub fn change_attention(&self, input: ChangeAttention) -> Result<AttentionMutationResult> {
        validate_identity(input.project_id, &input.command_id, None)?;
        if input.request_id.trim().is_empty() || input.request_id.len() > MAX_ID {
            bail!("attention ID must contain 1..{MAX_ID} characters");
        }
        let command_type = match &input.change {
            AttentionChange::MarkSeen => "attention_seen",
            AttentionChange::Acknowledge => "attention_acknowledged",
            AttentionChange::Respond(_) => "attention_responded",
        };
        let mut inner = self.inner.lock();
        let tx = inner.connection.transaction()?;
        if let Some(mut result) = cached::<AttentionMutationResult>(
            &tx,
            input.project_id,
            &input.command_id,
            command_type,
        )? {
            tx.commit()?;
            result.duplicate = true;
            return Ok(result);
        }

        let mut attention = load_attention(&tx, input.project_id, &input.request_id)?;
        if attention.revision != input.expected_revision {
            bail!(
                "attention revision conflict: expected {}, current {}",
                input.expected_revision,
                attention.revision
            );
        }
        let mut event = None;
        let mut changed = false;
        match &input.change {
            AttentionChange::MarkSeen if attention.seen_at_millis.is_none() => {
                attention.seen_at_millis = Some(now_millis());
                changed = true;
            }
            AttentionChange::Acknowledge if attention.acknowledged_at_millis.is_none() => {
                ensure_unresolved(&attention)?;
                attention.acknowledged_at_millis = Some(now_millis());
                changed = true;
            }
            AttentionChange::Respond(response) => {
                ensure_unresolved(&attention)?;
                if !attention.allowed_actions.contains(&response.kind()) {
                    bail!("response action is not allowed for this attention request");
                }
                if let AttentionResponse::Answer(text) = response {
                    if text.trim().is_empty() || text.len() > MAX_TEXT {
                        bail!("answer must contain 1..{MAX_TEXT} bytes");
                    }
                }
                attention.resolved_at_millis = Some(now_millis());
                attention.resolution = Some(response.clone());
                changed = true;
            }
            AttentionChange::MarkSeen => {}
            AttentionChange::Acknowledge => ensure_unresolved(&attention)?,
        }

        if changed {
            attention.revision += 1;
            let (kind, payload) = match &input.change {
                AttentionChange::MarkSeen => (
                    ActivityKind::AttentionSeen,
                    ActivityPayload::AttentionSeen {
                        request_id: attention.id.clone(),
                        revision: attention.revision,
                    },
                ),
                AttentionChange::Acknowledge => (
                    ActivityKind::AttentionAcknowledged,
                    ActivityPayload::AttentionAcknowledged {
                        request_id: attention.id.clone(),
                        revision: attention.revision,
                    },
                ),
                AttentionChange::Respond(_) => (
                    ActivityKind::AttentionResolved,
                    ActivityPayload::AttentionResolved {
                        request_id: attention.id.clone(),
                        revision: attention.revision,
                        response: attention.resolution.clone().expect("response set"),
                    },
                ),
            };
            let appended = append_event(
                &tx,
                attention.project_id,
                attention.session_id.clone(),
                attention.card_id.clone(),
                kind,
                payload,
            )?;
            event = Some(appended.clone());
            tx.execute(
                "UPDATE attention_requests SET revision = ?3, resolved = ?4, data = ?5
                 WHERE project_id = ?1 AND request_id = ?2",
                params![
                    attention.project_id,
                    attention.id,
                    attention.revision as i64,
                    attention.resolved_at_millis.is_some() as i64,
                    serde_json::to_string(&attention)?
                ],
            )?;
        }

        let result = AttentionMutationResult {
            attention,
            event: event.clone(),
            duplicate: false,
        };
        cache(
            &tx,
            input.project_id,
            &input.command_id,
            command_type,
            &result,
        )?;
        tx.commit()?;
        if let Some(event) = event {
            notify(&mut inner, event);
        }
        Ok(result)
    }

    /// A snapshot without a cursor returns the newest `limit` events. With a
    /// cursor, it returns the next chronological page for catch-up.
    pub fn snapshot(
        &self,
        project_id: i64,
        after_sequence: Option<u64>,
        limit: usize,
    ) -> Result<ActivitySnapshot> {
        validate_project(project_id)?;
        validate_limit(limit)?;
        let inner = self.inner.lock();
        snapshot_locked(&inner.connection, project_id, after_sequence, limit)
    }

    /// Read the authoritative current state of a request, whether unresolved
    /// or resolved. Waiters use this after a stream resync so a response cannot
    /// be lost just because its event has aged out of the bounded replay page.
    pub fn attention(&self, project_id: i64, request_id: &str) -> Result<Attention> {
        validate_project(project_id)?;
        if request_id.trim().is_empty() || request_id.len() > MAX_ID {
            bail!("attention ID must contain 1..{MAX_ID} characters");
        }
        let inner = self.inner.lock();
        load_attention(&inner.connection, project_id, request_id)
    }

    /// Every unresolved request of a project, oldest first: the derived
    /// board read consults this, not the bounded event page.
    pub fn unresolved_attention(&self, project_id: i64) -> Result<Vec<Attention>> {
        validate_project(project_id)?;
        let inner = self.inner.lock();
        let connection = &inner.connection;
        let mut stmt = connection.prepare(
            "SELECT data FROM attention_requests WHERE project_id = ?1 AND resolved = 0
             ORDER BY request_id ASC",
        )?;
        let rows = stmt.query_map([project_id], |row| row.get::<_, String>(0))?;
        rows.map(|row| {
            serde_json::from_str(&row?).map_err(|error| {
                anyhow::Error::from(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                ))
            })
        })
        .collect()
    }

    /// Replay and register under the same lock as append. More than 200
    /// missed events asks the client to take a fresh bounded snapshot first.
    pub fn watch(&self, project_id: i64, after_sequence: u64) -> Result<WatchResult> {
        validate_project(project_id)?;
        let mut inner = self.inner.lock();
        let snapshot = snapshot_locked(
            &inner.connection,
            project_id,
            Some(after_sequence),
            MAX_REPLAY,
        )?;
        if snapshot.has_more {
            return Ok(WatchResult::ResyncRequired);
        }
        let (tx, rx) = mpsc::sync_channel(WATCH_QUEUE);
        let lagged = Arc::new(AtomicBool::new(false));
        let attached = Arc::new(AtomicBool::new(true));
        inner
            .subscribers
            .entry(project_id)
            .or_default()
            .push(Subscriber {
                tx,
                lagged: lagged.clone(),
                attached: attached.clone(),
            });
        Ok(WatchResult::Ready(
            snapshot,
            ActivitySubscription {
                rx,
                lagged,
                attached,
            },
        ))
    }
}

fn validate_project(project_id: i64) -> Result<()> {
    if project_id <= 0 {
        bail!("project ID must be positive");
    }
    Ok(())
}

fn validate_identity(project_id: i64, command_id: &str, session_id: Option<&str>) -> Result<()> {
    validate_project(project_id)?;
    if command_id.trim().is_empty() || command_id.len() > MAX_ID {
        bail!("command ID must contain 1..{MAX_ID} characters");
    }
    if session_id.is_some_and(|id| id.trim().is_empty() || id.len() > MAX_ID) {
        bail!("session ID must contain 1..{MAX_ID} characters");
    }
    Ok(())
}

fn validate_card_id(card_id: Option<&str>) -> Result<()> {
    if card_id.is_some_and(|id| id.trim().is_empty() || id.len() > MAX_ID) {
        bail!("card ID must contain 1..{MAX_ID} characters");
    }
    Ok(())
}

fn validate_activity(kind: &ActivityKind, payload: &ActivityPayload) -> Result<()> {
    let valid_pair = matches!(
        (kind, payload),
        (
            ActivityKind::AgentStateChanged,
            ActivityPayload::AgentState { .. }
        ) | (ActivityKind::Reported, ActivityPayload::Message { .. })
            | (
                ActivityKind::BoardChanged,
                ActivityPayload::BoardChanged { .. }
            )
            | (
                ActivityKind::SessionLifecycle,
                ActivityPayload::SessionLifecycle { .. }
            )
            | (
                ActivityKind::CommandResult,
                ActivityPayload::CommandResult { .. }
            )
    );
    if !valid_pair {
        bail!("activity kind does not match its payload");
    }
    match payload {
        ActivityPayload::AgentState { message, .. } => {
            if message.as_ref().is_some_and(|value| value.len() > MAX_TEXT) {
                bail!("agent state message exceeds {MAX_TEXT} bytes");
            }
        }
        ActivityPayload::Message { text } => {
            if text.trim().is_empty() || text.len() > MAX_TEXT {
                bail!("activity message must contain 1..{MAX_TEXT} bytes");
            }
        }
        ActivityPayload::BoardChanged {
            action,
            title,
            column,
            from_column,
            ..
        } => {
            if action.trim().is_empty() || action.len() > 200 {
                bail!("board activity action must contain 1..200 bytes");
            }
            if title.as_ref().is_some_and(|value| value.len() > MAX_TEXT) {
                bail!("board activity title exceeds {MAX_TEXT} bytes");
            }
            if column.as_ref().is_some_and(|value| value.len() > MAX_TEXT)
                || from_column
                    .as_ref()
                    .is_some_and(|value| value.len() > MAX_TEXT)
            {
                bail!("board activity column exceeds {MAX_TEXT} bytes");
            }
        }
        ActivityPayload::SessionLifecycle { state, detail } => {
            if state.trim().is_empty() || state.len() > 200 {
                bail!("session lifecycle state must contain 1..200 bytes");
            }
            if detail.as_ref().is_some_and(|value| value.len() > MAX_TEXT) {
                bail!("session lifecycle detail exceeds {MAX_TEXT} bytes");
            }
        }
        ActivityPayload::CommandResult {
            command_id, detail, ..
        } => {
            if command_id.trim().is_empty() || command_id.len() > MAX_ID {
                bail!("result command ID must contain 1..{MAX_ID} bytes");
            }
            if detail.as_ref().is_some_and(|value| value.len() > MAX_TEXT) {
                bail!("command result detail exceeds {MAX_TEXT} bytes");
            }
        }
        ActivityPayload::AttentionRequested { .. }
        | ActivityPayload::AttentionSeen { .. }
        | ActivityPayload::AttentionAcknowledged { .. }
        | ActivityPayload::AttentionResolved { .. } => {
            bail!("attention events must use the revision-checked attention API")
        }
    }
    Ok(())
}

fn validate_limit(limit: usize) -> Result<()> {
    if limit == 0 || limit > MAX_REPLAY {
        bail!("activity page must contain 1..{MAX_REPLAY} events");
    }
    Ok(())
}

fn has_duplicates<T: PartialEq>(values: &[T]) -> bool {
    values
        .iter()
        .enumerate()
        .any(|(i, value)| values[..i].contains(value))
}

fn ensure_unresolved(attention: &Attention) -> Result<()> {
    if !attention.is_unresolved() {
        bail!("attention request is already resolved");
    }
    Ok(())
}

fn next_sequence(tx: &Transaction<'_>, project_id: i64) -> Result<u64> {
    let sequence: i64 = tx.query_row(
        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM activity_events WHERE project_id = ?1",
        [project_id],
        |row| row.get(0),
    )?;
    Ok(sequence as u64)
}

fn append_event(
    tx: &Transaction<'_>,
    project_id: i64,
    session_id: Option<String>,
    card_id: Option<String>,
    kind: ActivityKind,
    payload: ActivityPayload,
) -> Result<ActivityEvent> {
    let sequence = next_sequence(tx, project_id)?;
    let event = ActivityEvent {
        id: format!("evt-{project_id}-{sequence}"),
        project_id,
        sequence,
        at_millis: now_millis(),
        session_id,
        card_id,
        kind,
        payload,
    };
    write_event(tx, &event)?;
    Ok(event)
}

fn write_event(tx: &Transaction<'_>, event: &ActivityEvent) -> Result<()> {
    tx.execute(
        "INSERT INTO activity_events(project_id, sequence, event_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            event.project_id,
            event.sequence as i64,
            event.id,
            serde_json::to_string(event)?
        ],
    )?;
    Ok(())
}

fn cached<T: DeserializeOwned>(
    tx: &Transaction<'_>,
    project_id: i64,
    command_id: &str,
    command_type: &str,
) -> Result<Option<T>> {
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT command_type, result FROM activity_commands WHERE project_id = ?1 AND command_id = ?2",
            params![project_id, command_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match row {
        None => Ok(None),
        Some((stored_type, result)) if stored_type == command_type => {
            Ok(Some(serde_json::from_str(&result)?))
        }
        Some(_) => bail!("command ID was already used for a different operation"),
    }
}

fn cache<T: Serialize>(
    tx: &Transaction<'_>,
    project_id: i64,
    command_id: &str,
    command_type: &str,
    result: &T,
) -> Result<()> {
    tx.execute(
        "INSERT INTO activity_commands(project_id, command_id, command_type, result) VALUES (?1, ?2, ?3, ?4)",
        params![project_id, command_id, command_type, serde_json::to_string(result)?],
    )?;
    Ok(())
}

fn load_attention(conn: &Connection, project_id: i64, id: &str) -> Result<Attention> {
    let encoded: String = conn
        .query_row(
            "SELECT data FROM attention_requests WHERE project_id = ?1 AND request_id = ?2",
            params![project_id, id],
            |row| row.get(0),
        )
        .optional()?
        .context("unknown attention request")?;
    Ok(serde_json::from_str(&encoded)?)
}

fn snapshot_locked(
    conn: &Connection,
    project_id: i64,
    after_sequence: Option<u64>,
    limit: usize,
) -> Result<ActivitySnapshot> {
    let watermark: i64 = conn.query_row(
        "SELECT COALESCE(MAX(sequence), 0) FROM activity_events WHERE project_id = ?1",
        [project_id],
        |row| row.get(0),
    )?;
    let watermark = watermark as u64;
    if after_sequence.is_some_and(|sequence| sequence > watermark) {
        bail!("activity cursor is ahead of the project watermark");
    }

    let (events, has_more) = match after_sequence {
        Some(sequence) => {
            let mut stmt = conn.prepare(
                "SELECT data FROM activity_events WHERE project_id = ?1 AND sequence > ?2
                 ORDER BY sequence ASC LIMIT ?3",
            )?;
            let rows = stmt.query_map(
                params![project_id, sequence as i64, (limit + 1) as i64],
                |row| row.get::<_, String>(0),
            )?;
            let mut events = rows
                .map(|row| {
                    serde_json::from_str(&row?).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })
                })
                .collect::<rusqlite::Result<Vec<ActivityEvent>>>()?;
            let has_more = events.len() > limit;
            events.truncate(limit);
            (events, has_more)
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT data FROM activity_events WHERE project_id = ?1
                 ORDER BY sequence DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![project_id, limit as i64], |row| {
                row.get::<_, String>(0)
            })?;
            let mut events = rows
                .map(|row| {
                    serde_json::from_str(&row?).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })
                })
                .collect::<rusqlite::Result<Vec<ActivityEvent>>>()?;
            events.reverse();
            let total: i64 = conn.query_row(
                "SELECT COUNT(*) FROM activity_events WHERE project_id = ?1",
                [project_id],
                |row| row.get(0),
            )?;
            let has_more = total as usize > events.len();
            (events, has_more)
        }
    };

    let mut stmt = conn.prepare(
        "SELECT data FROM attention_requests WHERE project_id = ?1 AND resolved = 0
         ORDER BY request_id ASC",
    )?;
    let rows = stmt.query_map([project_id], |row| row.get::<_, String>(0))?;
    let attention = rows
        .map(|row| {
            serde_json::from_str(&row?).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .collect::<rusqlite::Result<Vec<Attention>>>()?;
    Ok(ActivitySnapshot {
        project_id,
        watermark,
        events,
        attention,
        has_more,
    })
}

fn notify(inner: &mut Inner, event: ActivityEvent) {
    let Some(subscribers) = inner.subscribers.get_mut(&event.project_id) else {
        return;
    };
    subscribers.retain(|subscriber| {
        if !subscriber.attached.load(Ordering::Acquire) {
            return false;
        }
        match subscriber.tx.try_send(event.clone()) {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(_)) => {
                subscriber.lagged.store(true, Ordering::Release);
                false
            }
            Err(mpsc::TrySendError::Disconnected(_)) => false,
        }
    });
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal() -> (tempfile::TempDir, ActivityJournal) {
        let dir = tempfile::tempdir().unwrap();
        let journal = ActivityJournal::open(&dir.path().join("activity.db")).unwrap();
        (dir, journal)
    }

    #[test]
    fn the_schema_is_versioned() {
        let journal = ActivityJournal::open_in_memory().unwrap();
        let inner = journal.inner.lock();
        let version: i64 = inner
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    fn request(command_id: &str) -> CreateAttention {
        CreateAttention {
            project_id: 9,
            command_id: command_id.into(),
            session_id: Some("session-a".into()),
            card_id: Some("card-stable-1".into()),
            kind: AttentionKind::Approval,
            reason: "Run the migration?".into(),
            allowed_actions: vec![AttentionActionKind::Approve, AttentionActionKind::Deny],
        }
    }

    #[test]
    fn activity_is_project_sequenced_replayed_and_persistent() {
        let (dir, journal) = journal();
        let first = journal
            .publish(PublishActivity {
                project_id: 9,
                command_id: "state-1".into(),
                session_id: Some("session-a".into()),
                card_id: Some("card-stable-1".into()),
                kind: ActivityKind::AgentStateChanged,
                payload: ActivityPayload::AgentState {
                    state: AgentState::Working,
                    message: None,
                },
            })
            .unwrap();
        let attention = journal.create_attention(request("request-1")).unwrap();
        assert_eq!(first.sequence, 1);
        assert_eq!(attention.event.sequence, 2);
        assert_eq!(attention.attention.source_event_id, attention.event.id);

        let page = journal.snapshot(9, Some(1), 10).unwrap();
        assert_eq!(page.watermark, 2);
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].id, attention.event.id);
        assert_eq!(page.attention, vec![attention.attention.clone()]);

        drop(journal);
        let reopened = ActivityJournal::open(&dir.path().join("activity.db")).unwrap();
        let restored = reopened.snapshot(9, None, 10).unwrap();
        assert_eq!(restored.watermark, 2);
        assert_eq!(restored.attention[0].id, attention.attention.id);
    }

    #[test]
    fn attention_commands_are_idempotent_revision_checked_and_action_validated() {
        let (_dir, journal) = journal();
        let created = journal.create_attention(request("request-1")).unwrap();
        let disallowed = ChangeAttention {
            project_id: 9,
            request_id: created.attention.id.clone(),
            command_id: "answer-not-allowed".into(),
            expected_revision: 1,
            change: AttentionChange::Respond(AttentionResponse::Answer("no".into())),
        };
        assert!(journal.change_attention(disallowed).is_err());

        let acknowledge = ChangeAttention {
            project_id: 9,
            request_id: created.attention.id.clone(),
            command_id: "ack-1".into(),
            expected_revision: 1,
            change: AttentionChange::Acknowledge,
        };
        let acknowledged = journal.change_attention(acknowledge.clone()).unwrap();
        assert!(acknowledged.attention.is_unresolved());
        assert!(acknowledged.attention.acknowledged_at_millis.is_some());
        assert_eq!(acknowledged.attention.revision, 2);
        assert_eq!(acknowledged.event.as_ref().unwrap().sequence, 2);

        let retried_ack = journal.change_attention(acknowledge).unwrap();
        assert!(retried_ack.duplicate);
        assert_eq!(retried_ack.attention, acknowledged.attention);

        let change = ChangeAttention {
            project_id: 9,
            request_id: created.attention.id.clone(),
            command_id: "approve-1".into(),
            expected_revision: 2,
            change: AttentionChange::Respond(AttentionResponse::Approve),
        };
        let applied = journal.change_attention(change.clone()).unwrap();
        assert!(!applied.attention.is_unresolved());
        assert_eq!(applied.attention.revision, 3);
        assert_eq!(applied.event.as_ref().unwrap().sequence, 3);

        let retried = journal.change_attention(change).unwrap();
        assert!(retried.duplicate);
        assert_eq!(retried.attention, applied.attention);
        assert_eq!(journal.snapshot(9, None, 10).unwrap().watermark, 3);

        let stale = ChangeAttention {
            project_id: 9,
            request_id: created.attention.id,
            command_id: "approve-2".into(),
            expected_revision: 2,
            change: AttentionChange::Respond(AttentionResponse::Deny),
        };
        assert!(journal.change_attention(stale).is_err());
    }

    #[test]
    fn watch_replays_atomically_and_requires_resync_after_queue_overload() {
        let (_dir, journal) = journal();
        let subscription = match journal.watch(9, 0).unwrap() {
            WatchResult::Ready(snapshot, subscription) => {
                assert_eq!(snapshot.watermark, 0);
                subscription
            }
            WatchResult::ResyncRequired => panic!("empty activity feed resynced"),
        };
        for i in 0..WATCH_QUEUE + 2 {
            journal
                .publish(PublishActivity {
                    project_id: 9,
                    command_id: format!("event-{i}"),
                    session_id: None,
                    card_id: None,
                    kind: ActivityKind::Reported,
                    payload: ActivityPayload::Message {
                        text: i.to_string(),
                    },
                })
                .unwrap();
        }
        assert_eq!(
            subscription.try_recv(),
            Err(ActivityReceiveError::ResyncRequired)
        );
        assert!(matches!(
            journal.watch(9, 0).unwrap(),
            WatchResult::Ready(..)
        ));
    }

    #[test]
    fn concurrent_watchers_receive_the_same_sequenced_tail() {
        let (_dir, journal) = journal();
        let first = match journal.watch(9, 0).unwrap() {
            WatchResult::Ready(snapshot, subscription) => {
                assert_eq!(snapshot.watermark, 0);
                subscription
            }
            WatchResult::ResyncRequired => panic!("empty activity feed resynced"),
        };
        let second = match journal.watch(9, 0).unwrap() {
            WatchResult::Ready(snapshot, subscription) => {
                assert_eq!(snapshot.watermark, 0);
                subscription
            }
            WatchResult::ResyncRequired => panic!("empty activity feed resynced"),
        };
        let event = journal
            .publish(PublishActivity {
                project_id: 9,
                command_id: "shared-tail".into(),
                session_id: None,
                card_id: None,
                kind: ActivityKind::Reported,
                payload: ActivityPayload::Message {
                    text: "visible to both clients".into(),
                },
            })
            .unwrap();
        assert_eq!(first.try_recv(), Ok(event.clone()));
        assert_eq!(second.try_recv(), Ok(event));
    }

    #[test]
    fn bounded_snapshot_marks_older_events_and_watch_resyncs_on_expired_cursor() {
        let (_dir, journal) = journal();
        for i in 0..MAX_REPLAY + 1 {
            journal
                .publish(PublishActivity {
                    project_id: 9,
                    command_id: format!("snapshot-{i}"),
                    session_id: None,
                    card_id: None,
                    kind: ActivityKind::Reported,
                    payload: ActivityPayload::Message {
                        text: format!("event {i}"),
                    },
                })
                .unwrap();
        }
        let snapshot = journal.snapshot(9, None, MAX_REPLAY).unwrap();
        assert_eq!(snapshot.events.len(), MAX_REPLAY);
        assert!(snapshot.has_more);
        assert!(matches!(
            journal.watch(9, 0).unwrap(),
            WatchResult::ResyncRequired
        ));
        assert!(matches!(
            journal.watch(9, snapshot.watermark).unwrap(),
            WatchResult::Ready(_, _)
        ));
    }
}
