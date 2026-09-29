//! Home: radar's cockpit.
//!
//! Home is where the whole workspace is understood without diving into any
//! terminal. It gathers the workspace into:
//!
//!   * **Needs you** — every unresolved request for human attention, with the
//!     actions to answer, approve, deny, or dismiss it in place.
//!   * **Projects** — one Basecamp-style lane per project: its to-dos (the
//!     board's cards, ticked once Done), its board's lane counts, and its
//!     running and external sessions.
//!
//! The sidebar is deliberately *not* this: it is a quiet project switcher with
//! a liveness pulse. When there are no projects yet, Home falls back to the
//! empty-state setup card — the same panel that gets a first project going.
//!
//! The cockpit is rebuilt whenever attention, sessions, git, or the layout
//! move (see `App::refresh_home`); it holds no state of its own.

use adw::prelude::*;
use std::path::Path;

use super::board;
use super::live_agents::{self, AgentSession};
use super::{App, SharedApp};
use crate::db::{NewWorkspaceLayout, Preferences, Project, Slot};
use crate::programs;
use crate::session::activity::{
    AgentState, Attention, AttentionActionKind, AttentionChange, AttentionResponse,
};

/// The Home panel. The cockpit when there is work to show; the setup/empty
/// state when there is not.
pub fn panel(app: &SharedApp) -> gtk::Widget {
    if app.projects.borrow().is_empty() {
        empty_panel(app)
    } else {
        view(app)
    }
}

/// The cross-project cockpit. Takes `&App` rather than the shared `Rc`: every
/// action it wires goes through a window action or the attention helpers, so
/// it can be rebuilt from the cheap `&self` refresh paths.
pub fn cockpit(app: &App) -> gtk::Widget {
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    scroll.add_css_class("home-scroll");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 20);
    content.add_css_class("home");
    content.add_css_class("home-cockpit");
    content.set_margin_top(20);
    content.set_margin_bottom(28);
    content.set_margin_start(28);
    content.set_margin_end(28);
    content.set_halign(gtk::Align::Fill);
    content.set_hexpand(true);

    let needs = unresolved_attention(app);
    let projects = app.projects.borrow().clone();
    let running = app
        .agent_sessions
        .borrow()
        .by_project
        .values()
        .flatten()
        .filter(|session| live_agents::sidebar_session_is_live(session))
        .count();

    // A one-line pulse for the whole workspace.
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let title = gtk::Label::new(Some("Home"));
    title.add_css_class("heading");
    title.set_xalign(0.0);
    header.append(&title);
    let projects_label = match projects.len() {
        1 => "1 project".to_string(),
        count => format!("{count} projects"),
    };
    let counts = gtk::Label::new(Some(&format!(
        "{projects_label} · {running} running · {} need you",
        needs.len()
    )));
    counts.add_css_class("caption");
    counts.add_css_class("dim-label");
    counts.set_hexpand(true);
    counts.set_xalign(1.0);
    header.append(&counts);
    content.append(&header);

    needs_you_section(app, &content, &needs);
    projects_section(app, &content, &projects);
    footer(app, &content);

    scroll.set_child(Some(&content));
    scroll.set_hexpand(true);
    scroll.set_vexpand(true);
    scroll.upcast()
}

/// The current Home view: the cockpit, a project's board, or a card
/// conversation — the Basecamp drill-down, with a way back.
pub fn view(app: &App) -> gtk::Widget {
    match app.home_nav.borrow().last().cloned() {
        Some(super::HomeView::Board(project_id)) => board_view(app, project_id),
        Some(super::HomeView::Card(project_id, card_id)) => card_view(app, project_id, &card_id),
        None => cockpit(app),
    }
}

/// A project's board, opened inside Home: a column per lane, a card per
/// to-do. Clicking a card opens its conversation; Back returns to the cockpit.
fn board_view(app: &App, project_id: i64) -> gtk::Widget {
    let project = app
        .projects
        .borrow()
        .iter()
        .find(|project| project.id == project_id)
        .cloned();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("home-view");
    let Some(project) = project else {
        root.append(&view_header("Board", None));
        root.append(&quiet("That project is no longer available"));
        return root.upcast();
    };
    let git = app
        .status
        .borrow()
        .get(&project_id)
        .map(|status| status.summary());
    root.append(&view_header(&project.name, git.as_deref()));
    let Some(state) = app.board_states.borrow().get(&project_id).cloned() else {
        root.append(&quiet("Loading the board…"));
        return root.upcast();
    };
    let board = state.to_board();

    let columns = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    columns.set_margin_top(14);
    columns.set_margin_bottom(14);
    columns.set_margin_start(16);
    columns.set_margin_end(16);
    columns.set_valign(gtk::Align::Fill);
    for column in &board.columns {
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 8);
        box_.add_css_class("board-column");
        box_.set_valign(gtk::Align::Start);
        box_.set_width_request(250);
        box_.set_vexpand(true);
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let name = gtk::Label::new(Some(&column.name));
        name.set_xalign(0.0);
        name.set_hexpand(true);
        name.add_css_class("caption-heading");
        head.append(&name);
        let count = gtk::Label::new(Some(&column.cards.len().to_string()));
        count.add_css_class("caption");
        count.add_css_class("dim-label");
        head.append(&count);
        box_.append(&head);
        for card in &column.cards {
            box_.append(&board_card_button(project_id, card));
        }
        columns.append(&box_);
    }
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&columns)
        .build();
    root.append(&scroller);
    root.upcast()
}

fn board_card_button(project_id: i64, card: &crate::board::Card) -> gtk::Widget {
    let button = gtk::Button::new();
    button.add_css_class("board-card");
    button.set_halign(gtk::Align::Fill);
    button.add_css_class("flat");
    button.set_tooltip_text(Some("Open this card"));
    button.set_action_name(Some("win.open-card"));
    button.set_action_target_value(Some(&(project_id, card.id.as_str()).to_variant()));
    let body = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let title = gtk::Label::new(Some(&card.title));
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.set_wrap(true);
    if card.done {
        title.add_css_class("todo-done");
    }
    body.append(&title);
    if let Some(who) = &card.claimed_by {
        let claim = gtk::Label::new(Some(&format!("@{who}")));
        claim.set_xalign(0.0);
        claim.add_css_class("caption");
        claim.add_css_class("todo-claim");
        body.append(&claim);
    }
    button.set_child(Some(&body));
    button.upcast()
}

/// A card conversation, opened inside Home.
fn card_view(app: &App, project_id: i64, card_id: &str) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("home-view");
    let project = app
        .projects
        .borrow()
        .iter()
        .find(|project| project.id == project_id)
        .cloned();
    let title = project
        .as_ref()
        .map(|project| project.name.clone())
        .unwrap_or_else(|| "Card".to_string());
    root.append(&view_header(&title, None));
    root.append(&super::card::detail(app, project_id, card_id));
    root.upcast()
}

/// The Back-to-Home header shared by the drill-down views.
fn view_header(title: &str, meta: Option<&str>) -> gtk::Widget {
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    bar.add_css_class("home-view-bar");
    let back = gtk::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Back to Home")
        .build();
    back.add_css_class("flat");
    back.set_action_name(Some("win.home-back"));
    bar.append(&back);
    let name = gtk::Label::new(Some(title));
    name.set_xalign(0.0);
    name.add_css_class("heading");
    bar.append(&name);
    if let Some(meta) = meta {
        let meta = gtk::Label::new(Some(meta));
        meta.add_css_class("caption");
        meta.add_css_class("dim-label");
        meta.set_xalign(1.0);
        meta.set_hexpand(true);
        bar.append(&meta);
    }
    bar.upcast()
}

// ---- Needs you ----

/// Every unresolved request across every project, newest first.
fn unresolved_attention(app: &App) -> Vec<(i64, Attention)> {
    let mut items = Vec::new();
    for (project_id, activity) in app.activity.borrow().iter() {
        for attention in &activity.snapshot.attention {
            if attention.is_unresolved() {
                items.push((*project_id, attention.clone()));
            }
        }
    }
    items.sort_by_key(|(_, attention)| std::cmp::Reverse(attention.created_at_millis));
    items
}

fn needs_you_section(app: &App, content: &gtk::Box, items: &[(i64, Attention)]) {
    if items.is_empty() {
        return;
    }
    append_heading(content, "Needs you", Some(&format!("{} open", items.len())));
    for (project_id, attention) in items {
        content.append(&attention_card(app, *project_id, attention));
    }
}

fn attention_card(app: &App, project_id: i64, attention: &Attention) -> gtk::Widget {
    let online = app
        .activity_online
        .borrow()
        .get(&project_id)
        .copied()
        .unwrap_or(false);
    let pending = app.home_pending_attention.borrow().contains(&attention.id);

    let frame = gtk::Frame::new(None);
    frame.add_css_class("attention-card");
    let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    body.set_margin_top(10);
    body.set_margin_bottom(10);
    body.set_margin_start(10);
    body.set_margin_end(10);

    let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let kind = gtk::Label::new(Some(&format!(
        "{} · {}",
        board::attention_kind_label(attention.kind),
        project_name(app, project_id)
    )));
    kind.add_css_class("caption-heading");
    kind.set_xalign(0.0);
    kind.set_hexpand(true);
    top.append(&kind);
    let age = gtk::Label::new(Some(&board::relative_age(attention.created_at_millis)));
    age.add_css_class("caption");
    age.add_css_class("dim-label");
    top.append(&age);
    body.append(&top);

    let reason = board::activity_label(&attention.reason, false);
    reason.set_selectable(true);
    body.append(&reason);

    let mut target = Vec::new();
    if let Some(card_id) = &attention.card_id {
        target.push(format!("Card {}", board::short_session(card_id)));
    }
    if let Some(session_id) = &attention.session_id {
        target.push(format!("Agent {}", board::short_session(session_id)));
    }
    if !target.is_empty() {
        body.append(&board::activity_label(&target.join(" · "), true));
    }
    if pending {
        body.append(&board::activity_label("Sending response…", true));
    } else if !online {
        body.append(&board::activity_label(
            "Offline · this request remains open",
            true,
        ));
    }

    let feedback = gtk::Label::new(None);
    feedback.add_css_class("caption");
    feedback.add_css_class("dim-label");
    feedback.set_xalign(0.0);
    feedback.set_visible(false);

    let buttons = gtk::FlowBox::new();
    buttons.set_selection_mode(gtk::SelectionMode::None);
    buttons.set_min_children_per_line(1);
    buttons.set_max_children_per_line(3);
    buttons.set_row_spacing(4);
    buttons.set_column_spacing(4);

    if let Some(session_id) = &attention.session_id {
        let open = gtk::Button::with_label("Open session");
        open.add_css_class("flat");
        open.set_action_name(Some("win.activity-session-open"));
        open.set_action_target_value(Some(&(project_id, session_id.as_str()).to_variant()));
        buttons.append(&open);
    }
    if let Some(card_id) = &attention.card_id {
        let open = gtk::Button::with_label("Open card");
        open.add_css_class("flat");
        open.set_action_name(Some("win.project-board-card"));
        open.set_action_target_value(Some(&(project_id, card_id.as_str()).to_variant()));
        buttons.append(&open);
    }
    if attention.seen_at_millis.is_none() {
        buttons.append(&change_button(
            app,
            project_id,
            attention,
            "Mark seen",
            AttentionChange::MarkSeen,
            &feedback,
            online,
            pending,
        ));
    }
    if attention.acknowledged_at_millis.is_none() {
        buttons.append(&change_button(
            app,
            project_id,
            attention,
            "Acknowledge",
            AttentionChange::Acknowledge,
            &feedback,
            online,
            pending,
        ));
    }
    for action in &attention.allowed_actions {
        match action {
            AttentionActionKind::Answer => {
                let button = gtk::Button::with_label("Answer");
                button.set_sensitive(online && !pending);
                let parent: gtk::Window = app.window.clone().upcast();
                let home = app.session_home.clone();
                let tx = app.activity_tx.clone();
                let pending_set = app.home_pending_attention.clone();
                let feedback = feedback.clone();
                let attention = attention.clone();
                button.connect_clicked(move |_| {
                    board::answer_dialog(
                        &parent,
                        project_id,
                        &home,
                        &tx,
                        &pending_set,
                        &feedback,
                        &attention,
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
                &feedback,
                online,
                pending,
            )),
            AttentionActionKind::Deny => buttons.append(&change_button(
                app,
                project_id,
                attention,
                "Deny",
                AttentionChange::Respond(AttentionResponse::Deny),
                &feedback,
                online,
                pending,
            )),
            AttentionActionKind::Dismiss => buttons.append(&change_button(
                app,
                project_id,
                attention,
                "Dismiss",
                AttentionChange::Respond(AttentionResponse::Dismiss),
                &feedback,
                online,
                pending,
            )),
        }
    }

    body.append(&buttons);
    body.append(&feedback);
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
    feedback: &gtk::Label,
    online: bool,
    pending: bool,
) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.set_sensitive(online && !pending);
    let home = app.session_home.clone();
    let tx = app.activity_tx.clone();
    let pending_set = app.home_pending_attention.clone();
    let feedback = feedback.clone();
    let attention = attention.clone();
    button.connect_clicked(move |button| {
        button.set_sensitive(false);
        board::submit_attention_change(
            project_id,
            &home,
            &tx,
            &pending_set,
            &feedback,
            &attention,
            change.clone(),
        );
    });
    button
}

// ---- Projects: one lane per project, with its to-dos and sessions ----

fn projects_section(app: &App, content: &gtk::Box, projects: &[Project]) {
    let to_dos = open_todo_count(app);
    let trailing = match to_dos {
        1 => "1 to-do".to_string(),
        count => format!("{count} to-dos"),
    };
    append_heading(content, "Projects", Some(&trailing));

    let grid = gtk::FlowBox::new();
    grid.set_selection_mode(gtk::SelectionMode::None);
    // Fill the width: one project takes the row, two split it, three+ form a
    // Basecamp-style row of lanes. Both hints are pinned to the same count so
    // FlowBox makes real columns instead of wrapping on natural widths.
    let per_line = projects.len().clamp(1, 3) as u32;
    grid.set_min_children_per_line(per_line);
    grid.set_max_children_per_line(per_line);
    grid.set_homogeneous(true);
    grid.set_row_spacing(14);
    grid.set_column_spacing(14);
    grid.set_valign(gtk::Align::Start);
    grid.add_css_class("lane-grid");
    for project in projects {
        grid.insert(&project_lane(app, project), -1);
    }
    content.append(&grid);
}

/// Open to-dos across every project: the cards in non-Done lanes.
fn open_todo_count(app: &App) -> usize {
    app.board_summaries
        .borrow()
        .values()
        .map(|summary| {
            summary
                .lanes
                .iter()
                .filter(|lane| !lane.done)
                .map(|lane| lane.cards.len())
                .sum::<usize>()
        })
        .sum()
}

/// One project's lane: header, board counts, its open to-dos, its running
/// sessions, and a way into the full board.
fn project_lane(app: &App, project: &Project) -> gtk::Widget {
    let lane = gtk::Box::new(gtk::Orientation::Vertical, 8);
    lane.add_css_class("lane");
    lane.set_valign(gtk::Align::Start);
    lane.set_hexpand(true);

    lane.append(&lane_header(app, project));

    let summaries = app.board_summaries.borrow();
    let summary = summaries.get(&project.id);

    // Board counts as pills: what each lane holds, at a glance.
    let pills = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    pills.add_css_class("lane-pills");
    let mut any_pill = false;
    if let Some(summary) = summary {
        for lane_summary in &summary.lanes {
            if lane_summary.cards.is_empty() {
                continue;
            }
            any_pill = true;
            let pill = gtk::Label::new(Some(&format!(
                "{} {}",
                lane_summary.name,
                lane_summary.cards.len()
            )));
            pill.add_css_class("pill");
            if lane_summary.done {
                pill.add_css_class("pill-done");
            } else if lane_summary.name.eq_ignore_ascii_case("in progress") {
                pill.add_css_class("pill-active");
            } else if lane_summary.name.eq_ignore_ascii_case("review") {
                pill.add_css_class("pill-review");
            }
            pills.append(&pill);
        }
    }
    if !any_pill {
        let pill = gtk::Label::new(Some("No board"));
        pill.add_css_class("pill");
        pills.append(&pill);
    }
    lane.append(&pills);

    lane.append(&section_label("To-dos"));
    if let Some(summary) = summary {
        let (open, done) = open_and_done(summary);
        if open.is_empty() && done.is_empty() {
            lane.append(&quiet("No to-dos yet"));
        }
        for (lane_name, card) in open.iter().take(8) {
            let note = latest_card_note(app, project.id, &card.id);
            lane.append(&todo_row(project.id, lane_name, card, note.as_deref()));
        }
        if open.len() > 8 {
            lane.append(&quiet(&format!("+{} more", open.len() - 8)));
        }
        if !done.is_empty() {
            lane.append(&quiet(&format!("{} done", done.len())));
            for card in done.iter().take(3) {
                lane.append(&todo_row(project.id, "Done", card, None));
            }
        }
    } else {
        lane.append(&quiet("No board"));
    }

    let mut sessions: Vec<(AgentSession, String)> = app
        .agent_sessions
        .borrow()
        .by_project
        .get(&project.id)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(live_agents::sidebar_session_is_live)
        .map(|session| {
            let title = session.title.clone();
            (session, title)
        })
        .collect();
    if !sessions.is_empty() {
        live_agents::sort_sidebar_sessions(&mut sessions);
        lane.append(&section_label("Sessions"));
        for (session, _) in sessions.iter().take(4) {
            lane.append(&session_row(app, project.id, session));
        }
        if sessions.len() > 4 {
            lane.append(&quiet(&format!("+{} more", sessions.len() - 4)));
        }
    }

    lane.append(&lane_footer(project, sessions.len()));
    lane.upcast()
}

fn lane_header(app: &App, project: &Project) -> gtk::Widget {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);

    let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    dot.add_css_class("lane-dot");
    dot.set_valign(gtk::Align::Center);
    dot.set_size_request(9, 9);
    header.append(&dot);

    let name = gtk::Button::with_label(&project.name);
    name.add_css_class("flat");
    name.add_css_class("lane-name");
    name.set_tooltip_text(Some("Open this project's workspace"));
    name.set_action_name(Some("win.open-project"));
    name.set_action_target_value(Some(&project.id.to_variant()));
    header.append(&name);

    let git = app
        .status
        .borrow()
        .get(&project.id)
        .map(|status| status.summary())
        .unwrap_or_else(|| "…".to_string());
    let git = gtk::Label::new(Some(&git));
    git.add_css_class("caption");
    git.add_css_class("dim-label");
    git.set_xalign(1.0);
    git.set_hexpand(true);
    git.set_ellipsize(gtk::pango::EllipsizeMode::Start);
    header.append(&git);

    let create = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Create an agent in this project")
        .build();
    if let Some(image) = create.child().and_downcast::<gtk::Image>() {
        image.set_pixel_size(13);
    }
    create.add_css_class("flat");
    create.add_css_class("cockpit-action");
    create.set_valign(gtk::Align::Center);
    create.set_action_name(Some("win.project-agent-create"));
    create.set_action_target_value(Some(&project.id.to_variant()));
    header.append(&create);
    header.upcast()
}

/// Split a board's lanes into open to-dos (in-progress, review, then the
/// rest) and the Done lane's cards.
fn open_and_done(
    summary: &board::BoardSummary,
) -> (Vec<(String, board::WorkCard)>, Vec<board::WorkCard>) {
    let mut lanes: Vec<&board::LaneSummary> = summary.lanes.iter().collect();
    lanes.sort_by_key(|lane| lane_rank(&lane.name, lane.done));
    let mut open = Vec::new();
    let mut done = Vec::new();
    for lane in lanes {
        if lane.done {
            done.extend(lane.cards.iter().cloned());
        } else {
            for card in &lane.cards {
                open.push((lane.name.clone(), card.clone()));
            }
        }
    }
    (open, done)
}

fn lane_rank(name: &str, done: bool) -> u8 {
    if done {
        return 9;
    }
    if name.eq_ignore_ascii_case("in progress") {
        0
    } else if name.eq_ignore_ascii_case("review") {
        1
    } else if name.eq_ignore_ascii_case("backlog") {
        2
    } else {
        3
    }
}

/// One to-do: a checkbox that closes it, its title (which opens the card
/// panel), and the lane, claim and latest agent note beneath it.
fn todo_row(
    project_id: i64,
    lane_name: &str,
    card: &board::WorkCard,
    note: Option<&str>,
) -> gtk::Widget {
    let done = lane_name.eq_ignore_ascii_case("done");
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("todo-row");
    row.set_hexpand(true);

    let tick = gtk::Button::new();
    tick.add_css_class("flat");
    tick.add_css_class("todo-tick");
    tick.set_tooltip_text(Some(if done {
        "Reopen this to-do"
    } else {
        "Close this to-do"
    }));
    tick.set_action_name(Some("win.card-toggle-done"));
    tick.set_action_target_value(Some(&(project_id, card.id.as_str()).to_variant()));
    tick.set_valign(gtk::Align::Start);
    let mark = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    mark.add_css_class("todo-box");
    mark.set_size_request(13, 13);
    if done {
        mark.add_css_class("done");
    }
    tick.set_child(Some(&mark));
    row.append(&tick);

    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("todo");
    button.set_halign(gtk::Align::Fill);
    button.set_hexpand(true);
    button.set_tooltip_text(Some("Open this card — read it, reply, edit it"));
    button.set_action_name(Some("win.open-card"));
    button.set_action_target_value(Some(&(project_id, card.id.as_str()).to_variant()));

    let texts = gtk::Box::new(gtk::Orientation::Vertical, 0);
    texts.set_hexpand(true);
    let title = gtk::Label::new(Some(&card.title));
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    if done {
        title.add_css_class("todo-done");
    }
    texts.append(&title);

    let meta = gtk::Box::new(gtk::Orientation::Horizontal, 7);
    let lane_label = gtk::Label::new(Some(lane_name));
    lane_label.add_css_class("caption");
    lane_label.add_css_class("dim-label");
    meta.append(&lane_label);
    if let Some(claim) = &card.claim {
        let claim_label = gtk::Label::new(Some(claim));
        claim_label.add_css_class("caption");
        claim_label.add_css_class("todo-claim");
        claim_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        meta.append(&claim_label);
    }
    texts.append(&meta);
    if let Some(note) = note {
        let note_label = gtk::Label::new(Some(note));
        note_label.set_xalign(0.0);
        note_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        note_label.add_css_class("caption");
        note_label.add_css_class("todo-note");
        texts.append(&note_label);
    }
    button.set_child(Some(&texts));
    row.append(&button);
    row.upcast()
}

/// The latest agent note on a card — the "what was done" a human gets to see
/// without opening the terminal.
fn latest_card_note(app: &App, project_id: i64, card_id: &str) -> Option<String> {
    let activity = app.activity.borrow();
    let snapshot = &activity.get(&project_id)?.snapshot;
    snapshot.events.iter().rev().find_map(|event| {
        if event.card_id.as_deref() != Some(card_id) || event.session_id.is_none() {
            return None;
        }
        match &event.payload {
            crate::session::activity::ActivityPayload::Message { text } => Some(shorten(text, 96)),
            _ => None,
        }
    })
}

fn shorten(text: &str, limit: usize) -> String {
    let text = text.lines().next().unwrap_or("").trim();
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut short: String = text.chars().take(limit.saturating_sub(1)).collect();
    short.push('…');
    short
}

fn lane_footer(project: &Project, running: usize) -> gtk::Widget {
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    footer.add_css_class("lane-foot");
    let open_board = gtk::Button::with_label("Open board");
    open_board.add_css_class("flat");
    open_board.set_tooltip_text(Some("Open this project's board in Home"));
    open_board.set_action_name(Some("win.home-board"));
    open_board.set_action_target_value(Some(&project.id.to_variant()));
    footer.append(&open_board);
    let status = gtk::Label::new(Some(&match running {
        0 => "no agents".to_string(),
        1 => "1 running".to_string(),
        count => format!("{count} running"),
    }));
    status.add_css_class("caption");
    status.add_css_class("dim-label");
    status.set_xalign(1.0);
    status.set_hexpand(true);
    footer.append(&status);
    footer.upcast()
}

fn section_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("lane-section");
    label.set_xalign(0.0);
    label
}

fn quiet(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("caption");
    label.add_css_class("dim-label");
    label.set_xalign(0.0);
    label
}

fn session_row(app: &App, project_id: i64, session: &AgentSession) -> gtk::Widget {
    let active = app.active_panel_session_id(project_id).as_deref() == Some(session.id.as_str());
    let state = app.latest_agent_activity(project_id, session);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("cockpit-row");
    row.set_margin_start(12);

    let dot = gtk::Label::new(Some("●"));
    dot.add_css_class("agent-state-dot");
    dot.add_css_class(match state {
        Some((state, _, _)) => state_class(state),
        None => "agent-state-unknown",
    });
    dot.set_valign(gtk::Align::Center);
    row.append(&dot);

    let texts = gtk::Box::new(gtk::Orientation::Vertical, 0);
    texts.set_hexpand(true);
    let title = gtk::Label::new(Some(&session.title));
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    if active {
        title.add_css_class("cockpit-active");
    }
    texts.append(&title);

    let program_name = programs::by_id(&session.program_id)
        .map(|program| program.name)
        .unwrap_or_else(|| session.program_id.clone());
    let lifecycle = if session.external.is_some() {
        "external terminal"
    } else {
        "running"
    };
    let detail = match &state {
        Some((state, at_millis, message)) => {
            let text = message
                .as_ref()
                .map(|message| format!("{} · {message}", board::agent_state_label(*state)))
                .unwrap_or_else(|| board::agent_state_label(*state).to_string());
            format!(
                "{program_name} · {lifecycle} · {} · {text}",
                board::relative_age(*at_millis)
            )
        }
        None => format!("{program_name} · {lifecycle}"),
    };
    let detail = gtk::Label::new(Some(&detail));
    detail.add_css_class("caption");
    detail.add_css_class("dim-label");
    detail.set_xalign(0.0);
    detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
    texts.append(&detail);
    row.append(&texts);

    let can_open = live_agents::can_open_sidebar_session(
        session,
        programs::by_id(&session.program_id)
            .is_some_and(|program| !program.resume_session.is_empty()),
    );
    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("session-item");
    button.set_halign(gtk::Align::Fill);
    button.set_hexpand(true);
    button.set_sensitive(can_open);
    button.set_tooltip_text(Some(if can_open {
        if session.external.is_some() {
            "Focus this terminal"
        } else {
            "Open this session"
        }
    } else {
        "History only; no supported reopen link"
    }));
    if can_open {
        button.set_action_name(Some("win.project-session-open"));
        button.set_action_target_value(Some(&(project_id, session.id.as_str()).to_variant()));
    }
    button.set_child(Some(&row));
    button.upcast()
}

fn state_class(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => "agent-state-working",
        AgentState::WaitingForInput | AgentState::WaitingForApproval => "agent-state-waiting",
        AgentState::Idle => "agent-state-idle",
        AgentState::Unknown => "agent-state-unknown",
    }
}

// ---- Shared bits ----

fn append_heading(content: &gtk::Box, text: &str, trailing: Option<&str>) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("cockpit-heading");
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("caption-heading");
    row.append(&label);
    if let Some(trailing) = trailing {
        let right = gtk::Label::new(Some(trailing));
        right.add_css_class("caption");
        right.add_css_class("dim-label");
        right.set_xalign(1.0);
        right.set_hexpand(true);
        row.append(&right);
    }
    content.append(&row);
}

/// A quiet setup affordance at the foot of the cockpit: the everyday Home is
/// the work, but the first-run tools stay one click away.
fn footer(app: &App, content: &gtk::Box) {
    let _ = app;
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.set_margin_top(8);
    let hint = gtk::Label::new(Some(
        "Workspace setup, programs and layout live in Preferences",
    ));
    hint.add_css_class("caption");
    hint.add_css_class("dim-label");
    hint.set_xalign(0.0);
    hint.set_hexpand(true);
    row.append(&hint);
    let prefs = gtk::Button::with_label("Preferences");
    prefs.add_css_class("flat");
    prefs.set_action_name(Some("win.preferences"));
    row.append(&prefs);
    content.append(&row);
}

fn project_name(app: &App, project_id: i64) -> String {
    app.projects
        .borrow()
        .iter()
        .find(|project| project.id == project_id)
        .map(|project| project.name.clone())
        .unwrap_or_else(|| format!("project {project_id}"))
}

// ---- Empty state ----

/// The empty state with something to do: the program each slot uses, the
/// layout a new project opens with, and the two ways forward — the sidebar's
/// search for directories that already exist, and **New project…** for a
/// fresh folder with a `git init` in it.
fn empty_panel(app: &SharedApp) -> gtk::Widget {
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 20);
    content.add_css_class("home");
    content.set_halign(gtk::Align::Center);
    content.set_valign(gtk::Align::Center);
    // No width_request: a hard floor here would also floor the whole content
    // pane — the splitter stops at the pane's minimum, so every pixel this
    // card demands is a pixel the handle can no longer give the sidebar.
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

fn git_init(path: &Path) -> bool {
    std::process::Command::new("git")
        .arg("init")
        .current_dir(path)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}
