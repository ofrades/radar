//! A primitive: one of the things a project workspace is made of.
//!
//! Editor, agent, diff, terminal — and the board, radar's one built-in. No
//! tabs, no tab strips. A pane is a small header over a terminal running that
//! program — the header carries the live info, the program dropdown and the
//! close right on the chip — and hiding a pane does not stop its program:
//! the widget is detached, the process and its pty keep going, so an agent
//! never dies because you looked away.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use super::pane::Pane;
use crate::db::Slot;
use crate::programs::Program;

pub struct Primitive {
    pub program_id: String,
    pub widget: gtk::Widget,
    pub pane: Option<Rc<Pane>>,
    /// The provider conversation this pane was launched on, when the launch
    /// named one exactly (`--session <id>`). How a sidebar row recognizes
    /// its own conversation in a tab: showing a running tab is only honest
    /// when it really is the conversation the row points at.
    pub launched_session: RefCell<Option<String>>,
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
            pane: Some(pane),
            launched_session: RefCell::new(None),
        })
    }

    /// A built-in primitive: radar draws the content itself and no program
    /// runs, so there is no pty to keep alive and nothing to relaunch.
    pub fn builtin(program_id: &str, widget: gtk::Widget, tooltip: &str) -> Rc<Primitive> {
        widget.set_tooltip_text(Some(tooltip));
        Rc::new(Primitive {
            program_id: program_id.to_string(),
            widget,
            pane: None,
            launched_session: RefCell::new(None),
        })
    }

    pub fn focus(&self) {
        match &self.pane {
            Some(pane) => pane.focus(),
            None => self.widget.grab_focus(),
        };
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
