//! The app window.
//!
//! A project workspace is a handful of primitives — editor, agent, diff,
//! terminal, the board — and nothing else. Home leads; workspace tools decide
//! which primitives are on
//! screen, and the layout arranges them the same way every time:
//!
//!   ┌──────────┬───────────────────────────────┐
//!   │ ▣ ▤ ◫ ▦  │   editor      │      agent     │
//!   │ filter + │               ├────────────────┤
//!   │ project  │               │      diff      │
//!   │ project  ├───────────────┴────────────────┤
//!   │ project  │            terminal            │
//!   └──────────┴───────────────────────────────┘
//!
//! Hiding a primitive detaches its widget; the program keeps running, so putting
//! the agent away for a moment never interrupts it.

mod board;
mod card;
mod confetti;
mod dialogs;
mod group;
mod home;
mod hud;
mod keynav;
mod live_agents;
mod markdown;
mod notify;
mod pane;
mod primitive;
mod split;
mod style;
mod theme;

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
    Db, NewWorkspaceLayout, Project, Slot, TabKey, WorkspaceAxis, WorkspaceGroup, WorkspaceLayout,
    WorkspaceState,
};
use crate::discover::{self, Candidate};
use crate::programs::{self, CommandSpec, Kind, LaunchOptions, Program};

use group::Group;
use pane::Pane;
use primitive::{label_for, Primitive};
pub use theme::Theme;

type SharedDb = Rc<Db>;

/// The four content primitives, in layout order: the agent leads, because that
/// is what the workspace is for.
const PRIMITIVES: [Slot; 4] = [Slot::Agent, Slot::Diff, Slot::Shell, Slot::Editor];

type ZoomState = (Vec<Rc<Group>>, Option<split::Node<Group>>);

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

/// A project's workspace: its tabs, arranged into groups.
///
/// A group is a pane with one header. It holds one tab by default; drag a
/// header onto another and they share a header, with a chip each to switch.
/// Drag one out again and it becomes its own pane. Tabs are per-key — radar
/// can run two agent tabs side by side — while the dock, the chords and the
/// HUD keep aiming at a primitive's first tab. The layout places groups by
/// what they lead with: agent on the left, changes and editor stacked beside
/// it, commands along the bottom.
struct Workspace {
    project: Project,
    /// Open tabs, by key.
    tabs: RefCell<HashMap<TabKey, Rc<Primitive>>>,
    /// The default program per primitive kind, used when a tab of that kind
    /// opens and remembered when one changes, even before it is opened.
    programs: RefCell<HashMap<Slot, String>>,
    /// The panes, in layout order.
    groups: RefCell<Vec<Rc<Group>>>,
    /// Divider positions the user dragged, keyed by layout signature.
    positions: RefCell<HashMap<String, i32>>,
    /// Focusable dividers in the currently rendered layout.
    dividers: RefCell<Vec<gtk::Paned>>,
    /// Holds exactly one child: the layout built from `groups`.
    holder: gtk::Box,
    /// The panes to return to after a zoom, with the arrangement it had.
    zoom: RefCell<Option<ZoomState>>,
    /// The user's manual arrangement. None keeps the classic auto layout;
    /// the first body-drop split plants it, and pruning keeps it honest.
    tree: RefCell<Option<split::Node<Group>>>,
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

    fn groups(&self) -> Vec<Rc<Group>> {
        self.groups.borrow().clone()
    }

    fn dividers(&self) -> Vec<gtk::Paned> {
        self.dividers.borrow().clone()
    }

    /// Which pane holds a tab.
    fn group_of(&self, key: TabKey) -> Option<Rc<Group>> {
        self.groups
            .borrow()
            .iter()
            .find(|group| group.contains(key))
            .cloned()
    }

    fn is_visible(&self, key: TabKey) -> bool {
        self.group_of(key).is_some()
    }

    /// Which primitive kinds have a tab on screen — what the dock and the
    /// HUD's "on screen" marks show.
    fn visible_kinds(&self) -> Vec<Slot> {
        let mut kinds: Vec<Slot> = self
            .groups
            .borrow()
            .iter()
            .flat_map(|group| group.tabs())
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

    /// The tab a pane leads with: the first one in `PRIMITIVES` it holds,
    /// oldest instance first.
    fn anchor(group: &Rc<Group>) -> Option<TabKey> {
        group.tabs().into_iter().min_by_key(|key| {
            (
                PRIMITIVES
                    .iter()
                    .position(|other| other == &key.slot)
                    .unwrap_or(9),
                key.instance,
            )
        })
    }

    fn push_group(&self, group: Rc<Group>) {
        self.groups.borrow_mut().push(group);
    }

    fn forget_group(&self, group: &Rc<Group>) {
        self.groups
            .borrow_mut()
            .retain(|other| !Rc::ptr_eq(other, group));
    }
}

struct App {
    db: SharedDb,
    session_home: PathBuf,
    theme: RefCell<Theme>,
    window: adw::ApplicationWindow,
    workspace_bar: gtk::Box,
    workspace_title: gtk::Button,
    toggles: RefCell<HashMap<Slot, gtk::ToggleButton>>,
    /// Home leads the dock; it is checked while the home panel shows.
    /// True while the home panel is on screen instead of a project's
    /// workspace — the empty state, or the user's explicit "go home".
    home_shown: Cell<bool>,
    stack: gtk::Stack,
    toasts: adw::ToastOverlay,
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
}

#[derive(Clone)]
enum HomeView {
    Project(i64),
    Card(i64, String),
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
    Last,
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

/// Adopt a provider conversation for a radar-spawned session once the CLI's
/// own store reveals which session the run had. Fire-and-forget: the catalog
/// row upgrades in place, and the sidebar gains an exact reopen link.
fn bind_provider_session(
    home: &std::path::Path,
    radar_id: &str,
    program_id: &str,
    provider_session_id: &str,
) {
    use crate::session::daemon::{Client, Command};
    let request = Command::CatalogBind {
        radar_id: radar_id.to_string(),
        provider: program_id.to_string(),
        provider_session_id: provider_session_id.to_string(),
    };
    let _ = Client::request(home, request);
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
    let workspace_title = gtk::Button::new();
    workspace_title.add_css_class("flat");
    workspace_title.add_css_class("heading");
    workspace_title.set_hexpand(true);
    workspace_title.set_halign(gtk::Align::Start);
    workspace_title.set_action_name(Some("win.workspace-project"));
    workspace_title.set_tooltip_text(Some("Back to this project's tasks and conversations"));
    workspace_bar.append(&workspace_title);
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
    let adder_header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    adder_header.add_css_class("home-view-bar");
    let adder_back = gtk::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Back to Home")
        .build();
    adder_back.add_css_class("flat");
    adder_back.set_action_name(Some("win.home-back"));
    adder_header.append(&adder_back);
    let adder_title = gtk::Label::new(Some("Add a project"));
    adder_title.add_css_class("heading");
    adder_title.set_hexpand(true);
    adder_title.set_xalign(0.0);
    adder_header.append(&adder_title);
    let adder_choose = gtk::Button::with_label("Choose folder…");
    adder_choose.add_css_class("flat");
    adder_choose.set_tooltip_text(Some("Add a folder anywhere, with the system chooser"));
    adder_choose.set_action_name(Some("win.home-import-dialog"));
    adder_header.append(&adder_choose);

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
    window.set_content(Some(&root));
    window.set_tooltip_text(Some(&format!("state: {}", paths.database().display())));

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
    start_status_drainer(&state);
    start_activity_drainer(&state);
    start_agent_session_drainer(&state);
    start_add_drainer(&state);
    wire_workspace_drop(&state);
    watch_theme(&state);
    state.add_root_button.set_label(&format!(
        "from {}",
        crate::db::abbreviate(&state.add_root.borrow())
    ));
    App::refresh_projects(&state);
    state.request_agent_scan();
    start_agent_session_polling(&state);
    // Development aid: exercise the new-project flow — folder, git init, add,
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
    if std::env::var("RADAR_GROUP_TEST").is_ok() {
        let state_for_group = state.clone();
        glib::timeout_add_local_once(Duration::from_millis(1400), move || {
            if let Some(workspace) = state_for_group.current_workspace() {
                state_for_group.show_primitive(&workspace, TabKey::first(Slot::Diff));
                state_for_group.group_into(
                    &workspace,
                    TabKey::first(Slot::Diff),
                    TabKey::first(Slot::Agent),
                );
            }
        });
    }
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

fn item(label: &str, action: &str) -> gio::MenuItem {
    gio::MenuItem::new(Some(label), Some(action))
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
                        app.refresh_menus();
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
        // The focused pane's menu, without the mouse: Menu / Shift+F10 (see
        // keynav, which takes the chord ahead of the terminal).
        let app = app.clone();
        add(
            "pane-menu",
            Box::new(move || {
                let Some(workspace) = app.current_workspace() else {
                    return;
                };
                if let Some(group) = app.focused_group(&workspace) {
                    group.menu_button.popup();
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
            app_for_action.toggle_primitive(&workspace, key);
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
        // The sidebar's unified session action: every row — live tab, catalog
        // history, or external terminal — opens through here.
        let action = gio::SimpleAction::new(
            "project-session-open",
            Some(glib::VariantTy::new("(xs)").expect("a project/identity tuple")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some((project_id, identity)) =
                parameter.and_then(|value| value.get::<(i64, String)>())
            else {
                return;
            };
            app_for_action.open_catalog_session(project_id, &identity);
        });
        app.window.add_action(&action);
    }
    {
        let action = gio::SimpleAction::new(
            "project-agent-create",
            Some(glib::VariantTy::new("x").expect("a project ID")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some(project_id) = parameter.and_then(|value| value.get::<i64>()) else {
                return;
            };
            app_for_action.select_project(project_id);
            let Some(workspace) = app_for_action
                .current_workspace()
                .filter(|workspace| workspace.project.id == project_id)
            else {
                return;
            };
            let global = app_for_action.db.preferences().unwrap_or_default();
            let preferences = app_for_action
                .db
                .project_settings(project_id)
                .unwrap_or_default()
                .apply_to(&global);
            let Some(program) = programs::for_slot(Slot::Agent, &preferences) else {
                app_for_action.toast("No agent installed — set one in Preferences");
                return;
            };
            app_for_action.add_tab(&workspace, TabKey::first(Slot::Agent), &program);
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
            app_for_action.select_project(project_id);
            if let Some(workspace) = app_for_action.current_workspace() {
                app_for_action.open_agent_session(&workspace, &claim);
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
            app_for_action.add_home_todo(project_id, &title);
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
        // The card detail's Edit control: the board's card dialog.
        let action = gio::SimpleAction::new(
            "card-edit",
            Some(glib::VariantTy::new("(xs)").expect("a project and card ID")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            if let Some((project_id, card_id)) =
                parameter.and_then(|value| value.get::<(i64, String)>())
            {
                app_for_action.edit_home_card(project_id, &card_id);
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
        // Dropping one header onto another: they share the target's header.
        let action = gio::SimpleAction::new(
            "primitive-group",
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
            app_for_action.group_into(&workspace, TabKey::parse(&pair.0), TabKey::parse(&pair.1));
        });
        app.window.add_action(&action);
    }
    {
        // The chip's ＋: another tab of the same primitive, grouped under the
        // same header, running the program the item named.
        let action = gio::SimpleAction::new(
            "primitive-add",
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
            let Some(program) = programs::by_id(&pair.1) else {
                return;
            };
            app_for_action.add_tab(&workspace, TabKey::parse(&pair.0), &program);
        });
        app.window.add_action(&action);
    }
    {
        // Dropping a chip on a pane's content pulls it out into its own pane.
        let action = gio::SimpleAction::new("primitive-split-out", Some(glib::VariantTy::STRING));
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some(name) = parameter.and_then(|value| value.get::<String>()) else {
                return;
            };
            let Some(workspace) = app_for_action.current_workspace() else {
                return;
            };
            app_for_action.split_out(&workspace, TabKey::parse(&name));
        });
        app.window.add_action(&action);
    }
    {
        // Dropping a pane onto another pane's body: the target's region
        // divides in two and the dragged pane takes the dropped half.
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
        // Which program fills a primitive: the "preferred X" choice, per pane.
        let action = gio::SimpleAction::new("primitive-program", Some(glib::VariantTy::STRING));
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some(name) = parameter.and_then(|value| value.get::<String>()) else {
                return;
            };
            let key = TabKey::parse(&name);
            if app_for_action.current_workspace().is_none() {
                return;
            }
            app_for_action
                .hud
                .present_programs(&app_for_action, key.slot);
        });
        app.window.add_action(&action);
    }
    {
        // The chip dropdown's pick: one program, one click, straight to the
        // same replace-and-relaunch the HUD choice takes.
        let action = gio::SimpleAction::new(
            "primitive-program-set",
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
            let Some(program) = programs::by_id(&pair.1) else {
                return;
            };
            app_for_action.set_tab_program(&workspace, TabKey::parse(&pair.0), program, true);
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
                let Some(key) = app
                    .focused_group(&workspace)
                    .and_then(|group| group.active_key())
                else {
                    return;
                };
                app.hud.present_programs(&app, key.slot);
            }),
        );
    }
    {
        let action = gio::SimpleAction::new("pane-close", Some(glib::VariantTy::STRING));
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some(name) = parameter.and_then(|value| value.get::<String>()) else {
                return;
            };
            let Some(workspace) = app_for_action.current_workspace() else {
                return;
            };
            app_for_action.close_pane(&workspace, TabKey::parse(&name));
        });
        app.window.add_action(&action);
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
        // A pane's program reported live state — its own title, or its exit.
        // Aim it at the pane's header, wherever the pane is grouped today.
        // Empty text clears: the header goes back to just the chips.
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
            app_for_action.store_header_info(project_id, TabKey::parse(&slot), text);
        });
        app.window.add_action(&action);
    }
    {
        // The terminal bell — how agent CLIs ask for attention. The pane's
        // header marks it until that pane is looked at.
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
                if let Some(group) = workspace.group_of(key) {
                    group.set_attention(key);
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

    // ---- keyboard ----
    // Alt is radar's only modifier, so every Ctrl chord reaches the programs
    // in the panels the way their authors wrote them. The one exception is
    // cycling: the window manager owns Alt+Tab, so the cycle stays on Ctrl.
    let accels: [(&str, &[&str]); 15] = [
        ("win.preferences", &["<Alt>comma"]),
        ("win.refresh", &["<Alt>r"]),
        ("win.quit", &["<Alt>q"]),
        ("win.zoom", &["<Alt>f"]),
        ("win.hud", &["<Alt>h"]),
        ("win.show-home", &["<Alt>Home", "<Alt>b"]),
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
            let first = workspace
                .groups()
                .first()
                .and_then(|group| group.active_key());
            if let Some(primitive) = first.and_then(|key| workspace.tab(key)) {
                primitive.focus();
            }
        }
    }

    /// A pane's own menu: only operations that act on this pane or its active
    /// tab. Other tabs are opened from the dock or the HUD.
    fn primitive_menu_model(&self, workspace: &Rc<Workspace>, key: TabKey) -> gio::Menu {
        let menu = gio::Menu::new();
        if key.slot != Slot::Board {
            let program = gio::Menu::new();
            program.append_item(&item(
                &format!("Change {} program…", key.label()),
                &format!("win.primitive-program::{}", key.as_str()),
            ));
            menu.append_section(None, &program);
        }

        let Some(group) = workspace.group_of(key) else {
            return menu;
        };

        let group_actions = gio::Menu::new();
        let held_kinds: Vec<Slot> = group.tabs().iter().map(|member| member.slot).collect();
        let others: Vec<Slot> = PRIMITIVES
            .iter()
            .filter(|other| !held_kinds.contains(other) && **other != Slot::Custom)
            .copied()
            .collect();
        for other in others {
            let entry = gio::MenuItem::new(Some(&format!("Group with {}", label_for(other))), None);
            entry.set_action_and_target_value(
                Some("win.primitive-group"),
                Some(&(key.as_str(), TabKey::first(other).as_str()).to_variant()),
            );
            group_actions.append_item(&entry);
        }
        if group.tabs().len() > 1 {
            let entry = gio::MenuItem::new(Some("Split out into its own pane"), None);
            entry.set_action_and_target_value(
                Some("win.primitive-split-out"),
                Some(&key.as_str().to_variant()),
            );
            group_actions.append_item(&entry);
        }
        if group_actions.n_items() > 0 {
            menu.append_section(Some("Group"), &group_actions);
        }

        let panel = gio::Menu::new();
        let close = gio::MenuItem::new(Some("Close pane"), None);
        close.set_action_and_target_value(Some("win.pane-close"), Some(&key.as_str().to_variant()));
        panel.append_item(&close);
        if group.tabs().len() > 1 {
            let close_tab = gio::MenuItem::new(Some(&format!("Close {}", key.label())), None);
            close_tab.set_action_and_target_value(
                Some("win.primitive-close"),
                Some(&key.as_str().to_variant()),
            );
            panel.append_item(&close_tab);
        }

        let ordered = self.ordered_groups(workspace);
        if let Some(index) = ordered
            .iter()
            .position(|candidate| Rc::ptr_eq(candidate, &group))
        {
            if index > 0 {
                panel.append_item(&item(
                    "Move up",
                    &format!("win.pane-move-up::{}", key.as_str()),
                ));
            }
            if index + 1 < ordered.len() {
                panel.append_item(&item(
                    "Move down",
                    &format!("win.pane-move-down::{}", key.as_str()),
                ));
            }
        }
        let zoom = gio::MenuItem::new(Some("Zoom pane (Alt+F)"), None);
        zoom.set_action_and_target_value(Some("win.pane-zoom"), Some(&key.as_str().to_variant()));
        panel.append_item(&zoom);
        menu.append_section(Some("Pane"), &panel);
        menu
    }

    /// One chip's inline program dropdown: only programs of the chip's own
    /// kind — an agent chip lists agents, an editor chip lists editors — with
    /// the one running now marked. Each pick replaces this tab's program.
    fn chip_program_menu(&self, workspace: &Rc<Workspace>, key: TabKey) -> gio::Menu {
        let menu = gio::Menu::new();
        let Some(kind) = programs::Kind::from_slot(key.slot) else {
            return menu;
        };
        let current = workspace
            .tab(key)
            .map(|primitive| primitive.program_id.clone())
            .or_else(|| workspace.programs.borrow().get(&key.slot).cloned());
        let section = gio::Menu::new();
        for program in programs::embeddable().iter().filter(|p| p.kind == kind) {
            let is_current = current.as_deref() == Some(program.id.as_str());
            let label = if is_current {
                format!("✓ {}", program.name)
            } else {
                program.name.clone()
            };
            let entry = gio::MenuItem::new(Some(&label), None);
            entry.set_action_and_target_value(
                Some("win.primitive-program-set"),
                Some(&(key.as_str(), program.id.clone()).to_variant()),
            );
            section.append_item(&entry);
        }
        if section.n_items() > 0 {
            menu.append_section(None, &section);
        }
        menu
    }

    /// The chip's ＋: the same kind-filtered list as the dropdown, but each
    /// pick adds another tab of this primitive — grouped under this header as
    /// a new chip, running the program the item names.
    fn chip_add_menu(&self, key: TabKey) -> gio::Menu {
        let menu = gio::Menu::new();
        let Some(kind) = programs::Kind::from_slot(key.slot) else {
            return menu;
        };
        let section = gio::Menu::new();
        for program in programs::embeddable().iter().filter(|p| p.kind == kind) {
            let entry = gio::MenuItem::new(Some(&format!("New {} tab", program.name)), None);
            entry.set_action_and_target_value(
                Some("win.primitive-add"),
                Some(&(key.as_str(), program.id.clone()).to_variant()),
            );
            section.append_item(&entry);
        }
        if section.n_items() > 0 {
            menu.append_section(None, &section);
        }
        menu
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
    fn focused_group(&self, workspace: &Workspace) -> Option<Rc<Group>> {
        let focus = self.window.focus_widget()?;
        workspace
            .groups()
            .into_iter()
            .find(|group| focus.is_ancestor(&group.widget))
    }

    /// The visible pane order. In auto mode, derive the same order the renderer
    /// uses; in manual mode, the saved split tree is authoritative.
    fn ordered_groups(&self, workspace: &Workspace) -> Vec<Rc<Group>> {
        workspace
            .tree
            .borrow()
            .as_ref()
            .map(split::Node::leaves)
            .or_else(|| auto_node(&workspace.groups()).map(|tree| tree.leaves()))
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
                app.refresh_menus();
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
        *self.theme.borrow_mut() = theme;
    }

    fn launch_options(&self) -> LaunchOptions {
        let preferences = self.db.preferences().unwrap_or_default();
        LaunchOptions {
            safe: !preferences.agent_auto_flags,
            extra_args: Vec::new(),
            prompt: None,
            resume: false,
            session: None,
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
    fn add_home_todo(&self, project_id: i64, title: &str) {
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
            Ok(_) => {
                self.home_focus_todo.set(Some(project_id));
                self.home_todo_drafts.borrow_mut().remove(&project_id);
                self.refresh_board_summary(project_id);
            }
            Err(error) => self.toast(&format!("Could not add the to-do: {error}")),
        }
    }

    /// Open a card as a conversation inside Home.
    fn open_home_card(&self, project_id: i64, card_id: &str) {
        self.enter_home_view(HomeView::Card(project_id, card_id.to_string()));
    }

    /// Drill into a project's own view inside Home.
    fn open_home_project(&self, project_id: i64) {
        self.enter_home_view(HomeView::Project(project_id));
    }

    fn enter_home_view(&self, view: HomeView) {
        gtk::prelude::GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
        self.home_shown.set(true);
        *self.current.borrow_mut() = None;
        self.home_nav.borrow_mut().push(view);
        self.stack.set_visible_child_name("_home");
        self.sync_toggles();
    }

    /// Leave the current Home drill-down and return to the cockpit. The picker
    /// is a separate stack page, so Back from it shows the cockpit explicitly.
    fn home_back(self: &Rc<Self>) {
        gtk::prelude::GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
        self.home_nav.borrow_mut().pop();
        if self.home_nav.borrow().is_empty() {
            self.show_home();
            return;
        }
        if matches!(self.home_nav.borrow().last(), Some(HomeView::AddProject)) {
            self.stack.set_visible_child_name("_add");
        } else {
            self.stack.set_visible_child_name("_home");
            self.refresh_home();
        }
    }

    /// Edit a card's title and body through a small store-backed dialog.
    fn edit_home_card(self: &Rc<Self>, project_id: i64, card_id: &str) {
        let card = self
            .board_states
            .borrow()
            .get(&project_id)
            .and_then(|state| state.cards.iter().find(|card| card.id == card_id))
            .cloned();
        let Some(card) = card else {
            self.toast("That card is no longer on the board");
            return;
        };
        let window = gtk::Window::builder()
            .title("Edit card")
            .transient_for(&self.window)
            .modal(true)
            .resizable(false)
            .default_width(460)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        content.set_margin_top(14);
        content.set_margin_bottom(14);
        content.set_margin_start(14);
        content.set_margin_end(14);
        let title = gtk::Entry::new();
        title.set_text(&card.title);
        title.set_placeholder_text(Some("Title"));
        content.append(&title);
        let body = gtk::TextView::new();
        body.buffer().set_text(&card.body);
        body.set_wrap_mode(gtk::WrapMode::WordChar);
        let scroll = gtk::ScrolledWindow::builder()
            .height_request(160)
            .child(&body)
            .build();
        content.append(&scroll);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let window_for_cancel = window.clone();
        cancel.connect_clicked(move |_| window_for_cancel.close());
        buttons.append(&cancel);
        let save = gtk::Button::with_label("Save");
        save.add_css_class("suggested-action");
        buttons.append(&save);
        content.append(&buttons);
        window.set_child(Some(&content));

        let app = self.clone();
        let card_id = card.id.clone();
        let revision = card.revision;
        let window_for_save = window.clone();
        save.connect_clicked(move |_| {
            let buffer = body.buffer();
            let text = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                .to_string();
            let new_title = title.text().trim().to_string();
            if new_title.is_empty() {
                title.grab_focus();
                return;
            }
            let command = gui_command_id("edit");
            match crate::session::daemon::board_card_update(
                &app.session_home,
                project_id,
                &card_id,
                Some(&new_title),
                Some(&text),
                Some(revision),
                &command,
            ) {
                Ok(_) => {
                    window_for_save.close();
                    app.refresh_board_summary(project_id);
                }
                Err(error) => eprintln!("radar: editing the card: {error}"),
            }
        });
        window.present();
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

    /// Route a human message on a card: a **live** claimed agent gets it typed
    /// into its terminal (only when it is not mid-turn); a **dormant** claimed
    /// agent is resumed — first on the exact conversation bound to the claim,
    /// else its last; a **to-do** starts the project's default agent attached
    /// to the card and claims it. The comment is always recorded on the thread.
    fn message_card(&self, project_id: i64, card_id: &str, text: &str) {
        card::publish_comment(
            &self.session_home,
            &self.activity_tx,
            project_id,
            card_id,
            text.to_string(),
        );
        let card = self
            .board_states
            .borrow()
            .get(&project_id)
            .and_then(|state| state.cards.iter().find(|card| card.id == card_id).cloned());
        let Some(card) = card else {
            self.toast("That card is no longer on the board");
            return;
        };
        let prompt = format!(
            "A human sent a message on board card \"{}\":\n\n{}\n\nRead the card and its \
             thread with `radar card show \"{}\"` and continue.",
            card.title, text, card.id
        );
        if self.current.borrow().as_ref() != Some(&project_id) {
            self.select_project(project_id);
        }
        let Some(workspace) = self.current_workspace() else {
            return;
        };

        if let Some(claim) = card.claim.clone() {
            let live = workspace.tabs_of_kind(Slot::Agent).into_iter().find(|key| {
                workspace
                    .tab(*key)
                    .and_then(|primitive| {
                        primitive.pane.as_ref().and_then(|pane| pane.session_pid())
                    })
                    .and_then(programs::launch::radar_agent_of)
                    .is_some_and(|agent| agent == claim)
            });
            if let Some(key) = live {
                self.show_agent_session(&workspace, key);
                self.inject_if_idle(project_id, &workspace, key, text);
                return;
            }
            // Dormant: resume the agent that holds the claim.
            self.open_agent_session_with(&workspace, &claim, Some(prompt));
            self.toast("Message sent; resuming the agent on this card");
            return;
        }

        // No claim: a to-do. Start the project's default agent attached to it.
        let global = self.db.preferences().unwrap_or_default();
        let preferences = self
            .db
            .project_settings(project_id)
            .unwrap_or_default()
            .apply_to(&global);
        let Some(program) = programs::for_slot(Slot::Agent, &preferences) else {
            self.toast("No agent installed — set one in Preferences");
            return;
        };
        let key = workspace.next_key(Slot::Agent);
        let stamp = crate::programs::launch::now_stamp();
        let claim = format!("{}-{}", program.id, stamp);
        let extras = Extras {
            prompt: Some(prompt),
            card: Some(card_id.to_string()),
            instance: Some(stamp),
        };
        if self
            .ensure_primitive_with(&workspace, key, Some(&program.id), Resume::No, &extras)
            .is_none()
        {
            self.toast("The agent is not installed");
            return;
        }
        self.show_agent_session(&workspace, key);
        // Claim it for the new instance, so its gate passes and the next
        // message routes straight back to it.
        let _ = crate::session::daemon::board_card_claim(
            &self.session_home,
            project_id,
            card_id,
            Some(&claim),
            None,
            &gui_command_id("claim"),
        );
        self.refresh_board_summary(project_id);
        self.toast("Started an agent on this to-do");
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
                    self.notify_attention(project_id, item.request_id, item.kind, item.reason);
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
                            project_id,
                            &notified,
                            already_outstanding,
                            request_id,
                            *attention_kind,
                            reason,
                        )
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
                        self.notify_attention(project_id, item.request_id, item.kind, item.reason);
                    }
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

    fn notify_attention(
        &self,
        project_id: i64,
        request_id: String,
        kind: crate::session::activity::AttentionKind,
        reason: String,
    ) {
        if !self
            .notified_attention
            .borrow_mut()
            .insert((project_id, request_id.clone()))
        {
            return;
        }
        let Some(application) = self.window.application() else {
            return;
        };
        let project = self
            .projects
            .borrow()
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.name.clone())
            .unwrap_or_else(|| "Project".to_string());
        let kind = match kind {
            crate::session::activity::AttentionKind::Question => "Question",
            crate::session::activity::AttentionKind::Approval => "Approval",
            crate::session::activity::AttentionKind::Failure => "Failure",
            crate::session::activity::AttentionKind::Review => "Review",
        };
        let notification = gio::Notification::new(&format!("{kind} · {project}"));
        notification.set_body(Some(&reason));
        notification.set_priority(gio::NotificationPriority::High);
        let target = project_id.to_variant();
        notification.set_default_action_and_target_value("app.open-project", Some(&target));
        notification.add_button_with_target_value(
            "Open Project",
            "app.open-project",
            Some(&target),
        );
        application.send_notification(
            Some(&format!("attention-{project_id}-{request_id}")),
            &notification,
        );
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

    /// Which sidebar session (by id) the project's agent panel is showing:
    /// the panel's visible tab, when that tab is an agent tab — named by
    /// the exact conversation it was launched on, else by the live
    /// session whose stable place that tab is.
    fn active_panel_session_id(&self, project_id: i64) -> Option<String> {
        let workspace = self.workspaces.borrow().get(&project_id).cloned()?;
        let panel = Self::agent_panel(&workspace)?;
        let key = panel.active_key()?;
        if key.slot != Slot::Agent {
            // The panel's stack is showing another kind — no agent session.
            return None;
        }
        let primitive = workspace.tab(key)?;
        let launched = primitive.launched_session.borrow().clone();
        let tab = key.as_str();
        let sessions = self
            .agent_sessions
            .borrow()
            .by_project
            .get(&project_id)
            .cloned()
            .unwrap_or_default();
        live_agents::active_panel_session(&sessions, &tab, launched.as_deref())
            .map(|session| session.id.clone())
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
        if !self.home_shown.get() {
            return;
        }
        // The Add-a-project picker is its own stack page and keeps its own
        // state (search text, scroll); refresh it by staying on it.
        if matches!(self.home_nav.borrow().last(), Some(HomeView::AddProject)) {
            self.stack.set_visible_child_name("_add");
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
        // Clear focus before destroying the view so GTK holds no stale widget.
        gtk::prelude::GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
        while let Some(child) = self.stack.child_by_name("_home") {
            self.stack.remove(&child);
        }
        self.stack.add_named(&home::view(self), Some("_home"));
        self.stack.set_visible_child_name("_home");
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
        let Some(project) = self
            .db
            .project(id)
            .ok()
            .flatten()
            .or_else(|| self.projects.borrow().iter().find(|p| p.id == id).cloned())
        else {
            return;
        };
        *self.current.borrow_mut() = Some(id);
        self.home_shown.set(false);
        self.workspace_for(&project);
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
        self.refresh_menus();
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

    fn workspace_for(&self, project: &Project) -> Rc<Workspace> {
        if let Some(existing) = self.workspaces.borrow().get(&project.id) {
            return existing.clone();
        }

        let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        holder.set_vexpand(true);
        holder.set_hexpand(true);

        let workspace = Rc::new(Workspace {
            project: project.clone(),
            tabs: RefCell::new(HashMap::new()),
            programs: RefCell::new(HashMap::new()),
            groups: RefCell::new(Vec::new()),
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
            if saved_state.is_none() {
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

        // Create each visible primitive before rebuilding the groups that refer
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
        let mut groups = Vec::with_capacity(restore_plan.groups.len());
        for saved_group in &restore_plan.groups {
            let keys: Vec<TabKey> = saved_group
                .slots
                .iter()
                .copied()
                .filter(|key| workspace.tab(*key).is_some())
                .collect();
            if keys.is_empty() {
                continue;
            }
            let group = Group::new();
            for key in &keys {
                if let Some(primitive) = workspace.tab(*key) {
                    group.insert(*key, &primitive.widget, false);
                }
            }
            if keys.contains(&saved_group.active) {
                group.activate(saved_group.active);
            }
            group.rebuild_header();
            self.refresh_group_menu(&group);
            groups.push(group);
        }
        *workspace.groups.borrow_mut() = groups.clone();

        if let Some(layout) = restore_plan.layout.as_ref() {
            let layout_groups: Vec<Rc<Group>> = groups
                .iter()
                .filter(|group| {
                    group_id(group)
                        .is_some_and(|id| restore_plan.layout_group_anchors.contains(&id))
                })
                .cloned()
                .collect();
            let restored = restore_layout(layout, &layout_groups);
            if restored
                .as_ref()
                .is_some_and(|tree| layout_covers(tree, &layout_groups))
            {
                *workspace.tree.borrow_mut() = restored;
            }
        }
        if let Some(zoomed) = restore_plan.zoomed {
            if let Some(group) = groups.iter().find(|group| group_id(group) == Some(zoomed)) {
                let tree = workspace.tree.borrow_mut().take();
                *workspace.zoom.borrow_mut() = Some((groups.clone(), tree));
                *workspace.groups.borrow_mut() = vec![group.clone()];
            }
        }
        self.layout(&workspace);
        self.sync_toggles();
        self.persist_primitives(&workspace);
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
        let program = wanted
            .and_then(|id| programs::by_id(&id))
            .filter(|program| program.installed())
            .or_else(|| programs::for_slot(key.slot, &preferences))?;

        workspace
            .programs
            .borrow_mut()
            .insert(key.slot, program.id.clone());
        let mut options = self.launch_options();
        match &resume {
            Resume::No => {}
            Resume::Last => options.resume = true,
            Resume::Session(id) => options.session = Some(id.clone()),
        }
        options.prompt = extras.prompt.clone();
        options.card = extras.card.clone();
        // An agent meets the board at launch: the board skill that makes it
        // the convention is installed before the agent draws its first frame,
        // and the launch claims work under a name unique to this instance —
        // two agents of the same kind never hold each other's cards.
        let mut launch_record: Option<(String, u128)> = None;
        if program.kind == Kind::Agent {
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
            launch_record = Some((
                format!("{}-{}", program.id, stamp),
                crate::programs::launch::now_millis(),
            ));
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
        // When a launched agent's program exits, ask the CLI's own session
        // store which conversation that instance had, and bind it to the
        // claim: clicking the claim later reopens exactly that
        // conversation. The store read takes ~100ms, so it runs on a
        // worker thread and the binding lands back on the main loop.
        if let Some((claim, launched_ms)) = launch_record {
            let (tx, rx) = async_channel::unbounded::<(i64, String, String, String)>();
            let db_for_bindings = self.db.clone();
            glib::MainContext::default().spawn_local(async move {
                while let Ok((project_id, bound_claim, program_id, session)) = rx.recv().await {
                    let _ = db_for_bindings.bind_session(
                        project_id,
                        &bound_claim,
                        &program_id,
                        &session,
                    );
                }
            });
            let tx_for_exit = tx;
            let program_id = program.id.clone();
            let cwd = workspace.project.path.clone();
            let project_id = workspace.project.id;
            let lifecycle_home = self.session_home.clone();
            let lifecycle_session = session_id.clone();
            pane.set_exit_handler(move || {
                publish_session_lifecycle(
                    &lifecycle_home,
                    project_id,
                    &lifecycle_session,
                    "exited",
                );
                let tx = tx_for_exit.clone();
                let program_id = program_id.clone();
                let cwd = cwd.clone();
                let claim = claim.clone();
                let exit_home = lifecycle_home.clone();
                let exit_radar_session = lifecycle_session.clone();
                std::thread::spawn(move || {
                    if let Some(session) =
                        crate::programs::sessions::newest_since(&program_id, &cwd, launched_ms)
                    {
                        let _ =
                            tx.try_send((project_id, claim, program_id.clone(), session.clone()));
                        bind_provider_session(
                            &exit_home,
                            &exit_radar_session,
                            &program_id,
                            &session,
                        );
                    }
                });
            });
        } else {
            let lifecycle_home = self.session_home.clone();
            let lifecycle_session = session_id.clone();
            let project_id = workspace.project.id;
            // Without a claim there is no launch record, but an agent run
            // still earns its exact reopen link: capture the conversation it
            // had the same way, keyed by this session's stable id.
            let capture = program.kind == crate::programs::Kind::Agent;
            let capture_home = lifecycle_home.clone();
            let capture_radar = lifecycle_session.clone();
            let capture_program = program.id.clone();
            let capture_cwd = workspace.project.path.clone();
            // No launch record means no recorded stamp; the pane spawns now,
            // so "created after this instant" is the right capture window.
            let capture_launch = crate::session::catalog::now_millis();
            pane.set_exit_handler(move || {
                publish_session_lifecycle(
                    &lifecycle_home,
                    project_id,
                    &lifecycle_session,
                    "exited",
                );
                if capture {
                    let bind_home = capture_home.clone();
                    let bind_radar = capture_radar.clone();
                    let bind_program = capture_program.clone();
                    let bind_cwd = capture_cwd.clone();
                    std::thread::spawn(move || {
                        let cutoff = u128::try_from(capture_launch.max(0)).unwrap_or(0);
                        if let Some(session) = crate::programs::sessions::newest_since(
                            &bind_program,
                            &bind_cwd,
                            cutoff,
                        ) {
                            bind_provider_session(&bind_home, &bind_radar, &bind_program, &session);
                        }
                    });
                }
            });
        }
        // The pane's header wants the program's live self-description: its
        // name plus whatever it puts in the terminal title, and its exit when
        // it goes away. The window action routes it — the pane outlives any
        // one group, so the observer aims at the action, not at a header.
        // The tab key rides along, so two tabs of one kind report apart.
        let window = self.window.clone();
        let project_id = workspace.project.id;
        let name = program.name.clone();
        pane.set_info_observer(move |text| {
            let info = text.map(|text| format!("{name} · {text}"));
            let _ = gtk::prelude::WidgetExt::activate_action(
                &window,
                "win.pane-info",
                Some(&(project_id, key.as_str(), info.unwrap_or_default()).to_variant()),
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
        let opening = workspace.group_of(key).is_none();
        if let Some(group) = workspace.group_of(key) {
            group.remove(key);
            if group.is_empty() {
                workspace.forget_group(&group);
            }
            group.rebuild_header();
            // A no-op when the pane went away: an empty group has no header.
            self.refresh_group_menu(&group);
        } else {
            let Some(primitive) = self.ensure_primitive(workspace, key, None, Resume::No) else {
                self.toast(&format!(
                    "No {} installed — set one in Preferences",
                    label_for(key.slot).to_lowercase()
                ));
                return;
            };
            let group = Group::new();
            group.insert(key, &primitive.widget, true);
            group.rebuild_header();
            self.refresh_group_menu(&group);
            workspace.push_group(group.clone());
            // In an arranged tree, a brand-new pane lands beside the last one.
            if let Some(node) = workspace.tree.borrow_mut().as_mut() {
                node.append(&group);
            }
        }
        *workspace.zoom.borrow_mut() = None;
        self.layout(workspace);
        self.sync_toggles();
        self.refresh_workspace_menus(workspace);
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

    /// The chip's ＋: another tab of the same primitive, grouped under the
    /// same header as a new chip, running `program`. The next free instance
    /// of the kind takes the new process.
    fn add_tab(&self, workspace: &Rc<Workspace>, source: TabKey, program: &Program) {
        if source.slot == Slot::Custom {
            return;
        }
        let key = workspace.next_key(source.slot);
        let Some(primitive) = self.ensure_primitive(workspace, key, Some(&program.id), Resume::No)
        else {
            return;
        };
        // Beside the tab whose ＋ was pressed; a pane of its own when that
        // tab has since gone.
        match workspace.group_of(source) {
            Some(group) => {
                group.insert(key, &primitive.widget, true);
                group.rebuild_header();
                self.refresh_group_menu(&group);
            }
            None => {
                let group = Group::new();
                group.insert(key, &primitive.widget, true);
                group.rebuild_header();
                self.refresh_group_menu(&group);
                workspace.push_group(group.clone());
                if let Some(node) = workspace.tree.borrow_mut().as_mut() {
                    node.append(&group);
                }
            }
        }
        *workspace.zoom.borrow_mut() = None;
        self.layout(workspace);
        self.sync_toggles();
        self.refresh_workspace_menus(workspace);
        self.persist_primitives(workspace);
        // A new tab is a claim: the keys land in it, ready to type into.
        if let Some(primitive) = workspace.tab(key) {
            primitive.focus();
        }
        self.toast(&format!(
            "New {} tab → {}",
            label_for(source.slot),
            program.name
        ));
    }

    /// Close every tab in a pane while leaving their programs available
    /// to reopen from the dock or the HUD.
    fn close_pane(&self, workspace: &Rc<Workspace>, key: TabKey) {
        self.restore_zoom(workspace);
        let Some(group) = workspace.group_of(key) else {
            return;
        };
        for member in group.tabs() {
            group.remove(member);
        }
        workspace.forget_group(&group);
        group.rebuild_header();
        self.layout(workspace);
        self.sync_toggles();
        self.refresh_workspace_menus(workspace);
        self.persist_primitives(workspace);
    }

    /// Leave zoom mode before changing the visible pane set.
    fn restore_zoom(&self, workspace: &Rc<Workspace>) {
        if let Some((groups, tree)) = workspace.zoom.borrow_mut().take() {
            *workspace.groups.borrow_mut() = groups;
            *workspace.tree.borrow_mut() = tree;
        }
    }

    /// Show a tab without hiding it when it is already on screen.
    fn show_primitive(&self, workspace: &Rc<Workspace>, key: TabKey) {
        if !workspace.is_visible(key) {
            self.toggle_primitive(workspace, key);
        }
    }

    /// Clicking a chip: switch that pane to the tab.
    fn activate_primitive(&self, workspace: &Rc<Workspace>, key: TabKey) {
        let key = workspace.resolve_tab(key);
        if let Some(group) = workspace.group_of(key) {
            group.activate(key);
            self.refresh_group_menu(&group);
            if let Some(primitive) = workspace.tab(key) {
                primitive.focus();
            }
            self.persist_primitives(workspace);
            self.refresh_home();
            return;
        }
        self.show_primitive(workspace, key);
        if let Some(group) = workspace.group_of(key) {
            group.activate(key);
            self.refresh_group_menu(&group);
            if let Some(primitive) = workspace.tab(key) {
                primitive.focus();
            }
            self.persist_primitives(workspace);
            self.refresh_home();
        }
    }

    /// The workspace's one agent panel: the first pane on screen that holds
    /// an Agent tab. Sidebar navigation lands every session here, so a
    /// project shows one agent at a time no matter how many run.
    fn agent_panel(workspace: &Rc<Workspace>) -> Option<Rc<Group>> {
        workspace
            .groups
            .borrow()
            .iter()
            .find(|group| group.tabs().iter().any(|key| key.slot == Slot::Agent))
            .cloned()
    }

    /// Show an open Agent tab the sidebar way: in THE agent panel, alone in
    /// its stack, with the keys in it. A tab living in another pane — dragged
    /// out for a side-by-side, or left there by an older layout — moves back
    /// into the panel; the pane it left keeps its other tabs or goes away.
    /// Hiding the agent that was on screen never stops it: the process and
    /// its pty belong to the session, not the pane.
    fn show_agent_session(&self, workspace: &Rc<Workspace>, key: TabKey) {
        let Some(primitive) = workspace.tab(key) else {
            return;
        };
        self.restore_zoom(workspace);
        let mut moved = false;
        match (Self::agent_panel(workspace), workspace.group_of(key)) {
            // Already home: the stack switch is all that is left to do.
            (Some(panel), Some(home)) if Rc::ptr_eq(&panel, &home) => {}
            (Some(panel), home) => {
                if let Some(home) = home {
                    home.remove(key);
                    if home.is_empty() {
                        workspace.forget_group(&home);
                    }
                    home.rebuild_header();
                    self.refresh_group_menu(&home);
                }
                panel.insert(key, &primitive.widget, true);
                panel.rebuild_header();
                self.refresh_group_menu(&panel);
                moved = true;
            }
            // No agent tab is on screen: the panel comes into being here.
            (None, _) => {
                let group = Group::new();
                group.insert(key, &primitive.widget, true);
                group.rebuild_header();
                self.refresh_group_menu(&group);
                workspace.push_group(group.clone());
                if let Some(node) = workspace.tree.borrow_mut().as_mut() {
                    node.append(&group);
                }
                moved = true;
            }
        }
        if moved {
            *workspace.zoom.borrow_mut() = None;
            self.layout(workspace);
            self.sync_toggles();
            self.refresh_workspace_menus(workspace);
            self.persist_primitives(workspace);
        }
        self.activate_primitive(workspace, key);
    }

    /// Open the session bound to this exact board claim. If neither a live
    /// process stamp nor a stored provider conversation identifies it, leave
    /// the current agent untouched rather than guessing from its program.
    fn open_agent_session(&self, workspace: &Rc<Workspace>, claim: &str) {
        self.open_agent_session_with(workspace, claim, None);
    }

    fn open_agent_session_with(
        &self,
        workspace: &Rc<Workspace>,
        claim: &str,
        prompt: Option<String>,
    ) {
        let extras = Extras {
            prompt,
            card: None,
            instance: None,
        };
        let project_id = workspace.project.id;
        let exact_tab = workspace.tabs_of_kind(Slot::Agent).into_iter().find(|key| {
            workspace
                .tab(*key)
                .and_then(|primitive| primitive.pane.as_ref().and_then(|pane| pane.session_pid()))
                .and_then(programs::launch::radar_agent_of)
                .is_some_and(|agent| agent == claim)
        });
        if let Some(key) = exact_tab {
            self.show_agent_session(workspace, key);
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
                self.show_agent_session(workspace, key);
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
            self.show_agent_session(workspace, key);
        } else {
            self.toast("No agent installed — set one in Preferences");
        }
    }

    /// Open one sidebar session row, whatever kind it is: a live Radar pane,
    /// an external terminal, or a catalog conversation to resume.
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
        let Some(program) = programs::by_id(&session.program_id) else {
            self.toast("The session's agent is not installed");
            return;
        };
        let resume = match live_agents::exact_provider_session_id(&session) {
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
        let keys = workspace.tabs_of_kind(Slot::Agent);
        let key = agent_tab_for_session(
            &keys,
            session
                .radar_session_id
                .as_ref()
                .and_then(|id| parse_stable_session_id(id))
                .filter(|(parsed, _, _)| *parsed == project_id)
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
                    self.show_agent_session(&workspace, key);
                } else {
                    // Dead, or running some other conversation in the
                    // conversation's own place: run it again on the row's
                    // talk. The displaced conversation stays in the CLI's
                    // own session store.
                    self.relaunch_agent(&workspace, key, resume);
                }
            }
            None => {
                if self
                    .ensure_primitive(&workspace, key, Some(&program.id), resume)
                    .is_some()
                {
                    self.show_agent_session(&workspace, key);
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
        self.show_agent_session(&workspace, key);
    }

    /// Run a tab's agent again on `resume`'s conversation, in its own
    /// pane — same widget, same scrollback, fresh claim stamp — and show
    /// it the sidebar way: in THE agent panel. A live instance of a
    /// different conversation is displaced: the click named the
    /// conversation to see, and the displaced one stays in the CLI's own
    /// session store.
    fn relaunch_agent(&self, workspace: &Rc<Workspace>, key: TabKey, resume: Resume) {
        self.relaunch_agent_with(workspace, key, resume, &Extras::default());
    }

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
            Resume::Last => options.resume = true,
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
        #[cfg(feature = "vte")]
        if let Some(pane) = primitive.pane.as_ref() {
            pane.respawn(&spec);
        }
        *primitive.launched_session.borrow_mut() = match &resume {
            Resume::Session(id) => Some(id.clone()),
            Resume::Last | Resume::No => None,
        };
        self.show_agent_session(workspace, key);
    }

    /// Hovering a member selects its content and moves keyboard focus into that
    /// tab, the same focus-follows-pointer behavior as hovering a pane.
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
        let Some(group) = workspace.group_of(key) else {
            return;
        };
        if group.active_key() != Some(key) {
            group.activate(key);
            self.refresh_group_menu(&group);
            self.persist_primitives(workspace);
            self.refresh_home();
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

    /// Drop one tab onto another's header: they share that header.
    fn group_into(&self, workspace: &Rc<Workspace>, source: TabKey, target: TabKey) {
        // Kinds resolve to the tab of that kind that exists — the menu's
        // "Group with Editor" and the dock speak in kinds, chips in keys.
        let source = workspace.resolve_tab(source);
        let target = workspace.resolve_tab(target);
        if source.slot == Slot::Board || target.slot == Slot::Board {
            self.toast("Project tasks live in Home");
            return;
        }
        if source == target || source.slot == Slot::Custom {
            return;
        }
        let Some(primitive) = self.ensure_primitive(workspace, source, None, Resume::No) else {
            return;
        };
        let Some(target_group) = workspace.group_of(target) else {
            return;
        };
        if let Some(source_group) = workspace.group_of(source) {
            if Rc::ptr_eq(&source_group, &target_group) {
                return;
            }
            source_group.remove(source);
            source_group.rebuild_header();
            if source_group.is_empty() {
                workspace.forget_group(&source_group);
            } else {
                self.refresh_group_menu(&source_group);
            }
        }
        target_group.insert(source, &primitive.widget, true);
        target_group.rebuild_header();
        self.refresh_group_menu(&target_group);
        *workspace.zoom.borrow_mut() = None;
        self.layout(workspace);
        self.sync_toggles();
        self.refresh_workspace_menus(workspace);
        self.persist_primitives(workspace);
        trace(&format!(
            "group: {} joined {}",
            source.as_str(),
            target.as_str()
        ));
    }

    /// Pull a tab out of a shared header into its own pane.
    fn split_out(&self, workspace: &Rc<Workspace>, key: TabKey) {
        let key = workspace.resolve_tab(key);
        let Some(group) = workspace.group_of(key) else {
            return;
        };
        if group.tabs().len() < 2 {
            return; // already its own pane
        }
        let Some(primitive) = self.ensure_primitive(workspace, key, None, Resume::No) else {
            return;
        };
        group.remove(key);
        group.rebuild_header();
        self.refresh_group_menu(&group);
        let own = Group::new();
        own.insert(key, &primitive.widget, true);
        own.rebuild_header();
        self.refresh_group_menu(&own);
        workspace.push_group(own.clone());
        // In an arranged tree, the new pane appears beside its old group.
        if let Some(node) = workspace.tree.borrow_mut().as_mut() {
            node.replace(
                &group,
                split::Node::split(
                    split::Axis::Horizontal,
                    0.5,
                    format!("tree-split-{}", key.as_str()),
                    split::Node::leaf(&group),
                    split::Node::leaf(&own),
                ),
            );
        }
        *workspace.zoom.borrow_mut() = None;
        self.layout(workspace);
        self.sync_toggles();
        self.refresh_workspace_menus(workspace);
        self.persist_primitives(workspace);
        trace(&format!("split out: {}", key.as_str()));
    }

    /// Move a pane one place in the visible arrangement while preserving the
    /// existing divider shape and sizes.
    fn move_pane(&self, workspace: &Rc<Workspace>, key: TabKey, delta: isize) {
        let Some(group) = workspace.group_of(key) else {
            return;
        };
        let ordered = self.ordered_groups(workspace);
        let Some(index) = ordered
            .iter()
            .position(|candidate| Rc::ptr_eq(candidate, &group))
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
                *tree = auto_node(&workspace.groups());
            }
            let Some(tree) = tree.as_mut() else {
                return;
            };
            if !tree.swap_leaves(&group, target) {
                return;
            }
        }

        *workspace.zoom.borrow_mut() = None;
        self.layout(workspace);
        self.sync_toggles();
        self.refresh_workspace_menus(workspace);
        self.persist_primitives(workspace);
    }

    /// Drop pane A onto pane B's body: B's region divides in two and A takes
    /// half — a real, nested split, the way a tiling manager does it. The
    /// zone (left/right/top/bottom/center) says which half A takes.
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
        // The dragged tab must lead its own pane before it can take a
        // half of someone else's.
        if let Some(group) = workspace.group_of(dragged) {
            if group.tabs().len() >= 2 {
                self.split_out(workspace, dragged);
            }
        }
        let (Some(dragged_group), Some(target_group)) =
            (workspace.group_of(dragged), workspace.group_of(target))
        else {
            return;
        };
        if Rc::ptr_eq(&dragged_group, &target_group) {
            return;
        }
        let (axis, dragged_first) = match zone {
            "left" => (split::Axis::Horizontal, true),
            "top" => (split::Axis::Vertical, true),
            "bottom" => (split::Axis::Vertical, false),
            // right and centre: the dragged pane takes the right side, the
            // way a tiling manager splits by default.
            _ => (split::Axis::Horizontal, false),
        };
        // Plant the arrangement tree from the auto layout if this is the
        // first manual split; from then on the tree is the law.
        let planted = {
            let existing = workspace.tree.borrow_mut().take();
            // The dragged pane is about to take a half of the target's
            // region. Lift it out of wherever it sits first — the leaf a
            // split-out just planted beside its old group, or its place in
            // the auto arrangement — so it lands in the tree exactly once.
            // A group in two leaves would try to parent one widget into two
            // panes, and the loser half stays empty.
            let others: Vec<Rc<Group>> = workspace
                .groups()
                .into_iter()
                .filter(|group| !Rc::ptr_eq(group, &dragged_group))
                .collect();
            let mut tree = match existing {
                Some(tree) => tree.take_leaf(&dragged_group),
                None => auto_node(&others),
            }
            .or_else(|| auto_node(&others));
            let (first, second) = if dragged_first {
                (
                    split::Node::leaf(&dragged_group),
                    split::Node::leaf(&target_group),
                )
            } else {
                (
                    split::Node::leaf(&target_group),
                    split::Node::leaf(&dragged_group),
                )
            };
            let key = format!("tree-{}-{}", dragged.as_str(), target.as_str());
            let ok = match tree.as_mut() {
                Some(tree) => tree.replace(
                    &target_group,
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
        self.refresh_workspace_menus(workspace);
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
            if let Some(group) = workspace.group_of(key) {
                group.remove(key);
                if group.is_empty() {
                    workspace.forget_group(&group);
                }
                group.rebuild_header();
                // The group may have other members left; their chip controls
                // need their models back after the rebuild.
                self.refresh_group_menu(&group);
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
            self.refresh_workspace_menus(workspace);
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

    /// Arrange the panes: agent on the left, changes and editor stacked beside
    /// it, commands along the bottom. Whatever is not open is not there.
    fn layout(&self, workspace: &Rc<Workspace>) {
        workspace.dividers.borrow_mut().clear();
        let groups = workspace.groups();

        for group in &groups {
            group.widget.unparent();
        }
        while let Some(child) = workspace.holder.first_child() {
            workspace.holder.remove(&child);
        }

        if groups.is_empty() {
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
            .and_then(|tree| tree.prune(&workspace.groups()));
        if let Some(node @ split::Node::Split { .. }) = pruned {
            *workspace.tree.borrow_mut() = Some(node);
        }

        let groups = workspace.groups();

        for group in &groups {
            group.widget.unparent();
        }
        while let Some(child) = workspace.holder.first_child() {
            workspace.holder.remove(&child);
        }

        if groups.is_empty() {
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
            None => match auto_node(&groups) {
                Some(tree) => self.build_tree(workspace, &tree),
                None => return,
            },
        };
        workspace.holder.append(&root);
        trace(&format!(
            "layout: {} pane(s) {:?}",
            groups.len(),
            groups
                .iter()
                .map(|group| group
                    .tabs()
                    .iter()
                    .map(|key| key.as_str())
                    .collect::<Vec<_>>()
                    .join("+"))
                .collect::<Vec<_>>()
        ));
    }

    /// Turn an arrangement node into widgets.
    fn build_tree(&self, workspace: &Rc<Workspace>, node: &split::Node<Group>) -> gtk::Widget {
        match node {
            split::Node::Leaf(group) => group.widget.clone().upcast::<gtk::Widget>(),
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

        let extent = if orientation == gtk::Orientation::Horizontal {
            self.window.width().max(700)
        } else {
            self.window.height().max(500)
        };
        paned.set_position(
            workspace
                .positions
                .borrow()
                .get(key)
                .copied()
                .unwrap_or((extent as f64 * fraction) as i32),
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

        let resize_keys = gtk::EventControllerKey::new();
        resize_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let paned_for_keys = paned.clone();
        let window_for_keys = self.window.clone();
        resize_keys.connect_key_pressed(move |_, key, _, _| {
            let focused = window_for_keys
                .focus_widget()
                .is_some_and(|focus| focus == paned_for_keys.clone().upcast::<gtk::Widget>());
            if !focused {
                return glib::Propagation::Proceed;
            }
            let delta = match (orientation, key) {
                (gtk::Orientation::Horizontal, gtk::gdk::Key::Left)
                | (gtk::Orientation::Vertical, gtk::gdk::Key::Up) => -16,
                (gtk::Orientation::Horizontal, gtk::gdk::Key::Right)
                | (gtk::Orientation::Vertical, gtk::gdk::Key::Down) => 16,
                _ => return glib::Propagation::Proceed,
            };
            paned_for_keys.set_position(paned_for_keys.position().saturating_add(delta));
            glib::Propagation::Stop
        });
        paned.add_controller(resize_keys);
        workspace.dividers.borrow_mut().push(paned.clone());
        paned.upcast()
    }

    /// Zoom the focused pane to the whole window, and back.
    fn toggle_zoom(&self, key: Option<TabKey>) {
        let Some(workspace) = self.current_workspace() else {
            return;
        };
        let previous_zoom = workspace.zoom.borrow_mut().take();
        if let Some((groups, tree)) = previous_zoom {
            *workspace.groups.borrow_mut() = groups;
            *workspace.tree.borrow_mut() = tree;
            self.layout(&workspace);
            self.sync_toggles();
            self.refresh_workspace_menus(&workspace);
            self.persist_primitives(&workspace);
            return;
        }
        let groups = workspace.groups();
        if groups.len() < 2 {
            self.toast("Only one pane is showing");
            return;
        }
        let target = key
            .and_then(|key| workspace.group_of(key))
            .or_else(|| self.focused_group(&workspace))
            .unwrap_or_else(|| groups[0].clone());
        // The arrangement waits in the zoom slot while the single pane shows.
        let saved_tree = workspace.tree.borrow_mut().take();
        *workspace.zoom.borrow_mut() = Some((groups.clone(), saved_tree));
        *workspace.groups.borrow_mut() = vec![target];
        self.layout(&workspace);
        self.sync_toggles();
        self.refresh_workspace_menus(&workspace);
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
        if let Some(project) = self.current_project() {
            self.workspace_title.set_label(&project.name);
            if let Some(label) = self.workspace_title.child().and_downcast::<gtk::Label>() {
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                label.set_max_width_chars(32);
            }
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

    /// The pane menu belongs to whichever tab its header is showing — and so
    /// do the chips' program dropdowns and ＋ menus, which is why every
    /// header rebuild comes back here.
    fn refresh_group_menu(&self, group: &Rc<Group>) {
        let Some(key) = group.active_key() else {
            return;
        };
        let Some(workspace) = self.current_workspace() else {
            return;
        };
        group
            .menu_button
            .set_menu_model(Some(&self.primitive_menu_model(&workspace, key)));
        for (member, program_button, add_button) in group.chip_controls() {
            program_button.set_menu_model(Some(&self.chip_program_menu(&workspace, member)));
            add_button.set_menu_model(Some(&self.chip_add_menu(member)));
        }
        // A pane rebuilt into a new group starts with a blank header; restore
        // whatever each member's program has said so far.
        for workspace in self.workspaces.borrow().values() {
            for member in group.tabs() {
                let home = workspace.group_of(member);
                if home.is_some_and(|home| Rc::ptr_eq(&home, group)) {
                    if let Some(text) = self
                        .header_info
                        .borrow()
                        .get(&(workspace.project.id, member))
                    {
                        group.set_member_info(member, text);
                    }
                }
            }
        }
    }

    /// Remember what a pane's program said and aim it at the pane's header,
    /// wherever that pane is grouped today. `None` clears.
    fn store_header_info(&self, project_id: i64, key: TabKey, text: Option<String>) {
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
        let shown = text.unwrap_or_default();
        for workspace in self.workspaces.borrow().values() {
            if workspace.project.id != project_id {
                continue;
            }
            if let Some(group) = workspace.group_of(key) {
                group.set_member_info(key, &shown);
            }
        }
    }

    /// Rebuild every pane menu, after a preference change.
    fn refresh_menus(&self) {
        if let Some(workspace) = self.current_workspace() {
            self.refresh_workspace_menus(&workspace);
        }
    }

    fn refresh_workspace_menus(&self, workspace: &Workspace) {
        for group in workspace.groups() {
            self.refresh_group_menu(&group);
        }
    }
}

/// Save one project's tabs and presentation state to SQLite.
fn persist_workspace(db: &Db, workspace: &Workspace) {
    let zoom = workspace.zoom.borrow();
    let (groups, tree) = match zoom.as_ref() {
        Some((groups, tree)) => (groups.clone(), tree.clone()),
        None => (workspace.groups(), workspace.tree.borrow().clone()),
    };
    let zoomed = zoom
        .as_ref()
        .and_then(|_| workspace.groups().first().and_then(group_id));

    // One row per tab, same-primitive tabs included: the second agent tab is
    // a second `agent` row, and its key comes back the same way on restore.
    let mut keys: Vec<TabKey> = groups.iter().flat_map(|group| group.tabs()).collect();
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

    let saved_groups = groups
        .iter()
        .filter_map(|group| {
            let tabs = group.tabs();
            Some(WorkspaceGroup {
                active: group.active_key()?,
                slots: tabs,
            })
        })
        .collect();
    let state = WorkspaceState {
        groups: saved_groups,
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

/// The kind a pane leads with — auto layout places panes by kind, however
/// many instances a kind has grown.
fn anchor_kind(group: &Rc<Group>) -> Option<Slot> {
    Workspace::anchor(group).map(|key| key.slot)
}

fn group_id(group: &Rc<Group>) -> Option<TabKey> {
    group.tabs().first().copied()
}

fn save_layout(node: &split::Node<Group>) -> Option<WorkspaceLayout> {
    match node {
        split::Node::Leaf(group) => Some(WorkspaceLayout::Pane {
            group: group_id(group)?,
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

fn restore_layout(node: &WorkspaceLayout, groups: &[Rc<Group>]) -> Option<split::Node<Group>> {
    let anchors: Vec<TabKey> = groups.iter().filter_map(group_id).collect();
    restore_layout_keys(node, &anchors)?;
    restore_layout_nodes(node, groups)
}

fn restore_layout_nodes(
    node: &WorkspaceLayout,
    groups: &[Rc<Group>],
) -> Option<split::Node<Group>> {
    match node {
        WorkspaceLayout::Pane { group } => groups
            .iter()
            .find(|candidate| group_id(candidate) == Some(*group))
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
            restore_layout_nodes(first, groups)?,
            restore_layout_nodes(second, groups)?,
        )),
    }
}

fn restore_layout_keys(node: &WorkspaceLayout, groups: &[TabKey]) -> Option<Vec<TabKey>> {
    fn collect(node: &WorkspaceLayout, groups: &[TabKey], leaves: &mut Vec<TabKey>) -> Option<()> {
        match node {
            WorkspaceLayout::Pane { group } if groups.contains(group) => {
                leaves.push(*group);
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
                collect(first, groups, leaves)?;
                collect(second, groups, leaves)
            }
        }
    }

    let mut leaves = Vec::new();
    collect(node, groups, &mut leaves)?;
    if leaves.len() != groups.len()
        || groups
            .iter()
            .any(|group| leaves.iter().filter(|leaf| **leaf == *group).count() != 1)
    {
        return None;
    }
    Some(leaves)
}

struct WorkspaceRestorePlan {
    groups: Vec<WorkspaceGroup>,
    layout: Option<WorkspaceLayout>,
    layout_group_anchors: Vec<TabKey>,
    zoomed: Option<TabKey>,
}

fn saved_board_open(state: &WorkspaceState) -> bool {
    let board = TabKey::first(Slot::Board);
    state.board_open
        || state.zoomed == Some(board)
        || state
            .groups
            .iter()
            .any(|group| group.slots.contains(&board))
}

fn strip_board_layout(node: &WorkspaceLayout, board: TabKey) -> Option<WorkspaceLayout> {
    match node {
        WorkspaceLayout::Pane { group } if *group != board => Some(node.clone()),
        WorkspaceLayout::Pane { .. } => None,
        WorkspaceLayout::Split {
            axis,
            ratio,
            key,
            first,
            second,
        } => match (
            strip_board_layout(first, board),
            strip_board_layout(second, board),
        ) {
            (Some(first), Some(second)) => Some(WorkspaceLayout::Split {
                axis: *axis,
                ratio: *ratio,
                key: key.clone(),
                first: Box::new(first),
                second: Box::new(second),
            }),
            (Some(remaining), None) | (None, Some(remaining)) => Some(remaining),
            (None, None) => None,
        },
    }
}

fn workspace_restore_plan(
    state: Option<&WorkspaceState>,
    wanted: &[TabKey],
) -> WorkspaceRestorePlan {
    let mut groups = Vec::new();
    let mut assigned = HashSet::new();
    if let Some(state) = state {
        for saved_group in &state.groups {
            let slots: Vec<TabKey> = saved_group
                .slots
                .iter()
                .copied()
                .filter(|key| {
                    key.slot != Slot::Board && wanted.contains(key) && assigned.insert(*key)
                })
                .collect();
            if slots.is_empty() {
                continue;
            }
            let active = if slots.contains(&saved_group.active) {
                saved_group.active
            } else {
                slots[0]
            };
            groups.push(WorkspaceGroup { slots, active });
        }
    }
    for key in wanted.iter().filter(|key| key.slot != Slot::Board) {
        if assigned.insert(*key) {
            groups.push(WorkspaceGroup {
                slots: vec![*key],
                active: *key,
            });
        }
    }

    let board = TabKey::first(Slot::Board);
    let board_open = state.is_some_and(saved_board_open);
    let zoomed = state
        .and_then(|state| state.zoomed)
        .filter(|key| *key != board)
        .filter(|key| groups.len() > 1 && groups.iter().any(|group| group.slots.contains(key)));
    let layout_group_anchors = groups
        .iter()
        .filter(|group| !board_open || !group.slots.contains(&board))
        .filter_map(|group| group.slots.first().copied())
        .collect();
    let layout = state
        .and_then(|state| state.layout.as_ref())
        .and_then(|layout| {
            if board_open {
                strip_board_layout(layout, board)
            } else {
                Some(layout.clone())
            }
        });

    WorkspaceRestorePlan {
        groups,
        layout,
        layout_group_anchors,
        zoomed,
    }
}

fn layout_covers(node: &split::Node<Group>, groups: &[Rc<Group>]) -> bool {
    let leaves = node.leaves();
    leaves.len() == groups.len()
        && groups
            .iter()
            .all(|group| leaves.iter().any(|leaf| Rc::ptr_eq(group, leaf)))
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
    if let Some(key) = stable_key {
        return key;
    }
    keys.iter()
        .copied()
        .find(|key| !is_live(*key))
        .unwrap_or(next_key)
}

/// The classic arrangement as a tree: agent-anchored main pane on the left,
/// the rest stacked on the side, shell panes along the bottom. Mirrors the
/// ratios and divider keys this used to build directly, so saved divider
/// positions keep working in auto mode.
fn auto_node(groups: &[Rc<Group>]) -> Option<split::Node<Group>> {
    let bottom: Vec<Rc<Group>> = groups
        .iter()
        .filter(|group| anchor_kind(group) == Some(Slot::Shell))
        .cloned()
        .collect();
    let rest: Vec<Rc<Group>> = groups
        .iter()
        .filter(|group| !bottom.iter().any(|other| Rc::ptr_eq(other, group)))
        .cloned()
        .collect();
    let main_left = rest
        .iter()
        .find(|group| anchor_kind(group) == Some(Slot::Agent))
        .cloned()
        .or_else(|| rest.first().cloned());
    let side: Vec<Rc<Group>> = rest
        .iter()
        .filter(|group| match &main_left {
            Some(main) => !Rc::ptr_eq(main, group),
            None => true,
        })
        .cloned()
        .collect();

    let side_node = fold_nodes(&side, split::Axis::Vertical, 0.5, "side");
    let main = match (&main_left, side_node) {
        (Some(main), Some(side)) => Some(split::Node::split(
            split::Axis::Horizontal,
            0.42,
            "main",
            split::Node::leaf(main),
            side,
        )),
        (Some(main), None) => Some(split::Node::leaf(main)),
        (None, Some(side)) => Some(side),
        (None, None) => None,
    };
    let bottom_node = fold_nodes(&bottom, split::Axis::Horizontal, 0.55, "bottom");
    match (main, bottom_node) {
        (Some(main), Some(bottom)) => Some(split::Node::split(
            split::Axis::Vertical,
            0.68,
            "outer",
            main,
            bottom,
        )),
        (Some(main), None) => Some(main),
        (None, Some(bottom)) => Some(bottom),
        (None, None) => None,
    }
}

/// Fold panes into nested splits along one axis, from the back, alternating
/// sides — the same shape the widget fold built, so divider keys line up.
fn fold_nodes(
    groups: &[Rc<Group>],
    axis: split::Axis,
    ratio: f64,
    key: &str,
) -> Option<split::Node<Group>> {
    let mut iter = groups.iter().rev();
    let mut acc = split::Node::leaf(iter.next()?);
    for (index, group) in iter.enumerate() {
        acc = if index % 2 == 0 {
            split::Node::split(
                axis,
                ratio,
                format!("{key}{index}"),
                split::Node::leaf(group),
                acc,
            )
        } else {
            split::Node::split(
                axis,
                ratio,
                format!("{key}{index}"),
                acc,
                split::Node::leaf(group),
            )
        };
    }
    Some(acc)
}

/// Dropping a pane on the project list pulls that primitive into a pane of its
/// own: the natural counter-gesture to dropping it onto another pane.
fn wire_workspace_drop(app: &SharedApp) {
    let target = gtk::DropTarget::new(glib::types::Type::STRING, gtk::gdk::DragAction::MOVE);
    target.set_propagation_phase(gtk::PropagationPhase::Capture);
    let bar = app.workspace_bar.clone();
    target.connect_drop(move |_, value, _, _| {
        let Ok(payload) = value.get::<String>() else {
            return false;
        };
        trace(&format!("drop: workspace toolbar got payload={payload}"));
        // Deferred one main-loop turn so the relayout happens after the drag
        // has fully finished — see the note in group.rs's drop handler.
        let bar = bar.clone();
        let variant = payload.to_variant();
        glib::idle_add_local_once(move || {
            let _ = bar.activate_action("win.primitive-split-out", Some(&variant));
        });
        true
    });
    app.workspace_bar
        .clone()
        .upcast::<gtk::Widget>()
        .add_controller(target);
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
        let paths = Rc::new(Paths::with_root(scratch.path().join("state")));
        paths.ensure().unwrap();
        let db = Rc::new(Db::open(&paths).unwrap());
        let _daemon = TestDaemon(
            std::process::Command::new(
                std::env::var("RADAR_TEST_BIN").expect("run scripts/home-smoke.sh"),
            )
            .arg("--home")
            .arg(&paths.data_dir)
            .arg("serve")
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
        assert!(created.path.join(".git").is_dir());
        assert!(
            db.tabs(created.id).unwrap().is_empty(),
            "creation stays in the human project view"
        );
        assert!(widgets(&stack.child_by_name("_home").unwrap())
            .iter()
            .any(|widget| widget.has_css_class("project-view")));
        window.destroy();
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
    fn retired_board_restores_tool_tree_without_board_group() {
        let agent = TabKey::first(Slot::Agent);
        let diff = TabKey::first(Slot::Diff);
        let board = TabKey::first(Slot::Board);
        let state = WorkspaceState {
            groups: vec![
                WorkspaceGroup {
                    slots: vec![agent],
                    active: agent,
                },
                WorkspaceGroup {
                    slots: vec![diff],
                    active: diff,
                },
            ],
            layout: Some(WorkspaceLayout::Split {
                axis: WorkspaceAxis::Horizontal,
                ratio: 0.5,
                key: "manual".into(),
                first: Box::new(WorkspaceLayout::Pane { group: agent }),
                second: Box::new(WorkspaceLayout::Pane { group: diff }),
            }),
            board_open: true,
            ..WorkspaceState::default()
        };

        let plan = workspace_restore_plan(Some(&state), &[agent, diff, board]);
        assert_eq!(plan.zoomed, None);
        assert_eq!(plan.layout_group_anchors, vec![agent, diff]);
        assert_eq!(
            plan.groups
                .iter()
                .map(|group| group.slots[0])
                .collect::<Vec<_>>(),
            vec![agent, diff]
        );

        let layout = plan.layout.as_ref().unwrap();
        let restored_tree = restore_layout_keys(layout, &plan.layout_group_anchors).unwrap();
        assert_eq!(restored_tree, vec![agent, diff]);

        let tools_after_dismissal: Vec<TabKey> = plan
            .groups
            .iter()
            .filter(|group| !group.slots.contains(&board))
            .filter_map(|group| group.slots.first().copied())
            .collect();
        assert_eq!(tools_after_dismissal, restored_tree);

        let tree_with_board = WorkspaceLayout::Split {
            axis: WorkspaceAxis::Horizontal,
            ratio: 0.5,
            key: "invalid".into(),
            first: Box::new(WorkspaceLayout::Pane { group: agent }),
            second: Box::new(WorkspaceLayout::Pane { group: board }),
        };
        assert!(restore_layout_keys(&tree_with_board, &plan.layout_group_anchors).is_none());
    }

    #[test]
    fn legacy_board_zoom_state_restores_only_tool_groups() {
        let agent = TabKey::first(Slot::Agent);
        let diff = TabKey::first(Slot::Diff);
        let board = TabKey::first(Slot::Board);
        let state = WorkspaceState {
            groups: vec![
                WorkspaceGroup {
                    slots: vec![agent],
                    active: agent,
                },
                WorkspaceGroup {
                    slots: vec![diff],
                    active: diff,
                },
                WorkspaceGroup {
                    slots: vec![board],
                    active: board,
                },
            ],
            layout: Some(WorkspaceLayout::Split {
                axis: WorkspaceAxis::Horizontal,
                ratio: 0.5,
                key: "outer".into(),
                first: Box::new(WorkspaceLayout::Pane { group: agent }),
                second: Box::new(WorkspaceLayout::Split {
                    axis: WorkspaceAxis::Vertical,
                    ratio: 0.5,
                    key: "inner".into(),
                    first: Box::new(WorkspaceLayout::Pane { group: diff }),
                    second: Box::new(WorkspaceLayout::Pane { group: board }),
                }),
            }),
            zoomed: Some(board),
            ..WorkspaceState::default()
        };

        let plan = workspace_restore_plan(Some(&state), &[agent, diff, board]);
        assert_eq!(plan.zoomed, None);
        assert_eq!(plan.layout_group_anchors, vec![agent, diff]);
        assert_eq!(
            restore_layout_keys(plan.layout.as_ref().unwrap(), &plan.layout_group_anchors),
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
mod agent_panel_tests {
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
