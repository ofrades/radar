//! The home panel: radar at rest.
//!
//! Shown when there is nothing else to show — no projects, no panes — and
//! reachable any time from the button by the logo, the dock, or `Alt+Home`.
//! It is the empty state with something to do: the program each slot uses,
//! the layout a new project opens with, and the two ways forward — the
//! sidebar's search for directories that already exist, and **New project…**
//! for a fresh folder with a `git init` in it.

use adw::prelude::*;

use super::SharedApp;
use crate::db::{NewWorkspaceLayout, Preferences, Slot};
use crate::programs;

/// Build the panel. Rebuilt every time home is shown, so the dropdowns always
/// say what the preferences say right now.
pub fn panel(app: &SharedApp) -> gtk::Widget {
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 20);
    content.add_css_class("home");
    content.set_halign(gtk::Align::Center);
    content.set_valign(gtk::Align::Center);
    content.set_width_request(480);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);
    content.set_margin_end(24);

    // The brand: the same logo the sidebar header wears, with the pitch.
    let brand = gtk::Box::new(gtk::Orientation::Vertical, 6);
    brand.set_halign(gtk::Align::Center);
    let logo = gtk::Image::from_icon_name("radar");
    logo.add_css_class("home-logo");
    logo.set_pixel_size(56);
    brand.append(&logo);
    let title = gtk::Label::new(Some("Radar"));
    title.add_css_class("heading");
    brand.append(&title);
    let tagline = gtk::Label::new(Some(
        "One workspace per project — an agent, live changes, commands and an editor.",
    ));
    tagline.add_css_class("caption");
    tagline.add_css_class("dim-label");
    tagline.set_wrap(true);
    tagline.set_justify(gtk::Justification::Center);
    tagline.set_max_width_chars(46);
    brand.append(&tagline);
    content.append(&brand);

    // Setup: the program per slot, and the layout a new project opens with.
    let setup = gtk::ListBox::new();
    setup.set_selection_mode(gtk::SelectionMode::None);
    setup.add_css_class("boxed-list");
    let prefs = app.db.preferences().unwrap_or_default();
    for slot in [Slot::Agent, Slot::Diff, Slot::Shell, Slot::Editor] {
        setup.append(&program_row(app, slot, &prefs));
    }
    setup.append(&layout_row(app));
    content.append(&setup);

    let note = gtk::Label::new(Some(
        "Auto picks the best installed program, and these fill every new pane. \
         The layout is what a new project opens with.",
    ));
    note.add_css_class("caption");
    note.add_css_class("dim-label");
    note.set_wrap(true);
    note.set_justify(gtk::Justification::Center);
    note.set_max_width_chars(46);
    content.append(&note);

    // The two ways forward.
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    actions.set_halign(gtk::Align::Center);
    let new_project = gtk::Button::with_label("New project…");
    new_project.add_css_class("suggested-action");
    new_project.add_css_class("pill");
    new_project.set_tooltip_text(Some(
        "Pick or create a folder, run git init in it, and open it as a project",
    ));
    {
        let app = app.clone();
        new_project.connect_clicked(move |_| new_project_dialog(&app));
    }
    actions.append(&new_project);
    let find = gtk::Button::with_label("Find projects");
    find.add_css_class("pill");
    find.set_action_name(Some("win.find-projects"));
    find.set_tooltip_text(Some(
        "Search in the sidebar — matching directories under the scan root join the list with a +",
    ));
    actions.append(&find);
    content.append(&actions);

    scroll.set_child(Some(&content));
    scroll.upcast()
}

/// One setup row: the slot's icon and name, and a program dropdown, exactly
/// the control the preferences dialog uses — it writes the same preference.
fn program_row(app: &SharedApp, slot: Slot, prefs: &Preferences) -> gtk::ListBoxRow {
    let candidates = programs::candidates_for_slot(slot, prefs);
    let mut labels: Vec<String> = vec!["Auto (best installed)".to_string()];
    labels.extend(candidates.iter().map(|p| format!("{} — {}", p.name, p.id)));
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let dropdown = gtk::DropDown::from_strings(&label_refs);
    let selected = prefs
        .get(slot)
        .and_then(|id| candidates.iter().position(|p| p.id == id))
        .map(|index| index as u32 + 1)
        .unwrap_or(0);
    dropdown.set_selected(selected);
    dropdown.set_valign(gtk::Align::Center);

    let item = gtk::ListBoxRow::new();
    item.set_activatable(false);
    let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    box_.set_margin_top(10);
    box_.set_margin_bottom(10);
    box_.set_margin_start(12);
    box_.set_margin_end(12);
    let icon = gtk::Image::from_icon_name(super::icon_name(slot));
    icon.add_css_class("dim-label");
    icon.set_pixel_size(16);
    icon.set_valign(gtk::Align::Center);
    box_.append(&icon);
    let label = gtk::Label::new(Some(super::label_for(slot)));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    box_.append(&label);
    box_.append(&dropdown);
    item.set_child(Some(&box_));

    let db = app.db.clone();
    let app_for_change = app.clone();
    dropdown.connect_selected_notify(move |dropdown| {
        let index = dropdown.selected();
        let program = if index == 0 {
            None
        } else {
            candidates.get(index as usize - 1).map(|p| p.id.clone())
        };
        if let Err(error) = db.set_preference(slot, program.as_deref()) {
            eprintln!("radar: {error}");
        }
        // The dock's availability and the pane menus follow the preference.
        app_for_change.sync_toggles();
        app_for_change.refresh_menus();
    });
    item
}

/// The layout row: what a brand-new workspace opens with.
fn layout_row(app: &SharedApp) -> gtk::ListBoxRow {
    let item = gtk::ListBoxRow::new();
    item.set_activatable(false);
    let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    box_.set_margin_top(10);
    box_.set_margin_bottom(10);
    box_.set_margin_start(12);
    box_.set_margin_end(12);
    let icon = gtk::Image::from_icon_name("view-grid-symbolic");
    icon.add_css_class("dim-label");
    icon.set_pixel_size(16);
    icon.set_valign(gtk::Align::Center);
    box_.append(&icon);
    let label = gtk::Label::new(Some("Layout"));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    box_.append(&label);

    let strings: Vec<String> = NewWorkspaceLayout::ALL
        .iter()
        .map(|layout| layout.label().to_string())
        .collect();
    let label_refs: Vec<&str> = strings.iter().map(String::as_str).collect();
    let dropdown = gtk::DropDown::from_strings(&label_refs);
    let selected = app
        .db
        .ui_prefs()
        .ok()
        .and_then(|prefs| prefs.layout)
        .and_then(|layout| {
            NewWorkspaceLayout::ALL
                .iter()
                .position(|candidate| *candidate == layout)
        })
        .unwrap_or(0) as u32;
    dropdown.set_selected(selected);
    dropdown.set_valign(gtk::Align::Center);
    box_.append(&dropdown);
    item.set_child(Some(&box_));
    item.set_tooltip_text(Some("What a new project opens with"));

    let db = app.db.clone();
    dropdown.connect_selected_notify(move |dropdown| {
        let layout = NewWorkspaceLayout::ALL[dropdown.selected() as usize];
        let mut prefs = db.ui_prefs().unwrap_or_default();
        prefs.layout = Some(layout);
        if let Err(error) = db.set_ui_prefs(&prefs) {
            eprintln!("radar: {error}");
        }
    });
    item
}

/// Pick or create a folder, then make it a project: created if missing,
/// `git init` when it is not a repository already, added and opened.
fn new_project_dialog(app: &SharedApp) {
    #[allow(deprecated)]
    let dialog = gtk::FileChooserDialog::new(
        Some("New project — pick or create a folder"),
        Some(&app.window),
        gtk::FileChooserAction::SelectFolder,
        &[
            ("Cancel", gtk::ResponseType::Cancel),
            ("Create project", gtk::ResponseType::Accept),
        ],
    );
    let app = app.clone();
    #[allow(deprecated)]
    dialog.connect_response(move |dialog, response| {
        let path = (response == gtk::ResponseType::Accept)
            .then(|| dialog.file())
            .and_then(|file| file.and_then(|file| file.path()));
        dialog.close();
        if let Some(path) = path {
            create_project(&app, path);
        }
    });
    dialog.present();
}

/// The chosen folder becomes a project: directory if missing, git if bare.
/// The paths that end here have already been through a file chooser, so the
/// quick synchronous git init is not worth a thread.
pub(super) fn create_project(app: &SharedApp, path: std::path::PathBuf) {
    if let Err(error) = std::fs::create_dir_all(&path) {
        app.toast(&format!(
            "Could not create {}: {error}",
            crate::db::abbreviate(&path)
        ));
        return;
    }
    let existing_repo = path.join(".git").is_dir();
    let initialised = existing_repo || git_init(&path);
    let project = match app.db.add_project(&path) {
        Ok(project) => project,
        Err(error) => {
            app.toast(&format!("Could not add the project: {error}"));
            return;
        }
    };
    super::App::refresh_projects(app);
    app.select_project(project.id);
    let detail = if existing_repo {
        "already a git repository"
    } else if initialised {
        "new git repository"
    } else {
        "git init failed"
    };
    app.toast(&format!("{} · {detail}", project.name));
}

fn git_init(path: &std::path::Path) -> bool {
    std::process::Command::new("git")
        .arg("init")
        .current_dir(path)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}
