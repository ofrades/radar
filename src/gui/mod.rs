//! The app window.
//!
//! A project workspace is four primitives — editor, agent, diff, terminal — and
//! nothing else. No tabs: the sidebar's four icons decide which primitives are on
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

mod dialogs;
mod pane;
mod primitive;
mod style;
mod theme;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::Result;
use gtk::gio;
use gtk::glib;

use crate::config::Paths;
use crate::db::{Db, Project, Slot};
use crate::programs::{self, CommandSpec, LaunchOptions, Program};

use pane::Pane;
use primitive::{label_for, Primitive};
pub use theme::Theme;

type SharedDb = Rc<Db>;

/// The four content primitives, in layout order: the agent leads, because that
/// is what the workspace is for.
const PRIMITIVES: [Slot; 4] = [Slot::Agent, Slot::Diff, Slot::Shell, Slot::Editor];

/// Open the app.
pub fn run(paths: Paths, db: Db) -> Result<()> {
    let app = adw::Application::builder()
        .application_id("dev.omarchy.Radar")
        .build();
    let paths = Rc::new(paths);
    let db = Rc::new(db);

    app.connect_startup(|_| style::install());
    app.connect_activate(move |app| {
        let window = build_window(app, &paths, &db);
        window.present();
    });

    // A clean argv: GApplication would otherwise try to interpret radar's own
    // command line.
    let _ = app.run_with_args(&["radar"]);
    Ok(())
}

/// A project's workspace: its primitives, and which are on screen.
struct Workspace {
    project: Project,
    /// Opened primitives, by slot.
    primitives: RefCell<HashMap<Slot, Rc<Primitive>>>,
    /// Which program each slot uses, even before it is opened.
    programs: RefCell<HashMap<Slot, String>>,
    /// The primitives on screen, in layout order.
    visible: RefCell<Vec<Slot>>,
    /// Split positions the user dragged, keyed by layout signature.
    positions: RefCell<HashMap<String, i32>>,
    /// Holds exactly one child: the layout built from `visible`.
    holder: gtk::Box,
    /// The set of visible primitives to return to after a zoom.
    zoom: RefCell<Option<Vec<Slot>>>,
}

impl Workspace {
    fn primitive(&self, slot: Slot) -> Option<Rc<Primitive>> {
        self.primitives.borrow().get(&slot).cloned()
    }

    fn is_visible(&self, slot: Slot) -> bool {
        self.visible.borrow().contains(&slot)
    }

    /// The primitives on screen.
    fn visible_slots(&self) -> Vec<Slot> {
        self.visible.borrow().clone()
    }

    /// Sort a visible set into layout order.
    fn ordered(slots: &mut Vec<Slot>) {
        slots.sort_by_key(|slot| PRIMITIVES.iter().position(|other| other == slot).unwrap_or(9));
        slots.dedup();
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
    workspaces: RefCell<HashMap<i64, Rc<Workspace>>>,
    projects: RefCell<Vec<Project>>,
    rows: RefCell<Vec<(i64, gtk::ListBoxRow, gtk::Label, gtk::Label)>>,
    status: RefCell<HashMap<i64, crate::git::Status>>,
    status_tx: std::sync::mpsc::Sender<Vec<(i64, crate::git::Status)>>,
    status_rx: RefCell<std::sync::mpsc::Receiver<Vec<(i64, crate::git::Status)>>>,
    current: RefCell<Option<i64>>,
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
    sidebar_list.add_css_class("navigation-sidebar");
    sidebar_list.set_show_separators(false);

    let sidebar_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&sidebar_list)
        .build();

    // The four primitives, above the search box: the app's only chrome.
    let toggles = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    toggles.set_halign(gtk::Align::Center);

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
                format!("{}\tCtrl+B", primitive::PROJECTS_LABEL)
            } else {
                format!("{}\t{}", label_for(slot), accel_hint(slot))
            })
            .build();
        button.add_css_class("flat");
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
    search.set_placeholder_text(Some("Filter"));
    search.set_hexpand(true);

    let add_button = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add project (Ctrl+Shift+N)")
        .build();
    add_button.add_css_class("flat");
    add_button.set_action_name(Some("win.add-project"));

    let workspace_menu = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("Workspace menu")
        .build();
    workspace_menu.add_css_class("flat");

    // The sidebar's own header, matching the panes: icon, name, actions.
    let sidebar_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    sidebar_header.add_css_class("group-header");
    sidebar_header.set_margin_start(6);
    sidebar_header.set_margin_end(4);
    sidebar_header.set_margin_top(2);
    sidebar_header.set_margin_bottom(2);
    sidebar_header.append(&gtk::Image::from_icon_name(primitive::PROJECTS_ICON));
    let sidebar_title = gtk::Label::new(Some(primitive::PROJECTS_LABEL));
    sidebar_title.add_css_class("caption-heading");
    sidebar_title.set_xalign(0.0);
    sidebar_title.set_hexpand(true);
    sidebar_header.append(&sidebar_title);
    sidebar_header.append(&add_button);
    sidebar_header.append(&workspace_menu);

    let search_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    search_row.set_margin_start(8);
    search_row.set_margin_end(6);
    search_row.set_margin_top(4);
    search_row.set_margin_bottom(6);
    search_row.append(&search);

    let sidebar_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_box.add_css_class("projects-sidebar");
    sidebar_box.append(&sidebar_header);
    sidebar_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    sidebar_box.append(&toggles);
    sidebar_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    sidebar_box.append(&search_row);
    sidebar_box.append(&sidebar_scroll);

    // ---- main area ----
    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .vexpand(true)
        .build();
    let no_projects = status_page(
        "folder-open-symbolic",
        "No projects yet",
        "Add a directory to get started. A project is an agent, live changes, commands and an editor.",
        Some(("Add project", "win.add-project")),
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
    window.set_content(Some(&toasts));
    window.set_tooltip_text(Some(&format!("state: {}", paths.database().display())));

    let (status_tx, status_rx) = std::sync::mpsc::channel();
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
        workspaces: RefCell::new(HashMap::new()),
        projects: RefCell::new(Vec::new()),
        rows: RefCell::new(Vec::new()),
        status: RefCell::new(HashMap::new()),
        status_tx,
        status_rx: RefCell::new(status_rx),
        current: RefCell::new(None),
    });

    register_actions(&state, app, &workspace_menu);
    connect_widgets(&state);
    start_status_drainer(&state);
    watch_theme(&state);
    state.refresh_projects();
    if let Ok(prefs) = state.db.ui_prefs() {
        if let Some(id) = prefs.last_project {
            state.select_project(id);
        }
    }
    state.reload_theme();
    // Keys belong to the program you are looking at, not to the filter box.
    if let Some(workspace) = state.current_workspace() {
        let first = workspace.visible.borrow().first().copied();
        if let Some(primitive) = first.and_then(|slot| workspace.primitive(slot)) {
            primitive.focus();
        }
    }

    // Development aid: lay out every primitive on startup so the arrangement can
    // be checked without clicking. RADAR_PRIMITIVES=1.
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
        Slot::Shell => "utilities-terminal-symbolic",
        Slot::Custom => "application-x-executable-symbolic",
    }
}

fn accel_hint(slot: Slot) -> &'static str {
    match slot {
        Slot::Editor => "Ctrl+Shift+E",
        Slot::Agent => "Ctrl+Shift+A",
        Slot::Diff => "Ctrl+Shift+G",
        Slot::Shell => "Ctrl+Shift+T",
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
        let list = app.sidebar_list.clone();
        let app = app.clone();
        list.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            if let Some(id) = app.id_for_row(row) {
                app.select_project(id);
            }
        });
    }
    {
        let entry = app.sidebar_search.clone();
        let app = app.clone();
        entry.connect_search_changed(move |entry| app.filter_sidebar(&entry.text()));
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

fn register_actions(app: &SharedApp, gtk_app: &adw::Application, workspace_menu: &gtk::MenuButton) {
    let add = |name: &str, handler: Box<dyn Fn()>| {
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(move |_, _| handler());
        app.window.add_action(&action);
    };

    // ---- projects ----
    {
        let app = app.clone();
        add(
            "add-project",
            Box::new(move || {
                dialogs::add_project(&app.window, &app.db, {
                    let app = app.clone();
                    move |id| {
                        app.refresh_projects();
                        app.select_project(id);
                        app.toast("Project added");
                    }
                });
            }),
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
                app.refresh_projects();
                app.refresh_status();
                app.toast("Refreshed");
            }),
        );
    }
    {
        let app = app.clone();
        add(
            "quit",
            Box::new(move || {
                if let Some(window) = app.window.application().and_then(|a| a.active_window()) {
                    window.close();
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
                    app_for_save.refresh_projects();
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
                app.refresh_projects();
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
                app.refresh_projects();
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
            let slot = Slot::parse(&name);
            let Some(workspace) = app_for_action.current_workspace() else {
                app_for_action.toast("Select a project first");
                return;
            };
            app_for_action.toggle_primitive(&workspace, slot);
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
            let Some(workspace) = app_for_action.current_workspace() else {
                return;
            };
            let window = app_for_action.window.clone();
            let app_for_pick = app_for_action.clone();
            dialogs::choose_program(&window, move |program| {
                app_for_pick.set_primitive_program(&workspace, slot, program, true);
            });
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
                if let Some(primitive) = workspace.primitive(slot) {
                    primitive.focus();
                }
            }
        });
        app.window.add_action(&action);
    }
    {
        let app = app.clone();
        add("zoom", Box::new(move || app.toggle_zoom()));
    }
    {
        let app = app.clone();
        add("toggle-sidebar", Box::new(move || app.toggle_sidebar()));
    }

    // ---- workspace menu (in the sidebar, since there is no header bar) ----
    {
        let app = app.clone();
        workspace_menu.set_menu_model(Some(&app.workspace_menu_model()));
    }

    // ---- keyboard ----
    let accels: [(&str, &[&str]); 15] = [
        ("win.add-project", &["<Control><Shift>n"]),
        ("win.preferences", &["<Control>comma"]),
        ("win.refresh", &["<Control><Shift>r"]),
        ("win.quit", &["<Control><Shift>q"]),
        ("win.toggle-sidebar", &["<Control>b"]),
        ("win.zoom", &["F11"]),
        ("win.primitive-toggle::editor", &["<Control><Shift>e"]),
        ("win.primitive-toggle::agent", &["<Control><Shift>a"]),
        ("win.primitive-toggle::diff", &["<Control><Shift>g"]),
        ("win.primitive-toggle::shell", &["<Control><Shift>t"]),
        ("win.primitive-program::agent", &["<Control><Shift>p"]),
        ("win.primitive-focus::editor", &["<Control><Shift>1"]),
        ("win.primitive-focus::agent", &["<Control><Shift>2"]),
        ("win.primitive-focus::diff", &["<Control><Shift>3"]),
        ("win.primitive-focus::shell", &["<Control><Shift>4"]),
    ];
    for (action, keys) in accels {
        let _ = gtk_app.set_accels_for_action(action, keys);
    }
}

impl App {
    fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }

    fn workspace_menu_model(&self) -> gio::Menu {
        let menu = gio::Menu::new();
        let workspace = gio::Menu::new();
        workspace.append_item(&item("Add project…", "win.add-project"));
        workspace.append_item(&item("Refresh", "win.refresh"));
        workspace.append_item(&item("Preferences…", "win.preferences"));
        workspace.append_item(&item("Quit", "win.quit"));
        menu.append_section(None, &workspace);

        let project = gio::Menu::new();
        project.append_item(&item("Rename project…", "win.project-rename"));
        project.append_item(&item("Pin or unpin", "win.project-pin"));
        project.append_item(&item("Move up", "win.project-move-up"));
        project.append_item(&item("Move down", "win.project-move-down"));
        menu.append_section(None, &project);

        let panes = gio::Menu::new();
        for slot in PRIMITIVES {
            panes.append_item(&item(
                &format!("Toggle {}", label_for(slot)),
                &format!("win.primitive-toggle::{}", slot.as_str()),
            ));
        }
        menu.append_section(None, &panes);

        let externals = programs::external_programs();
        if !externals.is_empty() {
            let open_in = gio::Menu::new();
            for program in externals {
                open_in.append(
                    Some(&format!("Open in {}", program.name)),
                    Some(&format!("win.open-external-{}", program.id)),
                );
            }
            menu.append_section(None, &open_in);
        }
        menu
    }

    /// A pane's own menu: which program it runs, and what to do with it.
    fn primitive_menu_model(&self, slot: Slot) -> gio::Menu {
        let menu = gio::Menu::new();
        let program = gio::Menu::new();
        program.append_item(&item(
            "Change program…",
            &format!("win.primitive-program::{}", slot.as_str()),
        ));
        menu.append_section(None, &program);

        let panes = gio::Menu::new();
        panes.append_item(&item(
            &format!("Focus {}", label_for(slot)),
            &format!("win.primitive-focus::{}", slot.as_str()),
        ));
        for other in PRIMITIVES {
            if other == slot {
                continue;
            }
            panes.append_item(&item(
                &format!("Toggle {}", label_for(other)),
                &format!("win.primitive-toggle::{}", other.as_str()),
            ));
        }
        menu.append_section(None, &panes);

        let layout = gio::Menu::new();
        layout.append_item(&item("Zoom pane (F11)", "win.zoom"));
        layout.append_item(&item("Toggle sidebar", "win.toggle-sidebar"));
        menu.append_section(None, &layout);
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

    /// Confirm, then remove a project from the sidebar. Never touches the disk.
    fn confirm_remove(&self, id: i64) {
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
        // The callback outlives this borrow, so it works from owned handles: the
        // database, the sidebar list, and the toast overlay.
        let db = self.db.clone();
        let toasts = self.toasts.clone();
        let list = self.sidebar_list.clone();
        let current = self.current.clone();
        let window = self.window.clone();
        dialog.choose(Some(&window), None::<&gio::Cancellable>, move |result| {
            if result != Ok(1) {
                return;
            }
            if let Err(error) = db.remove_project(project.id) {
                eprintln!("radar: {error}");
            }
            if *current.borrow() == Some(project.id) {
                *current.borrow_mut() = None;
            }
            // Rebuild the list from a fresh read; the row that asked for this is
            // about to disappear.
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            toasts.add_toast(adw::Toast::new("Project removed"));
        });
    }

    fn reload_theme(&self) {
        let theme = Theme::load();
        if let Ok(manager) = adw::StyleManager::default().downcast::<adw::StyleManager>() {
            manager.set_color_scheme(if theme.dark {
                adw::ColorScheme::ForceDark
            } else {
                adw::ColorScheme::ForceLight
            });
        }
        for workspace in self.workspaces.borrow().values() {
            for primitive in workspace.primitives.borrow().values() {
                primitive.pane.apply_theme(&theme);
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

    fn refresh_projects(&self) {
        let projects = self.db.projects().unwrap_or_default();
        let selected = *self.current.borrow();
        *self.projects.borrow_mut() = projects.clone();

        while let Some(child) = self.sidebar_list.first_child() {
            self.sidebar_list.remove(&child);
        }
        self.rows.borrow_mut().clear();
        if projects.is_empty() {
            let hint = gtk::Label::new(Some("No projects yet.\nPress + to add one."));
            hint.add_css_class("dim-label");
            hint.set_justify(gtk::Justification::Center);
            hint.set_margin_top(24);
            hint.set_wrap(true);
            self.sidebar_list.append(&hint);
            self.show_placeholder();
            return;
        }

        for project in &projects {
            let (row, summary, badge) = self.build_project_row(project);
            self.rows
                .borrow_mut()
                .push((project.id, row.clone(), summary, badge));
            self.sidebar_list.append(&row);
        }
        self.filter_sidebar(&self.sidebar_search.text());
        self.refresh_status();

        if let Some(id) = selected {
            self.select_row_for(id);
        }
        if selected.is_none() || selected.is_some_and(|id| !projects.iter().any(|p| p.id == id)) {
            if let Some(first) = projects.first() {
                self.select_project(first.id);
            }
        }
    }

    fn build_project_row(&self, project: &Project) -> (gtk::ListBoxRow, gtk::Label, gtk::Label) {
        let row = gtk::ListBoxRow::new();

        let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        box_.set_margin_top(4);
        box_.set_margin_bottom(4);
        box_.set_margin_start(6);
        box_.set_margin_end(4);

        let texts = gtk::Box::new(gtk::Orientation::Vertical, 0);
        texts.set_hexpand(true);
        let name = gtk::Label::new(Some(&project.name));
        name.set_xalign(0.0);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        if project.pinned {
            name.set_text(&format!("{}  📌", project.name));
        }
        texts.append(&name);

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
        badge.add_css_class("caption");
        badge.add_css_class("accent");
        badge.set_valign(gtk::Align::Center);
        box_.append(&badge);

        // Remove, per row: the id travels with the action so no state is needed.
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
        row.set_tooltip_text(Some(&project.display_path()));

        let status = self.status.borrow().get(&project.id).cloned();
        let (text, count) = match (&status, project.is_missing()) {
            (_, true) => ("missing".to_string(), None),
            (Some(status), _) => (
                status.summary(),
                (status.changed > 0).then_some(status.changed),
            ),
            (None, _) => ("…".to_string(), None),
        };
        summary.set_text(&format!("{text}  ·  {parent}"));
        match count {
            Some(changed) => badge.set_text(&format!("●{changed}")),
            None => badge.set_visible(false),
        }
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
            match self.status.borrow().get(id) {
                Some(status) => {
                    summary.set_text(&format!("{}  ·  {parent}", status.summary()));
                    if status.changed > 0 {
                        badge.set_text(&format!("●{}", status.changed));
                        badge.set_visible(true);
                    } else {
                        badge.set_visible(false);
                    }
                }
                None => summary.set_text(&format!("…  ·  {parent}")),
            }
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

    // ---- the workspace and its primitives ----

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
        let workspace = self.workspace_for(&project);
        self.stack.set_visible_child_name(&format!("project-{id}"));
        let _ = self.db.touch_project(id);
        let _ = self.db.remember_last_project(Some(id));
        self.sync_toggles();
        self.refresh_menus();
        self.select_row_for(id);
        let _ = workspace;
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
            visible: RefCell::new(Vec::new()),
            positions: RefCell::new(HashMap::new()),
            holder: holder.clone(),
            zoom: RefCell::new(None),
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
        let preferences = self.db.preferences().unwrap_or_default();
        let mut visible = Vec::new();
        if stored.is_empty() {
            // The agent is the point of the workspace; everything else is one
            // keystroke away.
            visible = vec![Slot::Agent];
            for slot in &visible {
                if let Some(program) = programs::for_slot(*slot, &preferences) {
                    workspace
                        .programs
                        .borrow_mut()
                        .insert(*slot, program.id.clone());
                }
            }
        } else {
            for tab in &stored {
                workspace
                    .programs
                    .borrow_mut()
                    .insert(tab.slot, tab.program_id.clone());
                visible.push(tab.slot);
            }
        }
        Workspace::ordered(&mut visible);
        *workspace.visible.borrow_mut() = visible.clone();

        // Open the ones that are on screen; a program that is gone falls back to
        // the preferred one for its slot.
        for slot in visible {
            if self.ensure_primitive(&workspace, slot).is_none() {
                continue;
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
            &label_for(slot),
            pane::ShiftEnter::for_slot(slot),
        ));
        let primitive = Primitive::new(slot, &program, pane);
        primitive
            .menu_button
            .set_menu_model(Some(&self.primitive_menu_model(slot)));
        workspace
            .primitives
            .borrow_mut()
            .insert(slot, primitive.clone());
        Some(primitive)
    }

    /// Show or hide one primitive. Hiding never stops the program.
    fn toggle_primitive(&self, workspace: &Rc<Workspace>, slot: Slot) {
        if slot == Slot::Custom {
            return;
        }
        if workspace.is_visible(slot) {
            workspace.visible.borrow_mut().retain(|other| *other != slot);
        } else {
            if self.ensure_primitive(workspace, slot).is_none() {
                self.toast(&format!(
                    "No {} installed — set one in Preferences",
                    label_for(slot).to_lowercase()
                ));
                return;
            }
            let mut visible = workspace.visible.borrow_mut();
            visible.push(slot);
            Workspace::ordered(&mut visible);
        }
        *workspace.zoom.borrow_mut() = None;
        trace(&format!(
            "toggle {}: visible {:?}",
            slot.as_str(),
            workspace.visible_slots()
                .iter()
                .map(|slot| slot.as_str())
                .collect::<Vec<_>>()
        ));
        self.layout(workspace);
        self.sync_toggles();
        self.persist_primitives(workspace);
    }

    /// Show a primitive without toggling it off when it is already visible.
    fn show_primitive(&self, workspace: &Rc<Workspace>, slot: Slot) {
        if !workspace.is_visible(slot) {
            self.toggle_primitive(workspace, slot);
        }
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
        }
        workspace
            .programs
            .borrow_mut()
            .insert(slot, program.id.clone());
        if visible && !workspace.is_visible(slot) {
            let mut list = workspace.visible.borrow_mut();
            list.push(slot);
            Workspace::ordered(&mut list);
        }
        self.ensure_primitive(workspace, slot);
        self.layout(workspace);
        self.sync_toggles();
        self.persist_primitives(workspace);
        self.toast(&format!("{} → {}", label_for(slot), program.name));
    }

    /// Arrange the visible primitives into panes, the same way every time.
    fn layout(&self, workspace: &Rc<Workspace>) {
        let visible = workspace.visible.borrow().clone();

        // Everything is detached first: a widget lives in one parent at a time and
        // GTK will not reparent it for us.
        for primitive in workspace.primitives.borrow().values() {
            primitive.widget.unparent();
        }
        while let Some(child) = workspace.holder.first_child() {
            workspace.holder.remove(&child);
        }

        if visible.is_empty() {
            workspace.holder.append(&status_page(
                "view-list-symbolic",
                "Every pane is hidden",
                "Use the icons above the project list to bring one back.",
                None,
            ));
            return;
        }

        let widget = |slot: Slot| {
            workspace
                .primitive(slot)
                .map(|primitive| primitive.widget.clone().upcast::<gtk::Widget>())
        };

        // Terminal along the bottom, editor on the left, agent and diff stacked
        // on the right. Whatever is not visible simply is not there.
        let side_slots: Vec<Slot> = [Slot::Diff, Slot::Editor]
            .into_iter()
            .filter(|slot| visible.contains(slot))
            .collect();
        let side = match side_slots.len() {
            0 => None,
            1 => widget(side_slots[0]),
            _ => {
                let (top, bottom) = (widget(side_slots[0]), widget(side_slots[1]));
                match (top, bottom) {
                    (Some(top), Some(bottom)) => Some(
                        self.stacked(workspace, "side", gtk::Orientation::Vertical, &top, &bottom, 520),
                    ),
                    (Some(only), None) | (None, Some(only)) => Some(only),
                    _ => None,
                }
            }
        };
        let agent = widget(Slot::Agent).filter(|_| visible.contains(&Slot::Agent));
        let main = match (visible.contains(&Slot::Agent), agent, side) {
            (true, Some(agent), Some(side)) => Some(self.stacked(
                workspace,
                "main",
                gtk::Orientation::Horizontal,
                &agent,
                &side,
                560,
            )),
            (_, Some(only), None) => Some(only),
            (_, None, Some(only)) => Some(only),
            _ => None,
        };
        let terminal = widget(Slot::Shell).filter(|_| visible.contains(&Slot::Shell));
        let root = match (main, terminal) {
            (Some(main), Some(terminal)) => {
                Some(self.stacked(workspace, "outer", gtk::Orientation::Vertical, &main, &terminal, 620))
            }
            (Some(only), None) | (None, Some(only)) => Some(only),
            _ => None,
        };

        if let Some(root) = root {
            workspace.holder.append(&root);
        }
        trace(&format!(
            "layout: {:?} ({} panes)",
            visible.iter().map(|slot| slot.as_str()).collect::<Vec<_>>(),
            workspace.primitives.borrow().len()
        ));
    }

    /// A draggable divider whose position is remembered.
    fn stacked(
        &self,
        workspace: &Rc<Workspace>,
        key: &str,
        orientation: gtk::Orientation,
        first: &gtk::Widget,
        second: &gtk::Widget,
        default_position: i32,
    ) -> gtk::Widget {
        let paned = gtk::Paned::new(orientation);
        paned.set_wide_handle(true);
        paned.set_resize_start_child(true);
        paned.set_resize_end_child(true);
        paned.set_shrink_start_child(false);
        paned.set_shrink_end_child(false);
        paned.set_vexpand(true);
        paned.set_hexpand(true);
        paned.set_start_child(Some(first));
        paned.set_end_child(Some(second));
        paned.set_position(
            workspace
                .positions
                .borrow()
                .get(key)
                .copied()
                .unwrap_or(default_position),
        );

        let workspace_for_position = workspace.clone();
        let key = key.to_string();
        paned.connect_position_notify(move |paned| {
            workspace_for_position
                .positions
                .borrow_mut()
                .insert(key.clone(), paned.position());
        });
        paned.upcast()
    }

    /// Zoom the first visible primitive to the whole window, and back.
    fn toggle_zoom(&self) {
        let Some(workspace) = self.current_workspace() else {
            return;
        };
        if let Some(previous) = workspace.zoom.borrow_mut().take() {
            *workspace.visible.borrow_mut() = previous;
            self.layout(&workspace);
            self.sync_toggles();
            return;
        }
        let visible = workspace.visible.borrow().clone();
        if visible.len() < 2 {
            self.toast("Only one pane is showing");
            return;
        }
        *workspace.zoom.borrow_mut() = Some(visible.clone());
        *workspace.visible.borrow_mut() = vec![visible[0]];
        self.layout(&workspace);
        self.sync_toggles();
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
    fn persist_primitives(&self, workspace: &Rc<Workspace>) {
        let mut tabs = Vec::new();
        for (index, slot) in workspace.visible.borrow().iter().enumerate() {
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
        if let Err(error) = self.db.set_tabs(workspace.project.id, &tabs) {
            eprintln!("radar: could not store the panes: {error}");
        }
    }

    /// Make the sidebar icons match the layout.
    fn sync_toggles(&self) {
        let visible = self
            .current_workspace()
            .map(|workspace| workspace.visible_slots())
            .unwrap_or_default();
        let preferences = self.db.preferences().unwrap_or_default();
        self.projects_toggle.set_active(self.sidebar_shown.get());
        for (slot, button) in self.toggles.borrow().iter() {
            button.set_active(visible.contains(slot));
            let available = *slot == Slot::Shell || programs::for_slot(*slot, &preferences).is_some();
            button.set_sensitive(available);
        }
    }

    /// Rebuild the pane menus and the sidebar icons' sensitivity.
    fn refresh_menus(&self) {
        for workspace in self.workspaces.borrow().values() {
            for (slot, primitive) in workspace.primitives.borrow().iter() {
                primitive
                    .menu_button
                    .set_menu_model(Some(&self.primitive_menu_model(*slot)));
            }
        }
    }
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
