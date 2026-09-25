//! Keyboard control: moving the keys between primitives.
//!
//! radar claims a few chords for itself — the ones a terminal program would
//! otherwise eat — and passes every other key through untouched, so each
//! primitive keeps the keys it was written for:
//!
//! - `Ctrl+Arrows` move focus between panes, the sidebar, and dividers. Plain
//!   arrows resize a focused divider. The chord is taken here, in the capture
//!   phase on the window, because a VTE terminal encodes `Ctrl+Arrows` and
//!   would send it to the shell as word movement instead. Text fields keep
//!   the chord: a cursor moves by words there, and radar never competes with
//!   a text cursor.
//! - `Ctrl+Tab` / `Ctrl+Shift+Tab` cycle panes and dividers in layout order.
//! - `Menu` (or `Shift+F10`) opens the focused pane's menu — every pane
//!   action radar has, no mouse needed.
//!
//! The "which pane is over there" geometry is pure data, like `split.rs`, so
//! it is testable without a display.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use super::group::Group;
use super::SharedApp;

/// Which way the keys should move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    /// For toasts that say there was nothing that way.
    fn word(self) -> &'static str {
        match self {
            Direction::Left => "to the left",
            Direction::Right => "to the right",
            Direction::Up => "above",
            Direction::Down => "below",
        }
    }
}

/// A pane's rectangle, in window coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

/// The candidate nearest in `direction`: mostly "that way", with a soft
/// penalty for drifting across, so in a column of panes "right" picks the
/// pane actually beside you rather than one far up or down.
pub fn pick(current: &Rect, candidates: &[(usize, Rect)], direction: Direction) -> Option<usize> {
    let (cx, cy) = current.center();
    let mut best: Option<(f64, usize)> = None;
    for (id, rect) in candidates {
        if rect.w <= 0.0 || rect.h <= 0.0 {
            continue;
        }
        let (ox, oy) = rect.center();
        let (delta, drift) = match direction {
            Direction::Left | Direction::Right => (ox - cx, oy - cy),
            Direction::Up | Direction::Down => (oy - cy, ox - cx),
        };
        let going = match direction {
            Direction::Left | Direction::Up => delta < -8.0,
            Direction::Right | Direction::Down => delta > 8.0,
        };
        if !going {
            continue;
        }
        let score = delta.abs() + drift.abs() * 2.0;
        if best.is_none_or(|(top, _)| score < top) {
            best = Some((score, *id));
        }
    }
    best.map(|(_, id)| id)
}

/// One thing the keys can land on.
enum Target {
    Sidebar,
    Pane(Rc<Group>),
    Divider(gtk::Paned),
}

/// Install radar's own chords on the window. Runs in the capture phase,
/// ahead of the focused widget — a terminal would otherwise encode these
/// keys into the program it runs.
pub fn install(app: &SharedApp) {
    let controller = gtk::EventControllerKey::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    let app_for_keys = app.clone();
    controller.connect_key_pressed(move |_, key, _, modifiers| {
        let app = &app_for_keys;
        let ctrl = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK);
        let shift = modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK);
        let hud_open = app.hud.is_visible();

        if let Some(direction) = match key {
            gtk::gdk::Key::Left => Some(Direction::Left),
            gtk::gdk::Key::Right => Some(Direction::Right),
            gtk::gdk::Key::Up => Some(Direction::Up),
            gtk::gdk::Key::Down => Some(Direction::Down),
            _ => None,
        } {
            if ctrl && !shift && !hud_open && keys_are_free(&app.window) {
                move_focus(app, direction);
                return glib::Propagation::Stop;
            }
            return glib::Propagation::Proceed;
        }
        if ctrl && key == gtk::gdk::Key::Tab && !hud_open {
            cycle_focus(app, shift);
            return glib::Propagation::Stop;
        }
        let menu = key == gtk::gdk::Key::Menu || (shift && key == gtk::gdk::Key::F10);
        if menu && !hud_open {
            open_pane_menu(app);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    app.window.add_controller(controller);
    install_ring(app);
}

/// False when the keys are in a text field: a cursor keeps `Ctrl+Arrows` for
/// word movement, the way it always was.
fn keys_are_free(window: &adw::ApplicationWindow) -> bool {
    let Some(focus) = window.focus_widget() else {
        return true;
    };
    let mut cursor = Some(focus);
    while let Some(widget) = cursor {
        if widget.is::<gtk::Editable>() || widget.is::<gtk::TextView>() {
            return false;
        }
        cursor = widget.parent();
    }
    true
}

/// Directional navigation lands on panels only; divider hit areas are kept out
/// so Ctrl+Arrows always moves between the sidebar and panes.
fn panel_targets(app: &SharedApp, workspace: &super::Workspace) -> Vec<(Target, Rect)> {
    let mut targets = Vec::new();
    if app.sidebar_shown.get() {
        if let Some(rect) = rect_of(&app.sidebar, &app.window) {
            targets.push((Target::Sidebar, rect));
        }
    }
    for group in workspace.groups() {
        if let Some(rect) = rect_of(group.widget.upcast_ref(), &app.window) {
            targets.push((Target::Pane(group), rect));
        }
    }
    targets
}

/// Ctrl+Tab also cycles through dividers so they remain keyboard-focusable.
fn cycle_targets(app: &SharedApp, workspace: &super::Workspace) -> Vec<(Target, Rect)> {
    let mut targets = panel_targets(app, workspace);
    for divider in workspace.dividers() {
        if let Some(rect) = divider_rect_of(&divider, &app.window) {
            targets.push((Target::Divider(divider), rect));
        }
    }
    targets
}

fn rect_of(widget: &gtk::Widget, window: &adw::ApplicationWindow) -> Option<Rect> {
    let (x, y) = widget.translate_coordinates(window, 0.0, 0.0)?;
    let (w, h) = (widget.width() as f64, widget.height() as f64);
    (w > 0.0 && h > 0.0).then_some(Rect { x, y, w, h })
}

/// A Paned's allocation spans both children; navigation should aim for the
/// actual handle between them instead.
fn divider_rect_of(paned: &gtk::Paned, window: &adw::ApplicationWindow) -> Option<Rect> {
    const HANDLE_HIT_AREA: f64 = 8.0;
    let (x, y) = paned.translate_coordinates(window, 0.0, 0.0)?;
    let position = paned.position().max(0) as f64;
    let (width, height) = (paned.width() as f64, paned.height() as f64);
    (width > 0.0 && height > 0.0).then_some(match paned.orientation() {
        gtk::Orientation::Horizontal => Rect {
            x: x + position - HANDLE_HIT_AREA / 2.0,
            y,
            w: HANDLE_HIT_AREA,
            h: height,
        },
        gtk::Orientation::Vertical => Rect {
            x,
            y: y + position - HANDLE_HIT_AREA / 2.0,
            w: width,
            h: HANDLE_HIT_AREA,
        },
        _ => unreachable!("GtkPaned only has horizontal and vertical orientations"),
    })
}

/// Where the keys are now: an index into the supplied navigation targets, if
/// they are in the workspace at all.
fn current_index(
    app: &SharedApp,
    targets: &[(Target, Rect)],
    focus: Option<&gtk::Widget>,
) -> Option<usize> {
    let focus = focus?;
    targets.iter().position(|(target, _)| match target {
        Target::Sidebar => focus.is_ancestor(&app.sidebar),
        Target::Pane(group) => focus.is_ancestor(&group.widget),
        Target::Divider(divider) => *focus == divider.clone().upcast::<gtk::Widget>(),
    })
}

fn move_focus(app: &SharedApp, direction: Direction) {
    let Some(workspace) = app.current_workspace() else {
        return;
    };
    let targets = panel_targets(app, &workspace);
    if targets.is_empty() {
        app.toast("No panes on screen — open one with the dock or Ctrl+Shift+K");
        return;
    }
    let focus = app.window.focus_widget();
    let current = current_index(app, &targets, focus.as_ref());
    let current_rect = current
        .and_then(|index| targets.get(index).map(|(_, rect)| *rect))
        .or_else(|| {
            let focus = focus.as_ref()?;
            workspace
                .dividers()
                .into_iter()
                .find(|divider| *focus == divider.clone().upcast::<gtk::Widget>())
                .and_then(|divider| divider_rect_of(&divider, &app.window))
        });
    let Some(current_rect) = current_rect else {
        // The keys are nowhere in the workspace (a fresh window): hand them
        // to the first pane, so the chord works before any click.
        focus_target(app, &workspace, &targets[0].0);
        return;
    };
    let rest: Vec<(usize, Rect)> = targets
        .iter()
        .enumerate()
        .filter(|(index, _)| Some(*index) != current)
        .map(|(index, (_, rect))| (index, *rect))
        .collect();
    match pick(&current_rect, &rest, direction) {
        Some(index) => focus_target(app, &workspace, &targets[index].0),
        None => app.toast(&format!("Nothing {}", direction.word())),
    }
}

fn cycle_focus(app: &SharedApp, backwards: bool) {
    let Some(workspace) = app.current_workspace() else {
        return;
    };
    let targets = cycle_targets(app, &workspace);
    if targets.is_empty() {
        return;
    }
    let focus = app.window.focus_widget();
    let count = targets.len() as i32;
    let next = match current_index(app, &targets, focus.as_ref()) {
        Some(current) => (current as i32 + if backwards { -1 } else { 1 }).rem_euclid(count) as usize,
        // Nothing has the keys yet: forwards starts at the first pane,
        // backwards at the last.
        None => (if backwards { count - 1 } else { 0 }) as usize,
    };
    focus_target(app, &workspace, &targets[next].0);
}

/// Give the keys to a target: the primitive that is showing in the pane, or
/// the project list. Arrow keys inside the sidebar switch projects — the
/// list's own selection change does that.
fn focus_target(app: &SharedApp, workspace: &Rc<super::Workspace>, target: &Target) {
    match target {
        Target::Sidebar => {
            app.sidebar_list.grab_focus();
        }
        Target::Pane(group) => {
            if let Some(slot) = group.active_slot() {
                group.activate(slot);
                if let Some(primitive) = workspace.primitive(slot) {
                    primitive.focus();
                }
            }
        }
        Target::Divider(divider) => {
            divider.grab_focus();
        }
    }
}

/// The focused pane's menu: which program it runs, grouping, splitting,
/// toggles — the whole pane vocabulary, keyboard-driven.
fn open_pane_menu(app: &SharedApp) {
    let Some(workspace) = app.current_workspace() else {
        return;
    };
    if let Some(group) = app.focused_group(&workspace) {
        group.menu_button.popup();
    }
}

/// The ring follows focus, however it got there. Focusing a chip also activates
/// the primitive it names, so the visible content follows keyboard focus.
fn install_ring(app: &SharedApp) {
    let app = Rc::clone(app);
    let window = app.window.clone();
    window.connect_focus_widget_notify(move |window| {
        let focus = window.focus_widget();
        let in_sidebar = focus
            .as_ref()
            .is_some_and(|focus| focus.is_ancestor(&app.sidebar));
        let workspace = app.current_workspace();
        if let (Some(workspace), Some(focus)) = (&workspace, focus.as_ref()) {
            for group in workspace.groups() {
                if let Some(slot) = group.slot_for_focus(focus) {
                    if group.active_slot() != Some(slot) {
                        group.activate(slot);
                        app.refresh_group_menu(&group);
                        app.persist_primitives(workspace);
                    }
                }
            }
        }
        let focused = if in_sidebar {
            None
        } else {
            workspace
                .as_ref()
                .and_then(|workspace| app.focused_group(workspace))
        };
        for group in workspace
            .as_ref()
            .map(|workspace| workspace.groups())
            .unwrap_or_default()
        {
            let on = focused.as_ref().is_some_and(|f| Rc::ptr_eq(f, &group));
            group.widget.remove_css_class("kbd-focus");
            if on {
                group.widget.add_css_class("kbd-focus");
            }
        }
        for divider in workspace
            .as_ref()
            .map(|workspace| workspace.dividers())
            .unwrap_or_default()
        {
            let on = focus
                .as_ref()
                .is_some_and(|focus| *focus == divider.clone().upcast::<gtk::Widget>());
            divider.remove_css_class("divider-focus");
            if on {
                divider.add_css_class("divider-focus");
            }
        }
        app.sidebar.remove_css_class("kbd-focus");
        if in_sidebar {
            app.sidebar.add_css_class("kbd-focus");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rects are (x, y, w, h) in window coordinates.
    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    fn candidates(rects: &[Rect]) -> Vec<(usize, Rect)> {
        rects.iter().enumerate().map(|(index, rect)| (index, *rect)).collect()
    }

    #[test]
    fn sideways_picks_the_neighbouring_pane() {
        let current = rect(300.0, 0.0, 300.0, 600.0);
        let all = [rect(0.0, 0.0, 300.0, 600.0), rect(600.0, 0.0, 300.0, 600.0)];
        let rest = candidates(&all);
        assert_eq!(pick(&current, &rest, Direction::Left), Some(0));
        assert_eq!(pick(&current, &rest, Direction::Right), Some(1));
        assert_eq!(
            pick(&current, &rest, Direction::Up),
            None,
            "nothing above a full-height pane"
        );
    }

    #[test]
    fn down_prefers_the_pane_below_over_the_one_just_near() {
        // A 2x2 grid: below the top-right pane, the bottom-right pane wins
        // over the bottom-left one, which is nearer as the crow flies but
        // drifts across.
        let current = rect(300.0, 0.0, 300.0, 300.0);
        let bottom_right = rect(300.0, 300.0, 300.0, 300.0);
        let bottom_left = rect(0.0, 300.0, 300.0, 300.0);
        let rest = candidates(&[bottom_left, bottom_right]);
        assert_eq!(pick(&current, &rest, Direction::Down), Some(1));
    }

    #[test]
    fn left_from_the_leftmost_pane_finds_nothing() {
        let current = rect(0.0, 0.0, 300.0, 600.0);
        let rest = candidates(&[rect(300.0, 0.0, 300.0, 600.0)]);
        assert_eq!(pick(&current, &rest, Direction::Left), None);
        assert_eq!(
            pick(&current, &rest, Direction::Right),
            Some(0),
            "the same pane is reachable the other way"
        );
    }

    #[test]
    fn arrow_focus_from_a_divider_moves_to_a_panel_on_that_side() {
        let divider = rect(296.0, 0.0, 8.0, 600.0);
        let panels = candidates(&[
            rect(0.0, 0.0, 296.0, 600.0),
            rect(304.0, 0.0, 300.0, 600.0),
        ]);
        assert_eq!(pick(&divider, &panels, Direction::Left), Some(0));
        assert_eq!(pick(&divider, &panels, Direction::Right), Some(1));
    }

    #[test]
    fn stacked_panes_hand_over_cleanly() {
        // A classic radar layout: editor and agent stacked on the right of a
        // main pane, commands along the bottom. From the agent, down lands on
        // the commands pane even though the editor sits closer in x.
        let editor = rect(300.0, 0.0, 400.0, 300.0);
        let agent = rect(700.0, 0.0, 400.0, 300.0);
        let commands = rect(300.0, 300.0, 800.0, 300.0);
        let current = agent;
        let rest = candidates(&[editor, commands]);
        assert_eq!(pick(&current, &rest, Direction::Down), Some(1));
        assert_eq!(
            pick(&current, &rest, Direction::Left),
            Some(0),
            "across the main pane"
        );
    }

    #[test]
    fn empty_and_degenerate_candidates_are_skipped() {
        let current = rect(0.0, 0.0, 100.0, 100.0);
        let collapsed = rect(300.0, 0.0, 0.0, 100.0);
        let rest = candidates(&[collapsed]);
        assert_eq!(pick(&current, &rest, Direction::Right), None);
        assert_eq!(pick(&current, &[], Direction::Right), None);
    }
}
