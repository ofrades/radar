//! The Agents page: every live session's real panel, in the workspace's own
//! arrangement.
//!
//! The wall mirrors the project workspaces' policy — only sessions whose
//! to-do is live work stand here (a card in Todo or Done leaves the wall,
//! program running). A panel drags by its header: drop on an edge to split
//! (side by side, or stacked bottom-up), drop in the middle to swap. The
//! arrangement is the same [`split::Node`] tree the workspaces use, with the
//! session's daemon id — unique across projects — as the drag payload.
//!
//! The page is a persistent stack page, NOT a Home drill-down: Home rebuilds
//! on every journal event, and that would tear attached terminals down
//! mid-keystroke. The board updates in place — panels come and go with their
//! sessions; a panel's pane is attached once and never rebuilt.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::gdk;
use gtk::glib;

use super::pane::Pane;
use super::split::{self, Node};
use super::theme::Theme;
use super::{live_agents, App};
use crate::db::Project;

/// One panel on the wall: a real pane attached to the daemon session, wearing
/// the workspace panel's own header — activity sign, session name, its to-do
/// with a done check, and a close that keeps the program running.
pub(super) struct AgentPanel {
    /// The panel as the arrangement tree sees it: the body with the drop-edge
    /// indicator floating over it.
    pub widget: gtk::Overlay,
    header: gtk::Box,
    edge: gtk::Box,
    /// The daemon session this panel attaches. Unique across projects — the
    /// drag payload and the identity everywhere.
    pub session_id: String,
    pub project: Project,
    pub pane: Rc<Pane>,
    dot: gtk::Label,
    session_label: gtk::Label,
    todo_button: gtk::Button,
    todo_label: gtk::Label,
    todo_done: gtk::Button,
    /// The process generation the attachment holds. A new pid under the same
    /// stable id (a stop and a respawn) means the attachment is watching a
    /// dead process — the panel re-attaches.
    pub pid: Option<u32>,
}

/// The wall's arrangement: the same pieces a workspace's layout uses.
pub(super) struct AgentsBoard {
    /// Holds exactly one child: the layout built from `panels`.
    pub holder: gtk::Box,
    pub panels: RefCell<Vec<Rc<AgentPanel>>>,
    /// The user's manual arrangement. None keeps the auto layout; the first
    /// edge-drop split plants it, and pruning keeps it honest.
    pub tree: RefCell<Option<Node<AgentPanel>>>,
    /// Divider positions the user dragged, keyed by the split node's key.
    pub positions: RefCell<HashMap<String, i32>>,
    /// Focusable dividers in the currently rendered layout.
    pub dividers: RefCell<Vec<gtk::Paned>>,
}

/// The persistent page: the header (Back, count) over the wall.
pub(super) struct AgentsPage {
    pub root: gtk::Widget,
    pub meta: gtk::Label,
    pub board: Rc<AgentsBoard>,
    conversations: gtk::Box,
}

pub(super) fn page() -> AgentsPage {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("home-view");
    root.add_css_class("agents-view");

    let meta = gtk::Label::new(None);
    let arrange = gtk::Button::with_label("Auto arrange");
    arrange.add_css_class("flat");
    arrange.set_tooltip_text(Some(
        "Fit all panels to the available space; reset manual arrangement",
    ));
    arrange.set_action_name(Some("win.agents-auto-arrange"));
    let header = super::home::page_header("Agents", Some(&meta), Some(arrange.upcast()));
    root.append(&header);

    // The wall: the workspace's arrangement, filling the page — panels side
    // by side, stacked, split and swapped by drag, dividers resizable.
    let board = Rc::new(AgentsBoard {
        holder: gtk::Box::new(gtk::Orientation::Vertical, 0),
        panels: RefCell::new(Vec::new()),
        tree: RefCell::new(None),
        positions: RefCell::new(HashMap::new()),
        dividers: RefCell::new(Vec::new()),
    });
    board.holder.set_vexpand(true);
    board.holder.set_hexpand(true);
    board.holder.append(&empty_state());
    root.append(&board.holder);
    let conversations = gtk::Box::new(gtk::Orientation::Vertical, 6);
    conversations.set_margin_start(18);
    conversations.set_margin_end(18);
    conversations.set_margin_bottom(18);
    root.append(&conversations);

    AgentsPage {
        root: root.upcast(),
        meta,
        board,
        conversations,
    }
}

/// A useful destination even before the first agent starts.
fn empty_state() -> gtk::Widget {
    let page = adw::StatusPage::builder()
        .icon_name("utilities-terminal-symbolic")
        .title("No agent panels yet")
        .description("Open a project and start a session from a to-do. Active work appears here; hidden panels keep their programs running.")
        .vexpand(true)
        .build();
    let projects = gtk::Button::with_label("Browse projects");
    projects.add_css_class("suggested-action");
    projects.set_halign(gtk::Align::Center);
    projects.set_action_name(Some("win.show-home"));
    page.set_child(Some(&projects));
    page.upcast()
}

/// The live sessions that belong to a project someone still has, in the
/// sidebar's project order, most recently active session first.
fn live_sessions_by_project(app: &App) -> Vec<(Project, Vec<live_agents::AgentSession>)> {
    let index = app.agent_sessions.borrow();
    app.projects
        .borrow()
        .iter()
        .filter_map(|project| {
            let mut rows: Vec<live_agents::AgentSession> = index
                .by_project
                .get(&project.id)?
                .iter()
                .filter(|session| live_agents::sidebar_session_is_live(session))
                .cloned()
                .collect();
            if rows.is_empty() {
                return None;
            }
            rows.sort_by_key(|session| std::cmp::Reverse(session.last_activity_at));
            Some((project.clone(), rows))
        })
        .collect()
}

/// The sessions the Agents wall stands, in sidebar project order: daemon-hosted
/// live work only — external terminals cannot be attached, and a session whose
/// to-do is parked in Todo/Done (or gone) leaves the wall. The single source of
/// truth for the wall's membership and Home's Agents count, so the card's
/// number always describes what entering shows.
pub(super) fn wall_sessions(app: &App) -> Vec<(Project, Vec<live_agents::AgentSession>)> {
    let dismissed = app.dismissed_agents.borrow();
    live_sessions_by_project(app)
        .into_iter()
        .map(|(project, rows)| {
            let rows: Vec<live_agents::AgentSession> = rows
                .into_iter()
                .filter(|session| {
                    session
                        .radar_session_id
                        .as_deref()
                        .is_some_and(|id| !dismissed.contains(id))
                        && wall_shows_session(app, project.id, session)
                })
                .collect();
            (project, rows)
        })
        .filter(|(_, rows)| !rows.is_empty())
        .collect()
}

/// The wall's policy, mirroring the project workspace's own: a session's
/// panel stands while its to-do is live work. A card in Todo or Done — or one
/// that left the board — keeps the session off the wall (never stopping it);
/// a session with no to-do is unjudged and shows.
fn wall_shows_session(app: &App, project_id: i64, session: &live_agents::AgentSession) -> bool {
    let Some(card_id) = app.session_card_id(project_id, session) else {
        return true;
    };
    let states = app.board_states.borrow();
    let Some(board) = states.get(&project_id) else {
        return true;
    };
    let Some(card) = board.cards.iter().find(|card| card.id == card_id) else {
        // The to-do is gone from the board: the session is a leftover.
        return false;
    };
    if card.done {
        return false;
    }
    match board.lanes.iter().find(|lane| lane.id == card.lane_id) {
        Some(lane) => !matches!(lane.kind.as_str(), "todo" | "done"),
        None => {
            !(card.lane.eq_ignore_ascii_case("todo")
                || card.lane.eq_ignore_ascii_case("backlog")
                || card.lane.eq_ignore_ascii_case("done"))
        }
    }
}

/// Bring the wall in line with discovery: panels appear for new live sessions
/// whose to-do is live work, leave for ended ones, Todo/Done cards, and
/// sessions the human closed. The panes are attached once and never rebuilt.
pub(super) fn sync(app: &App, page: &AgentsPage) {
    let board = &page.board;
    // The wall's membership: (session id -> project, session). One shared
    // predicate with Home's Agents count — the card's number is this list.
    let wanted: Vec<(Project, live_agents::AgentSession)> = wall_sessions(app)
        .into_iter()
        .flat_map(|(project, rows)| {
            rows.into_iter()
                .map(move |session| (project.clone(), session))
        })
        .collect();

    // Structural change? Panels come and go; the panes are never rebuilt.
    // A pid change under the same stable id is structural too: the old
    // attachment watches a dead process and must be replaced.
    let same = board.panels.borrow().len() == wanted.len()
        && board.panels.borrow().iter().all(|panel| {
            wanted.iter().any(|(_, session)| {
                session.radar_session_id.as_deref() == Some(&panel.session_id)
                    && session.pid == panel.pid
            })
        });
    if !same {
        // Drop the panels whose session is gone, closed, parked in Todo —
        // or respawned under the same id (a new pid). Take the tree out
        // once: take_leaf consumes nodes, so the drops fold through owned
        // reassignment, not through the borrow.
        let mut removed = false;
        let mut tree = board.tree.borrow_mut().take();
        let mut dropped: Vec<Rc<AgentPanel>> = Vec::new();
        board.panels.borrow_mut().retain(|panel| {
            let keep = wanted.iter().any(|(_, session)| {
                session.radar_session_id.as_deref() == Some(&panel.session_id)
                    && session.pid == panel.pid
            });
            if !keep {
                removed = true;
                dropped.push(panel.clone());
            }
            keep
        });
        for panel in &dropped {
            if let Some(node) = tree.take() {
                tree = node.take_leaf(panel);
            }
        }
        *board.tree.borrow_mut() = tree;
        // Add the new sessions' panels and hang them on the tree.
        for (project, session) in &wanted {
            let Some(id) = session.radar_session_id.clone() else {
                continue;
            };
            if board
                .panels
                .borrow()
                .iter()
                .any(|panel| panel.session_id == id && panel.pid == session.pid)
            {
                continue;
            }
            let Some(program) = crate::programs::by_id(&session.program_id) else {
                continue;
            };
            let panel = agent_panel(app, project, session, &program, &app.theme.borrow().clone());
            board.panels.borrow_mut().push(panel.clone());
            if let Some(tree) = board.tree.borrow_mut().as_mut() {
                tree.append(&panel);
            }
            removed = true;
        }
        if removed || board.panels.borrow().len() != wanted.len() {
            layout(app, board);
        }
    }

    // The headers: the dot breathes with the journal, the session name
    // follows the program, and the to-do link reflects the store.
    for panel in board.panels.borrow().iter() {
        let Some(session) = wanted.iter().find_map(|(_, session)| {
            (session.radar_session_id.as_deref() == Some(&panel.session_id))
                .then(|| session.clone())
        }) else {
            continue;
        };
        let sign = app.session_activity_sign(panel.project.id, &session);
        panel
            .dot
            .set_css_classes(&["activity-dot", sign.css_class()]);
        panel.dot.set_tooltip_text(Some(sign.label()));
        panel.session_label.set_text(&session.title);
        match session
            .card_id
            .as_deref()
            .and_then(|card_id| Some((card_id, app.card_title(panel.project.id, card_id)?)))
        {
            Some((card_id, title)) => {
                panel.todo_label.set_text(&title);
                panel.todo_button.set_visible(true);
                panel
                    .todo_button
                    .set_action_target_value(Some(&(panel.project.id, card_id).to_variant()));
                panel.todo_done.set_visible(true);
                panel
                    .todo_done
                    .set_action_target_value(Some(&(panel.project.id, card_id).to_variant()));
            }
            None => {
                panel.todo_button.set_visible(false);
                panel.todo_done.set_visible(false);
            }
        }
    }

    // Protocol conversations have no terminal to tile; their rows open the
    // native conversation controls while the terminal wall stays attached.
    while let Some(child) = page.conversations.first_child() {
        page.conversations.remove(&child);
    }
    let conversations = app.live_acp_cards();
    for (project_id, agent) in &conversations {
        let Some(card_id) = agent.card_id.as_deref() else {
            continue;
        };
        let title = app
            .card_title(*project_id, card_id)
            .unwrap_or_else(|| card_id.into());
        let button =
            gtk::Button::with_label(&format!("{title} · {} · {}", agent.provider, agent.state));
        button.set_action_name(Some("win.acp-card-open"));
        button.set_action_target_value(Some(&(*project_id, card_id).to_variant()));
        page.conversations.append(&button);
    }
    page.conversations.set_visible(!conversations.is_empty());
    board
        .holder
        .set_visible(!board.panels.borrow().is_empty() || conversations.is_empty());
    page.meta.set_text(&super::home::agents_count_text(
        board.panels.borrow().len() + conversations.len(),
    ));
}

fn auto_node(board: &AgentsBoard, panels: &[Rc<AgentPanel>]) -> Option<Node<AgentPanel>> {
    split::auto_node(panels, board.holder.width(), board.holder.height())
}

pub(super) fn auto_arrange(app: &App, board: &Rc<AgentsBoard>) {
    board.tree.borrow_mut().take();
    board.positions.borrow_mut().clear();
    layout(app, board);
}

/// Render the arrangement into the holder. Structural changes only — a
/// relayout re-parents live panels, which is safe but not free.
fn layout(app: &App, board: &Rc<AgentsBoard>) {
    let focus = app
        .window
        .focus_widget()
        .filter(|focus| focus.is_ancestor(&board.holder));
    if focus.is_some() {
        gtk::prelude::GtkWindowExt::set_focus(&app.window, None::<&gtk::Widget>);
    }
    super::tiling::detach_dividers(&board.dividers.borrow());
    board.dividers.borrow_mut().clear();
    let panels = board.panels.borrow().clone();

    for panel in &panels {
        panel.widget.unparent();
    }
    while let Some(child) = board.holder.first_child() {
        board.holder.remove(&child);
    }

    if panels.is_empty() {
        board.holder.append(&empty_state());
        return;
    }

    let root = match board.tree.borrow().as_ref() {
        Some(tree) => build_tree(app, board, tree),
        None => {
            let board = Rc::downgrade(board);
            super::tiling::automatic(
                panels
                    .iter()
                    .map(|panel| panel.widget.clone().upcast())
                    .collect(),
                &app.window,
                move |dividers| {
                    if let Some(board) = board.upgrade() {
                        *board.dividers.borrow_mut() = dividers;
                    }
                },
            )
        }
    };
    board.holder.append(&root);
    if let Some(focus) = focus.filter(|focus| focus.is_ancestor(&board.holder)) {
        focus.grab_focus();
    }
}

fn build_tree(app: &App, board: &Rc<AgentsBoard>, node: &Node<AgentPanel>) -> gtk::Widget {
    match node {
        Node::Leaf(panel) => panel.widget.clone().upcast::<gtk::Widget>(),
        Node::Split {
            axis,
            ratio,
            key,
            first,
            second,
        } => {
            let gtk_axis = match axis {
                split::Axis::Horizontal => gtk::Orientation::Horizontal,
                split::Axis::Vertical => gtk::Orientation::Vertical,
            };
            let f = build_tree(app, board, first);
            let s = build_tree(app, board, second);
            stacked(app, board, key, gtk_axis, &f, &s, *ratio)
        }
    }
}

/// A draggable divider whose position is remembered by the split node's key —
/// the same memory the workspace's dividers keep.
fn stacked(
    app: &App,
    board: &Rc<AgentsBoard>,
    key: &str,
    orientation: gtk::Orientation,
    first: &gtk::Widget,
    second: &gtk::Widget,
    fraction: f64,
) -> gtk::Widget {
    let paned = gtk::Paned::new(orientation);
    paned.set_wide_handle(true);
    paned.set_resize_start_child(true);
    paned.set_resize_end_child(true);
    paned.set_shrink_start_child(false);
    paned.set_shrink_end_child(false);
    paned.set_vexpand(true);
    paned.set_hexpand(true);
    paned.set_focusable(true);
    paned.set_tooltip_text(Some("Focusable divider — use arrow keys to resize"));
    paned.set_start_child(Some(first));
    paned.set_end_child(Some(second));

    super::tiling::fit_divider(&paned, fraction, board.positions.borrow().get(key).copied());

    let board_for_position = Rc::clone(board);
    let key = key.to_string();
    paned.connect_position_notify(move |paned| {
        board_for_position
            .positions
            .borrow_mut()
            .insert(key.clone(), paned.position());
    });

    super::tiling::keyboard_resize(&paned, &app.window);
    board.dividers.borrow_mut().push(paned.clone());
    paned.upcast()
}

/// Swap two panels' places. The ids are the drag payload's session ids.
pub(super) fn swap(app: &App, board: &Rc<AgentsBoard>, first_id: &str, second_id: &str) {
    if first_id == second_id {
        return;
    }
    let (first, second) = {
        let panels = board.panels.borrow();
        (
            panels.iter().find(|p| p.session_id == first_id).cloned(),
            panels.iter().find(|p| p.session_id == second_id).cloned(),
        )
    };
    let (Some(first), Some(second)) = (first, second) else {
        return;
    };
    if Rc::ptr_eq(&first, &second) {
        return;
    }
    let mut tree = {
        let mut guard = board.tree.borrow_mut();
        if guard.is_none() {
            *guard = auto_node(board, &board.panels.borrow());
        }
        guard.take()
    };
    let Some(tree) = tree.as_mut() else {
        return;
    };
    let swapped = tree.swap_leaves(&first, &second);
    let arranged = tree.clone();
    *board.tree.borrow_mut() = Some(arranged);
    if swapped {
        layout(app, board);
    }
}

/// The dragged panel takes a half of the target's region — an edge drop. The
/// middle is a swap (see [`swap`]).
pub(super) fn nest(
    app: &App,
    board: &Rc<AgentsBoard>,
    dragged_id: &str,
    target_id: &str,
    zone: &str,
) {
    if dragged_id == target_id {
        return;
    }
    let (axis, dragged_first) = match zone {
        "left" => (split::Axis::Horizontal, true),
        "top" => (split::Axis::Vertical, true),
        "bottom" => (split::Axis::Vertical, false),
        // right: the dragged panel takes the right side, the way a tiling
        // manager splits by default.
        _ => (split::Axis::Horizontal, false),
    };
    let dragged_panel = board
        .panels
        .borrow()
        .iter()
        .find(|panel| panel.session_id == dragged_id)
        .cloned();
    let target_panel = board
        .panels
        .borrow()
        .iter()
        .find(|panel| panel.session_id == target_id)
        .cloned();
    let (Some(dragged_panel), Some(target_panel)) = (dragged_panel, target_panel) else {
        return;
    };
    if Rc::ptr_eq(&dragged_panel, &target_panel) {
        return;
    }

    let planted = {
        let existing = board.tree.borrow_mut().take();
        // Lift the dragged panel out of wherever it sits first — its place in
        // the auto arrangement — so it lands in the tree exactly once. A
        // panel in two leaves would try to parent one widget into two panes,
        // and the loser half stays empty.
        let others: Vec<Rc<AgentPanel>> = board
            .panels
            .borrow()
            .iter()
            .filter(|panel| !Rc::ptr_eq(panel, &dragged_panel))
            .cloned()
            .collect();
        let mut tree = match existing {
            Some(tree) => tree.take_leaf(&dragged_panel),
            None => auto_node(board, &others),
        }
        .or_else(|| auto_node(board, &others));
        let (first, second) = if dragged_first {
            (Node::leaf(&dragged_panel), Node::leaf(&target_panel))
        } else {
            (Node::leaf(&target_panel), Node::leaf(&dragged_panel))
        };
        let key = format!("agents-tree-{dragged_id}-{target_id}");
        let ok = match tree.as_mut() {
            Some(tree) => tree.replace(&target_panel, Node::split(axis, 0.5, key, first, second)),
            None => false,
        };
        if ok {
            *board.tree.borrow_mut() = tree;
        }
        ok
    };
    if planted {
        layout(app, board);
    }
}

/// A panel the human closed: off the wall, program keeps running — the same
/// contract as closing a workspace panel. The caller records the dismissal.
pub(super) fn forget(app: &App, board: &Rc<AgentsBoard>, session_id: &str) {
    let removed = take_first(&board.panels, |panel| panel.session_id == session_id);
    if let Some(panel) = removed {
        let mut tree = board.tree.borrow_mut().take();
        if let Some(node) = tree.take() {
            tree = node.take_leaf(&panel);
        }
        *board.tree.borrow_mut() = tree;
    }
    layout(app, board);
}

/// Remove the first item matching `predicate`, borrowing the cell only for the
/// search and only for the removal — never both at once. Holding one
/// `borrow_mut()` across a closure that borrows again panics a `RefCell`, and
/// that panic takes the whole app down, so this is the one safe way to
/// "find and remove" from a `RefCell<Vec<_>>`.
fn take_first<T>(items: &RefCell<Vec<T>>, predicate: impl Fn(&T) -> bool) -> Option<T> {
    let position = items.borrow().iter().position(predicate)?;
    Some(items.borrow_mut().remove(position))
}

fn agent_panel(
    app: &App,
    project: &Project,
    session: &live_agents::AgentSession,
    program: &crate::programs::Program,
    theme: &Theme,
) -> Rc<AgentPanel> {
    // The spec is the fallback: a live session attaches as-is, and only a
    // vanished one runs this. Same program, same project, same card.
    let session_id = session
        .radar_session_id
        .clone()
        .unwrap_or_else(|| session.id.clone());
    let mut options = app.launch_options();
    options.card = session.card_id.clone();
    let mut spec = program.command_spec(&options);
    super::add_session_environment(
        &mut spec,
        project.id,
        &project.path,
        &app.session_home,
        &session_id,
    );
    let pane = Rc::new(Pane::spawn(
        &spec,
        &project.path,
        theme,
        &session.title,
        super::pane::ShiftEnter::for_slot(crate::db::Slot::Agent),
        &app.session_home,
        &session_id,
    ));

    // The same header a workspace panel wears — the panel-header classes, so
    // the look cannot drift from the workspace.
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 5);
    header.add_css_class("panel-header");
    header.set_valign(gtk::Align::Center);

    let dot = gtk::Label::new(Some("●"));
    dot.set_css_classes(&["activity-dot"]);
    dot.set_valign(gtk::Align::Center);
    header.append(&dot);

    let session_label = gtk::Label::new(Some(&session.title));
    session_label.add_css_class("caption-heading");
    session_label.add_css_class("pane-info");
    session_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    session_label.set_xalign(0.0);
    header.append(&session_label);

    let todo_button = gtk::Button::new();
    todo_button.add_css_class("flat");
    todo_button.add_css_class("panel-todo");
    todo_button.set_visible(false);
    let todo_label = gtk::Label::new(None);
    todo_label.add_css_class("caption");
    todo_label.add_css_class("dim-label");
    todo_label.add_css_class("pane-info");
    todo_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    todo_label.set_xalign(0.0);
    todo_button.set_child(Some(&todo_label));
    todo_button.set_tooltip_text(Some("Open this card — read it, reply, edit it"));
    todo_button.set_action_name(Some("win.open-card"));
    header.append(&todo_button);

    let todo_done = gtk::Button::builder()
        .icon_name("object-select-symbolic")
        .tooltip_text("Mark this to-do done")
        .build();
    todo_done.add_css_class("chip-done");
    todo_done.set_valign(gtk::Align::Center);
    todo_done.set_visible(false);
    todo_done.set_action_name(Some("win.card-toggle-done"));
    header.append(&todo_done);

    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    header.append(&spacer);

    let close = gtk::Button::builder()
        .icon_name("window-close-symbolic")
        .tooltip_text("Close this panel — its program keeps running")
        .build();
    close.add_css_class("flat");
    close.add_css_class("chip-close");
    close.set_valign(gtk::Align::Center);
    close.set_action_name(Some("win.agents-close"));
    close.set_action_target_value(Some(&session_id.to_variant()));
    header.append(&close);

    let content = gtk::Stack::builder().vexpand(true).hexpand(true).build();
    content.add_named(pane.widget(), Some(&session_id));

    // The per-edge drop indicator: a tint over exactly the half a drop would
    // hand to the dragged panel — or the whole panel for a swap.
    let edge = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    edge.add_css_class("drop-edge");
    edge.set_halign(gtk::Align::Fill);
    edge.set_valign(gtk::Align::Fill);
    edge.set_can_target(false);
    edge.set_visible(false);

    let widget = gtk::Overlay::new();
    widget.add_css_class("panel-pane");
    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    body.append(&header);
    body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    body.append(&content);
    widget.set_child(Some(&body));
    widget.add_overlay(&edge);

    let panel = Rc::new(AgentPanel {
        widget,
        header,
        edge,
        session_id,
        project: project.clone(),
        pane,
        dot,
        session_label,
        todo_button,
        todo_label,
        todo_done,
        pid: session.pid,
    });
    accept_drags(&panel);
    panel
}

/// The header drags; the whole panel accepts drops. The payload is the
/// session's daemon id; drops emit the agents-page's own actions, which
/// resolve this board.
fn accept_drags(panel: &Rc<AgentPanel>) {
    use gtk::PropagationPhase;

    // ---- drag a panel by its header ----
    let source = gtk::DragSource::builder()
        .actions(gdk::DragAction::MOVE)
        .build();
    source.set_propagation_phase(PropagationPhase::Capture);
    let panel_for_drag = panel.clone();
    source.connect_prepare(move |_, _, _| {
        Some(gdk::ContentProvider::for_value(
            &panel_for_drag.session_id.to_value(),
        ))
    });
    panel.header.add_controller(source);

    // ---- the whole panel accepts a drop ----
    // Where you release decides what happens: an edge splits the panel in
    // two (side by side, or stacked), the middle swaps places. Capture phase,
    // because a terminal widget has its own drop target for text.
    let target = gtk::DropTarget::new(glib::types::Type::STRING, gdk::DragAction::MOVE);
    target.set_propagation_phase(PropagationPhase::Capture);
    target.connect_accept(|_, drag| drag.actions().contains(gdk::DragAction::MOVE));

    let edge_for_highlight = panel.edge.clone();
    target.connect_motion(move |_, x, y| {
        let (w, h) = (edge_for_highlight.width(), edge_for_highlight.height());
        let (left, right, top, bottom) = super::panel::edge_margins(
            super::panel::drop_zone(w, h, x, y),
            w,
            h,
            super::panel::EDGE_PAD,
        );
        edge_for_highlight.set_margin_start(left);
        edge_for_highlight.set_margin_end(right);
        edge_for_highlight.set_margin_top(top);
        edge_for_highlight.set_margin_bottom(bottom);
        edge_for_highlight.set_visible(true);
        gdk::DragAction::MOVE
    });
    let edge_for_unhighlight = panel.edge.clone();
    target.connect_leave(move |_| {
        edge_for_unhighlight.set_visible(false);
    });

    let widget = panel.widget.clone();
    let edge = panel.edge.clone();
    let panel_for_drop = panel.clone();
    target.connect_drop(move |_, value, x, y| {
        let Ok(dragged) = value.get::<String>() else {
            return false;
        };
        let target_id = panel_for_drop.session_id.clone();
        let zone = super::panel::drop_zone(widget.width(), widget.height(), x, y);
        edge.set_visible(false);
        if dragged == target_id {
            return false;
        }
        let widget = widget.clone();
        if zone == "center" {
            let variant = (dragged, target_id).to_variant();
            glib::idle_add_local_once(move || {
                let _ = widget.activate_action("win.agents-swap", Some(&variant));
            });
        } else {
            let variant = (dragged, target_id, zone).to_variant();
            glib::idle_add_local_once(move || {
                let _ = widget.activate_action("win.agents-nest-split", Some(&variant));
            });
        }
        true
    });
    panel.widget.add_controller(target);
}

#[cfg(test)]
mod tests {
    use super::take_first;
    use std::cell::RefCell;

    /// Closing a panel used to hold one `borrow_mut()` across a closure that
    /// borrowed again, which panics and crashes the app. `take_first` must
    /// search and remove under separate borrows.
    #[test]
    fn take_first_borrows_once_at_a_time() {
        let items = RefCell::new(vec!["a", "b", "c"]);
        assert_eq!(take_first(&items, |item| *item == "b"), Some("b"));
        assert_eq!(*items.borrow(), vec!["a", "c"]);
        assert_eq!(take_first(&items, |item| *item == "z"), None);
        assert_eq!(*items.borrow(), vec!["a", "c"]);
    }
}
