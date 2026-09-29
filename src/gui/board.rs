//! The board pane: a project's BOARD.md, drawn as a native kanban.
//!
//! This is the one primitive that runs no program. The widget reads the board
//! file, renders a column per heading and a card per task line, and every
//! gesture — dragging a card, adding one, editing one — goes through the same
//! [`board`] operations the CLI uses, so the file stays the single truth and
//! an agent editing it in a neighbouring pane is never overwritten. A file
//! monitor re-reads the file when someone else (an agent, a `git checkout`)
//! changes it, just like the theme monitor re-applies colours.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::SyncSender;

use adw::prelude::*;
use gtk::gdk;
use gtk::glib;

use crate::board::{Board, Card, Column};
use crate::session::activity::{
    ActivityEvent, ActivityPayload, ActivitySnapshot, Attention, AttentionActionKind,
    AttentionChange, AttentionResponse, ChangeAttention,
};
use crate::session::board_store::StoredCard;
use crate::session::daemon::{Client, Command, Response};

type BoardObserver = Box<dyn Fn(Option<String>)>;

#[derive(Clone, Debug, Default)]
pub(super) struct BoardSummary {
    pub done: usize,
    pub total: usize,
    pub claimed: usize,
    pub in_progress: usize,
    pub review: usize,
    /// The board's columns as lanes, in file order, each with its cards — how
    /// Home draws a project's to-dos.
    pub lanes: Vec<LaneSummary>,
}

/// One board column, kept whole for the cockpit: its name, whether it is the
/// Done lane, and its cards.
#[derive(Clone, Debug)]
pub(super) struct LaneSummary {
    pub name: String,
    pub done: bool,
    pub cards: Vec<WorkCard>,
}

/// A card surfaced on the cockpit: its stable id and title, and the claim it
/// wears when one is present.
#[derive(Clone, Debug)]
pub(super) struct WorkCard {
    pub id: String,
    pub title: String,
    pub claim: Option<String>,
}

fn work_card(card: &Card) -> WorkCard {
    WorkCard {
        id: card.id.clone(),
        title: card.title.clone(),
        claim: card
            .claimed_by
            .as_deref()
            .filter(|claim| !claim.is_empty())
            .map(str::to_string),
    }
}

pub(super) fn summarize(board: &Board) -> BoardSummary {
    let mut summary = BoardSummary::default();
    for column in &board.columns {
        let name = column.name.trim();
        let done = name.eq_ignore_ascii_case("done");
        let in_progress = name.eq_ignore_ascii_case("in progress");
        let review = name.eq_ignore_ascii_case("review");
        if done {
            summary.done += column.cards.len();
        }
        if in_progress {
            summary.in_progress += column.cards.len();
        }
        if review {
            summary.review += column.cards.len();
        }
        let mut cards = Vec::with_capacity(column.cards.len());
        for card in &column.cards {
            summary.total += 1;
            if card
                .claimed_by
                .as_deref()
                .is_some_and(|claim| !claim.is_empty())
            {
                summary.claimed += 1;
            }
            cards.push(work_card(card));
        }
        summary.lanes.push(LaneSummary {
            name: name.to_string(),
            done,
            cards,
        });
    }
    summary
}

pub struct BoardPane {
    project: PathBuf,
    project_id: i64,
    session_home: PathBuf,
    activity_tx: SyncSender<super::ActivityNotice>,
    widget: gtk::Box,
    columns_box: gtk::Box,
    activity_items: gtk::Box,
    activity_status: gtk::Label,
    activity_feedback: gtk::Label,
    activity_snapshot: RefCell<ActivitySnapshot>,
    activity_online: Cell<bool>,
    pending_attention: Rc<RefCell<HashSet<String>>>,
    card_widgets: RefCell<HashMap<String, gtk::Widget>>,
    highlighted_card: RefCell<Option<String>>,
    dialog_parent: gtk::Window,
    /// Who to tell when the board's shape changes, and the last shape told
    /// (so an observer wired after the pane opened still gets it).
    observer: RefCell<Option<BoardObserver>>,
    last_stats: RefCell<Option<String>>,
}

impl BoardPane {
    pub fn new(
        project: &Path,
        project_id: i64,
        session_home: &Path,
        activity_tx: SyncSender<super::ActivityNotice>,
        parent: &impl IsA<gtk::Window>,
    ) -> Rc<BoardPane> {
        let columns_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        columns_box.set_margin_top(8);
        columns_box.set_margin_bottom(8);
        columns_box.set_margin_start(8);
        columns_box.set_margin_end(8);
        columns_box.set_valign(gtk::Align::Fill);
        columns_box.set_hexpand(true);
        columns_box.set_vexpand(true);

        let work_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .child(&columns_box)
            .build();
        work_scroll.add_css_class("board-work-scroll");
        work_scroll.set_hexpand(true);
        work_scroll.set_vexpand(true);

        let activity_panel = gtk::Box::new(gtk::Orientation::Vertical, 8);
        activity_panel.add_css_class("board-activity-panel");
        activity_panel.set_size_request(310, -1);
        activity_panel.set_vexpand(true);
        let heading = gtk::Label::new(Some("Project activity"));
        heading.set_xalign(0.0);
        heading.add_css_class("caption-heading");
        activity_panel.append(&heading);
        let activity_status = gtk::Label::new(Some("Connecting…"));
        activity_status.set_xalign(0.0);
        activity_status.add_css_class("caption");
        activity_status.add_css_class("dim-label");
        activity_panel.append(&activity_status);
        let activity_feedback = gtk::Label::new(None);
        activity_feedback.set_xalign(0.0);
        activity_feedback.set_wrap(true);
        activity_feedback.add_css_class("caption");
        activity_feedback.set_visible(false);
        activity_panel.append(&activity_feedback);
        let activity_items = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let activity_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(&activity_items)
            .build();
        activity_panel.append(&activity_scroll);

        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        widget.add_css_class("board-pane");
        widget.set_hexpand(true);
        widget.set_vexpand(true);
        widget.set_focusable(true);
        widget.append(&work_scroll);
        widget.append(&activity_panel);

        let pane = Rc::new(BoardPane {
            project: project.to_path_buf(),
            project_id,
            session_home: session_home.to_path_buf(),
            activity_tx,
            widget,
            columns_box,
            activity_items,
            activity_status,
            activity_feedback,
            activity_snapshot: RefCell::new(ActivitySnapshot {
                project_id,
                watermark: 0,
                events: Vec::new(),
                attention: Vec::new(),
                has_more: false,
            }),
            activity_online: Cell::new(false),
            pending_attention: Rc::new(RefCell::new(HashSet::new())),
            card_widgets: RefCell::new(HashMap::new()),
            highlighted_card: RefCell::new(None),
            dialog_parent: parent.clone().upcast(),
            observer: RefCell::new(None),
            last_stats: RefCell::new(None),
        });
        pane.reload();
        pane
    }

    /// The board pane as a widget, the way primitives are mounted.
    pub fn widget(&self) -> &gtk::Widget {
        self.widget.upcast_ref()
    }

    pub fn set_activity_state(&self, snapshot: ActivitySnapshot, online: bool) {
        *self.activity_snapshot.borrow_mut() = snapshot;
        self.activity_online.set(online);
        self.activity_status.set_text(if online {
            "Live · sequenced project feed"
        } else {
            "Offline · open requests stay available; reconnect to respond"
        });
        self.render_activity();
    }

    pub fn finish_attention_change(
        &self,
        request_id: &str,
        result: std::result::Result<Attention, String>,
    ) {
        self.pending_attention.borrow_mut().remove(request_id);
        match result {
            Ok(_) => {
                self.activity_feedback.set_text("Response saved.");
                self.activity_feedback.remove_css_class("error");
                self.activity_feedback.set_visible(true);
            }
            Err(error) => {
                self.activity_feedback
                    .set_text(&format!("Response not applied: {error}"));
                self.activity_feedback.add_css_class("error");
                self.activity_feedback.set_visible(true);
            }
        }
        self.render_activity();
    }

    pub fn focus_card(&self, card_id: &str) -> bool {
        let Some(card) = self.card_widgets.borrow().get(card_id).cloned() else {
            return false;
        };
        if let Some(previous) = self.highlighted_card.borrow().as_ref() {
            if let Some(widget) = self.card_widgets.borrow().get(previous) {
                widget.remove_css_class("board-card-target");
            }
        }
        card.add_css_class("board-card-target");
        card.grab_focus();
        *self.highlighted_card.borrow_mut() = Some(card_id.to_string());
        true
    }

    fn render_activity(&self) {
        while let Some(child) = self.activity_items.first_child() {
            self.activity_items.remove(&child);
        }
        let snapshot = self.activity_snapshot.borrow().clone();
        let board = self.board();

        append_activity_heading(&self.activity_items, "Needs attention");
        let mut attention = snapshot.attention.clone();
        attention.sort_by_key(|item| std::cmp::Reverse(item.created_at_millis));
        if attention.is_empty() {
            let empty = activity_label("No unresolved requests", true);
            self.activity_items.append(&empty);
        } else {
            for request in attention {
                self.render_attention(&request, board.as_ref());
            }
        }

        append_activity_heading(&self.activity_items, "Agent states");
        let mut states: Vec<(String, String, i64)> = Vec::new();
        for event in &snapshot.events {
            let (Some(session_id), ActivityPayload::AgentState { state, message }) =
                (&event.session_id, &event.payload)
            else {
                continue;
            };
            states.retain(|(session, _, _)| session != session_id);
            let state_name = agent_state_label(*state);
            let text = message
                .as_ref()
                .map(|message| format!("{state_name} · {message}"))
                .unwrap_or(state_name.to_string());
            states.push((session_id.clone(), text, event.at_millis));
        }
        states.sort_by_key(|(_, _, at)| std::cmp::Reverse(*at));
        if states.is_empty() {
            self.activity_items
                .append(&activity_label("No explicit agent state yet", true));
        } else {
            for (session_id, state, at) in states.into_iter().take(6) {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                row.add_css_class("activity-row");
                let label =
                    activity_label(&format!("{state}\n{}", short_session(&session_id)), false);
                label.set_hexpand(true);
                row.append(&label);
                let open = self.session_button(&session_id, "Open");
                row.append(&open);
                let age = activity_label(&relative_age(at), true);
                row.append(&age);
                self.activity_items.append(&row);
            }
        }

        append_activity_heading(&self.activity_items, "Recent activity");
        if snapshot.has_more {
            self.activity_items
                .append(&activity_label("Showing the latest 200 events", true));
        }
        if snapshot.events.is_empty() {
            self.activity_items
                .append(&activity_label("No project activity yet", true));
        } else {
            let mut latest_states: Vec<(String, crate::session::activity::AgentState)> = Vec::new();
            let mut rendered = 0;
            for event in snapshot.events.iter().rev() {
                if let (Some(session_id), ActivityPayload::AgentState { state, .. }) =
                    (&event.session_id, &event.payload)
                {
                    if let Some((_, previous)) = latest_states
                        .iter_mut()
                        .find(|(session, _)| session == session_id)
                    {
                        if previous == state {
                            continue;
                        }
                        *previous = *state;
                    } else {
                        latest_states.push((session_id.clone(), *state));
                    }
                }
                if rendered >= 24 {
                    break;
                }
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                row.add_css_class("activity-row");
                let label = activity_label(&activity_event_text(event), false);
                label.set_hexpand(true);
                row.append(&label);
                if let Some(session_id) = event.session_id.as_deref() {
                    row.append(&self.session_button(session_id, "Open"));
                }
                let age = activity_label(&relative_age(event.at_millis), true);
                row.append(&age);
                self.activity_items.append(&row);
                rendered += 1;
            }
        }
    }

    fn render_attention(&self, attention: &Attention, board: Option<&Board>) {
        let pending = self.pending_attention.borrow().contains(&attention.id);
        let online = self.activity_online.get();
        let frame = gtk::Frame::new(None);
        frame.add_css_class("attention-card");
        let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
        body.set_margin_top(8);
        body.set_margin_bottom(8);
        body.set_margin_start(8);
        body.set_margin_end(8);
        if pending || !online {
            body.append(&activity_label(
                if pending {
                    "Sending response…"
                } else {
                    "Offline · this request remains open"
                },
                true,
            ));
        }

        let title = gtk::Label::new(Some(&format!(
            "{} · {}",
            attention_kind_label(attention.kind),
            relative_age(attention.created_at_millis)
        )));
        title.set_xalign(0.0);
        title.add_css_class("caption-heading");
        body.append(&title);
        let reason = activity_label(&attention.reason, false);
        reason.set_selectable(true);
        body.append(&reason);

        let mut target = Vec::new();
        if let Some(card_id) = &attention.card_id {
            let card_title = board
                .and_then(|board| {
                    board
                        .find_id(card_id)
                        .map(|(column, card)| board.columns[column].cards[card].title.clone())
                })
                .unwrap_or_else(|| card_id.clone());
            target.push(format!("Card: {card_title}"));
        }
        if let Some(session_id) = &attention.session_id {
            target.push(format!("Agent: {}", short_session(session_id)));
        }
        if !target.is_empty() {
            body.append(&activity_label(&target.join("\n"), true));
        }

        let buttons = gtk::FlowBox::new();
        buttons.set_selection_mode(gtk::SelectionMode::None);
        buttons.set_min_children_per_line(1);
        buttons.set_max_children_per_line(2);
        buttons.set_row_spacing(4);
        buttons.set_column_spacing(4);
        if let Some(session_id) = &attention.session_id {
            buttons.append(&self.session_button(session_id, "Open session"));
        }
        if let Some(card_id) = &attention.card_id {
            let button = gtk::Button::with_label("Open card");
            button.add_css_class("flat");
            let parent = self.dialog_parent.clone();
            let project_id = self.project_id;
            let card_id = card_id.clone();
            button.connect_clicked(move |_| {
                let _ = gtk::prelude::WidgetExt::activate_action(
                    &parent,
                    "win.project-board-card",
                    Some(&(project_id, card_id.as_str()).to_variant()),
                );
            });
            buttons.append(&button);
        }
        if attention.seen_at_millis.is_none() {
            buttons.append(&self.attention_change_button(
                attention,
                "Mark seen",
                AttentionChange::MarkSeen,
            ));
        }
        if attention.acknowledged_at_millis.is_none() {
            buttons.append(&self.attention_change_button(
                attention,
                "Acknowledge",
                AttentionChange::Acknowledge,
            ));
        }
        for action in &attention.allowed_actions {
            match action {
                AttentionActionKind::Answer => {
                    let button = gtk::Button::with_label("Answer");
                    button.set_sensitive(online && !pending);
                    let parent = self.dialog_parent.clone();
                    let attention = attention.clone();
                    let project_id = self.project_id;
                    let home = self.session_home.clone();
                    let tx = self.activity_tx.clone();
                    let pending = self.pending_attention.clone();
                    let feedback = self.activity_feedback.clone();
                    button.connect_clicked(move |_| {
                        answer_dialog(
                            &parent, project_id, &home, &tx, &pending, &feedback, &attention,
                        );
                    });
                    buttons.append(&button);
                }
                AttentionActionKind::Approve => buttons.append(&self.attention_change_button(
                    attention,
                    "Approve",
                    AttentionChange::Respond(AttentionResponse::Approve),
                )),
                AttentionActionKind::Deny => buttons.append(&self.attention_change_button(
                    attention,
                    "Deny",
                    AttentionChange::Respond(AttentionResponse::Deny),
                )),
                AttentionActionKind::Dismiss => buttons.append(&self.attention_change_button(
                    attention,
                    "Dismiss",
                    AttentionChange::Respond(AttentionResponse::Dismiss),
                )),
            }
        }
        body.append(&buttons);
        frame.set_child(Some(&body));
        self.activity_items.append(&frame);
    }

    fn session_button(&self, session_id: &str, title: &str) -> gtk::Button {
        let button = gtk::Button::with_label(title);
        button.add_css_class("flat");
        let parent = self.dialog_parent.clone();
        let project_id = self.project_id;
        let session_id = session_id.to_string();
        button.connect_clicked(move |_| {
            let _ = gtk::prelude::WidgetExt::activate_action(
                &parent,
                "win.activity-session-open",
                Some(&(project_id, session_id.clone()).to_variant()),
            );
        });
        button
    }

    fn attention_change_button(
        &self,
        attention: &Attention,
        title: &str,
        change: AttentionChange,
    ) -> gtk::Button {
        let button = gtk::Button::with_label(title);
        let project_id = self.project_id;
        let home = self.session_home.clone();
        let tx = self.activity_tx.clone();
        let pending = self.pending_attention.clone();
        let feedback = self.activity_feedback.clone();
        let attention = attention.clone();
        button.set_sensitive(
            self.activity_online.get() && !self.pending_attention.borrow().contains(&attention.id),
        );
        button.connect_clicked(move |button| {
            button.set_sensitive(false);
            submit_attention_change(
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

    /// Watch the board's live shape: card and column counts after every read
    /// of BOARD.md, whoever wrote it. Replays the last counts, so an observer
    /// wired after the pane opened is not left waiting for the next edit.
    pub fn set_info_observer(&self, observer: impl Fn(Option<String>) + 'static) {
        let replay = self.last_stats.borrow().clone();
        *self.observer.borrow_mut() = Some(Box::new(observer));
        if let Some(stats) = replay {
            if let Some(emit) = self.observer.borrow().as_ref() {
                emit(Some(stats));
            }
        }
    }

    fn reload(&self) {
        let Some(board) = self.board() else {
            eprintln!(
                "radar: could not read the board for {}",
                self.project.display()
            );
            return;
        };
        while let Some(child) = self.columns_box.first_child() {
            self.columns_box.remove(&child);
        }
        self.card_widgets.borrow_mut().clear();
        for column in &board.columns {
            self.columns_box.append(&self.build_column(&board, column));
        }
        let summary = summarize(&board);
        let stats = format!(
            "{}/{} done · {} claimed · {} in progress · {} review",
            summary.done, summary.total, summary.claimed, summary.in_progress, summary.review
        );
        *self.last_stats.borrow_mut() = Some(stats.clone());
        if let Some(emit) = self.observer.borrow().as_ref() {
            emit(Some(stats));
        }
        if let Some(id) = self.highlighted_card.borrow().as_ref() {
            if let Some(card) = self.card_widgets.borrow().get(id) {
                card.add_css_class("board-card-target");
            }
        }
        self.render_activity();
    }

    /// The current board, read from the store (the daemon is the only writer).
    fn board(&self) -> Option<Board> {
        crate::session::daemon::board_state(&self.session_home, self.project_id, &self.project)
            .ok()
            .map(|state| state.to_board())
    }

    /// Re-read and redraw the board. Called by the app when the store publishes
    /// a `BoardChanged` event, so every client refreshes through one path.
    pub fn refresh(&self) {
        self.reload();
    }

    fn build_column(&self, b: &Board, column: &Column) -> gtk::Widget {
        let column_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        column_box.add_css_class("board-column");
        column_box.set_width_request(230);
        column_box.set_hexpand(true);
        column_box.set_vexpand(true);

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
        let cards = gtk::Box::new(gtk::Orientation::Vertical, 6);
        cards.set_vexpand(true);
        column_box.append(&header);

        for card in &column.cards {
            let widget = self.build_card(card);
            self.card_widgets
                .borrow_mut()
                .insert(card.id.clone(), widget.clone());
            cards.append(&widget);
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
        let home = self.session_home.clone();
        let project_id = self.project_id;
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
            let card_id = crate::session::daemon::board_state(&home, project_id, &project)
                .ok()
                .and_then(|state| {
                    state
                        .cards
                        .into_iter()
                        .find(|card| card.title == title)
                        .map(|card| card.id)
                });
            let Some(card_id) = card_id else {
                return false;
            };
            match crate::session::daemon::board_card_move(
                &home,
                project_id,
                &card_id,
                &column_name,
                None,
                &super::gui_command_id("move"),
            ) {
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
        let project_id = self.project_id;
        let home = self.session_home.clone();
        let parent = self.dialog_parent.clone();
        let names: Vec<String> = b.columns.iter().map(|c| c.name.clone()).collect();
        let default_column = column.name.clone();
        add.connect_clicked(move |_| {
            card_dialog(
                &parent,
                project_id,
                &home,
                None,
                &names,
                Some(&default_column),
            );
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

        let mut claim_button: Option<gtk::Button> = None;
        if let Some(who) = &card.claimed_by {
            // The claim is a link: clicking it opens that agent's session.
            // The win.session-open action resolves the name to the agent tab
            // it runs in; a card's own click (edit) never fires — see the
            // pick guard on the card's gesture below.
            let claim = gtk::Button::new();
            let label = gtk::Label::new(Some(&format!("@{who}")));
            label.set_xalign(0.0);
            label.add_css_class("board-claim");
            label.add_css_class("caption");
            claim.set_child(Some(&label));
            claim.add_css_class("board-claim-button");
            claim.add_css_class("flat");
            claim.set_cursor(gdk::Cursor::from_name("pointer", None).as_ref());
            claim.set_tooltip_text(Some(&format!("Jump to @{who}'s session")));
            let parent = self.dialog_parent.clone();
            let who_for_click = who.clone();
            claim.connect_clicked(move |_| {
                let _ = gtk::prelude::WidgetExt::activate_action(
                    &parent,
                    "win.session-open",
                    Some(&who_for_click.to_variant()),
                );
            });
            card_box.append(&claim);
            claim_button = Some(claim);
        }

        // Click to edit; drag to move between columns. The @claim link is
        // its own click target: a release that lands on it opens the
        // session, not this dialog — the pick says where the release
        // landed, whichever gesture would have won the press.
        let click = gtk::GestureClick::new();
        click.set_button(1);
        let project_id = self.project_id;
        let home = self.session_home.clone();
        let project = self.project.clone();
        let parent = self.dialog_parent.clone();
        let card_id_for_edit = card.id.clone();
        let card_for_pick = card_box.clone();
        let claim_for_pick = claim_button;
        click.connect_released(move |gesture, _, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            if let Some(claim) = claim_for_pick.as_ref() {
                if let Some(picked) = card_for_pick.pick(x, y, gtk::PickFlags::DEFAULT) {
                    if picked.is_ancestor(claim) {
                        return;
                    }
                }
            }
            let home = home.clone();
            let project = project.clone();
            let parent = parent.clone();
            let card_id = card_id_for_edit.clone();
            // Resolve the card when the dialog opens, not now: an agent may
            // have changed it since it was drawn.
            glib::idle_add_local(move || {
                edit_dialog(&parent, project_id, &home, &project, &card_id);
                glib::ControlFlow::Break
            });
        });
        card_box.add_controller(click);

        // Dragging is handled by the board operation, which re-reads the
        // file before moving the card so concurrent agent edits survive.
        let content = gdk::ContentProvider::for_value(&format!("card:{}", card.title).to_value());
        let source = gtk::DragSource::builder()
            .actions(gdk::DragAction::MOVE)
            .content(&content)
            .build();
        card_box.add_controller(source);

        card_box.set_tooltip_text(Some("Open card details · Enter"));
        let keys = gtk::EventControllerKey::new();
        let project_id = self.project_id;
        let home = self.session_home.clone();
        let project = self.project.clone();
        let parent = self.dialog_parent.clone();
        let card_id = card.id.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            if !matches!(key, gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::space) {
                return glib::Propagation::Proceed;
            }
            let home = home.clone();
            let project = project.clone();
            let parent = parent.clone();
            let card_id = card_id.clone();
            glib::idle_add_local(move || {
                edit_dialog(&parent, project_id, &home, &project, &card_id);
                glib::ControlFlow::Break
            });
            glib::Propagation::Stop
        });
        card_box.add_controller(keys);
        card_box.set_focusable(true);
        card_box.upcast()
    }
}

pub(super) fn activity_label(text: &str, muted: bool) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    if muted {
        label.add_css_class("caption");
        label.add_css_class("dim-label");
    }
    label
}

fn append_activity_heading(parent: &gtk::Box, text: &str) {
    let heading = gtk::Label::new(Some(text));
    heading.set_xalign(0.0);
    heading.add_css_class("caption-heading");
    heading.add_css_class("activity-section-heading");
    parent.append(&heading);
}

pub(super) fn agent_state_label(state: crate::session::activity::AgentState) -> &'static str {
    use crate::session::activity::AgentState;
    match state {
        AgentState::Unknown => "Unknown",
        AgentState::Working => "Working",
        AgentState::WaitingForInput => "Waiting for input",
        AgentState::WaitingForApproval => "Waiting for approval",
        AgentState::Idle => "Idle",
    }
}

pub(super) fn attention_kind_label(kind: crate::session::activity::AttentionKind) -> &'static str {
    use crate::session::activity::AttentionKind;
    match kind {
        AttentionKind::Question => "Question",
        AttentionKind::Approval => "Approval",
        AttentionKind::Failure => "Failure",
        AttentionKind::Review => "Review",
    }
}

pub(super) fn short_session(session_id: &str) -> String {
    let mut parts: Vec<&str> = session_id.rsplit('-').take(3).collect();
    parts.reverse();
    parts.join("-")
}

pub(super) fn relative_age(at_millis: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(at_millis);
    let seconds = now.saturating_sub(at_millis).max(0) / 1000;
    match seconds {
        0..=4 => "just now".to_string(),
        5..=59 => format!("{seconds}s ago"),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

fn activity_event_text(event: &ActivityEvent) -> String {
    match &event.payload {
        ActivityPayload::AgentState { state, message } => message.as_ref().map_or_else(
            || format!("Agent is {}", agent_state_label(*state).to_lowercase()),
            |message| format!("{} · {message}", agent_state_label(*state)),
        ),
        ActivityPayload::Message { text } => text.clone(),
        ActivityPayload::AttentionRequested {
            attention_kind,
            reason,
            ..
        } => format!("{}: {reason}", attention_kind_label(*attention_kind)),
        ActivityPayload::AttentionSeen { .. } => "Request marked seen".to_string(),
        ActivityPayload::AttentionAcknowledged { .. } => "Request acknowledged".to_string(),
        ActivityPayload::AttentionResolved { response, .. } => match response {
            AttentionResponse::Answer(answer) => format!("Answered: {answer}"),
            AttentionResponse::Approve => "Approved".to_string(),
            AttentionResponse::Deny => "Denied".to_string(),
            AttentionResponse::Dismiss => "Request dismissed".to_string(),
        },
        ActivityPayload::BoardChanged { action, title, .. } => title
            .as_ref()
            .map_or_else(|| action.clone(), |title| format!("{action}: {title}")),
        ActivityPayload::SessionLifecycle { state, detail } => detail.as_ref().map_or_else(
            || format!("Session {state}"),
            |detail| format!("{state}: {detail}"),
        ),
        ActivityPayload::CommandResult { ok, detail, .. } => detail.as_ref().map_or_else(
            || {
                if *ok {
                    "Command completed".to_string()
                } else {
                    "Command failed".to_string()
                }
            },
            Clone::clone,
        ),
    }
}

pub(super) fn submit_attention_change(
    project_id: i64,
    home: &Path,
    tx: &SyncSender<super::ActivityNotice>,
    pending: &Rc<RefCell<HashSet<String>>>,
    feedback: &gtk::Label,
    attention: &Attention,
    change: AttentionChange,
) {
    let request_id = attention.id.clone();
    if !pending.borrow_mut().insert(request_id.clone()) {
        return;
    }
    feedback.set_text("Sending response…");
    feedback.remove_css_class("error");
    feedback.set_visible(true);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let command = Command::ChangeAttention(ChangeAttention {
        project_id,
        request_id: request_id.clone(),
        command_id: format!("gui-{}-{now:x}", std::process::id()),
        expected_revision: attention.revision,
        change,
    });
    let home = home.to_path_buf();
    let tx = tx.clone();
    std::thread::spawn(move || {
        let result = match Client::request(&home, command) {
            Ok(Response::AttentionChanged(result)) => Ok(Box::new(result)),
            Ok(other) => Err(format!("unexpected server response: {other:?}")),
            Err(error) => Err(error.to_string()),
        };
        let _ = tx.send(super::ActivityNotice::Mutation {
            project_id,
            request_id,
            result,
        });
    });
}

pub(super) fn answer_dialog(
    parent: &gtk::Window,
    project_id: i64,
    home: &Path,
    tx: &SyncSender<super::ActivityNotice>,
    pending: &Rc<RefCell<HashSet<String>>>,
    feedback: &gtk::Label,
    attention: &Attention,
) {
    let window = gtk::Window::builder()
        .title("Answer question")
        .transient_for(parent)
        .modal(true)
        .resizable(false)
        .default_width(420)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(14);
    content.set_margin_bottom(14);
    content.set_margin_start(14);
    content.set_margin_end(14);
    let prompt = activity_label(&attention.reason, false);
    content.append(&prompt);
    let answer = gtk::Entry::new();
    answer.set_placeholder_text(Some("Your answer"));
    content.append(&answer);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let window_for_cancel = window.clone();
    cancel.connect_clicked(move |_| window_for_cancel.close());
    buttons.append(&cancel);
    let submit = gtk::Button::with_label("Send answer");
    submit.add_css_class("suggested-action");
    let window_for_submit = window.clone();
    let answer_for_submit = answer.clone();
    let home = home.to_path_buf();
    let tx = tx.clone();
    let pending = pending.clone();
    let feedback = feedback.clone();
    let attention = attention.clone();
    submit.connect_clicked(move |_| {
        let text = answer_for_submit.text().trim().to_string();
        if text.is_empty() {
            answer_for_submit.grab_focus();
            return;
        }
        submit_attention_change(
            project_id,
            &home,
            &tx,
            &pending,
            &feedback,
            &attention,
            AttentionChange::Respond(AttentionResponse::Answer(text)),
        );
        window_for_submit.close();
    });
    buttons.append(&submit);
    content.append(&buttons);
    window.set_child(Some(&content));
    window.present();
    answer.grab_focus();
}

/// The add/edit dialog. `existing` is the card's current column and content,
/// when editing; `None` adds a card, placed in `default_column`.
pub(super) fn card_dialog(
    parent: &gtk::Window,
    project_id: i64,
    session_home: &Path,
    existing: Option<(&str, &StoredCard)>,
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
        if !card.body.trim().is_empty() {
            notes.buffer().set_text(&card.body);
        }
    }
    let notes_frame = gtk::Frame::new(None);
    notes_frame.set_child(Some(&notes));
    outer.append(&notes_frame);

    outer.append(&caption("Claimed by"));
    let claim_entry = gtk::Entry::new();
    claim_entry.set_placeholder_text(Some("nobody — or a name, like claude"));
    if let Some((_, card)) = &existing {
        if let Some(who) = &card.claim {
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

    if let Some((_, card)) = &existing {
        let window_for_delete = window.clone();
        let home = session_home.to_path_buf();
        let card_id = card.id.clone();
        let delete = gtk::Button::with_label("Delete");
        delete.add_css_class("destructive-action");
        delete.set_halign(gtk::Align::Start);
        delete.set_hexpand(true);
        delete.connect_clicked(move |_| {
            if let Err(error) = crate::session::daemon::board_card_remove(
                &home,
                project_id,
                &card_id,
                &super::gui_command_id("remove"),
            ) {
                eprintln!("radar: could not remove the card: {error}");
                return;
            }
            window_for_delete.close();
        });
        buttons.append(&delete);
    }

    let window_for_save = window.clone();
    let title_entry = title_entry.clone();
    let notes = notes.clone();
    let claim_entry = claim_entry.clone();
    let dropdown = dropdown.clone();
    let home = session_home.to_path_buf();
    let existing_card = existing.map(|(column, card)| (column.to_string(), card.clone()));
    let columns_for_save = columns.to_vec();
    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    save.connect_clicked(move |_| {
        let title = title_entry.text().trim().to_string();
        if title.is_empty() {
            title_entry.grab_focus();
            return;
        }
        let body = notes
            .buffer()
            .text(
                &notes.buffer().start_iter(),
                &notes.buffer().end_iter(),
                true,
            )
            .to_string();
        let claim = {
            let text = claim_entry
                .text()
                .trim()
                .trim_start_matches('@')
                .to_string();
            (!text.is_empty()).then_some(text)
        };
        let column = columns_for_save
            .get(dropdown.selected() as usize)
            .cloned()
            .unwrap_or_default();
        let command = super::gui_command_id("card");
        let result = match &existing_card {
            Some((old_column, card)) => crate::session::daemon::board_card_update(
                &home,
                project_id,
                &card.id,
                Some(&title),
                Some(&body),
                Some(card.revision),
                &command,
            )
            .and_then(|_| {
                if &column != old_column {
                    crate::session::daemon::board_card_move(
                        &home,
                        project_id,
                        &card.id,
                        &column,
                        None,
                        &super::gui_command_id("move"),
                    )?;
                }
                if card.claim.as_deref() != claim.as_deref() {
                    crate::session::daemon::board_card_claim(
                        &home,
                        project_id,
                        &card.id,
                        claim.as_deref(),
                        None,
                        &super::gui_command_id("claim"),
                    )?;
                }
                Ok(())
            }),
            None => crate::session::daemon::board_card_add(
                &home,
                project_id,
                Some(&column),
                &title,
                &body,
                claim.as_deref(),
                &command,
            )
            .map(|_| ()),
        };
        match result {
            Ok(()) => window_for_save.close(),
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

/// Clicking a card: read its current state from the store, then edit that.
fn edit_dialog(
    parent: &gtk::Window,
    project_id: i64,
    session_home: &Path,
    project: &Path,
    card_id: &str,
) {
    let Ok(state) = crate::session::daemon::board_state(session_home, project_id, project) else {
        return;
    };
    let Some(stored) = state.cards.iter().find(|card| card.id == card_id) else {
        eprintln!("radar: the card {card_id} is gone from the board");
        return;
    };
    let column = state
        .lanes
        .iter()
        .find(|lane| lane.id == stored.lane_id)
        .map(|lane| lane.name.clone())
        .unwrap_or_default();
    let columns: Vec<String> = state.lanes.iter().map(|lane| lane.name.clone()).collect();
    card_dialog(
        parent,
        project_id,
        session_home,
        Some((&column, stored)),
        &columns,
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::summarize;
    use crate::board::{Board, Card, Column};

    fn card(id: &str, claimed_by: Option<&str>, done: bool) -> Card {
        Card {
            id: id.to_string(),
            title: id.to_string(),
            body: Vec::new(),
            claimed_by: claimed_by.map(str::to_string),
            done,
        }
    }

    #[test]
    fn progress_counts_done_column_and_keeps_review_separate_from_checkbox() {
        let board = Board {
            header: String::new(),
            columns: vec![
                Column {
                    name: "Backlog".to_string(),
                    cards: vec![card("todo", Some("codex-abc123"), true)],
                },
                Column {
                    name: "In progress".to_string(),
                    cards: vec![card("active", None, false)],
                },
                Column {
                    name: "review".to_string(),
                    cards: vec![card("review", Some("claude-def456"), true)],
                },
                Column {
                    name: " Done ".to_string(),
                    cards: vec![card("done", None, false)],
                },
            ],
        };

        let summary = summarize(&board);
        assert_eq!(
            (
                summary.done,
                summary.total,
                summary.claimed,
                summary.in_progress,
                summary.review,
            ),
            (1, 4, 2, 1, 1)
        );

        assert_eq!(
            summary
                .lanes
                .iter()
                .map(|lane| {
                    (
                        lane.name.as_str(),
                        lane.done,
                        lane.cards
                            .iter()
                            .map(|card| (card.id.as_str(), card.claim.as_deref()))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>(),
            vec![
                ("Backlog", false, vec![("todo", Some("codex-abc123"))]),
                ("In progress", false, vec![("active", None)]),
                ("review", false, vec![("review", Some("claude-def456"))]),
                ("Done", true, vec![("done", None)]),
            ]
        );
    }
}
