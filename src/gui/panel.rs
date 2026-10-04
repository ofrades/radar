//! A panel: one header over one tab.
//!
//! A workspace is a tiling of panels. Every open primitive — a session, a
//! terminal, the changes view — gets a panel of its own; two tabs never share
//! a header. Drag a panel by its header and drop it on another: an edge drop
//! splits that panel in two and the dragged one takes the half; a middle drop
//! swaps the two panels' places. There is no grouping.
//!
//! A panel's header says what it is and what it is for: the session's name
//! and, for an agent, the board to-do it corresponds to — the to-do title
//! opens its card view, and a check beside it marks the to-do done. Nothing
//! else but split and close: content is chosen in the new half.
//!
//! Both sides talk to the window through actions, so this module needs no
//! reference back to the app.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::gdk;
use gtk::glib;

use super::activity_sign::Sign;
use crate::db::{Slot, TabKey};

/// What a drop on a panel should do, decided by where it landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropIntent {
    /// Landed on an edge: split this panel and take that half.
    Split,
    /// Landed on the middle: exchange places with this panel.
    Swap,
    /// Nothing to do — a panel dropped on itself, or an un-draggable tab.
    Ignore,
}

/// Where a drop landed and what it means. `zone` is one of `left`, `right`,
/// `top`, `bottom` or `center` (see [`drop_zone`]).
pub fn drop_intent(zone: &str, dragged: TabKey, target: TabKey) -> DropIntent {
    if dragged.slot == Slot::Custom || dragged == target {
        return DropIntent::Ignore;
    }
    if zone == "center" {
        DropIntent::Swap
    } else {
        DropIntent::Split
    }
}

/// Which part of a panel a drop landed on: the half the drop implies, or the
/// middle for a swap.
pub fn drop_zone(width: i32, height: i32, x: f64, y: f64) -> &'static str {
    let (fw, fh) = (width as f64, height as f64);
    if fw <= 0.0 || fh <= 0.0 {
        return "right";
    }
    if y / fh < 0.25 {
        "top"
    } else if y / fh > 0.75 {
        "bottom"
    } else if x / fw < 0.3 {
        "left"
    } else if x / fw > 0.7 {
        "right"
    } else {
        "center"
    }
}

/// How far the edge indicator sits in from the panel's rim, so the panel's own
/// border stays visible under it.
pub(super) const EDGE_PAD: i32 = 6;

/// The margins that carve the edge rectangle down to the half `zone` points
/// at — (left, right, top, bottom), inset by `pad`. A centre drop swaps, so it
/// lights the whole panel.
pub fn edge_margins(zone: &str, width: i32, height: i32, pad: i32) -> (i32, i32, i32, i32) {
    let (half_w, half_h) = (width / 2, height / 2);
    match zone {
        "top" => (pad, pad, pad, half_h),
        "bottom" => (pad, pad, half_h, pad),
        "left" => (pad, half_w, pad, pad),
        "right" => (half_w, pad, pad, pad),
        // centre, and anything unexpected: the whole panel.
        _ => (pad, pad, pad, pad),
    }
}

/// A panel's header: the activity sign, the session's name, the to-do it
/// corresponds to, with split and close controls on the right.
struct Chip {
    /// The row as one unit, which drags hit-test and focus lookups walk.
    widget: gtk::Box,
    /// The session's live activity sign, coloured by its working / waiting /
    /// running / stopped state.
    sign: gtk::Label,
    /// What the session calls itself.
    session: gtk::Label,
    /// The board to-do the session works on, when it has one. The button
    /// around the label opens the to-do; the label keeps the quiet look.
    todo_button: gtk::Button,
    todo: gtk::Label,
    /// Marks the to-do done from right here, beside its title. One-way: a
    /// done to-do's panel hides by the workspace-visibility policy.
    todo_done: gtk::Button,
}

pub struct Panel {
    /// The panel as the arrangement tree sees it: the body with the drop-edge
    /// indicator floating over it.
    pub widget: gtk::Overlay,
    pub header: gtk::Box,
    content: gtk::Stack,
    /// The rounded tint that covers the half a drop would hand to the dropped
    /// panel. Invisible until a drag hovers.
    edge: gtk::Box,
    /// The tab this panel shows.
    key: RefCell<Option<TabKey>>,
    /// The header's controls, built by `rebuild_header`.
    chip: RefCell<Option<Chip>>,
    /// The session's name, pushed by the window.
    session: RefCell<Option<String>>,
    /// The board to-do the session works on, pushed by the window.
    todo: RefCell<Option<String>>,
    /// Where the header's to-do opens: the card's (project, id), pushed with
    /// the title.
    todo_link: RefCell<Option<(i64, String)>>,
    /// The program rang the terminal bell and has not been looked at since.
    attention: Cell<bool>,
    /// The session's live activity sign, pushed by the window.
    activity: RefCell<Option<Sign>>,
}

impl Panel {
    pub fn new() -> Rc<Panel> {
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        // The strip spans the panel's full width; controls are inset by CSS
        // padding so the surface itself reaches the panel's edges.
        header.add_css_class("panel-header");

        let content = gtk::Stack::builder().vexpand(true).hexpand(true).build();

        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.set_vexpand(true);
        body.set_hexpand(true);
        body.append(&header);
        body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        body.append(&content);

        // The per-edge drop indicator: a tint over exactly the half a drop
        // would hand to the dropped panel — or the whole panel for a swap — so
        // the gesture is readable before it happens. Deaf to input: it must
        // never intercept the drag it is announcing.
        let edge = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        edge.add_css_class("drop-edge");
        edge.set_halign(gtk::Align::Fill);
        edge.set_valign(gtk::Align::Fill);
        edge.set_can_target(false);
        edge.set_visible(false);

        let widget = gtk::Overlay::new();
        widget.add_css_class("panel-pane");
        widget.set_child(Some(&body));
        widget.add_overlay(&edge);

        let panel = Rc::new(Panel {
            widget,
            header,
            content,
            edge,
            key: RefCell::new(None),
            chip: RefCell::new(None),
            session: RefCell::new(None),
            todo: RefCell::new(None),
            todo_link: RefCell::new(None),
            attention: Cell::new(false),
            activity: RefCell::new(None),
        });
        Panel::accept_drags(&panel);
        panel
    }

    /// The header drags; the whole panel accepts drops.
    ///
    /// One target covers the entire panel — header and body — because a 22 pixel
    /// header is a hard thing to hit, and because a terminal widget has its own
    /// drop target that would otherwise take the drop. It runs in the capture
    /// phase so this one always sees the event first.
    fn accept_drags(panel: &Rc<Panel>) {
        use gtk::PropagationPhase;

        // Hovering a panel is focusing it: the motion controller forwards the
        // enter to `hover_primitive`, which (only for a pointer that is really
        // moving) focuses the panel's tab — and the ring that follows keyboard
        // focus marks it. No separate hover highlight.
        let hover = gtk::EventControllerMotion::new();
        hover.set_propagation_phase(gtk::PropagationPhase::Capture);
        let panel_on_enter = panel.clone();
        hover.connect_enter(move |_, _, _| {
            if let Some(key) = panel_on_enter.key() {
                let _ = panel_on_enter
                    .widget
                    .activate_action("win.primitive-hover", Some(&key.as_str().to_variant()));
            }
        });
        panel.widget.add_controller(hover);

        // ---- drag a panel by its header ----
        //
        // Capture phase: the header is full of widgets, and the close button
        // claims the press a drag needs. Seeing the press first means the drag
        // starts no matter which child is under the pointer.
        let source = gtk::DragSource::builder()
            .actions(gdk::DragAction::MOVE)
            .build();
        source.set_propagation_phase(gtk::PropagationPhase::Capture);
        let panel_for_drag = panel.clone();
        source.connect_prepare(move |_, _, _| {
            let key = panel_for_drag.key();
            super::trace(&format!(
                "drag: prepare active={:?}",
                key.map(|key| key.as_str())
            ));
            key.map(|key| gdk::ContentProvider::for_value(&key.as_str().to_value()))
        });
        let header_for_drag = panel.header.clone();
        source.connect_drag_begin(move |_, _| {
            header_for_drag.add_css_class("dragging");
            super::trace("drag: began");
        });
        let header_for_end = panel.header.clone();
        source.connect_drag_end(move |_, _, committed| {
            header_for_end.remove_css_class("dragging");
            super::trace(&format!("drag: ended (committed: {committed})"));
        });
        panel.header.add_controller(source);

        // ---- the whole panel accepts a drop ----
        //
        // Where you release decides what happens: an edge splits the panel in
        // two, the middle swaps places. One target for the entire panel,
        // because a terminal widget has its own drop target for text that would
        // otherwise take the drop.
        let target = gtk::DropTarget::new(glib::types::Type::STRING, gdk::DragAction::MOVE);
        target.set_propagation_phase(PropagationPhase::Capture);
        target.connect_accept(|_, drag| drag.actions().contains(gdk::DragAction::MOVE));

        let edge_for_highlight = panel.edge.clone();
        target.connect_motion(move |_, x, y| {
            // Light up the half the drop would act on, so the gesture is
            // readable before releasing.
            let (w, h) = (edge_for_highlight.width(), edge_for_highlight.height());
            let (left, right, top, bottom) = edge_margins(drop_zone(w, h, x, y), w, h, EDGE_PAD);
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
            let Ok(payload) = value.get::<String>() else {
                super::trace("drop: payload was not a string");
                return false;
            };
            let dragged = TabKey::parse(&payload);
            let Some(target) = panel_for_drop.key() else {
                return false;
            };
            let zone = drop_zone(widget.width(), widget.height(), x, y);
            let intent = drop_intent(zone, dragged, target);
            edge.set_visible(false);
            super::trace(&format!(
                "drop: payload={payload} x={x:.0} y={y:.0} target={} zone={zone} -> {intent:?}",
                target.as_str()
            ));
            match intent {
                DropIntent::Ignore => false,
                DropIntent::Swap => {
                    // Deferred one main-loop turn: relayouting while the drop
                    // is still finishing leaves the rebuilt tree unallocated
                    // (0x0) — every panel gone, only the sidebar left. After
                    // the idle the drag is fully over and the relayout sticks.
                    let widget = widget.clone();
                    let variant = (dragged.as_str(), target.as_str()).to_variant();
                    glib::idle_add_local_once(move || {
                        let _ = widget.activate_action("win.pane-swap", Some(&variant));
                    });
                    true
                }
                DropIntent::Split => {
                    let widget = widget.clone();
                    let variant =
                        (dragged.as_str(), target.as_str(), zone.to_string()).to_variant();
                    glib::idle_add_local_once(move || {
                        let _ = widget.activate_action("win.pane-nest-split", Some(&variant));
                    });
                    true
                }
            }
        });
        panel.widget.add_controller(target);
    }

    /// An uncommitted split chooses its content before starting a program.
    pub fn choose_content(&self, widget: &gtk::Widget) {
        self.content.add_named(widget, Some("choose"));
        self.content.set_visible_child_name("choose");
    }

    /// Set this panel's tab and show its widget.
    pub fn insert(&self, key: TabKey, widget: &gtk::Widget) {
        if let Some(chooser) = self.content.child_by_name("choose") {
            self.content.remove(&chooser);
        }
        // A primitive's widget always has a parent — its old panel's stack —
        // and GTK refuses, silently, to add a widget that already has one.
        // Detach it first or the panel comes up empty.
        if widget.parent().is_some() {
            widget.unparent();
        }
        let name = key.as_str();
        if self.content.child_by_name(&name).is_none() {
            self.content.add_named(widget, Some(&name));
        }
        *self.key.borrow_mut() = Some(key);
        self.content.set_visible_child_name(&name);
    }

    /// Empty the panel. Returns true when it holds no tab.
    pub fn remove(&self) -> bool {
        if let Some(key) = self.key.borrow_mut().take() {
            if let Some(widget) = self.content.child_by_name(key.as_str().as_str()) {
                self.content.remove(&widget);
            }
        }
        *self.session.borrow_mut() = None;
        *self.todo.borrow_mut() = None;
        self.attention.set(false);
        *self.activity.borrow_mut() = None;
        self.apply_labels();
        self.is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.key.borrow().is_none()
    }

    pub fn contains(&self, key: TabKey) -> bool {
        *self.key.borrow() == Some(key)
    }

    /// The tab this panel shows.
    pub fn key(&self) -> Option<TabKey> {
        *self.key.borrow()
    }

    /// What the session calls itself. Empty clears back to nothing.
    pub fn set_session(&self, text: &str) {
        let text = (!text.is_empty()).then(|| text.to_string());
        if *self.session.borrow() == text {
            return;
        }
        *self.session.borrow_mut() = text;
        self.apply_labels();
    }

    /// The board to-do the session works on, and where it opens — the card's
    /// (project, id), so clicking the header's to-do lands on its card view.
    /// `None` clears it.
    pub fn set_todo(&self, text: Option<&str>, link: Option<(i64, String)>) {
        let text = text.filter(|text| !text.is_empty()).map(str::to_string);
        if *self.todo.borrow() == text && *self.todo_link.borrow() == link {
            return;
        }
        *self.todo.borrow_mut() = text;
        *self.todo_link.borrow_mut() = link;
        self.apply_labels();
    }

    /// The program rang the bell — an agent asking for attention. The header
    /// marks it until the panel is looked at.
    pub fn set_attention(&self) {
        self.attention.set(true);
        self.apply_labels();
    }

    /// The window's read of the tab's live session state, from the project's
    /// activity journal: working, waiting, running, idle or stopped. `None`
    /// clears the sign (a tab that runs no agent, or no signal yet).
    pub fn set_activity(&self, sign: Option<Sign>) {
        if *self.activity.borrow() == sign {
            return;
        }
        *self.activity.borrow_mut() = sign;
        self.apply_labels();
    }

    /// Push the stored session / to-do / attention / sign onto the header. A
    /// header rebuilt into a new panel comes back with the same labels.
    fn apply_labels(&self) {
        let chip = self.chip.borrow();
        let Some(chip) = chip.as_ref() else {
            return;
        };
        let session = self.session.borrow().clone();
        chip.session.set_visible(session.is_some());
        chip.session.set_text(session.as_deref().unwrap_or(""));
        chip.session.set_tooltip_text(session.as_deref());
        let todo = self.todo.borrow().clone();
        chip.todo_button.set_visible(todo.is_some());
        chip.todo.set_text(
            &todo
                .as_deref()
                .map(|todo| format!("· {todo}"))
                .unwrap_or_default(),
        );
        chip.todo.set_tooltip_text(
            todo.as_deref()
                .map(|todo| format!("Open this to-do — read it, reply, edit it\n{todo}"))
                .as_deref(),
        );
        match self.todo_link.borrow().clone() {
            Some((project_id, card_id)) => {
                chip.todo_done.set_visible(true);
                chip.todo_done
                    .set_action_name(Some("win.session-todo-done"));
                chip.todo_done
                    .set_action_target_value(Some(&(project_id, card_id.as_str()).to_variant()));
                if let Some(key) = self.key() {
                    chip.todo_button.set_action_target_value(Some(
                        &(project_id, key.as_str(), card_id.as_str()).to_variant(),
                    ));
                    chip.todo_button.set_action_name(Some("win.session-todo"));
                } else {
                    chip.todo_button.set_action_name(None);
                    chip.todo_button.set_action_target_value(None);
                }
            }
            None => {
                chip.todo_done.set_visible(false);
                chip.todo_done.set_action_name(None);
                chip.todo_done.set_action_target_value(None);
                chip.todo_button.set_action_name(None);
                chip.todo_button.set_action_target_value(None);
            }
        }
        if self.attention.get() {
            chip.widget.add_css_class("attention");
        } else {
            chip.widget.remove_css_class("attention");
        }
        match *self.activity.borrow() {
            Some(sign) => {
                chip.sign
                    .set_css_classes(&["activity-dot", sign.css_class()]);
                chip.sign.set_tooltip_text(Some(sign.label()));
                chip.sign.set_visible(true);
            }
            None => {
                chip.sign.set_css_classes(&["activity-dot"]);
                chip.sign.set_visible(false);
            }
        }
    }

    /// The header: the session's name and the to-do it corresponds to, with a
    /// split and close on the right. Rebuilt whenever the panel's tab changes.
    pub fn rebuild_header(&self) {
        while let Some(child) = self.header.first_child() {
            self.header.remove(&child);
        }
        *self.chip.borrow_mut() = None;
        let Some(key) = self.key() else {
            return;
        };

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        row.add_css_class("panel-chip");
        row.add_css_class("active");
        row.set_valign(gtk::Align::Center);
        row.set_hexpand(true);

        // The activity sign leads the header: a status light before the name.
        let sign = gtk::Label::new(Some("●"));
        sign.set_css_classes(&["activity-dot"]);
        sign.set_valign(gtk::Align::Center);
        sign.set_visible(false);
        row.append(&sign);

        // The session's name. It reuses the pane-info look so a bell still
        // accents it, as the live title did before.
        let session = gtk::Label::new(None);
        session.add_css_class("caption-heading");
        session.add_css_class("pane-info");
        session.set_ellipsize(gtk::pango::EllipsizeMode::End);
        session.set_xalign(0.0);
        session.set_visible(false);
        row.append(&session);

        // The board to-do the session works on, dim beside the name. It opens
        // the to-do — the same card view a cockpit to-do row opens.
        let todo = gtk::Label::new(None);
        todo.add_css_class("caption");
        todo.add_css_class("dim-label");
        todo.add_css_class("pane-info");
        todo.set_ellipsize(gtk::pango::EllipsizeMode::End);
        todo.set_xalign(0.0);
        let todo_button = gtk::Button::new();
        todo_button.add_css_class("flat");
        todo_button.add_css_class("panel-todo");
        todo_button.set_child(Some(&todo));
        todo_button.set_visible(false);
        row.append(&todo_button);

        // Marking the to-do done from right there — the same quiet-control
        // look as close. The card's panel hides when the store says done;
        // the program keeps running.
        let todo_done = gtk::Button::builder()
            .icon_name("object-select-symbolic")
            .tooltip_text("Mark this to-do done")
            .build();
        todo_done.add_css_class("chip-done");
        todo_done.set_valign(gtk::Align::Center);
        todo_done.set_visible(false);
        row.append(&todo_done);

        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        row.append(&spacer);

        for (label, zone) in [("Split right", "right"), ("Split below", "bottom")] {
            let split = gtk::Button::with_label(label);
            split.add_css_class("flat");
            split.set_action_name(Some("win.panel-split"));
            split.set_action_target_value(Some(&(key.as_str(), zone).to_variant()));
            row.append(&split);
        }

        // Closing hides the tab; its program keeps running, and the chooser or
        // the HUD brings it back.
        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text(format!("Close {} — its program keeps running", key.label()))
            .build();
        close.add_css_class("flat");
        close.add_css_class("chip-close");
        close.set_valign(gtk::Align::Center);
        close.set_action_name(Some("win.primitive-close"));
        close.set_action_target_value(Some(&key.as_str().to_variant()));
        row.append(&close);

        let hover = gtk::EventControllerMotion::new();
        let row_for_hover = row.clone();
        let target = key.as_str().to_variant();
        hover.connect_enter(move |_, _, _| {
            let _ = row_for_hover.activate_action("win.primitive-hover", Some(&target));
        });
        row.add_controller(hover);

        *self.chip.borrow_mut() = Some(Chip {
            widget: row.clone(),
            sign,
            session,
            todo_button,
            todo,
            todo_done,
        });
        self.header.append(&row);
        self.apply_labels();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_split_and_the_middle_swaps() {
        let dragged = TabKey::first(Slot::Diff);
        let target = TabKey::first(Slot::Agent);
        for zone in ["top", "bottom", "left", "right"] {
            assert_eq!(
                drop_intent(zone, dragged, target),
                DropIntent::Split,
                "an edge drop splits"
            );
        }
        assert_eq!(
            drop_intent("center", dragged, target),
            DropIntent::Swap,
            "a middle drop swaps places"
        );
    }

    #[test]
    fn dropping_a_panel_on_itself_does_nothing() {
        let agent = TabKey::first(Slot::Agent);
        for zone in ["top", "center", "right"] {
            assert_eq!(drop_intent(zone, agent, agent), DropIntent::Ignore);
        }
    }

    #[test]
    fn a_non_primitive_payload_is_ignored() {
        let target = TabKey::first(Slot::Agent);
        for zone in ["top", "center"] {
            assert_eq!(
                drop_intent(zone, TabKey::first(Slot::Custom), target),
                DropIntent::Ignore
            );
        }
    }

    #[test]
    fn edge_margins_light_the_half_the_zone_points_at() {
        // A 400x300 panel, inset 6px from the rim.
        assert_eq!(edge_margins("top", 400, 300, 6), (6, 6, 6, 150));
        assert_eq!(edge_margins("bottom", 400, 300, 6), (6, 6, 150, 6));
        assert_eq!(edge_margins("left", 400, 300, 6), (6, 200, 6, 6));
        assert_eq!(edge_margins("right", 400, 300, 6), (200, 6, 6, 6));
        assert_eq!(
            edge_margins("center", 400, 300, 6),
            (6, 6, 6, 6),
            "a centre drop swaps, so it lights the whole panel"
        );
    }
}
