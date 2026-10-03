//! Local process boundary for managed sessions. One length-prefixed JSON request
//! per connection; Attach and Watch turn that connection into a bounded stream.
//! Control and feedback use separate connections from terminal output.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use super::activity::{
    ActivityEvent, ActivityJournal, ActivityKind, ActivityPayload, ActivityReceiveError,
    ActivitySnapshot, ActivitySubscription, Attention, AttentionMutationResult, ChangeAttention,
    CreateAttention, CreateAttentionResult, PublishActivity, WatchResult,
};
use super::agent::{AgentConfigOption, AgentHost, AgentModes, AgentStart, AgentStatus};
use super::beads_store::BeadsBoardStore;
use super::board_store::{Board, BoardChange, BoardState, BoardStore};
use super::catalog::{self, CatalogFilter, SessionCatalog};
use super::registry::{
    Feedback, Lifecycle, Output, ReceiveError, Registry, Sequenced, Snapshot, Spawn, Status,
    Subscription,
};
use super::Dims;

pub const VERSION: u32 = 4;
const MAX_REQUEST: usize = 128 * 1024;
const MAX_RESPONSE: usize = 128 * 1024 * 1024;
/// How long a client or server waits on a socket read. Generous enough for the
/// one slow operation a request can carry — creating a project's Beads
/// workspace (`bd init`, a few seconds) on first board access — while still
/// bounding a hung peer.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the daemon re-reads a provider's own session store per project.
const PROVIDER_IMPORT_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    pub command: Command,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Command {
    Ping,
    Create(Spawn),
    List,
    Attach {
        id: String,
    },
    Watch {
        id: String,
    },
    Input {
        id: String,
        bytes: Vec<u8>,
    },
    Resize {
        id: String,
        dims: Dims,
    },
    Stop {
        id: String,
    },
    Forget {
        id: String,
    },
    /// A worker's harness reports the end of its turn: the driver hands the
    /// card back while the session itself stays alive.
    TurnEnded {
        radar_id: String,
    },
    Shutdown,
    PublishActivity(PublishActivity),
    CreateAttention(CreateAttention),
    ChangeAttention(ChangeAttention),
    AttentionStatus {
        project_id: i64,
        request_id: String,
    },
    ActivitySnapshot {
        project_id: i64,
        after_sequence: Option<u64>,
        limit: usize,
    },
    WatchActivity {
        project_id: i64,
        after_sequence: u64,
    },
    /// One project of a catalog query: its stable id and root directory.
    CatalogList {
        projects: Vec<CatalogProject>,
        filter: CatalogFilter,
        query: Option<String>,
        limit: u32,
    },
    CatalogArchive {
        id: i64,
        archived: bool,
    },
    /// Backfill a live session the daemon has no Create record for (a session
    /// that predates the catalog or a daemon the GUI re-adopted).
    CatalogSeen {
        project_id: i64,
        radar_id: String,
        program: String,
        cwd: PathBuf,
    },
    SessionIdentity {
        radar_id: String,
    },
    /// A provider's actual active conversation, scoped to one worker incarnation.
    SessionIdentify {
        radar_id: String,
        instance: String,
        provider: String,
        conversation: String,
        reporter_pid: u32,
    },
    CardSessionLink {
        project_id: i64,
        card_id: String,
        provider: String,
        provider_session_id: String,
    },
    /// The whole board for a project: its lanes and cards, from the store.
    BoardState {
        project_id: i64,
    },
    CardAdd {
        project_id: i64,
        lane: Option<String>,
        title: String,
        body: String,
        claim: Option<String>,
        command_id: String,
    },
    CardUpdate {
        project_id: i64,
        card_id: String,
        title: Option<String>,
        body: Option<String>,
        expected_revision: Option<u64>,
        command_id: String,
    },
    CardMove {
        project_id: i64,
        card_id: String,
        lane: String,
        expected_revision: Option<u64>,
        command_id: String,
    },
    CardClaim {
        project_id: i64,
        card_id: String,
        claim: Option<String>,
        expected_revision: Option<u64>,
        command_id: String,
    },
    CardComplete {
        project_id: i64,
        card_id: String,
        expected_revision: Option<u64>,
        command_id: String,
    },
    CardReopen {
        project_id: i64,
        card_id: String,
        expected_revision: Option<u64>,
        command_id: String,
    },
    CardRemove {
        project_id: i64,
        card_id: String,
        command_id: String,
    },
    /// Claim the first unclaimed card, optionally within one lane.
    CardNext {
        project_id: i64,
        who: String,
        lane: Option<String>,
        command_id: String,
    },
    /// Start an ACP agent session owned by the daemon.
    AgentStart(AgentStart),
    /// Send a prompt to a running ACP agent.
    AgentPrompt {
        id: String,
        text: String,
    },
    /// Ask an ACP agent to cancel its current turn.
    AgentCancel {
        id: String,
    },
    /// Stop an ACP agent and close its connection.
    AgentStop {
        id: String,
    },
    /// Switch an agent's session mode.
    AgentSetMode {
        id: String,
        mode_id: String,
    },
    /// Switch an agent's session config option (model, effort, …).
    AgentSetConfigOption {
        id: String,
        config_id: String,
        value: String,
    },
    /// The conversations the agent itself still holds (`session/list`).
    AgentSessions {
        id: String,
    },
    /// Every ACP agent the daemon is running.
    AgentList,
}

/// The stored board plus what its reads derive from live facts. Lanes and
/// cards are intent; the derived set is the work's live face, computed at
/// read time and never persisted. Reading it like a stored board (deref)
/// keeps every existing consumer untouched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DerivedBoard {
    #[serde(flatten)]
    pub state: BoardState,
    /// Whose turn each card's loop is in: computed at read time, never
    /// stored. Keyed by card id.
    #[serde(default)]
    pub derived: Vec<crate::session::lane::DerivedCard>,
}

impl std::ops::Deref for DerivedBoard {
    type Target = BoardState;

    fn deref(&self) -> &BoardState {
        &self.state
    }
}

/// A project as the catalog knows it: the client supplies the roster, since
/// projects live in radar's own database, not the daemon's.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogProject {
    pub id: i64,
    pub path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Hello {
        version: u32,
    },
    Ok,
    Error(String),
    Status(Status),
    Sessions(Vec<Status>),
    Snapshot(Box<Snapshot>),
    Watching {
        status: Status,
        sequence: u64,
    },
    Output(Sequenced<Output>),
    Feedback(Sequenced<Feedback>),
    ResyncRequired,
    Activity(ActivityEvent),
    ActivityPublished(ActivityEvent),
    ActivitySnapshot(ActivitySnapshot),
    ActivityWatching {
        after_sequence: u64,
        snapshot: ActivitySnapshot,
    },
    AttentionCreated(CreateAttentionResult),
    AttentionChanged(AttentionMutationResult),
    AttentionStatus(Attention),
    Catalog(Vec<super::catalog::Entry>),
    SessionIdentity {
        provider: String,
        conversation: String,
    },
    /// The stored board plus what its reads derive from live facts.
    BoardState(DerivedBoard),
    CardChanged(Box<BoardChange>),
    CardNext(Option<Box<BoardChange>>),
    AgentStatus(AgentStatus),
    Agents(Vec<AgentStatus>),
    /// The mode change was accepted; the agent's echo confirms the state.
    AgentModes(AgentModes),
    /// The config option was switched; the agent's authoritative set is back.
    AgentConfigOptions(Vec<AgentConfigOption>),
    /// The agent's own conversations.
    AgentSessions(Vec<super::agent::AgentSessionInfo>),
}

/// Socket directory is private even when the surrounding RADAR_HOME is shared.
pub fn socket_path(home: &Path) -> PathBuf {
    home.join("run").join("sessions.sock")
}

pub struct Server {
    listener: UnixListener,
    path: PathBuf,
    _lock: File,
    services: Services,
}

impl Server {
    /// flock serializes startup and stale socket cleanup. Never unlink a live
    /// daemon's socket or stop it merely because its protocol version differs.
    pub fn bind(home: &Path) -> Result<Self> {
        let path = socket_path(home);
        let directory = path.parent().unwrap();
        fs::create_dir_all(directory)?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(directory.join("sessions.lock"))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("session daemon is already running");
        }
        let activity = Arc::new(ActivityJournal::open(&directory.join("activity.sqlite"))?);
        let catalog = Arc::new(SessionCatalog::open(&directory.join("catalog.sqlite"))?);
        // Beads is the board's default store. The old SQLite board is opened
        // only to migrate its cards into the project's Beads workspace, and is
        // the fallback when bd is not installed.
        let legacy = BoardStore::open(&directory.join("board.sqlite"))?;
        let board: Arc<dyn Board> =
            match BeadsBoardStore::open(directory.join("beads"), Some(legacy)) {
                Ok(store) => Arc::new(store),
                Err(error) => {
                    eprintln!("radar: Beads is unavailable ({error}); using the SQLite board");
                    Arc::new(BoardStore::open(&directory.join("board.sqlite"))?)
                }
            };
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(&path).context("bind session socket")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        // ACP agents bind their conversation id into the same claim ->
        // conversation store a TUI session writes, so a resume reopens the
        // exact conversation and a restart refreshes it rather than loses it.
        // The store is keyed by registered project, so a forged project id has
        // nothing to bind to; that stays a loud stderr note, not a silent no-op.
        let binding_paths = crate::config::Paths::with_root(home.to_path_buf());
        let binder: crate::session::agent::OnSessionId =
            Arc::new(move |spec: &AgentStart, session: &str| {
                let db = match crate::db::Db::open(&binding_paths) {
                    Ok(db) => db,
                    Err(error) => {
                        eprintln!("radar acp: session binding: {error:#}");
                        return;
                    }
                };
                let registered = db.projects().ok().is_some_and(|projects| {
                    projects.iter().any(|project| project.id == spec.project_id)
                });
                if !registered {
                    eprintln!(
                        "radar acp: session binding skipped: project {} is not registered in radar",
                        spec.project_id
                    );
                    return;
                }
                if let Err(error) =
                    db.bind_session(spec.project_id, &spec.id, &spec.provider, session)
                {
                    eprintln!("radar acp: session binding: {error:#}");
                }
            });
        Ok(Self {
            listener,
            path,
            _lock: lock,
            services: Services {
                registry: Arc::new(Registry::default()),
                activity,
                catalog,
                board,
                agents: Arc::new(AgentHost::default().on_session_id(binder)),
                imports: Arc::new(Mutex::new(HashMap::new())),
                stopping: Arc::new(AtomicBool::new(false)),
                workers: crate::session::driver::Workers::default(),
                home: home.to_path_buf(),
            },
        })
    }

    pub fn run(self) -> Result<()> {
        let mut workers = Vec::new();
        let driver = {
            let services = self.services.clone();
            std::thread::Builder::new()
                .name("radar-driver".to_string())
                .spawn(move || crate::session::driver::run(services))?
        };
        while !self.services.stopping.load(Ordering::Acquire) {
            workers.retain(|worker: &std::thread::JoinHandle<()>| !worker.is_finished());
            match self.listener.accept() {
                Ok((stream, _)) => {
                    // Local clients can still accidentally flood the daemon.
                    if workers.len() >= 128 {
                        drop(stream);
                        continue;
                    }
                    stream.set_read_timeout(Some(SOCKET_TIMEOUT))?;
                    stream.set_write_timeout(Some(SOCKET_TIMEOUT))?;
                    workers.push(std::thread::spawn({
                        let services = self.services.clone();
                        move || {
                            let mut stream = stream;
                            if let Err(error) = serve(&mut stream, services) {
                                let _ = write_frame(
                                    &mut stream,
                                    &Response::Error(error.to_string()),
                                    MAX_RESPONSE,
                                );
                            }
                        }
                    }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }
        self.services.registry.stop_all();
        self.services.agents.stop_all();
        for worker in workers {
            let _ = worker.join();
        }
        let _ = driver.join();
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.services.stopping.store(true, Ordering::Release);
        self.services.registry.stop_all();
        self.services.agents.stop_all();
        let _ = fs::remove_file(&self.path);
    }
}

/// The daemon's shared domains, cloned per connection so one request cannot
/// block another.
#[derive(Clone)]
pub(crate) struct Services {
    pub(crate) registry: Arc<Registry>,
    pub(crate) activity: Arc<ActivityJournal>,
    pub(crate) catalog: Arc<SessionCatalog>,
    pub(crate) board: Arc<dyn Board>,
    pub(crate) agents: Arc<AgentHost>,
    /// Last provider-history refresh per project root.
    pub(crate) imports: Arc<Mutex<HashMap<PathBuf, Instant>>>,
    pub(crate) stopping: Arc<AtomicBool>,
    /// The driver's card bindings for spawned worker sessions.
    pub(crate) workers: crate::session::driver::Workers,
    /// The daemon's data directory: settings, credentials, the board skill.
    pub(crate) home: PathBuf,
}

fn serve(stream: &mut UnixStream, services: Services) -> Result<()> {
    let request: Request = read_frame(stream, MAX_REQUEST)?;
    if request.version != VERSION {
        bail!(
            "unsupported session protocol version {} (expected {VERSION})",
            request.version
        );
    }
    if services.stopping.load(Ordering::Acquire) {
        bail!("session daemon is shutting down");
    }
    let response = match request.command {
        Command::Ping => Response::Hello { version: VERSION },
        Command::Create(spec) => {
            let _identity_update = services.registry.identity_update();
            if let Some((_, conversation)) = spec
                .env
                .iter()
                .rev()
                .find(|(name, _)| name == "RADAR_RESUME_SESSION_ID")
            {
                let provider = spec
                    .env
                    .iter()
                    .rev()
                    .find(|(name, _)| name == "RADAR_SESSION_PROVIDER")
                    .map(|(_, value)| value.as_str())
                    .context("Resume has no provider identity")?;
                crate::programs::sessions::validate_resume(
                    provider,
                    &spec.cwd,
                    conversation,
                    &spec.env,
                    &spec.argv,
                )?;
            }
            let (session, created) = services.registry.create_or_attach(spec.clone())?;
            let status = session.status();
            if let Some(project_id) = session
                .env_value("RADAR_PROJECT_ID")
                .and_then(|id| id.parse().ok())
                .or_else(|| catalog::project_of_session_id(&status.id))
                .filter(|_| created)
            {
                let provider = session
                    .env_value("RADAR_SESSION_PROVIDER")
                    .map(str::to_string)
                    .or_else(|| catalog::provider_of_session_id(&status.id))
                    .unwrap_or_else(|| "shell".to_string());
                if let Err(error) = services.catalog.record_radar_with_card(
                    project_id,
                    &status.id,
                    &provider,
                    &spec.cwd,
                    catalog::now_millis(),
                    session.card_id(),
                ) {
                    eprintln!("radar session catalog: {error:#}");
                }
                // A card-carrying launch becomes the driver's end-of-turn
                // binding: the worker's session exit hands the card back.
                if let Some(card_id) = session.card_id() {
                    services
                        .workers
                        .note(&status.id, project_id, card_id, spec.cwd.clone());
                }
                // A validated existing target is safe before the provider's first
                // callback (Cursor does not emit sessionStart on exact resume).
                if let Some((_, conversation)) = spec
                    .env
                    .iter()
                    .rev()
                    .find(|(name, _)| name == "RADAR_RESUME_SESSION_ID")
                {
                    services.catalog.bind_provider(
                        &status.id,
                        &provider,
                        conversation,
                        catalog::now_millis(),
                    )?;
                }
            }
            Response::Status(status)
        }
        Command::List => Response::Sessions(services.registry.list()),
        Command::Input { id, bytes } => {
            services.registry.get(&id)?.input(bytes)?;
            Response::Ok
        }
        Command::Resize { id, dims } => {
            services.registry.get(&id)?.resize(dims)?;
            Response::Ok
        }
        Command::Stop { id } => {
            services.registry.get(&id)?.stop();
            Response::Ok
        }
        Command::Forget { id } => {
            services.registry.forget(&id)?;
            Response::Ok
        }
        Command::TurnEnded { radar_id } => {
            services.workers.turn_ended(&radar_id);
            Response::Ok
        }
        Command::Shutdown => {
            services.registry.stop_all();
            services.agents.stop_all();
            services.stopping.store(true, Ordering::Release);
            Response::Ok
        }
        Command::CatalogList {
            projects,
            filter,
            query,
            limit,
        } => {
            let now = catalog::now_millis();
            services.catalog.reconcile(&services.registry.list(), now)?;
            services.catalog.archive_stale(now)?;
            for project in &projects {
                let due = {
                    let mut pending = services.imports.lock();
                    match pending.get(&project.path) {
                        Some(seen) if seen.elapsed() < PROVIDER_IMPORT_INTERVAL => false,
                        _ => {
                            pending.insert(project.path.clone(), Instant::now());
                            true
                        }
                    }
                };
                if due {
                    for provider in crate::programs::agents::SUPPORTED_AGENT_IDS {
                        if let Some(found) = crate::programs::sessions::list_provider_sessions(
                            provider,
                            &project.path,
                        ) {
                            let imported = found
                                .into_iter()
                                .map(|session| catalog::Imported {
                                    id: session.id,
                                    title: session.title,
                                    created_ms: session.created,
                                    last_activity_ms: session.updated.unwrap_or(session.created),
                                })
                                .collect::<Vec<_>>();
                            services.catalog.import_provider(
                                project.id,
                                provider,
                                &project.path,
                                &imported,
                                now,
                            )?;
                        }
                    }
                }
            }
            let ids: Vec<i64> = projects.iter().map(|project| project.id).collect();
            Response::Catalog(services.catalog.list(
                &ids,
                filter,
                query.as_deref(),
                i64::from(limit),
            )?)
        }
        Command::CatalogArchive { id, archived } => {
            services
                .catalog
                .archive(id, archived, catalog::now_millis())?;
            Response::Ok
        }
        Command::CatalogSeen {
            project_id,
            radar_id,
            program,
            cwd,
        } => {
            let session = services.registry.get(&radar_id).ok();
            services.catalog.seen_radar(
                project_id,
                &radar_id,
                &program,
                &cwd,
                catalog::now_millis(),
                session.as_ref().and_then(|session| session.card_id()),
            )?;
            Response::Ok
        }
        Command::SessionIdentity { radar_id } => {
            let (provider, conversation) = services
                .catalog
                .runtime_conversation(&radar_id)?
                .context("The provider has not reported an active conversation")?;
            Response::SessionIdentity {
                provider,
                conversation,
            }
        }
        Command::SessionIdentify {
            radar_id,
            instance,
            provider,
            conversation,
            reporter_pid,
        } => {
            let _identity_update = services.registry.identity_update();
            let session = services.registry.get(&radar_id)?;
            if session.env_value("RADAR_AGENT") != Some(instance.as_str())
                || session.env_value("RADAR_SESSION_PROVIDER") != Some(provider.as_str())
            {
                bail!("Provider identity report does not belong to this worker incarnation");
            }
            if matches!(provider.as_str(), "pi" | "omp")
                && session.status().pid != Some(reporter_pid)
            {
                bail!("A child agent cannot replace its parent's conversation identity");
            }
            if conversation.is_empty()
                || conversation.len() > 256
                || conversation.chars().any(char::is_control)
            {
                bail!("Invalid provider conversation ID");
            }
            services.catalog.bind_provider(
                &radar_id,
                &provider,
                &conversation,
                catalog::now_millis(),
            )?;
            Response::Ok
        }
        Command::CardSessionLink {
            project_id,
            card_id,
            provider,
            provider_session_id,
        } => {
            if !services
                .board
                .state(project_id)?
                .cards
                .iter()
                .any(|card| card.id == card_id)
            {
                bail!("This card is no longer on the board");
            }
            services.catalog.link_conversation(
                project_id,
                &provider,
                &provider_session_id,
                &card_id,
            )?;
            Response::Ok
        }
        Command::BoardState { project_id } => {
            let state = services.board.state(project_id)?;
            let derived = crate::session::lane::derive_board(
                &state,
                &services.workers,
                &services.registry.list(),
                &services.agents.list(),
                &services.activity.unresolved_attention(project_id)?,
            );
            Response::BoardState(DerivedBoard { state, derived })
        }
        Command::CardAdd {
            project_id,
            lane,
            title,
            body,
            claim,
            command_id,
        } => {
            let change = services.board.add_card(
                project_id,
                lane.as_deref(),
                &title,
                &body,
                claim.as_deref(),
            )?;
            publish_board_change(&services.activity, project_id, &command_id, &change)?;
            Response::CardChanged(Box::new(change))
        }
        Command::CardUpdate {
            project_id,
            card_id,
            title,
            body,
            expected_revision,
            command_id,
        } => {
            let change = services.board.update_card(
                project_id,
                &card_id,
                title.as_deref(),
                body.as_deref(),
                expected_revision,
            )?;
            publish_board_change(&services.activity, project_id, &command_id, &change)?;
            Response::CardChanged(Box::new(change))
        }
        Command::CardMove {
            project_id,
            card_id,
            lane,
            expected_revision,
            command_id,
        } => {
            let change =
                services
                    .board
                    .move_card(project_id, &card_id, &lane, expected_revision)?;
            publish_board_change(&services.activity, project_id, &command_id, &change)?;
            Response::CardChanged(Box::new(change))
        }
        Command::CardClaim {
            project_id,
            card_id,
            claim,
            expected_revision,
            command_id,
        } => {
            let change = services.board.claim_card(
                project_id,
                &card_id,
                claim.as_deref(),
                expected_revision,
            )?;
            if let Some(claim) = claim.as_deref() {
                associate_live_card(
                    &services.registry,
                    &services.catalog,
                    project_id,
                    &card_id,
                    claim,
                )?;
            }
            publish_board_change(&services.activity, project_id, &command_id, &change)?;
            Response::CardChanged(Box::new(change))
        }
        Command::CardComplete {
            project_id,
            card_id,
            expected_revision,
            command_id,
        } => {
            let change = services
                .board
                .complete_card(project_id, &card_id, expected_revision)?;
            publish_board_change(&services.activity, project_id, &command_id, &change)?;
            Response::CardChanged(Box::new(change))
        }
        Command::CardReopen {
            project_id,
            card_id,
            expected_revision,
            command_id,
        } => {
            let change = services
                .board
                .reopen_card(project_id, &card_id, expected_revision)?;
            publish_board_change(&services.activity, project_id, &command_id, &change)?;
            Response::CardChanged(Box::new(change))
        }
        Command::CardRemove {
            project_id,
            card_id,
            command_id,
        } => {
            let change = services.board.remove_card(project_id, &card_id)?;
            publish_board_change(&services.activity, project_id, &command_id, &change)?;
            Response::CardChanged(Box::new(change))
        }
        Command::CardNext {
            project_id,
            who,
            lane,
            command_id,
        } => {
            let result = services
                .board
                .next_card(project_id, &who, lane.as_deref())?;
            if let Some(change) = &result {
                associate_live_card(
                    &services.registry,
                    &services.catalog,
                    project_id,
                    &change.card.id,
                    &who,
                )?;
                publish_board_change(&services.activity, project_id, &command_id, change)?;
            }
            Response::CardNext(result.map(Box::new))
        }
        Command::AgentStart(spec) => {
            Response::AgentStatus(services.agents.start(spec, services.activity.clone())?)
        }
        Command::AgentPrompt { id, text } => {
            services.agents.prompt(&id, text)?;
            Response::Ok
        }
        Command::AgentCancel { id } => {
            services.agents.cancel(&id)?;
            Response::Ok
        }
        Command::AgentStop { id } => {
            services.agents.stop(&id)?;
            Response::Ok
        }
        Command::AgentSetMode { id, mode_id } => {
            Response::AgentModes(services.agents.set_mode_status(&id, &mode_id)?)
        }
        Command::AgentSetConfigOption {
            id,
            config_id,
            value,
        } => Response::AgentConfigOptions(
            services
                .agents
                .set_config_option_status(&id, &config_id, &value)?,
        ),
        Command::AgentSessions { id } => Response::AgentSessions(services.agents.sessions(&id)?),
        Command::AgentList => Response::Agents(services.agents.list()),
        Command::PublishActivity(input) => {
            Response::ActivityPublished(services.activity.publish(input)?)
        }
        Command::CreateAttention(input) => {
            Response::AttentionCreated(services.activity.create_attention(input)?)
        }
        Command::ChangeAttention(input) => {
            let request_id = input.request_id.clone();
            let result = services.activity.change_attention(input)?;
            if let Some(response) = result.attention.resolution.clone() {
                services.agents.resolve(&request_id, response);
            }
            Response::AttentionChanged(result)
        }
        Command::AttentionStatus {
            project_id,
            request_id,
        } => Response::AttentionStatus(services.activity.attention(project_id, &request_id)?),
        Command::ActivitySnapshot {
            project_id,
            after_sequence,
            limit,
        } => Response::ActivitySnapshot(services.activity.snapshot(
            project_id,
            after_sequence,
            limit,
        )?),
        Command::WatchActivity {
            project_id,
            after_sequence,
        } => match services.activity.watch(project_id, after_sequence)? {
            WatchResult::Ready(snapshot, subscription) => {
                write_frame(
                    stream,
                    &Response::ActivityWatching {
                        after_sequence,
                        snapshot,
                    },
                    MAX_RESPONSE,
                )?;
                return stream_activity(stream, subscription, services.stopping);
            }
            WatchResult::ResyncRequired => Response::ResyncRequired,
        },
        Command::Attach { id } => {
            let (snapshot, subscription) = services.registry.get(&id)?.attach();
            let closed = snapshot.status.stream_closed;
            write_frame(
                stream,
                &Response::Snapshot(Box::new(snapshot)),
                MAX_RESPONSE,
            )?;
            if closed {
                return Ok(());
            }
            return stream_events(
                stream,
                subscription,
                services.stopping,
                Response::Output,
                |event| matches!(event, Output::Closed),
            );
        }
        Command::Watch { id } => {
            let (status, sequence, subscription) = services.registry.get(&id)?.watch();
            let mut closed = status.stream_closed;
            let mut ended = !matches!(status.lifecycle, Lifecycle::Running);
            write_frame(
                stream,
                &Response::Watching { status, sequence },
                MAX_RESPONSE,
            )?;
            if closed && ended {
                return Ok(());
            }
            return stream_events(
                stream,
                subscription,
                services.stopping,
                Response::Feedback,
                move |event| {
                    match event {
                        Feedback::StreamClosed => closed = true,
                        Feedback::Lifecycle(lifecycle) => {
                            ended = !matches!(lifecycle, Lifecycle::Running)
                        }
                        _ => {}
                    }
                    closed && ended
                },
            );
        }
    };
    write_frame(stream, &response, MAX_RESPONSE)
}

/// Keep a card claimed from inside a live agent attached to that agent's
/// durable session. `card next` is the normal worker workflow, so it must do
/// the same association as the explicit `card claim` command.
fn associate_live_card(
    registry: &Registry,
    catalog: &SessionCatalog,
    project_id: i64,
    card_id: &str,
    claim: &str,
) -> Result<()> {
    for status in registry.list() {
        if catalog::project_of_session_id(&status.id) == Some(project_id)
            && registry
                .get(&status.id)
                .ok()
                .is_some_and(|session| session.agent_id() == Some(claim))
        {
            catalog.associate_card(project_id, &status.id, card_id)?;
        }
    }
    Ok(())
}

/// Publish the `BoardChanged` activity event a card mutation produced, so
/// every watcher — Home, the board pane, the web client — refreshes through
/// the stream it already listens to.
pub(crate) fn publish_board_change(
    activity: &ActivityJournal,
    project_id: i64,
    command_id: &str,
    change: &BoardChange,
) -> Result<()> {
    activity.publish(PublishActivity {
        project_id,
        command_id: command_id.to_string(),
        session_id: None,
        card_id: Some(change.card.id.clone()),
        kind: ActivityKind::BoardChanged,
        payload: ActivityPayload::BoardChanged {
            action: change.action.clone(),
            card_id: Some(change.card.id.clone()),
            title: Some(change.card.title.clone()),
            column: Some(change.card.lane.clone()),
            from_column: change.from_lane.clone(),
        },
    })?;
    Ok(())
}

fn stream_events<T>(
    stream: &mut UnixStream,
    subscription: Subscription<T>,
    stopping: Arc<AtomicBool>,
    wrap: impl Fn(Sequenced<T>) -> Response,
    mut finished: impl FnMut(&T) -> bool,
) -> Result<()> {
    while !stopping.load(Ordering::Acquire) {
        match subscription.try_recv() {
            Ok(event) => {
                let complete = finished(&event.event);
                write_frame(stream, &wrap(event), MAX_RESPONSE)?;
                if complete {
                    return Ok(());
                }
            }
            Err(ReceiveError::Empty) => {
                // Watch sockets must release their subscription on an idle
                // disconnect as well as on the next event.
                let mut byte = [0_u8];
                let n = unsafe {
                    libc::recv(
                        stream.as_raw_fd(),
                        byte.as_mut_ptr().cast(),
                        1,
                        libc::MSG_PEEK | libc::MSG_DONTWAIT,
                    )
                };
                if n == 0 {
                    return Ok(());
                }
                if n > 0 {
                    bail!("stream connections are read-only after attachment");
                }
                let error = std::io::Error::last_os_error();
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) {
                    return Err(error.into());
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(ReceiveError::ResyncRequired) => {
                write_frame(stream, &Response::ResyncRequired, MAX_RESPONSE)?;
                return Ok(());
            }
            Err(ReceiveError::Closed) => return Ok(()),
        }
    }
    Ok(())
}

fn stream_activity(
    stream: &mut UnixStream,
    subscription: ActivitySubscription,
    stopping: Arc<AtomicBool>,
) -> Result<()> {
    while !stopping.load(Ordering::Acquire) {
        match subscription.try_recv() {
            Ok(event) => write_frame(stream, &Response::Activity(event), MAX_RESPONSE)?,
            Err(ActivityReceiveError::ResyncRequired) => {
                write_frame(stream, &Response::ResyncRequired, MAX_RESPONSE)?;
                return Ok(());
            }
            Err(ActivityReceiveError::Closed) => return Ok(()),
            Err(ActivityReceiveError::Empty) => {
                let mut byte = [0_u8];
                let n = unsafe {
                    libc::recv(
                        stream.as_raw_fd(),
                        byte.as_mut_ptr().cast(),
                        1,
                        libc::MSG_PEEK | libc::MSG_DONTWAIT,
                    )
                };
                if n == 0 {
                    return Ok(());
                }
                if n > 0 {
                    bail!("activity watch connections are read-only after attachment");
                }
                let error = std::io::Error::last_os_error();
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) {
                    return Err(error.into());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    Ok(())
}

/// A disconnected socket, truncated frame, sequence gap, or ResyncRequired all
/// require a new attachment. Never continue an old stream after any of them.
pub struct Client {
    stream: UnixStream,
    next_sequence: Option<u64>,
    failed: bool,
}

impl Client {
    pub fn connect(home: &Path, command: Command) -> Result<Self> {
        let mut stream = UnixStream::connect(socket_path(home))?;
        stream.set_read_timeout(Some(SOCKET_TIMEOUT))?;
        stream.set_write_timeout(Some(SOCKET_TIMEOUT))?;
        write_frame(
            &mut stream,
            &Request {
                version: VERSION,
                command,
            },
            MAX_REQUEST,
        )?;
        Ok(Self {
            stream,
            next_sequence: None,
            failed: false,
        })
    }

    pub fn receive(&mut self) -> Result<Response> {
        if self.failed {
            bail!("connection requires a fresh attachment");
        }
        let response = match self.receive_checked() {
            Ok(response) => response,
            Err(error) => {
                self.failed = true;
                let _ = self.stream.shutdown(std::net::Shutdown::Both);
                return Err(error);
            }
        };
        if matches!(response, Response::ResyncRequired) {
            self.failed = true;
        }
        Ok(response)
    }

    fn receive_checked(&mut self) -> Result<Response> {
        let response: Response = read_frame(&mut self.stream, MAX_RESPONSE)?;
        let sequence = match &response {
            Response::Error(message) => bail!("{message}"),
            Response::Snapshot(snapshot) => {
                self.next_sequence = Some(snapshot.sequence + 1);
                None
            }
            Response::Watching { sequence, .. } => {
                self.next_sequence = Some(sequence + 1);
                None
            }
            Response::ActivityWatching {
                after_sequence,
                snapshot,
            } => {
                let mut expected = after_sequence + 1;
                for event in &snapshot.events {
                    if event.sequence != expected {
                        bail!("project activity replay has a sequence gap; resnapshot");
                    }
                    expected += 1;
                }
                if snapshot.watermark + 1 != expected {
                    bail!("project activity replay does not reach its watermark; resnapshot");
                }
                self.next_sequence = Some(expected);
                None
            }
            Response::Output(event) => Some(event.sequence),
            Response::Feedback(event) => Some(event.sequence),
            Response::Activity(event) => Some(event.sequence),
            _ => None,
        };
        if let Some(sequence) = sequence {
            if self.next_sequence != Some(sequence) {
                bail!("session stream sequence gap; attach again");
            }
            self.next_sequence = Some(sequence + 1);
        }
        Ok(response)
    }

    pub fn request(home: &Path, command: Command) -> Result<Response> {
        Self::connect(home, command)?.receive()
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<()> {
        self.stream.set_read_timeout(timeout)?;
        Ok(())
    }

    /// A handle used only to interrupt a blocked receive during client drop.
    pub fn interrupt_handle(&self) -> Result<UnixStream> {
        Ok(self.stream.try_clone()?)
    }
}

/// Ensure the local daemon is running. A detached reaper owns any child handle;
/// dropping the GUI client never kills the daemon or its sessions.
pub fn ensure_running(home: &Path) -> Result<()> {
    if matches!(Client::request(home, Command::Ping), Ok(Response::Hello { version }) if version == VERSION)
    {
        return Ok(());
    }
    let executable = std::env::current_exe().context("locate radar executable")?;
    let child = std::process::Command::new(executable)
        .arg("--home")
        .arg(home)
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("start session daemon")?;
    std::thread::Builder::new()
        .name("radar-daemon-reaper".into())
        .spawn(move || {
            let mut child = child;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(250)),
                }
            }
        })?;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        if matches!(Client::request(home, Command::Ping), Ok(Response::Hello { version }) if version == VERSION)
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    bail!("session daemon did not start within three seconds")
}

fn write_frame(writer: &mut impl Write, value: &impl Serialize, limit: usize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > limit {
        bail!("session frame exceeds {limit} bytes");
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    Ok(())
}

fn read_frame<T: DeserializeOwned>(reader: &mut impl Read, limit: usize) -> Result<T> {
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > limit {
        bail!("session frame exceeds {limit} bytes");
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Client helpers for the board store, so CLI, GUI and guard share one call
/// shape. Each ensures the daemon is running first.
fn board_request(home: &Path, command: Command) -> Result<Response> {
    ensure_running(home)?;
    match Client::request(home, command)? {
        Response::Error(message) => bail!("{message}"),
        response => Ok(response),
    }
}

pub fn board_state(home: &Path, project_id: i64) -> Result<DerivedBoard> {
    match board_request(home, Command::BoardState { project_id })? {
        Response::BoardState(state) => Ok(state),
        other => bail!("unexpected board response: {other:?}"),
    }
}

/// Like [`board_state`], but never starts the daemon. An edit-time gate runs on
/// every tool call and must fail open rather than pay to spawn a daemon; a
/// connection error means "cannot judge", and the caller allows the edit.
pub fn board_state_quick(home: &Path, project_id: i64) -> Result<DerivedBoard> {
    match Client::request(home, Command::BoardState { project_id })? {
        Response::BoardState(state) => Ok(state),
        Response::Error(message) => bail!("{message}"),
        other => bail!("unexpected board response: {other:?}"),
    }
}

pub fn board_card_add(
    home: &Path,
    project_id: i64,
    lane: Option<&str>,
    title: &str,
    body: &str,
    claim: Option<&str>,
    command_id: &str,
) -> Result<BoardChange> {
    match board_request(
        home,
        Command::CardAdd {
            project_id,
            lane: lane.map(str::to_string),
            title: title.to_string(),
            body: body.to_string(),
            claim: claim.map(str::to_string),
            command_id: command_id.to_string(),
        },
    )? {
        Response::CardChanged(change) => Ok(*change),
        other => bail!("unexpected board response: {other:?}"),
    }
}

pub fn board_card_update(
    home: &Path,
    project_id: i64,
    card_id: &str,
    title: Option<&str>,
    body: Option<&str>,
    expected_revision: Option<u64>,
    command_id: &str,
) -> Result<BoardChange> {
    match board_request(
        home,
        Command::CardUpdate {
            project_id,
            card_id: card_id.to_string(),
            title: title.map(str::to_string),
            body: body.map(str::to_string),
            expected_revision,
            command_id: command_id.to_string(),
        },
    )? {
        Response::CardChanged(change) => Ok(*change),
        other => bail!("unexpected board response: {other:?}"),
    }
}

pub fn board_card_move(
    home: &Path,
    project_id: i64,
    card_id: &str,
    lane: &str,
    expected_revision: Option<u64>,
    command_id: &str,
) -> Result<BoardChange> {
    match board_request(
        home,
        Command::CardMove {
            project_id,
            card_id: card_id.to_string(),
            lane: lane.to_string(),
            expected_revision,
            command_id: command_id.to_string(),
        },
    )? {
        Response::CardChanged(change) => Ok(*change),
        other => bail!("unexpected board response: {other:?}"),
    }
}

pub fn board_card_claim(
    home: &Path,
    project_id: i64,
    card_id: &str,
    claim: Option<&str>,
    expected_revision: Option<u64>,
    command_id: &str,
) -> Result<BoardChange> {
    match board_request(
        home,
        Command::CardClaim {
            project_id,
            card_id: card_id.to_string(),
            claim: claim.map(str::to_string),
            expected_revision,
            command_id: command_id.to_string(),
        },
    )? {
        Response::CardChanged(change) => Ok(*change),
        other => bail!("unexpected board response: {other:?}"),
    }
}

pub fn board_card_complete(
    home: &Path,
    project_id: i64,
    card_id: &str,
    expected_revision: Option<u64>,
    command_id: &str,
) -> Result<BoardChange> {
    match board_request(
        home,
        Command::CardComplete {
            project_id,
            card_id: card_id.to_string(),
            expected_revision,
            command_id: command_id.to_string(),
        },
    )? {
        Response::CardChanged(change) => Ok(*change),
        other => bail!("unexpected board response: {other:?}"),
    }
}

pub fn board_card_reopen(
    home: &Path,
    project_id: i64,
    card_id: &str,
    expected_revision: Option<u64>,
    command_id: &str,
) -> Result<BoardChange> {
    match board_request(
        home,
        Command::CardReopen {
            project_id,
            card_id: card_id.to_string(),
            expected_revision,
            command_id: command_id.to_string(),
        },
    )? {
        Response::CardChanged(change) => Ok(*change),
        other => bail!("unexpected board response: {other:?}"),
    }
}

pub fn board_card_remove(
    home: &Path,
    project_id: i64,
    card_id: &str,
    command_id: &str,
) -> Result<BoardChange> {
    match board_request(
        home,
        Command::CardRemove {
            project_id,
            card_id: card_id.to_string(),
            command_id: command_id.to_string(),
        },
    )? {
        Response::CardChanged(change) => Ok(*change),
        other => bail!("unexpected board response: {other:?}"),
    }
}

pub fn board_card_next(
    home: &Path,
    project_id: i64,
    who: &str,
    lane: Option<&str>,
    command_id: &str,
) -> Result<Option<BoardChange>> {
    match board_request(
        home,
        Command::CardNext {
            project_id,
            who: who.to_string(),
            lane: lane.map(str::to_string),
            command_id: command_id.to_string(),
        },
    )? {
        Response::CardNext(change) => Ok(change.map(|change| *change)),
        other => bail!("unexpected board response: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_gap_poisons_client_until_a_new_attachment() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let mut client = Client {
            stream: reader,
            next_sequence: Some(10),
            failed: false,
        };
        write_frame(
            &mut writer,
            &Response::Output(Sequenced {
                sequence: 11,
                event: Output::Bytes(vec![b'x']),
            }),
            MAX_RESPONSE,
        )
        .unwrap();
        assert!(client
            .receive()
            .unwrap_err()
            .to_string()
            .contains("sequence gap"));
        assert!(client
            .receive()
            .unwrap_err()
            .to_string()
            .contains("fresh attachment"));
    }

    #[test]
    fn truncated_frame_poisons_client_instead_of_reusing_partial_data() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let mut client = Client {
            stream: reader,
            next_sequence: None,
            failed: false,
        };
        writer.write_all(&100_u32.to_be_bytes()).unwrap();
        writer.write_all(b"partial").unwrap();
        drop(writer);
        assert!(client.receive().is_err());
        assert!(client
            .receive()
            .unwrap_err()
            .to_string()
            .contains("fresh attachment"));
    }

    #[test]
    fn incompatible_protocol_is_rejected_before_executing_a_command() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        write_frame(
            &mut client,
            &Request {
                version: VERSION + 1,
                command: Command::Shutdown,
            },
            MAX_REQUEST,
        )
        .unwrap();
        let stopping = Arc::new(AtomicBool::new(false));
        let error = serve(
            &mut server,
            Services {
                registry: Arc::new(Registry::default()),
                activity: Arc::new(ActivityJournal::open_in_memory().unwrap()),
                catalog: Arc::new(SessionCatalog::open_in_memory().unwrap()),
                board: Arc::new(BoardStore::open_in_memory().unwrap()),
                agents: Arc::new(AgentHost::default()),
                imports: Arc::new(Mutex::new(HashMap::new())),
                stopping: stopping.clone(),
                workers: crate::session::driver::Workers::default(),
                home: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("unsupported session protocol"));
        assert!(!stopping.load(Ordering::Acquire));
    }

    #[test]
    fn shutdown_closes_registry_before_acknowledging() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        write_frame(
            &mut client,
            &Request {
                version: VERSION,
                command: Command::Shutdown,
            },
            MAX_REQUEST,
        )
        .unwrap();
        let registry = Arc::new(Registry::default());
        serve(
            &mut server,
            Services {
                registry: registry.clone(),
                activity: Arc::new(ActivityJournal::open_in_memory().unwrap()),
                catalog: Arc::new(SessionCatalog::open_in_memory().unwrap()),
                board: Arc::new(BoardStore::open_in_memory().unwrap()),
                agents: Arc::new(AgentHost::default()),
                imports: Arc::new(Mutex::new(HashMap::new())),
                stopping: Arc::new(AtomicBool::new(false)),
                workers: crate::session::driver::Workers::default(),
                home: std::env::temp_dir(),
            },
        )
        .unwrap();
        assert!(matches!(
            read_frame::<Response>(&mut client, MAX_RESPONSE).unwrap(),
            Response::Ok
        ));
        let result = registry.create(Spawn {
            id: "late".into(),
            argv: vec!["/bin/true".into()],
            cwd: std::env::current_dir().unwrap(),
            dims: Dims { cols: 80, rows: 24 },
            env: Vec::new(),
            env_remove: Vec::new(),
        });
        assert!(result.is_err());
        assert!(registry.list().is_empty());
    }

    #[test]
    fn a_card_add_round_trips_and_publishes_a_board_event() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let board = Arc::new(BoardStore::open_in_memory().unwrap());
        let activity = Arc::new(ActivityJournal::open_in_memory().unwrap());
        write_frame(
            &mut client,
            &Request {
                version: VERSION,
                command: Command::CardAdd {
                    project_id: 4,
                    lane: None,
                    title: "Fix login".into(),
                    body: "the 302 loop".into(),
                    claim: None,
                    command_id: "cmd-add-1".into(),
                },
            },
            MAX_REQUEST,
        )
        .unwrap();
        serve(
            &mut server,
            Services {
                registry: Arc::new(Registry::default()),
                activity: activity.clone(),
                catalog: Arc::new(SessionCatalog::open_in_memory().unwrap()),
                board: board.clone(),
                agents: Arc::new(AgentHost::default()),
                imports: Arc::new(Mutex::new(HashMap::new())),
                stopping: Arc::new(AtomicBool::new(false)),
                workers: crate::session::driver::Workers::default(),
                home: std::env::temp_dir(),
            },
        )
        .unwrap();
        let response = read_frame::<Response>(&mut client, MAX_RESPONSE).unwrap();
        let Response::CardChanged(change) = response else {
            panic!("unexpected response: {response:?}");
        };
        assert_eq!(change.action, "added");
        assert_eq!(change.card.title, "Fix login");
        assert_eq!(change.card.lane, "Todo");

        // The mutation is visible in the store and published to the journal,
        // which is how every watcher hears about it.
        let state = board.state(4).unwrap();
        assert_eq!(state.cards.len(), 1);
        let snapshot = activity.snapshot(4, None, 50).unwrap();
        assert!(snapshot.events.iter().any(|event| matches!(
            &event.payload,
            ActivityPayload::BoardChanged { action, column, .. }
                if action == "added" && column.as_deref() == Some("Todo")
        )));
    }
}
