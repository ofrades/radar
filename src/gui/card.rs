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

/// Store-backed editor. A failed save leaves the draft intact; optimistic
/// revisions prevent overwriting another editor or agent's changes. Socket I/O
/// runs off the GTK thread so the dialog stays responsive while saving.
pub(super) fn edit_dialog(
    parent: &impl IsA<gtk::Window>,
    home: &Path,
    project_id: i64,
    card: crate::session::board_store::StoredCard,
    on_saved: impl Fn() + 'static,
) -> gtk::Window {
    let window = gtk::Window::builder()
        .title("Edit card")
        .transient_for(parent)
        .modal(true)
        .default_width(460)
        .default_height(340)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(14);
    content.set_margin_bottom(14);
    content.set_margin_start(14);
    content.set_margin_end(14);
    let title = gtk::Entry::new();
    title.set_text(&card.title);
    title.set_placeholder_text(Some("Title"));
    content.append(&title);
    let body = gtk::TextView::new();
    body.buffer().set_text(&card.body);
    body.set_wrap_mode(gtk::WrapMode::WordChar);
    let scroll = gtk::ScrolledWindow::builder()
        .min_content_height(160)
        .vexpand(true)
        .child(&body)
        .build();
    content.append(&scroll);
    let feedback = board::activity_label("", false);
    feedback.add_css_class("error");
    feedback.set_visible(false);
    content.append(&feedback);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let window_for_cancel = window.clone();
    cancel.connect_clicked(move |_| window_for_cancel.close());
    buttons.append(&cancel);
    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    buttons.append(&save);
    content.append(&buttons);
    window.set_child(Some(&content));

    let home = home.to_path_buf();
    let window_for_save = window.clone();
    let on_saved = Rc::new(on_saved);
    save.connect_clicked(move |save| {
        let new_title = title.text().trim().to_string();
        if new_title.is_empty() {
            feedback.set_text("A card needs a title.");
            feedback.set_visible(true);
            title.grab_focus();
            return;
        }
        let buffer = body.buffer();
        let text = buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string();
        if new_title == card.title && text == card.body {
            window_for_save.close();
            return;
        }
        feedback.set_visible(false);
        save.set_sensitive(false);
        cancel.set_sensitive(false);
        title.set_sensitive(false);
        body.set_sensitive(false);
        save.set_label("Saving…");
        let home = home.clone();
        let card_id = card.id.clone();
        let revision = card.revision;
        let command = super::gui_command_id("edit");
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let result = crate::session::daemon::board_card_update(
                &home,
                project_id,
                &card_id,
                Some(&new_title),
                Some(&text),
                Some(revision),
                &command,
            )
            .map(|_| ())
            .map_err(|error| error.to_string());
            let _ = tx.send_blocking(result);
        });
        let (window, save, cancel, title, body, feedback, on_saved) = (
            window_for_save.clone(),
            save.clone(),
            cancel.clone(),
            title.clone(),
            body.clone(),
            feedback.clone(),
            on_saved.clone(),
        );
        gtk::glib::MainContext::default().spawn_local(async move {
            match rx.recv().await {
                Ok(Ok(())) => {
                    window.close();
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
                    body.set_sensitive(true);
                }
            }
        });
    });
    window.present();
    window
}

#[cfg(test)]
mod editor_tests {
    use super::*;

    #[test]
    #[ignore = "requires a private D-Bus session and GTK display"]
    fn editor_validates_titles_and_closes_unchanged_without_writing() {
        gtk::init().unwrap();
        let parent = gtk::Window::new();
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
        let window = edit_dialog(&parent, home.path(), 1, card, || {
            panic!("unchanged card was saved")
        });
        let content = window.child().unwrap();
        let title = content
            .first_child()
            .unwrap()
            .downcast::<gtk::Entry>()
            .unwrap();
        let feedback = title
            .next_sibling()
            .unwrap()
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        let save = content
            .last_child()
            .unwrap()
            .last_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        title.set_text("   ");
        save.emit_clicked();
        assert!(window.is_visible());
        assert!(feedback.is_visible());
        assert_eq!(feedback.text(), "A card needs a title.");
        title.set_text("Original");
        save.emit_clicked();
        assert!(!window.is_visible());
        assert!(home.path().read_dir().unwrap().next().is_none());
        parent.close();
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

    let inner = gtk::Box::new(gtk::Orientation::Vertical, 10);
    inner.set_margin_top(12);
    inner.set_margin_bottom(12);
    inner.set_margin_start(16);
    inner.set_margin_end(16);

    let title = gtk::Label::new(Some(&card.title));
    title.set_xalign(0.0);
    title.set_wrap(true);
    title.add_css_class("card-panel-title");
    inner.append(&title);

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
    inner.append(&meta);

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
    inner.append(&id_row);

    if !card.body.trim().is_empty() {
        let body = markdown::render(&card.body);
        body.add_css_class("card-panel-body");
        inner.append(&body);
    }

    // Controls.
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    controls.set_margin_top(2);
    let edit = gtk::Button::with_label("Edit");
    edit.add_css_class("flat");
    edit.set_action_name(Some("win.card-edit"));
    edit.set_action_target_value(Some(&(project_id, card.id.as_str()).to_variant()));
    controls.append(&edit);
    let finish = gtk::Button::with_label(if card.done { "Reopen" } else { "Close to-do" });
    finish.add_css_class("flat");
    finish.set_action_name(Some("win.card-toggle-done"));
    finish.set_action_target_value(Some(&(project_id, card.id.as_str()).to_variant()));
    controls.append(&finish);

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
    if let Some(index) = columns.iter().position(|name| name == &lane) {
        lanes.set_selected(index as u32);
    }
    {
        let current = lane.clone();
        let card_id = card.id.clone();
        let columns = columns.clone();
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
    controls.append(&lanes);

    let open_board = gtk::Button::with_label("Open project");
    open_board.add_css_class("flat");
    open_board.set_tooltip_text(Some("Open this project in Home"));
    open_board.set_action_name(Some("win.home-project"));
    open_board.set_action_target_value(Some(&project_id.to_variant()));
    controls.append(&open_board);
    inner.append(&controls);

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
