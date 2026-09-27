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
    ActivitySnapshot, AttentionChange, AttentionResponse, ChangeAttention,
};
use crate::session::daemon::{self, Client, Command, Response};
use crate::session::registry::{Lifecycle, Output, Spawn, Status};
use crate::session::Dims;

const INDEX: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");
const XTERM_JS: &[u8] = include_bytes!("../web/vendor/xterm.mjs");
const XTERM_CSS: &[u8] = include_bytes!("../web/vendor/xterm.css");
const FIT_JS: &[u8] = include_bytes!("../web/vendor/fit-addon.mjs");

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
        .route("/assets/xterm.mjs", get(xterm_js))
        .route("/assets/xterm.css", get(xterm_css))
        .route("/assets/fit-addon.mjs", get(fit_js))
        .route("/api/projects", get(projects))
        .route(
            "/api/projects/{project_id}/sessions",
            get(sessions).post(create_shell),
        )
        .route("/api/projects/{project_id}/activity", get(activity))
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
        "tailnet: run `tailscale serve --bg --https=8443 {port}` to add a private HTTPS port"
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

async fn xterm_js() -> impl IntoResponse {
    static_asset("text/javascript; charset=utf-8", XTERM_JS)
}

async fn xterm_css() -> impl IntoResponse {
    static_asset("text/css; charset=utf-8", XTERM_CSS)
}

async fn fit_js() -> impl IntoResponse {
    static_asset("text/javascript; charset=utf-8", FIT_JS)
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
    title: Option<String>,
    state: &'static str,
    detail: Option<String>,
    pid: Option<u32>,
}

impl SessionView {
    fn new(status: Status) -> Self {
        let (label, slot, program) = session_label(&status.id);
        let (state, detail) = match &status.lifecycle {
            Lifecycle::Running => ("running", None),
            Lifecycle::Exited(info) => ("exited", Some(format!("exit {}", info.code))),
            Lifecycle::Failed(message) => ("failed", Some(message.clone())),
        };
        Self {
            id: status.id,
            label,
            slot,
            program,
            title: status.title,
            state,
            detail,
            pid: status.pid,
        }
    }
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
    require_project(&state.home, project_id)?;
    let statuses = list_sessions(&state.home)?;
    Ok(Json(
        statuses
            .into_iter()
            .filter(|status| session_project_id(&status.id) == Some(project_id))
            .map(SessionView::new)
            .collect(),
    ))
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
    action: ReplyAction,
    answer: Option<String>,
}

async fn respond_attention(
    RoutePath((project_id, request_id)): RoutePath<(i64, String)>,
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(reply): Json<AttentionReply>,
) -> ApiResult<Json<serde_json::Value>> {
    require_same_origin(&headers)?;
    require_project(&state.home, project_id)?;
    let response = match reply.action {
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
            change: AttentionChange::Respond(response),
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
    require_project(&state.home, project_id)?;
    if session_project_id(&session_id) != Some(project_id) {
        return Err(ApiError::not_found(
            "session does not belong to this project",
        ));
    }
    let home = state.home.clone();
    Ok(ws.on_upgrade(move |socket| bridge_terminal(socket, home, session_id)))
}

enum Outbound {
    Bytes(Vec<u8>),
    Text(String),
    Close,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TerminalCommand {
    Resize { cols: u16, rows: u16 },
}

async fn bridge_terminal(socket: WebSocket, home: Arc<PathBuf>, session_id: String) {
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
                        .blocking_send(Outbound::Bytes(snapshot.replay.clone()))
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
                        Output::Resize(_) => {}
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
                        if sender.send(Message::Binary(bytes.into())).await.is_err() {
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
                    Some(Ok(Message::Binary(bytes))) => {
                        let home = home.clone();
                        let id = session_id.clone();
                        let bytes = bytes.to_vec();
                        if let Err(error) = tokio::task::spawn_blocking(move || send_input(&home, &id, bytes)).await.unwrap_or_else(|e| Err(anyhow::anyhow!(e.to_string()))) {
                            let text = serde_json::json!({"type":"error", "message":error.to_string()}).to_string();
                            if sender.send(Message::Text(text.into())).await.is_err() { break; }
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<TerminalCommand>(text.as_str()) {
                            Ok(TerminalCommand::Resize { cols, rows }) => {
                                let home = home.clone();
                                let id = session_id.clone();
                                if let Err(error) = tokio::task::spawn_blocking(move || resize_session(&home, &id, cols, rows)).await.unwrap_or_else(|e| Err(anyhow::anyhow!(e.to_string()))) {
                                    let text = serde_json::json!({"type":"error", "message":error.to_string()}).to_string();
                                    if sender.send(Message::Text(text.into())).await.is_err() { break; }
                                }
                            }
                            Err(_) => {}
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
    use axum::http::HeaderValue;

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
