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

use super::primitive::{icon_name, label_for};
use crate::db::Slot;

/// Start dragging a chip, carrying the primitive's slot as a string.
pub const DRAG_TYPE: &str = "application/x-radar-primitive";

pub struct Group {
    pub widget: gtk::Box,
    pub header: gtk::Box,
    pub content: gtk::Stack,
    pub menu_button: gtk::MenuButton,
    /// Members, in header order.
    pub members: RefCell<Vec<Slot>>,
    /// The member whose widget is showing.
    pub active: RefCell<Option<Slot>>,
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

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.set_vexpand(true);
        widget.set_hexpand(true);
        widget.append(&header);
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        widget.append(&content);

        let group = Rc::new(Group {
            widget,
            header,
            content,
            menu_button,
            members: RefCell::new(Vec::new()),
            active: RefCell::new(None),
        });
        Group::accept_drops(&group);
        group
    }

    /// The header is a drop target: dropping a chip here joins the two.
    fn accept_drops(group: &Rc<Group>) {
        let target = gtk::DropTarget::new(glib::types::Type::STRING, gdk::DragAction::MOVE);
        target.set_types(&[glib::types::Type::STRING]);
        let header = group.header.clone();
        let group_for_drop = group.clone();
        target.connect_drop(move |_, value, _, _| {
            let Ok(source) = value.get::<String>() else {
                return false;
            };
            let Some(target_slot) = group_for_drop
                .active
                .borrow()
                .or_else(|| group_for_drop.members.borrow().first().copied())
            else {
                return false;
            };
            if source == target_slot.as_str() {
                return false;
            }
            // Through the window action, so nothing here needs the app.
            header.activate_action(
                "primitive-group",
                Some(&(source, target_slot.as_str().to_string()).to_variant()),
            );
            true
        });
        group.header.add_controller(target);
    }

    /// Show a member's widget, adding it if this group has not seen it before.
    pub fn insert(&self, slot: Slot, widget: &gtk::Widget, activate: bool) {
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
        self.rebuild_header();
    }

    pub fn active_slot(&self) -> Option<Slot> {
        self.active.borrow().or_else(|| self.members.borrow().first().copied())
    }

    /// A chip per member, showing which one is active.
    pub fn rebuild_header(&self) {
        while let Some(child) = self.header.first_child() {
            self.header.remove(&child);
        }
        let active = self.active_slot();
        for slot in self.members.borrow().iter() {
            let chip = gtk::Button::builder()
                .tooltip_text(format!("{}\tclick to switch, drag onto another pane to group", label_for(*slot)))
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

            // Dragging the chip is the gesture that groups panes.
            let source = gtk::DragSource::builder()
                .actions(gdk::DragAction::MOVE)
                .build();
            let payload = slot.as_str().to_string();
            source.connect_prepare(move |_, _, _| {
                Some(gdk::ContentProvider::for_value(&payload.to_value()))
            });
            source.connect_drag_begin(|source, drag| {
                drag.set_icon(Some(&gdk::Texture::from_bytes(
                    &glib::Bytes::from_static(b""),
                )
                .unwrap_or_else(|_| {
                    // A 1x1 transparent icon: the chip itself is the visual cue.
                    gdk::Texture::from_bytes(&glib::Bytes::from_static(&[
                        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d,
                        0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
                        0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00,
                        0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
                        0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
                        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
                    ]))
                    .unwrap_or_else(|_| gdk::Texture::from_bytes(&glib::Bytes::from_static(&[0])).expect("icon"))
                })))
                .ok();
                let _ = source;
            });
            chip.add_controller(source);
            self.header.append(&chip);
        }
        self.header.append(&self.menu_button);
    }
}
