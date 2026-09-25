//! The overlay panel: every primitive, every key, one place.
//!
//! Opened with `Ctrl+Shift+K` (or from the workspace menu), it floats over
//! the workspace: type to filter, pick a primitive to open, run an action, or
//! just read the keymap. It holds nothing of its own — every row fires the
//! same `win.` action the chips, the dock and the pane menus use, so each
//! primitive stays exactly what it was and the panel never grows a second
//! way to do a thing.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use super::primitive::label_for;
use super::{accel_hint, icon_name, primitive, SharedApp};
use crate::db::Slot;
use crate::programs::{self, Program};

/// The keymap, in one place: this panel renders it, the README quotes it.
/// Per-primitive toggles are not listed here — their keys are on the
/// primitive rows themselves.
const KEYMAP: &[(&str, &str)] = &[
    ("Move between panes and the sidebar", "Ctrl+← → ↑ ↓"),
    ("Resize the focused divider", "← → ↑ ↓"),
    ("Cycle panes and dividers", "Ctrl+Tab / Ctrl+Shift+Tab"),
    ("This overlay", "Ctrl+Shift+K"),
    ("The pane menu", "Right-click / Menu / Shift+F10"),
    ("Focus Editor / Agent / Changes / Commands", "Ctrl+Shift+1–4"),
    ("Change the focused pane's program", "Ctrl+Shift+P"),
    ("Search projects (and add one)", "Ctrl+Shift+N"),
    ("Zoom the focused pane", "F11"),
    ("Refresh theme and git status", "Ctrl+Shift+R"),
    ("Quit", "Ctrl+Shift+Q"),
    ("Copy / paste in a terminal", "Ctrl+Shift+C / Ctrl+Shift+V"),
    ("Terminal font size — Ctrl+0 resets", "Ctrl+= / Ctrl+-"),
    ("Newline in the agent prompt", "Shift+Enter"),
];

/// What the Enter key does with a row.
#[derive(Clone)]
enum Kind {
    /// Open (or focus) the primitive.
    Primitive(Slot),
    /// Select the program used by one primitive.
    Program(Slot, Program),
    /// Fire a `win.` action, no parameter.
    Action(&'static str),
    /// A section heading. Shows only while one of its rows does.
    Section,
    /// A keymap line: information, never activated.
    Info,
}

pub struct Hud {
    /// The backdrop: fills the window while the panel is open, dimming the
    /// workspace behind it.
    root: gtk::Box,
    heading: gtk::Label,
    search: gtk::SearchEntry,
    scroll: gtk::ScrolledWindow,
    list: gtk::ListBox,
    /// Rows in list order, with the text the filter matches.
    rows: RefCell<Vec<(gtk::ListBoxRow, String, Kind)>>,
}

impl Hud {
    pub fn new() -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.add_css_class("hud-root");
        root.set_halign(gtk::Align::Fill);
        root.set_valign(gtk::Align::Fill);
        // Hidden until present(); the workspace gets every key until then.
        root.set_visible(false);

        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.add_css_class("hud-card");
        card.set_halign(gtk::Align::Center);
        card.set_valign(gtk::Align::Center);
        card.set_width_request(540);
        root.append(&card);

        let title = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        title.add_css_class("hud-title");
        let heading = gtk::Label::new(Some("Keys and primitives"));
        heading.add_css_class("heading");
        heading.set_xalign(0.0);
        heading.set_hexpand(true);
        title.append(&heading);
        let hint = gtk::Label::new(Some("Esc closes"));
        hint.add_css_class("caption");
        hint.add_css_class("dim-label");
        title.append(&hint);
        card.append(&title);

        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Filter primitives and keys…"));
        search.add_css_class("hud-filter");
        card.append(&search);

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.add_css_class("hud-list");

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&list)
            .build();
        scroll.set_max_content_height(460);
        scroll.set_propagate_natural_height(true);
        card.append(&scroll);

        Rc::new(Hud {
            root,
            heading,
            search,
            scroll,
            list,
            rows: RefCell::new(Vec::new()),
        })
    }

    pub fn widget(&self) -> &gtk::Widget {
        self.root.upcast_ref()
    }

    pub fn is_visible(&self) -> bool {
        self.root.is_visible()
    }

    /// Bring the panel up over the workspace: fresh rows (visibility
    /// changes), empty filter, the keys on the filter box.
    pub fn present(&self, app: &SharedApp) {
        self.heading.set_text("Keys and primitives");
        self.search
            .set_placeholder_text(Some("Filter primitives and keys…"));
        self.rebuild(app);
        self.search.set_text("");
        self.apply_filter("");
        self.root.set_visible(true);
        self.search.grab_focus();
    }

    /// Show program choices in the same overlay used for primitive navigation
    /// and the keymap.
    pub fn present_programs(&self, app: &SharedApp, slot: Slot) {
        if matches!(slot, Slot::Board | Slot::Custom) {
            return;
        }
        self.heading
            .set_text(&format!("Choose a {} program", label_for(slot)));
        self.search.set_placeholder_text(Some("Filter programs…"));
        self.rebuild_programs(app, slot);
        self.search.set_text("");
        self.apply_filter("");
        self.root.set_visible(true);
        self.search.grab_focus();
    }

    pub fn close(&self, app: &SharedApp) {
        if !self.root.is_visible() {
            return;
        }
        self.root.set_visible(false);
        self.search.set_text("");
        // The keys go back to the program you were looking at.
        app.refocus_workspace();
    }

    /// Connect the panel to the app: row activation, and the keys while the
    /// filter box has them. Split from `new` so the panel holds no
    /// reference back to the app (it is dropped, its widgets live on).
    pub fn wire(self: &Rc<Self>, app: &SharedApp) {
        {
            let hud = self.clone();
            let app = app.clone();
            self.list
                .connect_row_activated(move |_, row| hud.activate(&app, row));
        }
        {
            let hud = self.clone();
            let app = app.clone();
            let keys = gtk::EventControllerKey::new();
            keys.set_propagation_phase(gtk::PropagationPhase::Capture);
            keys.connect_key_pressed(move |_, key, _, _| {
                match key {
                    gtk::gdk::Key::Up => hud.move_selection(-1),
                    gtk::gdk::Key::Down => hud.move_selection(1),
                    gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter => {
                        if let Some(row) = hud.list.selected_row() {
                            hud.activate(&app, &row);
                        }
                    }
                    // Esc empties the filter first, the way the sidebar's
                    // search does, then closes.
                    gtk::gdk::Key::Escape => {
                        if hud.search.text().is_empty() {
                            hud.close(&app);
                        } else {
                            hud.search.set_text("");
                        }
                    }
                    _ => return glib::Propagation::Proceed,
                }
                glib::Propagation::Stop
            });
            self.search.add_controller(keys);
        }
        {
            let hud = self.clone();
            self.search
                .connect_search_changed(move |entry| hud.apply_filter(&entry.text()));
        }
    }

    // ---- rows ----

    fn clear_rows(&self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        self.rows.borrow_mut().clear();
    }

    fn push(&self, row: gtk::ListBoxRow, haystack: String, kind: Kind) {
        self.list.append(&row);
        self.rows.borrow_mut().push((row, haystack, kind));
    }

    /// A section heading: not selectable, and hidden while all of its rows
    /// are filtered out.
    fn section(&self, title: &str) {
        let row = self.row_widget(None, title, None, None);
        row.set_selectable(false);
        row.set_activatable(false);
        row.add_css_class("hud-section");
        self.push(row, title.to_lowercase(), Kind::Section);
    }

    /// A row: leading icon (optional), a label, right-aligned mono keys, and
    /// an optional note where a state badge would sit.
    fn row_widget(
        &self,
        icon: Option<&str>,
        label: &str,
        keys: Option<&str>,
        note: Option<&str>,
    ) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        box_.set_margin_top(4);
        box_.set_margin_bottom(4);
        box_.set_margin_start(10);
        box_.set_margin_end(10);
        if let Some(icon) = icon {
            let image = gtk::Image::from_icon_name(icon);
            image.add_css_class("dim-label");
            box_.append(&image);
        }
        let text = gtk::Label::new(Some(label));
        text.set_xalign(0.0);
        text.set_hexpand(true);
        text.set_ellipsize(gtk::pango::EllipsizeMode::End);
        box_.append(&text);
        if let Some(note) = note {
            let state = gtk::Label::new(Some(note));
            state.add_css_class("caption");
            state.add_css_class("dim-label");
            state.set_valign(gtk::Align::Center);
            box_.append(&state);
        }
        if let Some(keys) = keys {
            let accel = gtk::Label::new(Some(keys));
            accel.add_css_class("hud-keys");
            accel.add_css_class("caption");
            accel.set_valign(gtk::Align::Center);
            box_.append(&accel);
        }
        row.set_child(Some(&box_));
        row
    }

    /// Fresh rows for the workspace as it is right now: which primitives are
    /// on screen, whether the sidebar is.
    fn rebuild(&self, app: &SharedApp) {
        self.clear_rows();

        let slots = app
            .current_workspace()
            .map(|workspace| workspace.visible_slots())
            .unwrap_or_default();

        self.section("Primitives");
        for slot in super::PRIMITIVES {
            let on = slots.contains(&slot);
            let row = self.row_widget(
                Some(icon_name(slot)),
                label_for(slot),
                Some(accel_hint(slot)),
                on.then_some("on screen"),
            );
            self.push(
                row,
                format!("{} {} open show focus", label_for(slot), accel_hint(slot)),
                Kind::Primitive(slot),
            );
        }
        let sidebar = self.row_widget(
            Some(primitive::PROJECTS_ICON),
            "Projects",
            Some("Ctrl+B"),
            app.sidebar_shown.get().then_some("on screen"),
        );
        self.push(
            sidebar,
            "projects sidebar project".to_string(),
            Kind::Action("win.toggle-sidebar"),
        );

        self.section("Actions");
        for (label, keys, action, extra) in [
            ("Search projects", "Ctrl+Shift+N", "win.find-projects", "find add"),
            ("Preferences…", "Ctrl+,", "win.preferences", "settings prefs"),
            (
                "Pane menu",
                "Right-click / Menu / Shift+F10",
                "win.pane-menu",
                "context",
            ),
            ("Zoom the focused pane", "F11", "win.zoom", "maximize"),
            ("Refresh", "Ctrl+Shift+R", "win.refresh", "reload status"),
        ] {
            let row = self.row_widget(None, label, Some(keys), None);
            self.push(row, format!("{label} {keys} {extra}"), Kind::Action(action));
        }

        self.section("Keys");
        for (what, keys) in KEYMAP {
            let row = self.row_widget(None, what, Some(keys), None);
            row.set_selectable(false);
            row.set_activatable(false);
            self.push(row, format!("{what} {keys}"), Kind::Info);
        }
    }

    fn rebuild_programs(&self, app: &SharedApp, slot: Slot) {
        self.clear_rows();
        let current = app.current_workspace().and_then(|workspace| {
            workspace
                .primitive(slot)
                .map(|primitive| primitive.program_id.clone())
                .or_else(|| workspace.programs.borrow().get(&slot).cloned())
        });
        let all = programs::embeddable();
        for kind in programs::Kind::ALL {
            let candidates: Vec<Program> = all
                .iter()
                .filter(|program| program.kind == kind)
                .cloned()
                .collect();
            if candidates.is_empty() {
                continue;
            }
            self.section(kind.label());
            for program in candidates {
                let is_current = current.as_deref() == Some(program.id.as_str());
                let note = if is_current {
                    "current"
                } else {
                    program.description.as_str()
                };
                let row = self.row_widget(None, &program.name, None, Some(note));
                let haystack = format!(
                    "{} {} {} {}",
                    program.name, program.id, program.description, program.command
                );
                self.push(row, haystack, Kind::Program(slot, program));
            }
        }
    }

    /// Run what a row promises, then put the keys back in the workspace.
    fn activate(&self, app: &SharedApp, row: &gtk::ListBoxRow) {
        let index = row.index() as usize;
        let kind = self
            .rows
            .borrow()
            .get(index)
            .map(|(_, _, kind)| kind.clone());
        let Some(kind) = kind else {
            return;
        };
        let mut focus_program = None;
        match kind {
            Kind::Primitive(slot) => {
                let _ = row.activate_action(
                    "win.primitive-activate",
                    Some(&slot.as_str().to_variant()),
                );
            }
            Kind::Program(slot, program) => {
                if let Some(workspace) = app.current_workspace() {
                    app.set_primitive_program(&workspace, slot, program, true);
                    focus_program = Some((workspace, slot));
                }
            }
            Kind::Action(action) => {
                let _ = row.activate_action(action, None);
            }
            Kind::Section | Kind::Info => {}
        }
        self.close(app);
        if let Some((workspace, slot)) = focus_program {
            if let Some(primitive) = workspace.primitive(slot) {
                primitive.focus();
            }
        }
    }

    /// Filter the rows by what they say; a heading shows only while one of
    /// its rows does. The selection moves to the first row that can still
    /// be activated, so Enter keeps meaning something.
    fn apply_filter(&self, query: &str) {
        let query = query.trim().to_lowercase();
        {
            let rows = self.rows.borrow();
            for (row, haystack, _) in rows.iter() {
                row.set_visible(query.is_empty() || haystack.contains(&query));
            }
            for index in 0..rows.len() {
                if !matches!(rows[index].2, Kind::Section) {
                    continue;
                }
                let any = rows.iter().skip(index + 1).take_while(|(_, _, kind)| !matches!(kind, Kind::Section))
                    .any(|(row, _, _)| row.is_visible());
                rows[index].0.set_visible(any);
            }
        }
        let keep = self
            .list
            .selected_row()
            .is_none_or(|row| !row.is_visible() || !row.is_selectable());
        if keep {
            let first = self
                .rows
                .borrow()
                .iter()
                .find(|(row, _, _)| row.is_visible() && row.is_selectable())
                .map(|(row, _, _)| row.clone());
            self.list.select_row(first.as_ref());
        }
    }

    /// Move the selection a row up or down, past rows that are filtered out,
    /// wrapping at the ends — a palette, not a list box.
    fn move_selection(&self, delta: i32) {
        let count = self.rows.borrow().len() as i32;
        if count == 0 {
            return;
        }
        let selected = self
            .list
            .selected_row()
            .map(|row| row.index())
            .unwrap_or(if delta > 0 { -1 } else { count });
        let rows = self.rows.borrow();
        let mut next = selected;
        for _ in 0..count {
            next = (next + delta).rem_euclid(count);
            let (row, _, _) = &rows[next as usize];
            if row.is_visible() && row.is_selectable() {
                self.list.select_row(Some(row));
                self.scroll_row_into_view(row);
                return;
            }
        }
    }

    /// Scroll the selected row into view without stealing the keys from the
    /// filter box: the viewport's own adjustment, nudged just enough.
    fn scroll_row_into_view(&self, row: &gtk::ListBoxRow) {
        let adjustment = self.scroll.vadjustment();
        let Some((_, top)) = row.translate_coordinates(&self.list, 0.0, 0.0) else {
            return;
        };
        let bottom = top + row.height() as f64;
        let (view_top, view_bottom) = (adjustment.value(), adjustment.value() + adjustment.page_size());
        if top < view_top {
            adjustment.set_value(top);
        } else if bottom > view_bottom {
            adjustment.set_value(bottom - adjustment.page_size());
        }
    }
}
