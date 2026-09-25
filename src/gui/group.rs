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
        Group::accept_drags(&group);
        group
    }

    /// The header drags and accepts drops.
    ///
    /// Dragging the header drags the primitive it is showing; dropping one header
    /// on another makes them share a header. The gestures live on the header box
    /// rather than on the chips, because a button claims the press that a drag
    /// would need.
    fn accept_drags(group: &Rc<Group>) {
        // Drag: whatever this header is showing.
        let source = gtk::DragSource::builder()
            .actions(gdk::DragAction::MOVE)
            .build();
        let group_for_drag = group.clone();
        source.connect_prepare(move |_, _, _| {
            let slot = group_for_drag.active_slot()?;
            Some(gdk::ContentProvider::for_value(&slot.as_str().to_value()))
        });
        group.header.add_controller(source);

        // Drop on the header: join this pane.
        let target = gtk::DropTarget::new(glib::types::Type::STRING, gdk::DragAction::MOVE);
        let header = group.header.clone();
        let group_for_drop = group.clone();
        target.connect_drop(move |_, value, _, _| {
            let Ok(payload) = value.get::<String>() else {
                return false;
            };
            // The dragged header names the primitive it was showing.
            let source_slot = Slot::parse(&payload);
            if source_slot == Slot::Custom {
                return false;
            }
            let Some(target_slot) = group_for_drop.active_slot() else {
                return false;
            };
            if source_slot == target_slot {
                return false;
            }
            let _ = header.activate_action(
                "primitive-group",
                Some(&(payload, target_slot.as_str().to_string()).to_variant()),
            );
            true
        });
        group.header.add_controller(target);

        // Drop on the content: pulling one of *this pane's* primitives out into a
        // pane of its own. Anything else lands here as a grouping request, which
        // is what dropping a pane onto another pane means.
        let content_target = gtk::DropTarget::new(glib::types::Type::STRING, gdk::DragAction::MOVE);
        let content = group.content.clone();
        let group_for_content = group.clone();
        content_target.connect_drop(move |_, value, _, _| {
            let Ok(payload) = value.get::<String>() else {
                return false;
            };
            let slot = Slot::parse(&payload);
            if slot == Slot::Custom {
                return false;
            }
            if group_for_content.contains(slot) && group_for_content.slots().len() > 1 {
                let _ = content.activate_action("primitive-split-out", Some(&payload.to_variant()));
                return true;
            }
            let Some(target_slot) = group_for_content.active_slot() else {
                return false;
            };
            let _ = content.activate_action(
                "primitive-group",
                Some(&(payload, target_slot.as_str().to_string()).to_variant()),
            );
            true
        });
        group.content.add_controller(content_target);
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

            self.header.append(&chip);
        }
        self.header.append(&self.menu_button);
    }
}
