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
mod hud;
mod keynav;
mod pane;
mod primitive;
mod split;
mod style;
mod theme;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::Result;
use gtk::gio;
use gtk::glib;

use crate::config::Paths;
use crate::db::{
    Db, Project, Slot, WorkspaceAxis, WorkspaceGroup, WorkspaceLayout, WorkspaceState,
};
use crate::discover::{self, Candidate};
use crate::programs::{self, CommandSpec, LaunchOptions, Program};

use pane::Pane;
use group::Group;
use primitive::{label_for, Primitive};
pub use theme::Theme;

type SharedDb = Rc<Db>;

/// The four content primitives, in layout order: the agent leads, because that
/// is what the workspace is for.
const PRIMITIVES: [Slot; 5] = [Slot::Agent, Slot::Diff, Slot::Board, Slot::Shell, Slot::Editor];

type ZoomState = (Vec<Rc<Group>>, Option<split::Node<Group>>);

/// Open the app.
pub fn run(paths: Paths, db: Db) -> Result<()> {
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

/// A project's workspace: its primitives, arranged into groups.
///
/// A group is a pane with one header. It holds one primitive by default; drag a
/// header onto another and they share a header, with a chip each to switch. Drag
/// one out again and it becomes its own pane. The layout places groups by what
/// they lead with: agent on the left, changes and editor stacked beside it,
/// commands along the bottom.
struct Workspace {
    project: Project,
    /// Opened primitives, by slot.
    primitives: RefCell<HashMap<Slot, Rc<Primitive>>>,
    /// Which program each slot uses, even before it is opened.
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
    fn primitive(&self, slot: Slot) -> Option<Rc<Primitive>> {
        self.primitives.borrow().get(&slot).cloned()
    }

    fn groups(&self) -> Vec<Rc<Group>> {
        self.groups.borrow().clone()
    }

    fn dividers(&self) -> Vec<gtk::Paned> {
        self.dividers.borrow().clone()
    }

    /// Which pane holds a primitive.
    fn group_of(&self, slot: Slot) -> Option<Rc<Group>> {
        self.groups.borrow().iter().find(|group| group.contains(slot)).cloned()
    }

    fn is_visible(&self, slot: Slot) -> bool {
        self.group_of(slot).is_some()
    }

    /// Every primitive on screen, in layout order.
    fn visible_slots(&self) -> Vec<Slot> {
        let mut all: Vec<Slot> = self
            .groups
            .borrow()
            .iter()
            .flat_map(|group| group.slots())
            .collect();
        all.sort_by_key(|slot| PRIMITIVES.iter().position(|other| other == slot).unwrap_or(9));
        all.dedup();
        all
    }

    /// The primitive a pane leads with: the first one in `PRIMITIVES` it holds.
    fn anchor(group: &Rc<Group>) -> Option<Slot> {
        group
            .slots()
            .into_iter()
            .min_by_key(|slot| PRIMITIVES.iter().position(|other| other == slot).unwrap_or(9))
    }

    fn push_group(&self, group: Rc<Group>) {
        self.groups.borrow_mut().push(group);
    }

    fn forget_group(&self, group: &Rc<Group>) {
        self.groups.borrow_mut().retain(|other| !Rc::ptr_eq(other, group));
    }
}

struct App {
    db: SharedDb,
    theme: RefCell<Theme>,
    window: adw::ApplicationWindow,
    sidebar: gtk::Widget,
    splitter: gtk::Paned,
    sidebar_shown: Cell<bool>,
    sidebar_list: gtk::ListBox,
    sidebar_search: gtk::SearchEntry,
    toggles: RefCell<HashMap<Slot, gtk::ToggleButton>>,
    projects_toggle: gtk::ToggleButton,
    stack: gtk::Stack,
    toasts: adw::ToastOverlay,
    /// The keyboard's own surface: primitives, actions and the keymap,
    /// floating over everything. One per window, not per workspace.
    hud: Rc<hud::Hud>,
    workspaces: RefCell<HashMap<i64, Rc<Workspace>>>,
    projects: RefCell<Vec<Project>>,
    rows: RefCell<Vec<(i64, gtk::ListBoxRow, gtk::Label, gtk::Label)>>,
    status: RefCell<HashMap<i64, crate::git::Status>>,
    status_tx: std::sync::mpsc::Sender<Vec<(i64, crate::git::Status)>>,
    status_rx: RefCell<std::sync::mpsc::Receiver<Vec<(i64, crate::git::Status)>>>,
    /// What each pane's program last said about itself — its name and its own
    /// live title, or its exit — keyed by (project, slot).
    header_info: RefCell<HashMap<(i64, Slot), String>>,
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

type SharedApp = Rc<App>;

fn build_window(app: &adw::Application, paths: &Rc<Paths>, db: &SharedDb) -> adw::ApplicationWindow {
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
    let no_projects = status_page(
        "folder-open-symbolic",
        "No projects yet",
        "Add a directory to get started. A project is an agent, live changes, commands and an editor.",
        Some(("Find projects", "win.find-projects")),
    );
    stack.add_named(&no_projects, Some("_empty"));
    let no_selection = status_page(
        "view-list-symbolic",
        "Nothing selected",
        "Pick a project from the sidebar.",
        None,
    );
    stack.add_named(&no_selection, Some("_none"));

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
    let state = Rc::new(App {
        db: db.clone(),
        theme: RefCell::new(Theme::load()),
        window: window.clone(),
        sidebar: sidebar_box.upcast(),
        splitter,
        sidebar_shown: Cell::new(true),
        sidebar_list,
        sidebar_search: search.clone(),
        toggles: RefCell::new(toggle_buttons),
        projects_toggle: projects_toggle.clone(),
        stack,
        toasts,
        hud: hud.clone(),
        workspaces: RefCell::new(HashMap::new()),
        projects: RefCell::new(Vec::new()),
        rows: RefCell::new(Vec::new()),
        status: RefCell::new(HashMap::new()),
        status_tx,
        status_rx: RefCell::new(status_rx),
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
    let state_for_close = Rc::downgrade(&state);
    window.connect_close_request(move |window| {
        if let Some(state) = state_for_close.upgrade() {
            state.persist_workspaces();
        }
        window.set_visible(false);
        glib::Propagation::Stop
    });
    start_status_drainer(&state);
    start_find_drainer(&state);
    wire_sidebar_drop(&state);
    watch_theme(&state);
    state.find_root_button.set_label(&format!(
        "from {}",
        crate::db::abbreviate(&state.find_root.borrow())
    ));
    App::refresh_projects(&state);
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
                state_for_group.show_primitive(&workspace, Slot::Diff);
                state_for_group.group_into(&workspace, Slot::Diff, Slot::Agent);
            }
        });
    }
    if std::env::var("RADAR_PRIMITIVES").is_ok() {
        let state_for_all = state.clone();
        glib::timeout_add_local_once(Duration::from_millis(1200), move || {
            if let Some(workspace) = state_for_all.current_workspace() {
                for slot in PRIMITIVES {
                    state_for_all.show_primitive(&workspace, slot);
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
            let Some(id) = app.id_for_row(row) else { return };
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
            app_for_motion.pointer_motion_ms.set(glib::monotonic_time() / 1000);
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
                        app.find_root_button.set_label(&format!(
                            "from {}",
                            crate::db::abbreviate(&path)
                        ));
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

fn watch_theme(app: &SharedApp) {
    let Some(dir) = theme::omarchy_theme_dir() else {
        return;
    };
    let file = gio::File::for_path(dir.join("colors.toml"));
    let Ok(monitor) = file.monitor_file(gio::FileMonitorFlags::NONE, None::<&gio::Cancellable>) else {
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
        add("find-projects", Box::new(move || app.focus_projects_search()));
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
                let Some(project) = app.current_project() else { return };
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
                let Some(project) = app.current_project() else { return };
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
                let Some(project) = app.current_project() else { return };
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
                let Some(project) = app.current_project() else { return };
                let spec = CommandSpec {
                    argv: program.command_spec(&LaunchOptions::default()).argv,
                    env_unset: Vec::new(),
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
            app_for_action.toggle_primitive(&workspace, Slot::parse(&name));
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
            app_for_action.activate_primitive(&workspace, Slot::parse(&name));
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
            app_for_action.hover_primitive(&workspace, Slot::parse(&name));
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
            app_for_action.group_into(&workspace, Slot::parse(&pair.0), Slot::parse(&pair.1));
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
            app_for_action.split_out(&workspace, Slot::parse(&name));
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
                Slot::parse(&triple.0),
                Slot::parse(&triple.1),
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
            let slot = Slot::parse(&name);
            if app_for_action.current_workspace().is_none() {
                return;
            }
            app_for_action.hud.present_programs(&app_for_action, slot);
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
                let Some(slot) = app
                    .focused_group(&workspace)
                    .and_then(|group| group.active_slot())
                else {
                    return;
                };
                app.hud.present_programs(&app, slot);
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
            app_for_action.close_pane(&workspace, Slot::parse(&name));
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
            let slot = Slot::parse(&name);
            if workspace.is_visible(slot) {
                app_for_action.toggle_primitive(&workspace, slot);
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
            app_for_action.move_pane(&workspace, Slot::parse(&name), delta);
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
            app_for_action.toggle_zoom(Some(Slot::parse(&name)));
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
            let slot = Slot::parse(&name);
            if let Some(workspace) = app_for_action.current_workspace() {
                app_for_action.activate_primitive(&workspace, slot);
            }
        });
        app.window.add_action(&action);
    }
    {
        // A pane's program reported live state — its own title, or its exit.
        // Aim it at the pane's header, wherever the pane is grouped today.
        // Empty text clears: the header goes back to just the chips.
        let action = gio::SimpleAction::new(
            "pane-info",
            Some(glib::VariantTy::new("(xss)").unwrap()),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some((project_id, slot, text)) =
                parameter.and_then(|value| value.get::<(i64, String, String)>())
            else {
                return;
            };
            let text = if text.is_empty() { None } else { Some(text) };
            app_for_action.store_header_info(project_id, Slot::parse(&slot), text);
        });
        app.window.add_action(&action);
    }
    {
        // The terminal bell — how agent CLIs ask for attention. The pane's
        // header marks it until that pane is looked at.
        let action = gio::SimpleAction::new(
            "pane-bell",
            Some(glib::VariantTy::new("(xs)").unwrap()),
        );
        let app_for_action = app.clone();
        action.connect_activate(move |_, parameter| {
            let Some((project_id, slot)) =
                parameter.and_then(|value| value.get::<(i64, String)>())
            else {
                return;
            };
            let slot = Slot::parse(&slot);
            for workspace in app_for_action.workspaces.borrow().values() {
                if workspace.project.id != project_id {
                    continue;
                }
                if let Some(group) = workspace.group_of(slot) {
                    group.set_attention(slot);
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

    // ---- keyboard ----
    // Alt is radar's only modifier, so every Ctrl chord reaches the programs
    // in the panels the way their authors wrote them. The one exception is
    // cycling: the window manager owns Alt+Tab, so the cycle stays on Ctrl.
    let accels: [(&str, &[&str]); 17] = [
        ("win.find-projects", &["<Alt>n"]),
        ("win.preferences", &["<Alt>comma"]),
        ("win.refresh", &["<Alt>r"]),
        ("win.quit", &["<Alt>q"]),
        ("win.toggle-sidebar", &["<Alt>b"]),
        ("win.zoom", &["<Alt>f"]),
        ("win.hud", &["<Alt>h"]),
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
                .and_then(|group| group.active_slot());
            if let Some(primitive) = first.and_then(|slot| workspace.primitive(slot)) {
                primitive.focus();
            }
        }
    }

    /// A pane's own menu: only operations that act on this pane or its active
    /// primitive. Other primitives are opened from the dock or the HUD.
    fn primitive_menu_model(&self, slot: Slot) -> gio::Menu {
        let menu = gio::Menu::new();
        if slot != Slot::Board {
            let program = gio::Menu::new();
            program.append_item(&item(
                &format!("Change {} program…", label_for(slot)),
                &format!("win.primitive-program::{}", slot.as_str()),
            ));
            menu.append_section(None, &program);
        }

        let Some(workspace) = self.current_workspace() else {
            return menu;
        };
        let Some(group) = workspace.group_of(slot) else {
            return menu;
        };

        let group_actions = gio::Menu::new();
        let others: Vec<Slot> = workspace
                .visible_slots()
                .into_iter()
                .filter(|other| *other != slot && *other != Slot::Custom)
                .collect();
        for other in others {
            let entry = gio::MenuItem::new(
                Some(&format!("Group with {}", label_for(other))),
                None,
            );
            entry.set_action_and_target_value(
                Some("win.primitive-group"),
                Some(&(slot.as_str().to_string(), other.as_str().to_string()).to_variant()),
            );
            group_actions.append_item(&entry);
        }
        if group.slots().len() > 1 {
            let entry = gio::MenuItem::new(Some("Split out into its own pane"), None);
            entry.set_action_and_target_value(
                Some("win.primitive-split-out"),
                Some(&slot.as_str().to_variant()),
            );
            group_actions.append_item(&entry);
        }
        if group_actions.n_items() > 0 {
            menu.append_section(Some("Group"), &group_actions);
        }

        let panel = gio::Menu::new();
        let close = gio::MenuItem::new(Some("Close pane"), None);
        close.set_action_and_target_value(
            Some("win.pane-close"),
            Some(&slot.as_str().to_variant()),
        );
        panel.append_item(&close);
        if group.slots().len() > 1 {
            let close_primitive = gio::MenuItem::new(
                Some(&format!("Close {}", label_for(slot))),
                None,
            );
            close_primitive.set_action_and_target_value(
                Some("win.primitive-close"),
                Some(&slot.as_str().to_variant()),
            );
            panel.append_item(&close_primitive);
        }

        let ordered = self.ordered_groups(&workspace);
        if let Some(index) = ordered.iter().position(|candidate| Rc::ptr_eq(candidate, &group)) {
            if index > 0 {
                panel.append_item(&item(
                    "Move up",
                    &format!("win.pane-move-up::{}", slot.as_str()),
                ));
            }
            if index + 1 < ordered.len() {
                panel.append_item(&item(
                    "Move down",
                    &format!("win.pane-move-down::{}", slot.as_str()),
                ));
            }
        }
        let zoom = gio::MenuItem::new(Some("Zoom pane (Alt+F)"), None);
        zoom.set_action_and_target_value(
            Some("win.pane-zoom"),
            Some(&slot.as_str().to_variant()),
        );
        panel.append_item(&zoom);
        menu.append_section(Some("Pane"), &panel);
        menu
    }

    /// Which project does a sidebar row belong to?
    fn id_for_row(&self, row: &gtk::ListBoxRow) -> Option<i64> {
        self.rows
            .borrow()
            .iter()
            .find(|(_, widget, _, _)| widget == row)
            .map(|(id, _, _, _)| *id)
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
        dialog.choose(Some(&self.window), None::<&gio::Cancellable>, move |result| {
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
        });
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
            for primitive in workspace.primitives.borrow().values() {
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
        }
    }

    // ---- projects ----

    fn refresh_projects(app: &SharedApp) {
        let projects = app.db.projects().unwrap_or_default();
        let selected = *app.current.borrow();
        *app.projects.borrow_mut() = projects.clone();

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
            app.show_placeholder();
            return;
        }

        for project in &projects {
            let (row, summary, badge) = app.build_project_row(project);
            app.rows
                .borrow_mut()
                .push((project.id, row.clone(), summary, badge));
            app.sidebar_list.append(&row);
        }
        app.filter_sidebar(&app.sidebar_search.text());
        // Candidate directories for the query go under the project rows.
        App::show_candidates(app, &app.sidebar_search.text());
        app.apply_status_labels();
        app.refresh_status();

        if let Some(id) = selected {
            app.select_row_for(id);
        }
        if selected.is_none() || selected.is_some_and(|id| !projects.iter().any(|p| p.id == id)) {
            if let Some(first) = projects.first() {
                app.select_project(first.id);
            }
        }
    }

    fn build_project_row(&self, project: &Project) -> (gtk::ListBoxRow, gtk::Label, gtk::Label) {
        let row = gtk::ListBoxRow::new();

        // Inset and rounded corners come from the stylesheet; the row only lays
        // out its content: icon, name, summary, badge, trash.
        let missing = project.is_missing();
        let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 8);

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
        icon.set_pixel_size(16);
        icon.set_valign(gtk::Align::Center);
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

        // Remove, per row: the id travels with the action so no state is
        // needed. The stylesheet reveals it on hover or keyboard focus.
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

        row.set_child(Some(&box_));
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
        (row, summary, badge)
    }

    fn apply_status_labels(&self) {
        let projects = self.projects.borrow();
        for (id, _row, summary, badge) in self.rows.borrow().iter() {
            let Some(project) = projects.iter().find(|p| p.id == *id) else {
                continue;
            };
            let parent = project
                .path
                .parent()
                .map(crate::db::abbreviate)
                .unwrap_or_else(|| project.display_path());
            if project.is_missing() {
                summary.set_text(&format!("missing  ·  {parent}"));
                badge.set_visible(false);
                continue;
            }
            match self.status.borrow().get(id) {
                Some(status) => {
                    summary.set_text(&format!("{}  ·  {parent}", status.summary()));
                }
                None => summary.set_text(&format!("…  ·  {parent}")),
            }
            let count = self.workspaces.borrow().get(id).map_or(0, |workspace| {
                workspace
                    .primitives
                    .borrow()
                    .values()
                    .filter(|primitive| {
                        primitive
                            .pane
                            .as_ref()
                            .is_some_and(|pane| pane.is_live())
                    })
                    .count()
            });
            badge.set_text(&count.to_string());
            badge.set_visible(count > 0);
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

    fn select_row_for(&self, id: i64) {
        let target = self
            .rows
            .borrow()
            .iter()
            .find(|(row_id, _, _, _)| *row_id == id)
            .map(|(_, row, _, _)| row.clone());
        if let Some(row) = target {
            self.sidebar_list.select_row(Some(&row));
        }
    }

    fn filter_sidebar(&self, query: &str) {
        let matcher = fuzzy_matcher::skim::SkimMatcherV2::default().ignore_case();
        use fuzzy_matcher::FuzzyMatcher;
        let projects = self.projects.borrow();
        for (id, row, _, _) in self.rows.borrow().iter() {
            let Some(project) = projects.iter().find(|p| p.id == *id) else {
                continue;
            };
            let visible = query.trim().is_empty()
                || matcher.fuzzy_match(&project.name, query).is_some()
                || matcher.fuzzy_match(&project.display_path(), query).is_some();
            row.set_visible(visible);
        }
    }

    fn show_placeholder(&self) {
        let name = if self.projects.borrow().is_empty() {
            "_empty"
        } else {
            "_none"
        };
        self.stack.set_visible_child_name(name);
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
            let (item, add) =
                candidate_row(&candidate.name, &candidate.display_path(), candidate.is_repo);
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
        self.workspace_for(&project);
        self.stack.set_visible_child_name(&format!("project-{id}"));
        let _ = self.db.touch_project(id);
        let _ = self.db.remember_last_project(Some(id));
        // Starting a project starts its board: BOARD.md is there before any
        // agent looks for it.
        let _ = crate::board::ensure_file(&project.path);
        self.sync_toggles();
        self.refresh_menus();
        self.select_row_for(id);
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
            primitives: RefCell::new(HashMap::new()),
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
        let restore = self
            .db
            .ui_prefs()
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
        let preferences = self.db.preferences().unwrap_or_default();
        let mut wanted: Vec<Slot> = Vec::new();
        if stored.is_empty() {
            if saved_state.is_none() {
                // The agent is the point of a new workspace; everything else
                // is one keystroke away. An empty saved snapshot means the user
                // intentionally hid every primitive.
                wanted.push(Slot::Agent);
                if let Some(program) = programs::for_slot(Slot::Agent, &preferences) {
                    workspace
                        .programs
                        .borrow_mut()
                        .insert(Slot::Agent, program.id.clone());
                }
            }
        } else {
            for tab in &stored {
                workspace
                    .programs
                    .borrow_mut()
                    .insert(tab.slot, tab.program_id.clone());
                wanted.push(tab.slot);
            }
        }
        wanted.sort_by_key(|slot| PRIMITIVES.iter().position(|other| other == slot).unwrap_or(9));
        wanted.dedup();

        if let Some(state) = &saved_state {
            *workspace.positions.borrow_mut() = state.positions.clone();
        }

        // Create each visible primitive before rebuilding the groups that refer
        // to it. VTE starts a child only when its terminal is mapped.
        for slot in &wanted {
            let _ = self.ensure_primitive(&workspace, *slot);
        }

        let mut groups = Vec::new();
        let mut assigned = HashSet::new();
        if let Some(state) = &saved_state {
            for saved_group in &state.groups {
                let slots: Vec<Slot> = saved_group
                    .slots
                    .iter()
                    .copied()
                    .filter(|slot| {
                        wanted.contains(slot)
                            && workspace.primitive(*slot).is_some()
                            && assigned.insert(*slot)
                    })
                    .collect();
                if slots.is_empty() {
                    continue;
                }
                let group = Group::new();
                for slot in &slots {
                    if let Some(primitive) = workspace.primitive(*slot) {
                        group.insert(*slot, &primitive.widget, false);
                    }
                }
                if slots.contains(&saved_group.active) {
                    group.activate(saved_group.active);
                }
                group.rebuild_header();
                self.refresh_group_menu(&group);
                groups.push(group);
            }
        }
        for slot in wanted {
            if assigned.insert(slot) {
                let Some(primitive) = workspace.primitive(slot) else {
                    continue;
                };
                let group = Group::new();
                group.insert(slot, &primitive.widget, true);
                group.rebuild_header();
                self.refresh_group_menu(&group);
                groups.push(group);
            }
        }
        *workspace.groups.borrow_mut() = groups.clone();

        if let Some(state) = &saved_state {
            if let Some(layout) = state.layout.as_ref() {
                let restored = restore_layout(layout, &groups);
                if restored
                    .as_ref()
                    .is_some_and(|tree| layout_covers(tree, &groups))
                {
                    *workspace.tree.borrow_mut() = restored;
                }
            }
            if let Some(zoomed) = state.zoomed {
                if groups.len() > 1 {
                    if let Some(group) = groups
                        .iter()
                        .find(|group| group_id(group) == Some(zoomed))
                    {
                        let tree = workspace.tree.borrow_mut().take();
                        *workspace.zoom.borrow_mut() = Some((groups.clone(), tree));
                        *workspace.groups.borrow_mut() = vec![group.clone()];
                    }
                }
            }
        }

        self.layout(&workspace);
        self.sync_toggles();
        self.persist_primitives(&workspace);
        workspace
    }

    /// Open a primitive's program if it is not open yet.
    fn ensure_primitive(&self, workspace: &Rc<Workspace>, slot: Slot) -> Option<Rc<Primitive>> {
        if let Some(existing) = workspace.primitive(slot) {
            return Some(existing);
        }
        // The board is the one primitive that runs nothing: the pane is
        // radar's own widget over the project's BOARD.md.
        if slot == Slot::Board {
            let pane = board::BoardPane::new(&workspace.project.path, &self.window);
            // The board's header lives on the board's own counts.
            let window = self.window.clone();
            let project_id = workspace.project.id;
            pane.set_info_observer(move |text| {
                let _ = gtk::prelude::WidgetExt::activate_action(
                    &window,
                    "win.pane-info",
                    Some(
                        &(project_id, Slot::Board.as_str(), text.unwrap_or_default())
                            .to_variant(),
                    ),
                );
            });
            workspace
                .programs
                .borrow_mut()
                .insert(slot, "board".to_string());
            let primitive = Primitive::builtin(
                "board",
                pane.widget().clone(),
                "Board\nbuilt into radar — the project's BOARD.md",
            );
            workspace
                .primitives
                .borrow_mut()
                .insert(slot, primitive.clone());
            return Some(primitive);
        }
        let preferences = self.db.preferences().unwrap_or_default();
        let wanted = workspace.programs.borrow().get(&slot).cloned();
        let program = wanted
            .and_then(|id| programs::by_id(&id))
            .filter(|program| program.installed())
            .or_else(|| programs::for_slot(slot, &preferences))?;

        workspace
            .programs
            .borrow_mut()
            .insert(slot, program.id.clone());
        let options = self.launch_options();
        let spec = program.command_spec(&options);
        let theme = self.theme.borrow().clone();
        let pane = Rc::new(Pane::spawn(
            &spec,
            &workspace.project.path,
            &theme,
            label_for(slot),
            pane::ShiftEnter::for_slot(slot),
        ));
        // The pane's header wants the program's live self-description: its
        // name plus whatever it puts in the terminal title, and its exit when
        // it goes away. The window action routes it — the pane outlives any
        // one group, so the observer aims at the action, not at a header.
        let window = self.window.clone();
        let project_id = workspace.project.id;
        let name = program.name.clone();
        pane.set_info_observer(move |text| {
            let info = text.map(|text| format!("{name} · {text}"));
            let _ = gtk::prelude::WidgetExt::activate_action(
                &window,
                "win.pane-info",
                Some(&(project_id, slot.as_str(), info.unwrap_or_default()).to_variant()),
            );
        });
        let window = self.window.clone();
        pane.set_bell_observer(move || {
            let _ = gtk::prelude::WidgetExt::activate_action(
                &window,
                "win.pane-bell",
                Some(&(project_id, slot.as_str()).to_variant()),
            );
        });
        let primitive = Primitive::new(&program, pane);
        workspace
            .primitives
            .borrow_mut()
            .insert(slot, primitive.clone());
        Some(primitive)
    }

    /// Show or hide a primitive. Hiding never stops the program.
    fn toggle_primitive(&self, workspace: &Rc<Workspace>, slot: Slot) {
        if slot == Slot::Custom {
            return;
        }
        self.restore_zoom(workspace);
        let opening = workspace.group_of(slot).is_none();
        if let Some(group) = workspace.group_of(slot) {
            group.remove(slot);
            if group.is_empty() {
                workspace.forget_group(&group);
            } else {
                self.refresh_group_menu(&group);
            }
            group.rebuild_header();
        } else {
            let Some(primitive) = self.ensure_primitive(workspace, slot) else {
                self.toast(&format!(
                    "No {} installed — set one in Preferences",
                    label_for(slot).to_lowercase()
                ));
                return;
            };
            let group = Group::new();
            group.insert(slot, &primitive.widget, true);
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
            if let Some(primitive) = workspace.primitive(slot) {
                primitive.focus();
            }
        }
    }

    /// Close every primitive in a pane while leaving their programs available
    /// to reopen from the dock or the HUD.
    fn close_pane(&self, workspace: &Rc<Workspace>, slot: Slot) {
        self.restore_zoom(workspace);
        let Some(group) = workspace.group_of(slot) else {
            return;
        };
        for member in group.slots() {
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

    /// Show a primitive without hiding it when it is already on screen.
    fn show_primitive(&self, workspace: &Rc<Workspace>, slot: Slot) {
        if !workspace.is_visible(slot) {
            self.toggle_primitive(workspace, slot);
        }
    }

    /// Clicking a chip: switch that pane to the primitive.
    fn activate_primitive(&self, workspace: &Rc<Workspace>, slot: Slot) {
        if let Some(group) = workspace.group_of(slot) {
            group.activate(slot);
            self.refresh_group_menu(&group);
            if let Some(primitive) = workspace.primitive(slot) {
                primitive.focus();
            }
            self.persist_primitives(workspace);
            return;
        }
        self.show_primitive(workspace, slot);
        if let Some(group) = workspace.group_of(slot) {
            group.activate(slot);
            self.refresh_group_menu(&group);
            if let Some(primitive) = workspace.primitive(slot) {
                primitive.focus();
            }
            self.persist_primitives(workspace);
        }
    }

    /// Hovering a member selects its content and moves keyboard focus into that
    /// primitive, the same focus-follows-pointer behavior as hovering a pane.
    fn hover_primitive(&self, workspace: &Rc<Workspace>, slot: Slot) {
        // Hover is mouse intent: a pane mapped under a parked pointer
        // synthesizes an enter the moment it appears — project switches and
        // resizes produce those by the dozen — and acting on one would yank
        // the keys out of whatever the user is navigating, the sidebar while
        // picking a project. Focus follows a moving mouse, not an appearing
        // pane.
        if !self.pointer_is_live() {
            return;
        }
        let Some(group) = workspace.group_of(slot) else {
            return;
        };
        if group.active_slot() != Some(slot) {
            group.activate(slot);
            self.refresh_group_menu(&group);
            self.persist_primitives(workspace);
        }
        if let Some(primitive) = workspace.primitive(slot) {
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

    /// Drop one primitive onto another's header: they share that header.
    fn group_into(&self, workspace: &Rc<Workspace>, source: Slot, target: Slot) {
        if source == target || source == Slot::Custom {
            return;
        }
        let Some(primitive) = self.ensure_primitive(workspace, source) else {
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

    /// Pull a primitive out of a shared header into its own pane.
    fn split_out(&self, workspace: &Rc<Workspace>, slot: Slot) {
        let Some(group) = workspace.group_of(slot) else {
            return;
        };
        if group.slots().len() < 2 {
            return; // already its own pane
        }
        let Some(primitive) = self.ensure_primitive(workspace, slot) else {
            return;
        };
        group.remove(slot);
        group.rebuild_header();
        let own = Group::new();
        own.insert(slot, &primitive.widget, true);
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
                    format!("tree-split-{}", slot.as_str()),
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
        trace(&format!("split out: {}", slot.as_str()));
    }

    /// Move a pane one place in the visible arrangement while preserving the
    /// existing divider shape and sizes.
    fn move_pane(&self, workspace: &Rc<Workspace>, slot: Slot, delta: isize) {
        let Some(group) = workspace.group_of(slot) else {
            return;
        };
        let ordered = self.ordered_groups(workspace);
        let Some(index) = ordered.iter().position(|candidate| Rc::ptr_eq(candidate, &group)) else {
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
    fn nest_split(&self, workspace: &Rc<Workspace>, dragged: Slot, target: Slot, zone: &str) {
        if dragged == target || dragged == Slot::Custom {
            return;
        }
        // The dragged primitive must lead its own pane before it can take a
        // half of someone else's.
        if let Some(group) = workspace.group_of(dragged) {
            if group.slots().len() >= 2 {
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
                (split::Node::leaf(&dragged_group), split::Node::leaf(&target_group))
            } else {
                (split::Node::leaf(&target_group), split::Node::leaf(&dragged_group))
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

    /// Replace the program behind a primitive.
    fn set_primitive_program(
        &self,
        workspace: &Rc<Workspace>,
        slot: Slot,
        program: Program,
        visible: bool,
    ) {
        // Drop the old pane: its process belongs to the program being replaced.
        if let Some(old) = workspace.primitives.borrow_mut().remove(&slot) {
            old.widget.unparent();
            if let Some(group) = workspace.group_of(slot) {
                group.remove(slot);
                if group.is_empty() {
                    workspace.forget_group(&group);
                }
                group.rebuild_header();
            }
        }
        workspace
            .programs
            .borrow_mut()
            .insert(slot, program.id.clone());
        if visible {
            self.show_primitive(workspace, slot);
        } else {
            self.layout(workspace);
            self.sync_toggles();
            self.refresh_workspace_menus(workspace);
            self.persist_primitives(workspace);
        }
        self.toast(&format!("{} → {}", label_for(slot), program.name));
        self.apply_status_labels();
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
                    .slots()
                    .iter()
                    .map(|slot| slot.as_str())
                    .collect::<Vec<_>>()
                    .join("+"))
                .collect::<Vec<_>>()
        ));
    }

    /// Turn an arrangement node into widgets.
    fn build_tree(&self, workspace: &Rc<Workspace>, node: &split::Node<Group>) -> gtk::Widget {
        match node {
            split::Node::Leaf(group) => group.widget.clone().upcast::<gtk::Widget>(),
            split::Node::Split { axis, ratio, key, first, second } => {
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
    fn toggle_zoom(&self, slot: Option<Slot>) {
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
        let target = slot
            .and_then(|slot| workspace.group_of(slot))
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

    /// Make the dock match the layout.
    fn sync_toggles(&self) {
        let visible = self
            .current_workspace()
            .map(|workspace| workspace.visible_slots())
            .unwrap_or_default();
        let preferences = self.db.preferences().unwrap_or_default();
        self.projects_toggle.set_active(self.sidebar_shown.get());
        for (slot, button) in self.toggles.borrow().iter() {
            button.set_active(visible.contains(slot));
            let available = *slot == Slot::Shell
                || *slot == Slot::Board
                || programs::for_slot(*slot, &preferences).is_some();
            button.set_sensitive(available);
        }
    }

    /// The pane menu belongs to whichever primitive its header is showing.
    fn refresh_group_menu(&self, group: &Rc<Group>) {
        let Some(slot) = group.active_slot() else {
            return;
        };
        group
            .menu_button
            .set_menu_model(Some(&self.primitive_menu_model(slot)));
        // A pane rebuilt into a new group starts with a blank header; restore
        // whatever each member's program has said so far.
        for workspace in self.workspaces.borrow().values() {
            for member in group.slots() {
                let home = workspace.group_of(member);
                if home.is_some_and(|home| Rc::ptr_eq(&home, group)) {
                    if let Some(text) =
                        self.header_info.borrow().get(&(workspace.project.id, member))
                    {
                        group.set_member_info(member, text);
                    }
                }
            }
        }
    }

    /// Remember what a pane's program said and aim it at the pane's header,
    /// wherever that pane is grouped today. `None` clears.
    fn store_header_info(&self, project_id: i64, slot: Slot, text: Option<String>) {
        {
            let mut info = self.header_info.borrow_mut();
            match &text {
                Some(entry) => {
                    info.insert((project_id, slot), entry.clone());
                }
                None => {
                    info.remove(&(project_id, slot));
                }
            }
        }
        let shown = text.unwrap_or_default();
        for workspace in self.workspaces.borrow().values() {
            if workspace.project.id != project_id {
                continue;
            }
            if let Some(group) = workspace.group_of(slot) {
                group.set_member_info(slot, &shown);
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

/// Save one project's primitives and presentation state to SQLite.
fn persist_workspace(db: &Db, workspace: &Workspace) {
    let zoom = workspace.zoom.borrow();
    let (groups, tree) = match zoom.as_ref() {
        Some((groups, tree)) => (groups.clone(), tree.clone()),
        None => (workspace.groups(), workspace.tree.borrow().clone()),
    };
    let zoomed = zoom
        .as_ref()
        .and_then(|_| workspace.groups().first().and_then(group_id));

    let mut slots: Vec<Slot> = groups.iter().flat_map(|group| group.slots()).collect();
    slots.sort_by_key(|slot| {
        PRIMITIVES
            .iter()
            .position(|other| other == slot)
            .unwrap_or(9)
    });
    slots.dedup();

    let mut tabs = Vec::new();
    for (index, slot) in slots.iter().enumerate() {
        let program_id = workspace
            .primitive(*slot)
            .map(|primitive| primitive.program_id.clone())
            .or_else(|| workspace.programs.borrow().get(slot).cloned());
        let Some(program_id) = program_id else {
            continue;
        };
        let mut tab = crate::db::Tab::new(*slot, program_id);
        tab.sort_order = index as i64;
        tabs.push(tab);
    }

    let saved_groups = groups
        .iter()
        .filter_map(|group| {
            let slots = group.slots();
            Some(WorkspaceGroup {
                active: group.active_slot()?,
                slots,
            })
        })
        .collect();
    let state = WorkspaceState {
        groups: saved_groups,
        layout: tree.as_ref().and_then(save_layout),
        programs: workspace.programs.borrow().clone(),
        positions: workspace.positions.borrow().clone(),
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

fn group_id(group: &Rc<Group>) -> Option<Slot> {
    group.slots().first().copied()
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
        } => {
            if !ratio.is_finite() || !(0.05..=0.95).contains(ratio) {
                return None;
            }
            Some(split::Node::split(
                match axis {
                    WorkspaceAxis::Horizontal => split::Axis::Horizontal,
                    WorkspaceAxis::Vertical => split::Axis::Vertical,
                },
                *ratio,
                key.clone(),
                restore_layout(first, groups)?,
                restore_layout(second, groups)?,
            ))
        }
    }
}

fn layout_covers(node: &split::Node<Group>, groups: &[Rc<Group>]) -> bool {
    let leaves = node.leaves();
    leaves.len() == groups.len()
        && groups
            .iter()
            .all(|group| leaves.iter().any(|leaf| Rc::ptr_eq(group, leaf)))
}

/// The classic arrangement as a tree: agent-anchored main pane on the left,
/// the rest stacked on the side, shell panes along the bottom. Mirrors the
/// ratios and divider keys this used to build directly, so saved divider
/// positions keep working in auto mode.
fn auto_node(groups: &[Rc<Group>]) -> Option<split::Node<Group>> {
    let bottom: Vec<Rc<Group>> = groups
        .iter()
        .filter(|group| Workspace::anchor(group) == Some(Slot::Shell))
        .cloned()
        .collect();
    let rest: Vec<Rc<Group>> = groups
        .iter()
        .filter(|group| !bottom.iter().any(|other| Rc::ptr_eq(other, group)))
        .cloned()
        .collect();
    let main_left = rest
        .iter()
        .find(|group| Workspace::anchor(group) == Some(Slot::Agent))
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
        (Some(main), Some(bottom)) => {
            Some(split::Node::split(split::Axis::Vertical, 0.68, "outer", main, bottom))
        }
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
            split::Node::split(axis, ratio, format!("{key}{index}"), split::Node::leaf(group), acc)
        } else {
            split::Node::split(axis, ratio, format!("{key}{index}"), acc, split::Node::leaf(group))
        };
    }
    Some(acc)
}

/// Dropping a pane on the project list pulls that primitive into a pane of its
/// own: the natural counter-gesture to dropping it onto another pane.
fn wire_sidebar_drop(app: &SharedApp) {
    let target = gtk::DropTarget::new(
        glib::types::Type::STRING,
        gtk::gdk::DragAction::MOVE,
    );
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
