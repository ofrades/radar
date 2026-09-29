//! Board summaries and attention controls shared by Home and card conversations.
//! No workspace pane: project boards live exclusively in Home.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc::SyncSender;

use crate::board::{Board, Card};
use crate::session::activity::{Attention, AttentionChange, AttentionResponse, ChangeAttention};
use crate::session::daemon::{Client, Command, Response};
use adw::prelude::*;

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

/// The lane's name for display. The store still calls the first lane
/// `Backlog`; the v2 migration renames it to `Todo`, but only a daemon
/// restart runs that. Showing `Todo` now keeps the rename visible without
/// waiting, and without touching what mutations send (always the store name).
pub(super) fn lane_label(name: &str) -> &str {
    if name.trim().eq_ignore_ascii_case("backlog") {
        "Todo"
    } else {
        name
    }
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
            name: lane_label(name).to_string(),
            done,
            cards,
        });
    }
    summary
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
                ("Todo", false, vec![("todo", Some("codex-abc123"))]),
                ("In progress", false, vec![("active", None)]),
                ("review", false, vec![("review", Some("claude-def456"))]),
                ("Done", true, vec![("done", None)]),
            ]
        );
    }
}
