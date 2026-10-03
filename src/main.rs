//! radar — native workspace manager.
//!
//! Run without arguments to open the app. Every subcommand exists so the same
//! state can be inspected, scripted and tested without a GUI.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use radar::config::Paths;
use radar::db::{Db, Preferences, Slot, Tab};
use radar::programs::{self, agents, Kind, LaunchOptions};
use radar::session::board_store::{BoardState, StoredCard};
use radar::{discover, git};

#[derive(Parser, Debug)]
#[command(
    name = "radar",
    version,
    about = "Native workspace manager: projects in a sidebar, tools in tabs",
    long_about = None
)]
struct Cli {
    /// Override the state directory (default: ~/.local/share/radar, or
    /// $RADAR_HOME)
    #[arg(long, global = true)]
    home: Option<PathBuf>,

    /// Machine readable output
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the persistent session daemon (foreground; independent of the GUI)
    Serve,
    /// Serve Radar's responsive browser client on localhost
    Web {
        /// Local HTTP port (Tailscale Serve can expose this to your tailnet)
        #[arg(long, default_value_t = 8787)]
        port: u16,
    },
    /// Control daemon-owned sessions without a GUI
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Publish explicit agent activity or manage human-attention requests
    Activity {
        #[command(subcommand)]
        action: ActivityAction,
    },
    /// Drive an ACP agent (Agent Client Protocol) through the session daemon
    Acp {
        #[command(subcommand)]
        action: AcpAction,
    },
    /// Speak MCP (Model Context Protocol) for the board
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
    /// List projects with their git status
    List,
    /// Add one or more project directories
    Add {
        /// Directories to add. `~` is expanded; `.` works too.
        paths: Vec<PathBuf>,
        /// Add every repository found under this directory (default: ./)
        #[arg(long, conflicts_with = "paths")]
        scan: Option<Option<PathBuf>>,
        /// How deep to scan
        #[arg(long, default_value_t = 2)]
        depth: usize,
        /// Do not select the first added project
        #[arg(long)]
        no_select: bool,
    },
    /// Remove a project from the sidebar (never from disk)
    Remove { path: PathBuf },
    /// Mark a project as opened and show what its tabs resolve to
    Open { path: PathBuf },
    /// Pin or unpin a project
    Pin {
        path: PathBuf,
        #[arg(long)]
        off: bool,
    },
    /// Move a project up or down in the sidebar
    Move {
        path: PathBuf,
        /// Negative moves up
        delta: i64,
    },
    /// Rename a project in the sidebar
    Rename { path: PathBuf, name: String },
    /// Drop projects whose directory has gone away
    Prune,
    /// Show or change the preferred program for a slot
    Prefs {
        /// One of: editor, agent, diff, shell
        slot: Option<String>,
        /// Program id to store; omit to just show
        program: Option<String>,
    },
    /// List agents, what omarchy picked, and what is installed
    Agents,
    /// List every program radar knows about
    Programs {
        /// Filter by kind: editor, agent, diff, shell, tool
        kind: Option<String>,
    },
    /// Fuzzy-find directories that could be added
    Find {
        query: Vec<String>,
        /// Where to search
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long, default_value_t = 2)]
        depth: usize,
        #[arg(long, default_value_t = 40)]
        limit: usize,
    },
    /// Check the environment radar needs
    Doctor,
    /// Show a project's board from radar's store
    Board {
        /// Project directory (default: the current directory)
        path: Option<PathBuf>,
    },
    /// Work with cards on a project's board
    Card {
        #[command(subcommand)]
        action: CardAction,
    },
    /// Wiring for agent-harness hooks (the board as a requirement)
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },
    /// Install radar's board skill into the user's global skills home
    /// (`~/.agents/skills`), which cursor, opencode and omp discover
    Skill,
    /// Install the whole board convention globally: skill, harness edit-gate
    /// plugins, and the git commit-gate dispatcher — nothing in a repository
    Setup,
    /// Open the native app (needs a build with --features gui)
    Gui,
}

#[derive(Subcommand, Debug)]
enum SessionAction {
    /// Create a session, or return the existing session with this ID
    Spawn {
        id: String,
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        #[arg(long, default_value_t = 80)]
        cols: u16,
        #[arg(long, default_value_t = 24)]
        rows: u16,
        /// Extra environment, KEY=VALUE, repeatable (e.g. RADAR_CARD_ID)
        #[arg(long = "env")]
        env: Vec<String>,
        #[arg(required = true, trailing_var_arg = true)]
        argv: Vec<String>,
    },
    /// Read the provider-reported active conversation (not the launch environment).
    Identity {
        #[arg(long)]
        id: Option<String>,
    },
    /// Report the end of this session's turn (called by session hooks)
    TurnEnded,
    /// Report the provider's actual conversation, never a launch-time guess.
    Identify {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        conversation: Option<String>,
        #[arg(long)]
        pid: u32,
    },
    List,
    /// Print an authoritative display snapshot as JSON
    Snapshot {
        id: String,
    },
    /// Snapshot then sequenced raw output frames as JSON (not a terminal renderer)
    Stream {
        id: String,
    },
    /// Status then lifecycle/title/bell feedback, independent of terminal output
    Watch {
        id: String,
    },
    /// Send literal input (use shell quoting for newline/control characters)
    Input {
        id: String,
        text: String,
    },
    Resize {
        id: String,
        cols: u16,
        rows: u16,
    },
    Stop {
        id: String,
    },
    /// Release an ended session's retained screen/history and ID
    Forget {
        id: String,
    },
    /// Stop all sessions and shut down the daemon
    Shutdown,
}

#[derive(Args, Debug, Default)]
struct ActivityIdentity {
    /// Radar project ID (defaults to RADAR_PROJECT_ID in a radar-launched pane)
    #[arg(long)]
    project_id: Option<i64>,
    /// Stable daemon session ID (defaults to RADAR_SESSION_ID)
    #[arg(long)]
    session_id: Option<String>,
    /// Stable card ID (defaults to RADAR_CARD_ID)
    #[arg(long)]
    card_id: Option<String>,
    /// Retry token. Reuse it to make a retry idempotent.
    #[arg(long)]
    command_id: Option<String>,
}

#[derive(Subcommand, Debug)]
enum ActivityAction {
    /// Explicitly report an agent state; state is never inferred from output
    State {
        #[command(flatten)]
        identity: ActivityIdentity,
        #[arg(long, value_enum)]
        state: radar::session::activity::AgentState,
        #[arg(long)]
        message: Option<String>,
    },
    /// Add a human-readable event to the project feed
    Report {
        #[command(flatten)]
        identity: ActivityIdentity,
        text: String,
    },
    /// Create a persistent question, approval, failure, or review request
    Request {
        #[command(flatten)]
        identity: ActivityIdentity,
        #[arg(long, value_enum)]
        kind: radar::session::activity::AttentionKind,
        #[arg(long)]
        reason: String,
        /// Allowed action; repeat for multiple choices
        #[arg(long = "allow", value_enum, required = true)]
        allowed_actions: Vec<radar::session::activity::AttentionActionKind>,
        /// Wait until a human responds, then print the authoritative response
        #[arg(long)]
        wait: bool,
    },
    /// Mark an attention request as seen
    Seen {
        #[command(flatten)]
        identity: ActivityIdentity,
        request_id: String,
        #[arg(long)]
        revision: u64,
    },
    /// Acknowledge an outstanding request without resolving it
    Acknowledge {
        #[command(flatten)]
        identity: ActivityIdentity,
        request_id: String,
        #[arg(long)]
        revision: u64,
    },
    /// Respond to and resolve an outstanding request
    Respond {
        #[command(flatten)]
        identity: ActivityIdentity,
        request_id: String,
        #[arg(long)]
        revision: u64,
        #[arg(long, value_enum)]
        action: ActivityResponseAction,
        #[arg(long)]
        answer: Option<String>,
    },
    /// Read a bounded snapshot of the project feed and unresolved requests
    Snapshot {
        #[arg(long)]
        project_id: Option<i64>,
        #[arg(long)]
        after: Option<u64>,
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// Replay from a project sequence, then stream sequenced live events
    Watch {
        #[arg(long)]
        project_id: Option<i64>,
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
#[value(rename_all = "kebab-case")]
enum ActivityResponseAction {
    Answer,
    Approve,
    Deny,
    Dismiss,
}

#[derive(Subcommand, Debug)]
enum AcpAction {
    /// Start an ACP agent session owned by the daemon
    Start {
        /// Stable id for this agent session
        id: String,
        /// The project directory the agent works in
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        /// The ACP agent program
        #[arg(long, default_value = "opencode")]
        program: String,
        /// An argument for the agent; repeat for several. Defaults to
        /// `acp` (the opencode shape).
        #[arg(long = "arg")]
        args: Vec<String>,
        /// Radar project ID (defaults to RADAR_PROJECT_ID)
        #[arg(long)]
        project_id: Option<i64>,
        /// Radar session ID (defaults to RADAR_SESSION_ID)
        #[arg(long)]
        session_id: Option<String>,
        /// Stable card ID (defaults to RADAR_CARD_ID)
        #[arg(long)]
        card_id: Option<String>,
        /// Reopen an existing conversation (session/load) by its ACP session
        /// id, as `radar acp list --json` reports it
        #[arg(long)]
        resume: Option<String>,
    },
    /// Reopen an agent's bound conversation (its last recorded ACP session)
    Resume {
        /// Stable id for this agent session
        id: String,
        /// The project directory the agent works in
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        /// The ACP agent program; defaults to the bound session's program
        #[arg(long)]
        program: Option<String>,
        /// Radar project ID (defaults to RADAR_PROJECT_ID)
        #[arg(long)]
        project_id: Option<i64>,
        /// Radar session ID (defaults to RADAR_SESSION_ID)
        #[arg(long)]
        session_id: Option<String>,
        /// Stable card ID (defaults to RADAR_CARD_ID)
        #[arg(long)]
        card_id: Option<String>,
    },
    /// Send a prompt to a running agent
    Prompt { id: String, text: String },
    /// Ask a running agent to cancel its current turn
    Cancel { id: String },
    /// Stop a running agent and close its connection
    Stop { id: String },
    /// List a running agent's session modes
    Modes { id: String },
    /// Switch a running agent's session mode
    Mode { id: String, mode_id: String },
    /// Inspect or set an agent's session config options (model, effort, …):
    /// `radar acp config <id>`, `radar acp config <id> <CONFIG_ID>`, or with
    /// a VALUE appended, set it
    Config {
        id: String,
        config_id: Option<String>,
        value: Option<String>,
    },
    /// List the conversations the agent itself still has (ACP session/list)
    Sessions { id: String },
    /// Bind one of the agent's conversations as the resume target, so
    /// `radar acp resume <id>` reopens it
    Adopt {
        id: String,
        /// The ACP session id, as `radar acp sessions` lists it
        session_id: String,
    },
    /// List the agents the daemon is running
    List,
}

#[derive(Subcommand, Debug)]
enum McpAction {
    /// Serve the board as MCP tools over stdio — point an MCP client at
    /// `radar mcp serve` and its agents work the board like the board skill
    /// teaches: claim, report, hand back.
    Serve {
        /// Project directory for the board (default: $RADAR_PROJECT_ROOT,
        /// else the working directory)
        #[arg(long)]
        project: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum CardAction {
    /// Add a card (default column: Todo)
    Add {
        /// Project directory (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
        /// The card's title
        title: String,
        /// Which column to put it in
        #[arg(long)]
        column: Option<String>,
        /// Notes for the card
        #[arg(long)]
        body: Option<String>,
        /// Claim it for this name right away
        #[arg(long)]
        by: Option<String>,
    },
    /// Release a card: drop its claim, nobody is on it
    Release {
        /// Project directory (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
        /// The card's title
        title: String,
    },
    /// Claim a card for a name
    Claim {
        /// Project directory (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
        /// The card's title
        title: String,
        /// Who is claiming it
        #[arg(long)]
        by: String,
    },
    /// Move a card to a column
    Move {
        /// Project directory (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
        /// The card's title
        title: String,
        /// Destination column
        #[arg(long)]
        to: String,
    },
    /// Mark a card done: checked, and moved to the last column
    Done {
        /// Project directory (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
        /// The card's title
        title: String,
    },
    /// Claim the first unclaimed card and print it — how an agent asks for work
    Next {
        /// Project directory (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
        /// Who is asking
        #[arg(long)]
        by: String,
        /// Only look in this column — how a reviewer picks up review work
        #[arg(long = "in")]
        in_column: Option<String>,
    },
    /// Show a card and its conversation thread (by id or title)
    Show {
        /// Project directory (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
        /// Stable card ID or its title
        card: String,
    },
    /// Post a message on a card's thread — how an agent reports to the human
    Comment {
        /// Project directory (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
        /// Stable card ID or its title
        card: String,
        /// The message
        text: String,
        /// Stable daemon session ID (defaults to RADAR_SESSION_ID)
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Edit a card's title and/or notes
    Edit {
        /// Project directory (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
        /// Stable card ID or its title
        card: String,
        /// New title
        #[arg(long)]
        title: Option<String>,
        /// New notes; repeat for multiple lines
        #[arg(long)]
        body: Option<String>,
    },
    /// Link the session you are running in to a card, claiming it for you.
    /// Reads RADAR_AGENT from the pane, so it needs no name.
    Associate {
        /// Project directory (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
        /// Stable card ID or its title
        card: String,
    },
    /// Dispatch a worker agent for a card. Idempotent: a card already claimed
    /// is left alone.
    Start {
        /// Project directory (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
        /// Stable card ID or its title
        card: String,
    },
}

#[derive(Subcommand, Debug)]
enum HookAction {
    /// Judge one edit or commit the way an agent harness's hook would: deny
    /// (exit 2) unless radar shows a live claim by $RADAR_AGENT. The tool
    /// call's JSON is read from stdin when piped, so the same command serves a
    /// hook and a human checking by hand.
    Guard {
        /// The file the agent wants to edit (default: from the hook's stdin
        /// JSON, `tool_input.file_path`)
        #[arg(long)]
        file: Option<PathBuf>,
        /// Judge a commit, not an edit: the git pre-commit hook's question,
        /// which needs no file — only the claim
        #[arg(long)]
        commit: bool,
        /// The project directory (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

fn session_command(paths: &Paths, action: SessionAction) -> Result<()> {
    use radar::session::daemon::{Client, Command as Request, Response};
    use radar::session::registry::{Feedback, Lifecycle, Output, Spawn};
    use radar::session::Dims;
    let streaming = matches!(
        action,
        SessionAction::Stream { .. } | SessionAction::Watch { .. }
    );
    let request = match action {
        SessionAction::Spawn {
            id,
            cwd,
            cols,
            rows,
            env,
            argv,
        } => {
            let pairs: Result<Vec<(String, String)>, anyhow::Error> = env
                .iter()
                .map(|pair| {
                    let (name, value) = pair
                        .split_once('=')
                        .with_context(|| format!("--env {pair} is not KEY=VALUE"))?;
                    Ok((name.to_string(), value.to_string()))
                })
                .collect();
            Request::Create(Spawn {
                id,
                cwd: cwd.canonicalize()?,
                argv,
                dims: Dims { cols, rows },
                env: pairs?,
                env_remove: Vec::new(),
            })
        }
        SessionAction::Identity { id } => Request::SessionIdentity {
            radar_id: id
                .or_else(|| std::env::var("RADAR_SESSION_ID").ok())
                .context("No Radar runtime identity")?,
        },
        SessionAction::TurnEnded => Request::TurnEnded {
            radar_id: std::env::var("RADAR_SESSION_ID").context("No Radar runtime identity")?,
        },
        SessionAction::Identify {
            provider,
            conversation,
            pid,
        } => {
            let instance = std::env::var("RADAR_AGENT").context("No Radar worker identity")?;
            let radar_id =
                std::env::var("RADAR_SESSION_ID").context("No Radar runtime identity")?;
            let conversation = match conversation {
                Some(id) => id,
                None => {
                    let value: serde_json::Value = serde_json::from_reader(std::io::stdin())?;
                    if value.get("parent_conversation_id").is_some() {
                        println!("{{}}");
                        return Ok(());
                    }
                    value["conversation_id"]
                        .as_str()
                        .context("Hook has no conversation ID")?
                        .to_string()
                }
            };
            match Client::request(
                &paths.data_dir,
                Request::SessionIdentify {
                    radar_id,
                    instance: instance.clone(),
                    provider: provider.clone(),
                    conversation: conversation.clone(),
                    reporter_pid: pid,
                },
            )? {
                Response::Ok => {}
                other => anyhow::bail!("Could not record provider identity: {other:?}"),
            }
            let project: i64 = std::env::var("RADAR_PROJECT_ID")
                .context("No Radar project identity")?
                .parse()?;
            radar::db::Db::open(paths)?.bind_session(
                project,
                &instance,
                &provider,
                &conversation,
            )?;
            println!("{{}}");
            return Ok(());
        }
        SessionAction::List => Request::List,
        SessionAction::Snapshot { id } | SessionAction::Stream { id } => Request::Attach { id },
        SessionAction::Watch { id } => Request::Watch { id },
        SessionAction::Input { id, text } => Request::Input {
            id,
            bytes: text.into_bytes(),
        },
        SessionAction::Resize { id, cols, rows } => Request::Resize {
            id,
            dims: Dims { cols, rows },
        },
        SessionAction::Stop { id } => Request::Stop { id },
        SessionAction::Forget { id } => Request::Forget { id },
        SessionAction::Shutdown => Request::Shutdown,
    };
    let mut client = Client::connect(&paths.data_dir, request)?;
    let mut stream_closed = false;
    let mut process_ended = false;
    loop {
        let response = client.receive()?;
        let resync = matches!(response, Response::ResyncRequired);
        let complete = match &response {
            Response::Snapshot(snapshot) => snapshot.status.stream_closed,
            Response::Output(event) => matches!(event.event, Output::Closed),
            Response::Watching { status, .. } => {
                stream_closed = status.stream_closed;
                process_ended = !matches!(status.lifecycle, Lifecycle::Running);
                stream_closed && process_ended
            }
            Response::Feedback(event) => {
                match &event.event {
                    Feedback::StreamClosed => stream_closed = true,
                    Feedback::Lifecycle(lifecycle) => {
                        process_ended = !matches!(lifecycle, Lifecycle::Running)
                    }
                    _ => {}
                }
                stream_closed && process_ended
            }
            _ => false,
        };
        println!("{}", serde_json::to_string(&response)?);
        if resync {
            anyhow::bail!("stream lagged; attach again for a fresh snapshot");
        }
        if !streaming || complete {
            break;
        }
        client.set_read_timeout(None)?;
    }
    Ok(())
}

fn activity_command(paths: &Paths, action: ActivityAction) -> Result<()> {
    use radar::session::activity::{
        ActivityKind, ActivityPayload, AttentionChange, AttentionResponse, ChangeAttention,
        CreateAttention, PublishActivity,
    };
    use radar::session::daemon::{self, Client, Command as Request, Response};

    daemon::ensure_running(&paths.data_dir)?;
    let project_id = |explicit: Option<i64>| -> Result<i64> {
        explicit
            .or_else(|| std::env::var("RADAR_PROJECT_ID").ok()?.parse().ok())
            .context("project ID is required (use --project-id or launch from a radar project)")
    };
    let command_id = |explicit: Option<String>| {
        explicit.unwrap_or_else(|| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();
            format!("cli-{}-{now:x}", std::process::id())
        })
    };
    let session_id =
        |explicit: Option<String>| explicit.or_else(|| std::env::var("RADAR_SESSION_ID").ok());
    let card_id =
        |explicit: Option<String>| explicit.or_else(|| std::env::var("RADAR_CARD_ID").ok());

    let mut wait_for_response = None;
    let request = match action {
        ActivityAction::State {
            identity,
            state,
            message,
        } => Request::PublishActivity(PublishActivity {
            project_id: project_id(identity.project_id)?,
            command_id: command_id(identity.command_id),
            session_id: session_id(identity.session_id),
            card_id: card_id(identity.card_id),
            kind: ActivityKind::AgentStateChanged,
            payload: ActivityPayload::AgentState { state, message },
        }),
        ActivityAction::Report { identity, text } => Request::PublishActivity(PublishActivity {
            project_id: project_id(identity.project_id)?,
            command_id: command_id(identity.command_id),
            session_id: session_id(identity.session_id),
            card_id: card_id(identity.card_id),
            kind: ActivityKind::Reported,
            payload: ActivityPayload::Message { text },
        }),
        ActivityAction::Request {
            identity,
            kind,
            reason,
            allowed_actions,
            wait,
        } => {
            let project_id = project_id(identity.project_id)?;
            if wait {
                wait_for_response = Some(project_id);
            }
            Request::CreateAttention(CreateAttention {
                project_id,
                command_id: command_id(identity.command_id),
                session_id: session_id(identity.session_id),
                card_id: card_id(identity.card_id),
                kind,
                reason,
                allowed_actions,
            })
        }
        ActivityAction::Seen {
            identity,
            request_id,
            revision,
        } => Request::ChangeAttention(ChangeAttention {
            project_id: project_id(identity.project_id)?,
            request_id,
            command_id: command_id(identity.command_id),
            expected_revision: revision,
            change: AttentionChange::MarkSeen,
        }),
        ActivityAction::Acknowledge {
            identity,
            request_id,
            revision,
        } => Request::ChangeAttention(ChangeAttention {
            project_id: project_id(identity.project_id)?,
            request_id,
            command_id: command_id(identity.command_id),
            expected_revision: revision,
            change: AttentionChange::Acknowledge,
        }),
        ActivityAction::Respond {
            identity,
            request_id,
            revision,
            action,
            answer,
        } => {
            let response = match action {
                ActivityResponseAction::Answer => AttentionResponse::Answer(
                    answer.context("--answer is required with --action answer")?,
                ),
                ActivityResponseAction::Approve if answer.is_none() => AttentionResponse::Approve,
                ActivityResponseAction::Deny if answer.is_none() => AttentionResponse::Deny,
                ActivityResponseAction::Dismiss if answer.is_none() => AttentionResponse::Dismiss,
                _ => anyhow::bail!("--answer is only valid with --action answer"),
            };
            Request::ChangeAttention(ChangeAttention {
                project_id: project_id(identity.project_id)?,
                request_id,
                command_id: command_id(identity.command_id),
                expected_revision: revision,
                change: AttentionChange::Respond(response),
            })
        }
        ActivityAction::Snapshot {
            project_id: explicit,
            after,
            limit,
        } => Request::ActivitySnapshot {
            project_id: project_id(explicit)?,
            after_sequence: after,
            limit,
        },
        ActivityAction::Watch {
            project_id: explicit,
            after,
        } => {
            let mut client = Client::connect(
                &paths.data_dir,
                Request::WatchActivity {
                    project_id: project_id(explicit)?,
                    after_sequence: after,
                },
            )?;
            let initial = client.receive()?;
            println!("{}", serde_json::to_string(&initial)?);
            if matches!(initial, Response::ResyncRequired) {
                anyhow::bail!("activity history is too old; take a fresh snapshot")
            }
            client.set_read_timeout(None)?;
            loop {
                let response = client.receive()?;
                let resync = matches!(response, Response::ResyncRequired);
                println!("{}", serde_json::to_string(&response)?);
                if resync {
                    anyhow::bail!("activity stream lagged; resnapshot and reconnect")
                }
            }
        }
    };

    let response = Client::request(&paths.data_dir, request)?;
    println!("{}", serde_json::to_string(&response)?);
    if let (Some(project_id), Response::AttentionCreated(created)) = (wait_for_response, &response)
    {
        use std::io::Write;
        std::io::stdout().flush()?;
        let attention = wait_for_attention(
            &paths.data_dir,
            project_id,
            &created.attention.id,
            created.event.sequence,
        )?;
        println!(
            "{}",
            serde_json::to_string(&Response::AttentionStatus(attention))?
        );
    }
    Ok(())
}

/// Drive an ACP agent through the daemon: start, prompt, cancel, stop, list.
fn acp_command(paths: &Paths, action: AcpAction) -> Result<()> {
    use radar::session::agent::AgentStart;
    use radar::session::daemon::Response;
    use radar::session::daemon::{self, Client, Command as Request};

    /// The current mode state of one running agent, or a legible failure.
    fn current_modes(home: &Path, id: &str) -> Result<radar::session::agent::AgentModes> {
        match Client::request(home, Request::AgentList)? {
            Response::Agents(agents) => agents
                .into_iter()
                .find(|agent| agent.id == id)
                .and_then(|agent| agent.modes)
                .context("agent has no modes (not ready yet, or the agent offers none)"),
            other => anyhow::bail!("unexpected daemon response: {other:?}"),
        }
    }

    /// The config options of one agent — all of them, or just one, whichever
    /// the caller narrowed with `config_id`.
    fn current_config_options(
        home: &Path,
        id: &str,
        config_id: Option<String>,
    ) -> Result<serde_json::Value> {
        match Client::request(home, Request::AgentList)? {
            Response::Agents(agents) => {
                let agent = agents
                    .into_iter()
                    .find(|agent| agent.id == id)
                    .context("agent session is not running")?;
                match config_id {
                    None => serde_json::to_value(&agent.config_options)
                        .context("config options serialize"),
                    Some(config_id) => {
                        let option = agent
                            .config_options
                            .iter()
                            .find(|option| option.id == config_id)
                            .context("agent does not advertise this config option")?;
                        Ok(serde_json::to_value(option)?)
                    }
                }
            }
            other => anyhow::bail!("unexpected daemon response: {other:?}"),
        }
    }

    daemon::ensure_running(&paths.data_dir)?;
    let default_project_id = || -> Result<i64> {
        std::env::var("RADAR_PROJECT_ID")
            .ok()
            .and_then(|value| value.parse().ok())
            .context("project ID is required (use --project-id or launch from a radar project)")
    };
    let request = match action {
        AcpAction::Start {
            id,
            cwd,
            program,
            args,
            project_id,
            session_id,
            card_id,
            resume,
        } => {
            let cwd = cwd.canonicalize()?;
            let project_id = match project_id {
                Some(id) => id,
                None => default_project_id()?,
            };
            // Known drivers get their default ACP args; explicit `--arg`s win.
            // The project directory never lands on the command line: it travels
            // in `NewSessionRequest`.
            let args = if args.is_empty() {
                radar::session::agent::default_acp_args(&program).unwrap_or_default()
            } else {
                args
            };
            Request::AgentStart(AgentStart {
                id,
                provider: program.clone(),
                program,
                args,
                cwd,
                project_id,
                session_id: session_id.or_else(|| std::env::var("RADAR_SESSION_ID").ok()),
                card_id: card_id.or_else(|| std::env::var("RADAR_CARD_ID").ok()),
                acp_session_id: resume,
            })
        }
        AcpAction::Resume {
            id,
            cwd,
            program,
            project_id,
            session_id,
            card_id,
        } => {
            let project_id = match project_id {
                Some(id) => id,
                None => default_project_id()?,
            };
            // The bound conversation names its program; resuming it under a
            // different program would be a different agent's session.
            use radar::config::Paths;
            let db = radar::db::Db::open(&Paths::with_root(paths.data_dir.clone()))?;
            let (bound_program, conversation) = db.bound_session(project_id, &id)?.context(
                "no bound conversation for this agent id yet; start it once and let it answer",
            )?;
            let conflicting = program
                .as_ref()
                .is_some_and(|explicit| explicit != &bound_program);
            if conflicting {
                anyhow::bail!(
                    "the bound conversation for {id} belongs to {bound_program}, not {}",
                    program.expect("conflicting program is set"),
                );
            }
            let program = program.unwrap_or(bound_program);
            Request::AgentStart(AgentStart {
                id,
                provider: program.clone(),
                program,
                args: Vec::new(),
                cwd: cwd.canonicalize()?,
                project_id,
                session_id: session_id.or_else(|| std::env::var("RADAR_SESSION_ID").ok()),
                card_id: card_id.or_else(|| std::env::var("RADAR_CARD_ID").ok()),
                acp_session_id: Some(conversation),
            })
        }
        AcpAction::Prompt { id, text } => Request::AgentPrompt { id, text },
        AcpAction::Cancel { id } => Request::AgentCancel { id },
        AcpAction::Stop { id } => Request::AgentStop { id },
        // Modes, Mode, and Config read the agent space directly and print on
        // their own; they need no generic request/response plumbing.
        AcpAction::Modes { id } => {
            println!(
                "{}",
                serde_json::to_string(&current_modes(&paths.data_dir, &id)?)?
            );
            return Ok(());
        }
        AcpAction::Mode { id, mode_id } => {
            match Client::request(&paths.data_dir, Request::AgentSetMode { id, mode_id })? {
                Response::AgentModes(modes) => {
                    println!("{}", serde_json::to_string(&modes)?);
                }
                other => anyhow::bail!("unexpected daemon response: {other:?}"),
            }
            return Ok(());
        }
        AcpAction::Config {
            id,
            config_id: None,
            value: _,
        } => {
            println!(
                "{}",
                serde_json::to_string(&current_config_options(&paths.data_dir, &id, None)?)?
            );
            return Ok(());
        }
        AcpAction::Config {
            id,
            config_id: Some(config_id),
            value: None,
        } => {
            println!(
                "{}",
                serde_json::to_string(&current_config_options(
                    &paths.data_dir,
                    &id,
                    Some(config_id)
                )?)?
            );
            return Ok(());
        }
        AcpAction::Config {
            id,
            config_id: Some(config_id),
            value: Some(value),
        } => {
            match Client::request(
                &paths.data_dir,
                Request::AgentSetConfigOption {
                    id,
                    config_id,
                    value,
                },
            )? {
                Response::AgentConfigOptions(options) => {
                    println!("{}", serde_json::to_string(&options)?);
                }
                other => anyhow::bail!("unexpected daemon response: {other:?}"),
            }
            return Ok(());
        }
        AcpAction::Sessions { id } => {
            match Client::request(&paths.data_dir, Request::AgentSessions { id })? {
                Response::AgentSessions(sessions) => {
                    println!("{}", serde_json::to_string(&sessions)?);
                }
                other => anyhow::bail!("unexpected daemon response: {other:?}"),
            }
            return Ok(());
        }
        AcpAction::Adopt { id, session_id } => {
            match Client::request(&paths.data_dir, Request::AgentSessions { id: id.clone() })? {
                Response::AgentSessions(sessions) => {
                    let listed = sessions
                        .iter()
                        .any(|session| session.session_id == session_id);
                    anyhow::ensure!(
                        listed,
                        "agent {id} does not list session {session_id}; adopt what it actually offers"
                    );
                }
                other => anyhow::bail!("unexpected daemon response: {other:?}"),
            }
            // The same binding a session/read a TUI claim writes; the resume
            // mechanic is one, regardless of where the conversation came from.
            let project_id = default_project_id()?;
            use radar::config::Paths;
            let db = radar::db::Db::open(&Paths::with_root(paths.data_dir.clone()))?;
            let Response::Agents(agents) = Client::request(&paths.data_dir, Request::AgentList)?
            else {
                anyhow::bail!("unexpected daemon response while locating {id}");
            };
            let agent = agents
                .into_iter()
                .find(|agent| agent.id == id)
                .context("agent session is not running")?;
            if agent
                .capabilities
                .as_ref()
                .is_none_or(|capabilities| !capabilities.load_session)
            {
                anyhow::bail!("agent {id} cannot reload sessions; adopting would have no effect");
            }
            db.bind_session(project_id, &id, &agent.provider, &session_id)?;
            return Ok(());
        }
        AcpAction::List => Request::AgentList,
    };
    let response = Client::request(&paths.data_dir, request)?;
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}

/// Keep an agent-side CLI invocation alive until its request receives a typed
/// human response. The durable record is checked after resync, while watching
/// from the snapshot watermark closes the race with a response arriving then.
fn wait_for_attention(
    home: &Path,
    project_id: i64,
    request_id: &str,
    mut cursor: u64,
) -> Result<radar::session::activity::Attention> {
    use radar::session::activity::{ActivityPayload, Attention};
    use radar::session::daemon::{Client, Command as Request, Response};

    let resolved = |event: &radar::session::activity::ActivityEvent| {
        event.project_id == project_id
            && matches!(
                &event.payload,
                ActivityPayload::AttentionResolved { request_id: id, .. } if id == request_id
            )
    };
    let current_attention = || -> Result<Attention> {
        match Client::request(
            home,
            Request::AttentionStatus {
                project_id,
                request_id: request_id.to_string(),
            },
        )? {
            Response::AttentionStatus(attention) => Ok(attention),
            other => anyhow::bail!("unexpected attention status response: {other:?}"),
        }
    };
    let delay = || std::thread::sleep(std::time::Duration::from_millis(100));

    loop {
        let mut client = match Client::connect(
            home,
            Request::WatchActivity {
                project_id,
                after_sequence: cursor,
            },
        ) {
            Ok(client) => client,
            Err(_) => {
                delay();
                continue;
            }
        };
        let initial = client.receive();
        match initial {
            Ok(Response::ActivityWatching { snapshot, .. }) => {
                if snapshot.events.iter().any(&resolved) {
                    if let Ok(attention) = current_attention() {
                        if attention.resolved_at_millis.is_some() {
                            return Ok(attention);
                        }
                    }
                }
                cursor = snapshot.watermark;
                client.set_read_timeout(None)?;
                loop {
                    match client.receive() {
                        Ok(Response::Activity(event)) => {
                            cursor = event.sequence;
                            if resolved(&event) {
                                match current_attention() {
                                    Ok(attention) if attention.resolved_at_millis.is_some() => {
                                        return Ok(attention);
                                    }
                                    Err(_) => break,
                                    _ => {}
                                }
                            }
                        }
                        Ok(Response::ResyncRequired) => break,
                        Ok(_) | Err(_) => break,
                    }
                }
            }
            Ok(Response::ResyncRequired) => {}
            Ok(_) | Err(_) => {
                delay();
                continue;
            }
        }
        drop(client);

        let snapshot = match Client::request(
            home,
            Request::ActivitySnapshot {
                project_id,
                after_sequence: None,
                limit: 200,
            },
        ) {
            Ok(Response::ActivitySnapshot(snapshot)) => snapshot,
            _ => {
                delay();
                continue;
            }
        };
        cursor = snapshot.watermark;
        match current_attention() {
            Ok(attention) if attention.resolved_at_millis.is_some() => return Ok(attention),
            Ok(_) => {}
            Err(_) => {
                delay();
            }
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = match &cli.home {
        Some(root) => Paths::with_root(root),
        None => Paths::resolve(),
    };
    // The daemon/session API has no database or GTK initialization dependency.
    match cli.command {
        Some(Command::Serve) => {
            return radar::session::daemon::Server::bind(&paths.data_dir)?.run()
        }
        Some(Command::Web { port }) => return radar::web::run(&paths.data_dir, port),
        Some(Command::Mcp {
            action: McpAction::Serve { project },
        }) => return radar::mcp::serve(paths, project),
        Some(Command::Session { action }) => return session_command(&paths, action),
        Some(Command::Activity { action }) => return activity_command(&paths, action),
        Some(Command::Acp { action }) => return acp_command(&paths, action),
        _ => {}
    }
    let db = Db::open(&paths)?;

    match cli.command {
        // No subcommand: this is the app.
        None | Some(Command::Gui) => run_gui(paths, db),
        Some(
            Command::Serve
            | Command::Web { .. }
            | Command::Mcp { .. }
            | Command::Session { .. }
            | Command::Activity { .. }
            | Command::Acp { .. },
        ) => {
            unreachable!("handled before opening the database")
        }
        Some(Command::List) => list(&db, cli.json),
        Some(Command::Add {
            paths: to_add,
            scan,
            depth,
            no_select,
        }) => add(&db, to_add, scan, depth, no_select, cli.json),
        Some(Command::Remove { path }) => {
            let project = db
                .project_by_path(&path)?
                .with_context(|| format!("{} is not in the sidebar", path.display()))?;
            db.remove_project(project.id)?;
            println!("removed {} (files untouched)", project.display_path());
            Ok(())
        }
        Some(Command::Open { path }) => open(&db, &path, cli.json),
        Some(Command::Pin { path, off }) => {
            let project = require_project(&db, &path)?;
            db.set_pinned(project.id, !off)?;
            println!(
                "{} {}",
                if off { "unpinned" } else { "pinned" },
                project.display_path()
            );
            Ok(())
        }
        Some(Command::Move { path, delta }) => {
            let project = require_project(&db, &path)?;
            db.move_project(project.id, delta)?;
            list(&db, cli.json)
        }
        Some(Command::Rename { path, name }) => {
            let project = require_project(&db, &path)?;
            db.rename_project(project.id, &name)?;
            println!("renamed to {}", db.project(project.id)?.unwrap().name);
            Ok(())
        }
        Some(Command::Prune) => {
            let pruned = db.prune_missing()?;
            if pruned.is_empty() {
                println!("nothing to prune");
            }
            for project in pruned {
                println!("pruned {}", project.display_path());
            }
            Ok(())
        }
        Some(Command::Prefs { slot, program }) => prefs(&db, slot, program, cli.json),
        Some(Command::Agents) => show_agents(&db, cli.json),
        Some(Command::Programs { kind }) => show_programs(kind, cli.json),
        Some(Command::Find {
            query,
            root,
            depth,
            limit,
        }) => {
            let root = match root {
                Some(root) => root,
                None => db.ui_prefs()?.resolved_add_root(),
            };
            find(&db, &query.join(" "), root, depth, limit, cli.json)
        }
        Some(Command::Doctor) => doctor(&paths, &db),
        Some(Command::Board { path }) => show_board(&paths, &db, path, cli.json),
        Some(Command::Card { action }) => match action {
            CardAction::Add {
                path,
                title,
                column,
                body,
                by,
            } => card_add(
                &paths,
                &db,
                path,
                &title,
                column.as_deref(),
                body.as_deref(),
                by.as_deref(),
            ),
            CardAction::Claim { path, title, by } => {
                card_claim(&paths, &db, path, &title, Some(&by), cli.json)
            }
            CardAction::Release { path, title } => {
                card_claim(&paths, &db, path, &title, None, cli.json)
            }
            CardAction::Move { path, title, to } => {
                card_move(&paths, &db, path, &title, &to, cli.json)
            }
            CardAction::Done { path, title } => card_done(&paths, &db, path, &title, cli.json),
            CardAction::Next {
                path,
                by,
                in_column,
            } => card_next(&paths, &db, path, &by, in_column.as_deref(), cli.json),
            CardAction::Show { path, card } => card_show(&paths, &db, path, &card, cli.json),
            CardAction::Comment {
                path,
                card,
                text,
                session_id,
            } => card_comment(&paths, &db, path, &card, &text, session_id),
            CardAction::Edit {
                path,
                card,
                title,
                body,
            } => card_edit(
                &paths,
                &db,
                path,
                &card,
                title.as_deref(),
                body.as_deref(),
                cli.json,
            ),
            CardAction::Associate { path, card } => {
                card_associate(&paths, &db, path, &card, cli.json)
            }
            CardAction::Start { path, card } => card_start(&paths, &db, path, &card, cli.json),
        },
        Some(Command::Hook { action }) => match action {
            HookAction::Guard { file, commit, path } => {
                if commit {
                    hook_commit_guard(&paths, &db, path)
                } else {
                    hook_guard(&paths, &db, file, path)
                }
            }
        },
        Some(Command::Skill) => {
            let path = radar::skill::install_default_skill()?;
            if cli.json {
                println!("{}", serde_json::json!({ "skill": path }));
            } else {
                println!("board skill: {}", path.display());
            }
            Ok(())
        }
        Some(Command::Setup) => {
            let installed = radar::setup::install_default()?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "skill": installed.skill,
                        "opencode": installed.opencode,
                        "omp": installed.omp,
                        "claude": installed.claude,
                        "session_hooks": installed.session_hooks,
                        "git_hooks_dir": installed.git_hooks_dir,
                        "git_hooks_path_set": installed.git_hooks_path_set,
                    })
                );
                return Ok(());
            }
            println!("installed the board convention globally:");
            if let Some(path) = installed.skill {
                println!("  skill           {}", path.display());
            }
            if let Some(path) = installed.opencode {
                println!("  opencode plugin {}", path.display());
            }
            if let Some(path) = installed.omp {
                println!("  omp extension   {}", path.display());
            }
            if let Some(path) = installed.claude {
                println!("  claude hook     {}", path.display());
            }
            for path in installed.session_hooks {
                println!("  session hook    {}", path.display());
            }
            if let Some(dir) = installed.git_hooks_dir {
                println!("  git gate        {}", dir.join("pre-commit").display());
            }
            if !installed.git_hooks_path_set {
                println!("  note: core.hooksPath already set — radar's git gate not activated");
            }
            Ok(())
        }
    }
}

fn require_project(db: &Db, path: &PathBuf) -> Result<radar::db::Project> {
    db.project_by_path(path)?
        .with_context(|| format!("{} is not in the sidebar", path.display()))
}

/// The directory a board command works on: the given path, `RADAR_PROJECT_ROOT`
/// for a radar-launched pane, or the current directory.
fn board_dir(path: Option<PathBuf>) -> Result<PathBuf> {
    radar::session::board::dir(path)
}

/// A board command's project: its id and root directory. Prefers the
/// `RADAR_PROJECT_ID` a radar-launched pane carries, else the sidebar lookup.
fn board_context(db: &Db, path: Option<PathBuf>) -> Result<(i64, PathBuf)> {
    radar::session::board::context(db, path)
}

fn board_command_id(prefix: &str) -> String {
    radar::session::board::command_id(prefix)
}

fn find_stored<'a>(state: &'a BoardState, needle: &str) -> Option<&'a StoredCard> {
    radar::session::board::find_card(state, needle)
}

fn find_stored_mut<'a>(state: &'a BoardState, needle: &str) -> Result<&'a StoredCard> {
    find_stored(state, needle).with_context(|| format!("no card matching \"{needle}\""))
}

fn print_stored(card: &StoredCard) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(card)?);
    Ok(())
}

fn show_board(paths: &Paths, db: &Db, path: Option<PathBuf>, json: bool) -> Result<()> {
    use radar::session::daemon as board_api;

    let (project_id, root) = board_context(db, path)?;
    db.require_board_enabled(&root)?;
    let board = board_api::board_state(&paths.data_dir, project_id)?;
    let state = board.state;
    if json {
        println!("{}", serde_json::to_string_pretty(&state)?);
        return Ok(());
    }
    for lane in &state.lanes {
        let cards: Vec<&StoredCard> = state
            .cards
            .iter()
            .filter(|card| card.lane_id == lane.id)
            .collect();
        println!("{} ({})", lane.name, cards.len());
        for card in cards {
            match &card.claim {
                Some(who) => println!("  · {}  @{}", card.title, who),
                None => println!("  · {}", card.title),
            }
            for note in card.body.lines().filter(|line| !line.trim().is_empty()) {
                println!("      {note}");
            }
        }
    }
    Ok(())
}

fn card_add(
    paths: &Paths,
    db: &Db,
    path: Option<PathBuf>,
    title: &str,
    column: Option<&str>,
    body: Option<&str>,
    by: Option<&str>,
) -> Result<()> {
    use radar::session::daemon as board_api;

    let (project_id, root) = board_context(db, path)?;
    db.require_board_enabled(&root)?;
    // Ensure the store's board exists before the first card lands, so an add
    // cannot seed empty defaults first.
    let _ = board_api::board_state(&paths.data_dir, project_id)?;
    let change = board_api::board_card_add(
        &paths.data_dir,
        project_id,
        column,
        title,
        body.unwrap_or(""),
        by,
        &board_command_id("add"),
    )?;
    println!("added \"{}\"", change.card.title);
    Ok(())
}

/// `radar card start <card>`: dispatch a worker agent for a card. The daemon
/// spawns it with the card's canonical work prompt as its first message,
/// attached to the card (`RADAR_CARD_ID`) and claiming it for the new instance.
/// Idempotent: a card already claimed is left alone, so an orchestrator can
/// call it repeatedly.
fn card_start(
    paths: &Paths,
    db: &Db,
    path: Option<PathBuf>,
    needle: &str,
    json: bool,
) -> Result<()> {
    use radar::session::daemon as board_api;
    use radar::session::dispatch::{self, Dispatch};

    let (project_id, root) = board_context(db, path)?;
    db.require_board_enabled(&root)?;
    let board = board_api::board_state(&paths.data_dir, project_id)?;
    let state = board.state;
    let card = find_stored(&state, needle)
        .with_context(|| format!("no card {needle}"))?
        .clone();

    match dispatch::start_card(
        paths,
        db,
        project_id,
        &root,
        &card,
        &board_command_id("start"),
    )? {
        Dispatch::AlreadyClaimed { claim } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "created": false, "cardId": card.id, "claim": claim })
                );
            } else {
                println!("{} is already claimed by {claim}", card.title);
            }
        }
        Dispatch::Started {
            claim, session_id, ..
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "created": true,
                        "cardId": card.id,
                        "claim": claim,
                        "sessionId": session_id,
                    })
                );
            } else {
                println!("Started {claim} on \"{}\" ({})", card.title, card.id);
            }
        }
    }
    Ok(())
}

/// `radar card associate <card>`: put the session you are running in on a
/// card, by claiming it for the pane's own `RADAR_AGENT`. No name argument, and
/// safe to run again after a fork or restore — the board's "this session is
/// working that card" command.
fn card_associate(
    paths: &Paths,
    db: &Db,
    path: Option<PathBuf>,
    needle: &str,
    json: bool,
) -> Result<()> {
    let agent = std::env::var("RADAR_AGENT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .context(
            "radar card associate must run inside a radar-launched pane (RADAR_AGENT is not set); \
             name the holder explicitly with `radar card claim --by <name>`",
        )?;
    card_claim(paths, db, path, needle, Some(&agent), json)
}

fn card_claim(
    paths: &Paths,
    db: &Db,
    path: Option<PathBuf>,
    needle: &str,
    by: Option<&str>,
    json: bool,
) -> Result<()> {
    use radar::session::daemon as board_api;

    let (project_id, root) = board_context(db, path)?;
    db.require_board_enabled(&root)?;
    let board = board_api::board_state(&paths.data_dir, project_id)?;
    let state = board.state;
    let card_id = find_stored_mut(&state, needle)?.id.clone();
    let change = board_api::board_card_claim(
        &paths.data_dir,
        project_id,
        &card_id,
        by,
        None,
        &board_command_id("claim"),
    )?;
    if json {
        return print_stored(&change.card);
    }
    match by {
        Some(who) => println!("\"{}\" claimed by {}", change.card.title, who),
        None => println!("\"{}\" released", change.card.title),
    }
    Ok(())
}

fn card_move(
    paths: &Paths,
    db: &Db,
    path: Option<PathBuf>,
    needle: &str,
    to: &str,
    json: bool,
) -> Result<()> {
    use radar::session::daemon as board_api;

    let (project_id, root) = board_context(db, path)?;
    db.require_board_enabled(&root)?;
    let board = board_api::board_state(&paths.data_dir, project_id)?;
    let state = board.state;
    let card_id = find_stored_mut(&state, needle)?.id.clone();
    let change = board_api::board_card_move(
        &paths.data_dir,
        project_id,
        &card_id,
        to,
        None,
        &board_command_id("move"),
    )?;
    if json {
        return print_stored(&change.card);
    }
    println!("\"{}\" moved to {}", change.card.title, to);
    Ok(())
}

fn card_done(
    paths: &Paths,
    db: &Db,
    path: Option<PathBuf>,
    needle: &str,
    json: bool,
) -> Result<()> {
    use radar::session::daemon as board_api;

    let (project_id, root) = board_context(db, path)?;
    db.require_board_enabled(&root)?;
    let board = board_api::board_state(&paths.data_dir, project_id)?;
    let state = board.state;
    let card_id = find_stored_mut(&state, needle)?.id.clone();
    let change = board_api::board_card_complete(
        &paths.data_dir,
        project_id,
        &card_id,
        None,
        &board_command_id("done"),
    )?;
    if json {
        return print_stored(&change.card);
    }
    println!("\"{}\" done", change.card.title);
    Ok(())
}

/// The work primitive: hand the agent the next unclaimed card, claimed in its
/// name. An agent's whole loop is `card next` → do it → move to Review. This
/// is also where an agent meets the convention: the first `card next` installs
/// the board skill into the user's global skills home and the repository's git
/// gate, so the convention arrives with the first claimed card.
fn card_next(
    paths: &Paths,
    db: &Db,
    path: Option<PathBuf>,
    by: &str,
    in_column: Option<&str>,
    json: bool,
) -> Result<()> {
    use radar::session::daemon as board_api;

    let (project_id, root) = board_context(db, path)?;
    db.require_board_enabled(&root)?;
    if let Err(error) = radar::skill::install(db, &root) {
        eprintln!("radar: could not install the board skill: {error}");
    }
    let Some(change) = board_api::board_card_next(
        &paths.data_dir,
        project_id,
        by,
        in_column,
        &board_command_id("next"),
    )?
    else {
        anyhow::bail!("no unclaimed cards");
    };
    if json {
        return print_stored(&change.card);
    }
    println!("\"{}\" — claimed for {}", change.card.title, by);
    for note in change
        .card
        .body
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        println!("      {note}");
    }
    Ok(())
}

fn human_age(at_millis: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(at_millis);
    let seconds = (now.saturating_sub(at_millis)).max(0) / 1000;
    match seconds {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// Read a card and its thread. The thread is a bonus: a card still shows
/// when the daemon is down or the project is not in the sidebar.
fn card_show(
    paths: &Paths,
    db: &Db,
    path: Option<PathBuf>,
    needle: &str,
    json: bool,
) -> Result<()> {
    use radar::session::daemon as board_api;
    use radar::session::daemon::{Client, Command as Request, Response};

    let (project_id, root) = board_context(db, path)?;
    db.require_board_enabled(&root)?;
    let board = board_api::board_state(&paths.data_dir, project_id)?;
    let state = board.state;
    let card = find_stored_mut(&state, needle)?.clone();
    let lane = state
        .lanes
        .iter()
        .find(|lane| lane.id == card.lane_id)
        .map(|lane| lane.name.clone())
        .unwrap_or_default();

    let thread = match Client::request(
        &paths.data_dir,
        Request::ActivitySnapshot {
            project_id,
            after_sequence: None,
            limit: 200,
        },
    ) {
        Ok(Response::ActivitySnapshot(snapshot)) => Some(snapshot),
        _ => None,
    };

    if json {
        let entries = thread
            .as_ref()
            .map(|snapshot| radar::mcp::thread_json(snapshot, &card.id))
            .unwrap_or_default();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "card": card,
                "lane": lane,
                "thread": entries,
            }))?
        );
        return Ok(());
    }

    println!("{}  [{}]", card.title, lane);
    if let Some(who) = &card.claim {
        println!("  @{who}");
    }
    for note in card.body.lines().filter(|line| !line.trim().is_empty()) {
        println!("  {note}");
    }
    if let Some(snapshot) = &thread {
        let entries = radar::mcp::thread_json(snapshot, &card.id);
        if !entries.is_empty() {
            println!();
            for entry in entries {
                let who = entry["author"].as_str().unwrap_or("");
                let text = entry["text"].as_str().unwrap_or("");
                let at = entry["at_millis"].as_i64().unwrap_or(0);
                println!("  {who} ({}): {text}", human_age(at));
            }
        }
    }
    Ok(())
}

/// Post a message on a card's thread — how an agent reports back to the human
/// without burying it in terminal output. The thread lives in the daemon
/// journal keyed by the card's stable id.
fn card_comment(
    paths: &Paths,
    db: &Db,
    path: Option<PathBuf>,
    needle: &str,
    text: &str,
    session_id: Option<String>,
) -> Result<()> {
    use radar::session::activity::{ActivityKind, ActivityPayload, PublishActivity};
    use radar::session::daemon as board_api;
    use radar::session::daemon::{Client, Command as Request, Response};

    let (project_id, root) = board_context(db, path)?;
    db.require_board_enabled(&root)?;
    let board = board_api::board_state(&paths.data_dir, project_id)?;
    let state = board.state;
    let card_id = find_stored_mut(&state, needle)?.id.clone();
    let command = Request::PublishActivity(PublishActivity {
        project_id,
        command_id: board_command_id("comment"),
        session_id: session_id.or_else(|| std::env::var("RADAR_SESSION_ID").ok()),
        card_id: Some(card_id.clone()),
        kind: ActivityKind::Reported,
        payload: ActivityPayload::Message {
            text: text.to_string(),
        },
    });
    match Client::request(&paths.data_dir, command)? {
        Response::ActivityPublished(event) => {
            println!("commented on {:?} (event {})", needle, event.sequence)
        }
        other => anyhow::bail!("unexpected daemon response: {other:?}"),
    }
    Ok(())
}

/// Edit a card's title and/or notes in the store, preserving its id, claim,
/// lane and done state.
fn card_edit(
    paths: &Paths,
    db: &Db,
    path: Option<PathBuf>,
    needle: &str,
    title: Option<&str>,
    body: Option<&str>,
    json: bool,
) -> Result<()> {
    use radar::session::daemon as board_api;

    let (project_id, root) = board_context(db, path)?;
    db.require_board_enabled(&root)?;
    let board = board_api::board_state(&paths.data_dir, project_id)?;
    let state = board.state;
    let card_id = find_stored_mut(&state, needle)?.id.clone();
    let change = board_api::board_card_update(
        &paths.data_dir,
        project_id,
        &card_id,
        title,
        body,
        None,
        &board_command_id("edit"),
    )?;
    if json {
        return print_stored(&change.card);
    }
    println!("\"{}\" updated", change.card.title);
    Ok(())
}

/// Whether the agent holds a live claim, read from the board store. `None`
/// means it could not be judged (daemon down, project not in the sidebar) and
/// the guard lets the call through.
fn claim_holds(paths: &Paths, db: &Db, dir: &Path, who: &str) -> Option<bool> {
    let (project_id, _root) = board_context(db, Some(dir.to_path_buf())).ok()?;
    // Never spawn a daemon here: an edit-time gate must fail open.
    let state = radar::session::daemon::board_state_quick(&paths.data_dir, project_id)
        .ok()
        .map(|board| board.state)?;
    Some(
        state
            .cards
            .iter()
            .any(|card| card.claim.as_deref() == Some(who) && !card.done),
    )
}

/// The hook half of the convention: an agent harness asks, before an edit
/// lands, whether the agent holds a board claim. Denied calls come back to the
/// model as a tool error whose text is the remedy — claim work, then retry —
/// so the guard enforces without stranding the agent.
///
/// Claude Code's PreToolUse hook is a subprocess: it pipes the tool call as
/// JSON and reads the exit code (2 denies, stderr goes to the model). The
/// opencode plugin calls the same check in-process. `--file` covers both and
/// the human running it by hand.
fn hook_guard(paths: &Paths, db: &Db, file: Option<PathBuf>, path: Option<PathBuf>) -> Result<()> {
    use std::io::IsTerminal;

    let mut input = String::new();
    if !std::io::stdin().is_terminal() {
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
    }
    let file = file.or_else(|| hook_file(&input));

    let dir = board_dir(path)?;
    let who = std::env::var("RADAR_AGENT").ok().filter(|s| !s.is_empty());
    let holds = who
        .as_deref()
        .and_then(|who| claim_holds(paths, db, &dir, who));
    guard_exit(radar::skill::guard_decision_for(
        db,
        &dir,
        who.as_deref(),
        file.as_deref(),
        holds,
    ))
}

/// The commit gate: the git pre-commit hook's half of the convention. No file
/// is weighed — the claim is the whole question, since whatever an agent
/// edited with, the work enters the repository here.
fn hook_commit_guard(paths: &Paths, db: &Db, path: Option<PathBuf>) -> Result<()> {
    let dir = board_dir(path)?;
    let who = std::env::var("RADAR_AGENT").ok().filter(|s| !s.is_empty());
    let holds = who
        .as_deref()
        .and_then(|who| claim_holds(paths, db, &dir, who));
    guard_exit(radar::skill::commit_decision_for(
        db,
        &dir,
        who.as_deref(),
        holds,
    ))
}

fn guard_exit(decision: radar::skill::GuardDecision) -> Result<()> {
    match decision {
        radar::skill::GuardDecision::Allow => Ok(()),
        radar::skill::GuardDecision::Deny(reason) => {
            eprintln!("{reason}");
            std::process::exit(2);
        }
    }
}

/// The file a harness's tool-call JSON wants to edit: `tool_input.file_path`
/// in Claude Code's pre-tool-use payload.
fn hook_file(input: &str) -> Option<PathBuf> {
    let value: serde_json::Value = serde_json::from_str(input).ok()?;
    value
        .get("tool_input")?
        .get("file_path")?
        .as_str()
        .map(PathBuf::from)
}

fn list(db: &Db, json: bool) -> Result<()> {
    let projects = db.projects()?;
    let selected = db.ui_prefs()?.last_project;

    let mut rows = Vec::with_capacity(projects.len());
    for project in &projects {
        let status = git::status(&project.path);
        let tabs = db.tabs(project.id)?.len();
        rows.push(serde_json::json!({
            "id": project.id,
            "name": project.name,
            "path": project.path,
            "display_path": project.display_path(),
            "pinned": project.pinned,
            "missing": project.is_missing(),
            "last_opened_at": project.last_opened_at,
            "open_count": project.open_count,
            "selected": selected == Some(project.id),
            "git": {
                "is_repo": status.is_repo,
                "branch": status.branch,
                "ahead": status.ahead,
                "behind": status.behind,
                "changed": status.changed,
                "summary": status.summary(),
            },
            "tabs": tabs,
        }));
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no projects yet — add one with `radar add <dir>`");
        return Ok(());
    }
    let width = projects
        .iter()
        .map(|p| p.name.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    for row in &rows {
        let tabs = row["tabs"].as_i64().unwrap_or(0);
        println!(
            "{marker} {name:width$}  {summary:<14}  {path}{tabs}",
            marker = if row["selected"] == true { "▸" } else { " " },
            name = row["name"].as_str().unwrap_or(""),
            width = width,
            summary = row["git"]["summary"].as_str().unwrap_or(""),
            path = row["display_path"].as_str().unwrap_or(""),
            tabs = if tabs > 0 {
                format!("  ({tabs} tabs)")
            } else {
                String::new()
            },
        );
    }
    Ok(())
}

fn add(
    db: &Db,
    paths: Vec<PathBuf>,
    scan: Option<Option<PathBuf>>,
    depth: usize,
    no_select: bool,
    json: bool,
) -> Result<()> {
    let mut added = Vec::new();
    let did_scan = scan.is_some();
    if let Some(root) = scan {
        let root = root.unwrap_or(std::env::current_dir()?);
        let candidates = discover::scan(&root, depth, 500);
        let repos: Vec<_> = candidates.into_iter().filter(|c| c.is_repo).collect();
        for candidate in repos {
            if db.project_by_path(&candidate.path)?.is_none() {
                added.push(db.add_project(&candidate.path)?);
            }
        }
    }
    for path in paths {
        added.push(db.add_project(&path)?);
    }
    if added.is_empty() && did_scan {
        println!("no new repositories found");
        return Ok(());
    }
    if let Some(first) = added.first() {
        db.log_event("project_added", Some(first.id), &serde_json::Value::Null)?;
        if !no_select {
            db.remember_last_project(Some(first.id))?;
        }
    }
    if json {
        let rows: Vec<_> = added
            .iter()
            .map(|p| serde_json::json!({ "id": p.id, "name": p.name, "path": p.path }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        for project in &added {
            println!("added {} ({})", project.name, project.display_path());
        }
    }
    Ok(())
}

/// Show what a project's tabs resolve to right now.
fn open(db: &Db, path: &PathBuf, json: bool) -> Result<()> {
    let project = require_project(db, path)?;
    db.touch_project(project.id)?;
    db.remember_last_project(Some(project.id))?;
    db.log_event("project_opened", Some(project.id), &serde_json::Value::Null)?;

    let preferences = db
        .project_settings(project.id)?
        .apply_to(&db.preferences()?);

    let stored = db.tabs(project.id)?;
    let tabs = if stored.is_empty() {
        default_tabs(&preferences)
    } else {
        stored
    };
    let options = launch_options(&preferences, false);

    let resolved: Vec<serde_json::Value> = tabs
        .iter()
        .filter_map(|tab| {
            let program = programs::by_id(&tab.program_id)?;
            let mut options = options.clone();
            options.extra_args = tab.extra_args.clone();
            let spec = program.command_spec(&options);
            Some(serde_json::json!({
                "slot": tab.slot.as_str(),
                "program": program.id,
                "name": program.name,
                "exec": spec.argv,
                "cwd": project.path,
            }))
        })
        .collect();

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "project": { "id": project.id, "name": project.name, "path": project.path },
                "status": git::status(&project.path).summary(),
                "tabs": resolved,
            }))?
        );
    } else {
        println!(
            "{}  ·  {}  ·  {}",
            project.name,
            project.display_path(),
            git::status(&project.path).summary()
        );
        for tab in resolved {
            println!(
                "  {:<7} {:<16} {}",
                tab["slot"].as_str().unwrap_or(""),
                tab["name"].as_str().unwrap_or(""),
                tab["exec"]
                    .as_array()
                    .map(|argv| argv
                        .iter()
                        .map(|a| a.as_str().unwrap_or(""))
                        .collect::<Vec<_>>()
                        .join(" "))
                    .unwrap_or_default()
            );
        }
    }
    Ok(())
}

/// The tabs a brand new project starts with: editor, agent, diff.
fn default_tabs(preferences: &Preferences) -> Vec<Tab> {
    let mut tabs = Vec::new();
    for slot in [Slot::Editor, Slot::Agent, Slot::Diff] {
        if let Some(program) = programs::for_slot(slot, preferences) {
            tabs.push(Tab::new(slot, program.id));
        }
    }
    tabs
}

fn launch_options(preferences: &Preferences, safe: bool) -> LaunchOptions {
    LaunchOptions {
        // omarchy's keybinding runs agents unattended; match that by default.
        safe: safe || !preferences.agent_auto_flags,
        extra_args: Vec::new(),
        prompt: None,
        session: None,
        create_session: None,
        agent_instance: None,
        card: None,
    }
}

fn prefs(db: &Db, slot: Option<String>, program: Option<String>, json: bool) -> Result<()> {
    match (slot, program) {
        // The reviewer is not a pane slot: it is the agent that reviews
        // finished board cards, so it gets its own preference.
        (Some(slot_name), program) if slot_name == "reviewer" => {
            let program = match program.as_deref() {
                None | Some("none") => None,
                Some(id) => {
                    anyhow::ensure!(
                        agents::is_supported(id),
                        "selectable agents are {}",
                        agents::SUPPORTED_AGENT_IDS.join(", ")
                    );
                    Some(id.to_string())
                }
            };
            db.set_reviewer(program.as_deref())?;
            db.log_event(
                "preference_changed",
                None,
                &serde_json::json!({ "slot": "reviewer", "program": program }),
            )?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "slot": "reviewer", "program": program })
                );
            } else {
                match program {
                    Some(id) => println!("reviewer: {id}"),
                    None => println!("reviewer: none (the daemon dispatches nobody)"),
                }
            }
            return Ok(());
        }
        (Some(slot), Some(program)) => {
            let slot = match slot.as_str() {
                "editor" => Slot::Editor,
                "agent" => Slot::Agent,
                "diff" => Slot::Diff,
                "shell" => Slot::Shell,
                other => anyhow::bail!("unknown slot {other} (editor, agent, diff, shell)"),
            };
            let program = if program == "none" {
                None
            } else {
                Some(program)
            };
            if let Some(id) = program.as_deref() {
                anyhow::ensure!(
                    programs::by_id(id).is_some(),
                    "{id} is not a program radar knows about (see `radar programs`)"
                );
                if slot == Slot::Agent {
                    anyhow::ensure!(
                        agents::is_supported(id),
                        "selectable agents are {}",
                        agents::SUPPORTED_AGENT_IDS.join(", ")
                    );
                }
            }
            db.set_preference(slot, program.as_deref())?;
            db.log_event(
                "preference_changed",
                None,
                &serde_json::json!({ "slot": slot.as_str(), "program": program }),
            )?;
        }
        (Some(slot), None) => {
            // Show the resolved choice for this slot and exit.
            let slot = match slot.as_str() {
                "editor" => Slot::Editor,
                "agent" => Slot::Agent,
                "diff" => Slot::Diff,
                "shell" => Slot::Shell,
                other => anyhow::bail!("unknown slot {other}"),
            };
            let preferences = db.preferences()?;
            let resolved = programs::for_slot(slot, &preferences);
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "slot": slot.as_str(),
                        "preferred": preferences.get(slot),
                        "resolved": resolved.map(|p| p.id),
                    })
                );
            } else {
                println!(
                    "{}: {}",
                    slot.as_str(),
                    resolved
                        .map(|p| p.name)
                        .unwrap_or_else(|| "nothing installed".into())
                );
            }
            return Ok(());
        }
        (None, _) => {}
    }

    let preferences = db.preferences()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&preferences)?);
        return Ok(());
    }
    for slot in [Slot::Editor, Slot::Agent, Slot::Diff, Slot::Shell] {
        let preferred = preferences.get(slot);
        let resolved = programs::for_slot(slot, &preferences);
        println!(
            "{:<7} {:<22} {}",
            slot.as_str(),
            preferred.unwrap_or("auto"),
            resolved
                .map(|p| format!("→ {} ({})", p.name, p.id))
                .unwrap_or_else(|| "→ nothing installed".into())
        );
    }
    println!(
        "\nagent permission flags: {}",
        if preferences.agent_auto_flags {
            "auto (skip prompts, like omarchy)"
        } else {
            "safe (ask)"
        }
    );
    Ok(())
}

fn show_agents(db: &Db, json: bool) -> Result<()> {
    let preferences = db.preferences()?;
    let default = agents::omarchy_default();
    let rows: Vec<serde_json::Value> = agents::supported_programs()
        .iter()
        .map(|agent| {
            serde_json::json!({
                "id": agent.id,
                "name": agent.name,
                "installed": agent.installed(),
                "omarchy": agent.omarchy,
                "default": default.as_deref() == Some(agent.id.as_str()),
                "preferred": preferences.agent.as_deref() == Some(agent.id.as_str()),
                "permission_args": agent.auto_args,
            })
        })
        .collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    let default_label = match default.as_deref() {
        Some(id) if !agents::is_supported(id) => {
            format!("{id} (not selectable in Radar)")
        }
        Some(id) => id.to_string(),
        None => "(none set — `omarchy default agent <name>`)".into(),
    };
    println!("omarchy default agent: {default_label}");
    let preferred_label = match preferences.agent.as_deref() {
        Some(id) if !agents::is_supported(id) => {
            format!("{id} (not selectable; ignored)")
        }
        Some(id) => id.to_string(),
        None => "(follow omarchy)".into(),
    };
    println!("preferred in radar: {preferred_label}");
    println!();
    for row in rows {
        let mark = if row["default"] == true {
            "◆"
        } else if row["preferred"] == true {
            "▸"
        } else {
            " "
        };
        println!(
            "{mark} {:<18} {:<16} {}",
            row["id"].as_str().unwrap_or(""),
            row["name"].as_str().unwrap_or(""),
            if row["installed"] == true {
                "installed"
            } else {
                "not installed"
            }
        );
    }
    Ok(())
}

fn show_programs(kind: Option<String>, json: bool) -> Result<()> {
    let wanted: Option<Kind> = kind.as_deref().and_then(|k| match k {
        "editor" => Some(Kind::Editor),
        "agent" => Some(Kind::Agent),
        "diff" => Some(Kind::Diff),
        "shell" => Some(Kind::Shell),
        "tool" => Some(Kind::Tool),
        _ => None,
    });
    let programs = programs::registry();
    if json {
        let rows: Vec<_> = programs
            .iter()
            .filter(|p| wanted.is_none_or(|k| p.kind == k))
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "name": p.name,
                    "kind": p.kind,
                    "installed": p.installed(),
                    "external": p.external,
                    "args": p.args,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    for kind in Kind::ALL {
        if wanted.is_some_and(|k| k != kind) {
            continue;
        }
        let of_kind: Vec<_> = programs.iter().filter(|p| p.kind == kind).collect();
        if of_kind.is_empty() {
            continue;
        }
        println!("{}", kind.label());
        for program in of_kind {
            println!(
                "  {:<16} {:<18} {:<12} {}",
                program.id,
                program.name,
                if program.installed() {
                    "installed"
                } else {
                    "missing"
                },
                program.detail()
            );
        }
        println!();
    }
    Ok(())
}

fn find(db: &Db, query: &str, root: PathBuf, depth: usize, limit: usize, json: bool) -> Result<()> {
    let mut candidates = discover::scan(&root, depth, 500);
    let known: Vec<PathBuf> = db.projects()?.into_iter().map(|p| p.path).collect();
    discover::mark_known(&mut candidates, &known);
    let hits = discover::filter(&candidates, query);
    let hits: Vec<_> = hits.into_iter().take(limit).collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
        return Ok(());
    }
    if hits.is_empty() {
        println!("nothing matches {query:?} under {}", root.display());
        return Ok(());
    }
    let width = hits
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(4);
    for candidate in hits {
        println!(
            "{marker} {name:width$}  {path}{known}",
            marker = if candidate.is_repo { "◆" } else { " " },
            name = candidate.name,
            width = width,
            path = candidate.display_path(),
            known = if candidate.known {
                "  (already added)"
            } else {
                ""
            },
        );
    }
    Ok(())
}

fn doctor(paths: &Paths, db: &Db) -> Result<()> {
    println!("radar {}", env!("CARGO_PKG_VERSION"));
    println!("state:  {}", paths.data_dir.display());
    println!("config: {}", paths.config_dir.display());
    println!(
        "db:     {} ({})",
        paths.database().display(),
        if paths.database().exists() {
            "present"
        } else {
            "will be created"
        }
    );
    println!(
        "schema: v{}",
        db.conn()
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?
    );
    println!("projects: {}", db.projects()?.len());
    println!();

    // name, present, what it gives us, how to get it when missing
    let checks: [(&str, bool, &str, &str); 8] = [
        (
            "git",
            radar::config::have("git"),
            "project status",
            "pacman -S git",
        ),
        (
            "fd",
            radar::config::have("fd"),
            "fast directory scanning",
            "pacman -S fd",
        ),
        (
            "rg",
            radar::config::have("rg"),
            "searching",
            "pacman -S ripgrep",
        ),
        (
            "fzf",
            radar::config::have("fzf"),
            "external fuzzy picking",
            "pacman -S fzf",
        ),
        (
            "omarchy",
            radar::config::have("omarchy"),
            "default agent + agent flags",
            "part of omarchy",
        ),
        (
            "lazygit",
            radar::config::have("lazygit"),
            "diff tabs",
            "pacman -S lazygit",
        ),
        (
            "libvte-2.91-gtk4",
            vte_available(),
            "embedded terminals",
            "omarchy pkg add vte4",
        ),
        (
            "gtk4",
            gtk_available(),
            "the app itself",
            "omarchy pkg add gtk4 libadwaita",
        ),
    ];
    for (name, present, gives, remedy) in checks {
        if present {
            println!("ok   {name:<20} {gives}");
        } else {
            println!("miss {name:<20} {gives} — install with: {remedy}");
        }
    }
    println!();
    println!(
        "gui build:    {}",
        if cfg!(feature = "gui") {
            "included"
        } else {
            "not included (rebuild with --features gui)"
        }
    );
    let preferences = db.preferences()?;
    for slot in [Slot::Editor, Slot::Agent, Slot::Diff, Slot::Shell] {
        println!(
            "{:<13} {}",
            format!("{}:", slot.as_str()),
            programs::for_slot(slot, &preferences)
                .map(|p| format!("{} ({})", p.name, p.id))
                .unwrap_or_else(|| "nothing installed".into())
        );
    }
    Ok(())
}

/// Is the GTK4 VTE development file present, which is what the build needs?
fn vte_available() -> bool {
    [
        "/usr/lib/pkgconfig/vte-2.91-gtk4.pc",
        "/usr/lib64/pkgconfig/vte-2.91-gtk4.pc",
    ]
    .iter()
    .any(|path| std::path::Path::new(path).exists())
        || std::env::var("PKG_CONFIG_PATH")
            .map(|paths| {
                paths
                    .split(':')
                    .any(|dir| std::path::Path::new(dir).join("vte-2.91-gtk4.pc").exists())
            })
            .unwrap_or(false)
}

fn gtk_available() -> bool {
    ["/usr/lib/pkgconfig/gtk4.pc", "/usr/lib64/pkgconfig/gtk4.pc"]
        .iter()
        .any(|path| std::path::Path::new(path).exists())
}

#[cfg(feature = "gui")]
fn run_gui(paths: Paths, db: Db) -> Result<()> {
    radar::gui::run(paths, db)
}

#[cfg(not(feature = "gui"))]
fn run_gui(_paths: Paths, _db: Db) -> Result<()> {
    eprintln!(
        "This build has no GUI.\n\n\
         The app needs the GTK4 VTE widget:\n  \
         omarchy pkg add vte4\n\
         then rebuild:\n  \
         cargo run --features gui\n\n\
         The CLI works already: radar list, radar find, radar doctor"
    );
    std::process::exit(2);
}
