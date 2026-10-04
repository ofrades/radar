//! Board summaries and attention controls shared by Home and card conversations.
//! No workspace pane: project boards live exclusively in Home.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc::SyncSender;

use crate::session::activity::{Attention, AttentionChange, AttentionResponse, ChangeAttention};
use crate::session::board_store::{BoardState, StoredCard};
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
    pub facts: Option<crate::session::lane::DerivedCard>,
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

fn work_card(card: &StoredCard) -> WorkCard {
    WorkCard {
        id: card.id.clone(),
        title: card.title.clone(),
        facts: None,
        claim: card
            .claim
            .as_deref()
            .filter(|claim| !claim.is_empty())
            .map(str::to_string),
    }
}

pub(super) fn summarize(state: &BoardState) -> BoardSummary {
    let mut summary = BoardSummary::default();
    for lane in &state.lanes {
        let name = lane.name.trim();
        // The lane's kind is its status in the store; a card is done exactly
        // while it sits in a done-kind lane.
        let done = lane.kind == "done";
        let in_progress = lane.kind == "in_progress" || name.eq_ignore_ascii_case("in progress");
        let review = lane.kind == "review" || name.eq_ignore_ascii_case("review");
        let mut cards = Vec::new();
        for card in state.cards.iter().filter(|card| card.lane_id == lane.id) {
            summary.total += 1;
            if done {
                summary.done += 1;
            }
            if in_progress {
                summary.in_progress += 1;
            }
            if review {
                summary.review += 1;
            }
            if card.claim.as_deref().is_some_and(|claim| !claim.is_empty()) {
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

/// Join by stable card id, never by lane position or claim name.
pub(super) fn summarize_derived(board: &crate::session::daemon::DerivedBoard) -> BoardSummary {
    let mut summary = summarize(&board.state);
    for lane in &mut summary.lanes {
        for card in &mut lane.cards {
            card.facts = board
                .derived
                .iter()
                .find(|facts| facts.card_id == card.id)
                .cloned();
        }
    }
    summary
}

fn live_label(facts: &crate::session::lane::DerivedCard) -> String {
    use crate::session::lane::{Column, LoopTurn, WorkerFact};
    let worker = match (facts.turn, facts.worker) {
        (LoopTurn::Human, Some(WorkerFact::Exited | WorkerFact::AgentStopped)) => {
            "Agent stopped · Your turn"
        }
        (LoopTurn::Human, Some(WorkerFact::Quiet)) => "Needs you · Agent quiet",
        (LoopTurn::Human, _) => "Needs you",
        (LoopTurn::Agent, Some(WorkerFact::Running)) => "Agent running",
        (LoopTurn::Agent, Some(WorkerFact::Quiet)) => "Agent quiet",
        (LoopTurn::Agent, _) => "Agent working",
        (LoopTurn::Nobody, Some(WorkerFact::AgentReady)) => "Agent ready",
        (LoopTurn::Nobody, Some(WorkerFact::Exited | WorkerFact::AgentStopped)) => "Agent stopped",
        (LoopTurn::Nobody, Some(WorkerFact::Quiet)) => "Agent quiet",
        (LoopTurn::Nobody, _) => "No active agent",
    };
    match facts.pr.as_ref().and(facts.column) {
        Some(column) => format!(
            "{worker} · {}",
            match column {
                Column::Validating => "Validating",
                Column::NeedsReview => "Needs review",
                Column::Ready => "Ready",
            }
        ),
        None => worker.into(),
    }
}

fn pr_label(pr: &crate::session::pr::PrFacts) -> String {
    use crate::session::pr::CiState;
    let mut parts = Vec::new();
    if pr.draft {
        parts.push("Draft".to_string());
    }
    parts.push(match pr.ci {
        CiState::Pending => "Checks pending".into(),
        CiState::Passing => "Checks passing".into(),
        CiState::Failing => "Checks failing".into(),
        CiState::None => "No checks".into(),
    });
    match pr.review.as_str() {
        "APPROVED" => parts.push("Approved".into()),
        "CHANGES_REQUESTED" => parts.push("Changes requested".into()),
        "REVIEW_REQUIRED" => parts.push("Review required".into()),
        _ => {}
    }
    if pr.mergeable == "CONFLICTING" {
        parts.push("Merge conflicts".into());
    }
    parts.join(" · ")
}

/// Shared by Home, board rows and card detail. Links live outside the card
/// navigation button so opening a PR never opens the card instead.
pub(super) fn facts_widget(
    project_id: i64,
    facts: &crate::session::lane::DerivedCard,
) -> gtk::Widget {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 2);
    body.add_css_class("card-live-facts");
    body.set_widget_name(&facts_widget_name(project_id, &facts.card_id));
    update_facts_widget(&body, facts);
    body.upcast()
}

pub(super) fn facts_widget_name(project_id: i64, card_id: &str) -> String {
    format!("card-facts-{project_id}-{card_id}")
}

/// Update only the facts, keeping card editors, answers and link focus intact.
pub(super) fn update_facts_widget(body: &gtk::Box, facts: &crate::session::lane::DerivedCard) {
    let live = body
        .first_child()
        .and_then(|child| child.downcast::<gtk::Label>().ok())
        .unwrap_or_else(|| {
            let label = activity_label("", true);
            body.append(&label);
            label
        });
    live.set_text(&live_label(facts));
    let quiet = facts.worker == Some(crate::session::lane::WorkerFact::Quiet);
    live.set_tooltip_text(quiet.then_some(
        "The worker is running, but Radar has not received a lifecycle signal after the grace period. Check its terminal or restart it if it is stalled.",
    ));
    live.remove_css_class("todo-claim");
    live.add_css_class("dim-label");
    if facts.turn == crate::session::lane::LoopTurn::Human || quiet {
        live.remove_css_class("dim-label");
        live.add_css_class("todo-claim");
    }
    if let Some(pr) = &facts.pr {
        let link = live
            .next_sibling()
            .and_then(|child| child.downcast::<gtk::LinkButton>().ok())
            .unwrap_or_else(|| {
                let link = gtk::LinkButton::new(&pr.url);
                link.set_halign(gtk::Align::Start);
                link.add_css_class("caption");
                body.append(&link);
                body.append(&activity_label("", true));
                link
            });
        link.set_uri(&pr.url);
        link.set_label(&format!("PR #{}", pr.number));
        link.set_tooltip_text(Some(&format!("{}\n{}", pr.title, pr.url)));
        let status = link
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        status.set_text(&pr_label(pr));
        status.set_tooltip_text(
            if pr.failing.is_empty() {
                None
            } else {
                Some(format!("Failing checks: {}", pr.failing.join(", ")))
            }
            .as_deref(),
        );
    } else {
        while let Some(child) = live.next_sibling() {
            body.remove(&child);
        }
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
    use crate::session::board_store::{BoardState, Lane, StoredCard};

    fn lane(id: i64, name: &str, kind: &str) -> Lane {
        Lane {
            id,
            name: name.to_string(),
            kind: kind.to_string(),
            position: id,
        }
    }

    fn card(
        id: &str,
        lane_id: i64,
        lane_name: &str,
        claim: Option<&str>,
        done: bool,
    ) -> StoredCard {
        StoredCard {
            id: id.to_string(),
            project_id: 1,
            lane_id,
            lane: lane_name.to_string(),
            done,
            position: 0,
            title: id.to_string(),
            body: String::new(),
            claim: claim.map(str::to_string),
            revision: 1,
            created_at_millis: 0,
            updated_at_millis: 0,
        }
    }

    #[test]
    fn progress_counts_done_column_and_keeps_review_separate_from_checkbox() {
        let state = BoardState {
            project_id: 1,
            lanes: vec![
                lane(0, "Backlog", "todo"),
                lane(1, "In progress", "in_progress"),
                lane(2, "review", "review"),
                lane(3, " Done ", "done"),
            ],
            cards: vec![
                card("todo", 0, "Backlog", Some("codex-abc123"), false),
                card("active", 1, "In progress", None, false),
                card("review", 2, "review", Some("claude-def456"), false),
                card("done", 3, " Done ", None, true),
            ],
        };

        let summary = summarize(&state);
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

    fn facts(card_id: &str) -> crate::session::lane::DerivedCard {
        crate::session::lane::DerivedCard {
            card_id: card_id.into(),
            claim: None,
            turn: crate::session::lane::LoopTurn::Human,
            worker: Some(crate::session::lane::WorkerFact::Running),
            pr: None,
            column: None,
            returned: false,
        }
    }

    fn pull_request() -> crate::session::pr::PrFacts {
        crate::session::pr::PrFacts {
            number: 42,
            url: "https://github.com/example/project/pull/42".into(),
            title: "Repair session binding".into(),
            branch: "card/active".into(),
            draft: false,
            state: "OPEN".into(),
            ci: crate::session::pr::CiState::Failing,
            failing: vec!["session integration".into()],
            review: "CHANGES_REQUESTED".into(),
            mergeable: "CONFLICTING".into(),
            updated_at: None,
        }
    }

    #[test]
    fn derived_summary_joins_by_id_without_reclassifying_lanes() {
        let mut active = facts("active");
        active.pr = Some(pull_request());
        let board = crate::session::daemon::DerivedBoard {
            state: BoardState {
                project_id: 1,
                lanes: vec![lane(1, "Review", "review")],
                cards: vec![
                    card("other", 1, "Review", None, false),
                    card("active", 1, "Review", Some("worker"), false),
                ],
            },
            derived: vec![active.clone()],
        };
        let summary = super::summarize_derived(&board);
        assert_eq!(summary.review, 2);
        assert_eq!(summary.in_progress, 0);
        assert!(summary.lanes[0].cards[0].facts.is_none());
        assert_eq!(summary.lanes[0].cards[1].facts, Some(active));
        // An open request wins even with a live worker, as the daemon says.
        assert_eq!(
            super::live_label(summary.lanes[0].cards[1].facts.as_ref().unwrap()),
            "Needs you"
        );
    }

    #[test]
    fn pr_status_distinguishes_missing_checks_and_unknown_mergeability() {
        let mut pr = pull_request();
        assert_eq!(
            super::pr_label(&pr),
            "Checks failing · Changes requested · Merge conflicts"
        );
        pr.ci = crate::session::pr::CiState::None;
        pr.review.clear();
        pr.mergeable = "UNKNOWN".into();
        assert_eq!(super::pr_label(&pr), "No checks");
        pr.draft = true;
        pr.ci = crate::session::pr::CiState::Pending;
        assert_eq!(super::pr_label(&pr), "Draft · Checks pending");
    }

    #[test]
    #[ignore = "requires a private D-Bus session and GTK display"]
    fn card_facts_render_live_status_and_actionable_pr() {
        use gtk::prelude::*;
        gtk::init().unwrap();
        let mut facts = facts("active");
        facts.pr = Some(pull_request());
        facts.column = Some(crate::session::lane::Column::NeedsReview);
        let body = super::facts_widget(1, &facts);
        let live = body
            .first_child()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        assert_eq!(live.text(), "Needs you · Needs review");
        let link = live
            .next_sibling()
            .unwrap()
            .downcast::<gtk::LinkButton>()
            .unwrap();
        assert_eq!(link.uri(), facts.pr.as_ref().unwrap().url);
        assert_eq!(link.label().as_deref(), Some("PR #42"));
        let status = link
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        assert_eq!(
            status.text(),
            "Checks failing · Changes requested · Merge conflicts"
        );
        assert!(status
            .tooltip_text()
            .unwrap()
            .contains("session integration"));
        facts.turn = crate::session::lane::LoopTurn::Agent;
        facts.worker = Some(crate::session::lane::WorkerFact::Quiet);
        facts.column = Some(crate::session::lane::Column::Validating);
        let pr = facts.pr.as_mut().unwrap();
        pr.ci = crate::session::pr::CiState::Passing;
        pr.failing.clear();
        pr.review = "APPROVED".into();
        pr.mergeable = "MERGEABLE".into();
        super::update_facts_widget(body.downcast_ref::<gtk::Box>().unwrap(), &facts);
        assert_eq!(live.text(), "Agent quiet · Validating");
        assert!(live.has_css_class("todo-claim"));
        assert!(live.tooltip_text().unwrap().contains("lifecycle signal"));
        assert_eq!(
            live.next_sibling().unwrap(),
            link.clone().upcast::<gtk::Widget>()
        );
        assert_eq!(status.text(), "Checks passing · Approved");
        assert!(status.tooltip_text().is_none());
        facts.worker = Some(crate::session::lane::WorkerFact::AgentReady);
        facts.turn = crate::session::lane::LoopTurn::Nobody;
        facts.column = Some(crate::session::lane::Column::Ready);
        super::update_facts_widget(body.downcast_ref::<gtk::Box>().unwrap(), &facts);
        assert_eq!(live.text(), "Agent ready · Ready");
        assert!(!live.has_css_class("todo-claim"));
        assert!(live.tooltip_text().is_none());
        facts.pr = None;
        super::update_facts_widget(body.downcast_ref::<gtk::Box>().unwrap(), &facts);
        assert_eq!(live.text(), "Agent ready");
        assert!(live.next_sibling().is_none());
        let no_pr = super::facts_widget(1, &facts);
        assert!(no_pr.first_child().unwrap().next_sibling().is_none());
    }
}
