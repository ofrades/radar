//! Dialogs: adding a project, choosing a program, and preferences.
//!
//! Each is a small modal window over the main one. Rows carry their payload in
//! the activation handler rather than in widget data, which keeps everything
//! safe and explicit.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;

use crate::db::{Db, Slot};
use crate::discover::{self, Candidate};
use crate::programs::{self, Program};

type SharedDb = Rc<Db>;

/// A modal window with a search box and a list.
fn picker_window(
    title: &str,
    placeholder: &str,
) -> (gtk::Window, gtk::Box, gtk::ListBox, gtk::SearchEntry) {
    let window = gtk::Window::builder()
        .title(title)
        .default_width(640)
        .default_height(560)
        .modal(true)
        .build();
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some(placeholder));
    search.set_margin_top(12);
    search.set_margin_bottom(6);
    search.set_margin_start(12);
    search.set_margin_end(12);
    outer.append(&search);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.set_activate_on_single_click(false);
    list.add_css_class("boxed-list");
    list.set_margin_start(12);
    list.set_margin_end(12);
    list.set_margin_bottom(12);

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();
    outer.append(&scroll);
    window.set_child(Some(&outer));
    (window, outer, list, search)
}

/// A row showing a title, a subtitle and an optional badge.
fn row(title: &str, subtitle: &str, badge: Option<&str>, dim: bool) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    let box_ = gtk::Box::new(gtk::Orientation::Vertical, 2);
    box_.set_margin_top(8);
    box_.set_margin_bottom(8);
    box_.set_margin_start(10);
    box_.set_margin_end(10);

    let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let label = gtk::Label::new(Some(title));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    if dim {
        label.add_css_class("dim-label");
    }
    label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    top.append(&label);
    if let Some(badge) = badge {
        let tag = gtk::Label::new(Some(badge));
        tag.add_css_class("dim-label");
        tag.add_css_class("caption");
        top.append(&tag);
    }
    box_.append(&top);

    let sub = gtk::Label::new(Some(subtitle));
    sub.set_xalign(0.0);
    sub.add_css_class("dim-label");
    sub.add_css_class("caption");
    sub.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    box_.append(&sub);

    row.set_child(Some(&box_));
    row
}

/// Add a project: fuzzy find over a root directory, or scan another one.
pub fn add_project<F: Fn(i64) + 'static>(
    parent: &impl IsA<gtk::Window>,
    db: &SharedDb,
    on_added: F,
) {
    let root = db
        .ui_prefs()
        .map(|prefs| prefs.resolved_add_root())
        .unwrap_or_else(|_| crate::config::default_project_root());

    let (window, outer, list, search) = picker_window("Add project", "Filter directories…");
    window.set_transient_for(Some(parent));

    let root_button = gtk::Button::with_label(&format!("scanning {}", crate::db::abbreviate(&root)));
    root_button.add_css_class("flat");
    root_button.add_css_class("caption");
    root_button.set_halign(gtk::Align::Start);
    root_button.set_margin_start(12);
    root_button.set_margin_end(12);
    root_button.set_tooltip_text(Some("Choose another directory to scan"));
    outer.insert_child_after(&root_button, Some(&search));

    let empty = gtk::Label::new(None);
    empty.add_css_class("dim-label");
    empty.set_margin_top(24);
    empty.set_text("No directories match.");
    empty.set_visible(false);
    outer.append(&empty);

    let candidates: Rc<RefCell<Vec<Candidate>>> = Rc::new(RefCell::new(Vec::new()));
    let on_added = Rc::new(on_added);

    // Fill the list for a query, wiring each row straight to "add this".
    let fill: Rc<dyn Fn(&str)> = {
        let list = list.clone();
        let candidates = candidates.clone();
        let db = db.clone();
        let on_added = on_added.clone();
        let window = window.clone();
        let empty = empty.clone();
        let _ = &window;
        Rc::new(move |query: &str| {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            let known: Vec<PathBuf> = db
                .projects()
                .map(|projects| projects.into_iter().map(|p| p.path).collect())
                .unwrap_or_default();
            let mut all = candidates.borrow().clone();
            discover::mark_known(&mut all, &known);
            let matches = discover::filter(&all, query);
            empty.set_visible(matches.is_empty());

            for candidate in matches {
                let badge = if candidate.known {
                    Some("added")
                } else if candidate.is_repo {
                    Some("git")
                } else {
                    None
                };
                let item = row(
                    &candidate.name,
                    &candidate.display_path(),
                    badge,
                    candidate.known,
                );
                let path = candidate.path.clone();
                let db = db.clone();
                let on_added = on_added.clone();
                let window = window.clone();
                item.connect_activate(move |item| {
                    match db.add_project(&path) {
                        Ok(project) => {
                            on_added(project.id);
                            window.close();
                        }
                        Err(error) => {
                            eprintln!("radar: {error}");
                            // Keep the dialog usable and explain on the row.
                            item.set_tooltip_text(Some(&error.to_string()));
                        }
                    }
                });
                list.append(&item);
            }
            if let Some(first) = list.row_at_index(0) {
                list.select_row(Some(&first));
            }
        })
    };

    // Initial scan, on a worker thread so the dialog appears immediately.
    {
        let fill = fill.clone();
        let candidates = candidates.clone();
        let window = window.clone();
        let root = root.clone();
        gtk::glib::spawn_future_local(async move {
            let found = discover::scan(&root, 3, 800);
            *candidates.borrow_mut() = found;
            fill("");
            let _ = window;
        });
    }

    {
        let fill = fill.clone();
        search.connect_search_changed(move |entry| fill(&entry.text()));
    }

    {
        let fill = fill.clone();
        let candidates = candidates.clone();
        let button_for_click = root_button.clone();
        let window = window.clone();
        let db = db.clone();
        let _ = &root_button;
        button_for_click.connect_clicked(move |_| {
            // gtk4-rs 0.8 has no binding for GtkFileDialog yet, so use the
            // (deprecated in GTK) chooser, which still works fine.
            #[allow(deprecated)]
            let dialog = gtk::FileChooserDialog::new(
                Some("Choose a directory to scan"),
                Some(&window),
                gtk::FileChooserAction::SelectFolder,
                &[
                    ("Cancel", gtk::ResponseType::Cancel),
                    ("Scan", gtk::ResponseType::Accept),
                ],
            );
            let fill = fill.clone();
            let candidates = candidates.clone();
            let root_button = root_button.clone();
            let db = db.clone();
            #[allow(deprecated)]
            dialog.connect_response(move |dialog, response| {
                if response == gtk::ResponseType::Accept {
                    if let Some(path) = dialog.file().and_then(|file| file.path()) {
                        root_button.set_label(&format!("scanning {}", crate::db::abbreviate(&path)));
                        // Remember it, so the next add starts here.
                        let mut prefs = db.ui_prefs().unwrap_or_default();
                        prefs.add_root = Some(path.clone());
                        if let Err(error) = db.set_ui_prefs(&prefs) {
                            eprintln!("radar: could not store the scan root: {error}");
                        }
                        *candidates.borrow_mut() = discover::scan(&path, 3, 800);
                        fill("");
                    }
                }
                dialog.close();
            });
            dialog.present();
        });
    }

    search.grab_focus();
    window.present();
}

/// Choose any installed program, grouped by kind.
pub fn choose_program<F: Fn(Program) + 'static>(parent: &impl IsA<gtk::Window>, on_pick: F) {
    let (window, _outer, list, search) = picker_window("Add a program", "Filter programs…");
    window.set_transient_for(Some(parent));
    let window_for_fill = window.clone();

    let all = programs::embeddable();
    let on_pick = Rc::new(on_pick);

    let fill: Rc<dyn Fn(&str)> = {
        let list = list.clone();
        let on_pick = on_pick.clone();
        Rc::new(move |query: &str| {
            let window = window_for_fill.clone();
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            let matcher = fuzzy_matcher::skim::SkimMatcherV2::default().ignore_case();
            use fuzzy_matcher::FuzzyMatcher;
            for kind in programs::Kind::ALL {
                let of_kind: Vec<&Program> = all
                    .iter()
                    .filter(|p| p.kind == kind)
                    .filter(|p| {
                        query.is_empty()
                            || matcher.fuzzy_match(&p.name, query).is_some()
                            || matcher.fuzzy_match(&p.id, query).is_some()
                    })
                    .collect();
                if of_kind.is_empty() {
                    continue;
                }
                let header = gtk::ListBoxRow::new();
                header.set_selectable(false);
                header.set_activatable(false);
                let label = gtk::Label::new(Some(kind.label()));
                label.set_xalign(0.0);
                label.add_css_class("caption-heading");
                label.set_margin_top(12);
                label.set_margin_start(10);
                label.set_margin_bottom(4);
                header.set_child(Some(&label));
                list.append(&header);

                for program in of_kind {
                    let item = row(&program.name, &program.detail(), None, false);
                    let program = program.clone();
                    let on_pick = on_pick.clone();
                    let window = window.clone();
                    item.connect_activate(move |_| {
                        on_pick(program.clone());
                        window.close();
                    });
                    list.append(&item);
                }
            }
            // Select the first real row so Enter works immediately.
            for index in 0..list.observe_children().n_items() as i32 {
                if let Some(child) = list.row_at_index(index) {
                    if child.is_selectable() {
                        list.select_row(Some(&child));
                        break;
                    }
                }
            }
        })
    };
    fill("");

    {
        let fill = fill.clone();
        search.connect_search_changed(move |entry| fill(&entry.text()));
    }

    search.grab_focus();
    window.present();
}

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

    let scroll = gtk::ScrolledWindow::builder().vexpand(true).child(&page).build();
    dialog.set_child(Some(&scroll));
    dialog.present();
}
