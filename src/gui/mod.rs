//! The app window.
//!
//! A project workspace is a handful of primitives — editor, agent, diff,
//! terminal, the board — and nothing else. No tabs: the sidebar's icons decide
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
mod dialogs;
mod group;
mod home;
mod hud;
mod keynav;
mod live_agents;
mod pane;
mod primitive;
mod split;
mod style;
mod theme;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
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
const PRIMITIVES: [Slot; 5] = [
    Slot::Agent,
    Slot::Diff,
    Slot::Board,
    Slot::Shell,
    Slot::Editor,
];

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
    sidebar: gtk::Widget,
    splitter: gtk::Paned,
    sidebar_shown: Cell<bool>,
    sidebar_list: gtk::ListBox,
    sidebar_search: gtk::SearchEntry,
    toggles: RefCell<HashMap<Slot, gtk::ToggleButton>>,
    projects_toggle: gtk::ToggleButton,
    /// Home leads the dock; it is checked while the home panel shows.
    home_toggle: gtk::ToggleButton,
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
    rows: RefCell<Vec<ProjectRow>>,
    agent_sessions: RefCell<HashMap<i64, Vec<live_agents::AgentSession>>>,
    agent_tx: std::sync::mpsc::Sender<Vec<live_agents::AgentSession>>,
    agent_rx: RefCell<std::sync::mpsc::Receiver<Vec<live_agents::AgentSession>>>,
    agent_scan_pending: Cell<bool>,
    expanded_projects: RefCell<HashSet<i64>>,
    /// Projects whose agent list currently shows archived sessions.
    archived_views: RefCell<HashSet<i64>>,
    status: RefCell<HashMap<i64, crate::git::Status>>,
    status_tx: std::sync::mpsc::Sender<Vec<(i64, crate::git::Status)>>,
    status_rx: RefCell<std::sync::mpsc::Receiver<Vec<(i64, crate::git::Status)>>>,
    activity: RefCell<HashMap<i64, ProjectActivity>>,
    activity_online: RefCell<HashMap<i64, bool>>,
    activity_watchers: RefCell<HashMap<i64, ActivityWatcher>>,
    activity_tx: std::sync::mpsc::SyncSender<ActivityNotice>,
    activity_rx: RefCell<std::sync::mpsc::Receiver<ActivityNotice>>,
    board_panes: RefCell<HashMap<i64, std::rc::Weak<board::BoardPane>>>,
    /// What each pane's program last said about itself — its name and its own
    /// live title, or its exit — keyed by (project, tab).
    header_info: RefCell<HashMap<(i64, TabKey), String>>,
    current: RefCell<Option<i64>>,
    // The search box doubles as the add flow: candidates for the query show
    // under the projects, each with its own add button.
    find_root: RefCell<PathBuf>,
    find_candidates: RefCell<Vec<Candidate>>,
    find_rows: RefCell<Vec<gtk::ListBoxRow>>,
    find_tx: std::sync::mpsc::Sender<(PathBuf, Vec<Candidate>)>,
    find_rx: RefCell<std::sync::mpsc::Receiver<(PathBuf, Vec<Candidate>)>>,
    find_root_button: gtk::Button,
    /// Last real pointer movement over the window, in milliseconds of the
    /// glib monotonic clock. Enter events a mapped widget synthesizes under a
    /// parked pointer must not read as mouse intent.
    pointer_motion_ms: Cell<i64>,
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
struct ProjectRow {
    id: i64,
    row: gtk::ListBoxRow,
    summary: gtk::Label,
    badge: gtk::Label,
    attention_badge: gtk::Label,
    agents: gtk::Box,
    agent_toggle: gtk::ToggleButton,
}

/// How an agent tab's program starts: fresh, on the project's last
/// conversation, or on one exact stored conversation (a board claim's
/// bound session).
enum Resume {
    No,
    Last,
    Session(String),
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

/// A compact age for sidebar subtitles: coarse, monotonic, no clock-format
/// churn. Beyond a week the exact day matters less than the order.
fn relative_time(millis: i64) -> String {
    let now = crate::session::catalog::now_millis();
    let seconds = (now.saturating_sub(millis)).max(0) / 1000;
    match seconds {
        0..=59 => "now".to_string(),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        86_400..=604_799 => format!("{}d", seconds / 86_400),
        604_800..=2_591_999 => format!("{}w", seconds / 604_800),
        _ => format!("{}mo", seconds / 2_592_000),
    }
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

    // ---- sidebar ----
    let sidebar_list = gtk::ListBox::new();
    sidebar_list.set_selection_mode(gtk::SelectionMode::Single);
    // No `navigation-sidebar`: its own row padding and radii would fight the
    // stylesheet. This sidebar styles its rows itself.
    sidebar_list.set_show_separators(false);

    let sidebar_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&sidebar_list)
        .build();

    // The dock: one toggle per primitive, along the bottom of the sidebar.
    // It spans the sidebar's full inset width like the search above, so all
    // three floating surfaces read as one family.
    let toggles = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    toggles.add_css_class("dock");
    toggles.set_halign(gtk::Align::Fill);
    toggles.set_margin_top(6);
    toggles.set_margin_bottom(8);

    // Home leads the dock: the panel the dock returns to when nothing is
    // open, and the empty state itself when there are no projects yet.
    let home_toggle = gtk::ToggleButton::builder()
        .icon_name("go-home-symbolic")
        .tooltip_text("Home\tAlt+Home")
        .build();
    home_toggle.add_css_class("flat");
    home_toggle.set_hexpand(true);
    home_toggle.set_action_name(Some("win.show-home"));
    if let Some(image) = home_toggle.child().and_downcast::<gtk::Image>() {
        image.set_pixel_size(16);
    }
    toggles.append(&home_toggle);

    // One toggle per primitive, in the order they are named: agent, changes,
    // project, editor, commands. The project toggle is the sidebar.
    let mut toggle_buttons = HashMap::new();
    let mut add_toggle = |slot: Slot, project: bool, row: &gtk::Box| {
        let button = gtk::ToggleButton::builder()
            .icon_name(if project {
                primitive::PROJECTS_ICON
            } else {
                icon_name(slot)
            })
            .tooltip_text(if project {
                format!("{}\tAlt+B", primitive::PROJECTS_LABEL)
            } else {
                format!("{}\t{}", label_for(slot), accel_hint(slot))
            })
            .build();
        button.add_css_class("flat");
        button.set_hexpand(true);
        // Smaller icon than the default: the dock is a compact control strip.
        if let Some(image) = button.child().and_downcast::<gtk::Image>() {
            image.set_pixel_size(16);
        }
        if project {
            button.set_action_name(Some("win.toggle-sidebar"));
        } else {
            button.set_action_name(Some("win.primitive-toggle"));
            button.set_action_target_value(Some(&slot.as_str().to_variant()));
            toggle_buttons.insert(slot, button.clone());
        }
        row.append(&button);
        button
    };
    let mut projects_toggle: Option<gtk::ToggleButton> = None;
    for (slot, is_project) in [
        (Slot::Agent, false),
        (Slot::Diff, false),
        (Slot::Board, false),
        (Slot::Custom, true),
        (Slot::Editor, false),
        (Slot::Shell, false),
    ] {
        let button = add_toggle(slot, is_project, &toggles);
        if is_project {
            projects_toggle = Some(button);
        }
    }
    let projects_toggle = projects_toggle.expect("the project toggle is in the row");

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search projects…"));
    search.set_tooltip_text(Some(
        "Filter your projects, or type to find a directory to add",
    ));
    search.set_hexpand(true);

    // Brand header: the app logo, not a pane header. No menu button.
    let sidebar_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    sidebar_header.add_css_class("group-header");
    let header_icon = gtk::Image::from_icon_name("radar");
    header_icon.set_pixel_size(20);
    header_icon.set_tooltip_text(Some("Radar"));
    sidebar_header.append(&header_icon);
    let sidebar_title = gtk::Label::new(Some("Radar"));
    sidebar_title.add_css_class("caption-heading");
    sidebar_title.set_xalign(0.0);
    sidebar_title.set_hexpand(true);
    sidebar_header.append(&sidebar_title);
    // Home, by the logo: back to the panel that is there when nothing is.
    let home_button = gtk::Button::builder()
        .icon_name("go-home-symbolic")
        .tooltip_text("Home\tAlt+Home")
        .build();
    home_button.add_css_class("flat");
    home_button.set_action_name(Some("win.show-home"));
    sidebar_header.append(&home_button);

    // The search goes straight into the sidebar box; its margins come from the
    // stylesheet, aligned with the row inset.

    // Shown only while a search is up: where the directory results come from.
    let find_root_button = gtk::Button::new();
    find_root_button.add_css_class("flat");
    find_root_button.add_css_class("caption");
    find_root_button.set_halign(gtk::Align::Start);
    find_root_button.set_margin_start(8);
    find_root_button.set_margin_bottom(2);
    find_root_button.set_tooltip_text(Some("Choose another directory to scan"));
    find_root_button.set_visible(false);

    let sidebar_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_box.add_css_class("projects-sidebar");
    sidebar_box.append(&sidebar_header);
    sidebar_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    sidebar_box.append(&search);
    sidebar_box.append(&find_root_button);
    sidebar_box.append(&sidebar_scroll);
    sidebar_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    sidebar_box.append(&toggles);

    // ---- main area ----
    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .vexpand(true)
        .build();
    // The home panel takes the empty states' place: it is what shows with no
    // projects and no panes. It needs the finished app — its dropdowns write
    // preferences and its new-project flow selects — so it joins the stack
    // once the state exists, just before it can first be shown.

    let splitter = gtk::Paned::new(gtk::Orientation::Horizontal);
    splitter.set_start_child(Some(&sidebar_box));
    splitter.set_end_child(Some(&stack));
    splitter.set_resize_start_child(false);
    splitter.set_shrink_start_child(false);
    splitter.set_wide_handle(true);
    splitter.set_vexpand(true);
    splitter.set_position(
        db.ui_prefs()
            .map(|prefs| prefs.sidebar_width)
            .unwrap_or(260),
    );

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&splitter));
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
    window.set_content(Some(&root));
    window.set_tooltip_text(Some(&format!("state: {}", paths.database().display())));

    let (status_tx, status_rx) = std::sync::mpsc::channel();
    let (find_tx, find_rx) = std::sync::mpsc::channel();
    let (activity_tx, activity_rx) = std::sync::mpsc::sync_channel(512);
    let (agent_tx, agent_rx) = std::sync::mpsc::channel();
    let state = Rc::new(App {
        db: db.clone(),
        session_home: paths.data_dir.clone(),
        theme: RefCell::new(Theme::load()),
        window: window.clone(),
        sidebar: sidebar_box.upcast(),
        splitter,
        sidebar_shown: Cell::new(true),
        sidebar_list,
        sidebar_search: search.clone(),
        toggles: RefCell::new(toggle_buttons),
        projects_toggle: projects_toggle.clone(),
        home_toggle: home_toggle.clone(),
        home_shown: Cell::new(false),
        stack,
        toasts,
        hud: hud.clone(),
        workspaces: RefCell::new(HashMap::new()),
        projects: RefCell::new(Vec::new()),
        rows: RefCell::new(Vec::new()),
        agent_sessions: RefCell::new(HashMap::new()),
        agent_tx,
        agent_rx: RefCell::new(agent_rx),
        agent_scan_pending: Cell::new(false),
        expanded_projects: RefCell::new(HashSet::new()),
        archived_views: RefCell::new(HashSet::new()),
        status: RefCell::new(HashMap::new()),
        status_tx,
        status_rx: RefCell::new(status_rx),
        activity: RefCell::new(HashMap::new()),
        activity_online: RefCell::new(HashMap::new()),
        activity_watchers: RefCell::new(HashMap::new()),
        activity_tx,
        activity_rx: RefCell::new(activity_rx),
        board_panes: RefCell::new(HashMap::new()),
        header_info: RefCell::new(HashMap::new()),
        current: RefCell::new(None),
        find_root: RefCell::new(
            db.ui_prefs()
                .map(|prefs| prefs.resolved_add_root())
                .unwrap_or_else(|_| crate::config::default_project_root()),
        ),
        find_candidates: RefCell::new(Vec::new()),
        find_rows: RefCell::new(Vec::new()),
        find_tx,
        find_rx: RefCell::new(find_rx),
        find_root_button: find_root_button.clone(),
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
    start_find_drainer(&state);
    wire_sidebar_drop(&state);
    watch_theme(&state);
    state.find_root_button.set_label(&format!(
        "from {}",
        crate::db::abbreviate(&state.find_root.borrow())
    ));
    App::refresh_projects(&state);
    state.request_agent_scan();
    start_agent_session_polling(&state);
    // Development aid: exercise the new-project flow — folder, git init, add,
    // open — without the file chooser. RADAR_NEW_PROJECT=/some/path.
    if let Ok(path) = std::env::var("RADAR_NEW_PROJECT") {
        home::create_project(&state, PathBuf::from(path));
    }
    // Seed the candidate cache, so the first search is instant.
    state.rescan_find();
    if let Ok(prefs) = state.db.ui_prefs() {
        if let Some(id) = prefs.last_project {
            state.select_project(id);
        }
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
        Slot::Board => "Alt+K",
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

/// A candidate row in the sidebar: icon, name, path, a git pill for
/// repositories, and its own add button. The row itself is inert — adding is
/// the button's job, so several projects can be added in one search.
fn candidate_row(title: &str, subtitle: &str, is_repo: bool) -> (gtk::ListBoxRow, gtk::Button) {
    let row = gtk::ListBoxRow::new();
    row.set_activatable(false);
    let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 8);

    let icon = gtk::Image::from_icon_name("folder-symbolic");
    icon.add_css_class("row-icon");
    icon.set_pixel_size(16);
    icon.set_valign(gtk::Align::Center);
    box_.append(&icon);

    let texts = gtk::Box::new(gtk::Orientation::Vertical, 1);
    texts.set_valign(gtk::Align::Center);
    texts.set_hexpand(true);
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

    if is_repo {
        let badge = gtk::Label::new(Some("git"));
        badge.add_css_class("badge");
        badge.set_valign(gtk::Align::Center);
        box_.append(&badge);
    }

    let add = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add project")
        .build();
    add.add_css_class("flat");
    add.set_valign(gtk::Align::Center);
    box_.append(&add);

    row.set_child(Some(&box_));
    (row, add)
}

fn connect_widgets(app: &SharedApp) {
    {
        let list = app.sidebar_list.clone();
        let app = app.clone();
        list.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let Some(id) = app.id_for_row(row) else {
                return;
            };
            // Switching projects must not take the keys out of the sidebar:
            // keyboard selection keeps them on the row, and a mouse click
            // brings them here. The switch itself can pull them away — the
            // pane holding the window's focus is hidden, and the stack hands
            // focus to the pane it just showed — so land them back on the row
            // afterwards. Rows not yet mapped (startup) have nothing to grab.
            app.select_project(id);
            let on_row = app
                .window
                .focus_widget()
                .is_some_and(|focus| focus == row.clone().upcast::<gtk::Widget>());
            if !on_row && row.is_mapped() {
                row.grab_focus();
            }
        });
    }
    {
        // One box, two jobs: filter the projects, and below them find
        // directories to add. A fresh scan follows a moment after typing
        // stops, so what shows is what is on disk right now.
        let entry = app.sidebar_search.clone();
        let app = app.clone();
        let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        entry.connect_search_changed(move |entry| {
            app.filter_sidebar(&entry.text());
            App::show_candidates(&app, &entry.text());
            if let Some(id) = pending.borrow_mut().take() {
                id.remove();
            }
            if !entry.text().trim().is_empty() {
                let app = app.clone();
                let pending_for_cb = pending.clone();
                let id = glib::timeout_add_local_once(Duration::from_millis(250), move || {
                    app.rescan_find();
                    *pending_for_cb.borrow_mut() = None;
                });
                *pending.borrow_mut() = Some(id);
            }
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
    {
        // Hover is focus for the sidebar as a whole, matching the panes:
        // the pointer entering the projects panel puts the keys on its
        // selected row — the same ring a pane wears. Same gate as the panes:
        // the sidebar mapping under a parked pointer must not grab.
        let app_for_hover = app.clone();
        let controller = gtk::EventControllerMotion::new();
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        controller.connect_enter(move |_, _, _| {
            let app = &app_for_hover;
            if !app.pointer_is_live() {
                return;
            }
            if let Some(row) = keynav::sidebar_focus_row(&app.sidebar_list) {
                row.grab_focus();
            }
        });
        app.sidebar.add_controller(controller);
    }
    {
        // Inside the panel, walking the rows with the pointer keeps the keys
        // on the row under it — the row the user would arrow from.
        let app_for_hover = app.clone();
        let controller = gtk::EventControllerMotion::new();
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        controller.connect_enter(move |_, _, y| {
            let app = &app_for_hover;
            if !app.pointer_is_live() {
                return;
            }
            if let Some(row) = app.sidebar_list.row_at_y(y as i32) {
                if app.id_for_row(&row).is_some() {
                    row.grab_focus();
                }
            }
        });
        app.sidebar_list.add_controller(controller);
    }
    {
        // Esc empties the search, the way a browser's address bar does.
        let entry = app.sidebar_search.clone();
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
        // Pick another directory for the find search to scan.
        let app = app.clone();
        let root_button = app.find_root_button.clone();
        root_button.connect_clicked(move |_| {
            #[allow(deprecated)]
            let dialog = gtk::FileChooserDialog::new(
                Some("Choose a directory to scan"),
                Some(&app.window),
                gtk::FileChooserAction::SelectFolder,
                &[
                    ("Cancel", gtk::ResponseType::Cancel),
                    ("Scan", gtk::ResponseType::Accept),
                ],
            );
            let app = app.clone();
            #[allow(deprecated)]
            dialog.connect_response(move |dialog, response| {
                if response == gtk::ResponseType::Accept {
                    if let Some(path) = dialog.file().and_then(|file| file.path()) {
                        // Remember it, so the next find starts here.
                        let mut prefs = app.db.ui_prefs().unwrap_or_default();
                        prefs.add_root = Some(path.clone());
                        if let Err(error) = app.db.set_ui_prefs(&prefs) {
                            eprintln!("radar: could not store the scan root: {error}");
                        }
                        *app.find_root.borrow_mut() = path.clone();
                        app.find_root_button
                            .set_label(&format!("from {}", crate::db::abbreviate(&path)));
                        app.rescan_find();
                    }
                }
                dialog.close();
            });
            dialog.present();
        });
    }
    {
        // Remember the sidebar width when it is dragged.
        let splitter = app.splitter.clone();
        let app = app.clone();
        let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        splitter.connect_position_notify(move |splitter| {
            if let Some(id) = pending.borrow_mut().take() {
                id.remove();
            }
            let app = app.clone();
            let pending_for_cb = pending.clone();
            let position = splitter.position();
            let id = glib::timeout_add_local_once(Duration::from_millis(400), move || {
                let mut prefs = app.db.ui_prefs().unwrap_or_default();
                prefs.sidebar_width = position;
                if let Err(error) = app.db.set_ui_prefs(&prefs) {
                    eprintln!("radar: could not store the sidebar width: {error}");
                }
                *pending_for_cb.borrow_mut() = None;
            });
            *pending.borrow_mut() = Some(id);
        });
    }
}

/// Apply directory scans that arrived from the worker thread.
fn start_find_drainer(app: &SharedApp) {
    let app = app.clone();
    glib::timeout_add_local(Duration::from_millis(120), move || {
        let batch = {
            let rx = app.find_rx.borrow();
            rx.try_recv().ok()
        };
        if let Some((_root, found)) = batch {
            *app.find_candidates.borrow_mut() = found;
            // Results only matter while a search is up.
            let query = app.sidebar_search.text();
            if !query.trim().is_empty() {
                App::show_candidates(&app, &query);
            }
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
            app.apply_status_labels();
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
        let Some(sessions) = latest else {
            return glib::ControlFlow::Continue;
        };
        app.agent_scan_pending.set(false);

        let mut grouped: HashMap<i64, Vec<live_agents::AgentSession>> = HashMap::new();
        for session in sessions {
            grouped.entry(session.project_id).or_default().push(session);
        }
        if *app.agent_sessions.borrow() == grouped {
            return glib::ControlFlow::Continue;
        }
        *app.agent_sessions.borrow_mut() = grouped;
        let rows: Vec<(i64, gtk::Box)> = app
            .rows
            .borrow()
            .iter()
            .map(|row| (row.id, row.agents.clone()))
            .collect();
        for (project_id, agents) in rows {
            app.populate_project_agents(project_id, &agents);
        }
        let query = app.sidebar_search.text();
        app.filter_sidebar(&query);
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
        for project_id in &changed_projects {
            app.refresh_activity_pane(*project_id);
        }
        if !changed_projects.is_empty() {
            app.apply_status_labels();
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

    // ---- projects ----
    {
        // The search is the add flow; this action just puts the keys there.
        let app = app.clone();
        add(
            "find-projects",
            Box::new(move || app.focus_projects_search()),
        );
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
                let entry = gtk::Entry::new();
                entry.set_text(&project.name);
                let dialog = gtk::Window::builder()
                    .title("Rename project")
                    .modal(true)
                    .default_width(420)
                    .transient_for(&app.window)
                    .build();
                let box_ = gtk::Box::new(gtk::Orientation::Vertical, 12);
                box_.set_margin_top(18);
                box_.set_margin_bottom(18);
                box_.set_margin_start(18);
                box_.set_margin_end(18);
                box_.append(&entry);
                let save = gtk::Button::with_label("Rename");
                save.add_css_class("suggested-action");
                box_.append(&save);
                dialog.set_child(Some(&box_));
                let app_for_save = app.clone();
                let dialog_for_save = dialog.clone();
                let entry_for_save = entry.clone();
                save.connect_clicked(move |_| {
                    let name = entry_for_save.text().to_string();
                    if let Err(error) = app_for_save.db.rename_project(project.id, &name) {
                        eprintln!("radar: {error}");
                    }
                    App::refresh_projects(&app_for_save);
                    dialog_for_save.close();
                });
                entry.connect_activate({
                    let save = save.clone();
                    move |_| save.emit_clicked()
                });
                dialog.present();
                entry.grab_focus();
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
            "project-session-archive",
            Some(glib::VariantTy::new("(xb)").expect("a catalog row and archive flag")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some((catalog_id, archived)) =
                parameter.and_then(|value| value.get::<(i64, bool)>())
            else {
                return;
            };
            use crate::session::daemon::{Client, Command};
            let request = Command::CatalogArchive {
                id: catalog_id,
                archived,
            };
            match Client::request(&app_for_action.session_home, request) {
                Ok(_) => app_for_action.request_agent_scan(),
                Err(error) => {
                    app_for_action.toast(&format!("Could not update the session: {error}"))
                }
            }
        });
        app.window.add_action(&action);
    }
    {
        let action = gio::SimpleAction::new(
            "project-archived-view",
            Some(glib::VariantTy::new("(xb)").expect("a project and archived-view flag")),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some((project_id, show)) = parameter.and_then(|value| value.get::<(i64, bool)>())
            else {
                return;
            };
            if show {
                app_for_action
                    .archived_views
                    .borrow_mut()
                    .insert(project_id);
            } else {
                app_for_action
                    .archived_views
                    .borrow_mut()
                    .remove(&project_id);
            }
            app_for_action.refresh_project_agents(project_id);
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
        add("toggle-sidebar", Box::new(move || app.toggle_sidebar()));
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
    let accels: [(&str, &[&str]); 18] = [
        ("win.find-projects", &["<Alt>n"]),
        ("win.preferences", &["<Alt>comma"]),
        ("win.refresh", &["<Alt>r"]),
        ("win.quit", &["<Alt>q"]),
        ("win.toggle-sidebar", &["<Alt>b"]),
        ("win.zoom", &["<Alt>f"]),
        ("win.hud", &["<Alt>h"]),
        ("win.show-home", &["<Alt>Home"]),
        ("win.primitive-toggle::editor", &["<Alt>e"]),
        ("win.primitive-toggle::agent", &["<Alt>a"]),
        ("win.primitive-toggle::diff", &["<Alt>g"]),
        ("win.primitive-toggle::board", &["<Alt>k"]),
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

    /// Which project does a sidebar row belong to?
    fn id_for_row(&self, row: &gtk::ListBoxRow) -> Option<i64> {
        self.rows
            .borrow()
            .iter()
            .find(|project_row| project_row.row == *row)
            .map(|project_row| project_row.id)
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

    /// Confirm, then remove a project from the sidebar. Never touches the disk.
    fn confirm_remove(self: &Rc<Self>, id: i64) {
        let Some(project) = self.db.project(id).ok().flatten() else {
            return;
        };
        let dialog = gtk::AlertDialog::builder()
            .message(format!("Remove {}?", project.name))
            .detail("It leaves the sidebar. Nothing on disk is touched.")
            .buttons(["Cancel", "Remove"])
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
                if let Err(error) = app.db.remove_project(project.id) {
                    eprintln!("radar: {error}");
                }
                // Rebuild the sidebar from a fresh read; this also drops the
                // selection of the removed project.
                App::refresh_projects(&app);
                app.toasts.add_toast(adw::Toast::new("Project removed"));
            },
        );
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
            self.board_panes.borrow_mut().remove(&project_id);
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

    fn apply_activity_notice(&self, notice: ActivityNotice) -> i64 {
        let project_id = match notice {
            ActivityNotice::Snapshot {
                project_id,
                snapshot,
                replace_events,
            } => {
                self.activity
                    .borrow_mut()
                    .entry(project_id)
                    .or_insert_with(|| ProjectActivity::empty(project_id))
                    .merge_snapshot(snapshot, replace_events);
                project_id
            }
            ActivityNotice::Event(event) => {
                let project_id = event.project_id;
                self.activity
                    .borrow_mut()
                    .entry(project_id)
                    .or_insert_with(|| ProjectActivity::empty(project_id))
                    .apply_event(event);
                project_id
            }
            ActivityNotice::Connection { project_id, online } => {
                self.activity_online.borrow_mut().insert(project_id, online);
                project_id
            }
            ActivityNotice::Mutation {
                project_id,
                request_id,
                result,
            } => {
                match result {
                    Ok(result) => {
                        let result = *result;
                        let attention = result.attention;
                        let mut activity = self.activity.borrow_mut();
                        let project = activity
                            .entry(project_id)
                            .or_insert_with(|| ProjectActivity::empty(project_id));
                        if let Some(event) = result.event {
                            project.apply_event(event);
                        }
                        project.upsert_attention(attention.clone());
                        drop(activity);
                        if let Some(pane) = self
                            .board_panes
                            .borrow()
                            .get(&project_id)
                            .and_then(std::rc::Weak::upgrade)
                        {
                            pane.finish_attention_change(&request_id, Ok(attention));
                        }
                    }
                    Err(error) => {
                        if let Some(pane) = self
                            .board_panes
                            .borrow()
                            .get(&project_id)
                            .and_then(std::rc::Weak::upgrade)
                        {
                            pane.finish_attention_change(&request_id, Err(error));
                        }
                    }
                }
                project_id
            }
        };

        project_id
    }

    fn refresh_activity_pane(&self, project_id: i64) {
        if let Some(pane) = self
            .board_panes
            .borrow()
            .get(&project_id)
            .and_then(std::rc::Weak::upgrade)
        {
            let activity = self.activity.borrow();
            let snapshot = activity
                .get(&project_id)
                .map(|activity| activity.snapshot.clone())
                .unwrap_or_else(|| crate::session::activity::ActivitySnapshot {
                    project_id,
                    watermark: 0,
                    events: Vec::new(),
                    attention: Vec::new(),
                    has_more: false,
                });
            let online = self
                .activity_online
                .borrow()
                .get(&project_id)
                .copied()
                .unwrap_or(false);
            pane.set_activity_state(snapshot, online);
        }
    }

    fn refresh_projects(app: &SharedApp) {
        let projects = app.db.projects().unwrap_or_default();
        let selected = *app.current.borrow();
        *app.projects.borrow_mut() = projects.clone();
        app.request_agent_scan();
        app.reconcile_activity_watchers(&projects);

        while let Some(child) = app.sidebar_list.first_child() {
            app.sidebar_list.remove(&child);
        }
        app.find_rows.borrow_mut().clear();
        app.rows.borrow_mut().clear();
        if projects.is_empty() {
            // A quiet hint, not a selectable row.
            let empty = gtk::ListBoxRow::new();
            empty.set_selectable(false);
            empty.set_activatable(false);
            empty.add_css_class("sidebar-empty");
            let hint_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
            hint_box.set_halign(gtk::Align::Center);
            hint_box.set_margin_top(32);
            hint_box.set_margin_bottom(24);
            let icon = gtk::Image::from_icon_name("folder-open-symbolic");
            icon.add_css_class("dim-label");
            icon.set_pixel_size(28);
            let title = gtk::Label::new(Some("No projects yet"));
            title.add_css_class("caption-heading");
            let hint = gtk::Label::new(Some("Search above for a directory to add"));
            hint.add_css_class("caption");
            hint.add_css_class("dim-label");
            hint.set_wrap(true);
            hint.set_justify(gtk::Justification::Center);
            hint_box.append(&icon);
            hint_box.append(&title);
            hint_box.append(&hint);
            empty.set_child(Some(&hint_box));
            app.sidebar_list.append(&empty);
            // Even with no projects, a search can already offer directories.
            App::show_candidates(app, &app.sidebar_search.text());
            app.show_home();
            return;
        }

        for project in &projects {
            let row = app.build_project_row(project);
            app.sidebar_list.append(&row.row);
            app.rows.borrow_mut().push(row);
        }
        app.filter_sidebar(&app.sidebar_search.text());
        // Candidate directories for the query go under the project rows.
        App::show_candidates(app, &app.sidebar_search.text());
        app.apply_status_labels();
        app.refresh_status();

        if let Some(id) = selected {
            app.select_row_for(id);
        }
        // Nothing selected picks the first project — unless the user is on
        // the home panel on purpose, which a refresh must not disturb.
        if !app.home_shown.get()
            && (selected.is_none()
                || selected.is_some_and(|id| !projects.iter().any(|p| p.id == id)))
        {
            if let Some(first) = projects.first() {
                app.select_project(first.id);
            }
        }
    }

    fn build_project_row(self: &Rc<Self>, project: &Project) -> ProjectRow {
        let row = gtk::ListBoxRow::new();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 8);

        let missing = project.is_missing();
        let icon = gtk::Image::from_icon_name(if missing {
            "dialog-warning-symbolic"
        } else {
            "folder-symbolic"
        });
        icon.add_css_class("row-icon");
        icon.set_pixel_size(16);
        icon.set_valign(gtk::Align::Center);
        if missing {
            icon.add_css_class("missing");
        }
        box_.append(&icon);

        let texts = gtk::Box::new(gtk::Orientation::Vertical, 1);
        texts.set_valign(gtk::Align::Center);
        texts.set_hexpand(true);
        let name_line = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let name = gtk::Label::new(Some(&project.name));
        name.set_xalign(0.0);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name_line.append(&name);
        if project.pinned {
            let pin = gtk::Image::from_icon_name("starred-symbolic");
            pin.add_css_class("pin-icon");
            pin.set_pixel_size(12);
            pin.set_valign(gtk::Align::Center);
            pin.set_tooltip_text(Some("Pinned"));
            name_line.append(&pin);
        }
        texts.append(&name_line);

        let parent = project
            .path
            .parent()
            .map(crate::db::abbreviate)
            .unwrap_or_else(|| project.display_path());
        let summary = gtk::Label::new(None);
        summary.set_xalign(0.0);
        summary.add_css_class("caption");
        summary.add_css_class("dim-label");
        summary.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        texts.append(&summary);
        box_.append(&texts);

        let badge = gtk::Label::new(None);
        badge.add_css_class("badge");
        badge.set_valign(gtk::Align::Center);
        badge.set_visible(false);
        box_.append(&badge);

        let attention_badge = gtk::Label::new(None);
        attention_badge.add_css_class("badge");
        attention_badge.add_css_class("attention-badge");
        attention_badge.set_valign(gtk::Align::Center);
        attention_badge.set_visible(false);
        box_.append(&attention_badge);

        let agents_revealer = gtk::Revealer::new();
        agents_revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
        agents_revealer.set_transition_duration(120);
        let agents = gtk::Box::new(gtk::Orientation::Vertical, 2);
        agents.add_css_class("agent-list");
        agents.set_margin_start(8);
        self.populate_project_agents(project.id, &agents);
        agents_revealer.set_child(Some(&agents));

        let agent_toggle = gtk::ToggleButton::new();
        agent_toggle.add_css_class("flat");
        agent_toggle.add_css_class("agent-toggle");
        agent_toggle.set_tooltip_text(Some("Show agents in this project"));
        agent_toggle.set_valign(gtk::Align::Center);
        let toggle_icon = gtk::Image::from_icon_name("pan-end-symbolic");
        agent_toggle.set_child(Some(&toggle_icon));
        let expanded = self.expanded_projects.borrow().contains(&project.id);
        agent_toggle.set_active(expanded);
        agents_revealer.set_reveal_child(expanded);
        let expanded_projects = self.expanded_projects.clone();
        let project_id = project.id;
        let revealer = agents_revealer.clone();
        agent_toggle.connect_toggled(move |button| {
            let expanded = button.is_active();
            revealer.set_reveal_child(expanded);
            toggle_icon.set_icon_name(Some(if expanded {
                "pan-down-symbolic"
            } else {
                "pan-end-symbolic"
            }));
            button.set_tooltip_text(Some(if expanded {
                "Hide agents in this project"
            } else {
                "Show agents in this project"
            }));
            if expanded {
                expanded_projects.borrow_mut().insert(project_id);
            } else {
                expanded_projects.borrow_mut().remove(&project_id);
            }
        });
        box_.append(&agent_toggle);
        let board_toggle = gtk::ToggleButton::with_label("Board");
        board_toggle.add_css_class("flat");
        board_toggle.add_css_class("row-action");
        board_toggle.set_valign(gtk::Align::Center);
        let board_enabled = crate::board::enabled(&self.db, &project.path).unwrap_or(true);
        board_toggle.set_active(board_enabled);
        board_toggle.set_sensitive(!missing);
        board_toggle.set_tooltip_text(Some(if board_enabled {
            "Disable board for this project"
        } else {
            "Enable board for this project"
        }));
        let project_path = project.path.clone();
        let project_id = project.id;
        let db = self.db.clone();
        let app = Rc::downgrade(self);
        let changing = Rc::new(Cell::new(false));
        let changing_signal = changing.clone();
        board_toggle.connect_toggled(move |button| {
            if changing_signal.get() {
                return;
            }
            let enabled = button.is_active();
            if let Err(error) = crate::board::set_enabled(&db, &project_path, enabled) {
                changing_signal.set(true);
                button.set_active(!enabled);
                changing_signal.set(false);
                if let Some(app) = app.upgrade() {
                    app.toast(&format!("Could not update board setting: {error}"));
                }
                return;
            }
            button.set_tooltip_text(Some(if enabled {
                "Disable board for this project"
            } else {
                "Enable board for this project"
            }));
            if let Some(app) = app.upgrade() {
                app.project_board_setting_changed(project_id, &project_path, enabled);
            }
        });
        box_.append(&board_toggle);
        let project_defaults = gtk::Button::builder()
            .icon_name("emblem-system-symbolic")
            .tooltip_text("Project defaults")
            .build();
        project_defaults.add_css_class("flat");
        project_defaults.add_css_class("row-action");
        project_defaults.set_valign(gtk::Align::Center);
        project_defaults.set_sensitive(!missing);
        let settings_app = Rc::downgrade(self);
        let settings_id = project.id;
        let settings_name = project.name.clone();
        project_defaults.connect_clicked(move |_| {
            if let Some(app) = settings_app.upgrade() {
                let changed_app = Rc::downgrade(&app);
                dialogs::project_preferences(
                    &app.window,
                    &app.db,
                    settings_id,
                    &settings_name,
                    move || {
                        if let Some(app) = changed_app.upgrade() {
                            app.sync_toggles();
                            app.refresh_menus();
                            app.toast("Project defaults saved");
                        }
                    },
                );
            }
        });
        box_.append(&project_defaults);
        let create_agent = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Create an agent in this project")
            .build();
        create_agent.add_css_class("flat");
        create_agent.add_css_class("row-action");
        create_agent.set_valign(gtk::Align::Center);
        create_agent.set_sensitive(!missing);
        create_agent.set_action_name(Some("win.project-agent-create"));
        create_agent.set_action_target_value(Some(&project.id.to_variant()));
        box_.append(&create_agent);

        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove from sidebar")
            .build();
        remove.add_css_class("flat");
        remove.add_css_class("row-action");
        remove.set_valign(gtk::Align::Center);
        remove.set_action_name(Some("win.project-remove"));
        remove.set_action_target_value(Some(&(project.id as i32).to_variant()));
        box_.append(&remove);

        content.append(&box_);
        content.append(&agents_revealer);
        row.set_child(Some(&content));
        row.set_tooltip_text(Some(&if missing {
            format!("{} (missing)", project.display_path())
        } else {
            project.display_path()
        }));

        let status = self.status.borrow().get(&project.id).cloned();
        let text = match (&status, missing) {
            (_, true) => "missing".to_string(),
            (Some(status), _) => status.summary(),
            (None, _) => "…".to_string(),
        };
        summary.set_text(&format!("{text}  ·  {parent}"));
        badge.set_tooltip_text(Some("Active embedded tools in this project"));
        attention_badge.set_tooltip_text(Some("Unresolved requests for human attention"));
        ProjectRow {
            id: project.id,
            row,
            summary,
            badge,
            attention_badge,
            agents,
            agent_toggle,
        }
    }

    fn project_board_setting_changed(&self, id: i64, path: &std::path::Path, enabled: bool) {
        if enabled {
            if let Err(error) = crate::board::ensure_enabled_file(&self.db, path) {
                self.toast(&format!("Board enabled, but setup failed: {error}"));
            }
        } else if let Some(workspace) = self.workspaces.borrow().get(&id).cloned() {
            self.restore_zoom(&workspace);
            let key = TabKey::first(Slot::Board);
            if workspace.is_visible(key) {
                self.toggle_primitive(&workspace, key);
            }
        }
        self.sync_toggles();
    }

    fn populate_project_agents(&self, project_id: i64, container: &gtk::Box) {
        while let Some(child) = container.first_child() {
            container.remove(&child);
        }

        let archived_view = self.archived_views.borrow().contains(&project_id);
        let mut sessions: Vec<_> = self
            .agent_sessions
            .borrow()
            .get(&project_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|session| session.archived == archived_view)
            .map(|session| {
                let title = session.title.clone();
                (session, title)
            })
            .collect();
        live_agents::sort_sidebar_sessions(&mut sessions);

        let has_archived = self
            .agent_sessions
            .borrow()
            .get(&project_id)
            .is_some_and(|rows| rows.iter().any(|session| session.archived));
        if has_archived {
            let filter = gtk::ToggleButton::with_label(if archived_view {
                "Showing archived"
            } else {
                "Archived"
            });
            filter.add_css_class("flat");
            filter.add_css_class("caption");
            filter.add_css_class("agent-archived-toggle");
            filter.set_halign(gtk::Align::Start);
            filter.set_margin_start(12);
            filter.set_active(archived_view);
            filter.set_action_name(Some("win.project-archived-view"));
            filter.set_action_target_value(Some(&(project_id, !archived_view).to_variant()));
            container.append(&filter);
        }

        for (session, title) in &sessions {
            let program = programs::by_id(&session.program_id);
            let program_name = program
                .as_ref()
                .map(|program| program.name.clone())
                .unwrap_or_else(|| session.program_id.clone());
            let state = if session.external.is_some() {
                "external terminal"
            } else if session.running {
                "running"
            } else {
                "ended"
            };
            let subtitle = format!(
                "{program_name} · {} · {state}",
                relative_time(session.last_activity_at)
            );

            let row = gtk::Box::new(gtk::Orientation::Horizontal, 2);
            let button = gtk::Button::new();
            button.add_css_class("flat");
            button.add_css_class("agent-child");
            button.set_halign(gtk::Align::Fill);
            button.set_hexpand(true);
            let action = if session.external.is_some() {
                "Focus terminal for"
            } else {
                "Open session:"
            };
            button.set_tooltip_text(Some(&format!(
                "{action} {title}{}",
                if archived_view { " (archived)" } else { "" }
            )));

            let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let icon = gtk::Image::from_icon_name("application-x-executable-symbolic");
            icon.add_css_class("row-icon");
            if session.running {
                icon.add_css_class("agent-running");
            }
            icon.set_pixel_size(12);
            icon.set_valign(gtk::Align::Center);
            let texts = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let label = gtk::Label::new(Some(title));
            label.set_xalign(0.0);
            label.set_hexpand(true);
            label.set_single_line_mode(true);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let detail = gtk::Label::new(Some(&subtitle));
            detail.set_xalign(0.0);
            detail.add_css_class("caption");
            detail.add_css_class("dim-label");
            detail.set_single_line_mode(true);
            detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
            texts.append(&label);
            texts.append(&detail);
            content.append(&icon);
            content.append(&texts);
            button.set_child(Some(&content));
            if let Some(target) = &session.external {
                button.set_action_name(Some("win.project-agent-open-external"));
                button.set_action_target_value(Some(
                    &(
                        project_id,
                        target.pid,
                        target.start_ticks,
                        target.window_pid,
                    )
                        .to_variant(),
                ));
            } else {
                button.set_action_name(Some("win.project-session-open"));
                button
                    .set_action_target_value(Some(&(project_id, session.id.as_str()).to_variant()));
            }
            row.append(&button);

            if let Some(catalog_id) = session.catalog_id {
                let archive = gtk::Button::builder()
                    .icon_name(if session.archived {
                        "view-restore-symbolic"
                    } else {
                        "mail-archive-symbolic"
                    })
                    .tooltip_text(if session.archived {
                        "Unarchive this session"
                    } else {
                        "Archive this session"
                    })
                    .build();
                archive.add_css_class("flat");
                archive.add_css_class("row-action");
                archive.set_valign(gtk::Align::Center);
                archive.set_action_name(Some("win.project-session-archive"));
                archive
                    .set_action_target_value(Some(&(catalog_id, !session.archived).to_variant()));
                row.append(&archive);
            }
            container.append(&row);
        }

        if sessions.is_empty() {
            let empty = gtk::Label::new(Some(if archived_view {
                "No archived sessions"
            } else {
                "No agent sessions yet"
            }));
            empty.add_css_class("caption");
            empty.add_css_class("dim-label");
            empty.set_xalign(0.0);
            empty.set_margin_start(12);
            empty.set_margin_top(3);
            empty.set_margin_bottom(3);
            container.append(&empty);
        }
    }
    fn refresh_project_agents(&self, project_id: i64) {
        let container = self
            .rows
            .borrow()
            .iter()
            .find(|row| row.id == project_id)
            .map(|row| row.agents.clone());
        if let Some(container) = container {
            self.populate_project_agents(project_id, &container);
        }
    }

    fn set_project_expanded(&self, project_id: i64, expanded: bool) {
        let toggle = self
            .rows
            .borrow()
            .iter()
            .find(|row| row.id == project_id)
            .map(|row| row.agent_toggle.clone());
        if let Some(toggle) = toggle {
            toggle.set_active(expanded);
        } else if expanded {
            self.expanded_projects.borrow_mut().insert(project_id);
        } else {
            self.expanded_projects.borrow_mut().remove(&project_id);
        }
    }

    fn apply_status_labels(&self) {
        let projects = self.projects.borrow();
        let rows = self.rows.borrow();
        let activity = self.activity.borrow();
        let statuses = self.status.borrow();
        let workspaces = self.workspaces.borrow();
        for row in rows.iter() {
            let Some(project) = projects.iter().find(|project| project.id == row.id) else {
                continue;
            };
            let parent = project
                .path
                .parent()
                .map(crate::db::abbreviate)
                .unwrap_or_else(|| project.display_path());
            let attention_count = activity
                .get(&row.id)
                .map_or(0, |activity| activity.snapshot.attention.len());
            row.attention_badge.set_text(&attention_count.to_string());
            row.attention_badge.set_visible(attention_count > 0);
            if project.is_missing() {
                row.summary.set_text(&format!("missing  ·  {parent}"));
                row.badge.set_visible(false);
                continue;
            }
            match statuses.get(&row.id) {
                Some(status) => {
                    row.summary
                        .set_text(&format!("{}  ·  {parent}", status.summary()));
                }
                None => row.summary.set_text(&format!("…  ·  {parent}")),
            }
            let count = workspaces.get(&row.id).map_or(0, |workspace| {
                workspace
                    .tabs
                    .borrow()
                    .values()
                    .filter(|primitive| primitive.pane.as_ref().is_some_and(|pane| pane.is_live()))
                    .count()
            });
            row.badge.set_text(&count.to_string());
            row.badge.set_visible(count > 0);
        }
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
            let sessions = live_agents::discover(&projects, &session_home);
            let _ = tx.send(sessions);
        });
    }

    fn select_row_for(&self, id: i64) {
        let target = self
            .rows
            .borrow()
            .iter()
            .find(|row| row.id == id)
            .map(|row| row.row.clone());
        if let Some(row) = target {
            self.sidebar_list.select_row(Some(&row));
        }
    }

    fn filter_sidebar(&self, query: &str) {
        let matcher = fuzzy_matcher::skim::SkimMatcherV2::default().ignore_case();
        use fuzzy_matcher::FuzzyMatcher;
        let query = query.trim();
        let projects = self.projects.borrow();
        let agent_sessions = self.agent_sessions.borrow();
        for row in self.rows.borrow().iter() {
            let Some(project) = projects.iter().find(|p| p.id == row.id) else {
                continue;
            };
            let project_matches = matcher.fuzzy_match(&project.name, query).is_some()
                || matcher
                    .fuzzy_match(&project.display_path(), query)
                    .is_some();
            let agent_matches = agent_sessions.get(&row.id).is_some_and(|sessions| {
                sessions.iter().any(|session| {
                    matcher.fuzzy_match(&session.title, query).is_some()
                        || matcher.fuzzy_match(&session.program_id, query).is_some()
                })
            });
            row.row
                .set_visible(query.is_empty() || project_matches || agent_matches);
        }
    }

    /// The home panel: the empty state, rebuilt so its dropdowns say what the
    /// preferences say right now. Workspaces stay alive behind it — going
    /// home looks away, it never stops anything.
    fn show_home(self: &Rc<Self>) {
        self.home_shown.set(true);
        *self.current.borrow_mut() = None;
        // The sidebar's selection stops meaning anything: no project is on
        // screen. None is ignored by the row-selected handler.
        self.sidebar_list.select_row(None::<&gtk::ListBoxRow>);
        while let Some(child) = self.stack.child_by_name("_home") {
            self.stack.remove(&child);
        }
        self.stack.add_named(&home::panel(self), Some("_home"));
        self.stack.set_visible_child_name("_home");
        let _ = self.db.remember_last_project(None);
        self.sync_toggles();
    }

    // ---- the search is the add flow ----

    /// Put the keys on the search: filter there, or type to find a directory.
    fn focus_projects_search(&self) {
        if !self.sidebar_shown.get() {
            self.toggle_sidebar();
        }
        self.sidebar_search.grab_focus();
    }

    /// Scan the root for candidate directories, off the main thread. The
    /// drainer refreshes the candidate section when the results arrive.
    fn rescan_find(&self) {
        let root = self.find_root.borrow().clone();
        let tx = self.find_tx.clone();
        std::thread::spawn(move || {
            let found = discover::scan(&root, 3, 800);
            let _ = tx.send((root, found));
        });
    }

    /// Rebuild the candidate section under the project rows: directories that
    /// match the query and are not projects yet, each with its own add button.
    /// An empty query leaves the sidebar a plain project list.
    fn show_candidates(app: &SharedApp, query: &str) {
        for row in app.find_rows.borrow_mut().drain(..) {
            app.sidebar_list.remove(&row);
        }
        let query = query.trim();
        // Where the results come from, and how to point the search elsewhere.
        app.find_root_button.set_visible(!query.is_empty());
        if query.is_empty() {
            return;
        }

        let known: Vec<PathBuf> = app
            .db
            .projects()
            .map(|projects| projects.into_iter().map(|p| p.path).collect())
            .unwrap_or_default();
        let mut all = app.find_candidates.borrow().clone();
        discover::mark_known(&mut all, &known);
        let matches: Vec<Candidate> = discover::filter(&all, query)
            .into_iter()
            .filter(|candidate| !candidate.known)
            .collect();

        for candidate in matches {
            let (item, add) = candidate_row(
                &candidate.name,
                &candidate.display_path(),
                candidate.is_repo,
            );
            let path = candidate.path.clone();
            let db = app.db.clone();
            let app_for_add = app.clone();
            add.connect_clicked(move |_| match db.add_project(&path) {
                Ok(project) => {
                    app_for_add.toast(&format!("Added {}", project.name));
                    // The rebuild puts the project among the rows above and
                    // drops this candidate row; the search stays up, so more
                    // can be added.
                    App::refresh_projects(&app_for_add);
                }
                Err(error) => eprintln!("radar: {error}"),
            });
            app.sidebar_list.append(&item);
            app.find_rows.borrow_mut().push(item);
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
        // Initialize the board only when enabled in global project settings.
        match crate::board::enabled(&self.db, &project.path) {
            Ok(true) => {
                if let Err(error) = crate::board::ensure_file(&project.path) {
                    eprintln!("radar: setting up the board: {error}");
                }
            }
            Ok(false) => {}
            Err(error) => eprintln!("radar: reading board policy: {error}"),
        }
        self.sync_toggles();
        self.refresh_menus();
        self.select_row_for(id);
        self.set_project_expanded(id, true);
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
        let board_enabled = crate::board::enabled(&self.db, &project.path).unwrap_or(true);
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
        let board_open = board_enabled && saved_state.as_ref().is_some_and(saved_board_open);
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
                    if *slot == Slot::Board && !board_enabled {
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
                if tab.slot == Slot::Board && !board_enabled {
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
        let board_key = TabKey::first(Slot::Board);
        if board_open && !wanted.contains(&board_key) {
            wanted.push(board_key);
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
        if restore_plan.board_open {
            if let Some(group) = groups.iter().find(|group| group.contains(board_key)) {
                let current_zoom = workspace.zoom.borrow_mut().take();
                let snapshot = current_zoom
                    .unwrap_or_else(|| (groups.clone(), workspace.tree.borrow_mut().take()));
                *workspace.zoom.borrow_mut() = Some(snapshot);
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
        if key.slot == Slot::Board
            && !crate::board::enabled(&self.db, &workspace.project.path).unwrap_or(true)
        {
            return None;
        }
        if let Some(existing) = workspace.tab(key) {
            return Some(existing);
        }
        // The board is the one primitive that runs nothing: the pane is
        // radar's own widget over the project's BOARD.md. One per project —
        // every board key resolves to the same pane.
        if key.slot == Slot::Board {
            let project_id = workspace.project.id;
            let pane = board::BoardPane::new(
                &workspace.project.path,
                project_id,
                &self.session_home,
                self.activity_tx.clone(),
                &self.window,
            );
            let snapshot = self
                .activity
                .borrow()
                .get(&project_id)
                .map(|activity| activity.snapshot.clone())
                .unwrap_or_else(|| ProjectActivity::empty(project_id).snapshot);
            let online = self
                .activity_online
                .borrow()
                .get(&project_id)
                .copied()
                .unwrap_or(false);
            pane.set_activity_state(snapshot, online);
            self.board_panes
                .borrow_mut()
                .insert(project_id, Rc::downgrade(&pane));
            // The board's header lives on the board's own counts.
            let window = self.window.clone();
            pane.set_info_observer(move |text| {
                let _ = gtk::prelude::WidgetExt::activate_action(
                    &window,
                    "win.pane-info",
                    Some(
                        &(
                            project_id,
                            TabKey::first(Slot::Board).as_str(),
                            text.unwrap_or_default(),
                        )
                            .to_variant(),
                    ),
                );
            });
            workspace
                .programs
                .borrow_mut()
                .insert(key.slot, "board".to_string());
            let primitive = Primitive::builtin(
                "board",
                pane.widget().clone(),
                "Board\nbuilt into radar — the project's BOARD.md",
            );
            workspace.tabs.borrow_mut().insert(key, primitive.clone());
            return Some(primitive);
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
        // An agent meets the board at launch: the board file and the skill
        // that makes it the convention are both in place before the agent
        // draws its first frame, and the launch claims work under a name
        // unique to this instance — two agents of the same kind never hold
        // each other's cards.
        let mut launch_record: Option<(String, u128)> = None;
        if program.kind == Kind::Agent {
            match crate::board::enabled(&self.db, &workspace.project.path) {
                Ok(true) => {
                    if let Err(error) =
                        crate::board::ensure_enabled_file(&self.db, &workspace.project.path)
                            .and_then(|_| crate::skill::install(&self.db, &workspace.project.path))
                    {
                        eprintln!("radar: setting up the board: {error}");
                    }
                }
                Ok(false) => {}
                Err(error) => eprintln!("radar: reading board policy: {error}"),
            }
            let stamp = crate::programs::launch::now_stamp();
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
        if key.slot != Slot::Board && self.leave_board(workspace) && workspace.is_visible(key) {
            self.activate_primitive(workspace, key);
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
        self.apply_status_labels();
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
        // The board is one per project: nothing to add, just look at it.
        if source.slot == Slot::Board {
            self.show_primitive(workspace, TabKey::first(Slot::Board));
            return;
        }
        self.leave_board(workspace);
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
        self.apply_status_labels();
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
        self.apply_status_labels();
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
        if key.slot != Slot::Board {
            self.leave_board(workspace);
        }
        if let Some(group) = workspace.group_of(key) {
            group.activate(key);
            self.refresh_group_menu(&group);
            if let Some(primitive) = workspace.tab(key) {
                primitive.focus();
            }
            self.persist_primitives(workspace);
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
            self.apply_status_labels();
        }
        self.activate_primitive(workspace, key);
    }

    /// The board's @claim links: the named agent's session, opened in THE
    /// agent panel. The exact match is the tab whose program carries the
    /// claim as its own `RADAR_AGENT`, read from /proc — two tabs running
    /// the same program are told apart by their stamps. When that cannot
    /// be read, the claim's leading program (`program-stamp`) still picks
    /// a tab, and any agent tab takes the rest: an agent radar did not
    /// launch works in the agent panel all the same. A program that has
    /// exited — or a tab not yet opened — runs again, resuming the
    /// project's last conversation with the agent's own resume flags.
    fn open_agent_session(&self, workspace: &Rc<Workspace>, claim: &str) {
        let wanted = claim.rsplit_once('-').map(|(program, _)| program);
        let mut candidates: Vec<(TabKey, bool, bool)> = workspace
            .tabs
            .borrow()
            .keys()
            .filter(|key| key.slot == Slot::Agent)
            .map(|key| {
                let primitive = workspace.tab(*key);
                let exact = primitive
                    .as_ref()
                    .and_then(|p| p.pane.as_ref())
                    .and_then(|pane| pane.session_pid())
                    .and_then(programs::launch::radar_agent_of)
                    .is_some_and(|agent| agent == claim);
                let program = primitive
                    .as_ref()
                    .is_some_and(|p| Some(p.program_id.as_str()) == wanted);
                (*key, exact, program)
            })
            .collect();
        // Best first: the exact claim, then the claim's program, then the
        // first agent tab of the kind.
        candidates.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)).then(a.0.cmp(&b.0)));
        if let Some((key, true, _)) = candidates.first() {
            // The claimed conversation is this one, running: never
            // disturb a live agent that already is what was asked for.
            self.show_agent_session(workspace, *key);
            return;
        }
        // The stored binding: what the claim's agent ran last, and the
        // conversation it had — captured when its program exited.
        if let Ok(Some((program_id, session_id))) =
            self.db.bound_session(workspace.project.id, claim)
        {
            let bound = workspace
                .tabs
                .borrow()
                .keys()
                .filter(|key| key.slot == Slot::Agent)
                .copied()
                .find(|key| {
                    workspace
                        .tab(*key)
                        .is_some_and(|p| p.program_id == program_id)
                });
            let key = bound.unwrap_or_else(|| workspace.resolve_tab(TabKey::first(Slot::Agent)));
            if bound.is_none() {
                // No tab of that program: open one on that conversation.
                if self
                    .ensure_primitive(
                        workspace,
                        key,
                        Some(&program_id),
                        Resume::Session(session_id),
                    )
                    .is_some()
                {
                    self.show_agent_session(workspace, key);
                } else {
                    self.toast("No agent installed — set one in Preferences");
                }
                return;
            }
            self.relaunch_agent(workspace, key, Resume::Session(session_id));
            return;
        }
        // No binding: the claim's program, then any agent tab, then a new
        // one — each reopened on the project's last conversation.
        let key = candidates
            .first()
            .map(|(key, ..)| *key)
            .unwrap_or_else(|| workspace.resolve_tab(TabKey::first(Slot::Agent)));
        match workspace.tab(key) {
            Some(_) => self.relaunch_agent(workspace, key, Resume::Last),
            None => {
                if self
                    .ensure_primitive(workspace, key, None, Resume::Last)
                    .is_some()
                {
                    self.show_agent_session(workspace, key);
                } else {
                    self.toast("No agent installed — set one in Preferences");
                }
            }
        }
    }

    /// Open one sidebar session row, whatever kind it is: a live Radar pane,
    /// an external terminal, or a catalog conversation to resume.
    fn open_catalog_session(&self, project_id: i64, identity: &str) {
        let session = self
            .agent_sessions
            .borrow()
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
        // History: resume the exact recorded conversation. Without the
        // provider's own session id there is no honest reopen — "last" could
        // be a different conversation than the row the user clicked.
        let Some(program) = programs::by_id(&session.program_id) else {
            self.toast("The session's agent is not installed");
            return;
        };
        let resume = match &session.provider_session_id {
            Some(id) if !program.resume_session.is_empty() => Resume::Session(id.clone()),
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
        options.agent_instance = Some(programs::launch::now_stamp());
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
            self.toast("The board has its own full-workspace panel");
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
            self.toast("The board has its own full-workspace panel");
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
        self.apply_status_labels();
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

    /// Return from the board to the saved tool arrangement.
    fn leave_board(&self, workspace: &Rc<Workspace>) -> bool {
        let key = TabKey::first(Slot::Board);
        if workspace.group_of(key).is_none() {
            return false;
        }
        self.toggle_primitive(workspace, key);
        true
    }

    /// Arrange the panes: agent on the left, changes and editor stacked beside
    /// it, commands along the bottom. Whatever is not open is not there.
    fn layout(&self, workspace: &Rc<Workspace>) {
        workspace.dividers.borrow_mut().clear();
        // The board is an attention surface, never a tile. Reuse zoom's saved
        // arrangement so closing it restores the tools and their divider tree.
        // Also migrate older layouts where the board shared a tab group.
        let board_key = TabKey::first(Slot::Board);
        if let Some(mut group) = workspace.group_of(board_key) {
            if group.tabs().len() > 1 {
                if let Some(primitive) = workspace.tab(board_key) {
                    group.remove(board_key);
                    group.rebuild_header();
                    self.refresh_group_menu(&group);
                    let own = Group::new();
                    own.insert(board_key, &primitive.widget, true);
                    own.rebuild_header();
                    self.refresh_group_menu(&own);
                    workspace.push_group(own.clone());
                    group = own;
                }
            }
            let groups = workspace.groups();
            if groups.len() > 1 {
                let tree = workspace.tree.borrow_mut().take();
                *workspace.zoom.borrow_mut() = Some((groups.clone(), tree));
                // Unmount the tool widgets without stopping their sessions.
                for previous in &groups {
                    previous.widget.unparent();
                }
                *workspace.groups.borrow_mut() = vec![group];
            }
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
        if self.leave_board(&workspace) {
            return;
        }
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

    fn toggle_sidebar(&self) {
        let showing = !self.sidebar_shown.get();
        self.splitter.set_start_child(if showing {
            Some(&self.sidebar)
        } else {
            None::<&gtk::Widget>
        });
        self.sidebar_shown.set(showing);
        self.projects_toggle.set_active(showing);
    }

    /// Store the primitives on screen, so the next launch looks the same.
    /// The database snapshot also keeps grouping, active chips and the split
    /// tree so the project comes back in the same arrangement.
    fn persist_primitives(&self, workspace: &Rc<Workspace>) {
        persist_workspace(&self.db, workspace);
        self.refresh_project_agents(workspace.project.id);
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
        let board_available = self.current_workspace().is_some_and(|workspace| {
            crate::board::enabled(&self.db, &workspace.project.path).unwrap_or(true)
        });
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
        self.projects_toggle.set_active(self.sidebar_shown.get());
        self.home_toggle.set_active(self.home_shown.get());
        for (slot, button) in self.toggles.borrow().iter() {
            button.set_active(visible.contains(slot));
            if *slot == Slot::Board {
                button.set_visible(board_available);
            }
            let available = *slot == Slot::Shell
                || (*slot == Slot::Board && board_available)
                || programs::for_slot(*slot, &preferences).is_some();
            button.set_sensitive(available);
        }
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
    let board_key = TabKey::first(Slot::Board);
    let board_open = workspace.group_of(board_key).is_some();
    let (all_groups, tree) = match zoom.as_ref() {
        Some((groups, tree)) => (groups.clone(), tree.clone()),
        None => (workspace.groups(), workspace.tree.borrow().clone()),
    };
    let groups: Vec<Rc<Group>> = all_groups
        .into_iter()
        .filter(|group| !board_open || !group.contains(board_key))
        .collect();
    let tree = tree.and_then(|tree| {
        if board_open {
            tree.prune(&groups)
        } else {
            Some(tree)
        }
    });
    let zoomed = if board_open {
        None
    } else {
        zoom.as_ref()
            .and_then(|_| workspace.groups().first().and_then(group_id))
    };

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
        board_open,
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
    board_open: bool,
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
                .filter(|key| wanted.contains(key) && assigned.insert(*key))
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
    for key in wanted {
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
        board_open,
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
fn wire_sidebar_drop(app: &SharedApp) {
    let target = gtk::DropTarget::new(glib::types::Type::STRING, gtk::gdk::DragAction::MOVE);
    target.set_propagation_phase(gtk::PropagationPhase::Capture);
    let list = app.sidebar_list.clone();
    target.connect_drop(move |_, value, _, _| {
        let Ok(payload) = value.get::<String>() else {
            return false;
        };
        trace(&format!("drop: sidebar got payload={payload}"));
        // Deferred one main-loop turn so the relayout happens after the drag
        // has fully finished — see the note in group.rs's drop handler.
        let list = list.clone();
        let variant = payload.to_variant();
        glib::idle_add_local_once(move || {
            let _ = list.activate_action("win.primitive-split-out", Some(&variant));
        });
        true
    });
    app.sidebar_list
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
    fn board_open_restore_preserves_tool_tree_for_dismissal() {
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
        assert!(plan.board_open);
        assert_eq!(plan.zoomed, None);
        assert_eq!(plan.layout_group_anchors, vec![agent, diff]);
        assert_eq!(
            plan.groups
                .iter()
                .map(|group| group.slots[0])
                .collect::<Vec<_>>(),
            vec![agent, diff, board]
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
    fn legacy_board_zoom_state_migrates_to_explicit_board_open() {
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
        assert!(plan.board_open);
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
