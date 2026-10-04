//! The preferences dialog.
//!
//! Each is a small modal window over the main one. Rows carry their payload in
//! the activation handler rather than in widget data, which keeps everything
//! safe and explicit. Adding a project happens from the sidebar's search.

use std::rc::Rc;

use adw::prelude::*;

use crate::db::{Db, Slot};
use crate::programs;

type SharedDb = Rc<Db>;

/// Preferences: the preferred program per slot, and agent flag policy.
pub fn preferences<F: Fn() + 'static>(
    parent: &impl IsA<gtk::Window>,
    db: &SharedDb,
    on_changed: F,
) {
    let dialog = gtk::Window::builder()
        .title("Preferences")
        .default_width(620)
        .modal(true)
        .transient_for(parent)
        .build();

    let page = gtk::Box::new(gtk::Orientation::Vertical, 16);
    page.set_margin_top(18);
    page.set_margin_bottom(18);
    page.set_margin_start(18);
    page.set_margin_end(18);

    let prefs = db.preferences().unwrap_or_default();
    let on_changed = Rc::new(on_changed);

    let slots = gtk::ListBox::new();
    slots.set_selection_mode(gtk::SelectionMode::None);
    slots.add_css_class("boxed-list");

    for slot in [Slot::Editor, Slot::Agent, Slot::Diff, Slot::Shell] {
        let candidates = programs::candidates_for_slot(slot, &prefs);
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
        let label = gtk::Label::new(Some(&format!("Preferred {}", slot.as_str())));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        box_.append(&label);
        box_.append(&dropdown);
        item.set_child(Some(&box_));
        slots.append(&item);

        let db = db.clone();
        let on_changed = on_changed.clone();
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
            on_changed();
        });
    }

    slots.append(&reviewer_row(db, None, on_changed.clone()));

    let flags_item = gtk::ListBoxRow::new();
    flags_item.set_activatable(false);
    let flags_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    flags_box.set_margin_top(10);
    flags_box.set_margin_bottom(10);
    flags_box.set_margin_start(12);
    flags_box.set_margin_end(12);
    let flags_label = gtk::Label::new(Some("Start agents with skip-permission flags"));
    flags_label.set_xalign(0.0);
    flags_label.set_hexpand(true);
    flags_box.append(&flags_label);
    let flags_switch = gtk::Switch::new();
    flags_switch.set_active(prefs.agent_auto_flags);
    flags_switch.set_valign(gtk::Align::Center);
    flags_box.append(&flags_switch);
    flags_item.set_child(Some(&flags_box));
    slots.append(&flags_item);

    let db_handle = db.clone();
    let on_changed_flags = on_changed.clone();
    flags_switch.connect_active_notify(move |switch| {
        let mut prefs = db_handle.preferences().unwrap_or_default();
        prefs.agent_auto_flags = switch.is_active();
        if let Err(error) = db_handle.set_preferences(&prefs) {
            eprintln!("radar: {error}");
        }
        on_changed_flags();
    });

    page.append(&slots);
    let note = gtk::Label::new(Some(
        "Preferred programs are used whenever a new Editor, Agent, Diff or Shell tab is opened.",
    ));
    note.add_css_class("dim-label");
    note.add_css_class("caption");
    note.set_wrap(true);
    note.set_xalign(0.0);
    page.append(&note);

    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&page)
        .build();
    dialog.set_child(Some(&scroll));
    dialog.present();
}

/// Per-project preferred programs, inheriting each global preference until
/// the user chooses an override.
pub fn project_preferences<F: Fn() + 'static>(
    parent: &impl IsA<gtk::Window>,
    db: &SharedDb,
    project_id: i64,
    project_name: &str,
    on_changed: F,
) {
    let dialog = gtk::Window::builder()
        .title(format!("{project_name} Defaults"))
        .default_width(560)
        .modal(true)
        .transient_for(parent)
        .build();
    let page = gtk::Box::new(gtk::Orientation::Vertical, 14);
    page.set_margin_top(18);
    page.set_margin_bottom(18);
    page.set_margin_start(18);
    page.set_margin_end(18);
    let global = db.preferences().unwrap_or_default();
    let settings = db.project_settings(project_id).unwrap_or_default();
    let on_changed = Rc::new(on_changed);
    let slots = gtk::ListBox::new();
    slots.set_selection_mode(gtk::SelectionMode::None);
    slots.add_css_class("boxed-list");

    for slot in [Slot::Editor, Slot::Agent, Slot::Diff, Slot::Shell] {
        let candidates = programs::candidates_for_slot(slot, &global);
        let global_name = global
            .get(slot)
            .and_then(programs::by_id)
            .map(|program| program.name)
            .unwrap_or_else(|| "Auto".to_string());
        let mut labels = vec![format!("Use global ({global_name})")];
        labels.extend(
            candidates
                .iter()
                .map(|program| format!("{} — {}", program.name, program.id)),
        );
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let dropdown = gtk::DropDown::from_strings(&label_refs);
        let selected = settings
            .get(slot)
            .and_then(|id| candidates.iter().position(|program| program.id == id))
            .map(|index| index as u32 + 1)
            .unwrap_or(0);
        dropdown.set_selected(selected);
        dropdown.set_valign(gtk::Align::Center);

        let item = gtk::ListBoxRow::new();
        item.set_activatable(false);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.set_margin_top(10);
        row.set_margin_bottom(10);
        row.set_margin_start(12);
        row.set_margin_end(12);
        let label = gtk::Label::new(Some(slot.as_str()));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        row.append(&label);
        row.append(&dropdown);
        item.set_child(Some(&row));
        slots.append(&item);

        let db = db.clone();
        let on_changed = on_changed.clone();
        dropdown.connect_selected_notify(move |dropdown| {
            let index = dropdown.selected();
            let program = if index == 0 {
                None
            } else {
                candidates
                    .get(index as usize - 1)
                    .map(|program| program.id.as_str())
            };
            if let Err(error) = db.set_project_preference(project_id, slot, program) {
                eprintln!("radar: {error}");
            }
            on_changed();
        });
    }

    slots.append(&reviewer_row(db, Some(project_id), on_changed.clone()));

    page.append(&slots);
    let note = gtk::Label::new(Some(
        "New panes use these defaults. Use global keeps the matching Preferences setting.",
    ));
    note.add_css_class("dim-label");
    note.add_css_class("caption");
    note.set_wrap(true);
    note.set_xalign(0.0);
    page.append(&note);
    dialog.set_child(Some(&page));
    dialog.present();
}

/// Reviewer selection is separate from the agent pane default: None means
/// manual review globally, and inheritance for a project.
fn reviewer_row(
    db: &SharedDb,
    project_id: Option<i64>,
    on_changed: Rc<dyn Fn()>,
) -> gtk::ListBoxRow {
    let prefs = db.preferences().unwrap_or_default();
    let current = match project_id {
        Some(id) => db
            .project_settings(id)
            .ok()
            .and_then(|settings| settings.reviewer),
        None => prefs.reviewer.clone(),
    };
    let global = prefs
        .reviewer
        .as_deref()
        .map(|id| {
            programs::by_id(id)
                .map(|program| program.name)
                .unwrap_or_else(|| id.to_string())
        })
        .unwrap_or_else(|| "Off".into());
    let mut choices: Vec<(Option<String>, String)> = vec![(
        None,
        match project_id {
            Some(_) => format!("Use global ({global})"),
            None => "Off — review manually".into(),
        },
    )];
    choices.extend(
        programs::candidates_for_slot(Slot::Agent, &prefs)
            .into_iter()
            .map(|program| {
                (
                    Some(program.id.clone()),
                    format!("{} — {}", program.name, program.id),
                )
            }),
    );
    if let Some(id) = current.as_ref() {
        if !choices.iter().any(|(value, _)| value.as_ref() == Some(id)) {
            choices.push((Some(id.clone()), format!("{id} — unavailable")));
        }
    }
    let labels: Vec<&str> = choices.iter().map(|(_, label)| label.as_str()).collect();
    let dropdown = gtk::DropDown::from_strings(&labels);
    dropdown.set_selected(
        choices
            .iter()
            .position(|(value, _)| *value == current)
            .unwrap_or(0) as u32,
    );
    dropdown.set_widget_name("reviewer-choice");
    dropdown.set_valign(gtk::Align::Center);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.set_margin_top(10);
    row.set_margin_bottom(10);
    row.set_margin_start(12);
    row.set_margin_end(12);
    let texts = gtk::Box::new(gtk::Orientation::Vertical, 4);
    texts.set_hexpand(true);
    let title = gtk::Label::new(Some("Automatic reviewer"));
    title.set_xalign(0.0);
    texts.append(&title);
    let note = gtk::Label::new(Some(
        "Choose an agent to review finished work automatically.",
    ));
    note.set_xalign(0.0);
    note.set_wrap(true);
    note.add_css_class("caption");
    note.add_css_class("dim-label");
    texts.append(&note);
    let error = gtk::Label::new(None);
    error.set_xalign(0.0);
    error.set_wrap(true);
    error.add_css_class("error");
    error.set_visible(false);
    texts.append(&error);
    row.append(&texts);
    row.append(&dropdown);
    let item = gtk::ListBoxRow::new();
    item.set_activatable(false);
    item.set_child(Some(&row));
    let db = db.clone();
    dropdown.connect_selected_notify(move |dropdown| {
        let Some((program, _)) = choices.get(dropdown.selected() as usize) else {
            return;
        };
        let result = match project_id {
            Some(id) => db.set_project_reviewer(id, program.as_deref()),
            None => db.set_reviewer(program.as_deref()).map(|_| ()),
        };
        match result {
            Ok(()) => {
                error.set_visible(false);
                on_changed();
            }
            Err(message) => {
                error.set_text(&format!("Could not save reviewer: {message}"));
                error.set_visible(true);
            }
        }
    });
    item
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a private D-Bus session and GTK display"]
    fn reviewer_choices_persist_override_and_inheritance() {
        gtk::init().unwrap();
        let home = tempfile::tempdir().unwrap();
        let db = Rc::new(Db::open(&crate::config::Paths::with_root(home.path())).unwrap());
        let project_dir = home.path().join("project");
        std::fs::create_dir(&project_dir).unwrap();
        let project = db.add_project(&project_dir).unwrap();
        db.set_reviewer(Some("custom-reviewer")).unwrap();
        db.set_project_reviewer(project.id, Some("project-reviewer"))
            .unwrap();
        fn dropdown(row: &gtk::ListBoxRow) -> gtk::DropDown {
            row.child()
                .unwrap()
                .last_child()
                .unwrap()
                .downcast()
                .unwrap()
        }
        let global = dropdown(&reviewer_row(&db, None, Rc::new(|| {})));
        assert_ne!(
            global.selected(),
            0,
            "unavailable saved choice remains selected"
        );
        let project_choice = dropdown(&reviewer_row(&db, Some(project.id), Rc::new(|| {})));
        assert_ne!(project_choice.selected(), 0);
        project_choice.set_selected(0);
        assert_eq!(
            db.reviewer(project.id).unwrap().as_deref(),
            Some("custom-reviewer")
        );
        assert!(db.project_settings(project.id).unwrap().reviewer.is_none());
        global.set_selected(0);
        assert!(db.reviewer(project.id).unwrap().is_none());
    }
}
