//! A group: one header, one or more tabs underneath.
//!
//! A pane starts as a single tab. Drag one header onto another and they group
//! under one header, with a small chip per tab to switch between them — the
//! same gesture as dropping a view into a tab group elsewhere, but opt-in:
//! nothing is grouped unless you group it.
//!
//! Chips are draggable, and the header accepts a drop. Both sides talk to the
//! window through actions, so this module needs no reference back to the app.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use gtk::gdk;
use gtk::glib;

use super::icon_name;
use super::primitive::label_for;
use crate::db::{Slot, TabKey};

/// What a drop should do, decided by where it landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropIntent {
    /// Landed on a pane's header: join that pane, sharing its header.
    Group,
    /// Landed on a pane's body: this tab gets a pane of its own.
    SplitOut,
    /// Nothing to do — dropping a tab onto its own header, say.
    Ignore,
}

/// Where a drop landed.
pub fn drop_intent(
    over_header: bool,
    dragged: TabKey,
    members: &[TabKey],
    active: Option<TabKey>,
) -> DropIntent {
    if dragged.slot == Slot::Custom {
        return DropIntent::Ignore;
    }
    if over_header {
        // Joining this pane is pointless if it is already the tab shown.
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

/// Whose widgets a pane's body shows. Radar draws only the Board's body —
/// the sidebar, settings and onboarding are not panes at all. Every other
/// slot's body is a terminal running a program radar does not own, so the
/// right click there is the program's: radar's pane menu keeps to the
/// three-dot button and the Menu key.
fn radar_draws(slot: Slot) -> bool {
    slot == Slot::Board
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

/// One tab's controls in the header. A chip is a small panel header: the
/// switch that shows it, its program's live info, a program dropdown, a ＋
/// that adds another tab of the same primitive, and a close of its own. A
/// header carries several only when tabs are grouped.
struct Chip {
    /// The cluster as one unit — main, dropdown, ＋ and close — which is
    /// what drags hit-test and focus lookups walk.
    widget: gtk::Box,
    /// The tab's live info, dim, beside its label; accented while it wants
    /// attention.
    info: gtk::Label,
    /// The inline program dropdown. Its model is attached by the window
    /// (`refresh_group_menu`), which knows the registry and what runs now.
    program_button: gtk::MenuButton,
    /// The ＋: add another tab of this same primitive, grouped under this
    /// header. Its model is attached by the window too — the same list of
    /// this kind's programs, each item adding a tab that runs it.
    add_button: gtk::MenuButton,
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
    pub members: RefCell<Vec<TabKey>>,
    /// The member whose widget is showing.
    pub active: RefCell<Option<TabKey>>,
    /// The header's chips, so a drag can carry the chip that was grabbed rather
    /// than always the active member.
    chips: RefCell<Vec<(TabKey, Chip)>>,
    /// Live info each member's program reports — its name, and whatever the
    /// program says it is doing — keyed by member.
    info: RefCell<HashMap<TabKey, String>>,
    /// Members that rang the terminal bell and have not been looked at since.
    attention: RefCell<HashSet<TabKey>>,
}

impl Group {
    pub fn new() -> Rc<Group> {
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        // The strip spans the pane's full width; chips are inset by CSS
        // padding so the surface itself reaches the pane's edges.
        header.add_css_class("group-header");

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
            info: RefCell::new(HashMap::new()),
            attention: RefCell::new(HashSet::new()),
        });
        Group::accept_drags(&group);
        Group::wire_context_menu(&group);
        group
    }

    /// A secondary click opens the same menu as the three-dot button and the
    /// Menu key — but at the pointer, like any context menu, not anchored up
    /// at the button. Radar's surfaces only: where radar draws what you are
    /// clicking on (the Board pane; the sidebar, settings and onboarding are
    /// no panes and carry no interception at all) the menu is radar's to
    /// open. Every other pane body is a terminal running somebody else's
    /// program, and the right click there belongs to that program — VTE hands
    /// button 3 to whatever asked for mouse events. The gesture steps aside
    /// so the press reaches the terminal untouched.
    fn wire_context_menu(group: &Rc<Group>) {
        let context_menu = group.menu_button.clone();
        let menu_origin = group.widget.clone();
        let group_for_menu = group.clone();
        let pointing_wired = std::cell::Cell::new(false);
        let context_click = gtk::GestureClick::new();
        context_click.set_button(3);
        context_click.set_propagation_phase(gtk::PropagationPhase::Capture);
        context_click.connect_pressed(move |gesture, _, x, y| {
            // The visible tab decides whose click this is. A terminal tab —
            // any slot but the Board — lets the press through unclaimed.
            let Some(key) = group_for_menu.active_key() else {
                return;
            };
            if !radar_draws(key.slot) {
                return;
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
            context_menu.grab_focus();
            // Point the popover at the pointer. The rect is in the menu
            // button's coordinate space, because the button parents the
            // popover — and the popover only exists once a menu model has
            // been set. If either widget is not on screen yet, fall through
            // and the menu opens at the button as before.
            if let Some(popover) = context_menu.popover() {
                if let Some((x, y)) = menu_origin.translate_coordinates(&context_menu, x, y) {
                    popover.set_pointing_to(Some(&gdk::Rectangle::new(
                        x.round() as i32,
                        y.round() as i32,
                        1,
                        1,
                    )));
                    if !pointing_wired.get() {
                        pointing_wired.set(true);
                        // Hand the anchor back once the menu closes, so the
                        // button itself and the Menu key open at the button.
                        popover.connect_closed(|popover| popover.set_pointing_to(None));
                    }
                }
            }
            context_menu.popup();
        });
        group.widget.add_controller(context_click);
    }

    /// The header drags; the whole pane accepts drops.
    ///
    /// One target covers the entire pane — header and body — because a 22 pixel
    /// header is a hard thing to hit, and because a terminal widget has its own
    /// drop target that would otherwise take the drop. It runs in the capture
    /// phase so this one always sees the event first.
    ///
    /// Dropping another pane here joins them; dropping one of *this* pane's own
    /// tabs here pulls it out into a pane of its own.
    fn accept_drags(group: &Rc<Group>) {
        use gtk::PropagationPhase;

        // Hovering a pane is focusing it: the motion controller forwards the
        // enter to `hover_primitive`, which (only for a pointer that is really
        // moving) focuses the pane's tab — and the ring that follows
        // keyboard focus marks it. No separate hover highlight.
        let hover = gtk::EventControllerMotion::new();
        hover.set_propagation_phase(gtk::PropagationPhase::Capture);
        let group_on_enter = group.clone();
        hover.connect_enter(move |_, _, _| {
            if let Some(key) = group_on_enter.active_key() {
                let _ = group_on_enter
                    .widget
                    .activate_action("win.primitive-hover", Some(&key.as_str().to_variant()));
            }
        });
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
            let grabbed = group_for_drag
                .chips
                .borrow()
                .iter()
                .find_map(|(key, chip)| {
                    let alloc = chip.widget.allocation();
                    (x >= alloc.x() as f64
                        && x < (alloc.x() + alloc.width()) as f64
                        && y >= alloc.y() as f64
                        && y < (alloc.y() + alloc.height()) as f64)
                        .then_some(*key)
                });
            let key = grabbed.or_else(|| group_for_drag.active_key());
            super::trace(&format!(
                "drag: prepare at ({x:.0},{y:.0}) grabbed={:?} active={:?}",
                grabbed.map(|key| key.as_str()),
                group_for_drag.active_key().map(|key| key.as_str())
            ));
            key.map(|key| gdk::ContentProvider::for_value(&key.as_str().to_value()))
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
            let dragged = TabKey::parse(&payload);
            let over_header = y <= header.height() as f64 + 2.0;
            let members = group_for_drop.tabs();
            let intent = drop_intent(over_header, dragged, &members, group_for_drop.active_key());
            edge.set_visible(false);
            header.remove_css_class("drop-header");
            super::trace(&format!(
                "drop: payload={payload} x={x:.0} y={y:.0} over_header={over_header} members={members:?} -> {intent:?}"
            ));
            match intent {
                DropIntent::Ignore => false,
                DropIntent::Group => {
                    let Some(target_key) = group_for_drop.active_key() else {
                        return false;
                    };
                    // Full action name, prefix included: without "win." the lookup
                    // silently fails and the drop does nothing.
                    // Deferred one main-loop turn: relayouting while the drop is
                    // still finishing leaves the rebuilt tree unallocated (0x0)
                    // — every pane gone, only the sidebar left. After the idle
                    // the drag is fully over and the relayout sticks.
                    let widget = widget.clone();
                    let variant = (dragged.as_str(), target_key.as_str()).to_variant();
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
                        let variant = dragged.as_str().to_variant();
                        glib::idle_add_local_once(move || {
                            let _ = widget
                                .activate_action("win.primitive-split-out", Some(&variant));
                        });
                    } else {
                        // Another pane dropped on this body: the body's region
                        // divides and the visitor takes the dropped half.
                        let Some(target_key) = group_for_drop.active_key() else {
                            return false;
                        };
                        let zone = drop_zone(widget.width(), widget.height(), x, y);
                        let variant = (
                            dragged.as_str(),
                            target_key.as_str(),
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
    pub fn insert(&self, key: TabKey, widget: &gtk::Widget, activate: bool) {
        // A primitive's widget always has a parent — its old pane's stack — and
        // GTK refuses, silently, to add a widget that already has one. Detach it
        // first or the pane comes up empty.
        if widget.parent().is_some() {
            widget.unparent();
        }
        let name = key.as_str();
        if self.content.child_by_name(&name).is_none() {
            self.content.add_named(widget, Some(&name));
        }
        if !self.members.borrow().contains(&key) {
            self.members.borrow_mut().push(key);
        }
        if activate || self.active.borrow().is_none() {
            *self.active.borrow_mut() = Some(key);
        }
        self.content
            .set_visible_child_name(self.active.borrow().unwrap_or(key).as_str().as_str());
    }

    /// Take a member out of this group. Returns true when the group is empty.
    pub fn remove(&self, key: TabKey) -> bool {
        self.members.borrow_mut().retain(|other| *other != key);
        if let Some(widget) = self.content.child_by_name(key.as_str().as_str()) {
            self.content.remove(&widget);
        }
        if self.active.borrow().as_ref() == Some(&key) {
            *self.active.borrow_mut() = self.members.borrow().first().copied();
        }
        if let Some(active) = *self.active.borrow() {
            self.content
                .set_visible_child_name(active.as_str().as_str());
        }
        self.info.borrow_mut().remove(&key);
        self.attention.borrow_mut().remove(&key);
        self.apply_info();
        self.is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.members.borrow().is_empty()
    }

    pub fn contains(&self, key: TabKey) -> bool {
        self.members.borrow().contains(&key)
    }

    pub fn tabs(&self) -> Vec<TabKey> {
        self.members.borrow().clone()
    }

    /// Switch to a member.
    pub fn activate(&self, key: TabKey) {
        if !self.contains(key) {
            return;
        }
        *self.active.borrow_mut() = Some(key);
        self.content.set_visible_child_name(key.as_str().as_str());
        // Looking at a member answers its bell; apply_info also repaints the
        // chips' active and attention marks.
        self.attention.borrow_mut().remove(&key);
        self.apply_info();
    }

    /// The chip whose controls currently hold keyboard focus.
    pub fn key_for_focus(&self, focus: &gtk::Widget) -> Option<TabKey> {
        self.chips.borrow().iter().find_map(|(key, chip)| {
            (focus == chip.widget.upcast_ref::<gtk::Widget>() || focus.is_ancestor(&chip.widget))
                .then_some(*key)
        })
    }

    pub fn active_key(&self) -> Option<TabKey> {
        self.active
            .borrow()
            .or_else(|| self.members.borrow().first().copied())
    }

    /// Live info a member's program reported — its name, its own title, its
    /// exit. Empty text clears: the chip goes back to just the label.
    pub fn set_member_info(&self, key: TabKey, text: &str) {
        if text.is_empty() {
            self.info.borrow_mut().remove(&key);
        } else {
            self.info.borrow_mut().insert(key, text.to_string());
        }
        if self.active_key() == Some(key) {
            self.apply_info();
        }
    }

    /// A member rang the terminal bell — an agent asking for attention. The
    /// header marks it until that member is looked at.
    pub fn set_attention(&self, key: TabKey) {
        self.attention.borrow_mut().insert(key);
        if self.active_key() == Some(key) {
            self.apply_info();
        }
    }

    /// Push each member's info onto its own chip: live text beside the label,
    /// dim until the program wants attention. A chip with nothing to say is
    /// just its label, as before.
    fn apply_info(&self) {
        let active = self.active_key();
        for (key, chip) in self.chips.borrow().iter() {
            let text = self.info.borrow().get(key).cloned();
            chip.info.set_visible(text.is_some());
            chip.info.set_text(text.as_deref().unwrap_or(""));
            chip.info.set_tooltip_text(text.as_deref());
            if active == Some(*key) {
                chip.widget.add_css_class("active");
            } else {
                chip.widget.remove_css_class("active");
            }
            if self.attention.borrow().contains(key) {
                chip.widget.add_css_class("attention");
            } else {
                chip.widget.remove_css_class("attention");
            }
        }
    }

    /// A chip per member, each a small panel header of its own: switch, live
    /// info, program dropdown, ＋, close. The header ends with the pane menu.
    pub fn rebuild_header(&self) {
        while let Some(child) = self.header.first_child() {
            self.header.remove(&child);
        }
        self.chips.borrow_mut().clear();
        let active = self.active_key();
        for key in self.members.borrow().iter() {
            let cluster = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            cluster.add_css_class("group-chip");
            cluster.set_valign(gtk::Align::Center);
            if Some(*key) == active {
                cluster.add_css_class("active");
            }

            // The switch: icon, name, and whatever the program says it is
            // doing. Still a button under the plain-text look, so click,
            // hover and keyboard focus all work.
            let main = gtk::Button::builder()
                .tooltip_text(format!(
                    "{}\tclick to switch — drag the header to move it",
                    key.label()
                ))
                .build();
            main.add_css_class("flat");
            main.add_css_class("chip-main");

            let inner = gtk::Box::new(gtk::Orientation::Horizontal, 5);
            let image = gtk::Image::from_icon_name(icon_name(key.slot));
            // Chips are the most compact headers: their icons follow the
            // small caption text.
            image.set_pixel_size(14);
            inner.append(&image);
            let label = gtk::Label::new(Some(&key.label()));
            label.add_css_class("caption-heading");
            inner.append(&label);
            let info = gtk::Label::new(None);
            info.add_css_class("dim-label");
            info.add_css_class("caption");
            info.add_css_class("pane-info");
            info.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            info.set_visible(false);
            inner.append(&info);
            main.set_child(Some(&inner));
            main.set_action_name(Some("win.primitive-activate"));
            main.set_action_target_value(Some(&key.as_str().to_variant()));
            cluster.append(&main);

            // This tab's program dropdown. The window attaches the model —
            // it knows the registry and which program runs — on the next
            // refresh after this rebuild. Same primitive kind only: an agent
            // chip lists agents, an editor chip lists editors.
            let program_button = gtk::MenuButton::builder()
                .icon_name("pan-down-symbolic")
                .tooltip_text(format!("Change {} program…", key.label()))
                .build();
            program_button.add_css_class("flat");
            program_button.add_css_class("chip-menu");
            program_button.set_valign(gtk::Align::Center);
            cluster.append(&program_button);

            // The ＋: another tab of this same primitive, grouped under this
            // header as a new chip. Same list of this kind's programs; the
            // one the new tab runs is the pick.
            let add_button = gtk::MenuButton::builder()
                .icon_name("list-add-symbolic")
                .tooltip_text(format!("Add another {} tab", label_for(key.slot)))
                .build();
            add_button.add_css_class("flat");
            add_button.add_css_class("chip-add");
            add_button.set_valign(gtk::Align::Center);
            cluster.append(&add_button);

            // Closing the chip hides the tab; its program keeps running,
            // and the dock or the HUD brings it back. The last member to
            // close takes the pane with it.
            let close = gtk::Button::builder()
                .icon_name("window-close-symbolic")
                .tooltip_text(format!("Close {} — its program keeps running", key.label()))
                .build();
            close.add_css_class("flat");
            close.add_css_class("chip-close");
            close.set_valign(gtk::Align::Center);
            close.set_action_name(Some("win.primitive-close"));
            close.set_action_target_value(Some(&key.as_str().to_variant()));
            cluster.append(&close);

            let hover = gtk::EventControllerMotion::new();
            let chip_for_hover = cluster.clone();
            let target = key.as_str().to_variant();
            hover.connect_enter(move |_, _, _| {
                let _ = chip_for_hover.activate_action("win.primitive-hover", Some(&target));
            });
            cluster.add_controller(hover);

            self.chips.borrow_mut().push((
                *key,
                Chip {
                    widget: cluster.clone(),
                    info,
                    program_button,
                    add_button,
                },
            ));
            self.header.append(&cluster);
        }
        self.header.append(&self.menu_button);
        self.apply_info();
    }

    /// The chips' program dropdowns and ＋ menus, for the window to attach
    /// models to — it knows the registry, the preferences and what each tab
    /// runs.
    pub fn chip_controls(&self) -> Vec<(TabKey, gtk::MenuButton, gtk::MenuButton)> {
        self.chips
            .borrow()
            .iter()
            .map(|(key, chip)| (*key, chip.program_button.clone(), chip.add_button.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_drops_group_and_body_drops_split() {
        let members = [TabKey::first(Slot::Agent)];
        assert_eq!(
            drop_intent(
                true,
                TabKey::first(Slot::Diff),
                &members,
                Some(TabKey::first(Slot::Agent))
            ),
            DropIntent::Group,
            "onto another pane's header: join it"
        );
        assert_eq!(
            drop_intent(
                false,
                TabKey::first(Slot::Diff),
                &members,
                Some(TabKey::first(Slot::Agent))
            ),
            DropIntent::SplitOut,
            "onto another pane's body: a pane of its own"
        );
    }

    #[test]
    fn dropping_a_primitive_on_its_own_pane_does_nothing() {
        assert_eq!(
            drop_intent(
                true,
                TabKey::first(Slot::Agent),
                &[TabKey::first(Slot::Agent)],
                Some(TabKey::first(Slot::Agent))
            ),
            DropIntent::Ignore
        );
        assert_eq!(
            drop_intent(
                false,
                TabKey::first(Slot::Agent),
                &[TabKey::first(Slot::Agent)],
                Some(TabKey::first(Slot::Agent))
            ),
            DropIntent::Ignore
        );
        assert_eq!(
            drop_intent(
                false,
                TabKey::first(Slot::Diff),
                &[TabKey::first(Slot::Agent), TabKey::first(Slot::Diff)],
                Some(TabKey::first(Slot::Agent))
            ),
            DropIntent::SplitOut,
            "a member of this pane, dropped on the body: out it comes"
        );
    }

    #[test]
    fn a_non_primitive_payload_is_ignored() {
        assert_eq!(
            drop_intent(
                true,
                TabKey::first(Slot::Custom),
                &[TabKey::first(Slot::Agent)],
                Some(TabKey::first(Slot::Agent))
            ),
            DropIntent::Ignore
        );
        assert_eq!(
            drop_intent(
                false,
                TabKey::first(Slot::Custom),
                &[TabKey::first(Slot::Agent)],
                Some(TabKey::first(Slot::Agent))
            ),
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

    #[test]
    fn the_right_click_belongs_to_radar_only_on_radar_drawn_panes() {
        // The board is radar's own widgets: the pane menu keeps the click.
        assert!(radar_draws(Slot::Board));
        // Every other slot's body is a terminal running a program radar does
        // not own — its right click goes to that program.
        for slot in [
            Slot::Editor,
            Slot::Agent,
            Slot::Diff,
            Slot::Shell,
            Slot::Custom,
        ] {
            assert!(
                !radar_draws(slot),
                "{} panes run a program, not radar's widgets",
                slot.as_str()
            );
        }
    }
}
