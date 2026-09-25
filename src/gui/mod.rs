//! The app window.
//!
//! Layout, top to bottom, with no header bar in between:
//!
//!   ┌────────────┬──────────────────────────────────────────────┐
//!   │ filter  + ⋮│  1 │ 2 │ 3 │ ⋯      (a pane: tab strip)      │
//!   │ project 1  │ ┌─────────────────────┬──────────────────┐   │
//!   │ project 2  │ │ a leaf of terminals │ another leaf     │   │
//!   │ project 3  │ └─────────────────────┴──────────────────┘   │
//!   └────────────┴──────────────────────────────────────────────┘
//!
//! The sidebar and every split are `GtkPaned`, so all of them can be dragged to
//! resize. Each pane is a [`Leaf`] with its own tab strip; splitting a pane
//! replaces it in the tree with a pair.

mod dialogs;
mod leaf;
mod pane;
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

use leaf::{Leaf, Page};
use pane::Pane;
pub use theme::Theme;

type SharedDb = Rc<Db>;

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

    // Run with a clean argv: GApplication would otherwise try to interpret
    // radar's own command line (and fail on arguments it does not know).
    let _ = app.run_with_args(&["radar"]);
    Ok(())
}

/// Where a leaf sits inside its parent split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Half {
    First,
    Second,
}

/// A node of the split tree.
enum Node {
    Leaf(Rc<Leaf>),
    Split {
        paned: gtk::Paned,
        orientation: gtk::Orientation,
        first: RefCell<Rc<Node>>,
        second: RefCell<Rc<Node>>,
    },
}

impl Node {
    fn widget(&self) -> gtk::Widget {
        match self {
            Node::Leaf(leaf) => leaf.widget.clone().upcast(),
            Node::Split { paned, .. } => paned.clone().upcast(),
        }
    }

    fn leaves(&self) -> Vec<Rc<Leaf>> {
        match self {
            Node::Leaf(leaf) => vec![leaf.clone()],
            Node::Split { first, second, .. } => {
                let mut all = first.borrow().leaves();
                all.extend(second.borrow().leaves());
                all
            }
        }
    }
}

/// A project's open workspace: its split tree, kept alive across project
/// switches so running programs are never interrupted.
struct Workspace {
    project: Project,
    /// Holds exactly one child: the root node's widget.
    holder: gtk::Box,
    root: RefCell<Rc<Node>>,
    focused: RefCell<Rc<Leaf>>,
    /// Set while a pane is zoomed to the whole window.
    zoom: RefCell<Option<Zoom>>,
}

struct Zoom {
    leaf: Rc<Leaf>,
    parent: gtk::Paned,
    position: Half,
}

impl Workspace {
    fn leaf(&self) -> Rc<Leaf> {
        self.focused.borrow().clone()
    }

    fn all_leaves(&self) -> Vec<Rc<Leaf>> {
        self.root.borrow().leaves()
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

    // ---- sidebar: filter, add, menu, list ----
    let sidebar_list = gtk::ListBox::new();
    sidebar_list.set_selection_mode(gtk::SelectionMode::Single);
    sidebar_list.add_css_class("navigation-sidebar");
    sidebar_list.set_show_separators(false);

    let sidebar_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&sidebar_list)
        .build();

    let sidebar_search = gtk::SearchEntry::new();
    sidebar_search.set_placeholder_text(Some("Projects"));
    sidebar_search.set_hexpand(true);

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

    let sidebar_top = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    sidebar_top.set_margin_top(6);
    sidebar_top.set_margin_bottom(6);
    sidebar_top.set_margin_start(8);
    sidebar_top.set_margin_end(6);
    sidebar_top.append(&sidebar_search);
    sidebar_top.append(&add_button);
    sidebar_top.append(&workspace_menu);

    let sidebar_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_box.add_css_class("projects-sidebar");
    sidebar_box.append(&sidebar_top);
    sidebar_box.append(&sidebar_scroll);

    // ---- main area ----
    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .vexpand(true)
        .build();
    let no_projects = status_page(
        "folder-open-symbolic",
        "No projects yet",
        "Add a directory to get started. Each project gets its own panes: an editor, an agent, a live diff.",
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

    // ---- sidebar | main, draggable ----
    let splitter = gtk::Paned::new(gtk::Orientation::Horizontal);
    splitter.set_start_child(Some(&sidebar_box));
    splitter.set_end_child(Some(&stack));
    splitter.set_resize_start_child(false);
    splitter.set_shrink_start_child(false);
    splitter.set_wide_handle(true);
    splitter.set_vexpand(true);
    splitter.set_position(db.ui_prefs().map(|prefs| prefs.sidebar_width).unwrap_or(280));

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
        sidebar_search: sidebar_search.clone(),
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
    window
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
    // Remember the sidebar width when it is dragged.
    {
        let app = app.clone();
        let save = {
            let app = app.clone();
            let timer = Rc::new(RefCell::new(None::<glib::SourceId>));
            move || {
                if let Some(id) = timer.borrow_mut().take() {
                    id.remove();
                }
                let app = app.clone();
                let timer_for_cb = timer.clone();
                let id = glib::timeout_add_local_once(Duration::from_millis(400), move || {
                    let width = app.splitter.position();
                    let mut prefs = app.db.ui_prefs().unwrap_or_default();
                    prefs.sidebar_width = width;
                    if let Err(error) = app.db.set_ui_prefs(&prefs) {
                        eprintln!("radar: could not store the sidebar width: {error}");
                    }
                    *timer_for_cb.borrow_mut() = None;
                });
                *timer.borrow_mut() = Some(id);
            }
        };
        app.splitter.connect_position_notify(move |_| save());
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

/// A `gio::Menu` action string for a window action with no arguments.
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
        let app = app.clone();
        add(
            "preferences",
            Box::new(move || {
                dialogs::preferences(&app.window, &app.db, {
                    let app = app.clone();
                    move || {
                        app.refresh_pane_menus();
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
    {
        let app = app.clone();
        add(
            "project-remove",
            Box::new(move || {
                let Some(project) = app.current_project() else { return };
                let dialog = gtk::AlertDialog::builder()
                    .message("Remove project?")
                    .detail(format!(
                        "{} will be removed from the sidebar. Nothing on disk is touched.",
                        project.name
                    ))
                    .buttons(["Cancel", "Remove"])
                    .cancel_button(0)
                    .default_button(0)
                    .build();
                let app_for_response = app.clone();
                dialog.choose(Some(&app.window), None::<&gio::Cancellable>, move |result| {
                    if result == Ok(1) {
                        if let Err(error) = app_for_response.db.remove_project(project.id) {
                            eprintln!("radar: {error}");
                        }
                        app_for_response.workspaces.borrow_mut().remove(&project.id);
                        *app_for_response.current.borrow_mut() = None;
                        app_for_response.refresh_projects();
                        app_for_response.show_placeholder();
                        app_for_response.toast("Project removed");
                    }
                });
            }),
        );
    }

    // ---- panes and tabs ----
    for (name, slot) in [
        ("toggle-editor", Slot::Editor),
        ("toggle-agent", Slot::Agent),
        ("toggle-diff", Slot::Diff),
        ("toggle-shell", Slot::Shell),
    ] {
        let app = app.clone();
        add(
            &format!("pane-{name}"),
            Box::new(move || app.toggle_slot(slot)),
        );
    }
    {
        let app = app.clone();
        add(
            "tab-choose",
            Box::new(move || {
                let Some(workspace) = app.current_workspace() else {
                    app.toast("Select a project first");
                    return;
                };
                let leaf = workspace.leaf();
                let window = app.window.clone();
                let app = app.clone();
                dialogs::choose_program(&window, move |program| {
                    let slot = program.kind.default_slot();
                    app.open_program(&leaf, program, slot, Vec::new());
                });
            }),
        );
    }
    {
        let app = app.clone();
        add(
            "tab-close",
            Box::new(move || {
                if let Some(workspace) = app.current_workspace() {
                    let leaf = workspace.leaf();
                    if let Some(page) = leaf.selected_page() {
                        leaf.tab_view.close_page(&page);
                    }
                }
            }),
        );
    }
    {
        let app = app.clone();
        add(
            "split-right",
            Box::new(move || app.split_focused(gtk::Orientation::Horizontal)),
        );
    }
    {
        let app = app.clone();
        add(
            "split-down",
            Box::new(move || app.split_focused(gtk::Orientation::Vertical)),
        );
    }
    {
        let app = app.clone();
        add(
            "split-close",
            Box::new(move || app.close_focused_pane()),
        );
    }
    {
        let app = app.clone();
        add(
            "focus-prev",
            Box::new(move || app.focus_sibling(false)),
        );
    }
    {
        let app = app.clone();
        add(
            "focus-next",
            Box::new(move || app.focus_sibling(true)),
        );
    }
    {
        let app = app.clone();
        add("zoom", Box::new(move || app.toggle_zoom()));
    }
    {
        let app = app.clone();
        add("toggle-sidebar", Box::new(move || app.toggle_sidebar()));
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
        ("win.pane-toggle-editor", &["<Control><Shift>e"]),
        ("win.pane-toggle-agent", &["<Control><Shift>a"]),
        ("win.pane-toggle-diff", &["<Control><Shift>g"]),
        ("win.pane-toggle-shell", &["<Control><Shift>t"]),
        ("win.tab-choose", &["<Control><Shift>p"]),
        ("win.tab-close", &["<Control><Shift>w"]),
        ("win.split-right", &["<Control><Shift>backslash"]),
        ("win.split-down", &["<Control><Shift>minus"]),
        ("win.focus-next", &["<Control><Shift>l"]),
        ("win.zoom", &["F11"]),
    ];
    for (action, keys) in accels {
        gtk_app.set_accels_for_action(action, keys);
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
        project.append_item(&item("Remove project…", "win.project-remove"));
        menu.append_section(None, &project);

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

    /// The menu of a pane: another tab here, another pane, or the workspace.
    fn pane_menu_model(&self) -> gio::Menu {
        let preferences = self.db.preferences().unwrap_or_default();
        let menu = gio::Menu::new();

        let tabs = gio::Menu::new();
        for slot in [Slot::Editor, Slot::Agent, Slot::Diff, Slot::Shell] {
            let name = programs::for_slot(slot, &preferences)
                .map(|program| program.name)
                .unwrap_or_else(|| "nothing installed".into());
            let label = match slot {
                Slot::Editor => format!("Editor — {name}"),
                Slot::Agent => format!("Agent — {name}"),
                Slot::Diff => format!("Diff — {name}"),
                Slot::Shell => format!("Terminal — {name}"),
                Slot::Custom => name,
            };
            tabs.append_item(&item(
                &label,
                &format!("win.pane-toggle-{}", slot.as_str()),
            ));
        }
        tabs.append_item(&item("Choose program…", "win.tab-choose"));
        menu.append_section(None, &tabs);

        let panes = gio::Menu::new();
        panes.append_item(&item("Split right", "win.split-right"));
        panes.append_item(&item("Split down", "win.split-down"));
        panes.append_item(&item("Focus next pane", "win.focus-next"));
        panes.append_item(&item("Zoom pane", "win.zoom"));
        panes.append_item(&item("Close pane", "win.split-close"));
        menu.append_section(None, &panes);

        let side = gio::Menu::new();
        side.append_item(&item("Toggle sidebar", "win.toggle-sidebar"));
        menu.append_section(None, &side);
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

    /// Re-read the theme and repaint every live terminal.
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
            for leaf in workspace.all_leaves() {
                for page in leaf.pages.borrow().iter() {
                    page.pane.apply_theme(&theme);
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

        let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        box_.set_margin_top(4);
        box_.set_margin_bottom(4);
        box_.set_margin_start(6);
        box_.set_margin_end(6);

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
    }

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
        workspace.leaf().focus();
        self.refresh_pane_menus();
        self.select_row_for(id);
    }

    // ---- the split tree ----

    fn workspace_for(&self, project: &Project) -> Rc<Workspace> {
        if let Some(existing) = self.workspaces.borrow().get(&project.id) {
            return existing.clone();
        }

        let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let first = Leaf::new();
        let root = Rc::new(Node::Leaf(first.clone()));
        holder.append(&root.widget());

        let workspace = Rc::new(Workspace {
            project: project.clone(),
            holder: holder.clone(),
            root: RefCell::new(root.clone()),
            focused: RefCell::new(first.clone()),
            zoom: RefCell::new(None),
        });

        wire_close_page(self.db.clone(), workspace.clone(), first.clone());
        wire_focus(&workspace, &first);

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

        // Restore stored tabs, or start with the standard trio.
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
        let tabs = if stored.is_empty() {
            let preferences = self.db.preferences().unwrap_or_default();
            [Slot::Editor, Slot::Agent, Slot::Diff]
                .into_iter()
                .filter_map(|slot| {
                    programs::for_slot(slot, &preferences).map(|program| db_tab(slot, program.id))
                })
                .collect()
        } else {
            stored
        };

        let preferences = self.db.preferences().unwrap_or_default();
        for tab in tabs {
            // A stored tab can name a program that is gone: fall back to the
            // current choice for that slot rather than losing the tab.
            let program = match programs::by_id(&tab.program_id) {
                Some(program) if program.installed() => program,
                _ => match programs::for_slot(tab.slot, &preferences) {
                    Some(fallback) => fallback,
                    None => {
                        self.toast(&format!("{} is not installed", tab.program_id));
                        continue;
                    }
                },
            };
            self.open_program(&first, program, tab.slot, tab.extra_args.clone());
        }
        persist_tabs(&self.db, &workspace);
        workspace
    }

    /// Open a program as a new tab in `leaf`.
    fn open_program(&self, leaf: &Rc<Leaf>, program: Program, slot: Slot, extra_args: Vec<String>) {
        let Some(workspace) = self.current_workspace() else {
            return;
        };
        let mut options = self.launch_options();
        options.extra_args = extra_args.clone();
        let spec = program.command_spec(&options);
        let title = program.name.clone();
        let theme = self.theme.borrow().clone();

        // Every pane is a terminal: the program runs as its author intended.
        let pane = Rc::new(Pane::spawn(&spec, &workspace.project.path, &theme, &title));
        let tab_page = leaf.tab_view.append(pane.widget());
        tab_page.set_title(&title);
        tab_page.set_tooltip(&format!("{}\n{}", program.name, pane.command()));
        if let Some(icon) = icon_for(slot) {
            tab_page.set_icon(Some(&icon));
        }
        leaf.add_page(Page {
            page: tab_page,
            slot,
            program_id: program.id.clone(),
            title,
            extra_args,
            pane,
        });
        *workspace.focused.borrow_mut() = leaf.clone();
    }

    /// Split the focused pane in two, and focus the new half.
    fn split_focused(&self, orientation: gtk::Orientation) {
        let Some(workspace) = self.current_workspace() else {
            return;
        };
        let leaf = workspace.leaf();
        let new_leaf = Leaf::new();

        // The new half gets a tab too: an empty pane is a puzzle, not a tool.
        let preferences = self.db.preferences().unwrap_or_default();
        let slot = match orientation {
            gtk::Orientation::Horizontal => Slot::Shell,
            _ => Slot::Diff,
        };
        let Some(program) = programs::for_slot(slot, &preferences) else {
            self.toast("nothing installed for a new pane");
            return;
        };
        new_leaf.tab_view.set_hexpand(true);
        wire_close_page(self.db.clone(), workspace.clone(), new_leaf.clone());
        wire_focus(&workspace, &new_leaf);
        self.open_program(&new_leaf, program, slot, Vec::new());

        let paned = gtk::Paned::new(orientation);
        paned.set_start_child(Some(&leaf.widget));
        paned.set_end_child(Some(&new_leaf.widget));
        paned.set_resize_start_child(true);
        paned.set_resize_end_child(true);
        paned.set_shrink_start_child(false);
        paned.set_shrink_end_child(false);
        paned.set_wide_handle(true);
        paned.set_vexpand(true);
        paned.set_position(match orientation {
            gtk::Orientation::Horizontal => 700,
            _ => 400,
        });

        let split = Rc::new(Node::Split {
            paned: paned.clone(),
            orientation,
            first: RefCell::new(Rc::new(Node::Leaf(leaf.clone()))),
            second: RefCell::new(Rc::new(Node::Leaf(new_leaf.clone()))),
        });

        // Replace the leaf with the pair, wherever it is in the tree.
        let root = workspace.root.borrow().clone();
        if let Some(parent) = find_parent_of_leaf(&root, &leaf) {
            replace_leaf(&parent, &leaf, split);
        } else {
            workspace.holder.remove(&root.widget());
            workspace.holder.append(&split.widget());
            *workspace.root.borrow_mut() = split;
        }
        *workspace.focused.borrow_mut() = new_leaf.clone();
        new_leaf.focus();
        self.refresh_pane_menus();
        persist_tabs(&self.db, &workspace);
        self.toast(match orientation {
            gtk::Orientation::Horizontal => "Split right",
            _ => "Split down",
        });
    }

    fn close_focused_pane(&self) {
        let Some(workspace) = self.current_workspace() else {
            return;
        };
        let leaf = workspace.leaf();
        let pages: Vec<adw::TabPage> = leaf
            .pages
            .borrow()
            .iter()
            .map(|page| page.page.clone())
            .collect();
        for page in pages {
            leaf.tab_view.close_page(&page);
        }
    }

    /// The orientation of the split holding a leaf, if any.
    fn split_orientation(&self, leaf: &Rc<Leaf>) -> Option<gtk::Orientation> {
        let workspace = self.current_workspace()?;
        let root = workspace.root.borrow().clone();
        let parent = find_parent_of_leaf(&root, leaf)?;
        match &*parent {
            Node::Split { orientation, .. } => Some(*orientation),
            Node::Leaf(_) => None,
        }
    }

    /// Move focus to the other pane of the nearest split.
    fn focus_sibling(&self, forward: bool) {
        let Some(workspace) = self.current_workspace() else {
            return;
        };
        let leaves = workspace.all_leaves();
        if leaves.len() < 2 {
            return;
        }
        let current = workspace.leaf();
        let index = leaves
            .iter()
            .position(|leaf| Rc::ptr_eq(leaf, &current))
            .unwrap_or(0);
        let next = if forward {
            (index + 1) % leaves.len()
        } else {
            (index + leaves.len() - 1) % leaves.len()
        };
        *workspace.focused.borrow_mut() = leaves[next].clone();
        leaves[next].focus();
        let _ = self.split_orientation(&current);
    }

    /// Zoom the focused pane to the whole window, and back.
    fn toggle_zoom(&self) {
        let Some(workspace) = self.current_workspace() else { return };

        if let Some(zoom) = workspace.zoom.borrow_mut().take() {
            workspace.holder.remove(&zoom.leaf.widget);
            match zoom.position {
                Half::First => zoom.parent.set_start_child(Some(&zoom.leaf.widget)),
                Half::Second => zoom.parent.set_end_child(Some(&zoom.leaf.widget)),
            }
            workspace.holder.append(&workspace.root.borrow().widget());
            zoom.leaf.focus();
            return;
        }

        let leaf = workspace.leaf();
        let root = workspace.root.borrow().clone();
        let Some(parent) = find_parent_of_leaf(&root, &leaf) else {
            self.toast("Already full window");
            return;
        };
        let position = position_of_leaf(&parent, &leaf);
        let Node::Split { paned, .. } = &*parent else {
            return;
        };
        match position {
            Half::First => paned.set_start_child(None::<&gtk::Widget>),
            Half::Second => paned.set_end_child(None::<&gtk::Widget>),
        }
        workspace.holder.remove(&root.widget());
        workspace.holder.append(&leaf.widget);
        *workspace.zoom.borrow_mut() = Some(Zoom {
            leaf: leaf.clone(),
            parent: paned.clone(),
            position,
        });
        leaf.focus();
    }

    fn toggle_sidebar(&self) {
        let showing = !self.sidebar_shown.get();
        self.splitter.set_start_child(if showing {
            Some(&self.sidebar)
        } else {
            None::<&gtk::Widget>
        });
        self.sidebar_shown.set(showing);
    }

    /// Focus a slot's tab in this pane, or open it here if there is none.
    fn toggle_slot(&self, slot: Slot) {
        let Some(workspace) = self.current_workspace() else {
            self.toast("Select a project first");
            return;
        };
        let leaf = workspace.leaf();
        if let Some(page) = leaf.page_for_slot(slot) {
            leaf.tab_view.set_selected_page(&page);
            leaf.focus();
            return;
        }
        let preferences = self.db.preferences().unwrap_or_default();
        let Some(program) = programs::for_slot(slot, &preferences) else {
            self.toast(&format!("No {} installed", slot.label().to_lowercase()));
            return;
        };
        self.open_program(&leaf, program, slot, Vec::new());
        persist_tabs(&self.db, &workspace);
    }

    /// Rebuild the pane menus, which list the preferred programs.
    fn refresh_pane_menus(&self) {
        let model = self.pane_menu_model();
        for workspace in self.workspaces.borrow().values() {
            for leaf in workspace.all_leaves() {
                leaf.menu_button.set_menu_model(Some(&model));
            }
        }
    }
}

/// Locate the split that directly contains a node (node identity).
fn find_parent(root: &Rc<Node>, target: &Rc<Node>) -> Option<Rc<Node>> {
    match &**root {
        Node::Leaf(_) => None,
        Node::Split { first, second, .. } => {
            for child in [first, second] {
                let child = child.borrow().clone();
                if Rc::ptr_eq(&child, target) {
                    return Some(root.clone());
                }
                if let Some(found) = find_parent(&child, target) {
                    return Some(found);
                }
            }
            None
        }
    }
}

/// Locate the split that directly contains a leaf.
///
/// Identity has to be by leaf: a freshly built `Rc<Node>` around the same leaf is
/// a different allocation, so pointer comparison on nodes would never match.
fn find_parent_of_leaf(root: &Rc<Node>, leaf: &Rc<Leaf>) -> Option<Rc<Node>> {
    match &**root {
        Node::Leaf(_) => None,
        Node::Split { first, second, .. } => {
            for child in [first, second] {
                let child = child.borrow().clone();
                if matches!(&*child, Node::Leaf(found) if Rc::ptr_eq(found, leaf)) {
                    return Some(root.clone());
                }
                if let Some(found) = find_parent_of_leaf(&child, leaf) {
                    return Some(found);
                }
            }
            None
        }
    }
}

/// Which half of its parent a leaf sits in.
fn position_of_leaf(parent: &Node, leaf: &Rc<Leaf>) -> Half {
    if let Node::Split { first, .. } = parent {
        if matches!(&**first.borrow(), Node::Leaf(found) if Rc::ptr_eq(found, leaf)) {
            return Half::First;
        }
    }
    Half::Second
}

/// The stored child node of a split, by half.
fn child_node(parent: &Node, half: Half) -> Option<Rc<Node>> {
    match parent {
        Node::Split { first, second, .. } => Some(match half {
            Half::First => first.borrow().clone(),
            Half::Second => second.borrow().clone(),
        }),
        Node::Leaf(_) => None,
    }
}

/// Swap the child of a split that holds `leaf` for another node.
fn replace_leaf(parent: &Rc<Node>, leaf: &Rc<Leaf>, new: Rc<Node>) {
    if let Node::Split {
        paned,
        first,
        second,
        ..
    } = &**parent
    {
        match position_of_leaf(parent, leaf) {
            Half::First => {
                *first.borrow_mut() = new.clone();
                paned.set_start_child(Some(&new.widget()));
            }
            Half::Second => {
                *second.borrow_mut() = new.clone();
                paned.set_end_child(Some(&new.widget()));
            }
        }
    }
}

/// Swap a child of a split for another node, by node identity.
fn replace_child(parent: &Rc<Node>, old: &Rc<Node>, new: Rc<Node>) {
    if let Node::Split {
        paned,
        first,
        second,
        ..
    } = &**parent
    {
        if Rc::ptr_eq(&first.borrow(), old) {
            *first.borrow_mut() = new.clone();
            paned.set_start_child(Some(&new.widget()));
        } else if Rc::ptr_eq(&second.borrow(), old) {
            *second.borrow_mut() = new.clone();
            paned.set_end_child(Some(&new.widget()));
        }
    }
}

/// The node that keeps its place when a split loses one half.
fn sibling_of(parent: &Node, target: &Rc<Node>) -> Rc<Node> {
    if let Node::Split { first, second, .. } = parent {
        if Rc::ptr_eq(&first.borrow(), target) {
            return second.borrow().clone();
        }
        return first.borrow().clone();
    }
    target.clone()
}

/// A stored tab for a slot, without importing the db module's builder.
fn db_tab(slot: Slot, program_id: String) -> crate::db::Tab {
    crate::db::Tab::new(slot, program_id)
}


/// Remove an emptied pane: its sibling takes its place, or the root stays.
fn collapse_leaf(workspace: &Rc<Workspace>, leaf: &Rc<Leaf>) {
    let root = workspace.root.borrow().clone();
    if let Some(parent) = find_parent_of_leaf(&root, leaf) {
        let buried = child_node(&parent, position_of_leaf(&parent, leaf));
        let survivor = buried
            .map(|node| sibling_of(&parent, &node))
            .unwrap_or_else(|| root.clone());
        match find_parent(&root, &parent) {
            Some(grandparent) => replace_child(&grandparent, &parent, survivor.clone()),
            None => {
                workspace.holder.remove(&root.widget());
                workspace.holder.append(&survivor.widget());
                *workspace.root.borrow_mut() = survivor.clone();
            }
        }
    }
    if let Some(next) = workspace.all_leaves().first() {
        *workspace.focused.borrow_mut() = next.clone();
    }
}

/// Wire a leaf's tab strip: closing a tab stores the change and tidies up.
fn wire_close_page(db: SharedDb, workspace: Rc<Workspace>, leaf: Rc<Leaf>) {
    let tab_view = leaf.tab_view.clone();
    tab_view.connect_close_page(move |view, page| {
        leaf.remove_page(page);
        if leaf.is_empty() {
            collapse_leaf(&workspace, &leaf);
        }
        persist_tabs(&db, &workspace);
        view.close_page_finish(page, true);
        true
    });
}

/// Whatever you click in focuses that pane, which is what pane actions and zoom
/// act on.
fn wire_focus(workspace: &Rc<Workspace>, leaf: &Rc<Leaf>) {
    let focus = gtk::EventControllerFocus::new();
    let focused = workspace.focused.clone();
    let leaf_for_focus = leaf.clone();
    focus.connect_enter(move |_| {
        *focused.borrow_mut() = leaf_for_focus.clone();
    });
    leaf.widget.add_controller(focus);
}

/// Store a workspace's tabs: every pane's, in tree order.
fn persist_tabs(db: &Db, workspace: &Rc<Workspace>) {
    let mut tabs = Vec::new();
    for leaf in workspace.all_leaves() {
        tabs.extend(leaf.to_tabs(workspace.project.id));
    }
    if let Err(error) = db.set_tabs(workspace.project.id, &tabs) {
        eprintln!("radar: could not store tabs: {error}");
    }
}

/// An icon for a tab, by slot.
fn icon_for(slot: Slot) -> Option<gio::Icon> {
    let name = match slot {
        Slot::Editor => "accessories-text-editor-symbolic",
        Slot::Agent => "utilities-terminal-symbolic",
        Slot::Diff => "view-list-symbolic",
        Slot::Shell => "utilities-terminal-symbolic",
        Slot::Custom => "application-x-executable-symbolic",
    };
    Some(gio::ThemedIcon::new(name).upcast())
}
