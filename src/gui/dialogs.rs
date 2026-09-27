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
