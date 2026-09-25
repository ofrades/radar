//! The board pane: a project's BOARD.md, drawn as a native kanban.
//!
//! This is the one primitive that runs no program. The widget reads the board
//! file, renders a column per heading and a card per task line, and every
//! gesture — dragging a card, adding one, editing one — goes through the same
//! [`board`] operations the CLI uses, so the file stays the single truth and
//! an agent editing it in a neighbouring pane is never overwritten. A file
//! monitor re-reads the file when someone else (an agent, a `git checkout`)
//! changes it, just like the theme monitor re-applies colours.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::gdk;
use gtk::gio;
use gtk::glib;

use crate::board::{self, Board, Card, Column};

/// How long to wait for the file to stop changing before re-reading it. A
/// single write can fire several monitor events; one rebuild is enough.
const RELOAD_DEBOUNCE_MS: u64 = 150;

pub struct BoardPane {
    project: PathBuf,
    widget: gtk::ScrolledWindow,
    columns_box: gtk::Box,
    dialog_parent: gtk::Window,
    /// Guards against scheduling two reloads for one burst of file events.
    pending_reload: Cell<Option<glib::SourceId>>,
}

impl BoardPane {
    pub fn new(project: &Path, parent: &impl IsA<gtk::Window>) -> Rc<BoardPane> {
        // Opening the pane opens the board: if the project has no file yet,
        // the default one is created, the same file an agent will find.
        let _ = board::ensure_file(project);

        let columns_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        columns_box.set_margin_top(8);
        columns_box.set_margin_bottom(8);
        columns_box.set_margin_start(8);
        columns_box.set_margin_end(8);
        columns_box.set_valign(gtk::Align::Fill);

        let widget = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .child(&columns_box)
            .build();
        widget.add_css_class("board-pane");
        widget.set_focusable(true);

        let pane = Rc::new(BoardPane {
            project: project.to_path_buf(),
            widget,
            columns_box,
            dialog_parent: parent.clone().upcast(),
            pending_reload: Cell::new(None),
        });
        pane.reload();
        pane.watch();
        pane
    }

    /// The board pane as a widget, the way primitives are mounted.
    pub fn widget(&self) -> &gtk::Widget {
        self.widget.upcast_ref()
    }

    /// Re-read BOARD.md and rebuild the columns. Cheap: a board is a page of
    /// text, and an agent move is one line.
    fn reload(&self) {
        let Ok(b) = board::load(&self.project) else {
            return;
        };
        while let Some(child) = self.columns_box.first_child() {
            self.columns_box.remove(&child);
        }
        for column in &b.columns {
            self.columns_box.append(&self.build_column(&b, column));
        }
    }

    /// Follow the file: anything that writes BOARD.md — an agent, the CLI,
    /// `git checkout` — ends up on screen here. radar's own writes go through
    /// the same door, so no gesture needs a manual refresh.
    fn watch(self: &Rc<BoardPane>) {
        let file = gio::File::for_path(board::file_path(&self.project));
        let Ok(monitor) = file.monitor_file(gio::FileMonitorFlags::NONE, None::<&gio::Cancellable>)
        else {
            return;
        };
        let pane = Rc::clone(self);
        monitor.connect_changed(move |_, _, _, _| {
            if pane.pending_reload.take().is_some() {
                return;
            }
            let pane_for_timeout = pane.clone();
            let id = glib::timeout_add_local(
                std::time::Duration::from_millis(RELOAD_DEBOUNCE_MS),
                move || {
                    pane_for_timeout.pending_reload.set(None);
                    pane_for_timeout.reload();
                    glib::ControlFlow::Break
                },
            );
            pane.pending_reload.set(Some(id));        });
        // The widget comes and goes with the workspace; the monitor itself is
        // kept alive for the app's lifetime, exactly like the theme monitor.
        std::mem::forget(monitor);
    }

    fn build_column(&self, b: &Board, column: &Column) -> gtk::Widget {
        let column_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        column_box.add_css_class("board-column");
        column_box.set_width_request(230);

        // Header: the column name, how many cards, and a way to add one.
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let name = gtk::Label::new(Some(&column.name));
        name.set_xalign(0.0);
        name.set_hexpand(true);
        name.add_css_class("heading");
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        header.append(&name);
        let count = gtk::Label::new(Some(&column.cards.len().to_string()));
        count.add_css_class("dim-label");
        count.add_css_class("caption");
        header.append(&count);

        let add = gtk::Button::from_icon_name("list-add-symbolic");
        add.add_css_class("flat");
        add.set_tooltip_text(Some(&format!("Add a card to {}", column.name)));
        header.append(&add);
        column_box.append(&header);

        // The cards.
        let cards = gtk::Box::new(gtk::Orientation::Vertical, 6);
        cards.set_vexpand(true);
        for card in &column.cards {
            cards.append(&self.build_card(card));
        }
        if column.cards.is_empty() {
            let empty = gtk::Label::new(Some("no cards"));
            empty.add_css_class("dim-label");
            empty.add_css_class("caption");
            empty.set_xalign(0.0);
            cards.append(&empty);
        }
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&cards)
            .build();
        scroll.set_vexpand(true);
        column_box.append(&scroll);

        // A drop anywhere in the column takes the dragged card. The hint
        // highlights the column widget — the drop target itself is only a
        // controller, it has no pixels to paint.
        let target = gtk::DropTarget::new(glib::types::Type::STRING, gdk::DragAction::MOVE);
        let project = self.project.clone();
        let column_name = column.name.clone();
        let hint_for_drop = column_box.clone();
        let hint_for_enter = column_box.clone();
        let hint_for_leave = column_box.clone();
        target.connect_drop(move |_, value, _, _| {
            let Some(payload) = value.get::<String>().ok() else {
                return false;
            };
            // Not a card (a chip drag from a pane header, say): let it be.
            let Some(title) = payload.strip_prefix("card:") else {
                return false;
            };
            hint_for_drop.remove_css_class("drop-hint");
            match board::move_card(&project, title, &column_name) {
                Ok(_) => true,
                Err(error) => {
                    eprintln!("radar: could not move the card: {error}");
                    false
                }
            }
        });
        target.connect_enter(move |_, _, _| {
            hint_for_enter.add_css_class("drop-hint");
            gdk::DragAction::MOVE
        });
        target.connect_leave(move |_| {
            hint_for_leave.remove_css_class("drop-hint");
        });
        column_box.add_controller(target);

        // The + button opens the card dialog, aimed at this column.
        let project = self.project.clone();
        let parent = self.dialog_parent.clone();
        let names: Vec<String> = b.columns.iter().map(|c| c.name.clone()).collect();
        let default_column = column.name.clone();
        add.connect_clicked(move |_| {
            card_dialog(&parent, &project, None, &names, Some(&default_column));
        });

        column_box.upcast()
    }

    fn build_card(&self, card: &Card) -> gtk::Widget {
        let card_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        card_box.add_css_class("board-card");

        let title = gtk::Label::new(Some(&card.title));
        title.set_xalign(0.0);
        title.set_wrap(true);
        title.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        if card.done {
            title.add_css_class("board-card-done");
        }
        card_box.append(&title);

        if !card.body.is_empty() {
            let notes = gtk::Label::new(Some(&card.body.join("\n")));
            notes.set_xalign(0.0);
            notes.set_wrap(true);
            notes.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            notes.add_css_class("dim-label");
            notes.add_css_class("caption");
            card_box.append(&notes);
        }

        if let Some(who) = &card.claimed_by {
            let claim = gtk::Label::new(Some(&format!("@{who}")));
            claim.set_xalign(0.0);
            claim.add_css_class("board-claim");
            claim.add_css_class("caption");
            card_box.append(&claim);
        }

        // Click to edit; drag to move between columns.
        let click = gtk::GestureClick::new();
        click.set_button(1);
        let project = self.project.clone();
        let parent = self.dialog_parent.clone();
        let title_for_edit = card.title.clone();
        click.connect_released(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let project = project.clone();
            let parent = parent.clone();
            let title = title_for_edit.clone();
            // Resolve the card when the dialog opens, not now: an agent may
            // have changed it since it was drawn.
            glib::idle_add_local(move || {
                edit_dialog(&parent, &project, &title);
                glib::ControlFlow::Break
            });
        });
        card_box.add_controller(click);

        // The drag payload is `card:<title>`: the board operations find a card
        // by title, and the `card:` prefix keeps the pane headers (which drop
        // strings too, for grouping) from ever mistaking a card for a slot.
        let content = gdk::ContentProvider::for_value(
            &format!("card:{}", card.title).to_value(),
        );
        let source = gtk::DragSource::builder()
            .actions(gdk::DragAction::MOVE)
            .content(&content)
            .build();
        card_box.add_controller(source);

        card_box.upcast()
    }
}

/// The add/edit dialog. `existing` is the card's current column and content,
/// when editing; `None` adds a card, placed in `default_column`.
fn card_dialog(
    parent: &gtk::Window,
    project: &Path,
    existing: Option<(&str, &Card)>,
    columns: &[String],
    default_column: Option<&str>,
) {
    if columns.is_empty() {
        return;
    }
    let window = gtk::Window::builder()
        .title(if existing.is_some() {
            "Edit card"
        } else {
            "Add a card"
        })
        .transient_for(parent)
        .modal(true)
        .resizable(false)
        .default_width(420)
        .build();

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 10);
    outer.set_margin_top(14);
    outer.set_margin_bottom(14);
    outer.set_margin_start(14);
    outer.set_margin_end(14);

    let caption = |text: &str| {
        let label = gtk::Label::new(Some(text));
        label.set_xalign(0.0);
        label.add_css_class("caption");
        label.add_css_class("dim-label");
        label
    };

    outer.append(&caption("Title"));
    let title_entry = gtk::Entry::new();
    let title_entry_original = title_entry.clone();
    if let Some((_, card)) = &existing {
        title_entry.set_text(&card.title);
    }
    outer.append(&title_entry);

    outer.append(&caption("Notes"));
    let notes = gtk::TextView::new();
    notes.set_wrap_mode(gtk::WrapMode::WordChar);
    notes.set_height_request(72);
    if let Some((_, card)) = &existing {
        if !card.body.is_empty() {
            notes.buffer().set_text(&card.body.join("\n"));
        }
    }
    let notes_frame = gtk::Frame::new(None);
    notes_frame.set_child(Some(&notes));
    outer.append(&notes_frame);

    outer.append(&caption("Claimed by"));
    let claim_entry = gtk::Entry::new();
    claim_entry.set_placeholder_text(Some("nobody — or a name, like claude"));
    if let Some((_, card)) = &existing {
        if let Some(who) = &card.claimed_by {
            claim_entry.set_text(who);
        }
    }
    outer.append(&claim_entry);

    outer.append(&caption("Column"));
    let strs: Vec<&str> = columns.iter().map(String::as_str).collect();
    let dropdown = gtk::DropDown::from_strings(&strs);
    let start = existing
        .map(|(column, _)| column)
        .or(default_column)
        .and_then(|name| columns.iter().position(|c| c == name))
        .unwrap_or(0) as u32;
    dropdown.set_selected(start);
    outer.append(&dropdown);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);

    if let Some((old_column, _)) = &existing {
        let old_title = existing.map(|(_, card)| card.title.clone()).unwrap_or_default();
        let window_for_delete = window.clone();
        let project = project.to_path_buf();
        let delete = gtk::Button::with_label("Delete");
        delete.add_css_class("destructive-action");
        delete.set_halign(gtk::Align::Start);
        delete.set_hexpand(true);
        delete.connect_clicked(move |_| {
            if let Err(error) = board::remove_card(&project, &old_title) {
                eprintln!("radar: could not remove the card: {error}");
                return;
            }
            window_for_delete.close();
        });
        buttons.append(&delete);
        let _ = old_column;
    }

    let window_for_save = window.clone();
    let title_entry = title_entry.clone();
    let notes = notes.clone();
    let claim_entry = claim_entry.clone();
    let dropdown = dropdown.clone();
    let project = project.to_path_buf();
    let old_title = existing.map(|(_, card)| card.title.clone());
    let columns_for_save = columns.to_vec();
    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    save.connect_clicked(move |_| {
        let title = title_entry.text().trim().to_string();
        if title.is_empty() {
            title_entry.grab_focus();
            return;
        }
        let body: Vec<String> = notes
            .buffer()
            .text(&notes.buffer().start_iter(), &notes.buffer().end_iter(), true)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        let claim = {
            let text = claim_entry.text().trim().trim_start_matches('@').to_string();
            (!text.is_empty()).then_some(text)
        };
        let column = columns_for_save
            .get(dropdown.selected() as usize)
            .cloned()
            .unwrap_or_default();
        let mut card = Card::new(&title);
        card.body = body;
        card.claimed_by = claim;

        let result = match &old_title {
            Some(old) => board::update_card(&project, old, card, Some(&column)),
            None => board::add_card(&project, Some(&column), &title, &card.body.join("\n"), card.claimed_by.as_deref()).map(|_| true),
        };
        match result {
            Ok(true) => window_for_save.close(),
            Ok(false) => eprintln!("radar: the card is gone — it was removed elsewhere"),
            Err(error) => eprintln!("radar: could not save the card: {error}"),
        }
    });
    buttons.append(&save);
    outer.append(&buttons);

    window.set_child(Some(&outer));
    window.present();
    let title_focus = title_entry_original.clone();
    title_focus.grab_focus();
}

/// Clicking a card: load its current state from the file, then edit that.
fn edit_dialog(parent: &gtk::Window, project: &Path, title: &str) {
    let Ok(b) = board::load(project) else {
        return;
    };
    let Some((c, i)) = b.find(title) else {
        eprintln!("radar: the card \"{title}\" is gone from the board");
        return;
    };
    let column = b.columns[c].name.clone();
    let card = b.columns[c].cards[i].clone();
    let columns: Vec<String> = b.columns.iter().map(|col| col.name.clone()).collect();
    card_dialog(parent, project, Some((&column, &card)), &columns, None);
}
