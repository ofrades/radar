//! Home: radar's cockpit.
//!
//! Home is where the whole workspace is understood without diving into any
//! terminal. It gathers the workspace into:
//!
//!   * **Needs you** — every unresolved request for human attention, with the
//!     actions to answer, approve, deny, or dismiss it in place.
//!   * **Agents** — every agent running across every project in one list,
//!     grouped by project, each row carrying its activity sign and a way to
//!     open the session. A large destination card on Home is the way in.
//!   * **Projects** — a small, clickable card per project: its git state, its
//!     board counts and its to-dos, each carrying the state of the session bound
//!     to it. The card opens the project's own view, where its to-dos are
//!     grouped Todo / In progress / Review and a new one can be typed; a to-do
//!     there opens its conversation.
//!
//! Home is the project navigator. Workspace tools live in a local toolbar,
//! not a permanent sidebar. When there are no projects yet, Home falls back to the
//! empty-state setup card — the same panel that gets a first project going.
//!
//! The cockpit is rebuilt whenever attention, sessions, git, or the layout
//! move (see `App::refresh_home`); it holds no state of its own.

use adw::prelude::*;
use std::path::Path;

use super::board;
use super::live_agents;
use super::{App, SharedApp};
use crate::db::Project;
use crate::session::activity::{
    Attention, AttentionActionKind, AttentionChange, AttentionResponse,
};

/// The Home panel. The cockpit when there is work to show; the setup/empty
/// state when there is not.
pub fn panel(app: &SharedApp) -> gtk::Widget {
    if app.projects.borrow().is_empty() {
        empty_panel()
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

    // Home is a destination, not a toolbar: title and context first, then
    // large cards that lead into the workspace.
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    header.add_css_class("home-hero");
    let heading = gtk::Box::new(gtk::Orientation::Vertical, 8);
    heading.set_hexpand(true);
    let title = gtk::Label::new(Some("Home"));
    title.add_css_class("hero-title");
    title.set_xalign(0.0);
    heading.append(&title);
    let projects_label = match projects.len() {
        1 => "1 project".to_string(),
        count => format!("{count} projects"),
    };
    let counts = gtk::Label::new(Some(&format!(
        "{projects_label} · {} need you",
        needs.len()
    )));
    counts.add_css_class("caption");
    counts.add_css_class("dim-label");
    counts.set_hexpand(true);
    counts.set_xalign(0.0);
    counts.set_wrap(true);
    heading.append(&counts);
    header.append(&heading);
    let shortcuts = gtk::Button::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("Tools and shortcuts · Alt+H")
        .action_name("win.hud")
        .build();
    shortcuts.set_valign(gtk::Align::Center);
    header.append(&shortcuts);
    content.append(&header);

    content.append(&agents_destination(running));
    needs_you_section(app, &content, &needs);
    projects_section(app, &content, &projects);

    scroll.set_child(Some(&content));
    scroll.set_hexpand(true);
    scroll.set_vexpand(true);
    scroll.upcast()
}

/// The current Home view: the cockpit, a project's own view, or a card
/// conversation — the Basecamp drill-down, with a way back. The Agents page
/// is not here: it is a persistent stack page of its own (live panes cannot
/// survive Home's rebuild-on-every-event).
pub fn view(app: &App) -> gtk::Widget {
    match app.home_nav.borrow().last().cloned() {
        Some(super::HomeView::Project(project_id)) => project_view(app, project_id),
        Some(super::HomeView::Card(project_id, card_id)) => card_view(app, project_id, &card_id),
        Some(super::HomeView::AddProject | super::HomeView::Agents) => cockpit(app),
        None => cockpit(app),
    }
}

/// A card conversation, opened inside Home.
fn card_view(app: &App, project_id: i64, card_id: &str) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("home-view");
    root.add_css_class("card-view");
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

/// A readable way back, followed by the destination's hero title.
fn view_header(title: &str, meta: Option<&str>) -> gtk::Widget {
    let meta = meta.map(|text| gtk::Label::new(Some(text)));
    page_header(title, meta.as_ref()).upcast()
}

/// Shared by persistent pages too: their metadata can update without
/// rebuilding the header or any live terminal beneath it.
pub(super) fn page_header(title: &str, meta: Option<&gtk::Label>) -> gtk::Box {
    let bar = gtk::Box::new(gtk::Orientation::Vertical, 10);
    bar.add_css_class("home-view-bar");
    let back = gtk::Button::builder()
        .label("← Back")
        .tooltip_text("Back to the previous view")
        .build();
    back.add_css_class("flat");
    back.set_halign(gtk::Align::Start);
    back.set_action_name(Some("win.home-back"));
    bar.append(&back);
    let name = gtk::Label::new(Some(title));
    name.set_xalign(0.0);
    name.add_css_class("hero-title");
    name.set_wrap(true);
    name.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    bar.append(&name);
    if let Some(meta) = meta {
        meta.add_css_class("caption");
        meta.add_css_class("dim-label");
        meta.set_xalign(0.0);
        meta.set_wrap(true);
        meta.set_hexpand(true);
        bar.append(meta);
    }
    bar
}

// ---- Agents: the page itself lives in agents.rs ----

/// A first-class Home destination, using the same big-card language as Projects.
fn agents_destination(running: usize) -> gtk::Widget {
    let button = gtk::Button::new();
    button.add_css_class("destination-card");
    button.set_action_name(Some("win.home-agents"));
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    let icon = gtk::Image::from_icon_name("utilities-terminal-symbolic");
    icon.set_pixel_size(32);
    row.append(&icon);
    let copy = gtk::Box::new(gtk::Orientation::Vertical, 8);
    copy.set_hexpand(true);
    let title = gtk::Label::new(Some("Agents"));
    title.add_css_class("destination-title");
    title.set_xalign(0.0);
    copy.append(&title);
    let summary = gtk::Label::new(Some(&format!(
        "{} · All projects, one workspace",
        agents_count_text(running)
    )));
    summary.set_xalign(0.0);
    summary.set_wrap(true);
    summary.add_css_class("dim-label");
    copy.append(&summary);
    let hint = gtk::Label::new(Some("See agent panels and work with them in place"));
    hint.set_xalign(0.0);
    hint.set_wrap(true);
    hint.add_css_class("caption");
    hint.add_css_class("dim-label");
    copy.append(&hint);
    row.append(&copy);
    row.append(&gtk::Image::from_icon_name("go-next-symbolic"));
    button.set_child(Some(&row));
    button.upcast()
}

/// Live count shared by Home's destination card and the Agents page.
pub(super) fn agents_count_text(running: usize) -> String {
    match running {
        0 => "No agents".to_string(),
        1 => "1 running".to_string(),
        count => format!("{count} running"),
    }
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
        open.set_action_name(Some("win.open-card"));
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

// ---- Projects: one lane per project, with its to-dos ----

fn projects_section(app: &App, content: &gtk::Box, projects: &[Project]) {
    let to_dos = open_todo_count(app);
    let trailing = match to_dos {
        1 => "1 to-do".to_string(),
        count => format!("{count} to-dos"),
    };
    append_heading(content, "Projects", Some(&trailing));

    // The way a project is born leads the section: a big plus card, above the
    // project cards, that opens Home's Add-a-project picker.
    content.append(&add_project_card(app));

    let grid = gtk::FlowBox::new();
    grid.set_selection_mode(gtk::SelectionMode::None);
    // At most three columns, but allow a narrow window to wrap to one.
    let per_line = projects.len().clamp(1, 3) as u32;
    grid.set_min_children_per_line(1);
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

/// The way a project is born, leading the Projects section: one big dashed
/// card that opens Home's combined Add-a-project picker — add a folder you
/// already have, or type a name to create one.
fn add_project_card(app: &App) -> gtk::Widget {
    let _ = app;
    let button = gtk::Button::new();
    button.add_css_class("add-project-card");
    button.set_halign(gtk::Align::Fill);
    button.set_hexpand(true);
    button.set_tooltip_text(Some("Create a new folder, or add one you already have"));
    button.set_action_name(Some("win.home-add-project"));

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let plus = gtk::Image::from_icon_name("list-add-symbolic");
    plus.set_pixel_size(24);
    plus.set_valign(gtk::Align::Center);
    row.append(&plus);

    let texts = gtk::Box::new(gtk::Orientation::Vertical, 0);
    texts.set_hexpand(true);
    texts.set_valign(gtk::Align::Center);
    let title = gtk::Label::new(Some("Add a project"));
    title.add_css_class("heading");
    title.set_xalign(0.0);
    texts.append(&title);
    let sub = gtk::Label::new(Some("Create a new folder, or find one you already have"));
    sub.add_css_class("caption");
    sub.add_css_class("dim-label");
    sub.set_xalign(0.0);
    texts.append(&sub);
    row.append(&texts);

    button.set_child(Some(&row));
    button.upcast()
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

/// One project's lane: its header, its board chips, a way to add a to-do, its
/// to-dos (each with the state of the session bound to it), and a footer pulse.
/// The name opens the workspace; **Open project** drills into the project's own
/// view; a to-do opens its card.
fn project_lane(app: &App, project: &Project) -> gtk::Widget {
    let lane = gtk::Box::new(gtk::Orientation::Vertical, 8);
    lane.add_css_class("lane");
    lane.set_valign(gtk::Align::Start);
    lane.set_hexpand(true);

    // The whole card is the way in: a click anywhere a control inside does not
    // claim opens the project view. Buttons and the input claim their clicks,
    // which denies this gesture, so the inline controls keep working.
    let lane_widget: gtk::Widget = lane.clone().upcast();
    let project_id = project.id;
    let gesture = gtk::GestureClick::new();
    gesture.set_button(gtk::gdk::BUTTON_PRIMARY);
    let lane_for_click = lane_widget.clone();
    gesture.connect_released(move |_, _, x, y| {
        // Whatever the click landed on that takes input wins; only the card's
        // own chrome (labels, gaps, background) drills in.
        if let Some(target) = lane_for_click.pick(x, y, gtk::PickFlags::DEFAULT) {
            if takes_input(&target, &lane_for_click) {
                return;
            }
        }
        let _ = gtk::prelude::WidgetExt::activate_action(
            &lane_for_click,
            "win.home-project",
            Some(&project_id.to_variant()),
        );
    });
    lane.add_controller(gesture);
    lane.set_cursor_from_name(Some("pointer"));

    lane.append(&lane_header(app, project));

    let summaries = app.board_summaries.borrow();
    let summary = summaries.get(&project.id);

    // Board counts as pills: what each lane holds, at a glance.
    let pills = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    pills.add_css_class("lane-pills");
    let mut any_pill = false;
    if let Some(summary) = summary {
        for lane_summary in &summary.lanes {
            // Done is not a status we keep in view; it leaves the board.
            if lane_summary.done || lane_summary.cards.is_empty() {
                continue;
            }
            any_pill = true;
            pills.append(&lane_pill(lane_summary));
        }
    }
    if !any_pill {
        let pill = gtk::Label::new(Some("No board"));
        pill.add_css_class("pill");
        pills.append(&pill);
    }
    lane.append(&pills);

    // The human's way in sits right under the chips, above the list it feeds.
    lane.append(&todo_add_entry(app, project.id));

    // Done cards leave the lane: they are celebrated and gone, not listed.
    lane.append(&section_label("To-dos"));
    if let Some(summary) = summary {
        let (open, _done) = open_and_done(summary);
        if open.is_empty() {
            lane.append(&quiet("No to-dos yet"));
        } else {
            lane.append(&todo_scroll(app, project.id, &open));
        }
    } else {
        lane.append(&quiet("No board"));
    }

    lane.append(&lane_footer(
        live_session_count(app, project.id),
        app.project_recent_exits(project.id),
    ));
    lane.upcast()
}

/// Whether a click that landed on `widget` should be handled by a control
/// inside the card rather than by the card's own drill-in.
fn takes_input(widget: &gtk::Widget, stop: &gtk::Widget) -> bool {
    let mut current = Some(widget.clone());
    while let Some(w) = current {
        if w.is::<gtk::Button>()
            || w.is::<gtk::MenuButton>()
            || w.is::<gtk::Entry>()
            || w.is::<gtk::DropDown>()
            || w.is::<gtk::TextView>()
            || w.is::<gtk::Range>()
        {
            return true;
        }
        if &w == stop {
            break;
        }
        current = w.parent();
    }
    false
}

fn lane_pill(lane: &board::LaneSummary) -> gtk::Label {
    let pill = gtk::Label::new(Some(&format!("{} {}", lane.name, lane.cards.len())));
    pill.add_css_class("pill");
    if lane.done {
        pill.add_css_class("pill-done");
    } else if lane.name.eq_ignore_ascii_case("in progress") {
        pill.add_css_class("pill-active");
    } else if lane.name.eq_ignore_ascii_case("review") {
        pill.add_css_class("pill-review");
    }
    pill
}

fn lane_header(app: &App, project: &Project) -> gtk::Widget {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);

    header.append(&sign_dot(app.project_activity_sign(project.id)));

    let name = gtk::Button::with_label(&project.name);
    name.add_css_class("flat");
    name.add_css_class("lane-name");
    name.set_tooltip_text(Some("Open this project's tasks and conversations"));
    name.set_action_name(Some("win.home-project"));
    name.set_action_target_value(Some(&project.id.to_variant()));
    header.append(&name);
    if project.pinned {
        let pin = gtk::Image::from_icon_name("starred-symbolic");
        pin.set_tooltip_text(Some("Pinned project"));
        pin.set_pixel_size(12);
        header.append(&pin);
    }

    let git = app
        .status
        .borrow()
        .get(&project.id)
        .map(|status| status.summary())
        .unwrap_or_else(|| "…".to_string());
    let git = gtk::Label::new(Some(if project.is_missing() {
        "Folder missing"
    } else {
        &git
    }));
    git.add_css_class("caption");
    git.add_css_class("dim-label");
    git.set_xalign(1.0);
    git.set_hexpand(true);
    git.set_ellipsize(gtk::pango::EllipsizeMode::Start);
    header.append(&git);

    header.append(&project_menu(project));
    header.upcast()
}

pub(super) fn project_menu(project: &Project) -> gtk::MenuButton {
    // One menu item bound to this project's `win.` action. Sections draw the
    // separators, so the destructive archive sits apart from the arrangement
    // controls and the name/settings pair. GTK4 popovers ignore an item's
    // `icon` unless the item is icon-only, so the labels carry the meaning.
    let item = |label: &str, action: &str| {
        let entry = gtk::gio::MenuItem::new(Some(label), None);
        entry.set_action_and_target_value(Some(action), Some(&project.id.to_variant()));
        entry
    };

    let menu = gtk::gio::Menu::new();
    let settings = gtk::gio::Menu::new();
    settings.append_item(&item("Edit project name…", "win.home-project-edit"));
    settings.append_item(&item("Project defaults…", "win.home-project-defaults"));
    menu.append_section(None, &settings);

    let arrange = gtk::gio::Menu::new();
    arrange.append_item(&item(
        if project.pinned {
            "Unpin project"
        } else {
            "Pin project"
        },
        "win.home-project-pin",
    ));
    arrange.append_item(&item("Move earlier", "win.home-project-up"));
    arrange.append_item(&item("Move later", "win.home-project-down"));
    menu.append_section(None, &arrange);

    let danger = gtk::gio::Menu::new();
    danger.append_item(&item("Archive project…", "win.home-project-archive"));
    menu.append_section(None, &danger);

    gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("Manage project")
        .menu_model(&menu)
        .build()
}

/// Split a board's lanes into open to-dos (in-progress, review, then the
/// rest) and the Done lane's cards.
pub(super) fn open_and_done(
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
    } else if name.eq_ignore_ascii_case("todo") || name.eq_ignore_ascii_case("backlog") {
        2
    } else {
        3
    }
}

/// The open to-dos, in a scroller the card never cuts short: no height cap, so
/// the lane grows with the work it holds and Home's own scroller carries the
/// page. The scroller stays for the width it does *not* claim: a never
/// horizontal policy contributes the list's minimum width, not its natural
/// width, so a long to-do title never widens the card.
fn todo_scroll(app: &App, project_id: i64, open: &[(String, board::WorkCard)]) -> gtk::Widget {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    list.add_css_class("todo-list");
    for (lane_name, card) in open {
        let note = latest_card_note(app, project_id, &card.id);
        list.append(&todo_row(app, project_id, lane_name, card, note.as_deref()));
    }
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_height(true)
        .child(&list)
        .build();
    scroll.add_css_class("todo-scroll");
    scroll.upcast()
}

fn lane_footer(running: usize, stopped: usize) -> gtk::Widget {
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    footer.add_css_class("lane-foot");
    let status = gtk::Label::new(Some(&lane_footer_text(running, stopped)));
    status.add_css_class("caption");
    status.add_css_class("dim-label");
    status.set_xalign(1.0);
    status.set_hexpand(true);
    footer.append(&status);
    footer.upcast()
}

/// The lane's one-line pulse: how many agents are running, and how many
/// sessions ended recently (the "stopped" activity that would otherwise be
/// invisible).
fn lane_footer_text(running: usize, stopped: usize) -> String {
    let running = match running {
        0 => "no agents".to_string(),
        1 => "1 running".to_string(),
        count => format!("{count} running"),
    };
    match stopped {
        0 => running,
        1 => format!("{running} · 1 stopped"),
        count => format!("{running} · {count} stopped"),
    }
}

fn section_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("lane-section");
    label.set_xalign(0.0);
    label
}

/// A project, opened inside Home: its board grouped by lane — Todo, In
/// progress, Review — with a way to add one, then its running sessions.
fn project_view(app: &App, project_id: i64) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("home-view");
    let project = app
        .projects
        .borrow()
        .iter()
        .find(|project| project.id == project_id)
        .cloned();
    let Some(project) = project else {
        root.append(&view_header("Project", None));
        root.append(&quiet("That project is no longer available"));
        return root.upcast();
    };
    let git = app
        .status
        .borrow()
        .get(&project_id)
        .map(|status| status.summary());
    root.append(&view_header(&project.name, git.as_deref()));

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.add_css_class("home-cockpit");
    content.add_css_class("project-view");
    content.set_vexpand(true);
    content.set_margin_top(16);
    content.set_margin_bottom(20);
    content.set_margin_start(22);
    content.set_margin_end(22);
    content.set_halign(gtk::Align::Fill);
    content.set_hexpand(true);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let open_workspace = gtk::Button::with_label("Open workspace");
    open_workspace.add_css_class("flat");
    open_workspace.set_tooltip_text(Some(
        "Open agent and developer tools; Home keeps running behind them",
    ));
    open_workspace.set_action_name(Some("win.open-project"));
    open_workspace.set_action_target_value(Some(&project_id.to_variant()));
    actions.append(&open_workspace);
    open_workspace.set_sensitive(!project.is_missing());
    actions.append(&project_menu(&project));
    content.append(&actions);
    let path = gtk::Label::new(Some(&project.subtitle()));
    path.set_xalign(0.0);
    path.set_selectable(true);
    path.add_css_class("caption");
    path.add_css_class("dim-label");
    content.append(&path);

    content.append(&todo_add_entry(app, project_id));

    // The board as columns, side by side: Todo, In progress, Review, Done —
    // each scrolls on its own. Sessions are not a column: each card carries
    // its own session chip instead.
    let columns = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    columns.add_css_class("project-columns");
    columns.set_vexpand(true);
    columns.set_valign(gtk::Align::Fill);
    columns.set_hexpand(true);
    {
        let summaries = app.board_summaries.borrow();
        match summaries.get(&project_id) {
            Some(summary) => {
                // Done is gone the moment it is done; only live lanes show.
                for lane in summary.lanes.iter().filter(|lane| !lane.done) {
                    columns.append(&lane_column(app, project_id, lane));
                }
            }
            None => content.append(&quiet("No board")),
        }
    }
    let hscroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&columns)
        .build();
    hscroll.add_css_class("project-columns-scroll");
    content.append(&hscroll);

    root.append(&content);
    root.upcast()
}

/// One lane as a column: a heading with its count, then its cards in a scroller.
fn lane_column(app: &App, project_id: i64, lane: &board::LaneSummary) -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
    column.add_css_class("project-column");
    column.set_width_request(270);
    column.set_valign(gtk::Align::Fill);
    column.set_vexpand(true);

    let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let name = gtk::Label::new(Some(&lane.name));
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.add_css_class("heading");
    head.append(&name);
    let count = gtk::Label::new(Some(&lane.cards.len().to_string()));
    count.add_css_class("caption");
    count.add_css_class("dim-label");
    head.append(&count);
    column.append(&head);

    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    for card in &lane.cards {
        let note = latest_card_note(app, project_id, &card.id);
        let label = if lane.done { "Done" } else { "" };
        list.append(&todo_row(app, project_id, label, card, note.as_deref()));
    }
    if lane.cards.is_empty() {
        list.append(&quiet("Nothing here"));
    }
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(&list)
        .build();
    column.append(&scroll);
    column.upcast()
}

/// How many of a project's sessions are live, for the lane's footer pulse.
fn live_session_count(app: &App, project_id: i64) -> usize {
    app.agent_sessions
        .borrow()
        .by_project
        .get(&project_id)
        .map(|sessions| {
            sessions
                .iter()
                .filter(|session| live_agents::sidebar_session_is_live(session))
                .count()
        })
        .unwrap_or(0)
}

/// One to-do: a checkbox that closes it, its title (which opens the card
/// panel), the lane, claim and latest agent note beneath it, and the session
/// bound to it on the right.
fn todo_row(
    app: &App,
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
    button.set_tooltip_text(Some(&format!(
        "Open this card — read it, reply, edit it\n{}",
        card.id
    )));
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
    // A grouped list already names the lane in its heading; only a lone row
    // (the Done list) carries its own.
    if !lane_name.is_empty() {
        let lane_label = gtk::Label::new(Some(lane_name));
        lane_label.add_css_class("caption");
        lane_label.add_css_class("dim-label");
        meta.append(&lane_label);
    }
    if let Some(claim) = &card.claim {
        let claim_label = gtk::Label::new(Some(claim));
        claim_label.add_css_class("caption");
        claim_label.add_css_class("todo-claim");
        claim_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        meta.append(&claim_label);
    }
    if meta.first_child().is_some() {
        texts.append(&meta);
    }
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
    if !done {
        row.append(&card_session_chip(app, project_id, card));
    }
    row.upcast()
}

/// A card's session at a glance: the agent bound to the card's claim, with
/// its state; a card whose session is gone — or that nobody holds — offers to
/// start one. Clicking opens the live session or resumes the claimed agent;
/// a fresh card starts the project's default agent attached to it (see
/// `win.card-session-create`).
fn card_session_chip(app: &App, project_id: i64, card: &board::WorkCard) -> gtk::Widget {
    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("card-session");
    button.set_valign(gtk::Align::Center);

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 5);
    let resumable = match &card.claim {
        Some(claim) => {
            app.live_claim_session(project_id, claim).is_some()
                || app.claim_has_exact_session(project_id, claim)
        }
        None => false,
    };
    if !resumable {
        // Nothing to open: an unclaimed card, or a claim whose conversation
        // is gone. A dead "Resume" button would lie; offer a new session.
        start_session_chip(&content, &button, project_id, &card.id);
    } else if let Some(claim) = &card.claim {
        let live = app.live_claim_session(project_id, claim);
        let sign = app.card_session_sign(project_id, card);
        let dot = gtk::Label::new(Some("●"));
        dot.add_css_class("agent-state-dot");
        dot.add_css_class(sign.css_class());
        dot.set_valign(gtk::Align::Center);
        content.append(&dot);
        let label = gtk::Label::new(Some(match sign {
            super::activity_sign::Sign::Unknown => "Session",
            _ => sign.label(),
        }));
        label.add_css_class("caption");
        label.add_css_class("dim-label");
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        content.append(&label);
        button.set_tooltip_text(Some(if live.is_some() {
            "Open this card's session"
        } else {
            "Resume the agent on this card"
        }));
        button.set_action_name(Some("win.open-claim"));
        button.set_action_target_value(Some(&(project_id, claim.as_str()).to_variant()));
    }
    button.set_child(Some(&content));
    button.upcast()
}

/// The "+ Session" affordance: clicking starts the project's default agent
/// attached to the card.
fn start_session_chip(content: &gtk::Box, button: &gtk::Button, project_id: i64, card_id: &str) {
    let plus = gtk::Image::from_icon_name("list-add-symbolic");
    plus.set_pixel_size(11);
    plus.set_valign(gtk::Align::Center);
    content.append(&plus);
    let label = gtk::Label::new(Some("Session"));
    label.add_css_class("caption");
    label.add_css_class("dim-label");
    content.append(&label);
    button.set_tooltip_text(Some("Start an agent session on this card"));
    button.set_action_name(Some("win.card-session-create"));
    button.set_action_target_value(Some(&(project_id, card_id).to_variant()));
}

/// The human's way in: an underlined input just under a project's board chips.
/// Type a title and press Enter to add the card; the store puts it in the
/// default lane (Todo). Empty input is ignored.
fn todo_add_entry(app: &App, project_id: i64) -> gtk::Entry {
    let entry = gtk::Entry::builder()
        .has_frame(false)
        .placeholder_text("Add a to-do…")
        .build();
    entry.add_css_class("todo-add");
    entry.set_hexpand(true);
    entry.set_margin_top(2);
    if let Some(draft) = app.home_todo_drafts.borrow().get(&project_id) {
        entry.set_text(draft);
    }
    let drafts = app.home_todo_drafts.clone();
    entry.connect_changed(move |entry| {
        drafts
            .borrow_mut()
            .insert(project_id, entry.text().to_string());
    });

    let window = app.window.clone();
    entry.connect_activate(move |entry| {
        let title = entry.text().trim().to_string();
        if title.is_empty() {
            return;
        }
        let _ = gtk::prelude::WidgetExt::activate_action(
            &window,
            "win.home-add-todo",
            Some(&(project_id, title).to_variant()),
        );
    });

    // The rebuild after a submit destroyed the old input; hand the keys back.
    if app.home_focus_todo.get() == Some(project_id) {
        app.home_focus_todo.set(None);
        let entry = entry.clone();
        gtk::glib::idle_add_local_once(move || {
            entry.grab_focus();
        });
    }
    entry
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

pub(super) fn quiet(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("caption");
    label.add_css_class("dim-label");
    label.set_xalign(0.0);
    label
}

/// A small colored status dot for an activity sign, with a pulse while work is
/// in flight or something is waiting.
fn sign_dot(sign: super::activity_sign::Sign) -> gtk::Widget {
    let dot = gtk::Label::new(Some("●"));
    dot.set_valign(gtk::Align::Center);
    dot.set_tooltip_text(Some(sign.label()));
    if sign.animates() {
        dot.set_css_classes(&["activity-dot", "activity-pulse", sign.css_class()]);
    } else {
        dot.set_css_classes(&["activity-dot", sign.css_class()]);
    }
    dot.upcast()
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

fn project_name(app: &App, project_id: i64) -> String {
    app.projects
        .borrow()
        .iter()
        .find(|project| project.id == project_id)
        .map(|project| project.name.clone())
        .unwrap_or_else(|| format!("project {project_id}"))
}

// ---- Empty state ----

/// A project-first empty state: create new work or find an existing folder.
/// Tool programs and layout remain in Preferences.
fn empty_panel() -> gtk::Widget {
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
        "Your projects, tasks and conversations — see what needs you, without opening a terminal.",
    ));
    tagline.add_css_class("caption");
    tagline.add_css_class("dim-label");
    tagline.set_wrap(true);
    tagline.set_justify(gtk::Justification::Center);
    tagline.set_max_width_chars(46);
    brand.append(&tagline);
    content.append(&brand);

    // One way forward: the Add-a-project picker covers new and existing.
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    actions.set_halign(gtk::Align::Center);
    let add = gtk::Button::with_label("Add a project…");
    add.add_css_class("suggested-action");
    add.add_css_class("pill");
    add.set_tooltip_text(Some("Create a new folder, or add one you already have"));
    add.set_action_name(Some("win.home-add-project"));
    actions.append(&add);
    content.append(&actions);

    scroll.set_child(Some(&content));
    scroll.upcast()
}

/// Import an existing folder without changing files or initializing git.
pub(super) fn add_project_dialog(app: &SharedApp) {
    #[allow(deprecated)]
    let dialog = gtk::FileChooserDialog::new(
        Some("Add an existing project folder"),
        Some(&app.window),
        gtk::FileChooserAction::SelectFolder,
        &[
            ("Cancel", gtk::ResponseType::Cancel),
            ("Add project", gtk::ResponseType::Accept),
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
            let _ = gtk::prelude::WidgetExt::activate_action(
                &app.window,
                "win.home-project-import",
                Some(&path.to_string_lossy().to_string().to_variant()),
            );
        }
    });
    dialog.present();
}

/// Validate the New-project view's fields and create the project. A bare name
/// under an existing parent — the same rules the dialog enforced, now without
/// the dialog. Errors toast; success lands in the new project's Home view.
pub(super) fn create_project_from_fields(app: &SharedApp, name: &str, parent: &str) {
    let name = name.trim();
    let parent = parent.trim();
    if !valid_project_folder_name(name) || parent.is_empty() {
        app.toast("Enter a project name (not a path) and a parent folder");
        return;
    }
    let path = match crate::db::normalize_path(Path::new(parent)) {
        Ok(parent) => parent.join(name),
        Err(error) => {
            app.toast(&format!("Invalid parent folder: {error}"));
            return;
        }
    };
    if path.exists() {
        app.toast("That folder already exists. Use Add existing folder instead.");
        return;
    }
    create_project(app, path);
}

pub(super) fn valid_project_folder_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

/// Create and register a folder project without changing its Git ownership.
/// A project can contain several repositories or have no repository at all.
pub(super) fn create_project(app: &SharedApp, path: std::path::PathBuf) {
    if let Err(error) = std::fs::create_dir_all(&path) {
        app.toast(&format!(
            "Could not create {}: {error}",
            crate::db::abbreviate(&path)
        ));
        return;
    }
    let project = match app.db.add_project(&path) {
        Ok(project) => project,
        Err(error) => {
            app.toast(&format!("Could not add the project: {error}"));
            return;
        }
    };
    super::App::refresh_projects(app);
    app.show_home();
    app.home_nav
        .borrow_mut()
        .push(super::HomeView::Project(project.id));
    app.refresh_home();
    app.toast(&format!("{} · project folder added", project.name));
}

#[cfg(test)]
mod tests {
    use super::{
        agents_count_text, agents_destination, lane_footer_text, page_header,
        valid_project_folder_name,
    };
    use adw::prelude::*;

    #[test]
    fn new_project_names_cannot_escape_the_parent_folder() {
        for name in ["", ".", "..", "../other", "a/b", "/absolute", "null\0name"] {
            assert!(!valid_project_folder_name(name), "{name:?}");
        }
        for name in ["my-project", "Project with spaces", "café"] {
            assert!(valid_project_folder_name(name), "{name:?}");
        }
    }

    #[test]
    fn the_lane_footer_reports_running_and_stopped_agents() {
        assert_eq!(lane_footer_text(0, 0), "no agents");
        assert_eq!(lane_footer_text(1, 0), "1 running");
        assert_eq!(lane_footer_text(3, 1), "3 running · 1 stopped");
        assert_eq!(lane_footer_text(0, 2), "no agents · 2 stopped");
    }

    #[test]
    fn the_agents_count_reads_like_the_lanes_footer() {
        assert_eq!(agents_count_text(0), "No agents");
        assert_eq!(agents_count_text(1), "1 running");
        assert_eq!(agents_count_text(4), "4 running");
    }

    #[test]
    #[ignore = "requires a private D-Bus session and GTK display"]
    fn navigation_cards_and_headers_are_explicit_destinations() {
        gtk::init().unwrap();
        for running in [0, 3] {
            let card = agents_destination(running)
                .downcast::<gtk::Button>()
                .unwrap();
            assert_eq!(card.action_name().as_deref(), Some("win.home-agents"));
            assert!(card.has_css_class("destination-card"));
            let actions = gtk::gio::SimpleActionGroup::new();
            actions.add_action(&gtk::gio::SimpleAction::new("home-agents", None));
            card.insert_action_group("win", Some(&actions));
            assert!(card.is_sensitive());
        }
        let meta = gtk::Label::new(Some("3 running"));
        let header = page_header("Agents", Some(&meta));
        let back = header
            .first_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        assert_eq!(back.label().as_deref(), Some("← Back"));
        assert_eq!(back.action_name().as_deref(), Some("win.home-back"));
        let title = back
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        assert!(title.has_css_class("hero-title"));
        assert!(title.wraps());
        meta.set_text("4 running");
        assert_eq!(header.last_child().unwrap(), meta.upcast::<gtk::Widget>());
    }
}
