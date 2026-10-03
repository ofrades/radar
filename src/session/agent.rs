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
    CancelNotification, ContentBlock, InitializeRequest, ListSessionsRequest, LoadSessionRequest,
    NewSessionRequest, PermissionOptionKind, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionConfigId, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigOptionValue, SessionConfigSelectOptions, SessionConfigValueId, SessionId,
    SessionModeId, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionModeRequest, TextContent,
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

/// One conversation an agent already has, from `session/list`. The cwd and
/// title are the agent's own; the id is what `acp resume` reopens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionInfo {
    pub session_id: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

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

/// One selectable value on a config option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfigValue {
    pub id: String,
    pub name: String,
}

/// One tunable session config option (`session/set_config_option`). Model
/// choice lands here on agents that expose it, alongside effort, thinking,
/// and the agent's own categories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfigOption {
    pub id: String,
    pub name: String,
    /// model | model-config | thought-level | mode | <agent's own>
    pub category: String,
    pub kind: AgentConfigKind,
}

/// What the option can switch to, and what it is at right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentConfigKind {
    Select {
        current: String,
        options: Vec<AgentConfigValue>,
    },
    Boolean {
        current: bool,
    },
}

impl AgentConfigOption {
    /// Human-readable label of the current value, as the agent named it.
    pub fn current_label(&self) -> Option<String> {
        match &self.kind {
            AgentConfigKind::Select { current, options } => options
                .iter()
                .find(|value| &value.id == current)
                .map(|value| value.name.clone()),
            AgentConfigKind::Boolean { current } => Some(if *current {
                "on".to_string()
            } else {
                "off".to_string()
            }),
        }
    }

    /// Turn a caller's string into a protocol value for this option, or `None`
    /// when the value does not fit the option's kind or shape.
    pub fn parse_value(&self, value: &str) -> Option<SessionConfigOptionValue> {
        match &self.kind {
            AgentConfigKind::Select { options, .. } => {
                if options.iter().any(|option| option.id == value) {
                    Some(SessionConfigOptionValue::value_id(
                        SessionConfigValueId::new(value),
                    ))
                } else {
                    None
                }
            }
            AgentConfigKind::Boolean { .. } => match value {
                "true" => Some(SessionConfigOptionValue::boolean(true)),
                "false" => Some(SessionConfigOptionValue::boolean(false)),
                _ => None,
            },
        }
    }
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
    /// An existing conversation to reopen instead of starting a new one.
    #[serde(default)]
    pub acp_session_id: Option<String>,
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
    /// Switch a session config option (model, effort, …).
    SetConfigOption(String, SessionConfigOptionValue),
    /// Ask for the agent's own conversation list; the reply travels back on
    /// the given channel.
    ListSessions(async_channel::Sender<Result<Vec<AgentSessionInfo>, String>>),
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

/// What the daemon wants done when an agent's conversation id is confirmed:
/// the binding that lets a later resume reopen the exact session.
pub type OnSessionId = Arc<dyn Fn(&AgentStart, &str) + Send + Sync>;

/// Owns every ACP agent the daemon is running. Dropping an agent's entry sends
/// it `Stop`, which closes the connection and terminates the process group.
#[derive(Default)]
pub struct AgentHost {
    agents: Mutex<HashMap<String, AgentEntry>>,
    pending: Mutex<HashMap<String, PendingPermission>>,
    on_session_id: Mutex<Option<OnSessionId>>,
}

impl AgentHost {
    /// Install the conversation binding callback. Called once at daemon build;
    /// every later confirmed session id goes through it exactly once.
    pub fn on_session_id(self, callback: OnSessionId) -> Self {
        *self.on_session_id.lock() = Some(callback);
        self
    }

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

    /// Switch one config option (model choice, effort, …). The value is
    /// validated against the option's tracked kind; the confirmation comes
    /// back on the agent's echo, which replaces the daemon state.
    pub fn set_config_option_status(
        self: &Arc<Self>,
        id: &str,
        config_id: &str,
        value: &str,
    ) -> Result<Vec<AgentConfigOption>> {
        let tracked = self
            .config_option(id, config_id)
            .context("agent does not advertise this config option (yet)")?;
        let parsed = tracked.parse_value(value).with_context(|| {
            format!(
                "value {value:?} does not fit {}: {}",
                tracked.name,
                match &tracked.kind {
                    AgentConfigKind::Select { options, .. } => format!(
                        "pick one of: {}",
                        options
                            .iter()
                            .map(|option| option.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    AgentConfigKind::Boolean { .. } => "pick true or false".to_string(),
                }
            )
        })?;
        self.send(
            id,
            AgentCommand::SetConfigOption(config_id.to_string(), parsed),
        )?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if self
                .config_option(id, config_id)
                .is_some_and(|after| agent_config_current_matches(&after, value))
            {
                return Ok(self
                    .list()
                    .into_iter()
                    .find(|status| status.id == id)
                    .map(|status| status.config_options)
                    .unwrap_or_default());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        bail!("agent did not confirm {config_id} = {value} in time")
    }

    /// Ask the agent to cancel its current turn.
    pub fn cancel(&self, id: &str) -> Result<()> {
        self.send(id, AgentCommand::Cancel)
    }

    /// The conversations the agent itself still has, from `session/list`.
    pub fn sessions(&self, id: &str) -> Result<Vec<AgentSessionInfo>> {
        let (sender, receiver) = async_channel::bounded(1);
        self.send(id, AgentCommand::ListSessions(sender))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match receiver.try_recv() {
                Ok(Ok(sessions)) => return Ok(sessions),
                Ok(Err(message)) => bail!("{message}"),
                Err(async_channel::TryRecvError::Closed) => {
                    bail!("agent session stopped while listing its conversations")
                }
                Err(async_channel::TryRecvError::Empty) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(async_channel::TryRecvError::Empty) => {
                    bail!("agent never answered the session list")
                }
            }
        }
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
        match entry.commands.try_send(command) {
            Ok(()) => Ok(()),
            Err(error) if error.is_closed() => {
                let state = entry.status.state.clone();
                bail!("agent session {id} is {state}; it no longer accepts commands")
            }
            Err(error) => bail!("agent session is not accepting commands: {error}"),
        }
    }

    fn set_status(&self, id: &str, state: &str, detail: Option<String>) {
        if let Some(entry) = self.agents.lock().get_mut(id) {
            entry.status.state = state.to_string();
            entry.status.detail = detail;
        }
    }

    fn set_session_id(&self, spec: &AgentStart, session_id: &str) {
        let binder = self.on_session_id.lock().clone();
        if let Some(entry) = self.agents.lock().get_mut(&spec.id) {
            let fresh = entry
                .status
                .acp_session_id
                .as_deref()
                .map(|previous| previous != session_id)
                .unwrap_or(true);
            entry.status.acp_session_id = Some(session_id.to_string());
            if fresh {
                if let Some(binder) = binder.as_ref() {
                    binder(spec, session_id);
                }
            }
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

    /// Merge the agent's config-option snapshot into the tracked state.
    /// Agents may send the full set (ConfigOptionUpdate) or just the options
    /// they changed (a set_config_option response), so unknown ids keep their
    /// previous entry. Returns the options whose current value really
    /// changed, as `(name, label)` pairs — the daemon reports those to the
    /// project feed. Mode-category options are excluded:
    /// `CurrentModeUpdate` already reports mode changes.
    fn set_config_options(
        &self,
        id: &str,
        options: Vec<AgentConfigOption>,
    ) -> Vec<(String, String)> {
        let mut agents = self.agents.lock();
        let Some(entry) = agents.get_mut(id) else {
            return Vec::new();
        };
        let previous = std::mem::take(&mut entry.status.config_options);
        let mut changes = Vec::new();
        let mut update = options.into_iter().peekable();
        let mut merged: Vec<AgentConfigOption> = Vec::with_capacity(previous.len());
        for old in previous {
            let next = match update.peek() {
                Some(incoming) if incoming.id == old.id => update.next().expect("peeked match"),
                _ => {
                    // The agent's snapshot does not mention this option; keep it.
                    merged.push(old);
                    continue;
                }
            };
            if next != old && next.category != "mode" {
                // CurrentModeUpdate already reports mode changes.
                if let Some(label) = next.current_label() {
                    changes.push((next.name.clone(), label));
                }
            }
            merged.push(next);
        }
        merged.extend(update);
        entry.status.config_options = merged;
        changes
    }

    /// Find one tracked config option.
    fn config_option(&self, id: &str, config_id: &str) -> Option<AgentConfigOption> {
        self.agents
            .lock()
            .get(id)
            .and_then(|entry| {
                entry
                    .status
                    .config_options
                    .iter()
                    .find(|o| o.id == config_id)
            })
            .cloned()
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

            // Resume reopens the bound conversation; the agent must have said
            // it can (load_session), or the request fails legibly instead of
            // silently continuing as a different session.
            let (session_id, modes, config_options, opening) =
                if let Some(resume) = connection_spec.acp_session_id.clone() {
                    if initialized.agent_capabilities.load_session {
                        let loaded = connection
                            .send_request(LoadSessionRequest::new(
                                resume.clone(),
                                connection_spec.cwd.clone(),
                            ))
                            .block_task()
                            .await?;
                        (
                            SessionId::new(resume.clone()),
                            loaded.modes,
                            loaded.config_options,
                            "resumed session",
                        )
                    } else {
                        let name = initialized
                            .agent_info
                            .as_ref()
                            .map(|info| info.name.as_str())
                            .unwrap_or("agent");
                        return Err(agent_client_protocol::schema::v1::Error::new(
                            -32601,
                            format!(
                            "{name} does not support session/load; cannot resume session {resume}"
                        )
                            .clone(),
                        ));
                    }
                } else {
                    let created = connection
                        .send_request(NewSessionRequest::new(connection_spec.cwd.clone()))
                        .block_task()
                        .await?;
                    (
                        created.session_id,
                        created.modes,
                        created.config_options,
                        "session",
                    )
                };
            connection_host.set_session_id(&connection_spec, &session_id.0);
            if let Some(modes) = &modes {
                connection_host.set_modes(&connection_spec.id, to_agent_modes(modes));
            }
            if let Some(options) = &config_options {
                connection_host.set_config_options(&connection_spec.id, config_options_of(options));
            }
            connection_host.set_status(&connection_spec.id, "ready", None);
            publish_state(
                &connection_journal,
                &connection_spec,
                AgentState::Idle,
                Some(format!("{opening} {}", session_id.0)),
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
                    AgentCommand::SetConfigOption(config_id, value) => {
                        // The response carries the authoritative updated set.
                        let request = SetSessionConfigOptionRequest::new(
                            session_id.clone(),
                            SessionConfigId::new(config_id.clone()),
                            value,
                        );
                        match connection.send_request(request).block_task().await {
                            Ok(response) => publish_config_changes(
                                &connection_journal,
                                &connection_host,
                                &connection_spec,
                                response.config_options.as_slice(),
                            ),
                            Err(error) => {
                                connection_host.set_status(
                                    &connection_spec.id,
                                    "failed",
                                    Some(format!("config {config_id}: {error}")),
                                );
                            }
                        }
                    }
                    AgentCommand::ListSessions(reply) => {
                        // Agents that cannot list stay legible, not silent.
                        let outcome = match initialized.agent_capabilities.session_capabilities.list
                        {
                            None => Err("agent does not support session/list".to_string()),
                            Some(_) => {
                                match connection
                                    .send_request(
                                        ListSessionsRequest::new()
                                            .cwd(Some(connection_spec.cwd.clone())),
                                    )
                                    .block_task()
                                    .await
                                {
                                    Ok(response) => Ok(response
                                        .sessions
                                        .into_iter()
                                        .map(|info| AgentSessionInfo {
                                            session_id: info.session_id.0.to_string(),
                                            cwd: info.cwd,
                                            title: info.title,
                                            updated_at: info.updated_at,
                                        })
                                        .collect()),
                                    Err(error) => Err(error.to_string()),
                                }
                            }
                        };
                        // Awaiting is what makes the send real: an unawaited
                        // send future is a dropped message and a closed channel.
                        let _ = reply.send(outcome).await;
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
            publish_config_changes(journal, host, spec, &update.config_options);
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

/// Replace the daemon's config-option state with the agent's and feed each
/// real value change to the project feed ("model: Opus").
fn publish_config_changes(
    journal: &ActivityJournal,
    host: &AgentHost,
    spec: &AgentStart,
    options: &[SessionConfigOption],
) {
    for (name, label) in host.set_config_options(&spec.id, config_options_of(options)) {
        publish_report(
            journal,
            spec,
            &format!("{}: {}", name.to_lowercase(), label),
        );
    }
}

/// Whether a tracked config option's current value matches a caller's string.
fn agent_config_current_matches(option: &AgentConfigOption, value: &str) -> bool {
    match &option.kind {
        AgentConfigKind::Select { current, .. } => current == value,
        AgentConfigKind::Boolean { current } => {
            (value == "true" && *current) || (value == "false" && !*current)
        }
    }
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

/// Flatten the agent's config options to what clients need: category as a
/// slug, and the kind resolved to the agent's own current value.
fn config_options_of(options: &[SessionConfigOption]) -> Vec<AgentConfigOption> {
    options
        .iter()
        .filter_map(|option| {
            let kind = match &option.kind {
                SessionConfigKind::Select(select) => {
                    let values: Vec<agent_client_protocol::schema::v1::SessionConfigSelectOption> =
                        match &select.options {
                            SessionConfigSelectOptions::Ungrouped(values) => values.clone(),
                            SessionConfigSelectOptions::Grouped(groups) => groups
                                .iter()
                                .flat_map(|group| group.options.clone())
                                .collect(),
                            // A future kind the daemon cannot represent yet.
                            _ => return None,
                        };
                    AgentConfigKind::Select {
                        current: select.current_value.0.to_string(),
                        options: values
                            .iter()
                            .map(|value| AgentConfigValue {
                                id: value.value.0.to_string(),
                                name: value.name.clone(),
                            })
                            .collect(),
                    }
                }
                SessionConfigKind::Boolean(option) => AgentConfigKind::Boolean {
                    current: option.current_value,
                },
                // A future kind the daemon cannot represent yet.
                _ => return None,
            };
            let category = match option.category.as_ref() {
                Some(SessionConfigOptionCategory::Mode) => "mode".to_string(),
                Some(SessionConfigOptionCategory::Model) => "model".to_string(),
                Some(SessionConfigOptionCategory::ModelConfig) => "model-config".to_string(),
                Some(SessionConfigOptionCategory::ThoughtLevel) => "thought-level".to_string(),
                Some(SessionConfigOptionCategory::Other(unknown)) => unknown.clone(),
                // A future category the daemon cannot name yet.
                None | Some(_) => "other".to_string(),
            };
            Some(AgentConfigOption {
                id: option.id.0.to_string(),
                name: option.name.clone(),
                category,
                kind,
            })
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
    fn config_options_flatten_with_category_kind_and_value_parsing() {
        use agent_client_protocol::schema::v1::{
            SessionConfigKind, SessionConfigSelect, SessionConfigSelectOption,
            SessionConfigSelectOptions, SessionConfigValueId,
        };
        let options = [SessionConfigOption::new(
            "model",
            "Model",
            SessionConfigKind::Select(SessionConfigSelect::new(
                "campo",
                SessionConfigSelectOptions::Ungrouped(vec![
                    SessionConfigSelectOption::new(SessionConfigValueId::new("campo"), "Campo"),
                    SessionConfigSelectOption::new(SessionConfigValueId::new("zeta"), "Zeta"),
                ]),
            )),
        )
        .category(Some(SessionConfigOptionCategory::Model))];
        let flattened = config_options_of(&options);
        assert_eq!(flattened.len(), 1);
        assert_eq!(flattened[0].id, "model");
        assert_eq!(flattened[0].name, "Model");
        assert_eq!(flattened[0].category, "model");
        assert_eq!(flattened[0].current_label().as_deref(), Some("Campo"));
        assert!(flattened[0].parse_value("zeta").is_some());
        assert!(flattened[0].parse_value("nonsense").is_none());
        assert!(flattened[0].parse_value("true").is_none());
    }

    #[test]
    fn config_options_merge_keeps_options_the_snapshot_left_out() {
        let host = AgentHost::default();
        let full = vec![
            AgentConfigOption {
                id: "model".into(),
                name: "Model".into(),
                category: "model".into(),
                kind: AgentConfigKind::Select {
                    current: "a".into(),
                    options: vec![
                        AgentConfigValue {
                            id: "a".into(),
                            name: "Alpha".into(),
                        },
                        AgentConfigValue {
                            id: "b".into(),
                            name: "Beta".into(),
                        },
                    ],
                },
            },
            AgentConfigOption {
                id: "effort".into(),
                name: "Effort".into(),
                category: "thought-level".into(),
                kind: AgentConfigKind::Select {
                    current: "default".into(),
                    options: Vec::new(),
                },
            },
        ];
        // A tracked placeholder agent so the host has somewhere to keep state.
        host.agents.lock().insert(
            "m".into(),
            AgentEntry {
                status: AgentStatus::new(&AgentStart {
                    id: "m".into(),
                    provider: "fake".into(),
                    program: "fake".into(),
                    args: Vec::new(),
                    cwd: "/tmp".into(),
                    project_id: 1,
                    session_id: None,
                    card_id: None,
                    acp_session_id: None,
                }),
                commands: async_channel::unbounded().0,
            },
        );
        host.set_config_options("m", full.clone());
        // A partial snapshot covering only `model`, now on a different value.
        let partial = vec![match &full[0].kind {
            AgentConfigKind::Select { options, .. } => AgentConfigOption {
                id: "model".into(),
                name: "Model".into(),
                category: "model".into(),
                kind: AgentConfigKind::Select {
                    current: "b".into(),
                    options: options.clone(),
                },
            },
            other => unreachable!("model is a select, not {other:?}"),
        }];
        let changes = host.set_config_options("m", partial);
        assert_eq!(changes, vec![("Model".to_string(), "Beta".to_string())]);
        let ids: Vec<String> = host
            .list()
            .into_iter()
            .find(|status| status.id == "m")
            .map(|status| {
                status
                    .config_options
                    .into_iter()
                    .map(|option| option.id)
                    .collect()
            })
            .expect("m tracked");
        assert_eq!(ids, vec!["model", "effort"]);
    }

    #[test]
    fn boolean_config_options_parse_true_and_false_only() {
        use agent_client_protocol::schema::v1::{SessionConfigBoolean, SessionConfigKind};
        let options = [SessionConfigOption::new(
            "thinking",
            "Thinking",
            SessionConfigKind::Boolean(SessionConfigBoolean::new(false)),
        )];
        let flattened = config_options_of(&options);
        assert_eq!(flattened[0].category, "other");
        assert_eq!(flattened[0].current_label().as_deref(), Some("off"));
        assert!(flattened[0].parse_value("true").is_some());
        assert!(flattened[0].parse_value("false").is_some());
        assert!(flattened[0].parse_value("zeta").is_none());
    }
}
