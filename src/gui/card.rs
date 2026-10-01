//! The card detail: a board card opened as a conversation, inside the app.
//!
//! A card is the unit of work and the place the human and the agent talk
//! about it. This view shows the card (its body rendered as Markdown), its
//! thread (comments, board moves, and the agent's questions), and the controls
//! to act on it: reply, edit, move it between lanes, or close it.
//!
//! The thread is the project's durable activity journal filtered to this
//! card's stable id — a human comment is an event with no session, an agent
//! comment carries its session, and board transitions appear as quiet system
//! lines. Nothing here writes a board file; the card lives in radar's store
//! and the conversation in the journal, so both survive agent runs and
//! restarts.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc::SyncSender;

use adw::prelude::*;

use super::{board, markdown, App};
use crate::session::activity::{
    ActivityEvent, ActivityKind, ActivityPayload, Attention, AttentionActionKind, AttentionChange,
    AttentionResponse, PublishActivity,
};
use crate::session::daemon::{Client, Command};

/// The card's inline editor: the title and description each swap between the
/// text they read and a field that edits them, and the controls row swaps to
/// Save/Cancel while either is open. No modal and no separate Edit step.
struct CardEditor {
    title: gtk::Stack,
    body: gtk::Stack,
    controls: gtk::Stack,
}

/// Turn off text selection throughout a read view. A selectable label claims
/// the click for selection, so the description's Markdown would otherwise never
/// reach its open-on-click gesture. The field the click opens still selects.
fn make_unselectable(widget: &gtk::Widget) {
    if let Some(label) = widget.downcast_ref::<gtk::Label>() {
        label.set_selectable(false);
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        make_unselectable(&current);
        child = current.next_sibling();
    }
}

/// Click `target` to open `field_stack`'s edit child, focus `focus`, and swap
/// the controls row to its Save/Cancel child.
fn open_on_click(
    target: &impl IsA<gtk::Widget>,
    field_stack: &gtk::Stack,
    focus: &impl IsA<gtk::Widget>,
    controls_stack: &gtk::Stack,
) {
    target.set_cursor_from_name(Some("pointer"));
    let field_stack = field_stack.clone();
    let focus = focus.clone();
    let controls_stack = controls_stack.clone();
    let gesture = gtk::GestureClick::new();
    gesture.connect_released(move |_, _, _, _| {
        field_stack.set_visible_child_name("edit");
        controls_stack.set_visible_child_name("edit");
        focus.grab_focus();
    });
    target.add_controller(gesture);
}

/// Build the inline editor for a card. Clicking the title or the description
/// swaps that one element for a field in place and turns the controls row into
/// Save/Cancel; Save writes both fields through the board store with the card's
/// revision and Cancel restores both. A failed save leaves the draft intact and
/// shows the error, and `on_saved` runs on success (the caller re-reads the
/// board). Socket I/O runs off the GTK thread.
fn card_editor(
    home: &Path,
    project_id: i64,
    card: &crate::session::board_store::StoredCard,
    columns: &[String],
    lane: &str,
    on_saved: impl Fn() + 'static,
) -> CardEditor {
    let title_view = gtk::Label::new(Some(&card.title));
    title_view.set_xalign(0.0);
    title_view.set_wrap(true);
    title_view.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    title_view.add_css_class("card-panel-title");

    let title = gtk::Entry::new();
    title.set_text(&card.title);
    title.set_hexpand(true);
    title.add_css_class("card-edit-title");

    let title_stack = gtk::Stack::new();
    title_stack.set_transition_type(gtk::StackTransitionType::None);
    title_stack.set_hhomogeneous(false);
    title_stack.set_vhomogeneous(false);
    title_stack.add_named(&title_view, Some("view"));
    title_stack.add_named(&title, Some("edit"));
    title_stack.set_visible_child_name("view");

    // The description reads as Markdown, or as a hint when the card has none.
    let body_view: gtk::Widget = if card.body.trim().is_empty() {
        let hint = board::activity_label("Add a description…", true);
        hint.add_css_class("card-panel-body");
        hint.add_css_class("card-edit-hint");
        hint.upcast()
    } else {
        let body = markdown::render(&card.body);
        body.add_css_class("card-panel-body");
        body
    };

    let text = gtk::TextView::new();
    text.buffer().set_text(&card.body);
    text.set_wrap_mode(gtk::WrapMode::WordChar);
    text.add_css_class("card-edit-body");
    let text_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(120)
        .child(&text)
        .build();

    let body_stack = gtk::Stack::new();
    body_stack.set_transition_type(gtk::StackTransitionType::None);
    body_stack.set_hhomogeneous(false);
    body_stack.set_vhomogeneous(false);
    body_stack.add_named(&body_view, Some("view"));
    body_stack.add_named(&text_scroll, Some("edit"));
    body_stack.set_visible_child_name("view");

    // The reading controls: close/reopen, move lane, open the project.
    let view_controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    view_controls.set_margin_top(2);
    view_controls.set_valign(gtk::Align::Start);
    let finish = gtk::Button::with_label(if card.done { "Reopen" } else { "Close to-do" });
    finish.add_css_class("flat");
    finish.set_action_name(Some("win.card-toggle-done"));
    finish.set_action_target_value(Some(&(project_id, card.id.as_str()).to_variant()));
    view_controls.append(&finish);

    // Display the rename (Backlog -> Todo) but keep the store's real name for
    // the move: the dropdown's integers index into `columns`.
    let labels: Vec<String> = columns
        .iter()
        .map(|name| board::lane_label(name).to_string())
        .collect();
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let lanes = gtk::DropDown::from_strings(&refs);
    lanes.set_valign(gtk::Align::Center);
    lanes.set_tooltip_text(Some("Move to another lane"));
    if let Some(index) = columns.iter().position(|name| name.as_str() == lane) {
        lanes.set_selected(index as u32);
    }
    {
        let current = lane.to_string();
        let card_id = card.id.clone();
        let columns = columns.to_vec();
        lanes.connect_selected_notify(move |dropdown| {
            let Some(column) = columns.get(dropdown.selected() as usize) else {
                return;
            };
            if column == &current {
                return;
            }
            let _ = gtk::prelude::WidgetExt::activate_action(
                dropdown,
                "win.card-move",
                Some(&(project_id, card_id.as_str(), column.as_str()).to_variant()),
            );
        });
    }
    view_controls.append(&lanes);

    let open_board = gtk::Button::with_label("Open project");
    open_board.add_css_class("flat");
    open_board.set_tooltip_text(Some("Open this project in Home"));
    open_board.set_action_name(Some("win.home-project"));
    open_board.set_action_target_value(Some(&project_id.to_variant()));
    view_controls.append(&open_board);

    // The editing controls, in the same place: Save, Cancel, and any error.
    let feedback = board::activity_label("", false);
    feedback.add_css_class("error");
    feedback.set_visible(false);
    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    let cancel = gtk::Button::with_label("Cancel");
    let edit_controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    edit_controls.set_margin_top(2);
    edit_controls.set_valign(gtk::Align::Start);
    edit_controls.append(&save);
    edit_controls.append(&cancel);
    edit_controls.append(&feedback);

    let controls_stack = gtk::Stack::new();
    controls_stack.set_transition_type(gtk::StackTransitionType::None);
    controls_stack.set_hhomogeneous(false);
    controls_stack.set_vhomogeneous(false);
    controls_stack.add_named(&view_controls, Some("view"));
    controls_stack.add_named(&edit_controls, Some("edit"));
    controls_stack.set_visible_child_name("view");

    make_unselectable(&body_view);
    open_on_click(&title_view, &title_stack, &title, &controls_stack);
    open_on_click(&body_view, &body_stack, &text, &controls_stack);

    let card_title = card.title.clone();
    let card_body = card.body.clone();
    let card_id = card.id.clone();
    let revision = card.revision;
    let home = home.to_path_buf();
    let on_saved = Rc::new(on_saved);

    {
        let title = title.clone();
        let text = text.clone();
        let card_title = card_title.clone();
        let card_body = card_body.clone();
        let title_stack = title_stack.clone();
        let body_stack = body_stack.clone();
        let controls_stack = controls_stack.clone();
        cancel.connect_clicked(move |_| {
            title.set_text(&card_title);
            text.buffer().set_text(&card_body);
            title_stack.set_visible_child_name("view");
            body_stack.set_visible_child_name("view");
            controls_stack.set_visible_child_name("view");
        });
    }

    let title_out = title_stack.clone();
    let body_out = body_stack.clone();
    let controls_out = controls_stack.clone();
    let save_for_activate = save.clone();
    title.connect_activate(move |_| save_for_activate.emit_clicked());
    save.connect_clicked(move |save| {
        let new_title = title.text().trim().to_string();
        if new_title.is_empty() {
            feedback.set_text("A card needs a title.");
            feedback.set_visible(true);
            title.grab_focus();
            return;
        }
        let buffer = text.buffer();
        let new_body = buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string();
        if new_title == card_title && new_body == card_body {
            title_stack.set_visible_child_name("view");
            body_stack.set_visible_child_name("view");
            controls_stack.set_visible_child_name("view");
            return;
        }
        feedback.set_visible(false);
        save.set_sensitive(false);
        cancel.set_sensitive(false);
        title.set_sensitive(false);
        text.set_sensitive(false);
        save.set_label("Saving…");
        let home = home.clone();
        let card_id = card_id.clone();
        let command = super::gui_command_id("edit");
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let result = crate::session::daemon::board_card_update(
                &home,
                project_id,
                &card_id,
                Some(&new_title),
                Some(&new_body),
                Some(revision),
                &command,
            )
            .map(|_| ())
            .map_err(|error| error.to_string());
            let _ = tx.send_blocking(result);
        });
        let (save, cancel, title, text, feedback, on_saved) = (
            save.clone(),
            cancel.clone(),
            title.clone(),
            text.clone(),
            feedback.clone(),
            on_saved.clone(),
        );
        let (title_stack, body_stack, controls_stack) = (
            title_stack.clone(),
            body_stack.clone(),
            controls_stack.clone(),
        );
        gtk::glib::MainContext::default().spawn_local(async move {
            match rx.recv().await {
                Ok(Ok(())) => {
                    title_stack.set_visible_child_name("view");
                    body_stack.set_visible_child_name("view");
                    controls_stack.set_visible_child_name("view");
                    on_saved();
                }
                result => {
                    let error = match result {
                        Ok(Err(error)) => error,
                        Err(error) => error.to_string(),
                        Ok(Ok(())) => unreachable!(),
                    };
                    feedback.set_text(&format!("Could not save: {error}"));
                    feedback.set_visible(true);
                    save.set_label("Save");
                    save.set_sensitive(true);
                    cancel.set_sensitive(true);
                    title.set_sensitive(true);
                    text.set_sensitive(true);
                }
            }
        });
    });

    CardEditor {
        title: title_out,
        body: body_out,
        controls: controls_out,
    }
}

#[cfg(test)]
mod editor_tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    #[ignore = "requires a private D-Bus session and GTK display"]
    fn inline_editor_validates_titles_and_skips_unchanged_saves() {
        gtk::init().unwrap();
        let home = tempfile::tempdir().unwrap();
        let card = crate::session::board_store::StoredCard {
            id: "card".into(),
            project_id: 1,
            lane_id: 1,
            lane: "Todo".into(),
            done: false,
            position: 0,
            title: "Original".into(),
            body: "Draft".into(),
            claim: None,
            revision: 1,
            created_at_millis: 0,
            updated_at_millis: 0,
        };
        let saved = Rc::new(Cell::new(false));
        let saved_flag = saved.clone();
        let editor = card_editor(
            home.path(),
            1,
            &card,
            &["Todo".to_string()],
            "Todo",
            move || saved_flag.set(true),
        );
        let title = editor
            .title
            .child_by_name("edit")
            .unwrap()
            .downcast::<gtk::Entry>()
            .unwrap();
        let edit_controls = editor.controls.child_by_name("edit").unwrap();
        let save = edit_controls
            .first_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        let feedback = edit_controls
            .last_child()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        // A blank title is rejected.
        title.set_text("   ");
        save.emit_clicked();
        assert_eq!(feedback.text(), "A card needs a title.");
        assert!(!saved.get());
        // Saving the unchanged text writes nothing.
        title.set_text("Original");
        save.emit_clicked();
        assert!(!saved.get());
        assert!(home.path().read_dir().unwrap().next().is_none());
    }

    #[test]
    #[ignore = "requires a private D-Bus session and GTK display"]
    fn title_and_description_are_visible_by_default() {
        gtk::init().unwrap();
        let home = tempfile::tempdir().unwrap();
        let card = crate::session::board_store::StoredCard {
            id: "card".into(),
            project_id: 1,
            lane_id: 1,
            lane: "Todo".into(),
            done: false,
            position: 0,
            title: "A title".into(),
            body: "A description that is long enough to wrap across the width of the card panel and take up a couple of lines.".into(),
            claim: None,
            revision: 1,
            created_at_millis: 0,
            updated_at_millis: 0,
        };
        let editor = card_editor(home.path(), 1, &card, &["Todo".to_string()], "Todo", || {});
        let root = gtk::Box::new(gtk::Orientation::Vertical, 16);
        root.set_margin_top(20);
        root.set_margin_start(20);
        root.set_margin_end(20);
        root.append(&editor.title);
        root.append(&editor.body);
        root.append(&editor.controls);
        let window = gtk::Window::new();
        window.set_child(Some(&root));
        window.set_default_size(500, 400);
        window.present();
        let ctx = gtk::glib::MainContext::default();
        for _ in 0..10 {
            while ctx.pending() {
                ctx.iteration(false);
            }
        }
        let (title_height, body_height) = (editor.title.height(), editor.body.height());
        window.close();
        assert!(title_height > 0, "title height was {title_height}");
        assert!(body_height > 0, "description height was {body_height}");
    }

    #[test]
    #[ignore = "requires a private D-Bus session and GTK display"]
    fn read_description_labels_do_not_steal_the_click() {
        gtk::init().unwrap();
        let view = markdown::render("Some **bold** text and a [link](https://example.com).");
        make_unselectable(&view);
        fn assert_unselectable(widget: &gtk::Widget) {
            if let Some(label) = widget.downcast_ref::<gtk::Label>() {
                assert!(!label.is_selectable(), "a read label is still selectable");
            }
            let mut child = widget.first_child();
            while let Some(current) = child {
                assert_unselectable(&current);
                child = current.next_sibling();
            }
        }
        assert_unselectable(&view);
    }
}

/// Build the in-app card detail. Rebuilt by `App::refresh_home` whenever the
/// journal or board changes, so it always shows the current conversation.
pub(super) fn detail(app: &App, project_id: i64, card_id: &str) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("card-panel");
    root.set_vexpand(true);
    root.set_hexpand(true);

    let project = app
        .projects
        .borrow()
        .iter()
        .find(|project| project.id == project_id)
        .cloned();
    let Some(_project) = project else {
        root.append(&message("That project is no longer available"));
        return root.upcast();
    };
    let Some(state) = app.board_states.borrow().get(&project_id).cloned() else {
        root.append(&message("Loading the card…"));
        return root.upcast();
    };
    let columns: Vec<String> = state.lanes.iter().map(|lane| lane.name.clone()).collect();
    let Some(card) = state.cards.iter().find(|card| card.id == card_id).cloned() else {
        root.append(&message("This card is no longer on the board"));
        return root.upcast();
    };
    let lane = state
        .lanes
        .iter()
        .find(|lane| lane.id == card.lane_id)
        .map(|lane| lane.name.clone())
        .unwrap_or_default();

    let inner = gtk::Box::new(gtk::Orientation::Vertical, 16);
    inner.set_margin_top(24);
    inner.set_margin_bottom(28);
    inner.set_margin_start(28);
    inner.set_margin_end(28);

    // The card is edited where it is read: clicking the title or the
    // description swaps that element for a field in place and turns the
    // controls row into Save/Cancel. No modal and no Edit button.
    let editor = card_editor(&app.session_home, project_id, &card, &columns, &lane, {
        let window = app.window.clone();
        move || {
            // Defer the re-read one turn: saving with Enter leaves focus in a
            // field, and the refresh guard skips a rebuild while an editable
            // still holds focus.
            let window = window.clone();
            gtk::glib::idle_add_local_once(move || {
                let _ = gtk::prelude::WidgetExt::activate_action(
                    &window,
                    "win.card-saved",
                    Some(&project_id.to_variant()),
                );
            });
        }
    });

    let reading = gtk::Box::new(gtk::Orientation::Vertical, 16);
    reading.append(&editor.title);

    let claim = card
        .claim
        .as_deref()
        .map(|who| format!(" · @{who}"))
        .unwrap_or_default();
    let done = if card.done { " · done" } else { "" };
    let lane_shown = board::lane_label(&lane);
    let meta = gtk::Label::new(Some(&format!("{lane_shown}{claim}{done}")));
    meta.set_xalign(0.0);
    meta.add_css_class("caption");
    meta.add_css_class("dim-label");
    reading.append(&meta);

    // The card's stable id, visible and copyable: how a human hands this
    // exact card to another agent (`radar card show "<id>"`).
    let id_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let id_label = gtk::Label::new(Some(&card.id));
    id_label.set_xalign(0.0);
    id_label.set_selectable(true);
    id_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    id_label.add_css_class("card-panel-id");
    id_row.append(&id_label);
    let copy = gtk::Button::with_label("Copy id");
    copy.add_css_class("flat");
    copy.add_css_class("card-panel-copy");
    copy.set_tooltip_text(Some("Copy this card's id, to reference it to an agent"));
    {
        let id = card.id.clone();
        let toasts = app.toasts.clone();
        let window = app.window.clone();
        copy.connect_clicked(move |_| {
            window.clipboard().set_text(&id);
            toasts.add_toast(adw::Toast::new("Card id copied"));
        });
    }
    id_row.append(&copy);
    reading.append(&id_row);

    reading.append(&editor.body);
    reading.append(&editor.controls);

    inner.append(&reading);

    let session_heading = board::activity_label("Linked sessions", false);
    session_heading.add_css_class("lane-section");
    inner.append(&session_heading);
    if app.todo_origin.get().is_some() && app.home_nav.borrow().len() == 1 {
        let back = gtk::Button::with_label("Back to session");
        back.set_halign(gtk::Align::Start);
        back.set_action_name(Some("win.home-back"));
        inner.append(&back);
    }
    let sessions = app
        .agent_sessions
        .borrow()
        .by_project
        .get(&project_id)
        .cloned()
        .unwrap_or_default();
    let linked: Vec<_> = sessions
        .iter()
        .filter(|session| {
            app.session_card_id(project_id, session).as_deref() == Some(card.id.as_str())
        })
        .collect();
    if linked.is_empty() {
        inner.append(&board::activity_label("No linked session yet.", true));
        let open = gtk::Button::with_label(if card.claim.is_some() {
            "Open claimed session"
        } else {
            "Start session"
        });
        open.set_halign(gtk::Align::Start);
        if let Some(claim) = &card.claim {
            open.set_action_name(Some("win.open-claim"));
            open.set_action_target_value(Some(&(project_id, claim.as_str()).to_variant()));
        } else {
            open.set_action_name(Some("win.card-session-create"));
            open.set_action_target_value(Some(&(project_id, card.id.as_str()).to_variant()));
            open.set_sensitive(!card.done);
        }
        inner.append(&open);
    }
    for session in linked {
        let running = super::live_agents::sidebar_session_is_live(session);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let label = board::activity_label(&session.title, false);
        label.set_hexpand(true);
        row.append(&label);
        row.append(&board::activity_label(
            if running { "Running" } else { "Stopped" },
            true,
        ));
        let open = gtk::Button::with_label(if running {
            "Open session"
        } else {
            "Resume session"
        });
        open.set_action_name(Some("win.todo-session"));
        open.set_action_target_value(Some(&(project_id, session.id.as_str()).to_variant()));
        let can_open = running || super::live_agents::exact_provider_session_id(session).is_some();
        open.set_sensitive(can_open);
        if !can_open {
            open.set_tooltip_text(Some("The agent did not report an exact conversation id; a different session will not be opened instead."));
        }
        row.append(&open);
        inner.append(&row);
    }

    inner.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    let thread_heading = gtk::Label::new(Some("Conversation"));
    thread_heading.set_xalign(0.0);
    thread_heading.add_css_class("lane-section");
    inner.append(&thread_heading);

    let pending = Rc::new(RefCell::new(HashSet::new()));
    let feedback = gtk::Label::new(None);
    feedback.add_css_class("caption");
    feedback.add_css_class("dim-label");
    feedback.set_xalign(0.0);
    feedback.set_visible(false);
    append_thread(app, &inner, project_id, &card.id, &pending, &feedback);
    inner.append(&feedback);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&inner)
        .build();
    scroller.add_css_class("card-thread");
    root.append(&scroller);

    // Reply.
    let reply_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    reply_row.set_margin_start(16);
    reply_row.set_margin_end(16);
    reply_row.set_margin_bottom(12);
    let reply = gtk::Entry::new();
    reply.set_placeholder_text(Some("Reply, or leave the agent a note…"));
    reply.set_hexpand(true);
    let send = gtk::Button::with_label("Send");
    send.add_css_class("suggested-action");
    {
        let card_id = card.id.clone();
        let activate: Rc<dyn Fn(&gtk::Entry)> = Rc::new(move |entry: &gtk::Entry| {
            let text = entry.text().trim().to_string();
            if text.is_empty() {
                return;
            }
            entry.set_text("");
            let _ = gtk::prelude::WidgetExt::activate_action(
                entry,
                "win.card-reply",
                Some(&(project_id, card_id.as_str(), text.as_str()).to_variant()),
            );
        });
        let reply_button = reply.clone();
        {
            let activate = activate.clone();
            send.connect_clicked(move |_| activate(&reply_button));
        }
        {
            let activate = activate.clone();
            reply.connect_activate(move |entry| activate(entry));
        }
    }
    reply_row.append(&reply);
    reply_row.append(&send);
    root.append(&reply_row);

    root.upcast()
}

fn message(text: &str) -> gtk::Widget {
    let label = gtk::Label::new(Some(text));
    label.set_wrap(true);
    label.add_css_class("card-panel-body");
    label.set_margin_top(16);
    label.set_margin_start(16);
    label.set_margin_end(16);
    label.upcast()
}

fn append_thread(
    app: &App,
    parent: &gtk::Box,
    project_id: i64,
    card_id: &str,
    pending: &Rc<RefCell<HashSet<String>>>,
    feedback: &gtk::Label,
) {
    let activity = app.activity.borrow();
    let Some(snapshot) = activity.get(&project_id).map(|a| &a.snapshot) else {
        return;
    };
    let mut events: Vec<&ActivityEvent> = snapshot
        .events
        .iter()
        .filter(|event| event.card_id.as_deref() == Some(card_id))
        .collect();
    events.sort_by_key(|event| event.sequence);
    let attention: Vec<&Attention> = snapshot
        .attention
        .iter()
        .filter(|attention| {
            attention.card_id.as_deref() == Some(card_id) && attention.is_unresolved()
        })
        .collect();

    if events.is_empty() && attention.is_empty() {
        let empty = gtk::Label::new(Some("No activity on this card yet"));
        empty.set_xalign(0.0);
        empty.add_css_class("caption");
        empty.add_css_class("dim-label");
        parent.append(&empty);
        return;
    }

    for event in events {
        match &event.payload {
            ActivityPayload::Message { text } => {
                let author = if event.session_id.is_some() {
                    "Agent"
                } else {
                    "You"
                };
                parent.append(&thread_message(author, text, event.at_millis));
            }
            ActivityPayload::BoardChanged { action, column, .. } => {
                parent.append(&thread_system(&board_change_text(
                    action,
                    column.as_deref(),
                )));
            }
            ActivityPayload::AttentionResolved { response, .. } => {
                parent.append(&thread_system(&format!(
                    "Answered: {}",
                    response_text(response)
                )));
            }
            _ => {}
        }
    }

    for request in attention {
        parent.append(&attention_card(app, project_id, request, pending, feedback));
    }
}

fn attention_card(
    app: &App,
    project_id: i64,
    attention: &Attention,
    pending: &Rc<RefCell<HashSet<String>>>,
    feedback: &gtk::Label,
) -> gtk::Widget {
    let frame = gtk::Frame::new(None);
    frame.add_css_class("attention-card");
    let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    body.set_margin_top(8);
    body.set_margin_bottom(8);
    body.set_margin_start(8);
    body.set_margin_end(8);
    let heading = gtk::Label::new(Some(&format!(
        "{} · {}",
        board::attention_kind_label(attention.kind),
        board::relative_age(attention.created_at_millis)
    )));
    heading.set_xalign(0.0);
    heading.add_css_class("caption-heading");
    body.append(&heading);
    body.append(&markdown::render(&attention.reason));

    let buttons = gtk::FlowBox::new();
    buttons.set_selection_mode(gtk::SelectionMode::None);
    buttons.set_min_children_per_line(1);
    buttons.set_max_children_per_line(3);
    buttons.set_row_spacing(4);
    buttons.set_column_spacing(4);
    for action in &attention.allowed_actions {
        match action {
            AttentionActionKind::Answer => {
                let button = gtk::Button::with_label("Answer");
                let parent: gtk::Window = app.window.clone().upcast();
                let home = app.session_home.clone();
                let tx = app.activity_tx.clone();
                let pending = pending.clone();
                let feedback = feedback.clone();
                let attention = attention.clone();
                button.connect_clicked(move |_| {
                    board::answer_dialog(
                        &parent, project_id, &home, &tx, &pending, &feedback, &attention,
                    );
                });
                buttons.append(&button);
            }
            AttentionActionKind::Approve => buttons.append(&change_button(
                app,
                project_id,
                attention,
                "Approve",
                AttentionChange::Respond(AttentionResponse::Approve),
                pending,
                feedback,
            )),
            AttentionActionKind::Deny => buttons.append(&change_button(
                app,
                project_id,
                attention,
                "Deny",
                AttentionChange::Respond(AttentionResponse::Deny),
                pending,
                feedback,
            )),
            AttentionActionKind::Dismiss => buttons.append(&change_button(
                app,
                project_id,
                attention,
                "Dismiss",
                AttentionChange::Respond(AttentionResponse::Dismiss),
                pending,
                feedback,
            )),
        }
    }
    body.append(&buttons);
    frame.set_child(Some(&body));
    frame.upcast()
}

#[allow(clippy::too_many_arguments)]
fn change_button(
    app: &App,
    project_id: i64,
    attention: &Attention,
    label: &str,
    change: AttentionChange,
    pending: &Rc<RefCell<HashSet<String>>>,
    feedback: &gtk::Label,
) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    let home = app.session_home.clone();
    let tx = app.activity_tx.clone();
    let pending = pending.clone();
    let feedback = feedback.clone();
    let attention = attention.clone();
    button.connect_clicked(move |button| {
        button.set_sensitive(false);
        board::submit_attention_change(
            project_id,
            &home,
            &tx,
            &pending,
            &feedback,
            &attention,
            change.clone(),
        );
    });
    button
}

fn board_change_text(action: &str, column: Option<&str>) -> String {
    let label = match action {
        "added" | "board_card_added" => "Added to the board",
        "moved" | "board_card_moved" => "Moved",
        "claimed" | "board_card_claimed" => "Claimed",
        "released" | "board_card_released" => "Released",
        "done" | "board_card_done" => "Closed",
        "edited" | "board_card_edited" => "Edited",
        other => {
            let text = other.replace('_', " ");
            return match column {
                Some(column) => format!("{text} · {column}"),
                None => text,
            };
        }
    };
    match column {
        Some(column) => format!("{label} · {column}"),
        None => label.to_string(),
    }
}

fn response_text(response: &AttentionResponse) -> String {
    match response {
        AttentionResponse::Answer(text) => text.clone(),
        AttentionResponse::Approve => "approved".to_string(),
        AttentionResponse::Deny => "denied".to_string(),
        AttentionResponse::Dismiss => "dismissed".to_string(),
    }
}

fn thread_message(author: &str, text: &str, at_millis: i64) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
    row.add_css_class("thread-row");
    row.add_css_class(if author == "You" {
        "thread-you"
    } else {
        "thread-agent"
    });
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let who = gtk::Label::new(Some(author));
    who.set_xalign(0.0);
    who.add_css_class("thread-author");
    who.set_hexpand(true);
    head.append(&who);
    let age = gtk::Label::new(Some(&board::relative_age(at_millis)));
    age.add_css_class("caption");
    age.add_css_class("dim-label");
    head.append(&age);
    row.append(&head);
    let body = markdown::render(text);
    row.append(&body);
    row.upcast()
}

fn thread_system(text: &str) -> gtk::Widget {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("caption");
    label.add_css_class("dim-label");
    label.add_css_class("thread-system");
    label.upcast()
}

/// Publish a human comment on a card. Fire-and-forget on a worker thread, the
/// same shape as the board's attention responses; the journal echoes the event
/// back through the project watcher.
pub(super) fn publish_comment(
    home: &Path,
    tx: &SyncSender<super::ActivityNotice>,
    project_id: i64,
    card_id: &str,
    text: String,
) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let command = Command::PublishActivity(PublishActivity {
        project_id,
        command_id: format!("gui-comment-{}-{now:x}", std::process::id()),
        session_id: None,
        card_id: Some(card_id.to_string()),
        kind: ActivityKind::Reported,
        payload: ActivityPayload::Message { text },
    });
    let home = home.to_path_buf();
    let tx = tx.clone();
    std::thread::spawn(move || {
        let error = match Client::request(&home, command) {
            Ok(_) => None,
            Err(error) => Some(error.to_string()),
        };
        let _ = tx.send(super::ActivityNotice::CardComment { project_id, error });
    });
}
