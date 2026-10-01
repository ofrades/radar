//! Local HTTP/WebSocket client for sessions owned by the session daemon.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as RoutePath, State};
use axum::http::header::{self, HeaderMap};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response as HttpResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};

use crate::config::{self, Paths};
use crate::db::{Db, Project};
use crate::session::activity::{
    ActivityKind, ActivityPayload, ActivitySnapshot, AttentionChange, AttentionResponse,
    ChangeAttention, PublishActivity,
};
use crate::session::board_store::BoardState;
use crate::session::catalog::CatalogFilter;
use crate::session::daemon::{self, Client, Command, Response};
use crate::session::registry::{Lifecycle, Output, Spawn, Status};
use crate::session::Dims;

const INDEX: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");
const GHOSTTY_TERMINAL_JS: &str = include_str!("../web/ghostty-terminal.mjs");
/// The vendored xterm.js fallback. `app.js` imports these by URL, so they must
/// be served from the same `assets/` path the module graph resolves against.
const XTERM_JS: &str = include_str!("../web/vendor/xterm.mjs");
const FIT_ADDON_JS: &str = include_str!("../web/vendor/fit-addon.mjs");
const XTERM_CSS: &str = include_str!("../web/vendor/xterm.css");
/// The browser's libghostty-vt module, built by `build.rs`.
const GHOSTTY_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ghostty-vt.wasm"));

#[derive(Clone)]
struct WebState {
    home: Arc<PathBuf>,
}

/// Run Radar's browser client on loopback. Tailscale Serve can reverse-proxy
/// this local port without exposing the daemon's private Unix socket.
pub fn run(home: &Path, port: u16) -> Result<()> {
    daemon::ensure_running(home).context("start session daemon")?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build web runtime")?;
    runtime.block_on(async_run(home.to_path_buf(), port))
}

async fn async_run(home: PathBuf, port: u16) -> Result<()> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("bind local web client at http://{address}"))?;
    let state = WebState {
        home: Arc::new(home),
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/assets/app.js", get(app_js))
        .route("/assets/app.css", get(app_css))
        .route("/assets/xterm.css", get(xterm_css))
        .route("/assets/xterm.mjs", get(xterm_js))
        .route("/assets/fit-addon.mjs", get(fit_addon_js))
        .route("/assets/ghostty-terminal.mjs", get(ghostty_terminal_js))
        .route("/assets/ghostty-vt.wasm", get(ghostty_wasm))
        .route("/api/projects", get(projects))
        .route(
            "/api/projects/{project_id}/sessions",
            get(sessions).post(create_shell),
        )
        .route("/api/projects/{project_id}/activity", get(activity))
        .route(
            "/api/projects/{project_id}/board",
            get(board_snapshot).post(add_card),
        )
        .route(
            "/api/projects/{project_id}/cards/{card_id}",
            post(mutate_card),
        )
        .route(
            "/api/projects/{project_id}/cards/{card_id}/comments",
            post(comment_card),
        )
        .route(
            "/api/projects/{project_id}/attention/{request_id}",
            post(respond_attention),
        )
        .route(
            "/api/projects/{project_id}/sessions/{session_id}/terminal",
            get(attach_terminal),
        )
        .with_state(state);

    eprintln!("radar web: http://{address}");
    eprintln!(
        "tailnet: run `tailscale serve --bg --https=443 --set-path=/radar {port}` to add a private /radar route"
    );
    axum::serve(listener, app)
        .await
        .context("serve browser client")
}

async fn index() -> Html<&'static str> {
    Html(INDEX)
}

async fn app_js() -> impl IntoResponse {
    static_asset("text/javascript; charset=utf-8", APP_JS.as_bytes())
}

async fn app_css() -> impl IntoResponse {
    static_asset("text/css; charset=utf-8", APP_CSS.as_bytes())
}

async fn xterm_css() -> impl IntoResponse {
    static_asset("text/css; charset=utf-8", XTERM_CSS.as_bytes())
}

async fn xterm_js() -> impl IntoResponse {
    static_asset("text/javascript; charset=utf-8", XTERM_JS.as_bytes())
}

async fn fit_addon_js() -> impl IntoResponse {
    static_asset("text/javascript; charset=utf-8", FIT_ADDON_JS.as_bytes())
}

async fn ghostty_terminal_js() -> impl IntoResponse {
    static_asset(
        "text/javascript; charset=utf-8",
        GHOSTTY_TERMINAL_JS.as_bytes(),
    )
}

async fn ghostty_wasm() -> impl IntoResponse {
    static_asset("application/wasm", GHOSTTY_WASM)
}

fn static_asset(content_type: &'static str, body: &'static [u8]) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, content_type)], body)
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> HttpResponse {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

#[derive(Serialize)]
struct ProjectView {
    id: i64,
    name: String,
    path: String,
}

impl From<Project> for ProjectView {
    fn from(project: Project) -> Self {
        Self {
            id: project.id,
            name: project.name,
            path: project.path.to_string_lossy().into_owned(),
        }
    }
}

#[derive(Serialize)]
struct SessionView {
    id: String,
    label: String,
    slot: Option<String>,
    program: Option<String>,
    claim: Option<String>,
    title: Option<String>,
    state: &'static str,
    detail: Option<String>,
    pid: Option<u32>,
    /// The durable catalog row backing this view, when one exists.
    catalog_id: Option<i64>,
    provider_session_id: Option<String>,
    last_activity_ms: Option<i64>,
    /// False for catalog-only history: no live PTY exists, so the terminal
    /// route must never be called for these rows.
    attachable: bool,
}

impl SessionView {
    fn new(status: Status) -> Self {
        let (label, slot, program) = session_label(&status.id);
        let (state, detail) = match &status.lifecycle {
            Lifecycle::Running => ("running", None),
            Lifecycle::Exited(info) => ("exited", Some(format!("exit {}", info.code))),
            Lifecycle::Failed(message) => ("failed", Some(message.clone())),
        };
        let claim = if state == "running" {
            status.pid.and_then(crate::programs::launch::radar_agent_of)
        } else {
            None
        };
        Self {
            id: status.id,
            label,
            slot,
            program,
            claim,
            title: status.title,
            state,
            detail,
            pid: status.pid,
            catalog_id: None,
            provider_session_id: None,
            last_activity_ms: None,
            // Every daemon-status row has (or had) a live PTY: running rows
            // attach interactively, retained ended rows attach read-only.
            attachable: true,
        }
    }

    /// A catalog-only history row: the conversation survives, the process
    /// does not. Never attachable.
    fn history(entry: &crate::session::catalog::Entry) -> Self {
        let (state, detail) = match entry.lifecycle.as_str() {
            "failed" => ("failed", Some("the run failed".to_string())),
            _ => ("ended", None),
        };
        Self {
            id: format!("catalog-{}", entry.id),
            label: entry
                .title
                .clone()
                .unwrap_or_else(|| format!("{} session", entry.provider)),
            slot: None,
            program: Some(entry.provider.clone()),
            title: entry.title.clone(),
            state,
            detail,
            pid: None,
            catalog_id: Some(entry.id),
            provider_session_id: Some(entry.provider_session_id.clone()),
            last_activity_ms: Some(entry.last_activity_at),
            claim: None,
            attachable: false,
        }
    }
}

/// Live daemon statuses enriched with their catalog rows, plus catalog-only
/// history — one list, newest activity first. This is the shared shape both
/// the native sidebar and the browser render from.
fn merge_catalog_sessions(
    statuses: Vec<Status>,
    entries: Vec<crate::session::catalog::Entry>,
) -> Vec<SessionView> {
    let live_ids: std::collections::HashSet<String> =
        statuses.iter().map(|status| status.id.clone()).collect();
    let mut rows: Vec<SessionView> = statuses
        .into_iter()
        .map(|status| {
            let mut view = SessionView::new(status);
            if let Some(entry) = entries
                .iter()
                .find(|entry| entry.radar_session_id.as_deref() == Some(view.id.as_str()))
            {
                view.catalog_id = Some(entry.id);
                view.provider_session_id = Some(entry.provider_session_id.clone());
                view.last_activity_ms = Some(entry.last_activity_at);
            }
            view
        })
        .collect();
    for entry in &entries {
        if entry
            .radar_session_id
            .as_ref()
            .is_some_and(|radar_id| live_ids.contains(radar_id))
        {
            continue;
        }
        if entry.radar_session_id.is_some() && entry.lifecycle == "running" {
            // Reconcile has not caught up with this id yet; skip until then.
            continue;
        }
        rows.push(SessionView::history(entry));
    }
    rows.sort_by(|a, b| {
        b.last_activity_ms
            .unwrap_or(0)
            .cmp(&a.last_activity_ms.unwrap_or(0))
    });
    rows
}

async fn projects(State(state): State<WebState>) -> ApiResult<Json<Vec<ProjectView>>> {
    let db = open_db(&state.home)?;
    let projects = db
        .projects()
        .map_err(ApiError::internal)?
        .into_iter()
        .map(ProjectView::from)
        .collect();
    Ok(Json(projects))
}

async fn sessions(
    RoutePath(project_id): RoutePath<i64>,
    State(state): State<WebState>,
) -> ApiResult<Json<Vec<SessionView>>> {
    let project = require_project(&state.home, project_id)?;
    let root = canonical_project_root(&project);
    let statuses: Vec<Status> = list_sessions(&state.home)?
        .into_iter()
        .filter(|status| session_belongs_to_project(status, project_id, &root))
        .collect();
    // One catalog query also reconciles lifecycle and refreshes provider
    // history (throttled server-side), so the browser list stays truthful.
    let entries = match Client::request(
        &state.home,
        Command::CatalogList {
            projects: vec![daemon::CatalogProject {
                id: project.id,
                path: project.path.clone(),
            }],
            filter: CatalogFilter::All,
            query: None,
            limit: 200,
        },
    )
    .map_err(ApiError::internal)?
    {
        Response::Catalog(entries) => entries,
        _ => Vec::new(),
    };
    Ok(Json(merge_catalog_sessions(statuses, entries)))
}

async fn create_shell(
    RoutePath(project_id): RoutePath<i64>,
    State(state): State<WebState>,
    headers: HeaderMap,
) -> ApiResult<Json<SessionView>> {
    require_same_origin(&headers)?;
    let project = require_project(&state.home, project_id)?;
    if !project.path.is_dir() {
        return Err(ApiError::bad_request("project directory is missing"));
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let session_id = format!(
        "web-project-{project_id}-shell-{}-{now:x}",
        std::process::id()
    );
    let shell = config::login_shell();
    let spawn = Spawn {
        id: session_id.clone(),
        argv: vec![shell, "-l".to_string()],
        cwd: project.path.clone(),
        env: vec![
            ("RADAR_HOME".to_string(), state.home.display().to_string()),
            ("RADAR_PROJECT_ID".to_string(), project_id.to_string()),
            (
                "RADAR_PROJECT_ROOT".to_string(),
                project.path.to_string_lossy().into_owned(),
            ),
            ("RADAR_SESSION_ID".to_string(), session_id),
        ],
        env_remove: Vec::new(),
        dims: Dims {
            cols: 100,
            rows: 30,
        },
    };
    let response =
        Client::request(&state.home, Command::Create(spawn)).map_err(ApiError::internal)?;
    match response {
        Response::Status(status) => Ok(Json(SessionView::new(status))),
        _ => Err(ApiError::internal(
            "daemon returned an unexpected create response",
        )),
    }
}

async fn activity(
    RoutePath(project_id): RoutePath<i64>,
    State(state): State<WebState>,
) -> ApiResult<Json<ActivitySnapshot>> {
    require_project(&state.home, project_id)?;
    match Client::request(
        &state.home,
        Command::ActivitySnapshot {
            project_id,
            after_sequence: None,
            limit: 100,
        },
    )
    .map_err(ApiError::internal)?
    {
        Response::ActivitySnapshot(snapshot) => Ok(Json(snapshot)),
        _ => Err(ApiError::internal(
            "daemon returned an unexpected activity response",
        )),
    }
}

async fn board_snapshot(
    RoutePath(project_id): RoutePath<i64>,
    State(state): State<WebState>,
) -> ApiResult<Json<Option<BoardState>>> {
    Ok(Json(board_for(&state.home, project_id)?))
}

/// The project's board if its board is enabled, `None` otherwise. Shared by the
/// read route and every card mutation, so a mutation returns the refreshed
/// board in one trip.
fn board_for(home: &Path, project_id: i64) -> ApiResult<Option<BoardState>> {
    let db = open_db(home)?;
    let Some(project) = db.project(project_id).map_err(ApiError::internal)? else {
        return Ok(None);
    };
    if !db
        .project_settings(project.id)
        .map_err(ApiError::internal)?
        .board_enabled
    {
        return Ok(None);
    }
    // The board lives in the daemon's store, not a file.
    match Client::request(home, Command::BoardState { project_id }).map_err(ApiError::internal)? {
        Response::BoardState(board) => Ok(Some(board)),
        _ => Err(ApiError::internal(
            "daemon returned an unexpected board response",
        )),
    }
}

#[derive(Deserialize)]
struct NewCard {
    title: String,
    #[serde(default)]
    lane: Option<String>,
}

/// Add a to-do from the browser — the human's way in, mirroring the native
/// Home input. Returns the refreshed board so the client renders in one trip.
async fn add_card(
    RoutePath(project_id): RoutePath<i64>,
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(card): Json<NewCard>,
) -> ApiResult<Json<Option<BoardState>>> {
    require_same_origin(&headers)?;
    let db = open_db(&state.home)?;
    let Some(project) = db.project(project_id).map_err(ApiError::internal)? else {
        return Err(ApiError::not_found("no such project"));
    };
    if !db
        .project_settings(project.id)
        .map_err(ApiError::internal)?
        .board_enabled
    {
        return Err(ApiError::bad_request("this project has no enabled board"));
    }
    let title = card.title.trim();
    if title.is_empty() {
        return Err(ApiError::bad_request("a title is required"));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    match Client::request(
        &state.home,
        Command::CardAdd {
            project_id,
            lane: card.lane,
            title: title.to_string(),
            body: String::new(),
            claim: None,
            command_id: format!("web-{}-{now:x}", std::process::id()),
        },
    )
    .map_err(ApiError::internal)?
    {
        Response::CardChanged(_) => {}
        _ => {
            return Err(ApiError::internal(
                "daemon returned an unexpected card response",
            ))
        }
    }
    Ok(Json(board_for(&state.home, project_id)?))
}

/// One card mutation from the browser: edit, move, claim/release, close or
/// reopen. Revision-checked exactly like the native client, so two writers
/// cannot silently clobber each other; the refreshed board comes back so the
/// client renders in one trip.
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum CardMutation {
    Update {
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        revision: Option<u64>,
    },
    Move {
        lane: String,
        #[serde(default)]
        revision: Option<u64>,
    },
    Claim {
        claim: String,
        #[serde(default)]
        revision: Option<u64>,
    },
    Release {
        #[serde(default)]
        revision: Option<u64>,
    },
    Complete {
        #[serde(default)]
        revision: Option<u64>,
    },
    Reopen {
        #[serde(default)]
        revision: Option<u64>,
    },
    Remove,
}

async fn mutate_card(
    RoutePath((project_id, card_id)): RoutePath<(i64, String)>,
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(mutation): Json<CardMutation>,
) -> ApiResult<Json<Option<BoardState>>> {
    require_same_origin(&headers)?;
    require_project(&state.home, project_id)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let command = |label: &str| format!("web-{label}-{}-{now:x}", std::process::id());
    let home = &state.home;
    match mutation {
        CardMutation::Update {
            title,
            body,
            revision,
        } => {
            let title = title.map(|title| title.trim().to_string());
            if title.as_deref() == Some("") {
                return Err(ApiError::bad_request("a card needs a title"));
            }
            daemon::board_card_update(
                home,
                project_id,
                &card_id,
                title.as_deref(),
                body.as_deref(),
                revision,
                &command("edit"),
            )
            .map_err(ApiError::internal)?;
        }
        CardMutation::Move { lane, revision } => {
            daemon::board_card_move(home, project_id, &card_id, &lane, revision, &command("move"))
                .map_err(ApiError::internal)?;
        }
        CardMutation::Claim { claim, revision } => {
            let claim = claim.trim();
            if claim.is_empty() {
                return Err(ApiError::bad_request("a claim needs a name"));
            }
            daemon::board_card_claim(
                home,
                project_id,
                &card_id,
                Some(claim),
                revision,
                &command("claim"),
            )
            .map_err(ApiError::internal)?;
        }
        CardMutation::Release { revision } => {
            daemon::board_card_claim(
                home,
                project_id,
                &card_id,
                None,
                revision,
                &command("release"),
            )
            .map_err(ApiError::internal)?;
        }
        CardMutation::Complete { revision } => {
            daemon::board_card_complete(home, project_id, &card_id, revision, &command("done"))
                .map_err(ApiError::internal)?;
        }
        CardMutation::Reopen { revision } => {
            daemon::board_card_reopen(home, project_id, &card_id, revision, &command("reopen"))
                .map_err(ApiError::internal)?;
        }
        CardMutation::Remove => {
            daemon::board_card_remove(home, project_id, &card_id, &command("remove"))
                .map_err(ApiError::internal)?;
        }
    }
    Ok(Json(board_for(home, project_id)?))
}

#[derive(Deserialize)]
struct NewComment {
    text: String,
}

/// A human reply on a card. The comment is a durable activity event in the
/// project journal, keyed by the card's stable id — the same thread the native
/// card view and `radar card show` read.
async fn comment_card(
    RoutePath((project_id, card_id)): RoutePath<(i64, String)>,
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(comment): Json<NewComment>,
) -> ApiResult<Json<serde_json::Value>> {
    require_same_origin(&headers)?;
    require_project(&state.home, project_id)?;
    let text = comment.text.trim();
    if text.is_empty() {
        return Err(ApiError::bad_request("a comment needs text"));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    match Client::request(
        &state.home,
        Command::PublishActivity(PublishActivity {
            project_id,
            command_id: format!("web-comment-{}-{now:x}", std::process::id()),
            session_id: None,
            card_id: Some(card_id),
            kind: ActivityKind::Reported,
            payload: ActivityPayload::Message {
                text: text.to_string(),
            },
        }),
    )
    .map_err(ApiError::internal)?
    {
        Response::ActivityPublished(event) => Ok(Json(serde_json::json!({ "event": event }))),
        _ => Err(ApiError::internal(
            "daemon returned an unexpected activity response",
        )),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReplyAction {
    Answer,
    Approve,
    Deny,
    Dismiss,
}

#[derive(Deserialize)]
struct AttentionReply {
    revision: u64,
    #[serde(default)]
    action: Option<ReplyAction>,
    #[serde(default)]
    answer: Option<String>,
    /// A non-answering state change: `seen` or `acknowledge`. Reading a
    /// request must not resolve it, so these stay distinct from `action`.
    #[serde(default)]
    change: Option<String>,
}

async fn respond_attention(
    RoutePath((project_id, request_id)): RoutePath<(i64, String)>,
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(reply): Json<AttentionReply>,
) -> ApiResult<Json<serde_json::Value>> {
    require_same_origin(&headers)?;
    require_project(&state.home, project_id)?;
    let change = match reply.change.as_deref() {
        Some("seen") => AttentionChange::MarkSeen,
        Some("acknowledge") => AttentionChange::Acknowledge,
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "unknown attention change: {other}"
            )))
        }
        None => {
            let action = reply
                .action
                .ok_or_else(|| ApiError::bad_request("an action is required"))?;
            AttentionChange::Respond(match action {
                ReplyAction::Answer => AttentionResponse::Answer(
                    reply
                        .answer
                        .filter(|answer| !answer.trim().is_empty())
                        .ok_or_else(|| ApiError::bad_request("answer text is required"))?,
                ),
                ReplyAction::Approve if reply.answer.is_none() => AttentionResponse::Approve,
                ReplyAction::Deny if reply.answer.is_none() => AttentionResponse::Deny,
                ReplyAction::Dismiss if reply.answer.is_none() => AttentionResponse::Dismiss,
                _ => {
                    return Err(ApiError::bad_request(
                        "answer is only valid for the answer action",
                    ))
                }
            })
        }
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    match Client::request(
        &state.home,
        Command::ChangeAttention(ChangeAttention {
            project_id,
            request_id,
            command_id: format!("web-{}-{now:x}", std::process::id()),
            expected_revision: reply.revision,
            change,
        }),
    )
    .map_err(ApiError::internal)?
    {
        Response::AttentionChanged(result) => Ok(Json(
            serde_json::to_value(result).map_err(ApiError::internal)?,
        )),
        _ => Err(ApiError::internal(
            "daemon returned an unexpected attention response",
        )),
    }
}

async fn attach_terminal(
    RoutePath((project_id, session_id)): RoutePath<(i64, String)>,
    State(state): State<WebState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> ApiResult<impl IntoResponse> {
    require_same_origin(&headers)?;
    if session_id.starts_with("catalog-") {
        return Err(ApiError::bad_request(
            "catalog history has no live terminal to attach to",
        ));
    }
    let project = require_project(&state.home, project_id)?;
    let root = canonical_project_root(&project);
    let session = list_sessions(&state.home)?
        .into_iter()
        .find(|session| session.id == session_id)
        .ok_or_else(|| ApiError::not_found("unknown session"))?;
    if !session_belongs_to_project(&session, project_id, &root) {
        return Err(ApiError::not_found(
            "session does not belong to this project",
        ));
    }
    let read_only = !matches!(&session.lifecycle, Lifecycle::Running);
    let home = state.home.clone();
    Ok(ws.on_upgrade(move |socket| bridge_terminal(socket, home, session_id, read_only)))
}

enum Outbound {
    /// Raw program output (tagged 0 on the wire).
    Bytes(Vec<u8>),
    /// A lossless libghostty-vt snapshot (tagged 1 on the wire).
    Snapshot(Vec<u8>),
    Text(String),
    Close,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TerminalCommand {
    Resize { cols: u16, rows: u16 },
}

async fn bridge_terminal(
    socket: WebSocket,
    home: Arc<PathBuf>,
    session_id: String,
    read_only: bool,
) {
    let (mut sender, mut receiver) = socket.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Outbound>(32);
    let interrupt = Arc::new(Mutex::new(None));
    let thread_interrupt = interrupt.clone();
    let thread_home = home.clone();
    let thread_session_id = session_id.clone();
    let output_thread = std::thread::Builder::new()
        .name(format!("radar-web-output-{session_id}"))
        .spawn(move || {
            let mut client = match Client::connect(
                &thread_home,
                Command::Attach {
                    id: thread_session_id,
                },
            ) {
                Ok(client) => client,
                Err(error) => {
                    let _ = out_tx.blocking_send(Outbound::Text(
                        serde_json::json!({"type":"error", "message":error.to_string()})
                            .to_string(),
                    ));
                    return;
                }
            };
            if let Ok(handle) = client.interrupt_handle() {
                *thread_interrupt.lock().unwrap() = Some(handle);
            }
            match client.receive() {
                Ok(Response::Snapshot(snapshot)) => {
                    if out_tx
                        .blocking_send(Outbound::Snapshot(snapshot.terminal_snapshot.clone()))
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(other) => {
                    let _ = out_tx.blocking_send(Outbound::Text(
                        serde_json::json!({"type":"error", "message":format!("unexpected attach response: {other:?}")})
                            .to_string(),
                    ));
                    return;
                }
                Err(error) => {
                    let _ = out_tx.blocking_send(Outbound::Text(
                        serde_json::json!({"type":"error", "message":error.to_string()})
                            .to_string(),
                    ));
                    return;
                }
            }
            let _ = client.set_read_timeout(None);
            loop {
                match client.receive() {
                    Ok(Response::Output(frame)) => match frame.event {
                        Output::Bytes(bytes) => {
                            if out_tx.blocking_send(Outbound::Bytes(bytes)).is_err() {
                                break;
                            }
                        }
                        Output::Resize(dims) => {
                            let text = serde_json::json!({
                                "type": "resize",
                                "cols": dims.cols,
                                "rows": dims.rows,
                            })
                            .to_string();
                            if out_tx.blocking_send(Outbound::Text(text)).is_err() {
                                break;
                            }
                        }
                        Output::Closed => {
                            let _ = out_tx.blocking_send(Outbound::Text(
                                serde_json::json!({"type":"closed"}).to_string(),
                            ));
                            let _ = out_tx.blocking_send(Outbound::Close);
                            break;
                        }
                    },
                    Ok(Response::ResyncRequired) => {
                        let _ = out_tx.blocking_send(Outbound::Text(
                            serde_json::json!({"type":"resync_required"}).to_string(),
                        ));
                        let _ = out_tx.blocking_send(Outbound::Close);
                        break;
                    }
                    Ok(other) => {
                        let _ = out_tx.blocking_send(Outbound::Text(
                            serde_json::json!({"type":"error", "message":format!("unexpected output response: {other:?}")})
                                .to_string(),
                        ));
                        let _ = out_tx.blocking_send(Outbound::Close);
                        break;
                    }
                    Err(error) => {
                        let _ = out_tx.blocking_send(Outbound::Text(
                            serde_json::json!({"type":"error", "message":error.to_string()})
                                .to_string(),
                        ));
                        let _ = out_tx.blocking_send(Outbound::Close);
                        break;
                    }
                }
            }
        });

    loop {
        tokio::select! {
            frame = out_rx.recv() => {
                match frame {
                    Some(Outbound::Bytes(bytes)) => {
                        let mut tagged = Vec::with_capacity(bytes.len() + 1);
                        tagged.push(0u8);
                        tagged.extend_from_slice(&bytes);
                        if sender.send(Message::Binary(tagged.into())).await.is_err() {
                            break;
                        }
                    }
                    Some(Outbound::Snapshot(bytes)) => {
                        let mut tagged = Vec::with_capacity(bytes.len() + 1);
                        tagged.push(1u8);
                        tagged.extend_from_slice(&bytes);
                        if sender.send(Message::Binary(tagged.into())).await.is_err() {
                            break;
                        }
                    }
                    Some(Outbound::Text(text)) => {
                        if sender.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Some(Outbound::Close) | None => {
                        let _ = sender.send(Message::Close(None)).await;
                        break;
                    }
                }
            }
            incoming = receiver.next() => {
                match incoming {
                    Some(Ok(Message::Binary(bytes))) if !read_only => {
                        let home = home.clone();
                        let id = session_id.clone();
                        let bytes = bytes.to_vec();
                        if let Err(error) = tokio::task::spawn_blocking(move || send_input(&home, &id, bytes)).await.unwrap_or_else(|e| Err(anyhow::anyhow!(e.to_string()))) {
                            let text = serde_json::json!({"type":"error", "message":error.to_string()}).to_string();
                            if sender.send(Message::Text(text.into())).await.is_err() { break; }
                        }
                    }
                    Some(Ok(Message::Binary(_))) => {}
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<TerminalCommand>(text.as_str()) {
                            Ok(TerminalCommand::Resize { cols, rows }) if !read_only => {
                                let home = home.clone();
                                let id = session_id.clone();
                                if let Err(error) = tokio::task::spawn_blocking(move || resize_session(&home, &id, cols, rows)).await.unwrap_or_else(|e| Err(anyhow::anyhow!(e.to_string()))) {
                                    let text = serde_json::json!({"type":"error", "message":error.to_string()}).to_string();
                                    if sender.send(Message::Text(text.into())).await.is_err() { break; }
                                }
                            }
                            Ok(TerminalCommand::Resize { .. }) | Err(_) => {}
                        }
                    }
                    Some(Ok(Message::Ping(bytes))) => {
                        if sender.send(Message::Pong(bytes)).await.is_err() { break; }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                }
            }
        }
    }

    out_rx.close();
    if let Some(handle) = interrupt.lock().unwrap().take() {
        let _ = handle.shutdown(std::net::Shutdown::Both);
    }
    if let Ok(thread) = output_thread {
        let _ = tokio::task::spawn_blocking(move || thread.join()).await;
    }
}

fn send_input(home: &Path, session_id: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
    for chunk in bytes.chunks(8192) {
        match Client::request(
            home,
            Command::Input {
                id: session_id.to_string(),
                bytes: chunk.to_vec(),
            },
        )? {
            Response::Ok => {}
            other => anyhow::bail!("unexpected input response: {other:?}"),
        }
    }
    Ok(())
}

fn resize_session(home: &Path, session_id: &str, cols: u16, rows: u16) -> anyhow::Result<()> {
    match Client::request(
        home,
        Command::Resize {
            id: session_id.to_string(),
            dims: Dims { cols, rows },
        },
    )? {
        Response::Ok => Ok(()),
        other => anyhow::bail!("unexpected resize response: {other:?}"),
    }
}

fn open_db(home: &Path) -> ApiResult<Db> {
    Db::open(&Paths::with_root(home.to_path_buf())).map_err(ApiError::internal)
}

fn require_project(home: &Path, project_id: i64) -> ApiResult<Project> {
    open_db(home)?
        .project(project_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("unknown project"))
}

fn canonical_project_root(project: &Project) -> PathBuf {
    project
        .path
        .canonicalize()
        .unwrap_or_else(|_| project.path.clone())
}

/// A session with a stable project id belongs to exactly that project; only a
/// session without one (a custom shell, say) falls back to its working
/// directory. The id is authoritative, so overlapping project paths cannot
/// list another project's session twice.
fn session_belongs_to_project(status: &Status, project_id: i64, project_root: &Path) -> bool {
    match session_project_id(&status.id) {
        Some(id) => id == project_id,
        None => status.cwd.starts_with(project_root),
    }
}

fn list_sessions(home: &Path) -> ApiResult<Vec<Status>> {
    match Client::request(home, Command::List).map_err(ApiError::internal)? {
        Response::Sessions(sessions) => Ok(sessions),
        _ => Err(ApiError::internal(
            "daemon returned an unexpected list response",
        )),
    }
}

fn session_project_id(session_id: &str) -> Option<i64> {
    let remainder = session_id
        .strip_prefix("project-")
        .or_else(|| session_id.strip_prefix("web-project-"))?;
    let project_id = remainder.split('-').next()?.parse().ok()?;
    (project_id > 0).then_some(project_id)
}

fn session_label(session_id: &str) -> (String, Option<String>, Option<String>) {
    if session_id.starts_with("web-project-") {
        return ("Web shell".to_string(), Some("shell".to_string()), None);
    }
    let parts: Vec<_> = session_id.splitn(5, '-').collect();
    if parts.len() == 5 && parts[0] == "project" {
        let slot = parts[2].to_string();
        let program = parts[4].to_string();
        return (
            format!("{} · {program}", slot.to_ascii_uppercase()),
            Some(slot),
            Some(program),
        );
    }
    ("Session".to_string(), None, None)
}

fn require_same_origin(headers: &HeaderMap) -> ApiResult<()> {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return Err(ApiError {
            status: StatusCode::FORBIDDEN,
            message: "same-origin request required".to_string(),
        });
    };
    let origin_host = origin
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(rest));
    let host_matches = origin_host.is_some_and(|origin_host| {
        [header::HOST.as_str(), "x-forwarded-host"]
            .iter()
            .any(|name| {
                headers
                    .get(*name)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|host| host.eq_ignore_ascii_case(origin_host))
            })
    });
    if host_matches {
        Ok(())
    } else {
        Err(ApiError {
            status: StatusCode::FORBIDDEN,
            message: "request origin does not match this Radar client".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::catalog::Entry;
    use axum::http::HeaderValue;

    fn entry(
        id: i64,
        provider: &str,
        radar_id: Option<&str>,
        lifecycle: &str,
        activity: i64,
    ) -> Entry {
        Entry {
            id,
            project_id: 17,
            provider: provider.to_string(),
            provider_session_id: format!("ses_{id}"),
            radar_session_id: radar_id.map(str::to_string),
            card_id: None,
            source: "provider".to_string(),
            title: Some(format!("Conversation {id}")),
            cwd: PathBuf::from("/work/project"),
            created_at: activity,
            last_activity_at: activity,
            ended_at: None,
            lifecycle: lifecycle.to_string(),
            archived_at: None,
        }
    }

    fn status(id: &str, lifecycle: Lifecycle) -> Status {
        Status {
            id: id.to_string(),
            cwd: PathBuf::from("/work/project"),
            pid: Some(9),
            lifecycle,
            title: Some("Live title".to_string()),
            stream_closed: false,
        }
    }

    fn exited() -> Lifecycle {
        Lifecycle::Exited(crate::session::ExitInfo {
            code: 0,
            signal: None,
        })
    }

    #[test]
    fn merged_sessions_order_by_activity_and_flag_attachability() {
        let statuses = vec![
            status("project-17-agent-0-opencode", Lifecycle::Running),
            status("project-17-agent-1-opencode", exited()),
        ];
        let entries = vec![
            entry(
                1,
                "opencode",
                Some("project-17-agent-0-opencode"),
                "running",
                5_000,
            ),
            entry(2, "opencode", None, "ended", 9_000),
        ];

        let merged = merge_catalog_sessions(statuses, entries);

        assert_eq!(
            merged
                .iter()
                .map(|view| view.id.as_str())
                .collect::<Vec<_>>(),
            [
                "catalog-2",
                "project-17-agent-0-opencode",
                "project-17-agent-1-opencode"
            ],
            "newest catalog activity first, then live rows"
        );
        let live = &merged[1];
        assert!(live.attachable, "a running daemon row is attachable");
        assert_eq!(
            live.catalog_id,
            Some(1),
            "live rows carry their catalog link"
        );
        let retained = &merged[2];
        assert!(
            retained.attachable,
            "a retained ended daemon row stays attachable read-only"
        );
        let history = &merged[0];
        assert!(
            !history.attachable,
            "catalog-only history is never attachable"
        );
        assert_eq!(history.state, "ended");
    }

    #[test]
    fn a_running_catalog_row_without_a_live_session_is_hidden_until_reconciled() {
        let statuses = Vec::new();
        let entries = vec![
            entry(
                3,
                "opencode",
                Some("project-17-agent-9-opencode"),
                "running",
                1_000,
            ),
            entry(4, "claude", None, "ended", 500),
        ];

        let merged = merge_catalog_sessions(statuses, entries);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "catalog-4");
    }

    #[test]
    fn maps_gui_and_web_session_ids_to_projects() {
        assert_eq!(
            session_project_id("project-17-agent-2-claude-code"),
            Some(17)
        );
        assert_eq!(session_project_id("web-project-17-shell-123"), Some(17));
        assert_eq!(session_project_id("project-nope-agent-0-opencode"), None);
        assert_eq!(session_project_id("unrelated"), None);
    }

    #[test]
    fn project_sessions_match_stable_ids_or_working_directory() {
        let make_status = |id: &str, cwd: &str| Status {
            id: id.to_string(),
            cwd: PathBuf::from(cwd),
            pid: Some(1),
            lifecycle: Lifecycle::Running,
            title: None,
            stream_closed: false,
        };
        let root = Path::new("/work/project");

        assert!(session_belongs_to_project(
            &make_status("project-17-agent-0-claude", "/elsewhere"),
            17,
            root,
        ));
        assert!(session_belongs_to_project(
            &make_status("custom-shell", "/work/project/packages/app"),
            17,
            root,
        ));
        assert!(!session_belongs_to_project(
            &make_status("custom-shell", "/work/another-project"),
            17,
            root,
        ));
        // A stable id wins over a working directory inside another project's
        // root, so an overlapping path never lists one session twice.
        assert!(!session_belongs_to_project(
            &make_status("project-17-agent-0-claude", "/work/other-project"),
            42,
            Path::new("/work/other-project"),
        ));
        assert!(session_belongs_to_project(
            &make_status("project-17-agent-0-claude", "/work/other-project"),
            17,
            Path::new("/work/project"),
        ));
    }

    #[test]
    fn status_without_a_working_directory_remains_compatible() {
        let status: Status = serde_json::from_value(serde_json::json!({
            "id": "project-17-agent-0-claude",
            "pid": 1,
            "lifecycle": "Running",
            "title": null,
            "stream_closed": false
        }))
        .unwrap();
        assert!(status.cwd.as_os_str().is_empty());
    }

    #[test]
    fn labels_gui_and_web_sessions() {
        assert_eq!(
            session_label("project-17-agent-2-claude-code"),
            (
                "AGENT · claude-code".to_string(),
                Some("agent".to_string()),
                Some("claude-code".to_string())
            )
        );
        assert_eq!(
            session_label("web-project-17-shell-123"),
            ("Web shell".to_string(), Some("shell".to_string()), None)
        );
    }

    #[test]
    fn card_mutations_parse_from_the_browser_shape() {
        let update: CardMutation = serde_json::from_value(serde_json::json!({
            "action": "update",
            "title": "New title",
            "body": "Body",
            "revision": 4
        }))
        .unwrap();
        match update {
            CardMutation::Update {
                title,
                body,
                revision,
            } => {
                assert_eq!(title.as_deref(), Some("New title"));
                assert_eq!(body.as_deref(), Some("Body"));
                assert_eq!(revision, Some(4));
            }
            _ => panic!("expected an update"),
        }

        let release: CardMutation =
            serde_json::from_value(serde_json::json!({ "action": "release" })).unwrap();
        assert!(matches!(release, CardMutation::Release { revision: None }));

        let complete: CardMutation =
            serde_json::from_value(serde_json::json!({ "action": "complete", "revision": 2 }))
                .unwrap();
        assert!(matches!(
            complete,
            CardMutation::Complete { revision: Some(2) }
        ));

        let move_to: CardMutation =
            serde_json::from_value(serde_json::json!({ "action": "move", "lane": "Review" }))
                .unwrap();
        match move_to {
            CardMutation::Move { lane, revision } => {
                assert_eq!(lane, "Review");
                assert_eq!(revision, None);
            }
            _ => panic!("expected a move"),
        }
    }

    #[test]
    fn attention_replies_accept_seen_and_acknowledge_without_an_action() {
        let seen: AttentionReply = serde_json::from_value(serde_json::json!({
            "revision": 2,
            "change": "seen"
        }))
        .unwrap();
        assert_eq!(seen.change.as_deref(), Some("seen"));
        assert!(seen.action.is_none());

        let answered: AttentionReply = serde_json::from_value(serde_json::json!({
            "revision": 1,
            "action": "answer",
            "answer": "yes"
        }))
        .unwrap();
        assert_eq!(answered.answer.as_deref(), Some("yes"));
        assert!(answered.change.is_none());
    }

    #[test]
    fn same_origin_check_accepts_loopback_and_tailscale_proxy_hosts() {
        let mut local = HeaderMap::new();
        local.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:8787"),
        );
        local.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8787"));
        assert!(require_same_origin(&local).is_ok());

        let mut proxied = HeaderMap::new();
        proxied.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://radar.tailnet.ts.net"),
        );
        proxied.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8787"));
        proxied.insert(
            "x-forwarded-host",
            HeaderValue::from_static("radar.tailnet.ts.net"),
        );
        assert!(require_same_origin(&proxied).is_ok());

        proxied.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        assert!(require_same_origin(&proxied).is_err());
    }
}
