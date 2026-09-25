//! A primitive: one of the four things a project workspace is made of.
//!
//! Editor, agent, diff, terminal — no tabs, no tab strips. A pane is a small
//! header (icon, program, menu) over a terminal running that program, and hiding
//! a pane does not stop its program: the widget is detached, the process and its
//! pty keep going, so an agent never dies because you looked away.

use std::rc::Rc;

use adw::prelude::*;
use gtk::gio;

use super::pane::Pane;
use crate::db::Slot;
use crate::programs::Program;

pub struct Primitive {
    pub program_id: String,
    pub widget: gtk::Box,
    pub pane: Rc<Pane>,
    pub menu_button: gtk::MenuButton,
}

impl Primitive {
    pub fn new(slot: Slot, program: &Program, pane: Rc<Pane>) -> Rc<Primitive> {
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.add_css_class("primitive-header");
        header.set_margin_start(6);
        header.set_margin_end(4);
        header.set_margin_top(2);
        header.set_margin_bottom(2);

        if let Some(icon) = icon_for(slot) {
            let image = gtk::Image::from_gicon(&icon);
            image.set_pixel_size(14);
            image.add_css_class("dim-label");
            header.append(&image);
        }

        let title = gtk::Label::new(Some(&program.name));
        title.add_css_class("caption-heading");
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        header.append(&title);

        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        header.append(&spacer);

        let menu_button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("Pane menu")
            .build();
        menu_button.add_css_class("flat");
        menu_button.set_valign(gtk::Align::Center);
        header.append(&menu_button);

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.set_vexpand(true);
        widget.set_hexpand(true);
        widget.set_tooltip_text(Some(&format!("{}\n{}", program.name, pane.command())));
        widget.append(&header);
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        widget.append(pane.widget());

        Rc::new(Primitive {
            program_id: program.id.clone(),
            widget,
            pane,
            menu_button,
        })
    }

    pub fn focus(&self) {
        self.pane.widget().grab_focus();
    }

}

/// The sidebar is a primitive like the others: it is the project list.
pub const PROJECTS_ICON: &str = "folder-symbolic";
pub const PROJECTS_LABEL: &str = "Project";

/// An icon for a primitive.
pub fn icon_for(slot: Slot) -> Option<gio::Icon> {
    let name = match slot {
        Slot::Editor => "accessories-text-editor-symbolic",
        Slot::Agent => "application-x-executable-symbolic",
        Slot::Diff => "view-dual-symbolic",
        Slot::Shell => "utilities-terminal-symbolic",
        Slot::Custom => "application-x-executable-symbolic",
    };
    Some(gio::ThemedIcon::new(name).upcast())
}

/// The label for a primitive, used in tooltips and menus. One source of truth:
/// the database's slot labels.
pub fn label_for(slot: Slot) -> &'static str {
    slot.label()
}
