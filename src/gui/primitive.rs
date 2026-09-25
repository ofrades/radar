//! A primitive: one of the four things a project workspace is made of.
//!
//! Editor, agent, diff, terminal — no tabs, no tab strips. A pane is a small
//! header (icon, program, menu) over a terminal running that program, and hiding
//! a pane does not stop its program: the widget is detached, the process and its
//! pty keep going, so an agent never dies because you looked away.

use std::rc::Rc;

use adw::prelude::*;

use super::pane::Pane;
use crate::db::Slot;
use crate::programs::Program;

pub struct Primitive {
    pub program_id: String,
    pub widget: gtk::Widget,
    pub pane: Rc<Pane>,
}

impl Primitive {
    /// A primitive is just its content: the header belongs to the group it is in,
    /// so naming it here as well would show the name twice.
    pub fn new(program: &Program, pane: Rc<Pane>) -> Rc<Primitive> {
        let widget = pane.widget().clone();
        // Hovering the pane says what is running in it and how it was started.
        widget.set_tooltip_text(Some(&format!("{}\n{}", program.name, pane.command())));
        Rc::new(Primitive {
            program_id: program.id.clone(),
            widget,
            pane,
        })
    }

    pub fn focus(&self) {
        self.pane.widget().grab_focus();
    }
}

/// The sidebar is a primitive like the others: it is the project list.
pub const PROJECTS_ICON: &str = "folder-symbolic";
pub const PROJECTS_LABEL: &str = "Project";

/// The label for a primitive, used in tooltips and menus. One source of truth:
/// the database's slot labels.
pub fn label_for(slot: Slot) -> &'static str {
    slot.label()
}
