//! A group: one header, one or more primitives underneath.
//!
//! A pane starts as a single primitive. Drag one header onto another and they
//! group under one header, with a small chip per member to switch between them —
//! the same gesture as dropping a view into a tab group elsewhere, but opt-in:
//! nothing is grouped unless you group it.
//!
//! Chips are draggable, and the header accepts a drop. Both sides talk to the
//! window through actions, so this module needs no reference back to the app.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::gdk;
use gtk::glib;

use super::icon_name;
use super::primitive::label_for;
use crate::db::Slot;

/// What a drop should do, decided by where it landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropIntent {
    /// Landed on a pane's header: join that pane, sharing its header.
    Group,
    /// Landed on a pane's body: this primitive gets a pane of its own.
    SplitOut,
    /// Nothing to do — dropping a primitive onto its own header, say.
    Ignore,
}

/// Where a drop landed.
pub fn drop_intent(
    over_header: bool,
    dragged: Slot,
    members: &[Slot],
    active: Option<Slot>,
) -> DropIntent {
    if dragged == Slot::Custom {
        return DropIntent::Ignore;
    }
    if over_header {
        // Joining this pane is pointless if it is already the primitive shown.
        if active == Some(dragged) {
            return DropIntent::Ignore;
        }
        return DropIntent::Group;
    }
    // On the body: a pane of its own. Already alone there, so nothing to do.
    if members == [dragged] {
        return DropIntent::Ignore;
    }
    DropIntent::SplitOut
}

/// Which part of a pane's body a drop landed on: the half the drop implies.
fn drop_zone(width: i32, height: i32, x: f64, y: f64) -> &'static str {
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

/// How far the edge indicator sits in from the pane's rim, so the pane's own
/// border stays visible under it.
const EDGE_PAD: i32 = 6;

/// The margins that carve the edge rectangle down to the half `zone` points
/// at — (left, right, top, bottom), inset by `pad`. A centre drop acts on the
/// right half, so it lights the right half too.
fn edge_margins(zone: &str, width: i32, height: i32, pad: i32) -> (i32, i32, i32, i32) {
    let (half_w, half_h) = (width / 2, height / 2);
    match zone {
        "top" => (pad, pad, pad, half_h),
        "bottom" => (pad, pad, half_h, pad),
        "left" => (pad, half_w, pad, pad),
        // right, and centre, which splits like right.
        _ => (half_w, pad, pad, pad),
    }
}

pub struct Group {
    /// The pane as the arrangement tree sees it: the body with the drop-edge
    /// indicator floating over it.
    pub widget: gtk::Overlay,
    /// The pane proper: header, separator, content.
    body: gtk::Box,
    pub header: gtk::Box,
    pub content: gtk::Stack,
    pub menu_button: gtk::MenuButton,
    /// The rounded tint that covers the half a body drop would hand to the
    /// dropped pane. Invisible until a drag hovers.
    edge: gtk::Box,
    /// Members, in header order.
    pub members: RefCell<Vec<Slot>>,
    /// The member whose widget is showing.
    pub active: RefCell<Option<Slot>>,
    /// The header's chips, so a drag can carry the chip that was grabbed rather
    /// than always the active member.
    chips: RefCell<Vec<(Slot, gtk::Button)>>,
}

impl Group {
    pub fn new() -> Rc<Group> {
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        header.add_css_class("group-header");
        header.set_margin_start(6);
        header.set_margin_end(4);
        header.set_margin_top(2);
        header.set_margin_bottom(2);

        let menu_button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("Pane menu")
            .build();
        menu_button.add_css_class("flat");
        menu_button.set_valign(gtk::Align::Center);
        menu_button.set_halign(gtk::Align::End);
        menu_button.set_hexpand(true);

        let content = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vexpand(true)
            .hexpand(true)
            .build();

        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.set_vexpand(true);
        body.set_hexpand(true);
        body.append(&header);
        body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        body.append(&content);

        // The per-edge drop indicator: a tint over exactly the half a body
        // drop would hand to the dropped pane, so the split the release would
        // make is readable before it happens. Deaf to input — it must never
        // intercept the drag it is announcing.
        let edge = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        edge.add_css_class("drop-edge");
        edge.set_halign(gtk::Align::Fill);
        edge.set_valign(gtk::Align::Fill);
        edge.set_can_target(false);
        edge.set_visible(false);

        let widget = gtk::Overlay::new();
        widget.add_css_class("group-pane");
        widget.set_child(Some(&body));
        widget.add_overlay(&edge);

        // A secondary click anywhere in a pane opens the same menu as the
        // three-dot button and the Menu key.
        let context_menu = menu_button.clone();
        let context_click = gtk::GestureClick::new();
        context_click.set_button(3);
        context_click.set_propagation_phase(gtk::PropagationPhase::Capture);
        context_click.connect_pressed(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            context_menu.grab_focus();
            context_menu.popup();
        });
        widget.add_controller(context_click);

        let group = Rc::new(Group {
            widget,
            body,
            header,
            content,
            menu_button,
            edge,
            members: RefCell::new(Vec::new()),
            active: RefCell::new(None),
            chips: RefCell::new(Vec::new()),
        });
        Group::accept_drags(&group);
        group
    }

    /// The header drags; the whole pane accepts drops.
    ///
    /// One target covers the entire pane — header and body — because a 22 pixel
    /// header is a hard thing to hit, and because a terminal widget has its own
    /// drop target that would otherwise take the drop. It runs in the capture
    /// phase so this one always sees the event first.
    ///
    /// Dropping another pane here joins them; dropping one of *this* pane's own
    /// primitives here pulls it out into a pane of its own.
    fn accept_drags(group: &Rc<Group>) {
        use gtk::PropagationPhase;

        // Hovering a pane marks it as the pointer-active panel without moving
        // GTK keyboard focus away from the running primitive.
        let hover = gtk::EventControllerMotion::new();
        let pane_on_enter = group.widget.clone();
        hover.connect_enter(move |_, _, _| pane_on_enter.add_css_class("pointer-hover"));
        let pane_on_leave = group.widget.clone();
        hover.connect_leave(move |_| pane_on_leave.remove_css_class("pointer-hover"));
        group.widget.add_controller(hover);

        // ---- drag a pane by its header ----
        //
        // Capture phase: the header is full of buttons (chips, the menu), and a
        // button claims the press a drag needs. Seeing the press first means the
        // drag starts no matter which child is under the pointer.
        let source = gtk::DragSource::builder()
            .actions(gdk::DragAction::MOVE)
            .build();
        source.set_propagation_phase(gtk::PropagationPhase::Capture);
        let group_for_drag = group.clone();
        source.connect_prepare(move |_, x, y| {
            // The chip under the pointer, if any: grabbing "Changes" out of an
            // agent+changes header must drag Changes, not whatever is showing.
            // Coordinates and chip allocations are both relative to the header.
            let grabbed = group_for_drag.chips.borrow().iter().find_map(|(slot, chip)| {
                let alloc = chip.allocation();
                (x >= alloc.x() as f64
                    && x < (alloc.x() + alloc.width()) as f64
                    && y >= alloc.y() as f64
                    && y < (alloc.y() + alloc.height()) as f64)
                    .then_some(*slot)
            });
            let slot = grabbed.or_else(|| group_for_drag.active_slot());
            super::trace(&format!(
                "drag: prepare at ({x:.0},{y:.0}) grabbed={:?} active={:?}",
                grabbed.map(|slot| slot.as_str()),
                group_for_drag.active_slot().map(|slot| slot.as_str())
            ));
            slot.map(|slot| gdk::ContentProvider::for_value(&slot.as_str().to_value()))
        });
        let header_for_drag = group.header.clone();
        source.connect_drag_begin(move |_, _| {
            header_for_drag.add_css_class("dragging");
            super::trace("drag: began");
        });
        let header_for_end = group.header.clone();
        source.connect_drag_end(move |_, _, committed| {
            header_for_end.remove_css_class("dragging");
            super::trace(&format!("drag: ended (committed: {committed})"));
        });
        group.header.add_controller(source);

        // ---- the whole pane accepts a drop ----
        //
        // One target for the entire pane, because a header is only ~22px tall and
        // a terminal widget has its own drop target for text that would otherwise
        // take the drop. Where you release decides what happens: the header joins
        // panes, the body splits one out.
        let target = gtk::DropTarget::new(glib::types::Type::STRING, gdk::DragAction::MOVE);
        target.set_propagation_phase(PropagationPhase::Capture);
        target.connect_accept(|_, drag| drag.actions().contains(gdk::DragAction::MOVE));

        let header_for_highlight = group.header.clone();
        let body_for_highlight = group.body.clone();
        let edge_for_highlight = group.edge.clone();
        target.connect_motion(move |_, x, y| {
            // Light up the half the drop would act on, so the gesture is
            // readable before releasing: the header tints for a join, one half
            // of the pane for a split.
            header_for_highlight.remove_css_class("drop-header");
            edge_for_highlight.set_visible(false);
            let over_header = y <= header_for_highlight.height() as f64 + 2.0;
            if over_header {
                header_for_highlight.add_css_class("drop-header");
            } else {
                let (w, h) = (body_for_highlight.width(), body_for_highlight.height());
                let (left, right, top, bottom) =
                    edge_margins(drop_zone(w, h, x, y), w, h, EDGE_PAD);
                edge_for_highlight.set_margin_start(left);
                edge_for_highlight.set_margin_end(right);
                edge_for_highlight.set_margin_top(top);
                edge_for_highlight.set_margin_bottom(bottom);
                edge_for_highlight.set_visible(true);
            }
            gdk::DragAction::MOVE
        });
        let header_for_unhighlight = group.header.clone();
        let edge_for_unhighlight = group.edge.clone();
        target.connect_leave(move |_| {
            edge_for_unhighlight.set_visible(false);
            header_for_unhighlight.remove_css_class("drop-header");
        });

        let widget = group.widget.clone();
        let header = group.header.clone();
        let edge = group.edge.clone();
        let group_for_drop = group.clone();
        target.connect_drop(move |_, value, x, y| {
            let Ok(payload) = value.get::<String>() else {
                super::trace("drop: payload was not a string");
                return false;
            };
            let dragged = Slot::parse(&payload);
            let over_header = y <= header.height() as f64 + 2.0;
            let members = group_for_drop.slots();
            let intent = drop_intent(over_header, dragged, &members, group_for_drop.active_slot());
            edge.set_visible(false);
            header.remove_css_class("drop-header");
            super::trace(&format!(
                "drop: payload={payload} x={x:.0} y={y:.0} over_header={over_header} members={members:?} -> {intent:?}"
            ));
            match intent {
                DropIntent::Ignore => false,
                DropIntent::Group => {
                    let Some(target_slot) = group_for_drop.active_slot() else {
                        return false;
                    };
                    // Full action name, prefix included: without "win." the lookup
                    // silently fails and the drop does nothing.
                    // Deferred one main-loop turn: relayouting while the drop is
                    // still finishing leaves the rebuilt tree unallocated (0x0)
                    // — every pane gone, only the sidebar left. After the idle
                    // the drag is fully over and the relayout sticks.
                    let widget = widget.clone();
                    let variant =
                        (payload, target_slot.as_str().to_string()).to_variant();
                    glib::idle_add_local_once(move || {
                        let _ = widget.activate_action("win.primitive-group", Some(&variant));
                    });
                    true
                }
                DropIntent::SplitOut => {
                    let widget = widget.clone();
                    if members.contains(&dragged) {
                        // This pane's own member, back on its body: out it
                        // comes into a pane of its own.
                        let variant = payload.to_variant();
                        glib::idle_add_local_once(move || {
                            let _ = widget
                                .activate_action("win.primitive-split-out", Some(&variant));
                        });
                    } else {
                        // Another pane dropped on this body: the body's region
                        // divides and the visitor takes the dropped half.
                        let Some(target_slot) = group_for_drop.active_slot() else {
                            return false;
                        };
                        let zone = drop_zone(widget.width(), widget.height(), x, y);
                        let variant = (
                            payload,
                            target_slot.as_str().to_string(),
                            zone.to_string(),
                        )
                            .to_variant();
                        glib::idle_add_local_once(move || {
                            let _ = widget
                                .activate_action("win.pane-nest-split", Some(&variant));
                        });
                    }
                    true
                }
            }
        });
        group.widget.add_controller(target);
    }

    /// Show a member's widget, adding it if this group has not seen it before.
    pub fn insert(&self, slot: Slot, widget: &gtk::Widget, activate: bool) {
        // A primitive's widget always has a parent — its old pane's stack — and
        // GTK refuses, silently, to add a widget that already has one. Detach it
        // first or the pane comes up empty.
        if widget.parent().is_some() {
            widget.unparent();
        }
        let name = slot.as_str();
        if self.content.child_by_name(name).is_none() {
            self.content.add_named(widget, Some(name));
        }
        if !self.members.borrow().contains(&slot) {
            self.members.borrow_mut().push(slot);
        }
        if activate || self.active.borrow().is_none() {
            *self.active.borrow_mut() = Some(slot);
        }
        self.content
            .set_visible_child_name(self.active.borrow().unwrap_or(slot).as_str());
    }

    /// Take a member out of this group. Returns true when the group is empty.
    pub fn remove(&self, slot: Slot) -> bool {
        self.members.borrow_mut().retain(|other| *other != slot);
        if let Some(widget) = self.content.child_by_name(slot.as_str()) {
            self.content.remove(&widget);
        }
        if self.active.borrow().as_ref() == Some(&slot) {
            *self.active.borrow_mut() = self.members.borrow().first().copied();
        }
        if let Some(active) = *self.active.borrow() {
            self.content.set_visible_child_name(active.as_str());
        }
        self.is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.members.borrow().is_empty()
    }

    pub fn contains(&self, slot: Slot) -> bool {
        self.members.borrow().contains(&slot)
    }

    pub fn slots(&self) -> Vec<Slot> {
        self.members.borrow().clone()
    }

    /// Switch to a member.
    pub fn activate(&self, slot: Slot) {
        if !self.contains(slot) {
            return;
        }
        *self.active.borrow_mut() = Some(slot);
        self.content.set_visible_child_name(slot.as_str());
        for (member, chip) in self.chips.borrow().iter() {
            if *member == slot {
                chip.add_css_class("active");
            } else {
                chip.remove_css_class("active");
            }
        }
    }

    /// The chip whose button (or child) currently holds keyboard focus.
    pub fn slot_for_focus(&self, focus: &gtk::Widget) -> Option<Slot> {
        self.chips.borrow().iter().find_map(|(slot, chip)| {
            (focus == chip.upcast_ref::<gtk::Widget>() || focus.is_ancestor(chip)).then_some(*slot)
        })
    }

    pub fn active_slot(&self) -> Option<Slot> {
        self.active.borrow().or_else(|| self.members.borrow().first().copied())
    }

    /// A chip per member, showing which one is active.
    pub fn rebuild_header(&self) {
        while let Some(child) = self.header.first_child() {
            self.header.remove(&child);
        }
        self.chips.borrow_mut().clear();
        let active = self.active_slot();
        for slot in self.members.borrow().iter() {
            let chip = gtk::Button::builder()
                .tooltip_text(format!("{}\tclick to switch — drag the header to move it", label_for(*slot)))
                .build();
            chip.add_css_class("flat");
            chip.add_css_class("group-chip");
            if Some(*slot) == active {
                chip.add_css_class("active");
            }

            let inner = gtk::Box::new(gtk::Orientation::Horizontal, 5);
            inner.append(&gtk::Image::from_icon_name(icon_name(*slot)));
            let label = gtk::Label::new(Some(label_for(*slot)));
            label.add_css_class("caption-heading");
            inner.append(&label);
            chip.set_child(Some(&inner));
            chip.set_action_name(Some("win.primitive-activate"));
            chip.set_action_target_value(Some(&slot.as_str().to_variant()));

            let hover = gtk::EventControllerMotion::new();
            let chip_for_hover = chip.clone();
            let target = slot.as_str().to_variant();
            hover.connect_enter(move |_, _, _| {
                let _ = chip_for_hover.activate_action("win.primitive-hover", Some(&target));
            });
            chip.add_controller(hover);

            self.chips.borrow_mut().push((*slot, chip.clone()));
            self.header.append(&chip);
        }
        self.header.append(&self.menu_button);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_drops_group_and_body_drops_split() {
        let members = [Slot::Agent];
        assert_eq!(
            drop_intent(true, Slot::Diff, &members, Some(Slot::Agent)),
            DropIntent::Group,
            "onto another pane's header: join it"
        );
        assert_eq!(
            drop_intent(false, Slot::Diff, &members, Some(Slot::Agent)),
            DropIntent::SplitOut,
            "onto another pane's body: a pane of its own"
        );
    }

    #[test]
    fn dropping_a_primitive_on_its_own_pane_does_nothing() {
        assert_eq!(
            drop_intent(true, Slot::Agent, &[Slot::Agent], Some(Slot::Agent)),
            DropIntent::Ignore
        );
        assert_eq!(
            drop_intent(false, Slot::Agent, &[Slot::Agent], Some(Slot::Agent)),
            DropIntent::Ignore
        );
        assert_eq!(
            drop_intent(
                false,
                Slot::Diff,
                &[Slot::Agent, Slot::Diff],
                Some(Slot::Agent)
            ),
            DropIntent::SplitOut,
            "a member of this pane, dropped on the body: out it comes"
        );
    }

    #[test]
    fn a_non_primitive_payload_is_ignored() {
        assert_eq!(
            drop_intent(true, Slot::Custom, &[Slot::Agent], Some(Slot::Agent)),
            DropIntent::Ignore
        );
        assert_eq!(
            drop_intent(false, Slot::Custom, &[Slot::Agent], Some(Slot::Agent)),
            DropIntent::Ignore
        );
    }

    #[test]
    fn edge_margins_light_the_half_the_zone_points_at() {
        // A 400x300 pane, inset 6px from the rim.
        assert_eq!(edge_margins("top", 400, 300, 6), (6, 6, 6, 150));
        assert_eq!(edge_margins("bottom", 400, 300, 6), (6, 6, 150, 6));
        assert_eq!(edge_margins("left", 400, 300, 6), (6, 200, 6, 6));
        assert_eq!(edge_margins("right", 400, 300, 6), (200, 6, 6, 6));
        assert_eq!(
            edge_margins("center", 400, 300, 6),
            edge_margins("right", 400, 300, 6),
            "centre splits like right, so it lights like right"
        );
    }
}
