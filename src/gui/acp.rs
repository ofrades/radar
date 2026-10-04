//! Native controls for daemon-owned agent conversations. The card remains a
//! task/history surface; prompts belong in this separate conversation window.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::{board, App};
use crate::db::{Db, Project, Slot};
use crate::session::activity::{
    ActivityPayload, ActivitySnapshot, AttentionActionKind, AttentionChange, AttentionResponse,
    ChangeAttention,
};
use crate::session::agent::{
    AgentConfigKind, AgentConfigOption, AgentModes, AgentSessionInfo, AgentStart, AgentStatus,
};
use crate::session::board_store::StoredCard;
use crate::session::daemon::{self, Client, Command, Response};
use adw::prelude::*;
use anyhow::{bail, Context, Result};

pub(super) fn card_agent_id(project_id: i64, card_id: &str) -> String {
    format!("acp-card-{project_id}-{card_id}")
}

#[derive(Clone)]
struct Target {
    home: PathBuf,
    project: Project,
    card_id: String,
    agent_id: String,
}

#[derive(Clone)]
struct Snapshot {
    agent: Option<AgentStatus>,
    card: StoredCard,
    activity: ActivitySnapshot,
    binding: Option<(String, String)>,
    other_worker: bool,
}

impl Target {
    fn importable(&self) -> Result<Snapshot> {
        let snapshot = self.read()?;
        let agent = snapshot
            .agent
            .as_ref()
            .context("Start the provider first")?;
        if agent.state != "ready" {
            bail!("Wait until the agent is ready before opening another conversation");
        }
        if !agent
            .capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.session_list && capabilities.load_session)
        {
            bail!("This provider does not support listing and reopening conversations");
        }
        if snapshot.card.done
            || snapshot.other_worker
            || snapshot
                .card
                .claim
                .as_ref()
                .is_some_and(|claim| claim != &self.agent_id)
        {
            bail!("Reopen or release this task before opening another conversation");
        }
        if snapshot.activity.attention.iter().any(|request| {
            request.card_id.as_deref() == Some(&self.card_id) && request.is_unresolved()
        }) {
            bail!("Answer the outstanding request before opening another conversation");
        }
        Ok(snapshot)
    }

    fn sessions(&self) -> Result<Vec<AgentSessionInfo>> {
        self.importable()?;
        match Client::request(
            &self.home,
            Command::AgentSessions {
                id: self.agent_id.clone(),
            },
        )? {
            Response::AgentSessions(sessions) => Ok(sessions
                .into_iter()
                .filter(|session| session.cwd == self.project.path)
                .collect()),
            other => bail!("Unexpected conversation list response: {other:?}"),
        }
    }

    fn read(&self) -> Result<Snapshot> {
        let agents = match Client::request(&self.home, Command::AgentList)? {
            Response::Agents(agents) => agents,
            other => bail!("Unexpected agent response: {other:?}"),
        };
        let board = daemon::board_state(&self.home, self.project.id)?;
        let card = board
            .cards
            .iter()
            .find(|card| card.id == self.card_id)
            .cloned()
            .context("This to-do is no longer on the board")?;
        let activity = match Client::request(
            &self.home,
            Command::ActivitySnapshot {
                project_id: self.project.id,
                after_sequence: None,
                limit: 200,
            },
        )? {
            Response::ActivitySnapshot(snapshot) => snapshot,
            other => bail!("Unexpected activity response: {other:?}"),
        };
        let db = Db::open(&crate::config::Paths::with_root(&self.home))?;
        let other_worker = board.derived.iter().any(|facts| {
            facts.card_id == self.card_id
                && matches!(
                    facts.worker,
                    Some(
                        crate::session::lane::WorkerFact::Running
                            | crate::session::lane::WorkerFact::Quiet
                    )
                )
        }) || agents.iter().any(|agent| {
            agent.id != self.agent_id
                && agent.cwd == self.project.path
                && agent.card_id.as_deref() == Some(&self.card_id)
                && agent_live(agent)
        });
        let agent = agents.into_iter().find(|agent| agent.id == self.agent_id);
        if agent.as_ref().is_some_and(|agent| {
            agent.cwd != self.project.path || agent.card_id.as_deref() != Some(&self.card_id)
        }) {
            bail!("This agent is attached to a different task");
        }
        Ok(Snapshot {
            agent,
            card,
            activity,
            binding: db.bound_session(self.project.id, &self.agent_id)?,
            other_worker,
        })
    }

    fn claim(&self, card: &StoredCard) -> Result<()> {
        if card.done {
            bail!("Reopen this to-do before assigning more work");
        }
        if let Some(claim) = &card.claim {
            if claim != &self.agent_id {
                bail!(
                    "This to-do is assigned to {claim}; release it before assigning another agent"
                );
            }
            return Ok(());
        }
        daemon::board_card_claim(
            &self.home,
            self.project.id,
            &self.card_id,
            Some(&self.agent_id),
            Some(card.revision),
            &super::gui_command_id("acp-claim"),
        )?;
        Ok(())
    }

    fn execute(&self, action: Action) -> Result<()> {
        match action {
            Action::Start(program) => {
                let snapshot = self.read()?;
                if snapshot.other_worker {
                    bail!("This to-do already has a running worker; open that session instead");
                }
                if snapshot.agent.as_ref().is_some_and(agent_live) {
                    bail!("This conversation is already running");
                }
                let resume = snapshot.binding.or_else(|| {
                    snapshot.agent.as_ref().and_then(|agent| {
                        agent
                            .acp_session_id
                            .clone()
                            .map(|id| (agent.provider.clone(), id))
                    })
                });
                let (program, conversation) = match resume {
                    Some((provider, id)) => (provider, Some(id)),
                    None => (
                        program.context("Choose an installed conversation agent")?,
                        None,
                    ),
                };
                self.claim(&snapshot.card)?;
                match Client::request(
                    &self.home,
                    Command::AgentStart(AgentStart {
                        id: self.agent_id.clone(),
                        provider: program.clone(),
                        program,
                        args: Vec::new(),
                        cwd: self.project.path.clone(),
                        project_id: self.project.id,
                        session_id: Some(format!("acp-{}", self.agent_id)),
                        card_id: Some(self.card_id.clone()),
                        acp_session_id: conversation,
                    }),
                )? {
                    Response::AgentStatus(_) => Ok(()),
                    other => bail!("Unexpected start response: {other:?}"),
                }
            }
            Action::Prompt(text) => {
                let snapshot = self.read()?;
                if snapshot.other_worker {
                    bail!("Another worker is running on this to-do");
                }
                let agent = snapshot
                    .agent
                    .context("Start or resume the conversation first")?;
                if agent.state != "ready" {
                    bail!("Wait until the agent is ready for another message");
                }
                if snapshot.activity.attention.iter().any(|request| {
                    request.card_id.as_deref() == Some(&self.card_id) && request.is_unresolved()
                }) {
                    bail!("Answer the outstanding request before sending another message");
                }
                self.claim(&snapshot.card)?;
                let session_id = format!("acp-{}", self.agent_id);
                let first_prompt = !snapshot.activity.events.iter().any(|event| event.session_id.as_deref() == Some(&session_id) && matches!(&event.payload, ActivityPayload::Message { text } if text.starts_with("You: ")));
                let text = if first_prompt {
                    format!("{text}\n\nTask context: {} ({}). Read its description and history with `radar card show \"{}\"`; keep progress updates on this task.", snapshot.card.title, self.card_id, self.card_id)
                } else {
                    text
                };
                self.command(Command::AgentPrompt {
                    id: self.agent_id.clone(),
                    text,
                })
            }
            Action::Command(command) => self.command(command),
            Action::Adopt(session_id) => {
                if !self
                    .sessions()?
                    .iter()
                    .any(|session| session.session_id == session_id)
                {
                    bail!(
                        "This conversation is no longer offered by the provider; refresh the list"
                    );
                }
                let snapshot = self.importable()?;
                let agent = snapshot.agent.context("Provider stopped")?;
                if agent.acp_session_id.as_deref() == Some(&session_id) {
                    return Ok(());
                }
                self.claim(&snapshot.card)?;
                self.command(Command::AgentStop {
                    id: self.agent_id.clone(),
                })?;
                let deadline = Instant::now() + Duration::from_secs(5);
                while self.read()?.agent.as_ref().is_some_and(agent_live) {
                    if Instant::now() >= deadline {
                        bail!("The current conversation has not stopped yet; retry when it stops");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                let db = Db::open(&crate::config::Paths::with_root(&self.home))?;
                db.bind_session(
                    self.project.id,
                    &self.agent_id,
                    &agent.provider,
                    &session_id,
                )?;
                self.execute(Action::Start(None))
            }
        }
    }

    fn command(&self, command: Command) -> Result<()> {
        match Client::request(&self.home, command)? {
            Response::Ok
            | Response::AgentModes(_)
            | Response::AgentConfigOptions(_)
            | Response::AttentionChanged(_) => Ok(()),
            other => bail!("Unexpected conversation response: {other:?}"),
        }
    }
}

fn agent_live(agent: &AgentStatus) -> bool {
    matches!(agent.state.as_str(), "starting" | "ready" | "working")
}

enum Action {
    Start(Option<String>),
    Prompt(String),
    Command(Command),
    Adopt(String),
}
enum Reply {
    Poll(std::result::Result<Box<Snapshot>, String>),
    Action(std::result::Result<(), String>, bool),
    Sessions(std::result::Result<Vec<AgentSessionInfo>, String>),
}

struct Conversation {
    target: Target,
    window: gtk::Window,
    state: gtk::Label,
    error: gtk::Label,
    agent_picker: gtk::DropDown,
    programs: Vec<String>,
    start: gtk::Button,
    send: gtk::Button,
    cancel: gtk::Button,
    stop: gtk::Button,
    list_sessions: gtk::Button,
    imports: gtk::Box,
    session_picker: gtk::DropDown,
    session_detail: gtk::Label,
    adopt: gtk::Button,
    sessions: RefCell<Vec<AgentSessionInfo>>,
    prompt: gtk::TextView,
    history: gtk::TextView,
    controls: gtk::Box,
    attention: gtk::Box,
    wait: gtk::Box,
    spinner: gtk::Spinner,
    wait_note: gtk::Label,
    waiting_since: Cell<Option<i64>>,
    sending: Cell<bool>,
    snapshot: RefCell<Option<Snapshot>>,
    options: RefCell<Option<(Option<AgentModes>, Vec<AgentConfigOption>)>>,
    attention_state: RefCell<Vec<crate::session::activity::Attention>>,
    pending: Cell<bool>,
    polling: Cell<bool>,
    closed: Cell<bool>,
    tx: mpsc::Sender<Reply>,
}

/// Closing this window leaves its daemon session alive, just like a terminal.
pub(super) fn open(app: &App, project_id: i64, card_id: &str) {
    let Some(project) = app
        .projects
        .borrow()
        .iter()
        .find(|project| project.id == project_id)
        .cloned()
    else {
        return;
    };
    let id = app
        .card_acp_agent(project_id, card_id)
        .map(|agent| agent.id)
        .unwrap_or_else(|| card_agent_id(project_id, card_id));
    let prefs = app
        .db
        .project_settings(project_id)
        .unwrap_or_default()
        .apply_to(&app.db.preferences().unwrap_or_default());
    let candidates: Vec<_> = crate::programs::candidates_for_slot(Slot::Agent, &prefs)
        .into_iter()
        .filter(|program| crate::session::agent::default_acp_args(&program.id).is_some())
        .collect();
    let target = Target {
        home: app.session_home.clone(),
        project,
        card_id: card_id.into(),
        agent_id: id,
    };
    let title = app
        .card_title(project_id, card_id)
        .unwrap_or_else(|| card_id.into());
    let (view, rx) = conversation_window(target, &title, candidates, &app.window);
    let weak = Rc::downgrade(&view);
    view.list_sessions.connect_clicked(move |_| {
        if let Some(view) = weak.upgrade() {
            view.list_sessions();
        }
    });
    let weak = Rc::downgrade(&view);
    view.adopt.connect_clicked(move |_| {
        if let Some(view) = weak.upgrade() {
            let id = view
                .sessions
                .borrow()
                .get(view.session_picker.selected() as usize)
                .map(|session| session.session_id.clone());
            if let Some(id) = id {
                view.run(Action::Adopt(id));
            }
        }
    });
    for (button, kind) in [
        (&view.start, 0),
        (&view.send, 1),
        (&view.cancel, 2),
        (&view.stop, 3),
    ] {
        let weak = Rc::downgrade(&view);
        button.connect_clicked(move |_| {
            let Some(view) = weak.upgrade() else {
                return;
            };
            let action = match kind {
                0 => Action::Start(
                    view.programs
                        .get(view.agent_picker.selected() as usize)
                        .cloned(),
                ),
                1 => {
                    view.send_prompt();
                    return;
                }
                2 => Action::Command(Command::AgentCancel {
                    id: view.target.agent_id.clone(),
                }),
                _ => Action::Command(Command::AgentStop {
                    id: view.target.agent_id.clone(),
                }),
            };
            view.run(action);
        });
    }
    let weak = Rc::downgrade(&view);
    view.window.connect_close_request(move |_| {
        if let Some(view) = weak.upgrade() {
            view.closed.set(true);
        }
        gtk::glib::Propagation::Proceed
    });
    let keys = gtk::EventControllerKey::new();
    let weak = Rc::downgrade(&view);
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        if key == gtk::gdk::Key::Return && modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK)
        {
            if let Some(view) = weak.upgrade() {
                view.send_prompt();
            }
            return gtk::glib::Propagation::Stop;
        }
        gtk::glib::Propagation::Proceed
    });
    view.prompt.add_controller(keys);
    view.window.present();
    view.poll();
    let mut last_poll = Instant::now();
    gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
        if view.closed.get() {
            return gtk::glib::ControlFlow::Break;
        }
        while let Ok(reply) = rx.try_recv() {
            match reply {
                Reply::Poll(result) => {
                    view.polling.set(false);
                    match result {
                        Ok(snapshot) => view.apply(*snapshot),
                        Err(error) => view.show_error(&error),
                    }
                }
                Reply::Action(result, clear_prompt) => {
                    view.pending.set(false);
                    view.sending.set(false);
                    // A rejected change must restore the authoritative value.
                    *view.options.borrow_mut() = None;
                    match result {
                        Ok(()) => {
                            view.error.set_visible(false);
                            if clear_prompt {
                                view.prompt.buffer().set_text("");
                            }
                        }
                        Err(error) => view.show_error(&error),
                    }
                    view.poll();
                }
                Reply::Sessions(result) => {
                    view.pending.set(false);
                    match result {
                        Ok(sessions) => {
                            view.error.set_visible(false);
                            view.show_sessions(sessions);
                        }
                        Err(error) => view.show_error(&error),
                    }
                    view.poll();
                }
            }
        }
        if last_poll.elapsed() >= Duration::from_secs(1) {
            view.poll();
            last_poll = Instant::now();
        }
        view.enable_controls();
        gtk::glib::ControlFlow::Continue
    });
}

fn conversation_window(
    target: Target,
    title: &str,
    candidates: Vec<crate::programs::Program>,
    parent: &impl IsA<gtk::Window>,
) -> (Rc<Conversation>, mpsc::Receiver<Reply>) {
    let labels: Vec<&str> = candidates
        .iter()
        .map(|program| program.name.as_str())
        .collect();
    let picker = gtk::DropDown::from_strings(&labels);
    let window = gtk::Window::builder()
        .title(format!("{title} — Agent conversation"))
        .default_width(760)
        .default_height(700)
        .transient_for(parent)
        .build();
    let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    page.add_css_class("agent-conversation");
    page.set_margin_top(18);
    page.set_margin_bottom(18);
    page.set_margin_start(18);
    page.set_margin_end(18);
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let state = gtk::Label::new(Some("Loading conversation…"));
    state.set_xalign(0.0);
    state.set_hexpand(true);
    state.set_wrap(true);
    let start = gtk::Button::with_label("Start conversation");
    let cancel = gtk::Button::with_label("Cancel turn");
    let stop = gtk::Button::with_label("Stop agent");
    toolbar.append(&state);
    toolbar.append(&picker);
    toolbar.append(&start);
    toolbar.append(&cancel);
    toolbar.append(&stop);
    page.append(&toolbar);
    let list_sessions = gtk::Button::with_label("Browse existing conversations…");
    list_sessions.set_halign(gtk::Align::Start);
    page.append(&list_sessions);
    let imports = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let import_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let session_picker = gtk::DropDown::from_strings(&[]);
    session_picker.set_hexpand(true);
    let adopt = gtk::Button::with_label("Open selected conversation");
    import_row.append(&session_picker);
    import_row.append(&adopt);
    imports.append(&import_row);
    let session_detail = board::activity_label("", true);
    session_detail.set_selectable(true);
    imports.append(&session_detail);
    imports.append(&board::activity_label(
        "Opening a saved conversation stops the current one. Task history is retained.",
        true,
    ));
    imports.set_visible(false);
    page.append(&imports);
    let controls = gtk::Box::new(gtk::Orientation::Vertical, 6);
    page.append(&controls);
    let wait = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let spinner = gtk::Spinner::new();
    let wait_note = board::activity_label("", true);
    wait_note.set_hexpand(true);
    wait.append(&spinner);
    wait.append(&wait_note);
    wait.set_visible(false);
    page.append(&wait);
    let error = board::activity_label("", false);
    error.add_css_class("error");
    error.set_visible(false);
    page.append(&error);
    page.append(&board::activity_label("Conversation activity", true));
    let history = gtk::TextView::new();
    history.add_css_class("conversation-history");
    history.set_left_margin(8);
    history.set_right_margin(8);
    history.set_editable(false);
    history.set_cursor_visible(false);
    history.set_wrap_mode(gtk::WrapMode::WordChar);
    history.set_top_margin(8);
    history.set_bottom_margin(8);
    let history_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&history)
        .build();
    page.append(&history_scroll);
    let attention = gtk::Box::new(gtk::Orientation::Vertical, 6);
    page.append(&attention);
    page.append(&board::activity_label("Message", true));
    let prompt = gtk::TextView::new();
    prompt.add_css_class("conversation-prompt");
    prompt.set_left_margin(8);
    prompt.set_right_margin(8);
    prompt.set_wrap_mode(gtk::WrapMode::WordChar);
    prompt.set_top_margin(8);
    prompt.set_bottom_margin(8);
    let prompt_scroll = gtk::ScrolledWindow::builder()
        .min_content_height(90)
        .max_content_height(180)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&prompt)
        .build();
    page.append(&prompt_scroll);
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let note = board::activity_label(
        "Reports appear when the agent finishes a turn. Ctrl+Enter sends.",
        true,
    );
    note.set_hexpand(true);
    footer.append(&note);
    let send = gtk::Button::with_label("Send");
    send.add_css_class("suggested-action");
    footer.append(&send);
    page.append(&footer);
    window.set_child(Some(&page));
    let (tx, rx) = mpsc::channel();
    let view = Rc::new(Conversation {
        target,
        window,
        state,
        error,
        agent_picker: picker,
        programs: candidates.into_iter().map(|program| program.id).collect(),
        start,
        send,
        cancel,
        stop,
        list_sessions,
        imports,
        session_picker,
        session_detail,
        adopt,
        sessions: RefCell::new(Vec::new()),
        prompt,
        history,
        controls,
        attention,
        wait,
        spinner,
        wait_note,
        waiting_since: Cell::new(None),
        sending: Cell::new(false),
        snapshot: RefCell::new(None),
        options: RefCell::new(None),
        attention_state: RefCell::new(Vec::new()),
        pending: Cell::new(false),
        polling: Cell::new(false),
        closed: Cell::new(false),
        tx,
    });
    let weak = Rc::downgrade(&view);
    view.session_picker.connect_selected_notify(move |_| {
        if let Some(view) = weak.upgrade() {
            view.update_session_detail();
            view.enable_controls();
        }
    });
    view.enable_controls();
    (view, rx)
}

impl Conversation {
    fn list_sessions(&self) {
        if !self.list_sessions.is_sensitive() || self.pending.replace(true) {
            return;
        }
        self.enable_controls();
        let target = self.target.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Reply::Sessions(
                target.sessions().map_err(|error| error.to_string()),
            ));
        });
    }

    fn show_sessions(&self, sessions: Vec<AgentSessionInfo>) {
        let labels: Vec<_> = sessions
            .iter()
            .map(|session| {
                session
                    .title
                    .as_deref()
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or(&session.session_id)
            })
            .collect();
        let model = gtk::StringList::new(&labels);
        *self.sessions.borrow_mut() = sessions;
        self.session_picker.set_model(Some(&model));
        self.session_picker.set_selected(0);
        self.update_session_detail();
        if self.sessions.borrow().is_empty() {
            self.show_error("The provider has no saved conversations for this project.");
        }
        self.enable_controls();
    }

    fn update_session_detail(&self) {
        let sessions = self.sessions.borrow();
        let text = sessions
            .get(self.session_picker.selected() as usize)
            .map(|session| {
                let updated = session
                    .updated_at
                    .as_ref()
                    .map(|time| format!(" · Updated {time}"))
                    .unwrap_or_default();
                format!("{}{updated}", session.session_id)
            })
            .unwrap_or_default();
        self.session_detail.set_text(&text);
    }

    fn show_error(&self, error: &str) {
        self.error.set_text(error);
        self.error.set_visible(true);
    }

    fn poll(&self) {
        if self.pending.get() || self.polling.replace(true) {
            return;
        }
        let target = self.target.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Reply::Poll(
                target
                    .read()
                    .map(Box::new)
                    .map_err(|error| error.to_string()),
            ));
        });
    }

    fn run(&self, action: Action) {
        if self.pending.replace(true) {
            return;
        }
        let clear = matches!(action, Action::Prompt(_));
        self.sending.set(clear);
        self.enable_controls();
        let target = self.target.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Reply::Action(
                target.execute(action).map_err(|error| error.to_string()),
                clear,
            ));
        });
    }

    fn send_prompt(&self) {
        if !self.send.is_sensitive() {
            return;
        }
        let buffer = self.prompt.buffer();
        let text = buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string();
        if text.trim().is_empty() {
            self.show_error("Write a message first.");
            return;
        }
        if text.len() > 16 * 1024 {
            self.show_error("Keep this message under 16 KB.");
            return;
        }
        self.run(Action::Prompt(text));
    }

    fn enable_controls(&self) {
        let snapshot = self.snapshot.borrow();
        let agent = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.agent.as_ref());
        let live = agent.is_some_and(agent_live);
        let writable = snapshot.as_ref().is_some_and(|snapshot| {
            !snapshot.card.done
                && !snapshot.other_worker
                && snapshot
                    .card
                    .claim
                    .as_ref()
                    .is_none_or(|claim| claim == &self.target.agent_id)
        });
        let ready = agent.is_some_and(|agent| agent.state == "ready") && writable;
        let attention = snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.activity.attention.iter().any(|request| {
                request.card_id.as_deref() == Some(&self.target.card_id) && request.is_unresolved()
            })
        });
        let bound = snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.binding.is_some()
                || snapshot
                    .agent
                    .as_ref()
                    .is_some_and(|agent| agent.acp_session_id.is_some())
        });
        self.start.set_visible(!live);
        self.start.set_label(if bound {
            "Resume conversation"
        } else {
            "Start conversation"
        });
        let can_resume = agent
            .and_then(|agent| agent.capabilities.as_ref())
            .is_none_or(|capabilities| capabilities.load_session);
        self.start.set_sensitive(
            !self.pending.get()
                && !live
                && writable
                && if bound {
                    can_resume
                } else {
                    !self.programs.is_empty()
                },
        );
        self.agent_picker.set_visible(!live && !bound);
        self.send
            .set_sensitive(!self.pending.get() && ready && !attention);
        self.prompt.set_sensitive(!self.pending.get());
        self.controls
            .set_sensitive(!self.pending.get() && ready && !attention);
        self.cancel.set_sensitive(
            !self.pending.get() && agent.is_some_and(|agent| agent.state == "working"),
        );
        self.stop.set_sensitive(!self.pending.get() && live);
        let can_list = agent
            .and_then(|agent| agent.capabilities.as_ref())
            .is_some_and(|capabilities| capabilities.session_list && capabilities.load_session);
        self.list_sessions.set_visible(live && can_list);
        let can_import = !self.pending.get() && ready && !attention && can_list;
        self.list_sessions.set_sensitive(can_import);
        self.imports
            .set_visible(live && can_list && !self.sessions.borrow().is_empty());
        self.imports.set_sensitive(can_import);
        self.adopt.set_sensitive(
            can_import
                && self
                    .sessions
                    .borrow()
                    .get(self.session_picker.selected() as usize)
                    .is_some_and(|session| {
                        agent.is_none_or(|agent| {
                            agent.acp_session_id.as_deref() != Some(&session.session_id)
                        })
                    }),
        );
        let working = agent.is_some_and(|agent| agent.state == "working");
        let starting = agent.is_some_and(|agent| agent.state == "starting");
        let animate = !attention && (working || starting || self.sending.get());
        self.wait.set_visible(attention || animate);
        self.spinner.set_visible(animate);
        self.spinner.set_spinning(animate);
        let note = if attention {
            "Waiting for your response. Answer the request below to continue.".into()
        } else if self.sending.get() {
            "Sending your message…".into()
        } else if starting {
            "Connecting to the agent… You can close this window and return later.".into()
        } else if working {
            let now = crate::session::catalog::now_millis();
            let started = self.waiting_since.get().unwrap_or(now);
            let seconds = now.saturating_sub(started).max(0) / 1000;
            let elapsed = if seconds < 60 {
                format!("{seconds}s")
            } else {
                format!("{}m {:02}s", seconds / 60, seconds % 60)
            };
            format!("Waiting for reply · {elapsed}. The reply appears when the turn finishes. You can draft your next message or return later.")
        } else {
            String::new()
        };
        if self.wait_note.text() != note {
            self.wait_note.set_text(&note);
        }
    }

    fn apply(self: &Rc<Self>, snapshot: Snapshot) {
        if snapshot
            .agent
            .as_ref()
            .is_some_and(|agent| agent.state == "working")
        {
            let latest_state = snapshot.activity.events.iter().rev().find(|event| {
                event.card_id.as_deref() == Some(&self.target.card_id)
                    && matches!(event.payload, ActivityPayload::AgentState { .. })
            });
            let started = latest_state
                .and_then(|event| {
                    matches!(
                        event.payload,
                        ActivityPayload::AgentState {
                            state: crate::session::activity::AgentState::Working,
                            ..
                        }
                    )
                    .then_some(event.at_millis)
                })
                .or(self.waiting_since.get())
                .unwrap_or_else(crate::session::catalog::now_millis);
            self.waiting_since.set(Some(started));
        } else {
            self.waiting_since.set(None);
        }
        let text = match snapshot.agent.as_ref() {
            Some(agent) => format!(
                "{} · {}{}",
                crate::programs::by_id(&agent.provider)
                    .map(|program| program.name)
                    .unwrap_or_else(|| agent.provider.clone()),
                match agent.state.as_str() {
                    "ready" => "Ready",
                    "working" => "Working",
                    "starting" => "Connecting",
                    "exited" => "Stopped",
                    "failed" => "Failed",
                    other => other,
                },
                agent
                    .detail
                    .as_ref()
                    .map(|detail| format!(" · {detail}"))
                    .unwrap_or_default()
            ),
            None => "No conversation running".into(),
        };
        self.state.set_text(&text);
        let history = snapshot
            .activity
            .events
            .iter()
            .filter(|event| event.card_id.as_deref() == Some(&self.target.card_id))
            .filter_map(|event| match &event.payload {
                ActivityPayload::Message { text } => Some(text.clone()),
                ActivityPayload::AgentState {
                    message: Some(message),
                    ..
                } => Some(message.clone()),
                ActivityPayload::SessionLifecycle { state, detail } => Some(format!(
                    "Agent {state}{}",
                    detail
                        .as_ref()
                        .map(|detail| format!(": {detail}"))
                        .unwrap_or_default()
                )),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let buffer = self.history.buffer();
        if buffer.text(&buffer.start_iter(), &buffer.end_iter(), false) != history {
            buffer.set_text(&history);
        }
        let options = snapshot
            .agent
            .as_ref()
            .map(|agent| (agent.modes.clone(), agent.config_options.clone()));
        if *self.options.borrow() != options {
            *self.options.borrow_mut() = options.clone();
            self.render_options(options);
        }
        let attention: Vec<_> = snapshot
            .activity
            .attention
            .iter()
            .filter(|request| {
                request.card_id.as_deref() == Some(&self.target.card_id) && request.is_unresolved()
            })
            .cloned()
            .collect();
        if *self.attention_state.borrow() != attention {
            *self.attention_state.borrow_mut() = attention.clone();
            while let Some(child) = self.attention.first_child() {
                self.attention.remove(&child);
            }
            for request in attention {
                self.attention
                    .append(&board::activity_label(&request.reason, false));
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                for (kind, label, response) in [
                    (
                        AttentionActionKind::Approve,
                        "Approve",
                        AttentionResponse::Approve,
                    ),
                    (AttentionActionKind::Deny, "Deny", AttentionResponse::Deny),
                    (
                        AttentionActionKind::Dismiss,
                        "Dismiss",
                        AttentionResponse::Dismiss,
                    ),
                ] {
                    if !request.allowed_actions.contains(&kind) {
                        continue;
                    }
                    let button = gtk::Button::with_label(label);
                    let weak = Rc::downgrade(self);
                    let request = request.clone();
                    button.connect_clicked(move |_| {
                        if let Some(view) = weak.upgrade() {
                            view.respond(&request, response.clone());
                        }
                    });
                    row.append(&button);
                }
                if request
                    .allowed_actions
                    .contains(&AttentionActionKind::Answer)
                {
                    let answer = gtk::Entry::new();
                    answer.set_hexpand(true);
                    row.append(&answer);
                    let send = gtk::Button::with_label("Answer");
                    let weak = Rc::downgrade(self);
                    let request = request.clone();
                    send.connect_clicked(move |_| {
                        if let Some(view) = weak.upgrade() {
                            if !answer.text().trim().is_empty() {
                                view.respond(
                                    &request,
                                    AttentionResponse::Answer(answer.text().into()),
                                );
                            }
                        }
                    });
                    row.append(&send);
                }
                self.attention.append(&row);
            }
        }
        *self.snapshot.borrow_mut() = Some(snapshot);
        self.enable_controls();
    }

    fn respond(&self, request: &crate::session::activity::Attention, response: AttentionResponse) {
        self.run(Action::Command(Command::ChangeAttention(ChangeAttention {
            project_id: self.target.project.id,
            request_id: request.id.clone(),
            command_id: super::gui_command_id("acp-answer"),
            expected_revision: request.revision,
            change: AttentionChange::Respond(response),
        })));
    }

    fn render_options(
        self: &Rc<Self>,
        options: Option<(Option<AgentModes>, Vec<AgentConfigOption>)>,
    ) {
        while let Some(child) = self.controls.first_child() {
            self.controls.remove(&child);
        }
        let Some((modes, options)) = options else {
            return;
        };
        if !options.iter().any(|option| option.category == "mode") {
            if let Some(modes) = modes {
                self.select_option(
                    "Mode",
                    &modes.current,
                    modes
                        .available
                        .into_iter()
                        .map(|mode| (mode.id, mode.name))
                        .collect(),
                    None,
                );
            }
        }
        for option in options {
            match option.kind {
                AgentConfigKind::Select { current, options } => self.select_option(
                    &option.name,
                    &current,
                    options
                        .into_iter()
                        .map(|option| (option.id, option.name))
                        .collect(),
                    Some(option.id),
                ),
                AgentConfigKind::Boolean { current } => self.select_option(
                    &option.name,
                    if current { "true" } else { "false" },
                    vec![("false".into(), "Off".into()), ("true".into(), "On".into())],
                    Some(option.id),
                ),
            }
        }
    }

    fn select_option(
        self: &Rc<Self>,
        name: &str,
        current: &str,
        mut values: Vec<(String, String)>,
        config_id: Option<String>,
    ) {
        if !values.iter().any(|(id, _)| id == current) {
            values.push((current.into(), format!("{current} (current)")));
        }
        let labels: Vec<&str> = values.iter().map(|(_, name)| name.as_str()).collect();
        let dropdown = gtk::DropDown::from_strings(&labels);
        dropdown.set_selected(
            values
                .iter()
                .position(|(id, _)| id == current)
                .map(|index| index as u32)
                .unwrap_or(gtk::INVALID_LIST_POSITION),
        );
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let label = board::activity_label(name, true);
        label.set_hexpand(true);
        row.append(&label);
        row.append(&dropdown);
        self.controls.append(&row);
        let weak = Rc::downgrade(self);
        let current = current.to_string();
        dropdown.connect_selected_notify(move |dropdown| {
            let Some((value, _)) = values.get(dropdown.selected() as usize) else {
                return;
            };
            if value == &current {
                return;
            }
            if let Some(view) = weak.upgrade() {
                let command = match &config_id {
                    Some(config_id) => Command::AgentSetConfigOption {
                        id: view.target.agent_id.clone(),
                        config_id: config_id.clone(),
                        value: value.clone(),
                    },
                    None => Command::AgentSetMode {
                        id: view.target.agent_id.clone(),
                        mode_id: value.clone(),
                    },
                };
                view.run(Action::Command(command));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Harness {
        home: tempfile::TempDir,
        target: Target,
        server: Option<std::thread::JoinHandle<()>>,
        program: String,
    }

    impl Harness {
        fn new() -> Self {
            let home = tempfile::tempdir().unwrap();
            let paths = crate::config::Paths::with_root(home.path());
            let db = Db::open(&paths).unwrap();
            let root = home.path().join("project");
            std::fs::create_dir(&root).unwrap();
            let project = db.add_project(&root).unwrap();
            let daemon = daemon::Server::bind(home.path()).unwrap();
            let server = std::thread::spawn(move || daemon.run().unwrap());
            let card = daemon::board_card_add(
                home.path(),
                project.id,
                None,
                "Test conversation",
                "Keep the exact conversation",
                None,
                "acp-ui-card",
            )
            .unwrap()
            .card;
            let target = Target {
                home: home.path().into(),
                project,
                agent_id: card_agent_id(card.project_id, &card.id),
                card_id: card.id,
            };
            let fixture = home.path().join("fake-agent");
            std::fs::copy(
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/acp_fake_agent.py"
                ),
                &fixture,
            )
            .unwrap();
            std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self {
                home,
                target,
                server: Some(server),
                program: fixture.to_string_lossy().into_owned(),
            }
        }

        fn wait(&self, mut predicate: impl FnMut(&Snapshot) -> bool) -> Snapshot {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let snapshot = self.target.read().unwrap();
                if predicate(&snapshot) {
                    return snapshot;
                }
                assert!(
                    Instant::now() < deadline,
                    "conversation never reached the expected state"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = Client::request(self.home.path(), Command::Shutdown);
            if let Some(server) = self.server.take() {
                server.join().unwrap();
            }
        }
    }

    #[test]
    fn native_conversation_claims_reports_switches_and_resumes_exactly() {
        let harness = Harness::new();
        harness
            .target
            .execute(Action::Start(Some(harness.program.clone())))
            .unwrap();
        harness.wait(|snapshot| {
            snapshot
                .agent
                .as_ref()
                .is_some_and(|agent| agent.state == "ready")
        });
        harness
            .target
            .execute(Action::Prompt("show radar environment".into()))
            .unwrap();
        let snapshot = harness.wait(|snapshot| snapshot.activity.events.iter().any(|event| matches!(&event.payload, ActivityPayload::Message { text } if text.starts_with("echo:"))));
        assert_eq!(
            snapshot.card.claim.as_deref(),
            Some(harness.target.agent_id.as_str())
        );
        let messages: Vec<_> = snapshot
            .activity
            .events
            .iter()
            .filter_map(|event| match &event.payload {
                ActivityPayload::Message { text } => Some(text),
                _ => None,
            })
            .collect();
        assert!(messages[0].starts_with("You: show radar environment"));
        let reply = messages
            .iter()
            .find(|text| text.starts_with("echo:"))
            .unwrap();
        for value in [
            &harness.target.agent_id,
            &harness.target.card_id,
            &harness.target.home.to_string_lossy().into_owned(),
        ] {
            assert!(reply.contains(value));
        }
        harness
            .target
            .execute(Action::Command(Command::AgentSetMode {
                id: harness.target.agent_id.clone(),
                mode_id: "review".into(),
            }))
            .unwrap();
        harness
            .target
            .execute(Action::Command(Command::AgentSetConfigOption {
                id: harness.target.agent_id.clone(),
                config_id: "model".into(),
                value: "model-b".into(),
            }))
            .unwrap();
        let snapshot = harness.target.read().unwrap();
        let agent = snapshot.agent.unwrap();
        assert_eq!(agent.modes.unwrap().current, "review");
        assert!(agent
            .config_options
            .iter()
            .any(|option| option.current_label().as_deref() == Some("Model B")));
        assert!(harness
            .target
            .execute(Action::Command(Command::AgentSetConfigOption {
                id: harness.target.agent_id.clone(),
                config_id: "model".into(),
                value: "invented-model".into()
            }))
            .is_err());
        harness
            .target
            .execute(Action::Command(Command::AgentStop {
                id: harness.target.agent_id.clone(),
            }))
            .unwrap();
        let stopped = harness.wait(|snapshot| {
            snapshot
                .agent
                .as_ref()
                .is_some_and(|agent| agent.state == "exited")
        });
        let exact_id = stopped.agent.as_ref().unwrap().acp_session_id.clone();
        // The durable binding survives releasing the claim; no last-session fallback.
        daemon::board_card_claim(
            harness.home.path(),
            harness.target.project.id,
            &harness.target.card_id,
            None,
            Some(stopped.card.revision),
            "release-acp-ui",
        )
        .unwrap();
        harness.target.execute(Action::Start(None)).unwrap();
        let resumed = harness.wait(|snapshot| {
            snapshot
                .agent
                .as_ref()
                .is_some_and(|agent| agent.state == "ready")
        });
        assert_eq!(resumed.agent.unwrap().acp_session_id, exact_id);
        daemon::board_card_move(
            harness.home.path(),
            harness.target.project.id,
            &harness.target.card_id,
            "Done",
            None,
            "close-acp-ui",
        )
        .unwrap();
        assert!(harness
            .target
            .execute(Action::Prompt("more work".into()))
            .unwrap_err()
            .to_string()
            .contains("Reopen"));
    }

    #[test]
    fn native_conversation_does_not_take_another_workers_claim() {
        let harness = Harness::new();
        let card = harness.target.read().unwrap().card;
        daemon::board_card_claim(
            harness.home.path(),
            card.project_id,
            &card.id,
            Some("another-worker"),
            Some(card.revision),
            "other-claim",
        )
        .unwrap();
        assert!(harness
            .target
            .execute(Action::Start(Some(harness.program.clone())))
            .unwrap_err()
            .to_string()
            .contains("another-worker"));
        assert_eq!(
            harness.target.read().unwrap().card.claim.as_deref(),
            Some("another-worker")
        );
        assert!(harness.target.read().unwrap().agent.is_none());
    }

    #[test]
    fn native_conversation_import_validates_and_reopens_the_selected_session() {
        let harness = Harness::new();
        harness
            .target
            .execute(Action::Start(Some(harness.program.clone())))
            .unwrap();
        harness.wait(|snapshot| {
            snapshot
                .agent
                .as_ref()
                .is_some_and(|agent| agent.state == "ready")
        });
        let sessions = harness.target.sessions().unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(harness
            .target
            .execute(Action::Adopt("missing-session".into()))
            .is_err());
        assert_eq!(
            harness
                .target
                .read()
                .unwrap()
                .agent
                .unwrap()
                .acp_session_id
                .as_deref(),
            Some("sess_fake_1")
        );
        harness
            .target
            .execute(Action::Adopt("sess_imported_2".into()))
            .unwrap();
        let imported = harness.wait(|snapshot| {
            snapshot
                .agent
                .as_ref()
                .is_some_and(|agent| agent.state == "ready")
        });
        assert_eq!(
            imported.agent.unwrap().acp_session_id.as_deref(),
            Some("sess_imported_2")
        );
        assert_eq!(imported.binding.unwrap().1, "sess_imported_2");
        harness
            .target
            .execute(Action::Prompt("wait for cancellation".into()))
            .unwrap();
        harness.wait(|snapshot| {
            snapshot
                .agent
                .as_ref()
                .is_some_and(|agent| agent.state == "working")
        });
        assert!(harness
            .target
            .execute(Action::Adopt("sess_fake_1".into()))
            .unwrap_err()
            .to_string()
            .contains("ready"));
        harness
            .target
            .command(Command::AgentCancel {
                id: harness.target.agent_id.clone(),
            })
            .unwrap();
        let ready = harness.wait(|snapshot| {
            snapshot
                .agent
                .as_ref()
                .is_some_and(|agent| agent.state == "ready")
        });
        daemon::board_card_claim(
            harness.home.path(),
            ready.card.project_id,
            &ready.card.id,
            Some("another-worker"),
            Some(ready.card.revision),
            "import-other-claim",
        )
        .unwrap();
        assert!(harness
            .target
            .execute(Action::Adopt("sess_fake_1".into()))
            .is_err());
        assert_eq!(
            harness.target.read().unwrap().binding.unwrap().1,
            "sess_imported_2"
        );
    }

    #[test]
    #[ignore = "requires a private D-Bus session and GTK display"]
    fn conversation_controls_follow_advertised_options_without_losing_drafts() {
        gtk::init().unwrap();
        let harness = Harness::new();
        harness
            .target
            .execute(Action::Start(Some(harness.program.clone())))
            .unwrap();
        let ready = harness.wait(|snapshot| {
            snapshot
                .agent
                .as_ref()
                .is_some_and(|agent| agent.state == "ready")
        });
        let parent = gtk::Window::new();
        let (view, _) = conversation_window(
            harness.target.clone(),
            "Test conversation",
            Vec::new(),
            &parent,
        );
        view.prompt.buffer().set_text("Unsent draft");
        view.apply(ready.clone());
        assert!(view.send.is_sensitive());
        assert!(!view.cancel.is_sensitive());
        assert!(view.stop.is_sensitive());
        assert!(!view.wait.get_visible());
        assert!(view.list_sessions.get_visible());
        assert!(view.list_sessions.is_sensitive());
        view.show_sessions(harness.target.sessions().unwrap());
        assert!(view.imports.get_visible());
        assert!(
            !view.adopt.is_sensitive(),
            "the current conversation is already open"
        );
        view.session_picker.set_selected(1);
        assert!(view.adopt.is_sensitive());
        assert!(view.session_detail.text().contains("sess_imported_2"));
        assert!(view.session_detail.text().contains("2026-10-04"));
        let mut row = view.controls.first_child();
        let mut count = 0;
        while let Some(current) = row {
            count += 1;
            row = current.next_sibling();
        }
        assert_eq!(
            count, 3,
            "only the advertised mode, model and boolean are shown"
        );
        let mut working = ready.clone();
        working.agent.as_mut().unwrap().state = "working".into();
        view.apply(working);
        assert!(view.wait.get_visible());
        assert!(view.spinner.is_spinning());
        assert!(view
            .wait_note
            .text()
            .contains("reply appears when the turn finishes"));
        view.waiting_since
            .set(Some(crate::session::catalog::now_millis() - 125_000));
        view.enable_controls();
        assert!(view.wait_note.text().contains("2m 05s"));
        assert!(!view.send.is_sensitive());
        assert!(view.cancel.is_sensitive());
        assert!(!view.list_sessions.is_sensitive());
        assert!(!view.imports.is_sensitive());
        assert_eq!(
            view.prompt.buffer().text(
                &view.prompt.buffer().start_iter(),
                &view.prompt.buffer().end_iter(),
                false
            ),
            "Unsent draft"
        );
        if let Ok(path) = std::env::var("RADAR_ACP_PREVIEW") {
            use gtk::gsk::prelude::GskRendererExt;
            let theme = super::super::Theme::load();
            super::super::style::install(&theme);
            view.window.present();
            let context = gtk::glib::MainContext::default();
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let root = view.window.child().unwrap();
            root.allocate(760, 700, -1, None);
            let snapshot = gtk::Snapshot::new();
            snapshot.append_color(
                &theme.background,
                &gtk::graphene::Rect::new(0.0, 0.0, 760.0, 700.0),
            );
            view.window.snapshot_child(&root, &snapshot);
            let node = snapshot
                .to_node()
                .expect("conversation produced a render node");
            let renderer = gtk::gsk::CairoRenderer::new();
            renderer.realize(None).unwrap();
            let texture = renderer.render_texture(&node, None);
            texture.save_to_png(path).unwrap();
            renderer.unrealize();
        }
        view.show_sessions(Vec::new());
        assert!(!view.imports.get_visible());
        assert!(!view.adopt.is_sensitive());
        assert!(view.error.text().contains("no saved conversations"));
        let mut unsupported = ready.clone();
        unsupported.agent.as_mut().unwrap().modes = None;
        unsupported.agent.as_mut().unwrap().config_options.clear();
        unsupported
            .agent
            .as_mut()
            .unwrap()
            .capabilities
            .as_mut()
            .unwrap()
            .session_list = false;
        view.apply(unsupported);
        assert!(!view.wait.get_visible());
        assert!(!view.spinner.is_spinning());
        assert!(view.waiting_since.get().is_none());
        assert!(view.controls.first_child().is_none());
        assert!(!view.list_sessions.get_visible());
        assert!(!view.imports.get_visible());
        let mut closed = ready;
        closed.card.done = true;
        view.apply(closed);
        assert!(!view.send.is_sensitive());
        assert!(!view.start.is_sensitive());
        harness
            .target
            .execute(Action::Prompt("ask for permission".into()))
            .unwrap();
        let attention = harness.wait(|snapshot| {
            snapshot
                .activity
                .attention
                .iter()
                .any(|request| request.is_unresolved())
        });
        view.apply(attention);
        assert!(view.wait.get_visible());
        assert!(!view.spinner.is_spinning());
        assert!(view.wait_note.text().contains("Waiting for your response"));
        assert!(view.cancel.is_sensitive());
        assert!(!view.send.is_sensitive());
        view.window.destroy();
    }
}
