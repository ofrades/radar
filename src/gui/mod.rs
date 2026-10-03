//! The app window.
//!
//! A project workspace is a handful of primitives — editor, agent, diff,
//! terminal, the board — and nothing else. Home leads; workspace tools decide
//! which primitives are on
//! screen, and the layout arranges them the same way every time. Each open
//! primitive is a panel of its own: panels tile the workspace, never share a
//! header. Drag a panel by its header and drop it on another's edge to split,
//! on its middle to swap places. Automatic tiling fits rows to the available
//! space; manual header drags override it until Auto arrange is chosen.
//!
//! Hiding a primitive detaches its widget; the program keeps running, so putting
//! the agent away for a moment never interrupts it.

mod activity_sign;
mod agents;
mod alert;
mod board;
mod card;
mod confetti;
mod dialogs;
mod home;
mod hud;
mod keynav;
mod live_agents;
mod markdown;
mod notify;
mod pane;
mod panel;
mod primitive;
mod split;
mod style;
pub mod term;
mod theme;
mod tiling;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adw::prelude::*;
use anyhow::Result;
use gtk::gio;
use gtk::glib;

use crate::config::Paths;
use crate::db::{
    workspace_restore_plan, Db, NewWorkspaceLayout, Project, Slot, TabKey, WorkspaceAxis,
    WorkspaceLayout, WorkspacePanel, WorkspaceState,
};
use crate::discover::{self, Candidate};
use crate::programs::{self, CommandSpec, Kind, LaunchOptions, Program};

use pane::Pane;
use panel::Panel;
use primitive::{label_for, Primitive};
pub use theme::Theme;

type SharedDb = Rc<Db>;

/// The four content primitives, in layout order: the agent leads, because that
/// is what the workspace is for.
const PRIMITIVES: [Slot; 4] = [Slot::Agent, Slot::Diff, Slot::Shell, Slot::Editor];

type ZoomState = (Vec<Rc<Panel>>, Option<split::Node<Panel>>);

pub(super) fn restore_scroll_position(scroller: &gtk::ScrolledWindow, position: f64) {
    let scroller = scroller.clone();
    glib::idle_add_local_once(move || {
        let adjustment = scroller.vadjustment();
        let settled = Rc::new(Cell::new(false));
        let apply = {
            let settled = settled.clone();
            move |adjustment: &gtk::Adjustment| {
                if settled.get() {
                    return;
                }
                let maximum = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
                let value = position.clamp(adjustment.lower(), maximum);
                adjustment.set_value(value);
                // Once the saved position is reachable, the restore is done.
                if value >= position - 0.5 {
                    settled.set(true);
                }
            }
        };
        apply(&adjustment);
        // The rebuilt child may not have its content allocated yet; re-apply
        // as it grows so a saved offset survives the first layout pass.
        adjustment.connect_upper_notify(move |adjustment| {
            apply(adjustment);
        });
    });
}

fn home_card_scroller(view: &gtk::Widget) -> Option<gtk::ScrolledWindow> {
    if !view.has_css_class("card-view") {
        return None;
    }
    view.last_child()?
        .first_child()?
        .downcast::<gtk::ScrolledWindow>()
        .ok()
}

/// Open the app.
pub fn run(paths: Paths, db: Db) -> Result<()> {
    crate::session::daemon::ensure_running(&paths.data_dir)?;
    let app = adw::Application::builder()
        .application_id("dev.omarchy.Radar")
        .build();
    let paths = Rc::new(paths);
    let db = Rc::new(db);
    let window = Rc::new(RefCell::new(None::<adw::ApplicationWindow>));

    app.connect_startup(|_| style::install(&Theme::load()));
    let window_for_activate = window.clone();
    app.connect_activate(move |app| {
        if let Some(window) = window_for_activate.borrow().as_ref() {
            window.present();
            return;
        }
        let window = build_window(app, &paths, &db);
        *window_for_activate.borrow_mut() = Some(window.clone());
        window.present();
    });
    // Closing the window hides the UI but leaves terminal children and their
    // PTYs running. Launching Radar again activates this same application.
    let _hold = app.hold();

    // A clean argv: GApplication would otherwise try to interpret radar's own
    // command line.
    let _ = app.run_with_args(&["radar"]);
    Ok(())
}

/// A project's workspace: its tabs, arranged into panels.
///
/// Each panel holds exactly one tab — one header, one program. Tabs are
/// per-key, so radar can run two agent panels side by side; the dock, the
/// chords and the HUD keep aiming at a primitive's first tab. The layout
/// fits panels to the available space, sharing the global Agents tiler.
struct Workspace {
    project: Project,
    /// Open tabs, by key.
    tabs: RefCell<HashMap<TabKey, Rc<Primitive>>>,
    /// The default program per primitive kind, used when a tab of that kind
    /// opens and remembered when one changes, even before it is opened.
    programs: RefCell<HashMap<Slot, String>>,
    /// The panes, in layout order.
    panels: RefCell<Vec<Rc<Panel>>>,
    /// Agent panels the user hid while their card requests them shown;
    /// refreshes keep that choice until the visibility policy changes.
    manual_hidden_agents: RefCell<HashMap<TabKey, String>>,
    /// Divider positions the user dragged, keyed by layout signature.
    positions: RefCell<HashMap<String, i32>>,
    /// Focusable dividers in the currently rendered layout.
    dividers: RefCell<Vec<gtk::Paned>>,
    /// Holds exactly one child: the layout built from `panels`.
    holder: gtk::Box,
    /// The panes to return to after a zoom, with the arrangement it had.
    zoom: RefCell<Option<ZoomState>>,
    /// The user's manual arrangement. None keeps responsive auto tiling;
    /// the first edge-drop split plants it, and pruning keeps it honest.
    tree: RefCell<Option<split::Node<Panel>>>,
    /// Divider drags are frequent; persist their last position once the drag
    /// settles rather than writing on every motion event.
    save_timeout: RefCell<Option<glib::SourceId>>,
}

impl Workspace {
    fn tab(&self, key: TabKey) -> Option<Rc<Primitive>> {
        self.tabs.borrow().get(&key).cloned()
    }

    /// The tab a key means: itself when it exists, else the first open tab of
    /// that primitive, else the key itself — the caller may be about to
    /// create it. The dock, the chords and the HUD speak in bare kinds, so
    /// they land on whatever tab of the kind exists.
    fn resolve_tab(&self, key: TabKey) -> TabKey {
        if self.tabs.borrow().contains_key(&key) {
            return key;
        }
        self.tabs_of_kind(key.slot)
            .into_iter()
            .next()
            .unwrap_or(key)
    }

    /// Every open tab of one primitive kind, oldest instance first.
    fn tabs_of_kind(&self, slot: Slot) -> Vec<TabKey> {
        let mut keys: Vec<TabKey> = self
            .tabs
            .borrow()
            .keys()
            .filter(|key| key.slot == slot)
            .copied()
            .collect();
        keys.sort();
        keys
    }

    /// The next free key for a kind: one past the highest open instance.
    fn next_key(&self, slot: Slot) -> TabKey {
        self.tabs_of_kind(slot)
            .last()
            .map(|key| key.next_instance())
            .unwrap_or_else(|| TabKey::first(slot))
    }

    fn panels(&self) -> Vec<Rc<Panel>> {
        self.panels.borrow().clone()
    }

    fn dividers(&self) -> Vec<gtk::Paned> {
        self.dividers.borrow().clone()
    }

    /// Which pane holds a tab.
    fn panel_of(&self, key: TabKey) -> Option<Rc<Panel>> {
        self.panels
            .borrow()
            .iter()
            .find(|panel| panel.contains(key))
            .cloned()
    }

    fn is_visible(&self, key: TabKey) -> bool {
        self.panel_of(key).is_some()
    }

    /// Which primitive kinds have a tab on screen — what the dock and the
    /// HUD's "on screen" marks show.
    fn visible_kinds(&self) -> Vec<Slot> {
        let mut kinds: Vec<Slot> = self
            .panels
            .borrow()
            .iter()
            .filter_map(|panel| panel.key())
            .map(|key| key.slot)
            .collect();
        kinds.sort_by_key(|slot| {
            PRIMITIVES
                .iter()
                .position(|other| other == slot)
                .unwrap_or(9)
        });
        kinds.dedup();
        kinds
    }

    fn push_panel(&self, panel: Rc<Panel>) {
        self.panels.borrow_mut().push(panel);
    }

    fn forget_panel(&self, panel: &Rc<Panel>) {
        self.panels
            .borrow_mut()
            .retain(|other| !Rc::ptr_eq(other, panel));
    }
}

struct App {
    db: SharedDb,
    session_home: PathBuf,
    theme: RefCell<Theme>,
    window: adw::ApplicationWindow,
    workspace_bar: gtk::Box,
    workspace_title: gtk::Label,
    toggles: RefCell<HashMap<Slot, gtk::ToggleButton>>,
    /// Home leads the dock; it is checked while the home panel shows.
    /// True while the home panel is on screen instead of a project's
    /// workspace — the empty state, or the user's explicit "go home".
    home_shown: Cell<bool>,
    stack: gtk::Stack,
    toasts: adw::ToastOverlay,
    /// The in-app activity alerts: shadcn-style cards, top-right, with sound.
    alerts: Rc<alert::Alerts>,
    /// The keyboard's own surface: primitives, actions and the keymap,
    /// floating over everything. One per window, not per workspace.
    hud: Rc<hud::Hud>,
    workspaces: RefCell<HashMap<i64, Rc<Workspace>>>,
    projects: RefCell<Vec<Project>>,
    agent_sessions: RefCell<live_agents::SessionIndex>,
    agent_tx: std::sync::mpsc::Sender<Result<live_agents::DiscoverySnapshot, String>>,
    agent_rx: RefCell<std::sync::mpsc::Receiver<Result<live_agents::DiscoverySnapshot, String>>>,
    agent_scan_pending: Cell<bool>,
    status: RefCell<HashMap<i64, crate::git::Status>>,
    status_tx: std::sync::mpsc::Sender<Vec<(i64, crate::git::Status)>>,
    status_rx: RefCell<std::sync::mpsc::Receiver<Vec<(i64, crate::git::Status)>>>,
    activity: RefCell<HashMap<i64, ProjectActivity>>,
    activity_online: RefCell<HashMap<i64, bool>>,
    activity_watchers: RefCell<HashMap<i64, ActivityWatcher>>,
    activity_tx: std::sync::mpsc::SyncSender<ActivityNotice>,
    activity_rx: RefCell<std::sync::mpsc::Receiver<ActivityNotice>>,
    /// The last board store state per project, so Home and card conversations
    /// render without re-fetching on every rebuild.
    board_states: RefCell<HashMap<i64, crate::session::board_store::BoardState>>,
    /// Where Home is drilled in: empty means the cockpit, otherwise a stack of
    /// board/card views with a way back.
    home_nav: RefCell<Vec<HomeView>>,
    board_summaries: RefCell<HashMap<i64, board::BoardSummary>>,
    notified_attention: RefCell<HashSet<(i64, String)>>,
    /// Attention responses the Home cockpit has sent but not yet heard back
    /// about. Home is rebuilt often, so the pending set outlives its widgets.
    home_pending_attention: Rc<RefCell<HashSet<String>>>,
    /// After a to-do is added from Home the cockpit is rebuilt, destroying the
    /// input that was focused. This remembers which project's input should get
    /// the keys back, so a human can add several to-dos in a row.
    home_focus_todo: Cell<Option<i64>>,
    home_todo_drafts: Rc<RefCell<HashMap<i64, String>>>,
    /// The confetti layer thrown when a to-do is completed.
    confetti: confetti::Confetti,
    /// The Agents page: every live session's real panel, tiled in the
    /// workspace's own arrangement — drag to split, resizable dividers, the
    /// works. A persistent stack page — Home's rebuild-on-every-event would
    /// tear attached terminals down mid-keystroke, so this page updates in
    /// place.
    agents_page: RefCell<Option<agents::AgentsPage>>,
    /// Sessions the human closed on the Agents page: the tile leaves the wall
    /// but the program keeps running, exactly like closing a workspace panel.
    /// The id leaves the set when its session ends.
    dismissed_agents: RefCell<HashSet<String>>,
    /// What each pane's program last said about itself — its name and its own
    /// live title, or its exit — keyed by (project, tab).
    header_info: RefCell<HashMap<(i64, TabKey), String>>,
    current: RefCell<Option<i64>>,
    // "Add a project": a search over the scan root that adds folders it finds,
    // and creates a folder for a typed name.
    add_list: gtk::ListBox,
    add_search: gtk::SearchEntry,
    add_root_button: gtk::Button,
    add_root: RefCell<PathBuf>,
    add_candidates: RefCell<Vec<Candidate>>,
    add_tx: std::sync::mpsc::Sender<(PathBuf, Vec<Candidate>)>,
    add_rx: RefCell<std::sync::mpsc::Receiver<(PathBuf, Vec<Candidate>)>>,
    /// Last real pointer movement over the window, in milliseconds of the
    /// glib monotonic clock. Enter events a mapped widget synthesizes under a
    /// parked pointer must not read as mouse intent.
    pointer_motion_ms: Cell<i64>,
    /// The project-card column count the current Home was built for, so a
    /// resize only rebuilds Home when the masonry actually changes shape.
    home_columns: Cell<usize>,
}

#[derive(Clone)]
enum HomeView {
    Project(i64),
    Card(i64, String),
    /// Navigation marker only: live panes remain on their persistent stack page.
    Agents,
    /// Home's combined "Add a project" picker (its own stack page).
    AddProject,
}

#[derive(Debug)]
enum ActivityNotice {
    Snapshot {
        project_id: i64,
        snapshot: crate::session::activity::ActivitySnapshot,
        replace_events: bool,
    },
    Event(crate::session::activity::ActivityEvent),
    Connection {
        project_id: i64,
        online: bool,
    },
    Mutation {
        project_id: i64,
        request_id: String,
        result: std::result::Result<Box<crate::session::activity::AttentionMutationResult>, String>,
    },
    /// A card comment sent from the GUI came back from the daemon (or failed).
    CardComment {
        project_id: i64,
        error: Option<String>,
    },
}

#[derive(Clone)]
struct ProjectActivity {
    snapshot: crate::session::activity::ActivitySnapshot,
}

impl ProjectActivity {
    fn empty(project_id: i64) -> Self {
        Self {
            snapshot: crate::session::activity::ActivitySnapshot {
                project_id,
                watermark: 0,
                events: Vec::new(),
                attention: Vec::new(),
                has_more: false,
            },
        }
    }

    fn merge_snapshot(
        &mut self,
        snapshot: crate::session::activity::ActivitySnapshot,
        replace_events: bool,
    ) {
        let fresh = snapshot.watermark >= self.snapshot.watermark;
        if replace_events && fresh {
            self.snapshot.events = snapshot.events;
        } else {
            for event in snapshot.events {
                self.insert_event(event);
            }
        }
        self.snapshot.project_id = snapshot.project_id;
        self.snapshot.watermark = self.snapshot.watermark.max(snapshot.watermark);
        if fresh {
            self.snapshot.attention = snapshot.attention;
            self.snapshot.has_more = snapshot.has_more;
        }
    }

    fn insert_event(&mut self, event: crate::session::activity::ActivityEvent) {
        if !self
            .snapshot
            .events
            .iter()
            .any(|current| current.id == event.id)
        {
            self.snapshot.events.push(event);
            self.snapshot.events.sort_by_key(|event| event.sequence);
            if self.snapshot.events.len() > 200 {
                let excess = self.snapshot.events.len() - 200;
                self.snapshot.events.drain(..excess);
            }
        }
    }

    fn apply_event(&mut self, event: crate::session::activity::ActivityEvent) {
        self.snapshot.watermark = self.snapshot.watermark.max(event.sequence);
        if let crate::session::activity::ActivityPayload::AttentionRequested {
            request_id,
            attention_kind,
            reason,
            allowed_actions,
        } = &event.payload
        {
            self.upsert_attention(crate::session::activity::Attention {
                id: request_id.clone(),
                source_event_id: event.id.clone(),
                project_id: event.project_id,
                session_id: event.session_id.clone(),
                card_id: event.card_id.clone(),
                kind: *attention_kind,
                reason: reason.clone(),
                allowed_actions: allowed_actions.clone(),
                created_at_millis: event.at_millis,
                seen_at_millis: None,
                acknowledged_at_millis: None,
                resolved_at_millis: None,
                resolution: None,
                revision: 1,
            });
        } else {
            use crate::session::activity::ActivityPayload;
            let changed = match &event.payload {
                ActivityPayload::AttentionSeen {
                    request_id,
                    revision,
                } => Some((
                    request_id.as_str(),
                    *revision,
                    Some(event.at_millis),
                    None,
                    None,
                )),
                ActivityPayload::AttentionAcknowledged {
                    request_id,
                    revision,
                } => Some((
                    request_id.as_str(),
                    *revision,
                    None,
                    Some(event.at_millis),
                    None,
                )),
                ActivityPayload::AttentionResolved {
                    request_id,
                    revision,
                    response,
                } => Some((
                    request_id.as_str(),
                    *revision,
                    None,
                    None,
                    Some((event.at_millis, response.clone())),
                )),
                _ => None,
            };
            if let Some((request_id, revision, seen, acknowledged, resolved)) = changed {
                if let Some(attention) = self
                    .snapshot
                    .attention
                    .iter_mut()
                    .find(|attention| attention.id == request_id)
                {
                    if revision >= attention.revision {
                        attention.revision = revision;
                        if seen.is_some() {
                            attention.seen_at_millis = seen;
                        }
                        if acknowledged.is_some() {
                            attention.acknowledged_at_millis = acknowledged;
                        }
                        if let Some((at, response)) = resolved {
                            attention.resolved_at_millis = Some(at);
                            attention.resolution = Some(response);
                        }
                    }
                }
            }
        }
        self.insert_event(event);
        self.snapshot
            .attention
            .retain(crate::session::activity::Attention::is_unresolved);
    }

    fn upsert_attention(&mut self, attention: crate::session::activity::Attention) {
        if let Some(existing) = self
            .snapshot
            .attention
            .iter_mut()
            .find(|existing| existing.id == attention.id)
        {
            if attention.revision >= existing.revision {
                *existing = attention;
            }
        } else if attention.is_unresolved() {
            self.snapshot.attention.push(attention);
        }
        self.snapshot.attention.retain(|item| item.is_unresolved());
    }
}

struct ActivityWatcher {
    stop: Arc<AtomicBool>,
    interrupt: Arc<Mutex<Option<UnixStream>>>,
}

impl ActivityWatcher {
    fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(mut interrupt) = self.interrupt.lock() {
            if let Some(stream) = interrupt.take() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

impl Drop for ActivityWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

type SharedApp = Rc<App>;

/// Optional launch context a card message adds to an agent launch: the human's
/// message as the initial prompt, and the card it belongs to (`RADAR_CARD_ID`).
#[derive(Default, Clone)]
struct Extras {
    prompt: Option<String>,
    card: Option<String>,
    instance: Option<String>,
}

/// How an agent tab's program starts: fresh, on the project's last
/// conversation, or on one exact stored conversation (a board claim's
/// bound session).
enum Resume {
    No,
    Session(String),
}

/// A unique command id for a board mutation issued by the GUI.
fn gui_command_id(prefix: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("gui-{prefix}-{}-{now:x}", std::process::id())
}

fn stable_session_id(project_id: i64, key: TabKey, program_id: &str) -> String {
    format!(
        "project-{project_id}-{}-{}-{program_id}",
        key.slot.as_str(),
        key.instance
    )
}

fn parse_stable_session_id(session_id: &str) -> Option<(i64, TabKey, String)> {
    let mut parts = session_id.splitn(5, '-');
    if parts.next()? != "project" {
        return None;
    }
    let project_id = parts.next()?.parse().ok()?;
    let slot = Slot::parse(parts.next()?);
    let instance = parts.next()?.parse::<u32>().ok()?;
    let program_id = parts.next()?.to_string();
    if project_id <= 0 || program_id.is_empty() {
        return None;
    }
    Some((project_id, TabKey { slot, instance }, program_id))
}

fn add_session_environment(
    spec: &mut CommandSpec,
    project_id: i64,
    project_root: &std::path::Path,
    home: &std::path::Path,
    session_id: &str,
) {
    spec.env_set.extend([
        ("RADAR_PROJECT_ID".to_string(), project_id.to_string()),
        (
            "RADAR_PROJECT_ROOT".to_string(),
            project_root.to_string_lossy().into_owned(),
        ),
        (
            "RADAR_HOME".to_string(),
            home.to_string_lossy().into_owned(),
        ),
        ("RADAR_SESSION_ID".to_string(), session_id.to_string()),
    ]);
}

fn publish_session_lifecycle(
    home: &std::path::Path,
    project_id: i64,
    session_id: &str,
    state: &str,
) {
    let home = home.to_path_buf();
    let session_id = session_id.to_string();
    let state = state.to_string();
    std::thread::spawn(move || {
        use crate::session::activity::{ActivityKind, ActivityPayload, PublishActivity};
        use crate::session::daemon::{Client, Command};
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let request = Command::PublishActivity(PublishActivity {
            project_id,
            command_id: format!("session-{}-{now:x}", std::process::id()),
            session_id: Some(session_id),
            card_id: None,
            kind: ActivityKind::SessionLifecycle,
            payload: ActivityPayload::SessionLifecycle {
                state,
                detail: None,
            },
        });
        let _ = Client::request(&home, request);
    });
}

fn build_window(
    app: &adw::Application,
    paths: &Rc<Paths>,
    db: &SharedDb,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Radar")
        .default_width(1500)
        .default_height(950)
        .build();

    // Workspace tools stay local to the workspace. Home needs no tool rail.
    let workspace_bar = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    workspace_bar.add_css_class("workspace-bar");
    let home_button = gtk::Button::with_label("Home");
    home_button.add_css_class("flat");
    home_button.set_action_name(Some("win.show-home"));
    home_button.set_tooltip_text(Some("All projects · Alt+Home / Alt+B"));
    workspace_bar.append(&home_button);
    let board_button = gtk::Button::with_label("Board");
    board_button.add_css_class("flat");
    // The project's board lives in Home; this is the way back to it.
    board_button.set_action_name(Some("win.workspace-project"));
    board_button.set_tooltip_text(Some("This project's board (to-dos and lanes) · Alt+K"));
    let workspace_title = gtk::Label::new(None);
    workspace_title.add_css_class("heading");
    workspace_title.set_hexpand(true);
    workspace_title.set_xalign(0.0);
    workspace_title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    workspace_bar.append(&workspace_title);
    workspace_bar.append(&board_button);
    let toggles = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    toggles.add_css_class("dock");
    let mut toggle_buttons = HashMap::new();
    for slot in PRIMITIVES {
        let button = gtk::ToggleButton::builder()
            .icon_name(icon_name(slot))
            .tooltip_text(format!("{}\t{}", label_for(slot), accel_hint(slot)))
            .build();
        button.add_css_class("flat");
        if let Some(image) = button.child().and_downcast::<gtk::Image>() {
            image.set_pixel_size(14);
        }
        button.set_action_name(Some("win.primitive-toggle"));
        button.set_action_target_value(Some(&slot.as_str().to_variant()));
        toggles.append(&button);
        toggle_buttons.insert(slot, button);
    }
    workspace_bar.append(&toggles);
    // A new session from the workspace: pick a to-do, start an agent on it —
    // without leaving for Home. Works while another agent panel is on screen.
    let new_session = gtk::Button::from_icon_name("list-add-symbolic");
    new_session.add_css_class("flat");
    new_session.set_tooltip_text(Some(
        "To-dos & sessions: create work, choose an agent, or open a conversation",
    ));
    new_session.set_action_name(Some("win.new-session"));
    toggles.append(&new_session);
    let tools = gtk::Button::with_label("Tools & shortcuts");
    tools.add_css_class("flat");
    tools.set_action_name(Some("win.hud"));
    workspace_bar.append(&tools);
    workspace_bar.set_visible(false);

    // Add a project: one Home view for both new and existing folders. Search
    // the scan root to add a folder you already have, or type a name to create
    // one there. Persistent widgets, so typing and scrolling keep their state.
    let add_list = gtk::ListBox::new();
    add_list.set_selection_mode(gtk::SelectionMode::None);
    let add_search = gtk::SearchEntry::new();
    add_search.set_placeholder_text(Some("Search folders, or type a new name…"));
    add_search.set_tooltip_text(Some(
        "Filter folders to add, or type a name to create a project there",
    ));
    add_search.set_hexpand(true);
    let add_root_button = gtk::Button::new();
    add_root_button.add_css_class("flat");
    add_root_button.add_css_class("caption");
    add_root_button.set_halign(gtk::Align::Start);
    add_root_button.set_tooltip_text(Some("Choose another folder to search"));

    let adder = gtk::Box::new(gtk::Orientation::Vertical, 0);
    adder.add_css_class("home-view");
    adder.add_css_class("add-project-view");
    let adder_choose = gtk::Button::with_label("Choose folder…");
    adder_choose.add_css_class("flat");
    adder_choose.set_tooltip_text(Some("Add a folder anywhere, with the system chooser"));
    adder_choose.set_action_name(Some("win.home-import-dialog"));
    let adder_header =
        home::page_header("Add a project", None, Some(adder_choose.clone().upcast()));

    let adder_body = gtk::Box::new(gtk::Orientation::Vertical, 10);
    adder_body.add_css_class("home-cockpit");
    adder_body.add_css_class("project-view");
    adder_body.set_vexpand(true);
    adder_body.set_margin_top(6);
    adder_body.set_margin_bottom(16);
    adder_body.set_margin_start(22);
    adder_body.set_margin_end(22);
    adder_body.set_halign(gtk::Align::Fill);
    adder_body.set_hexpand(true);

    let adder_card = gtk::Box::new(gtk::Orientation::Vertical, 6);
    adder_card.add_css_class("lane");
    adder_card.set_vexpand(true);
    adder_card.set_hexpand(true);
    adder_card.append(&add_search);
    adder_card.append(&add_root_button);
    let add_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&add_list)
        .build();
    adder_card.append(&add_scroll);
    adder_body.append(&adder_card);
    adder.append(&adder_header);
    adder.append(&adder_body);

    // ---- main area ----
    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .vexpand(true)
        .build();
    stack.add_named(&adder, Some("_add"));
    // The home panel takes the empty states' place: it is what shows with no
    // projects and no panes. It needs the finished app — its dropdowns write
    // preferences and its add-project flow selects — so it joins the stack
    // once the state exists, just before it can first be shown.

    let main = gtk::Box::new(gtk::Orientation::Vertical, 0);
    main.append(&workspace_bar);
    main.append(&stack);

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&main));
    // The overlay panel floats above everything else: keys and primitives,
    // one keystroke away (Alt+H).
    let hud = hud::Hud::new();
    let root = gtk::Overlay::new();
    // The frame: the panel hairline around the whole app, so the outer edge
    // reads as drawn rather than cut off. Where a pane is flush with the
    // window its own border paints the same line, so the edge stays one
    // uniform hairline all the way round.
    root.add_css_class("app-frame");
    root.set_child(Some(&toasts));
    root.add_overlay(hud.widget());
    // The confetti sits above everything and never takes input.
    let confetti = confetti::Confetti::new();
    root.add_overlay(confetti.widget());
    // Activity alerts float above even that: the newest thing wants you most.
    let alerts = alert::Alerts::new();
    root.add_overlay(alerts.widget());
    window.set_content(Some(&root));

    let (status_tx, status_rx) = std::sync::mpsc::channel();
    let (activity_tx, activity_rx) = std::sync::mpsc::sync_channel(512);
    let (agent_tx, agent_rx) = std::sync::mpsc::channel();
    let (add_tx, add_rx) = std::sync::mpsc::channel();
    let state = Rc::new(App {
        db: db.clone(),
        session_home: paths.data_dir.clone(),
        theme: RefCell::new(Theme::load()),
        window: window.clone(),
        workspace_bar,
        workspace_title,
        toggles: RefCell::new(toggle_buttons),
        home_shown: Cell::new(true),
        stack,
        toasts,
        alerts,
        hud: hud.clone(),
        workspaces: RefCell::new(HashMap::new()),
        projects: RefCell::new(Vec::new()),
        agent_sessions: RefCell::new(live_agents::SessionIndex::default()),
        agent_tx,
        agent_rx: RefCell::new(agent_rx),
        agent_scan_pending: Cell::new(false),
        status: RefCell::new(HashMap::new()),
        status_tx,
        status_rx: RefCell::new(status_rx),
        activity: RefCell::new(HashMap::new()),
        activity_online: RefCell::new(HashMap::new()),
        activity_watchers: RefCell::new(HashMap::new()),
        activity_rx: RefCell::new(activity_rx),
        activity_tx,
        board_states: RefCell::new(HashMap::new()),
        home_nav: RefCell::new(Vec::new()),
        board_summaries: RefCell::new(HashMap::new()),
        notified_attention: RefCell::new(HashSet::new()),
        home_pending_attention: Rc::new(RefCell::new(HashSet::new())),
        home_focus_todo: Cell::new(None),
        home_todo_drafts: Rc::new(RefCell::new(HashMap::new())),
        confetti,
        agents_page: RefCell::new(None),
        dismissed_agents: RefCell::new(HashSet::new()),
        header_info: RefCell::new(HashMap::new()),
        current: RefCell::new(None),
        add_list,
        add_search: add_search.clone(),
        add_root_button: add_root_button.clone(),
        add_root: RefCell::new(
            db.ui_prefs()
                .map(|prefs| prefs.resolved_add_root())
                .unwrap_or_else(|_| crate::config::default_project_root()),
        ),
        add_candidates: RefCell::new(Vec::new()),
        add_tx,
        add_rx: RefCell::new(add_rx),
        pointer_motion_ms: Cell::new(0),
        home_columns: Cell::new(0),
    });

    register_actions(&state, app);
    connect_widgets(&state);
    // The overlay panel and radar's own chords (Alt+Arrows and friends).
    hud.wire(&state);
    keynav::install(&state);
    // The home panel reads the app's preferences and fires its actions, so it
    // can only be built now — before the first refresh can show it.
    state.stack.add_named(&home::panel(&state), Some("_home"));
    let state_for_close = Rc::downgrade(&state);
    window.connect_close_request(move |window| {
        if let Some(state) = state_for_close.upgrade() {
            state.persist_workspaces();
        }
        window.set_visible(false);
        glib::Propagation::Stop
    });
    // Home's project masonry follows the window width: rebuild it when the
    // column count changes, not on every pixel of a resize drag.
    let state_for_resize = Rc::downgrade(&state);
    window.connect_realize(move |window| {
        let Some(surface) = window.surface() else {
            return;
        };
        let state = state_for_resize.clone();
        surface.connect_layout(move |_, width, _| {
            let Some(state) = state.upgrade() else {
                return;
            };
            let columns = home::home_column_count(width);
            if state.home_columns.replace(columns) != columns {
                let state = state.clone();
                glib::idle_add_local_once(move || state.refresh_home());
            }
        });
    });
    start_status_drainer(&state);
    start_activity_drainer(&state);
    start_agent_session_drainer(&state);
    start_add_drainer(&state);
    watch_theme(&state);
    state.add_root_button.set_label(&format!(
        "from {}",
        crate::db::abbreviate(&state.add_root.borrow())
    ));
    App::refresh_projects(&state);
    state.request_agent_scan();
    start_agent_session_polling(&state);
    // Development aid: exercise the new-project flow — folder creation, add,
    // open — without the file chooser. RADAR_NEW_PROJECT=/some/path.
    if let Ok(path) = std::env::var("RADAR_NEW_PROJECT") {
        home::create_project(&state, PathBuf::from(path));
    }
    // Start on the human overview. Persisted workspaces are restored on demand.
    if std::env::var("RADAR_NEW_PROJECT").is_err() {
        state.show_home();
    }
    state.reload_theme();
    // Keys belong to the program you are looking at, not to the filter box.
    state.refocus_workspace();

    // Development aid: lay out every primitive on startup so the arrangement can
    // be checked without clicking. RADAR_PRIMITIVES=1.
    if std::env::var("RADAR_PRIMITIVES").is_ok() {
        let state_for_all = state.clone();
        glib::timeout_add_local_once(Duration::from_millis(1200), move || {
            if let Some(workspace) = state_for_all.current_workspace() {
                for slot in PRIMITIVES {
                    state_for_all.show_primitive(&workspace, TabKey::first(slot));
                }
            }
        });
    }
    // Development aid: open Home's cockpit on startup, so the cross-project
    // surface can be checked without pressing Alt+Home. RADAR_HOME_PANEL=1.
    if std::env::var("RADAR_HOME_PANEL").is_ok() {
        let state_for_home = state.clone();
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            state_for_home.show_home();
        });
    }
    // Development aid: open a card panel on startup, so the panel can be
    // checked without clicking. RADAR_OPEN_CARD=<project_id>:<card_id>.
    if let Ok(value) = std::env::var("RADAR_OPEN_CARD") {
        if let Some((project, card_id)) = value.split_once(':') {
            if let Ok(project_id) = project.parse::<i64>() {
                let card_id = card_id.to_string();
                let state_for_card = state.clone();
                glib::timeout_add_local_once(Duration::from_millis(1700), move || {
                    state_for_card.open_home_card(project_id, &card_id);
                });
            }
        }
    }
    // Development aid: open a project's own view inside Home.
    // RADAR_OPEN_PROJECT=<project_id>.
    if let Ok(value) = std::env::var("RADAR_OPEN_PROJECT") {
        if let Ok(project_id) = value.parse::<i64>() {
            let state_for_project = state.clone();
            glib::timeout_add_local_once(Duration::from_millis(1700), move || {
                state_for_project.open_home_project(project_id);
            });
        }
    }
    // Development aid: open a project's workspace, so the workspace bar —
    // its Board button and dock — can be checked without clicking.
    // RADAR_OPEN_WORKSPACE=<project_id>.
    if let Ok(value) = std::env::var("RADAR_OPEN_WORKSPACE") {
        if let Ok(project_id) = value.parse::<i64>() {
            let state_for_workspace = state.clone();
            glib::timeout_add_local_once(Duration::from_millis(1700), move || {
                state_for_workspace.select_project(project_id);
            });
        }
    }
    // Development aid: open Home's Add-a-project picker on startup.
    // RADAR_OPEN_ADD_PROJECT=1 (RADAR_OPEN_NEW_PROJECT still accepted).
    if std::env::var("RADAR_OPEN_ADD_PROJECT").is_ok()
        || std::env::var("RADAR_OPEN_NEW_PROJECT").is_ok()
    {
        let state_for_new = state.clone();
        glib::timeout_add_local_once(Duration::from_millis(1700), move || {
            state_for_new.open_home_add();
        });
    }
    // Development aid: open Home's running-agents page on startup, so every
    // project's live panels can be checked without clicking.
    // RADAR_OPEN_AGENTS=1.
    if std::env::var("RADAR_OPEN_AGENTS").is_ok() {
        let state_for_agents = state.clone();
        glib::timeout_add_local_once(Duration::from_millis(1700), move || {
            state_for_agents.enter_agents_view();
        });
    }
    // Development aid: throw the completion confetti on startup, so the burst
    // can be checked without completing a card. RADAR_CONFETTI=1.
    if std::env::var("RADAR_CONFETTI").is_ok() {
        let state_for_confetti = state.clone();
        glib::timeout_add_local_once(Duration::from_millis(2000), move || {
            state_for_confetti.confetti.celebrate();
        });
    }
    window
}

fn icon_name(slot: Slot) -> &'static str {
    match slot {
        Slot::Editor => "accessories-text-editor-symbolic",
        Slot::Agent => "application-x-executable-symbolic",
        Slot::Diff => "view-dual-symbolic",
        Slot::Board => "view-grid-symbolic",
        Slot::Shell => "utilities-terminal-symbolic",
        Slot::Custom => "application-x-executable-symbolic",
    }
}

fn accel_hint(slot: Slot) -> &'static str {
    match slot {
        Slot::Editor => "Alt+E",
        Slot::Agent => "Alt+A",
        Slot::Diff => "Alt+G",
        Slot::Board => "",
        Slot::Shell => "Alt+T",
        Slot::Custom => "",
    }
}

fn status_page(
    icon: &str,
    title: &str,
    description: &str,
    action: Option<(&str, &str)>,
) -> gtk::Widget {
    let page = adw::StatusPage::builder()
        .icon_name(icon)
        .title(title)
        .description(description)
        .build();
    if let Some((label, action)) = action {
        let button = gtk::Button::with_label(label);
        button.add_css_class("suggested-action");
        button.add_css_class("pill");
        button.set_action_name(Some(action));
        button.set_halign(gtk::Align::Center);
        page.set_child(Some(&button));
    }
    page.upcast()
}

fn connect_widgets(app: &SharedApp) {
    {
        // The picker's search: filter what is on disk, offer to create a typed
        // name, and rescan a moment after typing stops.
        let entry = app.add_search.clone();
        let app = app.clone();
        let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        entry.connect_search_changed(move |entry| {
            App::render_add_results(&app, &entry.text());
            if let Some(id) = pending.borrow_mut().take() {
                id.remove();
            }
            if !entry.text().trim().is_empty() {
                let app = app.clone();
                let pending_for_cb = pending.clone();
                let id = glib::timeout_add_local_once(Duration::from_millis(250), move || {
                    app.rescan_add();
                    *pending_for_cb.borrow_mut() = None;
                });
                *pending.borrow_mut() = Some(id);
            }
        });
    }
    {
        // Esc empties the picker's search.
        let entry = app.add_search.clone();
        let entry_for_clear = entry.clone();
        let controller = gtk::EventControllerKey::new();
        controller.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape && !entry_for_clear.text().is_empty() {
                entry_for_clear.set_text("");
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        entry.add_controller(controller);
    }
    {
        // Pick another folder for the picker to search.
        let app = app.clone();
        let root_button = app.add_root_button.clone();
        root_button.connect_clicked(move |_| {
            #[allow(deprecated)]
            let dialog = gtk::FileChooserDialog::new(
                Some("Choose a folder to search"),
                Some(&app.window),
                gtk::FileChooserAction::SelectFolder,
                &[
                    ("Cancel", gtk::ResponseType::Cancel),
                    ("Search", gtk::ResponseType::Accept),
                ],
            );
            let app = app.clone();
            #[allow(deprecated)]
            dialog.connect_response(move |dialog, response| {
                if response == gtk::ResponseType::Accept {
                    if let Some(path) = dialog.file().and_then(|file| file.path()) {
                        let mut prefs = app.db.ui_prefs().unwrap_or_default();
                        prefs.add_root = Some(path.clone());
                        if let Err(error) = app.db.set_ui_prefs(&prefs) {
                            eprintln!("radar: could not store the scan root: {error}");
                        }
                        *app.add_root.borrow_mut() = path.clone();
                        app.add_root_button
                            .set_label(&format!("from {}", crate::db::abbreviate(&path)));
                        app.rescan_add();
                    }
                }
                dialog.close();
            });
            dialog.present();
        });
    }
    {
        // Real pointer motion, anywhere over the window: the clock hover-focus
        // checks. Capture phase, so primitives that consume motion (the
        // terminal among them) cannot starve the clock. Enter events a mapped
        // widget synthesizes under a parked pointer carry no motion, so they
        // never read as mouse intent.
        let app_for_motion = app.clone();
        let controller = gtk::EventControllerMotion::new();
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        controller.connect_motion(move |_, _, _| {
            app_for_motion
                .pointer_motion_ms
                .set(glib::monotonic_time() / 1000);
        });
        app.window.add_controller(controller);
    }
}

/// A picker row: icon, name, path, and a trailing hint. The row is inert; the
/// whole row is a flat button the caller wires to its action.
fn add_row(
    title: &str,
    subtitle: &str,
    trailing: &str,
    icon: &str,
) -> (gtk::ListBoxRow, gtk::Button) {
    let row = gtk::ListBoxRow::new();
    row.set_activatable(false);
    row.add_css_class("add-row");

    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.set_halign(gtk::Align::Fill);
    button.set_hexpand(true);

    let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let image = gtk::Image::from_icon_name(icon);
    image.add_css_class("dim-label");
    image.set_pixel_size(16);
    image.set_valign(gtk::Align::Center);
    box_.append(&image);

    let texts = gtk::Box::new(gtk::Orientation::Vertical, 0);
    texts.set_hexpand(true);
    texts.set_valign(gtk::Align::Center);
    let name = gtk::Label::new(Some(title));
    name.set_xalign(0.0);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    texts.append(&name);
    let sub = gtk::Label::new(Some(subtitle));
    sub.set_xalign(0.0);
    sub.add_css_class("caption");
    sub.add_css_class("dim-label");
    sub.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    texts.append(&sub);
    box_.append(&texts);

    let hint = gtk::Label::new(Some(trailing));
    hint.add_css_class("caption");
    hint.add_css_class("add-row-hint");
    hint.set_valign(gtk::Align::Center);
    box_.append(&hint);

    button.set_child(Some(&box_));
    row.set_child(Some(&button));
    (row, button)
}

/// Apply folder scans that arrived from the worker thread.
fn start_add_drainer(app: &SharedApp) {
    let app = app.clone();
    glib::timeout_add_local(Duration::from_millis(120), move || {
        let batch = {
            let rx = app.add_rx.borrow();
            rx.try_recv().ok()
        };
        if let Some((root, found)) = batch {
            if root != *app.add_root.borrow() {
                return glib::ControlFlow::Continue;
            }
            *app.add_candidates.borrow_mut() = found;
            let query = app.add_search.text();
            App::render_add_results(&app, &query);
        }
        glib::ControlFlow::Continue
    });
}

/// Apply git statuses that arrived from the worker thread.
fn start_status_drainer(app: &SharedApp) {
    let app = app.clone();
    glib::timeout_add_local(Duration::from_millis(120), move || {
        let batches: Vec<Vec<(i64, crate::git::Status)>> = {
            let rx = app.status_rx.borrow();
            let mut all = Vec::new();
            while let Ok(batch) = rx.try_recv() {
                all.push(batch);
            }
            all
        };
        if batches.is_empty() {
            return glib::ControlFlow::Continue;
        }
        let mut changed = false;
        {
            let mut status = app.status.borrow_mut();
            for batch in batches {
                for (id, fresh) in batch {
                    if status.get(&id) != Some(&fresh) {
                        status.insert(id, fresh);
                        changed = true;
                    }
                }
            }
        }
        if changed {
            app.refresh_home();
        }
        glib::ControlFlow::Continue
    });
}
fn start_agent_session_drainer(app: &SharedApp) {
    let app = app.clone();
    glib::timeout_add_local(Duration::from_millis(120), move || {
        let latest = {
            let rx = app.agent_rx.borrow();
            let mut latest = None;
            while let Ok(batch) = rx.try_recv() {
                latest = Some(batch);
            }
            latest
        };
        let Some(result) = latest else {
            return glib::ControlFlow::Continue;
        };
        app.agent_scan_pending.set(false);
        let sessions_changed = app.agent_sessions.borrow_mut().apply(result);
        if sessions_changed {
            let index = app.agent_sessions.borrow();
            for rows in index.by_project.values() {
                for session in rows {
                    app.track_panel_conversation(session);
                }
            }
            drop(index);
            app.sync_board_sessions();
            app.refresh_home();
        }
        glib::ControlFlow::Continue
    });
}

fn start_agent_session_polling(app: &SharedApp) {
    let app = app.clone();
    glib::timeout_add_local(Duration::from_secs(3), move || {
        app.request_agent_scan();
        glib::ControlFlow::Continue
    });
}

fn send_activity_notice(
    tx: &std::sync::mpsc::SyncSender<ActivityNotice>,
    mut notice: ActivityNotice,
    stop: &AtomicBool,
) -> bool {
    loop {
        match tx.try_send(notice) {
            Ok(()) => return true,
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return false,
            Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                if stop.load(Ordering::Acquire) {
                    return false;
                }
                notice = returned;
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn start_activity_watcher(
    home: PathBuf,
    project_id: i64,
    tx: std::sync::mpsc::SyncSender<ActivityNotice>,
    stop: Arc<AtomicBool>,
    interrupt: Arc<Mutex<Option<UnixStream>>>,
) {
    let _ = std::thread::Builder::new()
        .name(format!("radar-activity-{project_id}"))
        .spawn(move || {
            use crate::session::daemon::{Client, Command, Response};
            let mut cursor = 0;
            while !stop.load(Ordering::Acquire) {
                let mut client = match Client::connect(
                    &home,
                    Command::WatchActivity {
                        project_id,
                        after_sequence: cursor,
                    },
                ) {
                    Ok(client) => client,
                    Err(_) => {
                        let _ = send_activity_notice(
                            &tx,
                            ActivityNotice::Connection {
                                project_id,
                                online: false,
                            },
                            &stop,
                        );
                        activity_retry(&stop);
                        continue;
                    }
                };
                let _ = client.set_read_timeout(None);
                if let Ok(stream) = client.interrupt_handle() {
                    if let Ok(mut current) = interrupt.lock() {
                        *current = Some(stream);
                    }
                    if stop.load(Ordering::Acquire) {
                        if let Ok(mut current) = interrupt.lock() {
                            if let Some(stream) = current.take() {
                                let _ = stream.shutdown(std::net::Shutdown::Both);
                            }
                        }
                        break;
                    }
                }

                let mut resync = false;
                let mut disconnected = false;
                match client.receive() {
                    Ok(Response::ActivityWatching { snapshot, .. }) => {
                        let watermark = snapshot.watermark;
                        if !send_activity_notice(
                            &tx,
                            ActivityNotice::Snapshot {
                                project_id,
                                snapshot,
                                replace_events: false,
                            },
                            &stop,
                        ) {
                            break;
                        }
                        cursor = watermark;
                        let _ = send_activity_notice(
                            &tx,
                            ActivityNotice::Connection {
                                project_id,
                                online: true,
                            },
                            &stop,
                        );
                        loop {
                            if stop.load(Ordering::Acquire) {
                                break;
                            }
                            match client.receive() {
                                Ok(Response::Activity(event)) => {
                                    if event.sequence <= cursor {
                                        continue;
                                    }
                                    let sequence = event.sequence;
                                    if !send_activity_notice(
                                        &tx,
                                        ActivityNotice::Event(event),
                                        &stop,
                                    ) {
                                        disconnected = true;
                                        break;
                                    }
                                    cursor = sequence;
                                }
                                Ok(Response::ResyncRequired) => {
                                    resync = true;
                                    break;
                                }
                                Ok(_) | Err(_) => {
                                    disconnected = true;
                                    break;
                                }
                            }
                        }
                    }
                    Ok(Response::ResyncRequired) => resync = true,
                    Ok(_) | Err(_) => disconnected = true,
                }
                if let Ok(mut current) = interrupt.lock() {
                    current.take();
                }
                drop(client);
                if stop.load(Ordering::Acquire) {
                    break;
                }

                if resync {
                    match Client::request(
                        &home,
                        Command::ActivitySnapshot {
                            project_id,
                            after_sequence: None,
                            limit: 200,
                        },
                    ) {
                        Ok(Response::ActivitySnapshot(snapshot)) => {
                            let watermark = snapshot.watermark;
                            if send_activity_notice(
                                &tx,
                                ActivityNotice::Snapshot {
                                    project_id,
                                    snapshot,
                                    replace_events: true,
                                },
                                &stop,
                            ) {
                                cursor = watermark;
                                let _ = send_activity_notice(
                                    &tx,
                                    ActivityNotice::Connection {
                                        project_id,
                                        online: true,
                                    },
                                    &stop,
                                );
                                continue;
                            }
                            break;
                        }
                        _ => disconnected = true,
                    }
                }
                if disconnected {
                    let _ = send_activity_notice(
                        &tx,
                        ActivityNotice::Connection {
                            project_id,
                            online: false,
                        },
                        &stop,
                    );
                    activity_retry(&stop);
                }
            }
            if let Ok(mut current) = interrupt.lock() {
                current.take();
            }
        });
}

fn activity_retry(stop: &AtomicBool) {
    for _ in 0..10 {
        if stop.load(Ordering::Acquire) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn start_activity_drainer(app: &SharedApp) {
    let app = app.clone();
    glib::timeout_add_local(Duration::from_millis(80), move || {
        let notices: Vec<ActivityNotice> = {
            let rx = app.activity_rx.borrow();
            let mut notices = Vec::new();
            while let Ok(notice) = rx.try_recv() {
                notices.push(notice);
            }
            notices
        };
        let mut changed_projects = HashSet::new();
        for notice in notices {
            changed_projects.insert(app.apply_activity_notice(notice));
        }
        if !changed_projects.is_empty() {
            app.refresh_home();
        }
        glib::ControlFlow::Continue
    });
}

fn watch_theme(app: &SharedApp) {
    let Some(dir) = theme::omarchy_theme_dir() else {
        return;
    };
    let file = gio::File::for_path(dir.join("colors.toml"));
    let Ok(monitor) = file.monitor_file(gio::FileMonitorFlags::NONE, None::<&gio::Cancellable>)
    else {
        return;
    };
    let app = app.clone();
    monitor.connect_changed(move |_, _, _, _| app.reload_theme());
    std::mem::forget(monitor);
}

fn register_actions(app: &SharedApp, gtk_app: &adw::Application) {
    let add = |name: &str, handler: Box<dyn Fn()>| {
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(move |_, _| handler());
        app.window.add_action(&action);
    };
    {
        let action = gio::SimpleAction::new(
            "open-project",
            Some(glib::VariantTy::new("x").expect("a project ID")),
        );
        let app = app.clone();
        action.connect_activate(move |_, parameter| {
            if let Some(project_id) = parameter.and_then(|value| value.get::<i64>()) {
                app.open_project_home(project_id);
            }
        });
        gtk_app.add_action(&action);
    }

    // ---- projects ----
    {
        // Open the combined Add-a-project picker.
        let app = app.clone();
        add("home-add-project", Box::new(move || app.open_home_add()));
    }
    {
        // Create a project from the picker's typed name: (name, parent).
        let action = gio::SimpleAction::new(
            "home-project-create",
            Some(glib::VariantTy::new("(ss)").expect("a (name, parent) tuple")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some((name, parent)) = parameter.and_then(|value| value.get::<(String, String)>())
            else {
                return;
            };
            home::create_project_from_fields(&app_for_action, &name, &parent);
        });
        app.window.add_action(&action);
    }
    {
        // The picker's "Choose folder…": add a folder anywhere.
        let app = app.clone();
        add(
            "home-import-dialog",
            Box::new(move || home::add_project_dialog(&app)),
        );
    }
    {
        let action = gio::SimpleAction::new("home-project-import", Some(glib::VariantTy::STRING));
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            if let Some(path) = parameter.and_then(|value| value.get::<String>()) {
                app_for_action.add_home_project(Path::new(&path));
            }
        });
        app.window.add_action(&action);
    }
    for name in [
        "home-project-edit",
        "home-project-archive",
        "home-project-defaults",
        "home-project-pin",
        "home-project-up",
        "home-project-down",
    ] {
        let action = gio::SimpleAction::new(name, Some(glib::VariantTy::INT64));
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some(id) = parameter.and_then(|value| value.get::<i64>()) else {
                return;
            };
            match name {
                "home-project-archive" => app_for_action.confirm_remove(id),
                "home-project-edit" => app_for_action.edit_project_name(id),
                "home-project-defaults" => app_for_action.project_defaults(id),
                _ => {
                    let result = if name == "home-project-pin" {
                        app_for_action.db.project(id).and_then(|project| {
                            let project = project
                                .ok_or_else(|| anyhow::anyhow!("Project no longer exists"))?;
                            app_for_action.db.set_pinned(id, !project.pinned)
                        })
                    } else {
                        app_for_action
                            .db
                            .move_project(id, if name == "home-project-up" { -1 } else { 1 })
                    };
                    if let Err(error) = result {
                        app_for_action.toast(&format!("Could not update project: {error}"));
                    } else {
                        App::refresh_projects(&app_for_action);
                    }
                }
            }
        });
        app.window.add_action(&action);
    }
    {
        // Takes a project id, so a row's own button can call it.
        let action = gio::SimpleAction::new("project-remove", Some(glib::VariantTy::INT32));
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some(id) = parameter.and_then(|value| value.get::<i32>()) else {
                return;
            };
            app_for_action.confirm_remove(id as i64);
        });
        app.window.add_action(&action);
    }
    {
        let app = app.clone();
        add(
            "preferences",
            Box::new(move || {
                dialogs::preferences(&app.window, &app.db, {
                    let app = app.clone();
                    move || {
                        app.refresh_headers();
                        app.toast("Preferences saved");
                    }
                });
            }),
        );
    }
    {
        let app = app.clone();
        add(
            "refresh",
            Box::new(move || {
                app.reload_theme();
                App::refresh_projects(&app);
                app.refresh_status();
                app.toast("Refreshed");
            }),
        );
    }
    {
        // The overlay panel: every primitive, every key, one place.
        let app = app.clone();
        add(
            "hud",
            Box::new(move || {
                if app.hud.is_visible() {
                    app.hud.close(&app);
                } else {
                    app.hud.present(&app);
                }
            }),
        );
    }
    {
        let app = app.clone();
        add(
            "quit",
            Box::new(move || {
                app.persist_workspaces();
                if let Some(application) = app.window.application() {
                    application.quit();
                }
            }),
        );
    }
    {
        let app = app.clone();
        add(
            "project-rename",
            Box::new(move || {
                let Some(project) = app.current_project() else {
                    return;
                };
                app.edit_project_name(project.id);
            }),
        );
    }
    {
        let app = app.clone();
        add(
            "project-pin",
            Box::new(move || {
                let Some(project) = app.current_project() else {
                    return;
                };
                if let Err(error) = app.db.set_pinned(project.id, !project.pinned) {
                    eprintln!("radar: {error}");
                }
                App::refresh_projects(&app);
            }),
        );
    }
    for (name, delta) in [("project-move-up", -1i64), ("project-move-down", 1i64)] {
        let app = app.clone();
        add(
            name,
            Box::new(move || {
                let Some(project) = app.current_project() else {
                    return;
                };
                if let Err(error) = app.db.move_project(project.id, delta) {
                    eprintln!("radar: {error}");
                }
                App::refresh_projects(&app);
            }),
        );
    }
    for program in programs::external_programs() {
        let app = app.clone();
        let name = format!("open-external-{}", program.id);
        let label = program.name.clone();
        add(
            &name,
            Box::new(move || {
                let Some(project) = app.current_project() else {
                    return;
                };
                let spec = CommandSpec {
                    argv: program.command_spec(&LaunchOptions::default()).argv,
                    env_unset: Vec::new(),
                    env_set: Vec::new(),
                };
                match pane::spawn_external_window(&spec, &project.path) {
                    Ok(()) => app.toast(&format!("Opened in {label}")),
                    Err(error) => app.toast(&format!("Could not open {label}: {error}")),
                }
            }),
        );
    }

    // ---- primitives: the only content there is ----
    {
        // The dock and the Alt-chords toggle by kind; the key resolves to
        // the tab of that kind that exists, or opens the first.
        let action = gio::SimpleAction::new("primitive-toggle", Some(glib::VariantTy::STRING));
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some(name) = parameter.and_then(|value| value.get::<String>()) else {
                return;
            };
            let Some(workspace) = app_for_action.current_workspace() else {
                app_for_action.toast("Select a project first");
                return;
            };
            let key = workspace.resolve_tab(TabKey::parse(&name));
            let existing_tab = workspace.tab(key).is_some();
            app_for_action.toggle_primitive(&workspace, key);
            if existing_tab {
                app_for_action.remember_agent_panel_visibility(&workspace, key);
            }
        });
        app.window.add_action(&action);
    }
    {
        // Clicking a chip in a shared header.
        let action = gio::SimpleAction::new("primitive-activate", Some(glib::VariantTy::STRING));
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some(name) = parameter.and_then(|value| value.get::<String>()) else {
                return;
            };
            let Some(workspace) = app_for_action.current_workspace() else {
                return;
            };
            app_for_action.activate_primitive(&workspace, TabKey::parse(&name));
        });
        app.window.add_action(&action);
    }
    {
        // Hovering a chip changes the visible primitive without taking
        // keyboard focus away from the current content.
        let action = gio::SimpleAction::new("primitive-hover", Some(glib::VariantTy::STRING));
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some(name) = parameter.and_then(|value| value.get::<String>()) else {
                return;
            };
            let Some(workspace) = app_for_action.current_workspace() else {
                return;
            };
            app_for_action.hover_primitive(&workspace, TabKey::parse(&name));
        });
        app.window.add_action(&action);
    }
    {
        // The board's @claim links: clicking one opens the named agent's
        // session — the matching agent tab, activated and focused.
        let action = gio::SimpleAction::new("session-open", Some(glib::VariantTy::STRING));
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some(claim) = parameter.and_then(|value| value.get::<String>()) else {
                return;
            };
            let Some(workspace) = app_for_action.current_workspace() else {
                return;
            };
            app_for_action.open_agent_session(&workspace, &claim);
        });
        app.window.add_action(&action);
    }
    {
        // Explicitly request a fresh conversation instead of making the
        // normal Open/Resume action unexpectedly create a duplicate.
        let action = gio::SimpleAction::new(
            "card-session-new",
            Some(glib::VariantTy::new("(xs)").expect("a project and card ID")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            if let Some((project_id, card_id)) =
                parameter.and_then(|value| value.get::<(i64, String)>())
            {
                app_for_action.start_new_card_session(project_id, &card_id);
            }
        });
        app.window.add_action(&action);
    }
    {
        let action = gio::SimpleAction::new(
            "project-agent-open",
            Some(glib::VariantTy::new("(xs)").expect("a project/tab tuple")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some((project_id, tab)) = parameter.and_then(|value| value.get::<(i64, String)>())
            else {
                return;
            };
            app_for_action.select_project(project_id);
            if let Some(workspace) = app_for_action.current_workspace() {
                app_for_action.activate_primitive(&workspace, TabKey::parse(&tab));
            }
        });
        app.window.add_action(&action);
    }
    {
        let action = gio::SimpleAction::new(
            "project-agent-open-external",
            Some(glib::VariantTy::new("(xutu)").expect("a project and process identity")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some((project_id, pid, start_ticks, window_pid)) =
                parameter.and_then(|value| value.get::<(i64, u32, u64, u32)>())
            else {
                return;
            };
            app_for_action.select_project(project_id);
            #[cfg(target_os = "linux")]
            if let Err(error) = live_agents::focus_external(pid, start_ticks, window_pid) {
                app_for_action.toast(&error);
            }
            #[cfg(not(target_os = "linux"))]
            let _ = (pid, start_ticks, window_pid);
        });
        app.window.add_action(&action);
    }
    {
        // Home's project header: bring the project's workspace on screen.
        let action = gio::SimpleAction::new(
            "open-project",
            Some(glib::VariantTy::new("x").expect("a project ID")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            if let Some(project_id) = parameter.and_then(|value| value.get::<i64>()) {
                app_for_action.select_project(project_id);
                app_for_action.window.present();
            }
        });
        app.window.add_action(&action);
    }
    {
        // Home's claim chip: open the agent the claim names, the same
        // resolution the board's @claim link uses.
        let action = gio::SimpleAction::new(
            "open-claim",
            Some(glib::VariantTy::new("(xs)").expect("a project and claim")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some((project_id, claim)) =
                parameter.and_then(|value| value.get::<(i64, String)>())
            else {
                return;
            };
            app_for_action.hud.close_tasks(&app_for_action);
            app_for_action.select_project(project_id);
            if let Some(workspace) = app_for_action.current_workspace() {
                app_for_action.open_agent_session(&workspace, &claim);
            }
        });
        app.window.add_action(&action);
    }
    {
        let action = gio::SimpleAction::new(
            "card-worker",
            Some(glib::VariantTy::new("(xsss)").expect("project, card, kind and worker")),
        );
        let state = app.clone();
        action.connect_activate(move |_, parameter| {
            if let Some((project_id, card_id, kind, worker)) =
                parameter.and_then(|value| value.get::<(i64, String, String, String)>())
            {
                state.without_navigation(|| {
                    state.assign_card_worker(project_id, &card_id, &kind, &worker)
                });
            }
        });
        app.window.add_action(&action);
    }
    {
        // A board card nobody holds: start the project's default agent
        // attached to it, and claim it. The card row's Session control.
        let action = gio::SimpleAction::new(
            "card-session-create",
            Some(glib::VariantTy::new("(xs)").expect("a project and card ID")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            if let Some((project_id, card_id)) =
                parameter.and_then(|value| value.get::<(i64, String)>())
            {
                app_for_action.start_card_session(project_id, &card_id);
            }
        });
        app.window.add_action(&action);
    }
    {
        let action = gio::SimpleAction::new(
            "session-todo",
            Some(glib::VariantTy::new("(xss)").expect("a project, pane and todo")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            if let Some((project_id, key, card_id)) =
                parameter.and_then(|value| value.get::<(i64, String, String)>())
            {
                app_for_action.open_session_todo(project_id, TabKey::parse(&key), &card_id);
            }
        });
        app.window.add_action(&action);
    }
    {
        // The agent panel header's done control, beside the to-do title:
        // close the session's to-do from right there. One-way — Home's
        // checkbox owns reopen.
        let action = gio::SimpleAction::new(
            "session-todo-done",
            Some(glib::VariantTy::new("(xs)").expect("a project and card ID")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some((project_id, card_id)) =
                parameter.and_then(|value| value.get::<(i64, String)>())
            else {
                return;
            };
            app_for_action.complete_session_todo(project_id, &card_id);
        });
        app.window.add_action(&action);
    }
    {
        let action = gio::SimpleAction::new(
            "card-session-open",
            Some(glib::VariantTy::new("(xs)").expect("project and card")),
        );
        let state = app.clone();
        action.connect_activate(move |_, parameter| {
            if let Some((project, card)) = parameter.and_then(|value| value.get::<(i64, String)>())
            {
                state.open_card_session(project, &card, None);
            }
        });
        app.window.add_action(&action);
    }
    {
        let action = gio::SimpleAction::new(
            "card-conversation-open",
            Some(glib::VariantTy::new("(xss)").expect("project, card and conversation")),
        );
        let state = app.clone();
        action.connect_activate(move |_, parameter| {
            if let Some((project, card, conversation)) =
                parameter.and_then(|value| value.get::<(i64, String, String)>())
            {
                state.open_card_session(project, &card, Some(&conversation));
            }
        });
        app.window.add_action(&action);
    }
    {
        let action = gio::SimpleAction::new(
            "todo-session",
            Some(glib::VariantTy::new("(xs)").expect("a project and session identity")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            if let Some((project_id, identity)) =
                parameter.and_then(|value| value.get::<(i64, String)>())
            {
                app_for_action.hud.close_tasks(&app_for_action);
                app_for_action.open_catalog_session(project_id, &identity);
            }
        });
        app.window.add_action(&action);
    }
    {
        // Home's to-do row: open the card's panel — its thread and controls.
        let action = gio::SimpleAction::new(
            "open-card",
            Some(glib::VariantTy::new("(xs)").expect("a project and card ID")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some((project_id, card_id)) =
                parameter.and_then(|value| value.get::<(i64, String)>())
            else {
                return;
            };
            app_for_action.open_home_card(project_id, &card_id);
        });
        app.window.add_action(&action);
    }
    {
        // Home's to-do checkbox: close an open to-do, or reopen a done one.
        let action = gio::SimpleAction::new(
            "card-toggle-done",
            Some(glib::VariantTy::new("(xs)").expect("a project and card ID")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some((project_id, card_id)) =
                parameter.and_then(|value| value.get::<(i64, String)>())
            else {
                return;
            };
            app_for_action.toggle_card_done(project_id, &card_id);
        });
        app.window.add_action(&action);
    }
    {
        // Home's add-to-do input: the title typed under a project's To-dos.
        let action = gio::SimpleAction::new(
            "home-add-todo",
            Some(glib::VariantTy::new("(xs)").expect("a project ID and title")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some((project_id, title)) =
                parameter.and_then(|value| value.get::<(i64, String)>())
            else {
                return;
            };
            app_for_action.add_home_todo(project_id, &title, false);
        });
        app.window.add_action(&action);
    }
    {
        let action = gio::SimpleAction::new(
            "home-add-work",
            Some(glib::VariantTy::new("(xs)").expect("project and task")),
        );
        let state = app.clone();
        action.connect_activate(move |_, parameter| {
            if let Some((project_id, title)) =
                parameter.and_then(|value| value.get::<(i64, String)>())
            {
                state.add_home_todo(project_id, &title, true);
            }
        });
        app.window.add_action(&action);
    }
    {
        // Home's Back: leave a board/card drill-down for the cockpit.
        let app_for_action = app.clone();
        add("home-back", Box::new(move || app_for_action.home_back()));
    }
    {
        // The card detail's × (same as Back).
        let app_for_action = app.clone();
        add("close-card", Box::new(move || app_for_action.home_back()));
    }
    {
        // Open a project's own view inside Home.
        let action = gio::SimpleAction::new(
            "home-project",
            Some(glib::VariantTy::new("x").expect("a project ID")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            if let Some(project_id) = parameter.and_then(|value| value.get::<i64>()) {
                app_for_action.open_home_project(project_id);
            }
        });
        app.window.add_action(&action);
    }
    {
        // Home's running-agents page: every live session's real panel,
        // tiled, across every project.
        let app_for_action = app.clone();
        add(
            "home-agents",
            Box::new(move || app_for_action.enter_agents_view()),
        );
    }
    {
        // The Agents page's panel close: the panel leaves the wall, its
        // program keeps running — the same contract as closing a workspace
        // panel. The panel can come back by the session ending and something
        // new starting under the id; the workspace panel is the other way in.
        let action = gio::SimpleAction::new(
            "agents-close",
            Some(glib::VariantTy::new("s").expect("a session id")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some(id) = parameter.and_then(|value| value.get::<String>()) else {
                return;
            };
            app_for_action
                .dismissed_agents
                .borrow_mut()
                .insert(id.clone());
            if let Some(page) = app_for_action.agents_page.borrow().as_ref() {
                agents::forget(&app_for_action, &page.board, &id);
            }
        });
        app.window.add_action(&action);
    }
    {
        let app = app.clone();
        add(
            "workspace-auto-arrange",
            Box::new(move || {
                if let Some(workspace) = app.current_workspace() {
                    if let Some((panels, _)) = workspace.zoom.borrow_mut().take() {
                        *workspace.panels.borrow_mut() = panels;
                    }
                    workspace.tree.borrow_mut().take();
                    workspace.positions.borrow_mut().clear();
                    app.layout(&workspace);
                    app.sync_toggles();
                    app.persist_primitives(&workspace);
                }
            }),
        );
    }
    {
        let app = app.clone();
        add(
            "agents-auto-arrange",
            Box::new(move || {
                if let Some(page) = app.agents_page.borrow().as_ref() {
                    agents::auto_arrange(&app, &page.board);
                }
            }),
        );
    }
    {
        // The Agents page's drag grammar, resolving its own board: the
        // middle of a panel swaps, an edge splits.
        let action = gio::SimpleAction::new(
            "agents-swap",
            Some(glib::VariantTy::new("(ss)").expect("two session ids")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some((first, second)) = parameter.and_then(|value| value.get::<(String, String)>())
            else {
                return;
            };
            if let Some(page) = app_for_action.agents_page.borrow().as_ref() {
                agents::swap(&app_for_action, &page.board, &first, &second);
            }
        });
        app.window.add_action(&action);
    }
    {
        let action = gio::SimpleAction::new(
            "agents-nest-split",
            Some(glib::VariantTy::new("(sss)").expect("two session ids and a zone")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some((dragged, target, zone)) =
                parameter.and_then(|value| value.get::<(String, String, String)>())
            else {
                return;
            };
            if let Some(page) = app_for_action.agents_page.borrow().as_ref() {
                agents::nest(&app_for_action, &page.board, &dragged, &target, &zone);
            }
        });
        app.window.add_action(&action);
    }
    {
        // The card detail's inline editor saved: re-read the board so the card
        // shows the title and body it just wrote.
        let action = gio::SimpleAction::new(
            "card-saved",
            Some(glib::VariantTy::new("x").expect("a project ID")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            if let Some(project_id) = parameter.and_then(|value| value.get::<i64>()) {
                app_for_action.refresh_board_summary(project_id);
            }
        });
        app.window.add_action(&action);
    }
    {
        // The card detail's lane dropdown.
        let action = gio::SimpleAction::new(
            "card-move",
            Some(glib::VariantTy::new("(xss)").expect("a project, card and column")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            if let Some((project_id, card_id, column)) =
                parameter.and_then(|value| value.get::<(i64, String, String)>())
            {
                app_for_action.move_home_card(project_id, &card_id, &column);
            }
        });
        app.window.add_action(&action);
    }
    {
        // The card detail's reply box.
        let action = gio::SimpleAction::new(
            "card-reply",
            Some(glib::VariantTy::new("(xss)").expect("a project, card and text")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            if let Some((project_id, card_id, text)) =
                parameter.and_then(|value| value.get::<(i64, String, String)>())
            {
                app_for_action.message_card(project_id, &card_id, &text);
            }
        });
        app.window.add_action(&action);
    }
    {
        // Activity and attention records carry stable session IDs, so their
        // navigation does not depend on a mutable board claim name.
        let action = gio::SimpleAction::new(
            "activity-session-open",
            Some(glib::VariantTy::new("(xs)").expect("a project/session tuple")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some((project_id, session_id)) =
                parameter.and_then(|value| value.get::<(i64, String)>())
            else {
                return;
            };
            app_for_action.open_linked_session(project_id, &session_id);
        });
        app.window.add_action(&action);
    }
    {
        // Dropping a panel onto another panel's middle: they exchange places.
        let action = gio::SimpleAction::new(
            "pane-swap",
            Some(glib::VariantTy::new("(ss)").expect("a tuple type")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some(pair) = parameter.and_then(|value| value.get::<(String, String)>()) else {
                return;
            };
            let Some(workspace) = app_for_action.current_workspace() else {
                return;
            };
            app_for_action.swap_panes(&workspace, TabKey::parse(&pair.0), TabKey::parse(&pair.1));
        });
        app.window.add_action(&action);
    }
    {
        // Dropping a panel onto another panel's edge: the target's region
        // divides in two and the dragged panel takes the dropped half.
        let action = gio::SimpleAction::new(
            "pane-nest-split",
            Some(glib::VariantTy::new("(sss)").expect("a tuple type")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some(triple) = parameter.and_then(|value| value.get::<(String, String, String)>())
            else {
                return;
            };
            let Some(workspace) = app_for_action.current_workspace() else {
                return;
            };
            app_for_action.nest_split(
                &workspace,
                TabKey::parse(&triple.0),
                TabKey::parse(&triple.1),
                &triple.2,
            );
        });
        app.window.add_action(&action);
    }
    {
        // Change the program for the pane that currently has keyboard focus.
        let app = app.clone();
        add(
            "pane-program",
            Box::new(move || {
                let Some(workspace) = app.current_workspace() else {
                    return;
                };
                let Some(key) = app.focused_panel(&workspace).and_then(|panel| panel.key()) else {
                    return;
                };
                app.hud.present_programs(&app, key.slot);
            }),
        );
    }
    {
        let action = gio::SimpleAction::new("primitive-close", Some(glib::VariantTy::STRING));
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some(name) = parameter.and_then(|value| value.get::<String>()) else {
                return;
            };
            let Some(workspace) = app_for_action.current_workspace() else {
                return;
            };
            let key = workspace.resolve_tab(TabKey::parse(&name));
            if workspace.is_visible(key) {
                app_for_action.toggle_primitive(&workspace, key);
            }
        });
        app.window.add_action(&action);
    }
    for (name, delta) in [("pane-move-up", -1isize), ("pane-move-down", 1isize)] {
        let action = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some(name) = parameter.and_then(|value| value.get::<String>()) else {
                return;
            };
            let Some(workspace) = app_for_action.current_workspace() else {
                return;
            };
            app_for_action.move_pane(&workspace, TabKey::parse(&name), delta);
        });
        app.window.add_action(&action);
    }
    {
        let action = gio::SimpleAction::new("pane-zoom", Some(glib::VariantTy::STRING));
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some(name) = parameter.and_then(|value| value.get::<String>()) else {
                return;
            };
            app_for_action.toggle_zoom(Some(TabKey::parse(&name)));
        });
        app.window.add_action(&action);
    }
    {
        let action = gio::SimpleAction::new("primitive-focus", Some(glib::VariantTy::STRING));
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some(name) = parameter.and_then(|value| value.get::<String>()) else {
                return;
            };
            let key = TabKey::parse(&name);
            if let Some(workspace) = app_for_action.current_workspace() {
                app_for_action.activate_primitive(&workspace, key);
            }
        });
        app.window.add_action(&action);
    }
    {
        // A panel's program reported its live title. Aim it at the panel's
        // header as the session's name. Empty text clears back to the program.
        let action =
            gio::SimpleAction::new("pane-info", Some(glib::VariantTy::new("(xss)").unwrap()));
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some((project_id, slot, text)) =
                parameter.and_then(|value| value.get::<(i64, String, String)>())
            else {
                return;
            };
            let text = if text.is_empty() { None } else { Some(text) };
            app_for_action.store_pane_title(project_id, TabKey::parse(&slot), text);
        });
        app.window.add_action(&action);
    }
    {
        // The terminal bell — how agent CLIs ask for attention. The panel's
        // header marks it until that panel is looked at.
        let action =
            gio::SimpleAction::new("pane-bell", Some(glib::VariantTy::new("(xs)").unwrap()));
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some((project_id, slot)) = parameter.and_then(|value| value.get::<(i64, String)>())
            else {
                return;
            };
            let key = TabKey::parse(&slot);
            for workspace in app_for_action.workspaces.borrow().values() {
                if workspace.project.id != project_id {
                    continue;
                }
                if let Some(panel) = workspace.panel_of(key) {
                    panel.set_attention();
                }
            }
        });
        app.window.add_action(&action);
    }
    {
        let app = app.clone();
        add("zoom", Box::new(move || app.toggle_zoom(None)));
    }
    {
        let app = app.clone();
        add(
            "workspace-project",
            Box::new(move || {
                let id = *app.current.borrow();
                if let Some(id) = id {
                    app.show_home();
                    app.open_home_project(id);
                }
            }),
        );
    }
    {
        // The home panel: the empty state, plus setup and the new-project
        // flow. Workspaces stay alive behind it — going home never stops a
        // program, it only looks away.
        let app = app.clone();
        add("show-home", Box::new(move || app.show_home()));
    }
    {
        // A new session from the workspace: pick a to-do, start the
        // project's default agent on it. Agents are 1:1 with board to-dos,
        // so the picker is the create form.
        let app = app.clone();
        add(
            "new-session",
            Box::new(move || {
                let Some(project_id) = *app.current.borrow() else {
                    app.toast("Select a project first");
                    return;
                };
                // Re-read the board so the picker shows what holds right now.
                app.refresh_board_summary(project_id);
                app.hud.present_cards(&app, project_id);
            }),
        );
    }

    // ---- keyboard ----
    // Alt is radar's only modifier, so every Ctrl chord reaches the programs
    // in the panels the way their authors wrote them. The one exception is
    // cycling: the window manager owns Alt+Tab, so the cycle stays on Ctrl.
    let accels: [(&str, &[&str]); 16] = [
        ("win.preferences", &["<Alt>comma"]),
        ("win.refresh", &["<Alt>r"]),
        ("win.quit", &["<Alt>q"]),
        ("win.zoom", &["<Alt>f"]),
        ("win.hud", &["<Alt>h"]),
        ("win.show-home", &["<Alt>Home", "<Alt>b"]),
        ("win.workspace-project", &["<Alt>k"]),
        ("win.primitive-toggle::editor", &["<Alt>e"]),
        ("win.primitive-toggle::agent", &["<Alt>a"]),
        ("win.primitive-toggle::diff", &["<Alt>g"]),
        ("win.primitive-toggle::shell", &["<Alt>t"]),
        ("win.pane-program", &["<Alt>p"]),
        ("win.primitive-focus::editor", &["<Alt>1"]),
        ("win.primitive-focus::agent", &["<Alt>2"]),
        ("win.primitive-focus::diff", &["<Alt>3"]),
        ("win.primitive-focus::shell", &["<Alt>4"]),
    ];
    for (action, keys) in accels {
        gtk_app.set_accels_for_action(action, keys);
    }
}

impl App {
    fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }

    /// Put the keys back on the program you are looking at.
    fn refocus_workspace(&self) {
        if let Some(workspace) = self.current_workspace() {
            let first = workspace.panels().first().and_then(|panel| panel.key());
            if let Some(primitive) = first.and_then(|key| workspace.tab(key)) {
                primitive.focus();
            }
        }
    }

    fn current_project(&self) -> Option<Project> {
        let id = (*self.current.borrow())?;
        self.projects.borrow().iter().find(|p| p.id == id).cloned()
    }

    fn current_workspace(&self) -> Option<Rc<Workspace>> {
        let id = (*self.current.borrow())?;
        self.workspaces.borrow().get(&id).cloned()
    }

    /// The pane containing the widget that currently holds keyboard focus.
    fn focused_panel(&self, workspace: &Workspace) -> Option<Rc<Panel>> {
        let focus = self.window.focus_widget()?;
        workspace
            .panels()
            .into_iter()
            .find(|panel| focus.is_ancestor(&panel.widget))
    }

    /// The visible pane order. In auto mode, derive the same order the renderer
    /// uses; in manual mode, the saved split tree is authoritative.
    fn ordered_panels(&self, workspace: &Workspace) -> Vec<Rc<Panel>> {
        workspace
            .tree
            .borrow()
            .as_ref()
            .map(split::Node::leaves)
            .or_else(|| auto_node(workspace, &workspace.panels()).map(|tree| tree.leaves()))
            .unwrap_or_default()
    }

    /// Confirm, then archive a project without deleting its state or files.
    fn confirm_remove(self: &Rc<Self>, id: i64) {
        let Some(project) = self.db.project(id).ok().flatten() else {
            return;
        };
        let dialog = gtk::AlertDialog::builder()
            .message(format!("Archive {}?", project.name))
            .detail("It leaves Home, but its tasks, settings and sessions are kept. Nothing on disk is touched. Add the folder again to restore it.")
            .buttons(["Cancel", "Archive"])
            .cancel_button(0)
            .default_button(0)
            .build();
        // The callback outlives this borrow, so it works from an owned handle.
        let app = self.clone();
        dialog.choose(
            Some(&self.window),
            None::<&gio::Cancellable>,
            move |result| {
                if result != Ok(1) {
                    return;
                }
                if let Err(error) = app.db.set_archived(project.id, true) {
                    app.toast(&format!("Could not archive the project: {error}"));
                    return;
                }
                // Rebuild the sidebar from a fresh read; this also drops the
                // selection of the removed project.
                App::refresh_projects(&app);
                app.show_home();
                app.toast("Project archived");
            },
        );
    }

    fn project_defaults(self: &Rc<Self>, id: i64) {
        let Some(project) = self.db.project(id).ok().flatten() else {
            return;
        };
        let app = Rc::downgrade(self);
        dialogs::project_preferences(&self.window, &self.db, id, &project.name, move || {
            if let Some(app) = app.upgrade() {
                app.sync_toggles();
                app.refresh_headers();
                app.toast("Project defaults saved");
            }
        });
    }

    fn edit_project_name(self: &Rc<Self>, id: i64) {
        let Some(project) = self.db.project(id).ok().flatten() else {
            return;
        };
        let entry = gtk::Entry::new();
        entry.set_text(&project.name);
        let dialog = gtk::Window::builder()
            .title("Edit project name")
            .modal(true)
            .default_width(420)
            .transient_for(&self.window)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);
        content.append(&entry);
        let save = gtk::Button::with_label("Save");
        save.add_css_class("suggested-action");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let dialog_for_cancel = dialog.clone();
        cancel.connect_clicked(move |_| dialog_for_cancel.close());
        buttons.append(&cancel);
        buttons.append(&save);
        content.append(&buttons);
        dialog.set_child(Some(&content));
        let app = self.clone();
        let dialog_for_save = dialog.clone();
        let entry_for_save = entry.clone();
        save.connect_clicked(move |_| {
            let name = entry_for_save.text().trim().to_string();
            if name.is_empty() {
                app.toast("Project name cannot be empty");
                return;
            }
            if let Err(error) = app.db.rename_project(id, &name) {
                app.toast(&format!("Could not rename the project: {error}"));
                return;
            }
            App::refresh_projects(&app);
            app.refresh_home();
            dialog_for_save.close();
        });
        entry.connect_activate(move |_| save.emit_clicked());
        dialog.present();
        entry.grab_focus();
    }

    fn reload_theme(&self) {
        let theme = Theme::load();
        // Accent and pane roundness live in the stylesheet: re-render it so a
        // theme switch recolours radar's own chrome, not just the panes.
        style::refresh(&theme);
        if let Ok(manager) = adw::StyleManager::default().downcast::<adw::StyleManager>() {
            manager.set_color_scheme(if theme.dark {
                adw::ColorScheme::ForceDark
            } else {
                adw::ColorScheme::ForceLight
            });
        }
        for workspace in self.workspaces.borrow().values() {
            for primitive in workspace.tabs.borrow().values() {
                if let Some(pane) = &primitive.pane {
                    pane.apply_theme(&theme);
                }
            }
        }
        if let Some(page) = self.agents_page.borrow().as_ref() {
            for panel in page.board.panels.borrow().iter() {
                panel.pane.apply_theme(&theme);
            }
        }
        *self.theme.borrow_mut() = theme;
    }

    fn launch_options(&self) -> LaunchOptions {
        let preferences = self.db.preferences().unwrap_or_default();
        LaunchOptions {
            safe: !preferences.agent_auto_flags,
            extra_args: Vec::new(),
            prompt: None,
            session: None,
            create_session: None,
            agent_instance: None,
            card: None,
        }
    }

    // ---- projects ----

    fn reconcile_activity_watchers(&self, projects: &[Project]) {
        let wanted: HashSet<i64> = projects.iter().map(|project| project.id).collect();
        let mut watchers = self.activity_watchers.borrow_mut();
        let removed: Vec<i64> = watchers
            .keys()
            .filter(|project_id| !wanted.contains(project_id))
            .copied()
            .collect();
        for project_id in removed {
            if let Some(watcher) = watchers.remove(&project_id) {
                watcher.stop();
            }
            self.activity_online.borrow_mut().remove(&project_id);
        }
        for project_id in wanted {
            if watchers.contains_key(&project_id) {
                continue;
            }
            let stop = Arc::new(AtomicBool::new(false));
            let interrupt = Arc::new(Mutex::new(None));
            start_activity_watcher(
                self.session_home.clone(),
                project_id,
                self.activity_tx.clone(),
                stop.clone(),
                interrupt.clone(),
            );
            watchers.insert(project_id, ActivityWatcher { stop, interrupt });
        }
    }

    fn reconcile_board_watchers(self: &Rc<Self>, projects: &[Project]) {
        let wanted: HashSet<i64> = projects.iter().map(|project| project.id).collect();
        self.board_summaries
            .borrow_mut()
            .retain(|project_id, _| wanted.contains(project_id));
        self.board_states
            .borrow_mut()
            .retain(|project_id, _| wanted.contains(project_id));

        // The store publishes a BoardChanged event on every mutation, so the
        // summaries refresh through the activity stream; a project's first
        // summary is warmed here.
        for project in projects {
            self.refresh_board_summary(project.id);
        }
    }

    fn refresh_board_summary(&self, project_id: i64) {
        let project = self
            .projects
            .borrow()
            .iter()
            .find(|project| project.id == project_id)
            .cloned();
        let Some(project) = project else {
            return;
        };
        if project.is_missing() {
            self.board_summaries.borrow_mut().remove(&project_id);
            self.board_states.borrow_mut().remove(&project_id);
        } else if let Some(state) = self.fetch_board_state(project_id) {
            self.board_summaries
                .borrow_mut()
                .insert(project_id, board::summarize(&state));
            self.board_states.borrow_mut().insert(project_id, state);
        } else {
            self.board_summaries.borrow_mut().remove(&project_id);
        }
        self.sync_board_sessions();
        self.refresh_home();
    }

    /// Fetch a project's board from the daemon store.
    fn fetch_board_state(
        &self,
        project_id: i64,
    ) -> Option<crate::session::board_store::BoardState> {
        match crate::session::daemon::board_state(&self.session_home, project_id) {
            Ok(state) => Some(state),
            Err(error) => {
                eprintln!("radar: board state: {error}");
                None
            }
        }
    }

    /// The agent panel header's done control: close the session's to-do from
    /// right there. One-way — a done to-do's panel hides by the
    /// workspace-visibility policy while the program keeps running; Home's
    /// checkbox owns reopening.
    fn complete_session_todo(&self, project_id: i64, card_id: &str) {
        let done = self
            .board_states
            .borrow()
            .get(&project_id)
            .and_then(|state| state.cards.iter().find(|card| card.id == card_id))
            .map(|card| card.done);
        match done {
            Some(true) => return, // already done: the panel is on its way out
            None => {
                self.toast("That to-do is no longer on the board");
                return;
            }
            Some(false) => {}
        }
        let command = gui_command_id("done");
        match crate::session::daemon::board_card_complete(
            &self.session_home,
            project_id,
            card_id,
            None,
            &command,
        ) {
            Ok(_) => {
                self.refresh_board_summary(project_id);
                // Done cards leave the lanes; the burst is the send-off.
                self.confetti.celebrate();
                self.toast("Done. 🎉");
            }
            Err(error) => self.toast(&format!("Could not update the to-do: {error}")),
        }
    }

    /// Close an open to-do or reopen a done one, straight from Home. Goes
    /// through the board store, then re-reads the summary so the lane updates.
    fn toggle_card_done(&self, project_id: i64, card_id: &str) {
        let done = self
            .board_states
            .borrow()
            .get(&project_id)
            .and_then(|state| state.cards.iter().find(|card| card.id == card_id))
            .map(|card| card.done);
        let Some(done) = done else {
            self.toast("That to-do is no longer on the board");
            return;
        };
        let command = gui_command_id("toggle");
        let result = if done {
            crate::session::daemon::board_card_reopen(
                &self.session_home,
                project_id,
                card_id,
                None,
                &command,
            )
        } else {
            crate::session::daemon::board_card_complete(
                &self.session_home,
                project_id,
                card_id,
                None,
                &command,
            )
        };
        match result {
            Ok(_) => {
                self.refresh_board_summary(project_id);
                if done {
                    self.toast("To-do reopened");
                } else {
                    // Done cards leave the lanes; the burst is the send-off.
                    self.confetti.celebrate();
                    self.toast("Done. 🎉");
                }
            }
            Err(error) => self.toast(&format!("Could not update the to-do: {error}")),
        }
    }

    /// Add a to-do from Home: a new card on the project's board, in the store's
    /// default lane (Todo). Home is rebuilt so the lane shows it, and the
    /// input that submitted it gets the keys back for the next one.
    fn add_home_todo(&self, project_id: i64, title: &str, start: bool) {
        let title = title.trim();
        if title.is_empty() {
            return;
        }
        if !self
            .projects
            .borrow()
            .iter()
            .any(|project| project.id == project_id)
        {
            return;
        }
        let command = gui_command_id("todo");
        match crate::session::daemon::board_card_add(
            &self.session_home,
            project_id,
            None,
            title,
            "",
            None,
            &command,
        ) {
            Ok(change) => {
                self.home_todo_drafts.borrow_mut().remove(&project_id);
                if !start {
                    self.home_focus_todo.set(Some(project_id));
                }
                self.refresh_board_summary(project_id);
                if start {
                    self.start_card_session(project_id, &change.card.id);
                    self.open_home_card(project_id, &change.card.id);
                }
            }
            Err(error) => self.toast(&format!("Could not add the to-do: {error}")),
        }
    }

    /// Open a card as a conversation inside Home.
    fn open_home_card(&self, project_id: i64, card_id: &str) {
        if !self.home_shown.get() && *self.current.borrow() == Some(project_id) {
            self.hud.present_task(self, project_id, card_id);
            return;
        }
        self.enter_home_view(HomeView::Card(project_id, card_id.to_string()));
    }

    fn open_session_todo(&self, project_id: i64, key: TabKey, card_id: &str) {
        let primitive = self
            .workspaces
            .borrow()
            .get(&project_id)
            .and_then(|workspace| workspace.tab(key));
        let linked = primitive.as_ref().is_some_and(|primitive| {
            let runtime = stable_session_id(project_id, key, &primitive.program_id);
            self.catalog_card_sessions(project_id, card_id)
                .is_ok_and(|sessions| {
                    sessions.iter().any(|session| {
                        session.radar_session_id.as_deref() == Some(runtime.as_str())
                            || (live_agents::exact_provider_session_id(session).is_some()
                                && session.provider_session_id.as_deref()
                                    == primitive.launched_session.borrow().as_deref())
                    })
                })
        });
        if !linked || self.card_title(project_id, card_id).is_none() {
            self.toast("This session's to-do is no longer available");
            return;
        }
        self.open_home_card(project_id, card_id);
    }

    /// Drill into a project's own view inside Home.
    fn open_home_project(&self, project_id: i64) {
        self.enter_home_view(HomeView::Project(project_id));
    }

    /// The Agents page: every live session's real panel, tiled. A persistent
    /// stack page (see the field docs); entering syncs it with discovery.
    fn enter_agents_view(self: &Rc<Self>) {
        gtk::prelude::GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
        self.home_shown.set(true);
        *self.current.borrow_mut() = None;
        if !matches!(self.home_nav.borrow().last(), Some(HomeView::Agents)) {
            self.home_nav.borrow_mut().push(HomeView::Agents);
        }
        if self.agents_page.borrow().is_none() {
            *self.agents_page.borrow_mut() = Some(agents::page());
            if let Some(page) = self.agents_page.borrow().as_ref() {
                self.stack.add_named(&page.root, Some("_agents"));
            }
        }
        if let Some(page) = self.agents_page.borrow().as_ref() {
            agents::sync(self, page);
        }
        self.stack.set_visible_child_name("_agents");
        self.sync_toggles();
    }

    /// In-flush the Agents page when it is the one on screen: panels follow
    /// discovery; everything else is left alone.
    fn sync_agents_page_if_visible(&self) {
        if self.stack.visible_child_name().as_deref() == Some("_agents") {
            if let Some(page) = self.agents_page.borrow().as_ref() {
                agents::sync(self, page);
            }
        }
    }

    fn enter_home_view(&self, view: HomeView) {
        gtk::prelude::GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
        self.home_shown.set(true);
        *self.current.borrow_mut() = None;
        self.home_nav.borrow_mut().push(view);
        self.stack.set_visible_child_name("_home");
        self.sync_toggles();
    }

    /// Pop one destination, including persistent pages, without reconstructing
    /// their live panes. Back from a conversation returns to its origin.
    fn home_back(self: &Rc<Self>) {
        gtk::prelude::GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
        self.home_nav.borrow_mut().pop();
        if self.home_nav.borrow().is_empty() {
            self.show_home();
            return;
        }
        if matches!(self.home_nav.borrow().last(), Some(HomeView::Agents)) {
            self.enter_agents_view();
        } else if matches!(self.home_nav.borrow().last(), Some(HomeView::AddProject)) {
            self.stack.set_visible_child_name("_add");
        } else {
            self.stack.set_visible_child_name("_home");
            self.refresh_home();
        }
    }

    /// Run a card command without treating its session as a navigation request.
    fn without_navigation(&self, run: impl FnOnce()) {
        let current = *self.current.borrow();
        let last_project = self.db.ui_prefs().ok().map(|prefs| prefs.last_project);
        let home = self.home_shown.get();
        let visible = self.stack.visible_child_name();
        run();
        *self.current.borrow_mut() = current;
        self.home_shown.set(home);
        if let Some(last_project) = last_project {
            let _ = self.db.remember_last_project(last_project);
        }
        if let Some(visible) = visible {
            self.stack.set_visible_child_name(&visible);
        }
        self.sync_toggles();
        self.refresh_headers();
        self.refresh_home();
    }

    /// Move a card to another lane through the store.
    fn move_home_card(&self, project_id: i64, card_id: &str, column: &str) {
        let command = gui_command_id("move");
        match crate::session::daemon::board_card_move(
            &self.session_home,
            project_id,
            card_id,
            column,
            None,
            &command,
        ) {
            Ok(_) => self.refresh_board_summary(project_id),
            Err(error) => self.toast(&format!("Could not move the card: {error}")),
        }
    }

    /// Record the reply and route it to the live worker or exact conversation.
    /// An unclaimed to-do starts the default agent. None of these is navigation.
    fn message_card(&self, project_id: i64, card_id: &str, text: &str) {
        self.without_navigation(|| self.deliver_card_message(project_id, card_id, text));
    }

    fn deliver_card_message(&self, project_id: i64, card_id: &str, text: &str) {
        card::publish_comment(
            &self.session_home,
            &self.activity_tx,
            project_id,
            card_id,
            text.to_string(),
        );
        let card = self
            .fetch_board_state(project_id)
            .and_then(|state| state.cards.into_iter().find(|card| card.id == card_id));
        let Some(card) = card else {
            self.toast("That card is no longer on the board");
            return;
        };
        let Some(card) = self.prepare_card_work(project_id, card) else {
            return;
        };
        let prompt = format!(
            "A human sent a message on board card \"{}\":\n\n{}\n\nRead its thread with radar card show \"{}\" and continue.",
            card.title, text, card.id
        );
        if self.current.borrow().as_ref() != Some(&project_id) {
            self.select_project_with(project_id, false);
        }
        let Some(workspace) = self.current_workspace() else {
            return;
        };
        let worker = card
            .claim
            .as_deref()
            .and_then(|claim| self.live_card_worker(project_id, card_id, Some(claim)))
            .or_else(|| self.live_card_worker(project_id, card_id, None));
        if let Some((key, claim, _)) = worker {
            if card.claim.as_deref() != Some(claim.as_str()) {
                if let Err(error) = crate::session::daemon::board_card_claim(
                    &self.session_home,
                    project_id,
                    card_id,
                    Some(&claim),
                    Some(card.revision),
                    &gui_command_id("follow-up"),
                ) {
                    self.toast(&format!("Could not reconnect the worker: {error}"));
                    return;
                }
                self.refresh_board_summary(project_id);
            }
            self.inject_if_idle(project_id, &workspace, key, &prompt);
            return;
        }
        let sessions = match self.catalog_card_sessions(project_id, card_id) {
            Ok(sessions) => sessions,
            Err(error) => {
                self.toast(&format!("Could not read linked conversations: {error}"));
                return;
            }
        };
        if let Some(session) = sessions.into_iter().max_by_key(|session| {
            (
                session.card_id.as_deref() == Some(card_id),
                session.running,
                session.last_activity_at,
            )
        }) {
            if session.running {
                self.toast("This conversation is serving another to-do. Choose a new agent, or continue it when stopped.");
                return;
            }
            if let Some(conversation) = live_agents::exact_provider_session_id(&session) {
                self.launch_card_agent(
                    project_id,
                    &card,
                    prompt,
                    &session.program_id,
                    Resume::Session(conversation.to_string()),
                );
            } else {
                self.toast("The linked session has no exact resume identity. Choose an agent to reconnect this to-do.");
            }
            return;
        }
        if let Some(claim) = card.claim.as_deref() {
            if let Ok(Some((program, conversation))) = self.db.bound_session(project_id, claim) {
                let sessions = match self.project_catalog(project_id, None) {
                    Ok(sessions) => sessions,
                    Err(error) => {
                        self.toast(&format!(
                            "Could not check the claimed conversation: {error}"
                        ));
                        return;
                    }
                };
                if sessions.iter().any(|session| {
                    session.provider == program
                        && session.provider_session_id == conversation
                        && session.lifecycle == "running"
                }) {
                    self.toast("The claimed conversation is already running on another to-do.");
                    return;
                }
                self.launch_card_agent(
                    project_id,
                    &card,
                    prompt,
                    &program,
                    Resume::Session(conversation),
                );
                return;
            }
        }
        // No exact conversation exists: an explicit request starts fresh work.
        self.spawn_agent_for_card(project_id, card_id, prompt);
    }

    /// A board card's Session control: start the project's default agent
    /// attached to the card, and claim it. The prompt points the agent at the
    /// card so it knows what it was started for.
    fn start_card_session(&self, project_id: i64, card_id: &str) {
        self.without_navigation(|| self.start_card_worker(project_id, card_id));
    }

    /// Explicitly start a fresh agent conversation for a card. This is kept
    /// separate from `start_card_worker`, whose job is to open or resume the
    /// card's existing worker when one is available.
    fn start_new_card_session(&self, project_id: i64, card_id: &str) {
        self.without_navigation(|| {
            if self.live_card_worker(project_id, card_id, None).is_some() {
                self.toast("This to-do already has a running worker; open that session first");
                return;
            }
            let title = self
                .board_states
                .borrow()
                .get(&project_id)
                .and_then(|state| state.cards.iter().find(|card| card.id == card_id))
                .map(|card| card.title.clone())
                .unwrap_or_else(|| "the linked to-do".to_string());
            let prompt = crate::session::board_store::work_prompt(card_id, &title);
            if self.spawn_agent_for_card(project_id, card_id, prompt) {
                self.toast("Started a new agent session on this to-do");
            }
        });
    }

    fn project_catalog(
        &self,
        project_id: i64,
        query: Option<&str>,
    ) -> anyhow::Result<Vec<crate::session::catalog::Entry>> {
        let path = self
            .projects
            .borrow()
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.path.clone())
            .ok_or_else(|| anyhow::anyhow!("project not available"))?;
        match crate::session::daemon::Client::request(
            &self.session_home,
            crate::session::daemon::Command::CatalogList {
                projects: vec![crate::session::daemon::CatalogProject {
                    id: project_id,
                    path,
                }],
                filter: crate::session::catalog::CatalogFilter::All,
                query: query.map(str::to_string),
                limit: 1000,
            },
        )? {
            crate::session::daemon::Response::Catalog(entries) => Ok(entries),
            _ => anyhow::bail!("unexpected catalog response"),
        }
    }

    fn live_card_worker(
        &self,
        project_id: i64,
        card_id: &str,
        claim: Option<&str>,
    ) -> Option<(TabKey, String, String)> {
        let crate::session::daemon::Response::Sessions(sessions) =
            crate::session::daemon::Client::request(
                &self.session_home,
                crate::session::daemon::Command::List,
            )
            .ok()?
        else {
            return None;
        };
        let bound: HashSet<String> = if claim.is_none() {
            self.project_catalog(project_id, Some(card_id))
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| {
                    entry.card_id.as_deref() == Some(card_id) && entry.lifecycle == "running"
                })
                .filter_map(|entry| entry.radar_session_id)
                .collect()
        } else {
            HashSet::new()
        };
        sessions
            .into_iter()
            .filter_map(|session| {
                // A retained but ended session must not swallow a follow-up:
                // the next message relaunches the agent, resumed.
                if !matches!(
                    session.lifecycle,
                    crate::session::registry::Lifecycle::Running
                ) {
                    return None;
                }
                let (project, key, _) = parse_stable_session_id(&session.id)?;
                if project != project_id || key.slot != Slot::Agent {
                    return None;
                }
                let pid = session.pid?;
                if claim.is_none()
                    && programs::launch::radar_card_of(pid).as_deref() != Some(card_id)
                    && !bound.contains(&session.id)
                {
                    return None;
                }
                let agent = programs::launch::radar_agent_of(pid)?;
                claim
                    .is_none_or(|claim| claim == agent)
                    .then_some((key, agent, session.id))
            })
            .max_by_key(|(key, _, _)| key.instance)
    }

    fn start_card_worker(&self, project_id: i64, card_id: &str) {
        // Background sessions need not have a mapped terminal or discovery row.
        if self.live_card_worker(project_id, card_id, None).is_some() {
            return;
        }
        let card = self
            .board_states
            .borrow()
            .get(&project_id)
            .and_then(|state| state.cards.iter().find(|card| card.id == card_id).cloned());
        let Some(card) = card else {
            self.toast("That card is no longer on the board");
            return;
        };
        if card.done {
            self.toast("Reopen this to-do before starting a session");
            return;
        }
        if let Some(claim) = card.claim.as_deref() {
            // Open what the claim still holds. A claim whose conversation is
            // gone offers a fresh start instead of a dead end: a launch binds
            // its exact id before the CLI has created anything, so a crashed
            // first turn leaves the card pointing at a conversation that was
            // never written — reopening it would open an empty one that never
            // saw this card, and the work would be untriggerable.
            let live = self.live_claim_session(project_id, claim);
            if live.is_some()
                || (self.claim_has_exact_session(project_id, claim)
                    && !self.bound_conversation_is_lost(project_id, claim))
            {
                self.select_project(project_id);
                if let Some(workspace) = self.current_workspace() {
                    self.open_agent_session(&workspace, claim);
                }
                return;
            }
        }
        let prompt = crate::session::board_store::work_prompt(&card.id, &card.title);
        if self.spawn_agent_for_card(project_id, card_id, prompt) {
            self.toast("Started an agent on this card");
        }
    }

    fn assign_card_worker(&self, project_id: i64, card_id: &str, kind: &str, worker: &str) {
        let card = self
            .board_states
            .borrow()
            .get(&project_id)
            .and_then(|state| state.cards.iter().find(|card| card.id == card_id).cloned());
        let Some(card) = card.filter(|card| !card.done) else {
            self.toast("Reopen this to-do before assigning a conversation");
            return;
        };
        if self.card_claim_is_live(project_id, &card) {
            self.toast("This to-do already has a running worker; its assignment cannot be stolen");
            return;
        };
        let prompt = crate::session::board_store::work_prompt(&card.id, &card.title);
        if kind == "agent" {
            self.spawn_card_agent(project_id, card_id, prompt, Some(worker));
            return;
        }
        if kind != "session" {
            return;
        }
        let sessions = match self.project_catalog(project_id, None) {
            Ok(sessions) => sessions,
            Err(error) => {
                self.toast(&format!("Could not read conversations: {error}"));
                return;
            }
        };
        let session = worker
            .strip_prefix("catalog-")
            .and_then(|id| id.parse::<i64>().ok())
            .and_then(|id| sessions.into_iter().find(|session| session.id == id));
        let Some(session) = session.filter(|session| session.lifecycle != "running") else {
            self.toast("Choose a stopped conversation; a running worker cannot be reassigned");
            return;
        };
        if session.radar_session_id.as_deref() == Some(session.provider_session_id.as_str()) {
            self.toast("This conversation has no exact resume identity");
            return;
        }
        self.launch_card_agent(
            project_id,
            &card,
            prompt,
            &session.provider,
            Resume::Session(session.provider_session_id),
        );
    }

    /// Start the project's default agent on `card_id`, attached to the card
    /// (`RADAR_CARD_ID`) and claiming it for the new instance. Returns false —
    /// after toasting — when no agent is installed or the tab cannot be made.
    fn spawn_agent_for_card(&self, project_id: i64, card_id: &str, prompt: String) -> bool {
        self.spawn_card_agent(project_id, card_id, prompt, None)
    }

    fn spawn_card_agent(
        &self,
        project_id: i64,
        card_id: &str,
        prompt: String,
        agent: Option<&str>,
    ) -> bool {
        let global = self.db.preferences().unwrap_or_default();
        let preferences = self
            .db
            .project_settings(project_id)
            .unwrap_or_default()
            .apply_to(&global);
        let program = match agent {
            Some(id) => programs::by_id(id)
                .filter(|program| program.kind == Kind::Agent && program.installed()),
            None => programs::for_slot(Slot::Agent, &preferences),
        };
        let Some(program) = program else {
            self.toast("No agent installed — set one in Preferences");
            return false;
        };
        let card = self
            .board_states
            .borrow()
            .get(&project_id)
            .and_then(|state| state.cards.iter().find(|card| card.id == card_id).cloned());
        let Some(card) = card.filter(|card| !card.done) else {
            self.toast("Reopen this to-do before starting a session");
            return false;
        };
        self.launch_card_agent(project_id, &card, prompt, &program.id, Resume::No)
    }

    fn prepare_card_work(
        &self,
        project_id: i64,
        card: crate::session::board_store::StoredCard,
    ) -> Option<crate::session::board_store::StoredCard> {
        let state = self.fetch_board_state(project_id)?;
        let card = state
            .cards
            .iter()
            .find(|current| current.id == card.id)?
            .clone();
        let kind = state
            .lanes
            .iter()
            .find(|lane| lane.id == card.lane_id)?
            .kind
            .as_str();
        if kind != "review" && kind != "done" {
            return Some(card);
        }
        let lane = state.lanes.iter().find(|lane| lane.kind == "in_progress")?;
        match crate::session::daemon::board_card_move(
            &self.session_home,
            project_id,
            &card.id,
            &lane.name,
            Some(card.revision),
            &gui_command_id("continue"),
        ) {
            Ok(change) => {
                let continued = if let Some(claim) = card.claim.as_deref() {
                    match crate::session::daemon::board_card_claim(
                        &self.session_home,
                        project_id,
                        &card.id,
                        Some(claim),
                        Some(change.card.revision),
                        &gui_command_id("continue-claim"),
                    ) {
                        Ok(change) => change.card,
                        Err(error) => {
                            self.toast(&format!("Could not keep this to-do's worker: {error}"));
                            return None;
                        }
                    }
                } else {
                    change.card
                };
                self.refresh_board_summary(project_id);
                Some(continued)
            }
            Err(error) => {
                self.toast(&format!("Could not continue the to-do: {error}"));
                None
            }
        }
    }

    fn launch_card_agent(
        &self,
        project_id: i64,
        card: &crate::session::board_store::StoredCard,
        prompt: String,
        program_id: &str,
        resume: Resume,
    ) -> bool {
        let Some(program) = programs::by_id(program_id)
            .filter(|program| program.kind == Kind::Agent && program.installed())
        else {
            self.toast("The selected agent is not installed");
            return false;
        };
        if matches!(resume, Resume::Session(_)) && program.resume_session.is_empty() {
            self.toast("The selected agent cannot resume an exact conversation");
            return false;
        }
        if self.current.borrow().as_ref() != Some(&project_id) {
            self.select_project_with(project_id, false);
        }
        let Some(workspace) = self.current_workspace() else {
            return false;
        };
        let card_id = card.id.as_str();
        let key = workspace.next_key(Slot::Agent);
        let stamp = crate::programs::launch::now_stamp();
        let claim = format!("{}-{}", program.id, stamp);
        if let Err(error) = crate::session::daemon::board_card_claim(
            &self.session_home,
            project_id,
            card_id,
            Some(&claim),
            Some(card.revision),
            &gui_command_id("claim"),
        ) {
            self.toast(&format!("Could not assign the to-do: {error}"));
            return false;
        }
        let extras = Extras {
            prompt: Some(prompt),
            card: Some(card_id.to_string()),
            instance: Some(stamp),
        };
        if self
            .ensure_primitive_with(&workspace, key, Some(&program.id), resume, &extras)
            .is_none()
        {
            self.toast("The agent could not start; the assignment remains on the card");
            self.refresh_board_summary(project_id);
            return false;
        }
        self.activate_primitive(&workspace, key);
        self.refresh_board_summary(project_id);
        true
    }

    /// Type a message into a running agent's terminal, but only when it is not
    /// mid-turn; otherwise the message stays in the thread for its next turn.
    fn inject_if_idle(&self, project_id: i64, workspace: &Rc<Workspace>, key: TabKey, text: &str) {
        let Some(primitive) = workspace.tab(key) else {
            return;
        };
        let session_id = stable_session_id(project_id, key, &primitive.program_id);
        let state = self
            .agent_sessions
            .borrow()
            .by_project
            .get(&project_id)
            .and_then(|sessions| {
                sessions
                    .iter()
                    .find(|session| {
                        session.radar_session_id.as_deref() == Some(session_id.as_str())
                    })
                    .cloned()
            })
            .and_then(|session| self.latest_agent_activity(project_id, &session))
            .map(|(state, _, _)| state);
        if matches!(state, Some(crate::session::activity::AgentState::Working)) {
            self.toast("Agent is working; the message is in the card thread");
            return;
        }
        let command = crate::session::daemon::Command::Input {
            id: session_id,
            bytes: format!("{text}\r").into_bytes(),
        };
        if let Err(error) = crate::session::daemon::Client::request(&self.session_home, command) {
            self.toast(&format!("Could not reach the agent: {error}"));
        }
    }

    fn apply_activity_notice(&self, notice: ActivityNotice) -> i64 {
        let project_id = match notice {
            ActivityNotice::Snapshot {
                project_id,
                snapshot,
                replace_events,
            } => {
                let outstanding = snapshot.attention.clone();
                self.activity
                    .borrow_mut()
                    .entry(project_id)
                    .or_insert_with(|| ProjectActivity::empty(project_id))
                    .merge_snapshot(snapshot, replace_events);
                let (withdraw, raise) = {
                    let notified = self.notified_attention.borrow();
                    notify::plan_snapshot(project_id, &notified, &outstanding)
                };
                for request_id in withdraw {
                    self.withdraw_attention_notification(project_id, &request_id);
                }
                for item in raise {
                    self.notify_attention(project_id, &item);
                }
                project_id
            }
            ActivityNotice::Event(event) => {
                let project_id = event.project_id;
                let request = match &event.payload {
                    crate::session::activity::ActivityPayload::AttentionRequested {
                        request_id,
                        attention_kind,
                        reason,
                        ..
                    } => {
                        let already_outstanding =
                            self.activity
                                .borrow()
                                .get(&project_id)
                                .is_some_and(|activity| {
                                    activity.snapshot.attention.iter().any(|attention| {
                                        attention.id.as_str() == request_id.as_str()
                                    })
                                });
                        let notified = self.notified_attention.borrow();
                        notify::plan_request(
                            &notify::AttentionRequest {
                                project_id,
                                request_id,
                                kind: *attention_kind,
                                reason,
                                card_id: event.card_id.as_deref(),
                                session_id: event.session_id.as_deref(),
                            },
                            &notified,
                            already_outstanding,
                        )
                    }
                    _ => None,
                };
                let session_exit = match &event.payload {
                    crate::session::activity::ActivityPayload::SessionLifecycle {
                        state, ..
                    } if state == "exited" || state == "failed" => {
                        Some((event.session_id.clone(), state.clone()))
                    }
                    _ => None,
                };
                let resolved_request = match &event.payload {
                    crate::session::activity::ActivityPayload::AttentionResolved {
                        request_id,
                        ..
                    } => Some(request_id.clone()),
                    _ => None,
                };
                let board_changed = matches!(
                    &event.payload,
                    crate::session::activity::ActivityPayload::BoardChanged { .. }
                );
                self.activity
                    .borrow_mut()
                    .entry(project_id)
                    .or_insert_with(|| ProjectActivity::empty(project_id))
                    .apply_event(event);
                if let Some(item) = request {
                    let outstanding =
                        self.activity
                            .borrow()
                            .get(&project_id)
                            .is_some_and(|activity| {
                                activity.snapshot.attention.iter().any(|attention| {
                                    attention.id.as_str() == item.request_id.as_str()
                                })
                            });
                    if outstanding {
                        self.notify_attention(project_id, &item);
                    }
                }
                if let Some((session_id, state)) = session_exit {
                    self.alert_session_exit(project_id, session_id.as_deref(), &state);
                }
                if let Some(request_id) = resolved_request {
                    self.withdraw_attention_notification(project_id, &request_id);
                }
                if board_changed {
                    self.refresh_board_summary(project_id);
                }
                project_id
            }
            ActivityNotice::Connection { project_id, online } => {
                self.activity_online.borrow_mut().insert(project_id, online);
                project_id
            }
            ActivityNotice::CardComment {
                project_id, error, ..
            } => {
                if let Some(error) = &error {
                    self.toast(&format!("Could not post the comment: {error}"));
                }
                project_id
            }
            ActivityNotice::Mutation {
                project_id,
                request_id,
                result,
            } => {
                // Home's cockpit shares the response path: clear its pending
                // mark whichever widget sent the change.
                self.home_pending_attention.borrow_mut().remove(&request_id);
                match result {
                    Ok(result) => {
                        let result = *result;
                        let attention = result.attention;
                        let resolved_request_id = result
                            .event
                            .as_ref()
                            .and_then(|event| match &event.payload {
                                crate::session::activity::ActivityPayload::AttentionResolved {
                                    request_id,
                                    ..
                                } => Some(request_id.clone()),
                                _ => None,
                            })
                            .or_else(|| {
                                attention
                                    .resolved_at_millis
                                    .is_some()
                                    .then(|| attention.id.clone())
                            });
                        let mut activity = self.activity.borrow_mut();
                        let project = activity
                            .entry(project_id)
                            .or_insert_with(|| ProjectActivity::empty(project_id));
                        if let Some(event) = result.event {
                            project.apply_event(event);
                        }
                        project.upsert_attention(attention.clone());
                        drop(activity);
                        if let Some(request_id) = resolved_request_id {
                            self.withdraw_attention_notification(project_id, &request_id);
                        }
                    }
                    Err(error) => self.toast(&format!("Could not respond: {error}")),
                }
                project_id
            }
        };

        project_id
    }

    fn notify_attention(&self, project_id: i64, item: &notify::Raise) {
        if !self
            .notified_attention
            .borrow_mut()
            .insert((project_id, item.request_id.clone()))
        {
            return;
        }
        let project = self
            .projects
            .borrow()
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.name.clone())
            .unwrap_or_else(|| "Project".to_string());
        let kind_label = match item.kind {
            crate::session::activity::AttentionKind::Question => "Question",
            crate::session::activity::AttentionKind::Approval => "Approval",
            crate::session::activity::AttentionKind::Failure => "Failure",
            crate::session::activity::AttentionKind::Review => "Review",
        };

        // The desktop notification, when the app can raise one.
        if let Some(application) = self.window.application() {
            let notification = gio::Notification::new(&format!("{kind_label} · {project}"));
            notification.set_body(Some(&item.reason));
            notification.set_priority(gio::NotificationPriority::High);
            let target = project_id.to_variant();
            notification.set_default_action_and_target_value("app.open-project", Some(&target));
            notification.add_button_with_target_value(
                "Open Project",
                "app.open-project",
                Some(&target),
            );
            application.send_notification(
                Some(&format!("attention-{project_id}-{}", item.request_id)),
                &notification,
            );
        }

        // The in-app alert, with actions that open exactly what asked.
        let mut actions = Vec::new();
        if let Some(card_id) = &item.card_id {
            actions.push(alert::Action::new(
                "Open card",
                "win.open-card",
                Some((project_id, card_id.as_str()).to_variant()),
            ));
        }
        if let Some(session_id) = &item.session_id {
            actions.push(alert::Action::new(
                "Open session",
                "win.activity-session-open",
                Some((project_id, session_id.as_str()).to_variant()),
            ));
        }
        let tone = match item.kind {
            crate::session::activity::AttentionKind::Failure => alert::Tone::Danger,
            _ => alert::Tone::Warning,
        };
        self.alerts.show(
            tone,
            &format!("{kind_label} · {project}"),
            &item.reason,
            actions,
        );
    }

    /// Announce an agent that stopped (or failed) as an in-app alert, so the
    /// end of a run is visible rather than silently dropping off the session
    /// list.
    fn alert_session_exit(&self, project_id: i64, session_id: Option<&str>, state: &str) {
        let project = self
            .projects
            .borrow()
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.name.clone())
            .unwrap_or_else(|| "Project".to_string());
        let failed = state == "failed";
        let (tone, title) = if failed {
            (alert::Tone::Danger, "Agent failed")
        } else {
            (alert::Tone::Success, "Agent stopped")
        };
        let body = match session_id {
            Some(id) => format!("{project} · {}", board::short_session(id)),
            None => project,
        };
        let actions = vec![alert::Action::new(
            "Open project",
            "win.home-project",
            Some(project_id.to_variant()),
        )];
        self.alerts.show(tone, title, &body, actions);
    }

    fn withdraw_attention_notification(&self, project_id: i64, request_id: &str) {
        if !self
            .notified_attention
            .borrow()
            .contains(&(project_id, request_id.to_string()))
        {
            return;
        }
        if let Some(application) = self.window.application() {
            application.withdraw_notification(&format!("attention-{project_id}-{request_id}"));
        }
    }

    fn refresh_projects(app: &SharedApp) {
        let projects = app.db.projects().unwrap_or_default();
        let selected = *app.current.borrow();
        *app.projects.borrow_mut() = projects.clone();
        app.request_agent_scan();
        app.reconcile_activity_watchers(&projects);
        app.reconcile_board_watchers(&projects);

        if projects.is_empty() {
            // With no projects Home is the empty state — unless a drill-down
            // (New project) is up over it.
            if matches!(app.home_nav.borrow().last(), Some(HomeView::AddProject)) {
                app.sync_toggles();
            } else {
                app.show_home();
            }
            return;
        }

        app.refresh_status();

        // A removed project returns to Home rather than launching another.
        if !app.home_shown.get()
            && (selected.is_none()
                || selected.is_some_and(|id| !projects.iter().any(|p| p.id == id)))
        {
            app.show_home();
        }
        app.sync_toggles();
    }

    /// Filter the project list. A query matches a project's name or path, or
    /// the title of one of its running agent sessions — so typing an agent's
    /// conversation name still finds the project it lives in. Child session
    /// rows are gone; Home is where sessions are opened.
    fn activity_identity(session: &live_agents::AgentSession) -> Option<&str> {
        session
            .radar_session_id
            .as_deref()
            .or_else(|| parse_stable_session_id(&session.id).map(|_| session.id.as_str()))
    }

    fn latest_agent_activity(
        &self,
        project_id: i64,
        session: &live_agents::AgentSession,
    ) -> Option<(crate::session::activity::AgentState, i64, Option<String>)> {
        let identity = Self::activity_identity(session)?;
        self.activity
            .borrow()
            .get(&project_id)?
            .snapshot
            .events
            .iter()
            .rev()
            .find_map(|event| {
                if event.session_id.as_deref() != Some(identity) {
                    return None;
                }
                match &event.payload {
                    crate::session::activity::ActivityPayload::AgentState { state, message } => {
                        Some((*state, event.at_millis, message.clone()))
                    }
                    _ => None,
                }
            })
    }

    /// The listed session a board claim names, if one is present. Claims are
    /// exact `RADAR_AGENT` values, so this is the same match the @claim link
    /// uses — the card's session chip opens it directly.
    fn live_claim_session(
        &self,
        project_id: i64,
        claim: &str,
    ) -> Option<live_agents::AgentSession> {
        self.agent_sessions
            .borrow()
            .by_project
            .get(&project_id)?
            .iter()
            .find(|session| session.claim_id.as_deref() == Some(claim))
            .cloned()
    }

    /// Whether a claim still has a conversation to reopen: an exact binding
    /// captured from its agent (a live session alone is checked separately).
    fn claim_has_exact_session(&self, project_id: i64, claim: &str) -> bool {
        self.db
            .bound_session(project_id, claim)
            .is_ok_and(|bound| bound.is_some())
    }

    /// Whether the claim's bound conversation is gone from the provider's
    /// own store. A launch binds its exact conversation id up front, before
    /// the CLI has written anything, so a crashed first turn leaves a
    /// binding that points at nothing. `false` when radar cannot tell (the
    /// CLI has no readable store, or the read failed): the reopen is still
    /// worth trying then.
    fn bound_conversation_is_lost(&self, project_id: i64, claim: &str) -> bool {
        let path = self
            .workspaces
            .borrow()
            .get(&project_id)
            .map(|workspace| workspace.project.path.clone());
        let Some(path) = path else {
            return false;
        };
        let Some((program_id, provider_session_id)) =
            self.db.bound_session(project_id, claim).ok().flatten()
        else {
            return false;
        };
        crate::programs::sessions::has_provider_session(
            program_id.as_str(),
            &path,
            provider_session_id.as_str(),
        ) == Some(false)
    }

    /// The activity sign a project wears on Home: unresolved attention first,
    /// then the loudest state among its live sessions.
    fn project_activity_sign(&self, project_id: i64) -> activity_sign::Sign {
        let attention = self
            .activity
            .borrow()
            .get(&project_id)
            .map(|activity| {
                activity
                    .snapshot
                    .attention
                    .iter()
                    .filter(|attention| attention.is_unresolved())
                    .count()
            })
            .unwrap_or(0);
        let sessions = self.live_session_signs(project_id);
        activity_sign::project_sign(attention, &sessions)
    }

    /// The signs of a project's live sessions, for `project_sign` to aggregate.
    fn live_session_signs(&self, project_id: i64) -> Vec<activity_sign::Sign> {
        let sessions = self.agent_sessions.borrow();
        let Some(list) = sessions.by_project.get(&project_id) else {
            return Vec::new();
        };
        list.iter()
            .filter(|session| live_agents::sidebar_session_is_live(session))
            .map(|session| self.session_activity_sign(project_id, session))
            .collect()
    }

    /// One session's sign from the journal: its liveness plus the newest state
    /// or lifecycle event recorded against its stable id.
    fn session_activity_sign(
        &self,
        project_id: i64,
        session: &live_agents::AgentSession,
    ) -> activity_sign::Sign {
        let identity = Self::activity_identity(session);
        let activity = self.activity.borrow();
        let events: Vec<&crate::session::activity::ActivityEvent> = identity
            .and_then(|identity| {
                activity
                    .get(&project_id)
                    .map(|activity| (identity, activity))
            })
            .map(|(identity, activity)| {
                activity
                    .snapshot
                    .events
                    .iter()
                    .filter(|event| event.session_id.as_deref() == Some(identity))
                    .collect()
            })
            .unwrap_or_default();
        activity_sign::session_sign(
            live_agents::sidebar_session_is_live(session),
            session.external.is_some(),
            &events,
        )
    }

    /// The live agent session a tab is running, if it runs one: the session
    /// whose stable id is this tab's.
    fn tab_agent_session(&self, project_id: i64, key: TabKey) -> Option<live_agents::AgentSession> {
        let workspace = self.workspaces.borrow().get(&project_id).cloned()?;
        let primitive = workspace.tab(key)?;
        let session_id = stable_session_id(project_id, key, &primitive.program_id);
        let sessions = self.agent_sessions.borrow();
        sessions
            .by_project
            .get(&project_id)?
            .iter()
            .find(|session| session.radar_session_id.as_deref() == Some(session_id.as_str()))
            .cloned()
    }

    /// What a panel's header calls the session: its live title when it has one,
    /// otherwise the program it runs.
    fn panel_session_name(&self, workspace: &Workspace, key: TabKey) -> String {
        if let Some(text) = self.header_info.borrow().get(&(workspace.project.id, key)) {
            if !text.is_empty() {
                return text.clone();
            }
        }
        workspace
            .tab(key)
            .and_then(|primitive| programs::by_id(&primitive.program_id))
            .map(|program| program.name.clone())
            .unwrap_or_else(|| key.label())
    }

    /// A board card's title, by id.
    fn card_title(&self, project_id: i64, card_id: &str) -> Option<String> {
        self.board_states
            .borrow()
            .get(&project_id)?
            .cards
            .iter()
            .find(|card| card.id == card_id)
            .map(|card| card.title.clone())
    }

    /// Push one panel's session name, to-do and activity sign onto its header.
    fn refresh_panel_header(&self, workspace: &Workspace, panel: &Rc<Panel>) {
        let Some(key) = panel.key() else {
            return;
        };
        let project_id = workspace.project.id;
        let session = self.tab_agent_session(project_id, key);
        let sign = session
            .as_ref()
            .map(|session| self.session_activity_sign(project_id, session));
        panel.set_activity(sign);
        panel.set_session(&self.panel_session_name(workspace, key));
        let todo = session.as_ref().and_then(|session| {
            let card_id = self.session_card_id(project_id, session)?;
            let title = self.card_title(project_id, &card_id)?;
            Some((card_id, title))
        });
        match todo {
            Some((card_id, title)) => {
                panel.set_todo(Some(title.as_str()), Some((project_id, card_id)))
            }
            None => panel.set_todo(None, None),
        }
    }

    /// Push every open panel's header from the session index and the board, so
    /// the header shows the session's name, its to-do and its live state.
    fn refresh_all_panel_headers(&self) {
        let workspaces: Vec<Rc<Workspace>> = self.workspaces.borrow().values().cloned().collect();
        for workspace in workspaces {
            for panel in workspace.panels() {
                self.refresh_panel_header(&workspace, &panel);
            }
        }
    }

    /// The board to-do a tab's session is bound to: the process's stable
    /// `RADAR_CARD_ID`, else the card its claim currently holds.
    fn tab_card_id(&self, project_id: i64, key: TabKey) -> Option<String> {
        let session = self.tab_agent_session(project_id, key)?;
        self.session_card_id(project_id, &session)
    }

    fn card_claim_is_live(
        &self,
        project_id: i64,
        card: &crate::session::board_store::StoredCard,
    ) -> bool {
        card.claim.as_deref().is_some_and(|claim| {
            self.live_card_worker(project_id, &card.id, Some(claim))
                .is_some()
        })
    }

    fn card_sessions(&self, project_id: i64, card_id: &str) -> Vec<live_agents::AgentSession> {
        let sessions = self
            .agent_sessions
            .borrow()
            .by_project
            .get(&project_id)
            .cloned()
            .unwrap_or_default();
        sessions
            .into_iter()
            .filter(|session| {
                session.card_ids.iter().any(|id| id == card_id)
                    || self.session_card_id(project_id, session).as_deref() == Some(card_id)
            })
            .collect()
    }

    fn catalog_card_sessions(
        &self,
        project_id: i64,
        card_id: &str,
    ) -> anyhow::Result<Vec<live_agents::AgentSession>> {
        Ok(self
            .project_catalog(project_id, Some(card_id))?
            .iter()
            .filter(|entry| {
                entry.card_ids.iter().any(|id| id == card_id)
                    || entry.card_id.as_deref() == Some(card_id)
            })
            .map(live_agents::catalog_session)
            .collect())
    }

    fn open_card_session(&self, project_id: i64, card_id: &str, identity: Option<&str>) {
        let sessions = match self.catalog_card_sessions(project_id, card_id) {
            Ok(sessions) => sessions,
            Err(error) => {
                self.toast(&format!(
                    "Could not read this to-do's conversations: {error}"
                ));
                return;
            }
        };
        let session = if let Some(identity) = identity {
            sessions.into_iter().find(|session| session.id == identity)
        } else {
            sessions.into_iter().max_by_key(|session| {
                (
                    session.card_id.as_deref() == Some(card_id),
                    session.running,
                    session.last_activity_at,
                )
            })
        };
        if let Some(session) = session {
            if session.running || live_agents::exact_provider_session_id(&session).is_some() {
                self.hud.close_tasks(self);
                self.open_session(project_id, &session, Some(card_id));
                return;
            }
        }
        let card = self
            .fetch_board_state(project_id)
            .and_then(|state| state.cards.into_iter().find(|card| card.id == card_id));
        if let Some(claim) = card.as_ref().and_then(|card| card.claim.as_deref()) {
            if let Some((_, _, runtime)) = self.live_card_worker(project_id, card_id, Some(claim)) {
                self.hud.close_tasks(self);
                self.open_linked_session(project_id, &runtime);
                return;
            }
            if let Ok(Some((provider, provider_session_id))) =
                self.db.bound_session(project_id, claim)
            {
                let linked = crate::session::daemon::Client::request(
                    &self.session_home,
                    crate::session::daemon::Command::CardSessionLink {
                        project_id,
                        card_id: card_id.to_string(),
                        provider,
                        provider_session_id,
                    },
                );
                if linked.is_ok() {
                    self.request_agent_scan();
                    if let Ok(sessions) = self.catalog_card_sessions(project_id, card_id) {
                        if let Some(session) = sessions.into_iter().find(|session| {
                            session.running
                                || live_agents::exact_provider_session_id(session).is_some()
                        }) {
                            self.hud.close_tasks(self);
                            self.open_session(project_id, &session, Some(card_id));
                            return;
                        }
                    }
                }
            }
        }
        self.toast("No reachable conversation is linked. Choose an agent or a stopped conversation to reconnect this to-do.");
        self.open_home_card(project_id, card_id);
    }

    fn session_card_id(
        &self,
        project_id: i64,
        session: &live_agents::AgentSession,
    ) -> Option<String> {
        if session.project_id != project_id {
            return None;
        }
        session
            .card_id
            .clone()
            .or_else(|| {
                session
                    .claim_id
                    .as_deref()
                    .and_then(|claim| self.card_id_for_claim(project_id, claim))
            })
            .or_else(|| {
                let provider_id = live_agents::exact_provider_session_id(session)?;
                let boards = self.board_states.borrow();
                boards.get(&project_id)?.cards.iter().find_map(|card| {
                    let claim = card.claim.as_deref()?;
                    let (program, conversation) =
                        self.db.bound_session(project_id, claim).ok().flatten()?;
                    (program == session.program_id && conversation == provider_id)
                        .then(|| card.id.clone())
                })
            })
    }

    /// The user hid this card's agent panel while the board asked for it
    /// shown: remember the choice so a passive refresh leaves it hidden.
    fn remember_agent_panel_visibility(&self, workspace: &Workspace, key: TabKey) {
        if key.slot != Slot::Agent {
            return;
        }
        let project_id = workspace.project.id;
        let card_id = self.tab_card_id(project_id, key);
        let visibility = card_id.as_deref().and_then(|card_id| {
            self.board_states
                .borrow()
                .get(&project_id)
                .map(|board| board.workspace_visibility(card_id))
        });
        let mut manual_hidden = workspace.manual_hidden_agents.borrow_mut();
        if visibility == Some(crate::session::board_store::CardWorkspaceVisibility::Show)
            && !workspace.is_visible(key)
        {
            if let Some(card_id) = card_id {
                manual_hidden.insert(key, card_id);
            }
        } else {
            manual_hidden.remove(&key);
        }
    }

    fn sync_board_sessions(&self) {
        let workspaces: Vec<Rc<Workspace>> = self.workspaces.borrow().values().cloned().collect();
        for workspace in workspaces {
            self.sync_workspace_sessions(&workspace);
        }
    }

    /// The workspace mirrors the board: a session whose to-do is done or gone
    /// is hidden, and a session whose to-do is In progress or Review is shown.
    /// A to-do still in Todo leaves its session as it is — unless the user
    /// hid that panel, which stays hidden until the policy changes.
    fn sync_workspace_sessions(&self, workspace: &Rc<Workspace>) {
        let project_id = workspace.project.id;
        let mut decisions: Vec<(TabKey, bool)> = Vec::new();
        use crate::session::board_store::CardWorkspaceVisibility;
        for key in workspace.tabs_of_kind(Slot::Agent) {
            let Some(card_id) = self.tab_card_id(project_id, key) else {
                continue;
            };
            let visibility = {
                let states = self.board_states.borrow();
                let Some(board) = states.get(&project_id) else {
                    return;
                };
                board.workspace_visibility(&card_id)
            };
            let manually_hidden = {
                let mut hidden = workspace.manual_hidden_agents.borrow_mut();
                if visibility == CardWorkspaceVisibility::Show {
                    match hidden.get(&key) {
                        Some(manual_card) if manual_card == &card_id => true,
                        Some(_) => {
                            hidden.remove(&key);
                            false
                        }
                        None => false,
                    }
                } else {
                    hidden.remove(&key);
                    false
                }
            };
            if manually_hidden {
                continue;
            }
            match visibility {
                CardWorkspaceVisibility::Show => decisions.push((key, true)),
                CardWorkspaceVisibility::Hide => decisions.push((key, false)),
                CardWorkspaceVisibility::Preserve => {}
            }
        }

        // A live session whose to-do is In progress or Review, but whose panel
        // is not in the workspace: open it. This is how the workspace mirrors
        // the board even after a reload dropped the panel from the saved tabs.
        let mut opened = false;
        let sessions: Vec<live_agents::AgentSession> = self
            .agent_sessions
            .borrow()
            .by_project
            .get(&project_id)
            .cloned()
            .unwrap_or_default();
        for session in sessions {
            let Some(card_id) = session.card_id.as_deref() else {
                continue;
            };
            let active = {
                let states = self.board_states.borrow();
                states.get(&project_id).is_some_and(|board| {
                    board.workspace_visibility(card_id)
                        == crate::session::board_store::CardWorkspaceVisibility::Show
                })
            };
            if !active {
                continue;
            }
            let Some((_, key, _)) = parse_stable_session_id(&session.id) else {
                continue;
            };
            if workspace.tab(key).is_some() {
                continue;
            }
            if self.open_agent_panel(workspace, key, &session.program_id) {
                opened = true;
            }
        }

        for (key, want) in decisions {
            if want && !workspace.is_visible(key) {
                self.show_primitive(workspace, key);
            } else if !want && workspace.is_visible(key) {
                self.toggle_primitive(workspace, key);
            }
        }

        if opened {
            *workspace.zoom.borrow_mut() = None;
            self.layout(workspace);
            self.sync_toggles();
            self.refresh_workspace_headers(workspace);
            self.persist_primitives(workspace);
        }
    }

    /// Open a panel for a session already live in the daemon: attach its tab
    /// and lay it into the arrangement. False when the tab already exists or
    /// the primitive cannot be made.
    fn open_agent_panel(&self, workspace: &Rc<Workspace>, key: TabKey, program_id: &str) -> bool {
        if workspace.tab(key).is_some() {
            return false;
        }
        let Some(primitive) = self.ensure_primitive(workspace, key, Some(program_id), Resume::No)
        else {
            return false;
        };
        let panel = Panel::new();
        panel.insert(key, &primitive.widget);
        panel.rebuild_header();
        workspace.push_panel(panel.clone());
        if let Some(node) = workspace.tree.borrow_mut().as_mut() {
            node.append(&panel);
        }
        true
    }

    /// How many sessions in a project ended recently, for the lane footer's
    /// "stopped" count. Thirty minutes is long enough to notice an agent that
    /// just exited and short enough not to accumulate old runs.
    fn project_recent_exits(&self, project_id: i64) -> usize {
        let now = crate::session::catalog::now_millis();
        let activity = self.activity.borrow();
        let events: Vec<&crate::session::activity::ActivityEvent> = activity
            .get(&project_id)
            .map(|activity| activity.snapshot.events.iter().collect())
            .unwrap_or_default();
        activity_sign::recent_exits(&events, now, 30 * 60 * 1000)
    }

    /// The card a live session works on, matched by the exact claim its process
    /// carries — the reverse of the card's session chip, so a session can lead
    /// back to its board card.
    fn card_id_for_claim(&self, project_id: i64, claim: &str) -> Option<String> {
        self.board_states
            .borrow()
            .get(&project_id)?
            .cards
            .iter()
            .find(|card| card.claim.as_deref() == Some(claim) && !card.done)
            .map(|card| card.id.clone())
    }

    fn refresh_status(&self) {
        let targets: Vec<(i64, std::path::PathBuf)> = self
            .projects
            .borrow()
            .iter()
            .map(|project| (project.id, project.path.clone()))
            .collect();
        if targets.is_empty() {
            return;
        }
        let tx = self.status_tx.clone();
        std::thread::spawn(move || {
            let batch: Vec<(i64, crate::git::Status)> = targets
                .into_iter()
                .map(|(id, path)| (id, crate::git::status(&path)))
                .collect();
            let _ = tx.send(batch);
        });
    }

    fn request_agent_scan(&self) {
        if self.agent_scan_pending.replace(true) {
            return;
        }
        let projects = self.projects.borrow().clone();
        let session_home = self.session_home.clone();
        let tx = self.agent_tx.clone();
        std::thread::spawn(move || {
            let result =
                live_agents::discover(&projects, &session_home).map_err(|error| error.to_string());
            let _ = tx.send(result);
        });
    }

    /// The home panel: the empty state, rebuilt so its dropdowns say what the
    /// preferences say right now. Workspaces stay alive behind it — going
    /// home looks away, it never stops anything.
    fn show_home(self: &Rc<Self>) {
        self.home_shown.set(true);
        // Going Home always lands on the cockpit, not the last drill-down.
        self.home_nav.borrow_mut().clear();
        *self.current.borrow_mut() = None;
        while let Some(child) = self.stack.child_by_name("_home") {
            self.stack.remove(&child);
        }
        self.stack.add_named(&home::panel(self), Some("_home"));
        self.stack.set_visible_child_name("_home");
        let _ = self.db.remember_last_project(None);
        self.sync_toggles();
    }

    /// Rebuild the Home cockpit in place while it is on screen. Attention,
    /// sessions and git keep arriving after Home is built; this keeps the
    /// surface current without re-running show_home (which clears navigation
    /// and forgets the last project). Only the cockpit is rebuilt
    /// from here — the empty state needs the `Rc` only show_home holds.
    fn refresh_home(&self) {
        // The panel headers carry the same session names, to-dos and activity
        // signs as Home, so they refresh together, whichever surface is
        // showing.
        self.refresh_all_panel_headers();
        self.hud.refresh_tasks(self);
        if !self.home_shown.get() {
            return;
        }
        // The Add-a-project picker is its own stack page and keeps its own
        // state (search text, scroll); refresh it by staying on it.
        if matches!(self.home_nav.borrow().last(), Some(HomeView::AddProject)) {
            self.stack.set_visible_child_name("_add");
            return;
        }
        // The Agents page is the same deal: its tiles are live attached
        // terminals, so it updates in place instead of being rebuilt.
        if self.stack.visible_child_name().as_deref() == Some("_agents") {
            self.sync_agents_page_if_visible();
            return;
        }
        if self.stack.visible_child_name().as_deref() != Some("_home") {
            return;
        }
        // With no projects the empty state stands in for the cockpit.
        if self.projects.borrow().is_empty() && self.home_nav.borrow().is_empty() {
            return;
        }
        // Passive updates must not destroy an unfinished to-do or reply.
        // Navigation clears focus; a submitted to-do marks its focus handoff.
        if self.home_focus_todo.get().is_none() {
            if let (Some(focus), Some(home)) = (
                self.window.focus_widget(),
                self.stack.child_by_name("_home"),
            ) {
                if focus.is_ancestor(&home) {
                    let mut widget = Some(focus);
                    while let Some(current) = widget {
                        if current.is::<gtk::Editable>() || current.is::<gtk::TextView>() {
                            return;
                        }
                        widget = current.parent();
                    }
                }
            }
        }
        let card_scroll_position =
            matches!(self.home_nav.borrow().last(), Some(HomeView::Card(_, _)))
                .then(|| {
                    self.stack
                        .child_by_name("_home")
                        .and_then(|view| home_card_scroller(&view))
                        .map(|scroller| scroller.vadjustment().value())
                })
                .flatten();
        // Clear focus before destroying the view so GTK holds no stale widget.
        gtk::prelude::GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
        while let Some(child) = self.stack.child_by_name("_home") {
            self.stack.remove(&child);
        }
        self.stack.add_named(&home::view(self), Some("_home"));
        self.stack.set_visible_child_name("_home");
        if let Some(position) = card_scroll_position {
            if let Some(scroller) = self
                .stack
                .child_by_name("_home")
                .and_then(|view| home_card_scroller(&view))
            {
                restore_scroll_position(&scroller, position);
            }
        }
    }

    /// Open Home's combined Add-a-project picker, switching to Home from
    /// wherever we are. Existing folders it finds get an Add; a typed name
    /// that is not an existing folder gets a Create.
    fn open_home_add(self: &Rc<Self>) {
        gtk::prelude::GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
        self.home_shown.set(true);
        *self.current.borrow_mut() = None;
        if !matches!(self.home_nav.borrow().last(), Some(HomeView::AddProject)) {
            self.home_nav.borrow_mut().push(HomeView::AddProject);
        }
        self.stack.set_visible_child_name("_add");
        self.sync_toggles();
        self.rescan_add();
        App::render_add_results(self, &self.add_search.text());
        self.add_search.grab_focus();
    }

    /// Scan the picker's root for folders to add, off the main thread. The
    /// drainer re-renders the list when the results arrive.
    fn rescan_add(&self) {
        let root = self.add_root.borrow().clone();
        let tx = self.add_tx.clone();
        std::thread::spawn(move || {
            let found = discover::scan(&root, 3, 800);
            let _ = tx.send((root, found));
        });
    }

    /// Rebuild the picker's list for the query: registered projects it matches
    /// (open), a Create row for a typed name that does not exist yet, and
    /// folders found under the scan root that are not projects yet (add).
    fn render_add_results(app: &SharedApp, query: &str) {
        while let Some(child) = app.add_list.first_child() {
            app.add_list.remove(&child);
        }
        let query = query.trim();
        let root = app.add_root.borrow().clone();

        // Create: a bare, valid folder name that is not already on disk.
        if !query.is_empty() && home::valid_project_folder_name(query) && !root.join(query).exists()
        {
            let (row, create) = add_row(
                &format!("Create “{query}”"),
                &format!("new folder in {}", crate::db::abbreviate(&root)),
                "Create",
                "folder-new-symbolic",
            );
            let app_for_create = app.clone();
            let name = query.to_string();
            create.connect_clicked(move |_| {
                let parent = app_for_create
                    .add_root
                    .borrow()
                    .to_string_lossy()
                    .to_string();
                home::create_project_from_fields(&app_for_create, &name, &parent);
            });
            app.add_list.append(&row);
        }

        // Registered projects that match the query: open them.
        let matcher = fuzzy_matcher::skim::SkimMatcherV2::default().ignore_case();
        use fuzzy_matcher::FuzzyMatcher;
        let projects = app.projects.borrow();
        for project in projects.iter() {
            if !query.is_empty()
                && matcher.fuzzy_match(&project.name, query).is_none()
                && matcher
                    .fuzzy_match(&project.display_path(), query)
                    .is_none()
            {
                continue;
            }
            let (row, open) = add_row(
                &project.name,
                &project.display_path(),
                "Open",
                "folder-symbolic",
            );
            let app_for_open = app.clone();
            let project_id = project.id;
            open.connect_clicked(move |_| app_for_open.open_home_project(project_id));
            app.add_list.append(&row);
        }
        drop(projects);

        // Folders under the root that are not projects yet: add them.
        let known: Vec<PathBuf> = app
            .db
            .projects()
            .map(|projects| projects.into_iter().map(|p| p.path).collect())
            .unwrap_or_default();
        let mut all = app.add_candidates.borrow().clone();
        discover::mark_known(&mut all, &known);
        let matches: Vec<Candidate> = discover::filter(&all, query)
            .into_iter()
            .filter(|candidate| !candidate.known)
            .collect();
        for candidate in matches {
            let trailing = if candidate.is_repo {
                "git · Add"
            } else {
                "Add"
            };
            let (row, add) = add_row(
                &candidate.name,
                &candidate.display_path(),
                trailing,
                "folder-symbolic",
            );
            let app_for_add = app.clone();
            let path = candidate.path.clone();
            add.connect_clicked(move |_| {
                app_for_add.import_home_project(&path);
            });
            app.add_list.append(&row);
        }
    }

    /// Import/restore without deleting state or changing files.
    fn import_home_project(self: &Rc<Self>, path: &Path) -> Option<Project> {
        let result = self.db.add_project(path).and_then(|project| {
            self.db.set_archived(project.id, false)?;
            Ok(project)
        });
        match result {
            Ok(project) => {
                App::refresh_projects(self);
                self.toast(&format!("Added {}", project.name));
                Some(project)
            }
            Err(error) => {
                self.toast(&format!("Could not add project: {error}"));
                None
            }
        }
    }

    fn add_home_project(self: &Rc<Self>, path: &Path) {
        if let Some(project) = self.import_home_project(path) {
            self.show_home();
            self.open_home_project(project.id);
        }
    }

    // ---- the workspace and its panes ----

    fn select_project(&self, id: i64) {
        self.select_project_with(id, true);
    }

    fn select_project_with(&self, id: i64, launch_defaults: bool) {
        let Some(project) = self
            .db
            .project(id)
            .ok()
            .flatten()
            .or_else(|| self.projects.borrow().iter().find(|p| p.id == id).cloned())
        else {
            return;
        };
        if *self.current.borrow() != Some(id) {
            self.hud.close_tasks(self);
        }
        *self.current.borrow_mut() = Some(id);
        self.home_shown.set(false);
        self.workspace_for(&project, launch_defaults);
        self.stack.set_visible_child_name(&format!("project-{id}"));
        let _ = self.db.touch_project(id);
        let _ = self.db.remember_last_project(Some(id));
        // Load the board from the store, only when the project's board is
        // enabled.
        match self.db.board_enabled(&project.path) {
            Ok(true) => self.refresh_board_summary(id),
            Ok(false) => {}
            Err(error) => eprintln!("radar: reading board policy: {error}"),
        }
        self.sync_toggles();
        self.refresh_headers();
    }
    fn open_project_home(&self, project_id: i64) {
        if self
            .projects
            .borrow()
            .iter()
            .any(|project| project.id == project_id)
        {
            self.open_home_project(project_id);
            self.window.present();
        } else {
            self.toast("That project is no longer available");
        }
    }

    fn workspace_for(&self, project: &Project, launch_defaults: bool) -> Rc<Workspace> {
        if let Some(existing) = self.workspaces.borrow().get(&project.id) {
            return existing.clone();
        }

        let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        holder.set_vexpand(true);
        holder.set_hexpand(true);

        let workspace = Rc::new(Workspace {
            project: project.clone(),
            tabs: RefCell::new(HashMap::new()),
            manual_hidden_agents: RefCell::new(HashMap::new()),
            programs: RefCell::new(HashMap::new()),
            panels: RefCell::new(Vec::new()),
            positions: RefCell::new(HashMap::new()),
            dividers: RefCell::new(Vec::new()),
            holder: holder.clone(),
            zoom: RefCell::new(None),
            tree: RefCell::new(None),
            save_timeout: RefCell::new(None),
        });

        self.stack
            .add_named(&holder, Some(&format!("project-{}", project.id)));
        self.workspaces
            .borrow_mut()
            .insert(project.id, workspace.clone());

        if project.is_missing() {
            let missing = status_page(
                "dialog-warning-symbolic",
                "Directory missing",
                &format!("{} is not on disk any more.", project.display_path()),
                None,
            );
            self.stack
                .add_named(&missing, Some(&format!("missing-{}", project.id)));
            self.stack
                .set_visible_child_name(&format!("missing-{}", project.id));
            return workspace;
        }

        // Which primitives were on screen last time, and which program each ran.
        let ui_prefs = self.db.ui_prefs().ok();
        let restore = ui_prefs
            .as_ref()
            .map(|prefs| prefs.restore_tabs)
            .unwrap_or(true);
        let stored = if restore {
            self.db.tabs(project.id).unwrap_or_default()
        } else {
            Vec::new()
        };
        let saved_state = if restore {
            self.db.workspace_state(project.id).ok().flatten()
        } else {
            None
        };
        if let Some(state) = &saved_state {
            workspace
                .programs
                .borrow_mut()
                .extend(state.programs.clone());
        }
        let global_preferences = self.db.preferences().unwrap_or_default();
        let preferences = self
            .db
            .project_settings(project.id)
            .unwrap_or_default()
            .apply_to(&global_preferences);
        // Tabs to open, and the program each was running. Stored rows map to
        // tab keys in order — the second `agent` row becomes the second agent
        // tab — so a saved workspace with same-primitive tabs comes back with
        // all of them.
        let mut wanted: Vec<TabKey> = Vec::new();
        let mut key_programs: HashMap<TabKey, String> = HashMap::new();
        if stored.is_empty() {
            if saved_state.is_none() && launch_defaults {
                // A fresh workspace opens with the home panel's layout
                // preset — the agent leads, and the rest is scope. An empty
                // saved snapshot means the user intentionally hid every
                // primitive, so nothing is opened.
                let layout = ui_prefs
                    .as_ref()
                    .and_then(|prefs| prefs.layout)
                    .unwrap_or(NewWorkspaceLayout::Agent);
                for slot in layout.slots() {
                    if *slot == Slot::Board {
                        continue;
                    }
                    let key = workspace.next_key(*slot);
                    wanted.push(key);
                    if let Some(program) = programs::for_slot(*slot, &preferences) {
                        workspace
                            .programs
                            .borrow_mut()
                            .insert(*slot, program.id.clone());
                    }
                }
            }
        } else {
            for tab in &stored {
                if tab.slot == Slot::Board {
                    continue;
                }
                let key = workspace.next_key(tab.slot);
                wanted.push(key);
                key_programs.insert(key, tab.program_id.clone());
                workspace
                    .programs
                    .borrow_mut()
                    .insert(tab.slot, tab.program_id.clone());
            }
        }
        wanted.sort_by_key(|key| {
            (
                PRIMITIVES
                    .iter()
                    .position(|other| other == &key.slot)
                    .unwrap_or(9),
                key.instance,
            )
        });

        if let Some(state) = &saved_state {
            *workspace.positions.borrow_mut() = state.positions.clone();
        }

        // Create each visible primitive before rebuilding the panels that refer
        // to it. VTE starts a child only when its terminal is mapped.
        for key in &wanted {
            let _ = self.ensure_primitive(
                &workspace,
                *key,
                key_programs.get(key).map(String::as_str),
                Resume::No,
            );
        }

        let restore_plan = workspace_restore_plan(saved_state.as_ref(), &wanted);
        let mut panels = Vec::with_capacity(restore_plan.panels.len());
        for saved in &restore_plan.panels {
            let Some(primitive) = workspace.tab(saved.slot) else {
                continue;
            };
            let panel = Panel::new();
            panel.insert(saved.slot, &primitive.widget);
            panel.rebuild_header();
            panels.push(panel);
        }
        *workspace.panels.borrow_mut() = panels.clone();

        if let Some(layout) = restore_plan.layout.as_ref() {
            let layout_panels: Vec<Rc<Panel>> = panels
                .iter()
                .filter(|panel| {
                    panel_id(panel).is_some_and(|id| restore_plan.layout_anchors.contains(&id))
                })
                .cloned()
                .collect();
            let restored = restore_layout(layout, &layout_panels);
            if restored
                .as_ref()
                .is_some_and(|tree| layout_covers(tree, &layout_panels))
            {
                *workspace.tree.borrow_mut() = restored;
            }
        }
        if let Some(zoomed) = restore_plan.zoomed {
            if let Some(panel) = panels.iter().find(|panel| panel_id(panel) == Some(zoomed)) {
                let tree = workspace.tree.borrow_mut().take();
                *workspace.zoom.borrow_mut() = Some((panels.clone(), tree));
                *workspace.panels.borrow_mut() = vec![panel.clone()];
            }
        }
        self.layout(&workspace);
        self.sync_toggles();
        self.persist_primitives(&workspace);
        // The workspace mirrors the board: warm the open/closed set now, before
        // the session drainer runs again.
        self.sync_board_sessions();
        workspace
    }

    /// Open a tab's program if it is not open yet. `program` overrides the
    /// kind's default — how a ＋ opens its second tab running the program the
    /// pick named. `resume` says how an agent starts: fresh, on its own last
    /// conversation, or on one exact stored conversation — how a claimed
    /// card re-opens its agent.
    fn ensure_primitive(
        &self,
        workspace: &Rc<Workspace>,
        key: TabKey,
        program: Option<&str>,
        resume: Resume,
    ) -> Option<Rc<Primitive>> {
        self.ensure_primitive_with(workspace, key, program, resume, &Extras::default())
    }

    fn ensure_primitive_with(
        &self,
        workspace: &Rc<Workspace>,
        key: TabKey,
        program: Option<&str>,
        resume: Resume,
        extras: &Extras,
    ) -> Option<Rc<Primitive>> {
        // Retained only in persisted data for migration, never a workspace tool.
        if key.slot == Slot::Board {
            return None;
        }
        if let Some(existing) = workspace.tab(key) {
            return Some(existing);
        }
        let global_preferences = self.db.preferences().unwrap_or_default();
        let preferences = self
            .db
            .project_settings(workspace.project.id)
            .unwrap_or_default()
            .apply_to(&global_preferences);
        let wanted = program
            .map(|id| id.to_string())
            .or_else(|| workspace.programs.borrow().get(&key.slot).cloned());
        let program = match wanted {
            Some(id) => programs::by_id(&id),
            None => programs::for_slot(key.slot, &preferences),
        }?;

        workspace
            .programs
            .borrow_mut()
            .insert(key.slot, program.id.clone());
        let mut options = self.launch_options();
        match &resume {
            Resume::No => {}
            Resume::Session(id) => options.session = Some(id.clone()),
        }
        options.prompt = extras.prompt.clone();
        options.card = extras.card.clone();
        // An agent meets the board at launch: the board skill that makes it
        // the convention is installed before the agent draws its first frame,
        // and the launch claims work under a name unique to this instance —
        // two agents of the same kind never hold each other's cards.

        if program.kind == Kind::Agent {
            if let Err(error) = crate::setup::install_default_session_hooks() {
                self.toast(&format!(
                    "Could not install session identity tracking: {error}"
                ));
                return None;
            }
            match self.db.board_enabled(&workspace.project.path) {
                Ok(true) => {
                    if let Err(error) = crate::skill::install(&self.db, &workspace.project.path) {
                        eprintln!("radar: setting up the board: {error}");
                    }
                }
                Ok(false) => {}
                Err(error) => eprintln!("radar: reading board policy: {error}"),
            }
            let stamp = extras
                .instance
                .clone()
                .unwrap_or_else(crate::programs::launch::now_stamp);
            options.agent_instance = Some(stamp.clone());
            if matches!(resume, Resume::No) && program.create_session {
                options.create_session = Some(crate::programs::launch::provider_session_id(
                    workspace.project.id,
                    &stamp,
                ));
            }
        }
        let session_id = stable_session_id(workspace.project.id, key, &program.id);
        let mut spec = program.command_spec(&options);
        add_session_environment(
            &mut spec,
            workspace.project.id,
            &workspace.project.path,
            &self.session_home,
            &session_id,
        );
        let theme = self.theme.borrow().clone();
        let pane = Rc::new(Pane::spawn(
            &spec,
            &workspace.project.path,
            &theme,
            &key.label(),
            pane::ShiftEnter::for_slot(key.slot),
            &self.session_home,
            &session_id,
        ));
        publish_session_lifecycle(
            &self.session_home,
            workspace.project.id,
            &session_id,
            "attached",
        );
        let lifecycle_home = self.session_home.clone();
        let lifecycle_session = session_id.clone();
        let project_id = workspace.project.id;
        pane.set_exit_handler(move || {
            publish_session_lifecycle(&lifecycle_home, project_id, &lifecycle_session, "exited");
        });
        // The panel header wants the session's name: whatever the program puts
        // in the terminal title, and its exit when it goes away. The window
        // action routes it — the pane outlives any one panel, so the observer
        // aims at the action, not at a header. The tab key rides along, so two
        // tabs of one kind report apart.
        let window = self.window.clone();
        let project_id = workspace.project.id;
        pane.set_info_observer(move |text| {
            let _ = gtk::prelude::WidgetExt::activate_action(
                &window,
                "win.pane-info",
                Some(&(project_id, key.as_str(), text.unwrap_or_default()).to_variant()),
            );
        });
        let window = self.window.clone();
        pane.set_bell_observer(move || {
            let _ = gtk::prelude::WidgetExt::activate_action(
                &window,
                "win.pane-bell",
                Some(&(project_id, key.as_str()).to_variant()),
            );
        });
        let primitive = Primitive::new(&program, pane);
        if let Resume::Session(id) = resume {
            *primitive.launched_session.borrow_mut() = Some(id.clone());
        }
        workspace.tabs.borrow_mut().insert(key, primitive.clone());
        Some(primitive)
    }

    /// Show or hide a primitive. Hiding never stops the program.
    fn toggle_primitive(&self, workspace: &Rc<Workspace>, key: TabKey) {
        if key.slot == Slot::Custom {
            return;
        }
        self.restore_zoom(workspace);
        let opening = workspace.panel_of(key).is_none();
        if let Some(panel) = workspace.panel_of(key) {
            panel.remove();
            if panel.is_empty() {
                workspace.forget_panel(&panel);
            }
            panel.rebuild_header();
            // A no-op when the pane went away: an empty panel has no header.
        } else {
            // Agents are 1:1 with board to-dos: a bare agent is never created
            // from the dock, the chords or the HUD. Asking for an agent with
            // none on screen opens the project's open to-dos to start one on.
            if key.slot == Slot::Agent && workspace.tabs_of_kind(Slot::Agent).is_empty() {
                let _ =
                    gtk::prelude::WidgetExt::activate_action(&self.window, "win.new-session", None);
                return;
            }
            let Some(primitive) = self.ensure_primitive(workspace, key, None, Resume::No) else {
                self.toast(&format!(
                    "No {} installed — set one in Preferences",
                    label_for(key.slot).to_lowercase()
                ));
                return;
            };
            let panel = Panel::new();
            panel.insert(key, &primitive.widget);
            panel.rebuild_header();
            workspace.push_panel(panel.clone());
            // In an arranged tree, a brand-new pane lands beside the last one.
            if let Some(node) = workspace.tree.borrow_mut().as_mut() {
                node.append(&panel);
            }
        }
        *workspace.zoom.borrow_mut() = None;
        self.layout(workspace);
        self.sync_toggles();
        self.refresh_workspace_headers(workspace);
        self.persist_primitives(workspace);
        // Opening a panel is a claim on it: the chord, the dock button, the
        // menu — every "open" lands the keys in the panel it opened, ready to
        // type into. Hiding goes quietly.
        if opening {
            if let Some(primitive) = workspace.tab(key) {
                primitive.focus();
            }
        }
    }

    /// Leave zoom mode before changing the visible pane set.
    fn restore_zoom(&self, workspace: &Rc<Workspace>) {
        if let Some((panels, tree)) = workspace.zoom.borrow_mut().take() {
            *workspace.panels.borrow_mut() = panels;
            *workspace.tree.borrow_mut() = tree;
        }
    }

    /// Show a tab without hiding it when it is already on screen.
    fn show_primitive(&self, workspace: &Rc<Workspace>, key: TabKey) {
        if !workspace.is_visible(key) {
            self.toggle_primitive(workspace, key);
        }
    }

    /// Clicking a header: show the panel and put the keys in it.
    fn activate_primitive(&self, workspace: &Rc<Workspace>, key: TabKey) {
        let key = workspace.resolve_tab(key);
        self.show_primitive(workspace, key);
        if let Some(primitive) = workspace.tab(key) {
            primitive.focus();
        }
        self.persist_primitives(workspace);
        self.refresh_home();
    }

    /// Open the session bound to this exact board claim. If neither a live
    /// process stamp nor a stored provider conversation identifies it, leave
    /// the current agent untouched rather than guessing from its program.
    fn open_agent_session(&self, workspace: &Rc<Workspace>, claim: &str) {
        let extras = Extras::default();
        let project_id = workspace.project.id;
        let exact_tab = workspace.tabs_of_kind(Slot::Agent).into_iter().find(|key| {
            workspace
                .tab(*key)
                .and_then(|primitive| primitive.pane.as_ref().and_then(|pane| pane.session_pid()))
                .and_then(programs::launch::radar_agent_of)
                .is_some_and(|agent| agent == claim)
        });
        if let Some(key) = exact_tab {
            self.activate_primitive(workspace, key);
            return;
        }
        let sessions = self
            .agent_sessions
            .borrow()
            .by_project
            .get(&project_id)
            .cloned()
            .unwrap_or_default();
        if let Some(session) = sessions
            .iter()
            .find(|session| session.claim_id.as_deref() == Some(claim))
        {
            self.open_catalog_session(project_id, &session.id);
            return;
        }

        let Ok(Some((program_id, provider_session_id))) = self.db.bound_session(project_id, claim)
        else {
            self.toast(&format!("No exact session link for @{claim}"));
            return;
        };
        if let Some(session) = sessions.iter().find(|session| {
            session.program_id.as_str() == program_id.as_str()
                && session.provider_session_id.as_deref() == Some(provider_session_id.as_str())
        }) {
            self.open_catalog_session(project_id, &session.id);
            return;
        }
        let Some(program) = programs::by_id(&program_id) else {
            self.toast("The claimed session's agent is not installed");
            return;
        };
        if program.resume_session.is_empty() {
            self.toast("This agent cannot reopen the exact claimed conversation");
            return;
        }

        let key = workspace.tabs_of_kind(Slot::Agent).into_iter().find(|key| {
            workspace.tab(*key).is_some_and(|primitive| {
                primitive.program_id.as_str() == program_id.as_str()
                    && primitive.launched_session.borrow().as_deref()
                        == Some(provider_session_id.as_str())
            })
        });
        if let Some(key) = key {
            let on_conversation = workspace.tab(key).is_some_and(|primitive| {
                primitive.pane.as_ref().is_some_and(|pane| pane.is_live())
                    && primitive.launched_session.borrow().as_deref()
                        == Some(provider_session_id.as_str())
            });
            if on_conversation {
                self.activate_primitive(workspace, key);
            } else {
                self.relaunch_agent_with(
                    workspace,
                    key,
                    Resume::Session(provider_session_id),
                    &extras,
                );
            }
            return;
        }

        let key = workspace.next_key(Slot::Agent);
        if self
            .ensure_primitive_with(
                workspace,
                key,
                Some(&program_id),
                Resume::Session(provider_session_id),
                &extras,
            )
            .is_some()
        {
            self.activate_primitive(workspace, key);
        } else {
            self.toast("No agent installed — set one in Preferences");
        }
    }

    /// Open a session by its stable identity, whatever kind it is: a live
    /// Radar pane, an external terminal, or a catalog conversation to resume.
    fn open_catalog_session(&self, project_id: i64, identity: &str) {
        let session = self
            .agent_sessions
            .borrow()
            .by_project
            .get(&project_id)
            .and_then(|rows| rows.iter().find(|session| session.id == identity))
            .cloned();
        let Some(session) = session else {
            self.toast("This session is no longer listed");
            return;
        };
        self.open_session(project_id, &session, None);
    }

    fn track_panel_conversation(&self, session: &live_agents::AgentSession) {
        if !session.running {
            return;
        }
        let Some((project, key, _)) = session
            .radar_session_id
            .as_deref()
            .and_then(parse_stable_session_id)
        else {
            return;
        };
        if let Some(primitive) = self
            .workspaces
            .borrow()
            .get(&project)
            .and_then(|workspace| workspace.tab(key))
        {
            if primitive.program_id == session.program_id {
                *primitive.launched_session.borrow_mut() =
                    live_agents::exact_provider_session_id(session).map(str::to_string);
            }
        }
    }

    fn open_session(
        &self,
        project_id: i64,
        session: &live_agents::AgentSession,
        card_id: Option<&str>,
    ) {
        let latest;
        let session = if session.external.is_none() {
            match self.project_catalog(project_id, None) {
                Ok(entries) => {
                    // Refresh live tab identities before resolving a history click: the
                    // provider may have switched since the last three-second poll.
                    for entry in entries.iter().filter(|entry| entry.lifecycle == "running") {
                        self.track_panel_conversation(&live_agents::catalog_session(entry));
                    }
                    if let Some(entry) = entries.iter().find(|entry| {
                        entry.provider == session.program_id
                            && Some(entry.provider_session_id.as_str())
                                == session.provider_session_id.as_deref()
                    }) {
                        latest = live_agents::catalog_session(entry);
                        &latest
                    } else {
                        self.toast("This conversation is no longer in the catalog");
                        return;
                    }
                }
                Err(error) => {
                    self.toast(&format!("Could not verify this conversation: {error}"));
                    return;
                }
            }
        } else {
            session
        };
        if self.current.borrow().as_ref() != Some(&project_id) {
            self.select_project(project_id);
        }
        let Some(workspace) = self.current_workspace() else {
            return;
        };
        #[cfg(target_os = "linux")]
        if let Some(target) = &session.external {
            if let Err(error) =
                live_agents::focus_external(target.pid, target.start_ticks, target.window_pid)
            {
                self.toast(&error);
            }
            return;
        }
        if session.running {
            if let Some(radar_id) = &session.radar_session_id {
                self.open_linked_session(project_id, radar_id);
                return;
            }
        }
        // History: resume the exact recorded conversation. The row counts
        // as exact only when the provider itself reported the id — radar's
        // own backfilled identity is not a conversation (see
        // live_agents::exact_provider_session_id). Without one there is no
        // honest reopen — "last" could be a different conversation than
        // the row the user clicked.
        let Some(program) =
            programs::by_id(&session.program_id).filter(|program| program.installed())
        else {
            self.toast("The session's agent is not installed");
            return;
        };
        let resume = match live_agents::exact_provider_session_id(session) {
            Some(id) if !program.resume_session.is_empty() => Resume::Session(id.to_string()),
            _ => {
                self.toast(
                    "This conversation has no exact reopen link — its agent did not report a session id",
                );
                return;
            }
        };
        // Which tab shows this conversation: one already on it, the
        // session's own stable place, a quiet agent tab — or a new one.
        // Whatever the answer, it is shown the sidebar way: in THE agent
        // panel. Repeated clicks on a row land on the same tab instead of
        // stacking processes on one conversation.
        let keys: Vec<_> = workspace
            .tabs_of_kind(Slot::Agent)
            .into_iter()
            .filter(|key| {
                workspace
                    .tab(*key)
                    .is_some_and(|primitive| primitive.program_id == session.program_id)
            })
            .collect();
        let extras = Extras {
            card: card_id
                .map(str::to_string)
                .or_else(|| self.session_card_id(project_id, session)),
            ..Extras::default()
        };
        let key = agent_tab_for_session(
            &keys,
            session
                .radar_session_id
                .as_ref()
                .and_then(|id| parse_stable_session_id(id))
                .filter(|(parsed, _, _)| *parsed == project_id)
                .filter(|(_, key, _)| {
                    workspace
                        .tab(*key)
                        .is_none_or(|primitive| primitive.program_id == session.program_id)
                })
                .map(|(_, key, _)| key),
            session.provider_session_id.as_deref(),
            |key| {
                workspace
                    .tab(key)
                    .and_then(|primitive| primitive.launched_session.borrow().clone())
            },
            |key| {
                workspace
                    .tab(key)
                    .and_then(|primitive| primitive.pane.clone())
                    .is_some_and(|pane| pane.is_live())
            },
            workspace.next_key(Slot::Agent),
        );
        match workspace.tab(key) {
            Some(primitive) => {
                let on_conversation = primitive.launched_session.borrow().as_deref()
                    == session.provider_session_id.as_deref();
                if primitive.pane.as_ref().is_some_and(|pane| pane.is_live()) && on_conversation {
                    // The conversation asked for is this live tab: showing
                    // it is all the row can do.
                    self.activate_primitive(&workspace, key);
                } else {
                    // The chosen pane is quiet; live unrelated conversations
                    // are never displaced when reopening history.
                    self.relaunch_agent_with(&workspace, key, resume, &extras);
                }
            }
            None => {
                if self
                    .ensure_primitive_with(&workspace, key, Some(&program.id), resume, &extras)
                    .is_some()
                {
                    self.activate_primitive(&workspace, key);
                } else {
                    self.toast("The linked program is not installed");
                }
            }
        }
    }

    fn open_linked_session(&self, project_id: i64, session_id: &str) {
        let Some((id, key, program_id)) = parse_stable_session_id(session_id) else {
            self.toast("This activity item has no stable Radar session link");
            return;
        };
        if id != project_id {
            self.toast("The session link belongs to a different project");
            return;
        }
        if self.current.borrow().as_ref() != Some(&project_id) {
            self.select_project(project_id);
        }
        let Some(workspace) = self.current_workspace() else {
            return;
        };
        let Some(primitive) = self.ensure_primitive(&workspace, key, Some(&program_id), Resume::No)
        else {
            self.toast("The linked program is not installed");
            return;
        };
        if primitive.program_id != program_id {
            self.toast("The linked tab is open with a different program");
            return;
        }
        self.activate_primitive(&workspace, key);
    }

    /// Resume a conversation in a quiet pane, keeping its todo association.
    fn relaunch_agent_with(
        &self,
        workspace: &Rc<Workspace>,
        key: TabKey,
        resume: Resume,
        extras: &Extras,
    ) {
        let Some(primitive) = workspace.tab(key) else {
            return;
        };
        let Some(program) = programs::by_id(&primitive.program_id) else {
            self.activate_primitive(workspace, key);
            return;
        };
        let mut options = self.launch_options();
        match &resume {
            Resume::No => {}
            Resume::Session(id) => options.session = Some(id.clone()),
        }
        options.prompt = extras.prompt.clone();
        options.card = extras.card.clone();
        options.agent_instance = Some(
            extras
                .instance
                .clone()
                .unwrap_or_else(crate::programs::launch::now_stamp),
        );
        let session_id = stable_session_id(workspace.project.id, key, &program.id);
        let mut spec = program.command_spec(&options);
        add_session_environment(
            &mut spec,
            workspace.project.id,
            &workspace.project.path,
            &self.session_home,
            &session_id,
        );
        if let Some(pane) = primitive.pane.as_ref() {
            pane.respawn(&spec);
        }
        *primitive.launched_session.borrow_mut() = match &resume {
            Resume::Session(id) => Some(id.clone()),
            Resume::No => None,
        };
        self.activate_primitive(workspace, key);
    }

    /// Hovering a panel moves keyboard focus into it, the same
    /// focus-follows-pointer behavior as clicking its header.
    fn hover_primitive(&self, workspace: &Rc<Workspace>, key: TabKey) {
        let key = workspace.resolve_tab(key);
        // Hover is mouse intent: a pane mapped under a parked pointer
        // synthesizes an enter the moment it appears — project switches and
        // resizes produce those by the dozen — and acting on one would yank
        // the keys out of whatever the user is navigating, the sidebar while
        // picking a project. Focus follows a moving mouse, not an appearing
        // pane.
        if !self.pointer_is_live() {
            return;
        }
        if workspace.panel_of(key).is_none() {
            return;
        }
        if let Some(primitive) = workspace.tab(key) {
            let focused_here = self.window.focus_widget().is_some_and(|focus| {
                focus == primitive.widget || focus.is_ancestor(&primitive.widget)
            });
            if !focused_here {
                primitive.focus();
            }
        }
    }

    /// True while the pointer is actually moving. A pane mapped under a parked
    /// pointer synthesizes an enter event the moment it appears — switching
    /// projects or resizes produce those by the dozen — and acting on it would
    /// yank the keys out of whatever the user is navigating, the sidebar while
    /// picking a project. Focus follows a moving mouse, not a appearing pane.
    fn pointer_is_live(&self) -> bool {
        const MOTION_WINDOW_MS: i64 = 300;
        glib::monotonic_time() / 1000 - self.pointer_motion_ms.get() < MOTION_WINDOW_MS
    }

    /// Move a pane one place in the visible arrangement while preserving the
    /// existing divider shape and sizes.
    fn move_pane(&self, workspace: &Rc<Workspace>, key: TabKey, delta: isize) {
        let Some(panel) = workspace.panel_of(key) else {
            return;
        };
        let ordered = self.ordered_panels(workspace);
        let Some(index) = ordered
            .iter()
            .position(|candidate| Rc::ptr_eq(candidate, &panel))
        else {
            return;
        };
        let Some(target_index) = index.checked_add_signed(delta) else {
            return;
        };
        let Some(target) = ordered.get(target_index) else {
            return;
        };

        {
            let mut tree = workspace.tree.borrow_mut();
            if tree.is_none() {
                *tree = auto_node(workspace, &workspace.panels());
            }
            let Some(tree) = tree.as_mut() else {
                return;
            };
            if !tree.swap_leaves(&panel, target) {
                return;
            }
        }

        *workspace.zoom.borrow_mut() = None;
        self.layout(workspace);
        self.sync_toggles();
        self.refresh_workspace_headers(workspace);
        self.persist_primitives(workspace);
    }

    /// Exchange two panels' places in the visible arrangement, planting the
    /// auto layout first if the user has not arranged one yet.
    fn swap_panes(&self, workspace: &Rc<Workspace>, first: TabKey, second: TabKey) {
        let first_key = workspace.resolve_tab(first);
        let second_key = workspace.resolve_tab(second);
        if first_key.slot == Slot::Board || second_key.slot == Slot::Board {
            self.toast("Project tasks live in Home");
            return;
        }
        if first_key == second_key || first_key.slot == Slot::Custom {
            return;
        }
        let (Some(first), Some(second)) = (
            workspace.panel_of(first_key),
            workspace.panel_of(second_key),
        ) else {
            return;
        };
        if Rc::ptr_eq(&first, &second) {
            return;
        }
        {
            let mut tree = workspace.tree.borrow_mut();
            if tree.is_none() {
                *tree = auto_node(workspace, &workspace.panels());
            }
            let Some(tree) = tree.as_mut() else {
                return;
            };
            if !tree.swap_leaves(&first, &second) {
                return;
            }
        }
        *workspace.zoom.borrow_mut() = None;
        self.layout(workspace);
        self.sync_toggles();
        self.refresh_workspace_headers(workspace);
        self.persist_primitives(workspace);
        trace(&format!(
            "swap: {} <-> {}",
            first_key.as_str(),
            second_key.as_str()
        ));
    }

    /// Drop panel A on panel B's edge: B's region divides in two and A takes
    /// half — a real, nested split, the way a tiling manager does it. The
    /// zone (left/right/top/bottom) says which half A takes; a middle drop
    /// swaps instead (`swap_panes`).
    fn nest_split(&self, workspace: &Rc<Workspace>, dragged: TabKey, target: TabKey, zone: &str) {
        let dragged = workspace.resolve_tab(dragged);
        let target = workspace.resolve_tab(target);
        if dragged.slot == Slot::Board || target.slot == Slot::Board {
            self.toast("Project tasks live in Home");
            return;
        }
        if dragged == target || dragged.slot == Slot::Custom {
            return;
        }
        let (Some(dragged_panel), Some(target_panel)) =
            (workspace.panel_of(dragged), workspace.panel_of(target))
        else {
            return;
        };
        if Rc::ptr_eq(&dragged_panel, &target_panel) {
            return;
        }
        let (axis, dragged_first) = match zone {
            "left" => (split::Axis::Horizontal, true),
            "top" => (split::Axis::Vertical, true),
            "bottom" => (split::Axis::Vertical, false),
            // right: the dragged panel takes the right side, the way a tiling
            // manager splits by default.
            _ => (split::Axis::Horizontal, false),
        };
        // Plant the arrangement tree from the auto layout if this is the
        // first manual split; from then on the tree is the law.
        let planted = {
            let existing = workspace.tree.borrow_mut().take();
            // The dragged panel is about to take a half of the target's
            // region. Lift it out of wherever it sits first — its place in the
            // auto arrangement — so it lands in the tree exactly once. A panel
            // in two leaves would try to parent one widget into two panes, and
            // the loser half stays empty.
            let others: Vec<Rc<Panel>> = workspace
                .panels()
                .into_iter()
                .filter(|panel| !Rc::ptr_eq(panel, &dragged_panel))
                .collect();
            let mut tree = match existing {
                Some(tree) => tree.take_leaf(&dragged_panel),
                None => auto_node(workspace, &others),
            }
            .or_else(|| auto_node(workspace, &others));
            let (first, second) = if dragged_first {
                (
                    split::Node::leaf(&dragged_panel),
                    split::Node::leaf(&target_panel),
                )
            } else {
                (
                    split::Node::leaf(&target_panel),
                    split::Node::leaf(&dragged_panel),
                )
            };
            let key = format!("tree-{}-{}", dragged.as_str(), target.as_str());
            let ok = match tree.as_mut() {
                Some(tree) => tree.replace(
                    &target_panel,
                    split::Node::split(axis, 0.5, key, first, second),
                ),
                None => false,
            };
            if ok {
                *workspace.tree.borrow_mut() = tree;
            }
            ok
        };
        if !planted {
            return;
        }
        *workspace.zoom.borrow_mut() = None;
        self.layout(workspace);
        self.sync_toggles();
        self.refresh_workspace_headers(workspace);
        self.persist_primitives(workspace);
        trace(&format!(
            "nest: {} took the {} of {}'s pane",
            dragged.as_str(),
            zone,
            target.as_str()
        ));
    }

    /// Replace the program behind one tab, and remember the choice as the
    /// kind's default. The HUD speaks in kinds (`set_primitive_program`); the
    /// chip dropdown names the exact tab.
    fn set_tab_program(
        &self,
        workspace: &Rc<Workspace>,
        key: TabKey,
        program: Program,
        visible: bool,
    ) {
        // Drop the old pane: its process belongs to the program being replaced.
        if let Some(old) = workspace.tabs.borrow_mut().remove(&key) {
            old.widget.unparent();
            if let Some(panel) = workspace.panel_of(key) {
                panel.remove();
                if panel.is_empty() {
                    workspace.forget_panel(&panel);
                }
                panel.rebuild_header();
            }
        }
        workspace
            .programs
            .borrow_mut()
            .insert(key.slot, program.id.clone());
        if visible {
            self.show_primitive(workspace, key);
        } else {
            self.layout(workspace);
            self.sync_toggles();
            self.refresh_workspace_headers(workspace);
            self.persist_primitives(workspace);
        }
        self.toast(&format!("{} → {}", key.label(), program.name));
    }

    /// The HUD's kind-level program choice: land on the kind's tab that
    /// exists, or open the first one.
    fn set_primitive_program(
        &self,
        workspace: &Rc<Workspace>,
        slot: Slot,
        program: Program,
        visible: bool,
    ) {
        let key = workspace.resolve_tab(TabKey::first(slot));
        self.set_tab_program(workspace, key, program, visible);
    }

    /// Fit every visible panel to the space, unless the user arranged a tree.
    fn layout(&self, workspace: &Rc<Workspace>) {
        let focus = self
            .window
            .focus_widget()
            .filter(|focus| focus.is_ancestor(&workspace.holder));
        if focus.is_some() {
            gtk::prelude::GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
        }
        tiling::detach_dividers(&workspace.dividers.borrow());
        workspace.dividers.borrow_mut().clear();
        let panels = workspace.panels();

        for panel in &panels {
            panel.widget.unparent();
        }
        while let Some(child) = workspace.holder.first_child() {
            workspace.holder.remove(&child);
        }

        if panels.is_empty() {
            workspace.holder.append(&status_page(
                "view-list-symbolic",
                "Every pane is hidden",
                "Use the dock along the bottom to bring one back.",
                None,
            ));
            return;
        }

        // Prune the arrangement: panes that went away disappear, and a tree
        // that shrank to a single pane switches back to auto. Both borrows end
        // with their statement: this runs right after a nest-split planted the
        // tree, and a borrow held across the block — an `if let` scrutinee's
        // temporary lives to the end of the block — would collide with the
        // store below and abort the app mid-drop.
        let pruned = workspace
            .tree
            .borrow_mut()
            .take()
            .and_then(|tree| tree.prune(&workspace.panels()));
        if let Some(node @ split::Node::Split { .. }) = pruned {
            *workspace.tree.borrow_mut() = Some(node);
        }

        let panels = workspace.panels();

        for panel in &panels {
            panel.widget.unparent();
        }
        while let Some(child) = workspace.holder.first_child() {
            workspace.holder.remove(&child);
        }

        if panels.is_empty() {
            workspace.holder.append(&status_page(
                "view-list-symbolic",
                "Every pane is hidden",
                "Use the dock along the bottom to bring one back.",
                None,
            ));
            return;
        }

        let root = match workspace.tree.borrow().as_ref() {
            Some(tree) => self.build_tree(workspace, tree),
            None => {
                let workspace = Rc::downgrade(workspace);
                tiling::automatic(
                    panels
                        .iter()
                        .map(|panel| panel.widget.clone().upcast())
                        .collect(),
                    &self.window,
                    move |dividers| {
                        if let Some(workspace) = workspace.upgrade() {
                            *workspace.dividers.borrow_mut() = dividers;
                        }
                    },
                )
            }
        };
        workspace.holder.append(&root);
        if let Some(focus) = focus.filter(|focus| focus.is_ancestor(&workspace.holder)) {
            focus.grab_focus();
        }
        trace(&format!(
            "layout: {} pane(s) {:?}",
            panels.len(),
            panels
                .iter()
                .filter_map(|panel| panel.key())
                .map(|key| key.as_str())
                .collect::<Vec<_>>()
        ));
    }

    /// Turn an arrangement node into widgets.
    fn build_tree(&self, workspace: &Rc<Workspace>, node: &split::Node<Panel>) -> gtk::Widget {
        match node {
            split::Node::Leaf(panel) => panel.widget.clone().upcast::<gtk::Widget>(),
            split::Node::Split {
                axis,
                ratio,
                key,
                first,
                second,
            } => {
                let gtk_axis = match axis {
                    split::Axis::Horizontal => gtk::Orientation::Horizontal,
                    split::Axis::Vertical => gtk::Orientation::Vertical,
                };
                let f = self.build_tree(workspace, first);
                let s = self.build_tree(workspace, second);
                self.stacked(workspace, key, gtk_axis, &f, &s, *ratio)
            }
        }
    }

    /// A draggable divider whose position is remembered as a fraction.
    fn stacked(
        &self,
        workspace: &Rc<Workspace>,
        key: &str,
        orientation: gtk::Orientation,
        first: &gtk::Widget,
        second: &gtk::Widget,
        fraction: f64,
    ) -> gtk::Widget {
        let paned = gtk::Paned::new(orientation);
        paned.set_wide_handle(true);
        paned.set_resize_start_child(true);
        paned.set_resize_end_child(true);
        paned.set_shrink_start_child(false);
        paned.set_shrink_end_child(false);
        paned.set_vexpand(true);
        paned.set_hexpand(true);
        paned.set_focusable(true);
        paned.set_tooltip_text(Some("Focusable divider — use arrow keys to resize"));
        paned.set_start_child(Some(first));
        paned.set_end_child(Some(second));

        tiling::fit_divider(
            &paned,
            fraction,
            workspace.positions.borrow().get(key).copied(),
        );

        let workspace_for_position = workspace.clone();
        let db_for_position = self.db.clone();
        let key = key.to_string();
        paned.connect_position_notify(move |paned| {
            workspace_for_position
                .positions
                .borrow_mut()
                .insert(key.clone(), paned.position());
            schedule_workspace_save(db_for_position.clone(), workspace_for_position.clone());
        });

        tiling::keyboard_resize(&paned, &self.window);
        workspace.dividers.borrow_mut().push(paned.clone());
        paned.upcast()
    }

    /// Zoom the focused pane to the whole window, and back.
    fn toggle_zoom(&self, key: Option<TabKey>) {
        let Some(workspace) = self.current_workspace() else {
            return;
        };
        let previous_zoom = workspace.zoom.borrow_mut().take();
        if let Some((panels, tree)) = previous_zoom {
            *workspace.panels.borrow_mut() = panels;
            *workspace.tree.borrow_mut() = tree;
            self.layout(&workspace);
            self.sync_toggles();
            self.refresh_workspace_headers(&workspace);
            self.persist_primitives(&workspace);
            return;
        }
        let panels = workspace.panels();
        if panels.len() < 2 {
            self.toast("Only one pane is showing");
            return;
        }
        let target = key
            .and_then(|key| workspace.panel_of(key))
            .or_else(|| self.focused_panel(&workspace))
            .unwrap_or_else(|| panels[0].clone());
        // The arrangement waits in the zoom slot while the single pane shows.
        let saved_tree = workspace.tree.borrow_mut().take();
        *workspace.zoom.borrow_mut() = Some((panels.clone(), saved_tree));
        *workspace.panels.borrow_mut() = vec![target];
        self.layout(&workspace);
        self.sync_toggles();
        self.refresh_workspace_headers(&workspace);
        self.persist_primitives(&workspace);
    }

    /// Store the primitives on screen, so the next launch looks the same.
    /// The database snapshot also keeps grouping, active chips and the split
    /// tree so the project comes back in the same arrangement.
    fn persist_primitives(&self, workspace: &Rc<Workspace>) {
        persist_workspace(&self.db, workspace);
        self.refresh_home();
    }

    /// Save every project before the window is closed. Most changes are saved
    /// as they happen; this flushes any final divider movement as well.
    fn persist_workspaces(&self) {
        for workspace in self.workspaces.borrow().values() {
            if let Some(timeout) = workspace.save_timeout.borrow_mut().take() {
                timeout.remove();
            }
            persist_workspace(&self.db, workspace);
        }
    }

    /// Make the dock match the layout. The dock speaks in kinds: a toggle is
    /// on while any tab of that kind is on screen.
    fn sync_toggles(&self) {
        let visible = self
            .current_workspace()
            .map(|workspace| workspace.visible_kinds())
            .unwrap_or_default();
        let global_preferences = self.db.preferences().unwrap_or_default();
        let preferences = self.current_workspace().map_or_else(
            || global_preferences.clone(),
            |workspace| {
                self.db
                    .project_settings(workspace.project.id)
                    .unwrap_or_default()
                    .apply_to(&global_preferences)
            },
        );
        self.workspace_bar.set_visible(!self.home_shown.get());
        if self.home_shown.get() {
            self.hud.close_tasks(self);
        }
        if let Some(project) = self.current_project() {
            self.workspace_title.set_text(&project.name);
        }
        for (slot, button) in self.toggles.borrow().iter() {
            button.set_active(visible.contains(slot));
            let available =
                *slot == Slot::Shell || programs::for_slot(*slot, &preferences).is_some();
            button.set_sensitive(available);
        }
        // The sidebar's active-agent marks are arrangement-derived state,
        // like the dock: every path that relayouts panes lands here.
        self.refresh_home();
    }

    /// Remember the title a panel's program reported and aim it at the panel's
    /// header as the session's name. `None` clears back to the program.
    fn store_pane_title(&self, project_id: i64, key: TabKey, text: Option<String>) {
        {
            let mut info = self.header_info.borrow_mut();
            match &text {
                Some(entry) => {
                    info.insert((project_id, key), entry.clone());
                }
                None => {
                    info.remove(&(project_id, key));
                }
            }
        }
        for workspace in self.workspaces.borrow().values() {
            if workspace.project.id != project_id {
                continue;
            }
            if let Some(panel) = workspace.panel_of(key) {
                panel.set_session(&self.panel_session_name(workspace, key));
            }
        }
    }

    /// Rebuild every panel header, after a preference change.
    fn refresh_headers(&self) {
        self.refresh_all_panel_headers();
    }

    fn refresh_workspace_headers(&self, workspace: &Workspace) {
        for panel in workspace.panels() {
            self.refresh_panel_header(workspace, &panel);
        }
    }
}

/// Save one project's tabs and presentation state to SQLite.
fn persist_workspace(db: &Db, workspace: &Workspace) {
    let zoom = workspace.zoom.borrow();
    let (panels, tree) = match zoom.as_ref() {
        Some((panels, tree)) => (panels.clone(), tree.clone()),
        None => (workspace.panels(), workspace.tree.borrow().clone()),
    };
    let zoomed = zoom
        .as_ref()
        .and_then(|_| workspace.panels().first().and_then(panel_id));

    // One row per tab, same-primitive tabs included: the second agent tab is
    // a second `agent` row, and its key comes back the same way on restore.
    let mut keys: Vec<TabKey> = panels.iter().filter_map(|panel| panel.key()).collect();
    keys.sort_by_key(|key| {
        (
            PRIMITIVES
                .iter()
                .position(|other| other == &key.slot)
                .unwrap_or(9),
            key.instance,
        )
    });

    let mut tabs = Vec::new();
    for (index, key) in keys.iter().enumerate() {
        let program_id = workspace
            .tab(*key)
            .map(|primitive| primitive.program_id.clone())
            .or_else(|| workspace.programs.borrow().get(&key.slot).cloned());
        let Some(program_id) = program_id else {
            continue;
        };
        let mut tab = crate::db::Tab::new(key.slot, program_id);
        tab.sort_order = index as i64;
        tabs.push(tab);
    }

    let saved_panels = panels
        .iter()
        .filter_map(|panel| panel.key().map(|slot| WorkspacePanel { slot }))
        .collect();
    let state = WorkspaceState {
        panels: saved_panels,
        layout: tree.as_ref().and_then(save_layout),
        programs: workspace.programs.borrow().clone(),
        positions: workspace.positions.borrow().clone(),
        board_open: false,
        zoomed,
    };

    if let Err(error) = db.set_tabs(workspace.project.id, &tabs) {
        eprintln!("radar: could not store the panes: {error}");
    }
    if let Err(error) = db.set_workspace_state(workspace.project.id, &state) {
        eprintln!("radar: could not store the workspace layout: {error}");
    }
}

fn schedule_workspace_save(db: SharedDb, workspace: Rc<Workspace>) {
    if let Some(previous) = workspace.save_timeout.borrow_mut().take() {
        previous.remove();
    }
    let workspace_for_save = workspace.clone();
    let source = glib::timeout_add_local_once(Duration::from_millis(180), move || {
        *workspace_for_save.save_timeout.borrow_mut() = None;
        persist_workspace(&db, &workspace_for_save);
    });
    *workspace.save_timeout.borrow_mut() = Some(source);
}

fn panel_id(panel: &Rc<Panel>) -> Option<TabKey> {
    panel.key()
}

fn save_layout(node: &split::Node<Panel>) -> Option<WorkspaceLayout> {
    match node {
        split::Node::Leaf(panel) => Some(WorkspaceLayout::Pane {
            panel: panel_id(panel)?,
        }),
        split::Node::Split {
            axis,
            ratio,
            key,
            first,
            second,
        } => Some(WorkspaceLayout::Split {
            axis: match axis {
                split::Axis::Horizontal => WorkspaceAxis::Horizontal,
                split::Axis::Vertical => WorkspaceAxis::Vertical,
            },
            ratio: *ratio,
            key: key.clone(),
            first: Box::new(save_layout(first)?),
            second: Box::new(save_layout(second)?),
        }),
    }
}

fn restore_layout(node: &WorkspaceLayout, panels: &[Rc<Panel>]) -> Option<split::Node<Panel>> {
    let anchors: Vec<TabKey> = panels.iter().filter_map(panel_id).collect();
    restore_layout_keys(node, &anchors)?;
    restore_layout_nodes(node, panels)
}

fn restore_layout_nodes(
    node: &WorkspaceLayout,
    panels: &[Rc<Panel>],
) -> Option<split::Node<Panel>> {
    match node {
        WorkspaceLayout::Pane { panel } => panels
            .iter()
            .find(|candidate| panel_id(candidate) == Some(*panel))
            .map(split::Node::leaf),
        WorkspaceLayout::Split {
            axis,
            ratio,
            key,
            first,
            second,
        } => Some(split::Node::split(
            match axis {
                WorkspaceAxis::Horizontal => split::Axis::Horizontal,
                WorkspaceAxis::Vertical => split::Axis::Vertical,
            },
            *ratio,
            key.clone(),
            restore_layout_nodes(first, panels)?,
            restore_layout_nodes(second, panels)?,
        )),
    }
}

fn restore_layout_keys(node: &WorkspaceLayout, panels: &[TabKey]) -> Option<Vec<TabKey>> {
    fn collect(node: &WorkspaceLayout, panels: &[TabKey], leaves: &mut Vec<TabKey>) -> Option<()> {
        match node {
            WorkspaceLayout::Pane { panel } if panels.contains(panel) => {
                leaves.push(*panel);
                Some(())
            }
            WorkspaceLayout::Pane { .. } => None,
            WorkspaceLayout::Split {
                ratio,
                first,
                second,
                ..
            } => {
                if !ratio.is_finite() || !(0.05..=0.95).contains(ratio) {
                    return None;
                }
                collect(first, panels, leaves)?;
                collect(second, panels, leaves)
            }
        }
    }

    let mut leaves = Vec::new();
    collect(node, panels, &mut leaves)?;
    if leaves.len() != panels.len()
        || panels
            .iter()
            .any(|panel| leaves.iter().filter(|leaf| **leaf == *panel).count() != 1)
    {
        return None;
    }
    Some(leaves)
}

fn layout_covers(node: &split::Node<Panel>, panels: &[Rc<Panel>]) -> bool {
    let leaves = node.leaves();
    leaves.len() == panels.len()
        && panels
            .iter()
            .all(|panel| leaves.iter().any(|leaf| Rc::ptr_eq(panel, leaf)))
}

/// Which agent tab a sidebar row is about. The precedence keeps repeated
/// clicks from stacking tabs — and processes — on one conversation:
///
/// 1. a tab already launched on the row's conversation, wherever it sits;
/// 2. the row's own stable place — a radar-run session reopens the key it
///    ran at, so its identity survives GUI restarts;
/// 3. an imported conversation has no radar identity: the first quiet tab
///    of the kind, rather than displacing a live one;
/// 4. the next free key — the caller creates the tab and the panel homes it.
fn agent_tab_for_session(
    keys: &[TabKey],
    stable_key: Option<TabKey>,
    wanted: Option<&str>,
    conversation_of: impl Fn(TabKey) -> Option<String>,
    is_live: impl Fn(TabKey) -> bool,
    next_key: TabKey,
) -> TabKey {
    if let Some(wanted) = wanted {
        if let Some(key) = keys
            .iter()
            .copied()
            .find(|key| conversation_of(*key).as_deref() == Some(wanted))
        {
            return key;
        }
    }
    if let Some(key) = stable_key.filter(|key| !is_live(*key)) {
        return key;
    }
    keys.iter()
        .copied()
        .find(|key| !is_live(*key))
        .unwrap_or(next_key)
}

/// Capture the same auto arrangement the responsive container currently uses
/// when a drag turns it into a manual tree.
fn auto_node(workspace: &Workspace, panels: &[Rc<Panel>]) -> Option<split::Node<Panel>> {
    split::auto_node(panels, workspace.holder.width(), workspace.holder.height())
}

/// Append a line to a debug log when `RADAR_TRACE` is set.
fn trace(message: &str) {
    use std::io::Write;
    let Some(path) = std::env::var_os("RADAR_TRACE") else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{message}");
    }
}

#[cfg(test)]
mod home_navigation_tests {
    use super::*;

    struct TestDaemon(std::process::Child);

    impl Drop for TestDaemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[track_caller]
    fn wait_until(mut ready: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for GUI/daemon response"
            );
            drain();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn widgets(root: &gtk::Widget) -> Vec<gtk::Widget> {
        let mut result = vec![root.clone()];
        let mut child = root.first_child();
        while let Some(widget) = child {
            result.extend(widgets(&widget));
            child = widget.next_sibling();
        }
        result
    }

    fn drain() {
        let context = glib::MainContext::default();
        while context.pending() {
            context.iteration(false);
        }
    }

    fn activate(window: &adw::ApplicationWindow, action: &str, target: Option<&glib::Variant>) {
        gtk::prelude::WidgetExt::activate_action(window, action, target).unwrap();
        drain();
    }

    /// Run on a private session bus and GTK Broadway display (see
    /// scripts/home-smoke.sh), never against the user's real application.
    #[test]
    #[ignore = "requires a private D-Bus session and GTK display"]
    fn home_navigation_replaces_the_sidebar_without_losing_project_controls() {
        gtk::init().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        let bins = scratch.path().join("bin");
        std::fs::create_dir(&bins).unwrap();
        let launches = scratch.path().join("launches");
        // The stub answers `session list` empty and records every launch and
        // input line for assertions.
        let stub = format!("#!/bin/sh\nif [ \"$1\" = session ]; then printf '[]'; exit 0; fi\nprintf 'launch\\t%s\\t%s\\t%s\\t%s\\n' \"$0\" \"$RADAR_CARD_ID\" \"$RADAR_AGENT\" \"$*\" >> {launches:?}\nwhile IFS= read -r line; do printf 'input\\t%s\\t%s\\n' \"$RADAR_CARD_ID\" \"$line\" >> {launches:?}; done\n");
        for (name, script) in [
            ("opencode", stub.clone()),
            ("pi", stub),
            ("mise", format!("#!/bin/sh\nprintf '%s\\n' {bins:?}\n")),
        ] {
            let file = bins.join(name);
            std::fs::write(&file, script).unwrap();
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let old_path = std::env::var_os("PATH").unwrap();
        std::env::set_var(
            "PATH",
            std::env::join_paths(std::iter::once(bins).chain(std::env::split_paths(&old_path)))
                .unwrap(),
        );
        let paths = Rc::new(Paths::with_root(scratch.path().join("state")));
        paths.ensure().unwrap();
        let db = Rc::new(Db::open(&paths).unwrap());
        let mut prefs = db.preferences().unwrap();
        prefs.agent_auto_flags = false;
        prefs.set(Slot::Agent, Some("opencode".to_string()));
        db.set_preferences(&prefs).unwrap();
        let _daemon = TestDaemon(
            std::process::Command::new(
                std::env::var("RADAR_TEST_BIN").expect("run scripts/home-smoke.sh"),
            )
            .arg("--home")
            .arg(&paths.data_dir)
            .arg("serve")
            .env_remove("RADAR_CARD_ID")
            .env_remove("RADAR_AGENT")
            .env_remove("RADAR_SESSION_ID")
            .env_remove("RADAR_PROJECT_ID")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
        );
        wait_until(|| {
            crate::session::daemon::Client::request(
                &paths.data_dir,
                crate::session::daemon::Command::Ping,
            )
            .is_ok()
        });
        let folder = scratch.path().join("demo");
        std::fs::create_dir(&folder).unwrap();
        let project = db.add_project(&folder).unwrap();
        db.set_workspace_state(project.id, &WorkspaceState::default())
            .unwrap();
        db.remember_last_project(Some(project.id)).unwrap();
        let app = adw::Application::builder()
            .application_id("dev.omarchy.Radar.HomeTest")
            .build();
        app.register(None::<&gio::Cancellable>).unwrap();
        style::install(&Theme::load());
        let window = build_window(&app, &paths, &db);
        window.present();
        drain();
        let all = widgets(window.upcast_ref());
        assert!(!all
            .iter()
            .any(|widget| widget.has_css_class("projects-sidebar")));
        assert!(window.lookup_action("toggle-sidebar").is_none());
        let stack = all
            .iter()
            .find_map(|widget| widget.clone().downcast::<gtk::Stack>().ok())
            .unwrap();
        let bar = all
            .iter()
            .find(|widget| widget.has_css_class("workspace-bar"))
            .unwrap();
        assert_eq!(stack.visible_child_name().as_deref(), Some("_home"));
        assert!(!bar.is_visible(), "tools are secondary to Home");
        assert!(
            db.tabs(project.id).unwrap().is_empty(),
            "Home never launches tools"
        );

        activate(&window, "win.home-project", Some(&project.id.to_variant()));
        assert_eq!(stack.visible_child_name().as_deref(), Some("_home"));
        let project_view = stack.child_by_name("_home").unwrap();
        assert!(widgets(&project_view)
            .iter()
            .any(|widget| widget.has_css_class("project-view")));
        activate(&window, "win.home-back", None);
        assert_eq!(stack.visible_child_name().as_deref(), Some("_home"));
        assert!(widgets(&stack.child_by_name("_home").unwrap())
            .iter()
            .any(|widget| widget.has_css_class("home-cockpit")));

        activate(
            &window,
            "win.home-project-pin",
            Some(&project.id.to_variant()),
        );
        assert!(db.project(project.id).unwrap().unwrap().pinned);
        activate(
            &window,
            "win.home-project-edit",
            Some(&project.id.to_variant()),
        );
        let edit = gtk::Window::list_toplevels()
            .into_iter()
            .filter_map(|widget| widget.downcast::<gtk::Window>().ok())
            .find(|window| window.title().as_deref() == Some("Edit project name"))
            .unwrap();
        let edit_widgets = widgets(edit.upcast_ref());
        let entry = edit_widgets
            .iter()
            .find_map(|widget| widget.clone().downcast::<gtk::Entry>().ok())
            .unwrap();
        entry.set_text("Renamed demo");
        let save = edit_widgets
            .iter()
            .filter_map(|widget| widget.clone().downcast::<gtk::Button>().ok())
            .find(|button| button.label().as_deref() == Some("Save"))
            .unwrap();
        save.emit_clicked();
        drain();
        assert_eq!(
            db.project(project.id).unwrap().unwrap().name,
            "Renamed demo"
        );

        activate(&window, "win.open-project", Some(&project.id.to_variant()));
        assert_eq!(
            stack.visible_child_name().as_deref(),
            Some(format!("project-{}", project.id).as_str())
        );
        assert!(bar.is_visible());

        // The workspace carries a Board button: it navigates to this
        // project's board (the project view) inside Home, with Alt+K.
        let board_button = widgets(bar)
            .into_iter()
            .filter_map(|widget| widget.downcast::<gtk::Button>().ok())
            .find(|button| button.label().as_deref() == Some("Board"))
            .unwrap_or_else(|| panic!("the workspace bar has a Board button"));
        assert_eq!(
            board_button.action_name().as_deref(),
            Some("win.workspace-project")
        );
        assert!(app
            .accels_for_action("win.workspace-project")
            .iter()
            .any(|keys| keys.as_str() == "<Alt>k"));
        board_button.emit_clicked();
        drain();
        assert_eq!(stack.visible_child_name().as_deref(), Some("_home"));
        assert!(widgets(&stack.child_by_name("_home").unwrap())
            .iter()
            .any(|widget| widget.has_css_class("project-view")));
        activate(&window, "win.open-project", Some(&project.id.to_variant()));
        assert_eq!(
            stack.visible_child_name().as_deref(),
            Some(format!("project-{}", project.id).as_str())
        );

        let todos = widgets(window.upcast_ref())
            .into_iter()
            .find(|widget| widget.has_css_class("hud-root"))
            .unwrap();
        for index in 0..16 {
            crate::session::daemon::board_card_add(
                &paths.data_dir,
                project.id,
                None,
                &format!("List scroll {index}"),
                "",
                None,
                &format!("nav-list-{index}"),
            )
            .unwrap();
        }

        let workspace_width = stack.width();
        activate(&window, "win.new-session", None);
        assert!(todos.is_visible());
        assert_eq!(
            stack.width(),
            workspace_width,
            "the shared dialog must not resize the workspace"
        );
        let list_scroll = widgets(&todos)
            .into_iter()
            .find_map(|widget| widget.downcast::<gtk::ScrolledWindow>().ok())
            .expect("the to-do list is scrollable");
        wait_until(|| {
            list_scroll.vadjustment().upper() > list_scroll.vadjustment().page_size() + 20.0
        });
        list_scroll.vadjustment().set_value(40.0);
        activate(&window, "win.card-saved", Some(&project.id.to_variant()));
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < deadline {
            drain();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            (list_scroll.vadjustment().value() - 40.0).abs() < 1.0,
            "passive board refreshes preserve the filtered to-do list position"
        );

        assert!(widgets(&todos)
            .iter()
            .any(|widget| widget.has_css_class("hud-card")));
        assert_eq!(
            widgets(bar)
                .iter()
                .filter_map(|widget| widget.downcast_ref::<gtk::Button>())
                .filter(|button| button.action_name().as_deref() == Some("win.new-session"))
                .count(),
            1
        );
        assert!(!widgets(bar)
            .iter()
            .filter_map(|widget| widget.downcast_ref::<gtk::Button>())
            .any(|button| button.label().as_deref() == Some("To-dos")
                || button.label().as_deref() == Some("Auto arrange")));
        assert!(window.lookup_action("workspace-todos").is_none());
        assert_eq!(
            stack.visible_child_name().as_deref(),
            Some(format!("project-{}", project.id).as_str())
        );
        let add = widgets(&todos)
            .into_iter()
            .find_map(|widget| widget.downcast::<gtk::Entry>().ok())
            .unwrap();
        add.grab_focus();
        add.set_text("Created inside workspace");
        add.emit_activate();
        drain();
        assert_eq!(
            stack.visible_child_name().as_deref(),
            Some(format!("project-{}", project.id).as_str())
        );
        assert!(widgets(&todos)
            .iter()
            .filter_map(|widget| widget.downcast_ref::<gtk::Label>())
            .any(|label| label.text().as_str() == "Created inside workspace"));
        assert!(widgets(&todos)
            .iter()
            .filter_map(|widget| widget.downcast_ref::<gtk::Entry>())
            .any(|entry| entry.text().is_empty()));

        // Todo -> exact session -> todo stays beside the same pane.
        // Use an inert daemon child, never launch a real agent in this test.
        let card = crate::session::daemon::board_card_add(
            &paths.data_dir,
            project.id,
            None,
            "Navigation todo",
            &(0..100)
                .map(|i| format!("Paragraph {i}\n\n"))
                .collect::<String>(),
            None,
            "nav-add",
        )
        .unwrap()
        .card;
        let session_id = stable_session_id(project.id, TabKey::first(Slot::Agent), "opencode");
        let spawn = crate::session::registry::Spawn {
            id: session_id.clone(),
            argv: vec!["sh".into(), "-c".into(), "sleep 60".into()],
            cwd: folder.clone(),
            env: vec![
                ("RADAR_CARD_ID".into(), card.id.clone()),
                ("RADAR_AGENT".into(), "opencode-nav-test".into()),
            ],
            env_remove: vec![],
            dims: crate::session::Dims { cols: 80, rows: 24 },
        };
        crate::session::daemon::Client::request(
            &paths.data_dir,
            crate::session::daemon::Command::Create(spawn),
        )
        .unwrap();
        activate(
            &window,
            "win.open-card",
            Some(&(project.id, card.id.as_str()).to_variant()),
        );
        wait_until(|| {
            for expander in widgets(&todos)
                .iter()
                .filter_map(|widget| widget.downcast_ref::<gtk::Expander>())
            {
                expander.set_expanded(true);
            }
            widgets(&todos)
                .into_iter()
                .filter_map(|widget| widget.downcast::<gtk::Button>().ok())
                .any(|button| button.action_name().as_deref() == Some("win.card-conversation-open"))
        });
        for lane in ["Todo", "In progress", "Review"] {
            crate::session::daemon::board_card_move(
                &paths.data_dir,
                project.id,
                &card.id,
                lane,
                None,
                &format!("session-parity-{lane}"),
            )
            .unwrap();
            activate(
                &window,
                "win.card-session-open",
                Some(&(project.id, card.id.as_str()).to_variant()),
            );
            assert_eq!(
                stack.visible_child_name().as_deref(),
                Some(format!("project-{}", project.id).as_str())
            );
            activate(
                &window,
                "win.session-todo",
                Some(&(project.id, "agent", card.id.as_str()).to_variant()),
            );
            assert_eq!(
                stack.visible_child_name().as_deref(),
                Some(format!("project-{}", project.id).as_str())
            );
            assert!(todos.is_visible());
            assert!(widgets(&todos)
                .iter()
                .filter_map(|widget| widget.downcast_ref::<gtk::Button>())
                .any(|button| button.is_visible()
                    && button.tooltip_text().as_deref() == Some("Back to to-dos")));
            activate(&window, "win.new-session", None);
            assert_eq!(
                stack.visible_child_name().as_deref(),
                Some(format!("project-{}", project.id).as_str())
            );
        }

        let assigned = crate::session::daemon::board_card_add(
            &paths.data_dir,
            project.id,
            None,
            "Assign to existing session",
            "",
            None,
            "nav-assignment-add",
        )
        .unwrap()
        .card;
        activate(&window, "win.home-project", Some(&project.id.to_variant()));
        wait_until(|| {
            widgets(window.upcast_ref())
                .iter()
                .filter_map(|widget| widget.downcast_ref::<gtk::Label>())
                .any(|label| label.text().as_str() == "Assign to existing session")
        });
        activate(
            &window,
            "win.card-worker",
            Some(
                &(
                    project.id,
                    assigned.id.as_str(),
                    "session",
                    "opencode-nav-test",
                )
                    .to_variant(),
            ),
        );
        assert_eq!(stack.visible_child_name().as_deref(), Some("_home"));
        let crate::session::daemon::Response::BoardState(state) =
            crate::session::daemon::Client::request(
                &paths.data_dir,
                crate::session::daemon::Command::BoardState {
                    project_id: project.id,
                },
            )
            .unwrap()
        else {
            panic!("expected board state")
        };
        assert_eq!(
            state
                .cards
                .iter()
                .find(|card| card.id == assigned.id)
                .unwrap()
                .claim
                .as_deref(),
            None,
            "running conversations cannot be rebound to a different task"
        );
        // An invalid second selection must not invent an assignment.
        activate(
            &window,
            "win.card-worker",
            Some(&(project.id, assigned.id.as_str(), "session", "other-agent").to_variant()),
        );
        let crate::session::daemon::Response::BoardState(state) =
            crate::session::daemon::Client::request(
                &paths.data_dir,
                crate::session::daemon::Command::BoardState {
                    project_id: project.id,
                },
            )
            .unwrap()
        else {
            panic!("expected board state")
        };
        assert_eq!(
            state
                .cards
                .iter()
                .find(|card| card.id == assigned.id)
                .unwrap()
                .claim
                .as_deref(),
            None
        );

        // Starting existing card work from the board never navigates away.
        activate(&window, "win.home-project", Some(&project.id.to_variant()));
        activate(
            &window,
            "win.card-session-create",
            Some(&(project.id, card.id.as_str()).to_variant()),
        );
        assert_eq!(stack.visible_child_name().as_deref(), Some("_home"));
        assert!(!bar.is_visible());
        assert!(!todos.is_visible());

        // Back on the cockpit, the project card grows with its to-dos but
        // keeps its width: the list scrolls only vertically, never caps the
        // card's height, and never widens the card with a long to-do title.
        activate(&window, "win.show-home", None);
        let cockpit = stack.child_by_name("_home").unwrap();
        let scroll = widgets(&cockpit)
            .into_iter()
            .find(|widget| widget.has_css_class("todo-scroll"))
            .expect("the project card lists its to-dos");
        let scroll = scroll
            .downcast::<gtk::ScrolledWindow>()
            .expect("the to-do list is a scroller");
        assert_eq!(
            scroll.max_content_height(),
            -1,
            "the to-do list is not capped to a fixed height"
        );
        assert_eq!(
            scroll.hscrollbar_policy(),
            gtk::PolicyType::Never,
            "the to-do list does not claim its natural width"
        );
        assert_eq!(
            scroll.vscrollbar_policy(),
            gtk::PolicyType::Never,
            "the to-do list grows instead of scrolling"
        );
        let vadj = scroll.vadjustment();
        assert!(
            vadj.upper() <= vadj.page_size() + 0.5,
            "the to-do list grows with its content instead of scrolling \
             (content {}, viewport {})",
            vadj.upper(),
            vadj.page_size()
        );
        let list = widgets(&cockpit)
            .into_iter()
            .find(|widget| widget.has_css_class("todo-list"))
            .expect("the scroller wraps the to-do list");
        assert!(
            list.first_child().is_some(),
            "the to-do list carries the project's to-dos"
        );
        let masonry = widgets(&cockpit)
            .into_iter()
            .find(|widget| widget.has_css_class("lane-grid"))
            .expect("the projects masonry is present");
        let masonry = masonry
            .downcast::<gtk::Box>()
            .expect("the masonry is a box, not a flow box");
        assert_eq!(masonry.orientation(), gtk::Orientation::Horizontal);
        let column = masonry.first_child().expect("the masonry has a column");
        assert!(
            widgets(&column)
                .iter()
                .any(|widget| widget.has_css_class("lane")),
            "the column holds the project card"
        );

        // Agents is a drill-down origin too; returning must reuse its page.
        activate(&window, "win.home-agents", None);
        let agents_page = stack.child_by_name("_agents").unwrap();
        activate(
            &window,
            "win.open-card",
            Some(&(project.id, card.id.as_str()).to_variant()),
        );
        assert_eq!(stack.visible_child_name().as_deref(), Some("_home"));
        let home_thread_scroll = || {
            widgets(&stack.child_by_name("_home").unwrap())
                .into_iter()
                .find(|widget| widget.has_css_class("card-thread"))
                .unwrap()
                .downcast::<gtk::ScrolledWindow>()
                .unwrap()
        };
        wait_until(|| home_thread_scroll().vadjustment().upper() > 1000.0);
        home_thread_scroll().vadjustment().set_value(500.0);
        gtk::prelude::GtkWindowExt::set_focus(&window, None::<&gtk::Widget>);
        activate(&window, "win.card-saved", Some(&project.id.to_variant()));
        wait_until(|| home_thread_scroll().vadjustment().upper() > 1000.0);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < deadline {
            drain();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            (home_thread_scroll().vadjustment().value() - 500.0).abs() < 1.0,
            "Home refreshes preserve a manually scrolled conversation"
        );
        activate(&window, "win.home-back", None);
        assert_eq!(stack.visible_child_name().as_deref(), Some("_agents"));
        assert_eq!(stack.child_by_name("_agents").unwrap(), agents_page);
        activate(&window, "win.home-back", None);
        assert_eq!(stack.visible_child_name().as_deref(), Some("_home"));
        activate(&window, "win.open-project", Some(&project.id.to_variant()));
        let workspace_child = stack
            .child_by_name(&format!("project-{}", project.id))
            .unwrap();
        let has_agent_header = || {
            widgets(&workspace_child)
                .iter()
                .filter_map(|widget| widget.downcast_ref::<gtk::Button>())
                .any(|button| button.action_name().as_deref() == Some("win.session-todo"))
        };
        wait_until(has_agent_header);
        activate(&window, "win.primitive-toggle", Some(&"agent".to_variant()));
        assert!(!has_agent_header(), "the user can hide the agent panel");
        activate(&window, "win.card-saved", Some(&project.id.to_variant()));
        assert!(
            !has_agent_header(),
            "a passive board refresh must not undo the user's panel toggle"
        );
        activate(&window, "win.primitive-toggle", Some(&"agent".to_variant()));
        assert!(has_agent_header(), "the user can show the panel again");

        // Refreshing an open card detail must preserve its manual scroll offset.
        activate(
            &window,
            "win.open-card",
            Some(&(project.id, card.id.as_str()).to_variant()),
        );
        let thread_scroll = || {
            widgets(&todos)
                .into_iter()
                .find(|widget| widget.has_css_class("card-thread"))
                .unwrap()
                .downcast::<gtk::ScrolledWindow>()
                .unwrap()
        };
        wait_until(|| thread_scroll().vadjustment().upper() > 1000.0);
        thread_scroll().vadjustment().set_value(500.0);
        gtk::prelude::GtkWindowExt::set_focus(&window, None::<&gtk::Widget>);
        activate(&window, "win.card-saved", Some(&project.id.to_variant()));
        wait_until(|| thread_scroll().vadjustment().upper() > 1000.0);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < deadline {
            drain();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            (thread_scroll().vadjustment().value() - 500.0).abs() < 1.0,
            "passive updates preserve a manually scrolled conversation"
        );
        activate(
            &window,
            "win.session-todo-done",
            Some(&(project.id, card.id.as_str()).to_variant()),
        );
        wait_until(|| {
            matches!(
                crate::session::daemon::Client::request(
                    &paths.data_dir,
                    crate::session::daemon::Command::BoardState {
                        project_id: project.id
                    },
                ),
                Ok(crate::session::daemon::Response::BoardState(ref state))
                    if state.cards.iter().any(|c| c.id == card.id && c.done)
            )
        });
        wait_until(|| {
            !widgets(&workspace_child)
                .iter()
                .filter_map(|widget| widget.downcast_ref::<gtk::Button>())
                .any(|button| button.action_name().as_deref() == Some("win.session-todo"))
        });
        assert!(
            db.tabs(project.id).unwrap().is_empty(),
            "a done to-do's panel hides; its program keeps running"
        );
        let crate::session::daemon::Response::Sessions(sessions) =
            crate::session::daemon::Client::request(
                &paths.data_dir,
                crate::session::daemon::Command::List,
            )
            .unwrap()
        else {
            panic!("expected daemon sessions")
        };
        assert_eq!(
            sessions
                .iter()
                .filter(|session| session.id.contains("-agent-"))
                .count(),
            1
        );
        activate(&window, "win.workspace-project", None);
        assert!(!bar.is_visible());
        assert!(widgets(&stack.child_by_name("_home").unwrap())
            .iter()
            .any(|widget| widget.has_css_class("project-view")));

        activate(
            &window,
            "win.home-project-archive",
            Some(&project.id.to_variant()),
        );
        let archive = gtk::Window::list_toplevels()
            .into_iter()
            .flat_map(|root| widgets(&root))
            .filter_map(|widget| widget.downcast::<gtk::Button>().ok())
            .find(|button| button.label().as_deref() == Some("Archive"))
            .unwrap();
        archive.emit_clicked();
        wait_until(|| db.projects().unwrap().is_empty());
        drain();
        assert!(db.projects().unwrap().is_empty());
        assert!(db.project(project.id).unwrap().unwrap().archived);
        assert!(
            db.workspace_state(project.id).unwrap().is_some(),
            "archive preserves workspace settings"
        );
        assert!(folder.is_dir(), "archive never deletes project files");
        assert_eq!(stack.visible_child_name().as_deref(), Some("_home"));
        // The combined picker covers new and existing: it opens as its own
        // Home view, and typing a name that is not on disk offers Create.
        activate(&window, "win.home-add-project", None);
        assert_eq!(stack.visible_child_name().as_deref(), Some("_add"));
        let add_view = stack.child_by_name("_add").unwrap();
        let search = widgets(&add_view)
            .into_iter()
            .find_map(|widget| widget.downcast::<gtk::SearchEntry>().ok())
            .unwrap();
        search.set_text("picker-created-xyz");
        search.emit_by_name::<()>("search-changed", &[]);
        drain();
        assert!(
            widgets(&add_view)
                .into_iter()
                .filter_map(|widget| widget.downcast::<gtk::Button>().ok())
                .any(|button| {
                    widgets(button.upcast_ref())
                        .into_iter()
                        .filter_map(|widget| widget.downcast::<gtk::Label>().ok())
                        .any(|label| label.text().as_str() == "Create")
                }),
            "a typed new name offers a Create row"
        );
        activate(&window, "win.home-back", None);
        assert_eq!(stack.visible_child_name().as_deref(), Some("_home"));

        activate(
            &window,
            "win.home-project-import",
            Some(&folder.to_string_lossy().to_string().to_variant()),
        );
        assert_eq!(
            db.projects().unwrap()[0].id,
            project.id,
            "import restores the original record"
        );
        assert!(db.workspace_state(project.id).unwrap().is_some());
        assert!(
            !folder.join(".git").exists(),
            "import never initializes git"
        );
        assert_eq!(
            db.project(project.id).unwrap().unwrap().name,
            "Renamed demo"
        );
        activate(
            &window,
            "win.home-project-create",
            Some(&("created", scratch.path().to_string_lossy().to_string()).to_variant()),
        );
        let created = db
            .project_by_path(scratch.path().join("created"))
            .unwrap()
            .unwrap();
        assert!(created.path.is_dir());
        assert!(
            !created.path.join(".git").exists(),
            "creating a folder project must not initialize git"
        );
        assert!(
            db.tabs(created.id).unwrap().is_empty(),
            "creation stays in the human project view"
        );
        assert!(widgets(&stack.child_by_name("_home").unwrap())
            .iter()
            .any(|widget| widget.has_css_class("project-view")));
        // End-to-end board journey with inert agent executables: create/start,
        // review follow-up, exact stopped resume, and assignment to a conversation.
        activate(
            &window,
            "win.home-add-work",
            Some(&(created.id, "Autonomous task").to_variant()),
        );
        assert_eq!(stack.visible_child_name().as_deref(), Some("_home"));
        let board_state = || match crate::session::daemon::Client::request(
            &paths.data_dir,
            crate::session::daemon::Command::BoardState {
                project_id: created.id,
            },
        )
        .unwrap()
        {
            crate::session::daemon::Response::BoardState(state) => state,
            _ => panic!("expected board state"),
        };
        let work = board_state()
            .cards
            .into_iter()
            .find(|card| card.title == "Autonomous task")
            .unwrap();
        wait_until(|| {
            std::fs::read_to_string(&launches)
                .unwrap_or_default()
                .contains(&work.id)
        });
        assert!(work.claim.is_some());
        let runtime = match crate::session::daemon::Client::request(
            &paths.data_dir,
            crate::session::daemon::Command::List,
        )
        .unwrap()
        {
            crate::session::daemon::Response::Sessions(sessions) => {
                assert_eq!(
                    sessions
                        .iter()
                        .filter(|session| session
                            .id
                            .starts_with(&format!("project-{}-", created.id)))
                        .count(),
                    1,
                    "board launches must not create an unrelated default agent"
                );
                sessions
                    .into_iter()
                    .find(|session| {
                        session
                            .pid
                            .and_then(programs::launch::radar_card_of)
                            .as_deref()
                            == Some(work.id.as_str())
                    })
                    .unwrap()
                    .id
            }
            _ => panic!("expected sessions"),
        };
        crate::session::daemon::board_card_move(
            &paths.data_dir,
            created.id,
            &work.id,
            "Review",
            None,
            "test-review",
        )
        .unwrap();
        wait_until(|| {
            widgets(window.upcast_ref())
                .iter()
                .filter_map(|widget| widget.downcast_ref::<gtk::Label>())
                .any(|label| label.has_css_class("dim-label") && label.text().as_str() == "Review")
        });
        activate(
            &window,
            "win.card-reply",
            Some(&(created.id, work.id.as_str(), "Please add tests").to_variant()),
        );
        wait_until(|| {
            std::fs::read_to_string(&launches)
                .unwrap_or_default()
                .contains("Please add tests")
        });
        assert_eq!(stack.visible_child_name().as_deref(), Some("_home"));
        assert_eq!(
            board_state()
                .cards
                .iter()
                .find(|card| card.id == work.id)
                .unwrap()
                .lane,
            "In progress"
        );
        let catalog_entries = || match crate::session::daemon::Client::request(
            &paths.data_dir,
            crate::session::daemon::Command::CatalogList {
                projects: vec![crate::session::daemon::CatalogProject {
                    id: created.id,
                    path: created.path.clone(),
                }],
                filter: crate::session::catalog::CatalogFilter::Active,
                query: None,
                limit: 100,
            },
        )
        .unwrap()
        {
            crate::session::daemon::Response::Catalog(entries) => entries,
            _ => panic!("expected catalog"),
        };
        let provider_id = catalog_entries()
            .into_iter()
            .find(|session| session.radar_session_id.as_deref() == Some(&runtime))
            .unwrap()
            .provider_session_id;
        crate::session::daemon::Client::request(
            &paths.data_dir,
            crate::session::daemon::Command::Stop {
                id: runtime.clone(),
            },
        )
        .unwrap();
        wait_until(|| {
            catalog_entries().iter().any(|session| {
                session.provider_session_id == provider_id && session.lifecycle == "ended"
            })
        });
        crate::session::daemon::board_card_move(
            &paths.data_dir,
            created.id,
            &work.id,
            "Review",
            None,
            "test-second-review",
        )
        .unwrap();
        wait_until(|| {
            widgets(window.upcast_ref())
                .iter()
                .filter_map(|widget| widget.downcast_ref::<gtk::Label>())
                .any(|label| label.has_css_class("dim-label") && label.text().as_str() == "Review")
        });
        activate(
            &window,
            "win.card-reply",
            Some(
                &(
                    created.id,
                    work.id.as_str(),
                    "Resume this exact conversation",
                )
                    .to_variant(),
            ),
        );
        wait_until(|| {
            std::fs::read_to_string(&launches)
                .unwrap_or_default()
                .contains("Resume this exact conversation")
        });
        assert_eq!(stack.visible_child_name().as_deref(), Some("_home"));
        assert_eq!(
            catalog_entries()
                .iter()
                .filter(|session| session.provider_session_id == provider_id)
                .count(),
            1
        );
        wait_until(|| {
            catalog_entries()
                .iter()
                .find(|session| session.provider_session_id == provider_id)
                .is_some_and(|session| {
                    session.card_id.as_deref() == Some(work.id.as_str())
                        && session.lifecycle == "running"
                })
        });

        crate::session::daemon::Client::request(
            &paths.data_dir,
            crate::session::daemon::Command::PublishActivity(
                crate::session::activity::PublishActivity {
                    project_id: created.id,
                    command_id: "test-agent-summary".into(),
                    session_id: Some(runtime.clone()),
                    card_id: Some(work.id.clone()),
                    kind: crate::session::activity::ActivityKind::Reported,
                    payload: crate::session::activity::ActivityPayload::Message {
                        text: "Implemented the requested tests.".into(),
                    },
                },
            ),
        )
        .unwrap();
        wait_until(|| {
            widgets(window.upcast_ref())
                .iter()
                .filter_map(|widget| widget.downcast_ref::<gtk::Label>())
                .any(|label| label.text().as_str() == "Implemented the requested tests.")
        });
        crate::session::daemon::board_card_complete(
            &paths.data_dir,
            created.id,
            &work.id,
            None,
            "test-human-complete",
        )
        .unwrap();
        activate(
            &window,
            "win.card-reply",
            Some(&(created.id, work.id.as_str(), "Follow up after completion").to_variant()),
        );
        wait_until(|| {
            std::fs::read_to_string(&launches)
                .unwrap_or_default()
                .contains("Follow up after completion")
        });
        assert!(
            !board_state()
                .cards
                .iter()
                .find(|card| card.id == work.id)
                .unwrap()
                .done
        );
        assert_eq!(stack.visible_child_name().as_deref(), Some("_home"));

        let ended = catalog_entries()
            .into_iter()
            .find(|session| session.provider_session_id == provider_id)
            .unwrap();
        crate::session::daemon::Client::request(
            &paths.data_dir,
            crate::session::daemon::Command::Stop {
                id: ended.radar_session_id.clone().unwrap(),
            },
        )
        .unwrap();
        wait_until(|| {
            catalog_entries()
                .iter()
                .any(|session| session.id == ended.id && session.lifecycle == "ended")
        });
        activate(
            &window,
            "win.home-add-todo",
            Some(&(created.id, "Assigned conversation").to_variant()),
        );
        let assigned = board_state()
            .cards
            .into_iter()
            .find(|card| card.title == "Assigned conversation")
            .unwrap();
        activate(
            &window,
            "win.card-worker",
            Some(
                &(
                    created.id,
                    assigned.id.as_str(),
                    "session",
                    format!("catalog-{}", ended.id),
                )
                    .to_variant(),
            ),
        );
        wait_until(|| {
            catalog_entries().iter().any(|session| {
                session.id == ended.id
                    && session.card_id.as_deref() == Some(assigned.id.as_str())
                    && session.lifecycle == "running"
            })
        });
        assert_eq!(stack.visible_child_name().as_deref(), Some("_home"));
        assert!(std::fs::read_to_string(&launches)
            .unwrap()
            .contains("Assigned conversation"));
        activate(
            &window,
            "win.home-add-todo",
            Some(&(created.id, "Chosen pi agent").to_variant()),
        );
        let chosen = board_state()
            .cards
            .into_iter()
            .find(|card| card.title == "Chosen pi agent")
            .unwrap();
        activate(
            &window,
            "win.card-worker",
            Some(&(created.id, chosen.id.as_str(), "agent", "pi").to_variant()),
        );
        wait_until(|| {
            std::fs::read_to_string(&launches)
                .unwrap_or_default()
                .contains("Chosen pi agent")
        });
        assert!(std::fs::read_to_string(&launches)
            .unwrap()
            .contains("/bin/pi"));
        assert_eq!(stack.visible_child_name().as_deref(), Some("_home"));

        window.destroy();
        std::env::set_var("PATH", old_path);
    }
}

#[cfg(test)]
mod activity_ui_tests {
    use super::*;
    use crate::session::activity::{
        ActivityEvent, ActivityKind, AttentionActionKind, AttentionKind, AttentionResponse,
    };

    fn event(sequence: u64, payload: crate::session::activity::ActivityPayload) -> ActivityEvent {
        ActivityEvent {
            id: format!("event-{sequence}"),
            project_id: 73,
            sequence,
            at_millis: sequence as i64 * 100,
            session_id: Some("project-73-agent-0-opencode".to_string()),
            card_id: Some("card-1".to_string()),
            kind: match &payload {
                crate::session::activity::ActivityPayload::AttentionRequested { .. } => {
                    ActivityKind::AttentionRequested
                }
                crate::session::activity::ActivityPayload::AttentionAcknowledged { .. } => {
                    ActivityKind::AttentionAcknowledged
                }
                crate::session::activity::ActivityPayload::AttentionResolved { .. } => {
                    ActivityKind::AttentionResolved
                }
                _ => ActivityKind::Reported,
            },
            payload,
        }
    }

    #[test]
    fn activity_replay_deduplicates_and_resolution_removes_attention_badge() {
        let mut activity = ProjectActivity::empty(73);
        let requested = event(
            1,
            crate::session::activity::ActivityPayload::AttentionRequested {
                request_id: "attention-73-1".to_string(),
                attention_kind: AttentionKind::Approval,
                reason: "Deploy this change?".to_string(),
                allowed_actions: vec![AttentionActionKind::Approve, AttentionActionKind::Deny],
            },
        );
        activity.apply_event(requested.clone());
        activity.apply_event(requested);
        assert_eq!(activity.snapshot.events.len(), 1);
        assert_eq!(activity.snapshot.attention.len(), 1);

        activity.apply_event(event(
            2,
            crate::session::activity::ActivityPayload::AttentionAcknowledged {
                request_id: "attention-73-1".to_string(),
                revision: 2,
            },
        ));
        assert_eq!(activity.snapshot.attention[0].revision, 2);

        activity.apply_event(event(
            3,
            crate::session::activity::ActivityPayload::AttentionResolved {
                request_id: "attention-73-1".to_string(),
                revision: 3,
                response: AttentionResponse::Approve,
            },
        ));
        assert!(activity.snapshot.attention.is_empty());
        assert_eq!(activity.snapshot.watermark, 3);

        activity.merge_snapshot(
            crate::session::activity::ActivitySnapshot {
                project_id: 73,
                watermark: 1,
                events: vec![event(
                    1,
                    crate::session::activity::ActivityPayload::AttentionRequested {
                        request_id: "attention-73-1".to_string(),
                        attention_kind: AttentionKind::Approval,
                        reason: "Deploy this change?".to_string(),
                        allowed_actions: vec![
                            AttentionActionKind::Approve,
                            AttentionActionKind::Deny,
                        ],
                    },
                )],
                attention: vec![crate::session::activity::Attention {
                    id: "attention-73-1".to_string(),
                    source_event_id: "event-1".to_string(),
                    project_id: 73,
                    session_id: Some("project-73-agent-0-opencode".to_string()),
                    card_id: Some("card-1".to_string()),
                    kind: AttentionKind::Approval,
                    reason: "Deploy this change?".to_string(),
                    allowed_actions: vec![AttentionActionKind::Approve, AttentionActionKind::Deny],
                    created_at_millis: 100,
                    seen_at_millis: None,
                    acknowledged_at_millis: None,
                    resolved_at_millis: None,
                    resolution: None,
                    revision: 1,
                }],
                has_more: false,
            },
            true,
        );
        assert!(activity.snapshot.attention.is_empty());
        assert_eq!(activity.snapshot.events.len(), 3);
    }

    #[test]
    fn retired_board_restores_tool_tree_without_board_panel() {
        let agent = TabKey::first(Slot::Agent);
        let diff = TabKey::first(Slot::Diff);
        let board = TabKey::first(Slot::Board);
        let state = WorkspaceState {
            panels: vec![
                WorkspacePanel { slot: agent },
                WorkspacePanel { slot: diff },
            ],
            layout: Some(WorkspaceLayout::Split {
                axis: WorkspaceAxis::Horizontal,
                ratio: 0.5,
                key: "manual".into(),
                first: Box::new(WorkspaceLayout::Pane { panel: agent }),
                second: Box::new(WorkspaceLayout::Pane { panel: diff }),
            }),
            board_open: true,
            ..WorkspaceState::default()
        };

        let plan = workspace_restore_plan(Some(&state), &[agent, diff, board]);
        assert_eq!(plan.zoomed, None);
        assert_eq!(plan.layout_anchors, vec![agent, diff]);
        assert_eq!(
            plan.panels
                .iter()
                .map(|panel| panel.slot)
                .collect::<Vec<_>>(),
            vec![agent, diff]
        );

        let layout = plan.layout.as_ref().unwrap();
        let restored_tree = restore_layout_keys(layout, &plan.layout_anchors).unwrap();
        assert_eq!(restored_tree, vec![agent, diff]);

        let tools_after_dismissal: Vec<TabKey> = plan
            .panels
            .iter()
            .filter(|panel| panel.slot != board)
            .map(|panel| panel.slot)
            .collect();
        assert_eq!(tools_after_dismissal, restored_tree);

        let tree_with_board = WorkspaceLayout::Split {
            axis: WorkspaceAxis::Horizontal,
            ratio: 0.5,
            key: "invalid".into(),
            first: Box::new(WorkspaceLayout::Pane { panel: agent }),
            second: Box::new(WorkspaceLayout::Pane { panel: board }),
        };
        assert!(restore_layout_keys(&tree_with_board, &plan.layout_anchors).is_none());
    }

    #[test]
    fn legacy_board_zoom_state_restores_only_tool_panels() {
        let agent = TabKey::first(Slot::Agent);
        let diff = TabKey::first(Slot::Diff);
        let board = TabKey::first(Slot::Board);
        let state = WorkspaceState {
            panels: vec![
                WorkspacePanel { slot: agent },
                WorkspacePanel { slot: diff },
                WorkspacePanel { slot: board },
            ],
            layout: Some(WorkspaceLayout::Split {
                axis: WorkspaceAxis::Horizontal,
                ratio: 0.5,
                key: "outer".into(),
                first: Box::new(WorkspaceLayout::Pane { panel: agent }),
                second: Box::new(WorkspaceLayout::Split {
                    axis: WorkspaceAxis::Vertical,
                    ratio: 0.5,
                    key: "inner".into(),
                    first: Box::new(WorkspaceLayout::Pane { panel: diff }),
                    second: Box::new(WorkspaceLayout::Pane { panel: board }),
                }),
            }),
            zoomed: Some(board),
            ..WorkspaceState::default()
        };

        let plan = workspace_restore_plan(Some(&state), &[agent, diff, board]);
        assert_eq!(plan.zoomed, None);
        assert_eq!(plan.layout_anchors, vec![agent, diff]);
        assert_eq!(
            restore_layout_keys(plan.layout.as_ref().unwrap(), &plan.layout_anchors),
            Some(vec![agent, diff])
        );
    }

    #[test]
    fn stable_session_links_keep_project_tab_instance_and_hyphenated_program() {
        let (project_id, key, program_id) =
            parse_stable_session_id("project-73-agent-2-claude-code").unwrap();
        assert_eq!(project_id, 73);
        assert_eq!(
            key,
            TabKey {
                slot: Slot::Agent,
                instance: 2
            }
        );
        assert_eq!(program_id, "claude-code");
        assert!(parse_stable_session_id("random-session-id").is_none());
    }
}

#[cfg(test)]
mod agent_tab_tests {
    use super::*;

    fn k(instance: u32) -> TabKey {
        TabKey {
            slot: Slot::Agent,
            instance,
        }
    }

    fn chooser<'a>(
        keys: &'a [TabKey],
        conversations: impl Fn(TabKey) -> Option<String> + 'a,
        live: impl Fn(TabKey) -> bool + 'a,
    ) -> impl Fn(Option<TabKey>, Option<&str>, TabKey) -> TabKey + 'a {
        move |stable_key, wanted, next_key| {
            agent_tab_for_session(keys, stable_key, wanted, &conversations, &live, next_key)
        }
    }

    #[test]
    fn a_row_lands_on_the_tab_already_running_its_conversation() {
        // A radar-run row's own stable place holds a different conversation
        // now; the row's talk went to another tab. The exact match wins —
        // and the stable place's live agent is not disturbed by a relaunch.
        let keys = [k(0), k(3)];
        let choose = chooser(
            &keys,
            |key| {
                if key == k(0) {
                    Some("ses-other".to_string())
                } else {
                    Some("ses-wanted".to_string())
                }
            },
            |key| key == k(0),
        );
        assert_eq!(
            choose(Some(k(0)), Some("ses-wanted"), k(4)),
            k(3),
            "the conversation's own tab beats the row's stable key"
        );
    }

    #[test]
    fn a_radar_run_row_reopens_at_its_own_stable_key() {
        // No tab carries the conversation: the session returns to the key
        // it ran at, even while quiet tabs of the kind sit around.
        let keys = [k(0), k(1)];
        let choose = chooser(&keys, |_| None, |_| false);
        assert_eq!(choose(Some(k(1)), Some("ses-a"), k(2)), k(1));
    }

    #[test]
    fn an_imported_row_reuses_a_quiet_tab_and_never_a_live_one() {
        let keys = [k(0), k(1), k(2)];
        let choose = chooser(&keys, |_| None, |key| key == k(0) || key == k(2));
        assert_eq!(
            choose(None, Some("ses-a"), k(3)),
            k(1),
            "the one quiet tab of the kind takes the conversation"
        );
        assert_eq!(
            choose(None, Some("ses-a"), k(3)),
            k(1),
            "every tab live means a new key, not a displaced agent"
        );
    }

    #[test]
    fn every_tab_live_gives_an_imported_row_a_fresh_key() {
        let keys = [k(0)];
        let choose = chooser(&keys, |_| None, |_| true);
        assert_eq!(choose(None, Some("ses-a"), k(1)), k(1));
    }

    #[test]
    fn a_history_row_never_displaces_a_live_agent_at_its_old_key() {
        let keys = [k(0), k(1)];
        let choose = chooser(&keys, |_| Some("ses-other".into()), |key| key == k(0));
        assert_eq!(choose(Some(k(0)), Some("ses-wanted"), k(2)), k(1));
        let choose = chooser(&keys, |_| Some("ses-other".into()), |_| true);
        assert_eq!(choose(Some(k(0)), Some("ses-wanted"), k(2)), k(2));
    }

    #[test]
    fn a_row_without_a_conversation_id_skips_the_exact_match() {
        // No provider id to match: the stable key and the quiet tab still
        // answer, in that order.
        let keys = [k(0), k(1)];
        let choose = chooser(
            &keys,
            |key| (key == k(0)).then(|| "ses-a".to_string()),
            |key| key == k(0),
        );
        assert_eq!(choose(Some(k(1)), None, k(2)), k(1));
        assert_eq!(choose(None, None, k(2)), k(1), "k(0) is live");
    }
}
