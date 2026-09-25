//! The app window: a project sidebar on the left, tabbed workspaces on the
//! right, and one preference per slot deciding what fills a tab.
//!
//! State that outlives the process (projects, tabs, preferences) lives in
//! SQLite through [`Db`]. State that does not (which programs are running) lives
//! in [`Workspace`], one per project that has been opened, so switching projects
//! never interrupts an agent.

mod dialogs;
mod pane;
mod theme;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use anyhow::Result;
use adw::prelude::*;
use gtk::gio;

use crate::config::Paths;
use crate::db::{Db, Project, Slot, Tab};
use crate::programs::{self, CommandSpec, LaunchOptions, Program};

use pane::Pane;
pub use theme::Theme;

type SharedDb = Rc<Db>;

/// One live tab: the widget, and what it is so we can store it again.
struct Page {
    page: adw::TabPage,
    slot: Slot,
    program_id: String,
    title: String,
    extra_args: Vec<String>,
    pane: Rc<Pane>,
}

/// A project's open workspace. Kept alive while atlas runs so long-running
/// programs (agents, watch jobs) survive switching projects.
struct Workspace {
    project: Project,
    tab_view: adw::TabView,
    pages: RefCell<Vec<Page>>,
}

impl Workspace {
    /// What this workspace should be stored as.
    fn to_tabs(&self) -> Vec<Tab> {
        self.pages
            .borrow()
            .iter()
            .enumerate()
            .map(|(index, page)| {
                let mut tab = Tab::new(page.slot, page.program_id.clone());
                tab.title = Some(page.title.clone());
                tab.extra_args = page.extra_args.clone();
                tab.sort_order = index as i64;
                tab
            })
            .collect()
    }
}

struct App {
    db: SharedDb,
    theme: RefCell<Theme>,
    window: adw::ApplicationWindow,
    split: adw::OverlaySplitView,
    sidebar_list: gtk::ListBox,
    sidebar_search: gtk::SearchEntry,
    subtitle: gtk::Label,
    stack: gtk::Stack,
    toasts: adw::ToastOverlay,
    new_tab_button: gtk::MenuButton,
    main_menu_button: gtk::MenuButton,
    workspaces: RefCell<HashMap<i64, Rc<Workspace>>>,
    projects: RefCell<Vec<Project>>,
    /// Sidebar rows, in list order: project id and widget.
    rows: RefCell<Vec<(i64, gtk::ListBoxRow)>>,
    current: RefCell<Option<i64>>,
}

type SharedApp = Rc<App>;

/// Open the app.
pub fn run(paths: Paths, db: Db) -> Result<()> {
    let app = adw::Application::builder()
        .application_id("dev.omarchy.Atlas")
        .build();
    let paths = Rc::new(paths);
    let db = Rc::new(db);

    app.connect_activate(move |app| {
        let window = build_window(app, &paths, &db);
        window.present();
    });

    // Run with a clean argv: GApplication would otherwise try to interpret
    // atlas's own command line (and fail on arguments it does not know).
    let _ = app.run_with_args(&["atlas"]);
    Ok(())
}

fn build_window(app: &adw::Application, paths: &Rc<Paths>, db: &SharedDb) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Atlas")
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

    let sidebar_search = gtk::SearchEntry::new();
    sidebar_search.set_placeholder_text(Some("Filter projects…"));
    sidebar_search.set_hexpand(true);

    let add_button = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add project (Ctrl+Shift+N)")
        .build();
    add_button.add_css_class("flat");
    add_button.set_action_name(Some("win.add-project"));

    let sidebar_top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    sidebar_top.set_margin_top(8);
    sidebar_top.set_margin_bottom(8);
    sidebar_top.set_margin_start(10);
    sidebar_top.set_margin_end(10);
    sidebar_top.append(&sidebar_search);
    sidebar_top.append(&add_button);

    let sidebar_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_box.append(&sidebar_top);
    sidebar_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    sidebar_box.append(&sidebar_scroll);

    // ---- content ----
    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .vexpand(true)
        .build();

    // Empty states live in the same stack as the workspaces.
    let no_projects = status_page(
        "folder-open-symbolic",
        "No projects yet",
        "Add a directory to get started. Each project gets its own set of tabs: an editor, an agent, a live diff.",
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

    // OverlaySplitView gives us a sidebar we can show, hide and collapse, and
    // it keeps the content area simple: no navigation pages in the way.
    let split = adw::OverlaySplitView::builder()
        .sidebar_width_fraction(0.20)
        .collapsed(false)
        .build();
    split.set_sidebar(Some(&sidebar_box));
    split.set_content(Some(&stack));

    // ---- header bar ----
    let subtitle = gtk::Label::new(Some("no project"));
    subtitle.add_css_class("dim-label");
    subtitle.add_css_class("caption");

    let title_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let title = gtk::Label::new(Some("Atlas"));
    title.add_css_class("heading");
    title_box.append(&title);
    title_box.append(&subtitle);

    let sidebar_toggle = gtk::ToggleButton::builder()
        .icon_name("sidebar-show-symbolic")
        .tooltip_text("Toggle sidebar (F9)")
        .build();
    sidebar_toggle.add_css_class("flat");

    let new_tab_button = gtk::MenuButton::builder()
        .icon_name("tab-new-symbolic")
        .tooltip_text("New tab (Ctrl+Shift+T)")
        .build();
    new_tab_button.add_css_class("flat");

    let main_menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Main menu")
        .build();
    main_menu_button.add_css_class("flat");

    let header = adw::HeaderBar::builder().build();
    header.set_title_widget(Some(&title_box));
    header.pack_start(&sidebar_toggle);
    header.pack_end(&main_menu_button);
    header.pack_end(&new_tab_button);

    let toasts = adw::ToastOverlay::new();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&split));
    toasts.set_child(Some(&toolbar));
    window.set_content(Some(&toasts));

    let app_state = Rc::new(App {
        db: db.clone(),
        theme: RefCell::new(Theme::load()),
        window: window.clone(),
        split: split.clone(),
        sidebar_list: sidebar_list.clone(),
        sidebar_search: sidebar_search.clone(),
        subtitle,
        stack,
        toasts,
        new_tab_button,
        main_menu_button,
        workspaces: RefCell::new(HashMap::new()),
        projects: RefCell::new(Vec::new()),
        rows: RefCell::new(Vec::new()),
        current: RefCell::new(None),
    });
    window.set_tooltip_text(Some(&format!("state: {}", paths.database().display())));

    register_actions(&app_state, app);
    connect_widgets(&app_state);
    app_state.refresh_projects();
    if let Ok(prefs) = app_state.db.ui_prefs() {
        if let Some(id) = prefs.last_project {
            app_state.select_project(id);
        }
    }
    app_state.reload_theme();
    watch_theme(&app_state);
    app_state
        .window
        .clone()
        .upcast::<gtk::Widget>()
        .grab_focus();
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
    // Selecting a project.
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

    // Filtering the sidebar.
    {
        let entry = app.sidebar_search.clone();
        let app = app.clone();
        entry.connect_search_changed(move |entry| {
            app.filter_sidebar(&entry.text());
        });
    }
}

/// Actions are the single entry point for both menus and keyboard shortcuts.
fn register_actions(app: &SharedApp, gtk_app: &adw::Application) {
    // The sidebar toggle: a stateful action, bound to the button's "active"
    // property and applied to the split view. This is the libadwaita pattern.
    {
        let action = gio::SimpleAction::new_stateful("show-sidebar", None, &true.to_variant());
        let split = app.split.clone();
        action.connect_activate(move |action, _| {
            let showing = action.state().and_then(|v| v.get::<bool>()).unwrap_or(true);
            let next = !showing;
            action.set_state(&next.to_variant());
            split.set_show_sidebar(next);
        });
        app.window.add_action(&action);
        if let Some(toggle) = find_sidebar_toggle(&app.window) {
            action
                .bind_property("state", &toggle, "active")
                .bidirectional()
                .sync_create()
                .build();
        }
    }

    let add = |name: &str, handler: Box<dyn Fn()>| {
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(move |_, _| handler());
        app.window.add_action(&action);
    };

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
            "choose-program",
            Box::new(move || {
                let window = app.window.clone();
                let app = app.clone();
                dialogs::choose_program(&window, move |program| {
                    app.add_tab_for_program(program);
                });
            }),
        );
    }
    for slot in [Slot::Editor, Slot::Agent, Slot::Diff, Slot::Shell] {
        let app = app.clone();
        let name = format!("new-tab-{}", slot.as_str());
        add(&name, Box::new(move || app.add_tab_for_slot(slot)));
    }
    {
        let app = app.clone();
        add(
            "close-tab",
            Box::new(move || {
                if let Some(workspace) = app.current_workspace() {
                    if let Some(page) = workspace.tab_view.selected_page() {
                        workspace.tab_view.close_page(&page);
                    }
                }
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
                app.toast("Refreshed");
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
                        eprintln!("atlas: {error}");
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
                    eprintln!("atlas: {error}");
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
                    eprintln!("atlas: {error}");
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
                dialog.choose(
                    Some(&app.window),
                    None::<&gio::Cancellable>,
                    move |result| {
                        if result == Ok(1) {
                            if let Err(error) = app_for_response.db.remove_project(project.id) {
                                eprintln!("atlas: {error}");
                            }
                            app_for_response.workspaces.borrow_mut().remove(&project.id);
                            *app_for_response.current.borrow_mut() = None;
                            app_for_response.refresh_projects();
                            app_for_response.show_placeholder();
                            app_for_response.toast("Project removed");
                        }
                    },
                );
            }),
        );
    }
    for program in programs::external_programs() {
        let app = app.clone();
        let id = format!("open-external-{}", program.id);
        let label = program.name.clone();
        add(
            &id,
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

    // Keyboard shortcuts. Alt-free, terminal friendly combinations only.
    let accels: [(&str, &[&str]); 9] = [
        ("win.add-project", &["<Control><Shift>n"]),
        ("win.pick-program", &["<Control><Shift>p"]),
        ("win.new-tab-editor", &["<Control><Shift>e"]),
        ("win.new-tab-agent", &["<Control><Shift>a"]),
        ("win.new-tab-diff", &["<Control><Shift>g"]),
        ("win.close-tab", &["<Control><Shift>w"]),
        ("win.preferences", &["<Control>comma"]),
        ("win.refresh", &["<Control><Shift>r"]),
        ("win.show-sidebar", &["F9"]),
    ];
    for (action, keys) in accels {
        let _ = gtk_app.set_accels_for_action(action, keys);
    }
}

/// Find the sidebar toggle button in the header bar (it is the only
/// ToggleButton there).
fn find_sidebar_toggle(window: &adw::ApplicationWindow) -> Option<gtk::ToggleButton> {
    fn walk(widget: &gtk::Widget, depth: usize) -> Option<gtk::ToggleButton> {
        if depth > 4 {
            return None;
        }
        if let Some(toggle) = widget.downcast_ref::<gtk::ToggleButton>() {
            if toggle.icon_name().as_deref() == Some("sidebar-show-symbolic") {
                return Some(toggle.clone());
            }
        }
        let mut child = widget.first_child();
        while let Some(current) = child {
            if let Some(found) = walk(&current, depth + 1) {
                return Some(found);
            }
            child = current.next_sibling();
        }
        None
    }
    window.child().and_then(|child| walk(&child, 0))
}

fn watch_theme(app: &SharedApp) {
    let Some(dir) = theme::omarchy_theme_dir() else { return };
    let file = gio::File::for_path(dir.join("colors.toml"));
    let Ok(monitor) = file.monitor_file(gio::FileMonitorFlags::NONE, None::<&gio::Cancellable>) else {
        return;
    };
    let app = app.clone();
    monitor.connect_changed(move |_, _, _, _| {
        // omarchy rewrites the file on theme change; re-read and re-paint.
        app.reload_theme();
    });
    // The monitor must outlive this function.
    std::mem::forget(monitor);
}

impl App {
    fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }

    /// Which project does a sidebar row belong to?
    fn id_for_row(&self, row: &gtk::ListBoxRow) -> Option<i64> {
        self.rows
            .borrow()
            .iter()
            .find(|(_, widget)| widget == row)
            .map(|(id, _)| *id)
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
        if let Some(manager) = adw::StyleManager::default().downcast::<adw::StyleManager>().ok() {
            manager.set_color_scheme(if theme.dark {
                adw::ColorScheme::ForceDark
            } else {
                adw::ColorScheme::ForceLight
            });
        }
        for workspace in self.workspaces.borrow().values() {
            for page in workspace.pages.borrow().iter() {
                page.pane.apply_theme(&theme);
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

    /// Rebuild the sidebar from the database, keeping the selection.
    fn refresh_projects(&self) {
        let projects = self.db.projects().unwrap_or_default();
        let selected = *self.current.borrow();
        *self.projects.borrow_mut() = projects.clone();

        while let Some(child) = self.sidebar_list.first_child() {
            self.sidebar_list.remove(&child);
        }
        self.rows.borrow_mut().clear();
        if projects.is_empty() {
            let hint = gtk::Label::new(Some("No projects yet.\nPress the + button to add one."));
            hint.add_css_class("dim-label");
            hint.set_justify(gtk::Justification::Center);
            hint.set_margin_top(24);
            hint.set_wrap(true);
            self.sidebar_list.append(&hint);
            self.show_placeholder();
            return;
        }

        for project in &projects {
            let row = self.build_project_row(project);
            self.rows.borrow_mut().push((project.id, row.clone()));
            self.sidebar_list.append(&row);
        }
        self.filter_sidebar(&self.sidebar_search.text());
        self.refresh_menus();

        if let Some(id) = selected {
            self.select_row_for(id);
        }
        if selected.is_none() || selected.is_some_and(|id| !projects.iter().any(|p| p.id == id)) {
            if let Some(first) = projects.first() {
                self.select_project(first.id);
            }
        }
    }

    fn build_project_row(&self, project: &Project) -> gtk::ListBoxRow {
        let status = crate::git::status(&project.path);
        let row = gtk::ListBoxRow::new();

        let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        box_.set_margin_top(6);
        box_.set_margin_bottom(6);
        box_.set_margin_start(6);
        box_.set_margin_end(6);

        let texts = gtk::Box::new(gtk::Orientation::Vertical, 1);
        texts.set_hexpand(true);
        let name = gtk::Label::new(Some(&project.name));
        name.set_xalign(0.0);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        if project.pinned {
            name.set_text(&format!("📌 {}", project.name));
        }
        texts.append(&name);

        let summary = if project.is_missing() {
            "directory missing".to_string()
        } else {
            status.summary()
        };
        // The name is already the last path component, so the useful second
        // line is where it lives.
        let parent = project
            .path
            .parent()
            .map(crate::db::abbreviate)
            .unwrap_or_else(|| project.display_path());
        let subtitle = gtk::Label::new(Some(&format!("{summary}  ·  {parent}")));
        subtitle.set_xalign(0.0);
        subtitle.add_css_class("caption");
        subtitle.add_css_class("dim-label");
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        texts.append(&subtitle);

        box_.append(&texts);
        if status.dirty {
            let dot = gtk::Label::new(Some(&format!("●{}", status.changed)));
            dot.add_css_class("caption");
            dot.add_css_class("accent");
            dot.set_valign(gtk::Align::Center);
            box_.append(&dot);
        }
        row.set_child(Some(&box_));
        row.set_tooltip_text(Some(&project.display_path()));
        row
    }

    fn select_row_for(&self, id: i64) {
        let target = self
            .rows
            .borrow()
            .iter()
            .find(|(row_id, _)| *row_id == id)
            .map(|(_, row)| row.clone());
        if let Some(row) = target {
            self.sidebar_list.select_row(Some(&row));
        }
    }

    fn filter_sidebar(&self, query: &str) {
        let matcher = fuzzy_matcher::skim::SkimMatcherV2::default().ignore_case();
        use fuzzy_matcher::FuzzyMatcher;
        let projects = self.projects.borrow();
        for (id, row) in self.rows.borrow().iter() {
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
        self.subtitle.set_text("no project");
    }

    /// Switch to a project, creating its workspace on first visit.
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
        self.stack
            .set_visible_child_name(&format!("project-{id}"));
        self.subtitle
            .set_text(&format!("{} · {}", project.name, project.display_path()));
        let _ = self.db.touch_project(id);
        let _ = self.db.remember_last_project(Some(id));
        let _ = workspace;
        self.refresh_menus();
        self.select_row_for(id);
    }

    /// Get or create the workspace for a project, restoring its tabs.
    fn workspace_for(&self, project: &Project) -> Rc<Workspace> {
        if let Some(existing) = self.workspaces.borrow().get(&project.id) {
            return existing.clone();
        }

        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let tab_view = adw::TabView::new();
        tab_view.set_shortcuts(adw::TabViewShortcuts::ALL_SHORTCUTS);
        tab_view.set_vexpand(true);
        tab_view.set_hexpand(true);

        let tab_bar = adw::TabBar::builder().view(&tab_view).build();
        tab_bar.set_expand_tabs(false);

        let tab_menu = gtk::MenuButton::builder()
            .icon_name("tab-new-symbolic")
            .tooltip_text("New tab")
            .build();
        tab_menu.add_css_class("flat");
        tab_bar.set_end_action_widget(Some(&tab_menu));

        container.append(&tab_bar);
        container.append(&tab_view);

        let workspace = Rc::new(Workspace {
            project: project.clone(),
            tab_view: tab_view.clone(),
            pages: RefCell::new(Vec::new()),
        });

        // Close the tab when its page is closed, and let libadwaita finish.
        {
            let db = self.db.clone();
            let workspace_for_close = workspace.clone();
            tab_view.connect_close_page(move |view, page| {
                let mut pages = workspace_for_close.pages.borrow_mut();
                if let Some(index) = pages.iter().position(|p| p.page == *page) {
                    pages.remove(index);
                }
                drop(pages);
                let tabs = workspace_for_close.to_tabs();
                if let Err(error) = db.set_tabs(workspace_for_close.project.id, &tabs) {
                    eprintln!("atlas: could not store tabs: {error}");
                }
                view.close_page_finish(page, true);
                true
            });
        }

        self.stack
            .add_named(&container, Some(&format!("project-{}", project.id)));
        self.workspaces
            .borrow_mut()
            .insert(project.id, workspace.clone());

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
                .filter_map(|slot| programs::for_slot(slot, &preferences).map(|p| Tab::new(slot, p.id)))
                .collect()
        } else {
            stored
        };

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

        for tab in tabs {
            let Some(program) = programs::by_id(&tab.program_id) else {
                continue;
            };
            if !program.installed() {
                self.toast(&format!("{} is not installed", program.name));
                continue;
            }
            let mut options = self.launch_options();
            options.extra_args = tab.extra_args.clone();
            let title = tab.display_title(&program.name);
            let spec = program.command_spec(&options);
            self.open_page(&workspace, program, spec, title, tab.slot, tab.extra_args.clone());
        }
        self.persist_tabs(&workspace);
        workspace
    }

    /// Add a tab running the preferred program for a slot.
    fn add_tab_for_slot(&self, slot: Slot) {
        let Some(workspace) = self.current_workspace() else {
            self.toast("Select a project first");
            return;
        };
        let preferences = self.db.preferences().unwrap_or_default();
        let Some(program) = programs::for_slot(slot, &preferences) else {
            self.toast(&format!("No {} installed", slot.label().to_lowercase()));
            return;
        };
        self.add_tab(workspace, program, slot);
    }

    fn add_tab_for_program(&self, program: Program) {
        let Some(workspace) = self.current_workspace() else {
            self.toast("Select a project first");
            return;
        };
        let slot = program.kind.default_slot();
        self.add_tab(workspace, program, slot);
    }

    fn add_tab(&self, workspace: Rc<Workspace>, program: Program, slot: Slot) {
        let options = self.launch_options();
        let spec = program.command_spec(&options);
        let name = program.name.clone();
        self.open_page(&workspace, program, spec, name.clone(), slot, Vec::new());
        self.persist_tabs(&workspace);
        self.toast(&format!("Opened {name}"));
    }

    /// The single place that creates a tab.
    fn open_page(
        &self,
        workspace: &Rc<Workspace>,
        program: Program,
        spec: CommandSpec,
        title: String,
        slot: Slot,
        extra_args: Vec<String>,
    ) {
        let theme = self.theme.borrow().clone();
        let pane = Rc::new(Pane::spawn(&spec, &workspace.project.path, &theme, &title));
        let page = workspace.tab_view.append(pane.widget());
        page.set_title(&title);
        page.set_tooltip(&format!(
            "{} · {}\n{}",
            program.name,
            pane.command(),
            if pane.is_live() {
                "running inside atlas"
            } else {
                "opens in a separate terminal window"
            }
        ));
        if let Some(icon) = icon_for(slot) {
            page.set_icon(Some(&icon));
        }

        workspace.pages.borrow_mut().push(Page {
            page: page.clone(),
            slot,
            program_id: program.id.clone(),
            title,
            extra_args,
            pane,
        });
        workspace.tab_view.set_selected_page(&page);
    }

    fn persist_tabs(&self, workspace: &Rc<Workspace>) {
        let tabs = workspace.to_tabs();
        if let Err(error) = self.db.set_tabs(workspace.project.id, &tabs) {
            eprintln!("atlas: could not store tabs: {error}");
        }
    }

    /// Rebuild the "new tab" and main menus, which depend on preferences and
    /// on the current selection.
    fn refresh_menus(&self) {
        let preferences = self.db.preferences().unwrap_or_default();
        let slots = [Slot::Editor, Slot::Agent, Slot::Diff, Slot::Shell];

        let menu = gio::Menu::new();
        let section = gio::Menu::new();
        for slot in slots {
            let name = programs::for_slot(slot, &preferences)
                .map(|p| p.name)
                .unwrap_or_else(|| "nothing installed".into());
            let item = gio::MenuItem::new(
                Some(&format!("{} — {}", slot.label(), name)),
                Some(&format!("win.new-tab-{}", slot.as_str())),
            );
            section.append_item(&item);
        }
        menu.append_section(None, &section);
        let more = gio::Menu::new();
        more.append(Some("Choose program…"), Some("win.pick-program"));
        menu.append_section(None, &more);
        self.new_tab_button.set_menu_model(Some(&menu));

        let main = gio::Menu::new();
        let project_menu = gio::Menu::new();
        project_menu.append(Some("Rename project…"), Some("win.project-rename"));
        project_menu.append(Some("Pin or unpin"), Some("win.project-pin"));
        project_menu.append(Some("Move up"), Some("win.project-move-up"));
        project_menu.append(Some("Move down"), Some("win.project-move-down"));
        project_menu.append(Some("Remove project…"), Some("win.project-remove"));
        main.append_section(None, &project_menu);

        let externals = programs::external_programs();
        if !externals.is_empty() {
            let open_in = gio::Menu::new();
            for program in externals {
                open_in.append(
                    Some(&format!("Open in {}", program.name)),
                    Some(&format!("win.open-external-{}", program.id)),
                );
            }
            main.append_section(None, &open_in);
        }

        let app_menu = gio::Menu::new();
        app_menu.append(Some("Add project…"), Some("win.add-project"));
        app_menu.append(Some("Preferences…"), Some("win.preferences"));
        app_menu.append(Some("Refresh"), Some("win.refresh"));
        main.append_section(None, &app_menu);

        self.main_menu_button.set_menu_model(Some(&main));
    }
}

/// An icon for a tab, by slot or by program kind.
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
