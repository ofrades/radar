//! ACP agent sessions owned by the daemon.
//!
//! radar drives an agent CLI that speaks the [Agent Client Protocol][acp] over
//! stdio — `opencode acp` and any other ACP agent. One daemon-owned worker
//! thread per agent runs the SDK connection on a plain blocking thread (no
//! async runtime): agent updates become project activity, a permission prompt
//! becomes a durable attention request, and the human's response travels back
//! to the agent as the protocol's permission outcome.
//!
//! This is the adapter the board always wanted: instead of inferring "working"
//! from terminal output, radar now learns it from the agent itself.
//!
//! [acp]: https://agentclientprotocol.com/

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, InitializeRequest, NewSessionRequest, PermissionOptionKind,
    PromptRequest, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionConfigOption, SessionModeId, SessionNotification,
    SessionUpdate, SetSessionModeRequest, TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, Client, ConnectionTo};
use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::activity::{
    ActivityJournal, ActivityKind, ActivityPayload, AgentState, AttentionActionKind, AttentionKind,
    AttentionResponse, CreateAttention, PublishActivity,
};

/// Unique-enough idempotency token for the activity this adapter publishes.
static EVENT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One selectable session mode on a running agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMode {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// The agent's active session mode plus everything else it accepts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentModes {
    pub current: String,
    pub available: Vec<AgentMode>,
}

/// One tunable session config option (`session/set_config_option`), as the
/// agent advertised it. Model choice lands here on agents that expose it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfigOption {
    pub id: String,
    pub name: String,
}

/// What an agent said it can do — the ACP initialize handshake, flattened to
/// the flags radar and its clients branch on. Absent until the handshake
/// completes; an agent started moments ago reports none yet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCapabilities {
    pub load_session: bool,
    pub prompt_image: bool,
    pub prompt_audio: bool,
    pub prompt_embedded_context: bool,
    pub mcp_http: bool,
    pub mcp_sse: bool,
    pub session_list: bool,
    pub session_delete: bool,
}

/// Per-driver default ACP args. The catalog never closes the provider set: an
/// unknown program is still startable, it just gets no implicit args.
const DRIVER_DEFAULT_ARGS: &[(&str, &[&str])] = &[("opencode", &["acp"]), ("omp", &["acp"])];

/// Default ACP args for a known driver, or `None` when the program is not in
/// the catalog. Explicit args always win.
pub fn default_acp_args(program: &str) -> Option<Vec<String>> {
    DRIVER_DEFAULT_ARGS
        .iter()
        .find(|(slug, _)| *slug == program)
        .map(|(_, args)| args.iter().map(|arg| (*arg).to_string()).collect())
}

fn next_command_id(agent: &str, what: &str) -> String {
    let sequence = EVENT_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("acp-{agent}-{what}-{sequence}")
}

/// Everything the daemon needs to start one ACP agent session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStart {
    /// Stable daemon-side id for this agent session.
    pub id: String,
    /// Provider id for display and later resume (`opencode`, …).
    pub provider: String,
    /// The agent executable.
    pub program: String,
    /// Arguments for the agent (`["acp", "<project>"]` for opencode).
    pub args: Vec<String>,
    /// The project directory this agent works in.
    pub cwd: PathBuf,
    /// The radar project this agent belongs to; every event carries it.
    pub project_id: i64,
    /// The radar session id, when the agent was launched from a pane.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The board card this agent is working, when it has one.
    #[serde(default)]
    pub card_id: Option<String>,
}

/// What a client can see about one agent session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatus {
    pub id: String,
    pub provider: String,
    pub cwd: PathBuf,
    /// The conversation id the agent assigned, once the session is ready.
    #[serde(default)]
    pub acp_session_id: Option<String>,
    /// starting | ready | working | exited | failed
    pub state: String,
    #[serde(default)]
    pub detail: Option<String>,
    /// Feature matrix from the initialize handshake.
    #[serde(default)]
    pub capabilities: Option<AgentCapabilities>,
    /// Session modes from the agent: the current one plus what else it accepts.
    #[serde(default)]
    pub modes: Option<AgentModes>,
    /// Config options the agent exposes; settable later with set_config_option.
    #[serde(default)]
    pub config_options: Vec<AgentConfigOption>,
}

impl AgentStatus {
    fn new(spec: &AgentStart) -> Self {
        Self {
            id: spec.id.clone(),
            provider: spec.provider.clone(),
            cwd: spec.cwd.clone(),
            acp_session_id: None,
            state: "starting".to_string(),
            detail: None,
            capabilities: None,
            modes: None,
            config_options: Vec::new(),
        }
    }
}

enum AgentCommand {
    Prompt(String),
    /// Switch the session mode; the driver keeps its own id reused per turn.
    SetMode(SessionModeId),
    Cancel,
    Stop,
}

/// A human's decision, or the connection going away before one arrives.
enum PermissionDecision {
    Resolved(AttentionResponse),
    Cancelled,
}

struct AgentEntry {
    status: AgentStatus,
    commands: async_channel::Sender<AgentCommand>,
}

/// One pending permission prompt, keyed by its attention request id.
struct PendingPermission {
    agent_id: String,
    decision: async_channel::Sender<PermissionDecision>,
}

/// Owns every ACP agent the daemon is running. Dropping an agent's entry sends
/// it `Stop`, which closes the connection and terminates the process group.
#[derive(Default)]
pub struct AgentHost {
    agents: Mutex<HashMap<String, AgentEntry>>,
    pending: Mutex<HashMap<String, PendingPermission>>,
}

impl AgentHost {
    /// Start an ACP agent and its worker thread. The returned status is the
    /// `starting` snapshot; readiness and turn state arrive as activity.
    pub fn start(
        self: &Arc<Self>,
        spec: AgentStart,
        journal: Arc<ActivityJournal>,
    ) -> Result<AgentStatus> {
        if spec.id.is_empty() || spec.id.len() > 128 {
            bail!("agent session id must contain 1..128 characters");
        }
        if spec.project_id <= 0 {
            bail!("agent session needs a positive project id");
        }
        // The daemon fills per-driver defaults so callers never repeat the
        // `acp` argument shape; unknown programs just start bare.
        let mut spec = spec;
        if spec.args.is_empty() {
            spec.args = default_acp_args(&spec.program).unwrap_or_default();
        }
        let mut agents = self.agents.lock();
        if let Some(existing) = agents.get(&spec.id) {
            // A live agent keeps its id; a finished one may be replaced.
            if matches!(
                existing.status.state.as_str(),
                "starting" | "ready" | "working"
            ) {
                bail!("agent session already exists");
            }
            agents.remove(&spec.id);
        }
        let (commands, receiver) = async_channel::unbounded();
        let status = AgentStatus::new(&spec);
        agents.insert(
            spec.id.clone(),
            AgentEntry {
                status: status.clone(),
                commands,
            },
        );
        drop(agents);

        let host = Arc::clone(self);
        let thread_spec = spec.clone();
        std::thread::Builder::new()
            .name(format!("acp-{}", spec.id))
            .spawn(move || run_agent(host, thread_spec, journal, receiver))?;
        Ok(status)
    }

    /// Send a user prompt to a ready agent.
    pub fn prompt(&self, id: &str, text: String) -> Result<()> {
        self.send(id, AgentCommand::Prompt(text))
    }

    /// Switch the agent's session mode and wait briefly for the agent's echo
    /// so callers see the new state without re-polling.
    pub fn set_mode_status(&self, id: &str, mode_id: &str) -> Result<AgentModes> {
        self.send(id, AgentCommand::SetMode(SessionModeId::new(mode_id)))?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Some(modes) = self
                .list()
                .into_iter()
                .find(|status| status.id == id)
                .and_then(|status| status.modes)
            {
                if modes.current == mode_id {
                    return Ok(modes);
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        bail!("agent did not confirm session mode {mode_id} in time")
    }

    /// Ask the agent to cancel its current turn.
    pub fn cancel(&self, id: &str) -> Result<()> {
        self.send(id, AgentCommand::Cancel)
    }

    /// Stop an agent: end the connection, unblocking any permission prompt it
    /// is waiting on so its worker thread can exit.
    pub fn stop(&self, id: &str) -> Result<()> {
        self.send(id, AgentCommand::Stop)?;
        self.cancel_pending_for(id);
        Ok(())
    }

    /// Stop every agent. Called on daemon shutdown so each worker can close its
    /// connection and terminate its process group instead of leaking until exit.
    pub fn stop_all(&self) {
        let ids: Vec<String> = self.agents.lock().keys().cloned().collect();
        for id in ids {
            let _ = self.stop(&id);
        }
    }

    pub fn list(&self) -> Vec<AgentStatus> {
        let mut statuses: Vec<_> = self
            .agents
            .lock()
            .values()
            .map(|entry| entry.status.clone())
            .collect();
        statuses.sort_by(|a, b| a.id.cmp(&b.id));
        statuses
    }

    /// Deliver a human's attention response to the permission prompt waiting on
    /// it. Returns whether an agent was actually waiting.
    pub fn resolve(&self, request_id: &str, response: AttentionResponse) -> bool {
        let Some(pending) = self.pending.lock().remove(request_id) else {
            return false;
        };
        pending
            .decision
            .try_send(PermissionDecision::Resolved(response))
            .is_ok()
    }

    fn send(&self, id: &str, command: AgentCommand) -> Result<()> {
        let agents = self.agents.lock();
        let entry = agents.get(id).context("unknown agent session")?;
        entry
            .commands
            .try_send(command)
            .map_err(|error| anyhow::anyhow!("agent session is not accepting commands: {error}"))
    }

    fn set_status(&self, id: &str, state: &str, detail: Option<String>) {
        if let Some(entry) = self.agents.lock().get_mut(id) {
            entry.status.state = state.to_string();
            entry.status.detail = detail;
        }
    }

    fn set_session_id(&self, id: &str, session_id: &str) {
        if let Some(entry) = self.agents.lock().get_mut(id) {
            entry.status.acp_session_id = Some(session_id.to_string());
        }
    }

    fn set_capabilities(&self, id: &str, capabilities: AgentCapabilities) {
        if let Some(entry) = self.agents.lock().get_mut(id) {
            entry.status.capabilities = Some(capabilities);
        }
    }

    /// Swap the mode state wholesale; the agent's snapshot is authoritative.
    fn set_modes(&self, id: &str, modes: AgentModes) {
        if let Some(entry) = self.agents.lock().get_mut(id) {
            entry.status.modes = Some(modes);
        }
    }

    /// Replace the config-option list the agent advertised.
    fn set_config_options(&self, id: &str, options: Vec<AgentConfigOption>) {
        if let Some(entry) = self.agents.lock().get_mut(id) {
            entry.status.config_options = options;
        }
    }

    /// Record the agent's new current mode and return the previous one, so a
    /// mode-change activity report is only published on real changes.
    fn switch_mode(&self, id: &str, mode_id: &str) -> Option<String> {
        let mut agents = self.agents.lock();
        let entry = agents.get_mut(id)?;
        let modes = entry.status.modes.as_mut()?;
        let previous = std::mem::replace(&mut modes.current, mode_id.to_string());
        Some(previous)
    }

    fn register_pending(
        &self,
        request_id: String,
        agent_id: String,
    ) -> async_channel::Receiver<PermissionDecision> {
        let (decision, receiver) = async_channel::bounded(1);
        self.pending
            .lock()
            .insert(request_id, PendingPermission { agent_id, decision });
        receiver
    }

    /// Unblock every permission prompt belonging to a stopped agent.
    fn cancel_pending_for(&self, agent_id: &str) {
        let mut pending = self.pending.lock();
        let doomed: Vec<String> = pending
            .iter()
            .filter(|(_, value)| value.agent_id == agent_id)
            .map(|(key, _)| key.clone())
            .collect();
        for key in doomed {
            if let Some(value) = pending.remove(&key) {
                let _ = value.decision.try_send(PermissionDecision::Cancelled);
            }
        }
    }
}

/// Run one agent to completion on a blocking thread. `futures_lite::block_on`
/// drives the SDK's connection future without pulling an async runtime into the
/// daemon.
fn run_agent(
    host: Arc<AgentHost>,
    spec: AgentStart,
    journal: Arc<ActivityJournal>,
    receiver: async_channel::Receiver<AgentCommand>,
) {
    let outcome = futures_lite::future::block_on(connect(
        Arc::clone(&host),
        spec.clone(),
        Arc::clone(&journal),
        receiver,
    ));
    host.cancel_pending_for(&spec.id);
    match outcome {
        Ok(()) => {
            host.set_status(&spec.id, "exited", None);
            publish_lifecycle(&journal, &spec, "exited", None);
        }
        Err(error) => {
            host.set_status(&spec.id, "failed", Some(error.clone()));
            publish_lifecycle(&journal, &spec, "failed", Some(error));
        }
    }
}

async fn connect(
    host: Arc<AgentHost>,
    spec: AgentStart,
    journal: Arc<ActivityJournal>,
    receiver: async_channel::Receiver<AgentCommand>,
) -> Result<(), String> {
    let config = AcpAgentConfig::new(spec.program.as_str())
        .args(spec.args.clone())
        .env(
            "PATH",
            crate::config::path_value().to_string_lossy().into_owned(),
        );
    let agent = AcpAgent::new(config);

    let notification_journal = Arc::clone(&journal);
    let notification_spec = spec.clone();
    let transcript = Arc::new(Mutex::new(String::new()));
    let notification_transcript = Arc::clone(&transcript);
    let notification_host = Arc::clone(&host);

    let request_host = Arc::clone(&host);
    let request_journal = Arc::clone(&journal);
    let request_spec = spec.clone();

    let connection_journal = Arc::clone(&journal);
    let connection_host = Arc::clone(&host);
    let connection_spec = spec.clone();
    let connection_transcript = Arc::clone(&transcript);

    Client
        .builder()
        .name("radar")
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                observe_update(
                    &notification_journal,
                    &notification_spec,
                    &notification_transcript,
                    &notification_host,
                    notification,
                );
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                let decision =
                    ask_permission(&request_host, &request_journal, &request_spec, &request);
                let outcome = match decision {
                    PermissionDecision::Resolved(AttentionResponse::Approve) => {
                        select_option(&request, true)
                    }
                    PermissionDecision::Resolved(AttentionResponse::Deny) => {
                        select_option(&request, false)
                    }
                    _ => RequestPermissionOutcome::Cancelled,
                };
                responder.respond(RequestPermissionResponse::new(outcome))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
            let initialized = connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            connection_host.set_capabilities(
                &connection_spec.id,
                AgentCapabilities {
                    load_session: initialized.agent_capabilities.load_session,
                    prompt_image: initialized.agent_capabilities.prompt_capabilities.image,
                    prompt_audio: initialized.agent_capabilities.prompt_capabilities.audio,
                    prompt_embedded_context: initialized
                        .agent_capabilities
                        .prompt_capabilities
                        .embedded_context,
                    mcp_http: initialized.agent_capabilities.mcp_capabilities.http,
                    mcp_sse: initialized.agent_capabilities.mcp_capabilities.sse,
                    session_list: initialized
                        .agent_capabilities
                        .session_capabilities
                        .list
                        .is_some(),
                    session_delete: initialized
                        .agent_capabilities
                        .session_capabilities
                        .delete
                        .is_some(),
                },
            );

            let new_session = connection
                .send_request(NewSessionRequest::new(connection_spec.cwd.clone()))
                .block_task()
                .await?;
            let session_id = new_session.session_id;
            connection_host.set_session_id(&connection_spec.id, &session_id.0);
            if let Some(modes) = &new_session.modes {
                connection_host.set_modes(&connection_spec.id, to_agent_modes(modes));
            }
            if let Some(options) = &new_session.config_options {
                connection_host.set_config_options(&connection_spec.id, config_option_ids(options));
            }
            connection_host.set_status(&connection_spec.id, "ready", None);
            publish_state(
                &connection_journal,
                &connection_spec,
                AgentState::Idle,
                Some(format!("session {}", session_id.0)),
            );

            while let Ok(command) = receiver.recv().await {
                match command {
                    AgentCommand::Prompt(text) => {
                        connection_host.set_status(&connection_spec.id, "working", None);
                        publish_state(
                            &connection_journal,
                            &connection_spec,
                            AgentState::Working,
                            None,
                        );
                        let request = PromptRequest::new(
                            session_id.clone(),
                            vec![ContentBlock::Text(TextContent::new(text))],
                        );
                        match connection.send_request(request).block_task().await {
                            Ok(response) => {
                                let message = {
                                    let mut transcript = connection_transcript.lock();
                                    std::mem::take(&mut *transcript)
                                };
                                if !message.trim().is_empty() {
                                    publish_report(&connection_journal, &connection_spec, &message);
                                }
                                connection_host.set_status(&connection_spec.id, "ready", None);
                                publish_state(
                                    &connection_journal,
                                    &connection_spec,
                                    AgentState::Idle,
                                    Some(format!("{:?}", response.stop_reason)),
                                );
                            }
                            Err(error) => {
                                connection_host.set_status(
                                    &connection_spec.id,
                                    "failed",
                                    Some(error.to_string()),
                                );
                                publish_state(
                                    &connection_journal,
                                    &connection_spec,
                                    AgentState::Unknown,
                                    Some(error.to_string()),
                                );
                            }
                        }
                    }
                    AgentCommand::SetMode(mode_id) => {
                        let request =
                            SetSessionModeRequest::new(session_id.clone(), mode_id.clone());
                        match connection.send_request(request).block_task().await {
                            Ok(_) => {
                                // The agent echoes the authoritative mode
                                // through CurrentModeUpdate; nothing to assert
                                // here. A failed request leaves state untouched.
                            }
                            Err(error) => {
                                connection_host.set_status(
                                    &connection_spec.id,
                                    "failed",
                                    Some(format!("mode {}: {error}", mode_id.0)),
                                );
                            }
                        }
                    }
                    AgentCommand::Cancel => {
                        let _ = connection
                            .send_notification(CancelNotification::new(session_id.clone()));
                    }
                    AgentCommand::Stop => break,
                }
            }
            Ok(())
        })
        .await
        .map_err(|error| error.to_string())
}

/// Turn one `session/update` into project activity, keeping the daemon's
/// view of modes and config options aligned with what the agent reports.
fn observe_update(
    journal: &ActivityJournal,
    spec: &AgentStart,
    transcript: &Mutex<String>,
    host: &AgentHost,
    notification: SessionNotification,
) {
    match notification.update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            if let ContentBlock::Text(text) = chunk.content {
                let mut transcript = transcript.lock();
                transcript.push_str(&text.text);
            }
        }
        SessionUpdate::ToolCall(tool) => {
            publish_report(journal, spec, &format!("tool: {}", tool.title));
        }
        SessionUpdate::CurrentModeUpdate(update) => {
            let next = update.current_mode_id.0.to_string();
            let previous = host.switch_mode(&spec.id, &next);
            if previous.as_deref() != Some(next.as_str()) {
                let detail = host
                    .list()
                    .into_iter()
                    .find(|status| status.id == spec.id)
                    .and_then(|status| status.modes)
                    .and_then(|modes| {
                        modes
                            .available
                            .into_iter()
                            .find(|mode| mode.id == next)
                            .map(|mode| mode.name)
                    });
                match detail {
                    Some(name) => publish_report(journal, spec, &format!("mode: {name}")),
                    None => publish_report(journal, spec, &format!("mode: {next}")),
                }
            }
        }
        SessionUpdate::ConfigOptionUpdate(update) => {
            host.set_config_options(&spec.id, config_option_ids(&update.config_options));
        }
        _ => {}
    }
}

/// Create the durable attention request for a permission prompt and block this
/// handler until a human answers it or the connection is stopped. The agent is
/// paused waiting for the decision, so blocking the dispatch loop here is the
/// protocol's own backpressure, not a stall.
fn ask_permission(
    host: &AgentHost,
    journal: &ActivityJournal,
    spec: &AgentStart,
    request: &RequestPermissionRequest,
) -> PermissionDecision {
    let reason = request
        .tool_call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| "Agent requests permission".to_string());
    let command_id = next_command_id(&spec.id, "permission");
    let created = match journal.create_attention(CreateAttention {
        project_id: spec.project_id,
        command_id,
        session_id: spec.session_id.clone(),
        card_id: spec.card_id.clone(),
        kind: AttentionKind::Approval,
        reason,
        allowed_actions: vec![AttentionActionKind::Approve, AttentionActionKind::Deny],
    }) {
        Ok(created) => created,
        Err(error) => {
            // A store failure must not wedge the agent; deny by default.
            eprintln!("radar acp: could not create attention: {error:#}");
            return PermissionDecision::Cancelled;
        }
    };

    let request_id = created.attention.id.clone();
    let receiver = host.register_pending(request_id.clone(), spec.id.clone());

    // Close the race with a response that landed before registration.
    if let Ok(attention) = journal.attention(spec.project_id, &request_id) {
        if let Some(resolution) = attention.resolution {
            host.resolve(&request_id, resolution.clone());
        }
    }

    receiver
        .recv_blocking()
        .unwrap_or(PermissionDecision::Cancelled)
}

/// Map a human decision onto one of the agent's offered permission options.
fn select_option(request: &RequestPermissionRequest, approve: bool) -> RequestPermissionOutcome {
    let wanted = |kind: PermissionOptionKind| {
        matches!(
            (approve, kind),
            (
                true,
                PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
            ) | (
                false,
                PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways
            )
        )
    };
    match request.options.iter().find(|option| wanted(option.kind)) {
        Some(option) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            option.option_id.clone(),
        )),
        None => RequestPermissionOutcome::Cancelled,
    }
}

fn publish_state(
    journal: &ActivityJournal,
    spec: &AgentStart,
    state: AgentState,
    message: Option<String>,
) {
    let _ = journal.publish(PublishActivity {
        project_id: spec.project_id,
        command_id: next_command_id(&spec.id, "state"),
        session_id: spec.session_id.clone(),
        card_id: spec.card_id.clone(),
        kind: ActivityKind::AgentStateChanged,
        payload: ActivityPayload::AgentState { state, message },
    });
}

fn publish_report(journal: &ActivityJournal, spec: &AgentStart, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    let text = truncate(text, 16 * 1024);
    let _ = journal.publish(PublishActivity {
        project_id: spec.project_id,
        command_id: next_command_id(&spec.id, "report"),
        session_id: spec.session_id.clone(),
        card_id: spec.card_id.clone(),
        kind: ActivityKind::Reported,
        payload: ActivityPayload::Message { text },
    });
}

/// Normalize the agent's mode state. The agent is the source of truth, so a
/// mode in `current` that never appeared in `available` is still reported.
fn to_agent_modes(state: &agent_client_protocol::schema::v1::SessionModeState) -> AgentModes {
    let available = state
        .available_modes
        .iter()
        .map(|mode| AgentMode {
            id: mode.id.0.to_string(),
            name: mode.name.clone(),
            description: mode.description.clone(),
        })
        .collect();
    AgentModes {
        current: state.current_mode_id.0.to_string(),
        available,
    }
}

/// Flatten the agent's config options to what clients need.
fn config_option_ids(options: &[SessionConfigOption]) -> Vec<AgentConfigOption> {
    options
        .iter()
        .map(|option| AgentConfigOption {
            id: option.id.0.to_string(),
            name: option.name.clone(),
        })
        .collect()
}

fn publish_lifecycle(
    journal: &ActivityJournal,
    spec: &AgentStart,
    state: &str,
    detail: Option<String>,
) {
    let _ = journal.publish(PublishActivity {
        project_id: spec.project_id,
        command_id: next_command_id(&spec.id, "lifecycle"),
        session_id: spec.session_id.clone(),
        card_id: spec.card_id.clone(),
        kind: ActivityKind::SessionLifecycle,
        payload: ActivityPayload::SessionLifecycle {
            state: state.to_string(),
            detail,
        },
    });
}

/// Truncate on a character boundary; the journal rejects oversized text.
fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_agent_is_an_error_not_a_silent_no_op() {
        let host = AgentHost::default();
        assert!(host.prompt("nobody", "hi".into()).is_err());
        assert!(host.cancel("nobody").is_err());
        assert!(host.list().is_empty());
    }

    #[test]
    fn approving_selects_an_allow_option_and_denying_a_reject_option() {
        let request = RequestPermissionRequest::new(
            "sess",
            agent_client_protocol::schema::v1::ToolCallUpdate::new(
                "call-1",
                agent_client_protocol::schema::v1::ToolCallUpdateFields::new(),
            ),
            vec![
                agent_client_protocol::schema::v1::PermissionOption::new(
                    "once",
                    "Allow once",
                    PermissionOptionKind::AllowOnce,
                ),
                agent_client_protocol::schema::v1::PermissionOption::new(
                    "never",
                    "Reject",
                    PermissionOptionKind::RejectOnce,
                ),
            ],
        );
        assert!(matches!(
            select_option(&request, true),
            RequestPermissionOutcome::Selected(option) if &*option.option_id.0 == "once"
        ));
        assert!(matches!(
            select_option(&request, false),
            RequestPermissionOutcome::Selected(option) if &*option.option_id.0 == "never"
        ));
    }

    #[test]
    fn truncation_never_splits_a_character() {
        let text = "é".repeat(10);
        let cut = truncate(&text, 5);
        assert!(cut.len() <= 5);
        assert!(text.starts_with(&cut));
    }

    #[test]
    fn known_drivers_have_defaults_and_unknown_ones_none() {
        assert_eq!(default_acp_args("opencode").unwrap(), vec!["acp"]);
        assert_eq!(default_acp_args("omp").unwrap(), vec!["acp"]);
        assert_eq!(default_acp_args("unknown-agent"), None);
    }

    #[test]
    fn agent_modes_keep_the_current_mode_even_if_unlisted() {
        let state = agent_client_protocol::schema::v1::SessionModeState::new(
            "beyond".to_string(),
            vec![agent_client_protocol::schema::v1::SessionMode::new(
                "code", "Code",
            )],
        );
        let modes = to_agent_modes(&state);
        assert_eq!(modes.current, "beyond");
        assert_eq!(modes.available.len(), 1);
        assert_eq!(modes.available[0].id, "code");
        assert_eq!(modes.available[0].name, "Code");
        assert_eq!(modes.available[0].description, None);
    }

    #[test]
    fn config_options_flatten_to_id_and_name() {
        use agent_client_protocol::schema::v1::{
            SessionConfigKind, SessionConfigSelect, SessionConfigSelectOption,
            SessionConfigSelectOptions, SessionConfigValueId,
        };
        let options = [SessionConfigOption::new(
            "model",
            "Model",
            SessionConfigKind::Select(SessionConfigSelect::new(
                "gpt-5",
                SessionConfigSelectOptions::Ungrouped(vec![SessionConfigSelectOption::new(
                    SessionConfigValueId::new("gpt-5"),
                    "GPT-5",
                )]),
            )),
        )];
        let flattened = config_option_ids(&options);
        assert_eq!(flattened.len(), 1);
        assert_eq!(flattened[0].id, "model");
        assert_eq!(flattened[0].name, "Model");
    }
}
