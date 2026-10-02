//! The overlay panel: every primitive, every key, one place.
//!
//! Opened with `Alt+H` (or from the workspace menu), it floats over
//! the workspace: type to filter, pick a primitive to open, run an action, or
//! just read the keymap. It holds nothing of its own — every row fires the
//! same `win.` action the chips, the dock and the pane menus use, so each
//! primitive stays exactly what it was and the panel never grows a second
//! way to do a thing.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use super::primitive::label_for;
use super::{accel_hint, icon_name, App, SharedApp};
use crate::db::{Slot, TabKey};
use crate::programs::{self, Program};

/// The keymap, in one place: this panel renders it, the README quotes it.
/// Per-primitive toggles are not listed here — their keys are on the
/// primitive rows themselves.
const KEYMAP: &[(&str, &str)] = &[
    ("Move between panes", "Alt+← → ↑ ↓"),
    ("Home / all projects", "Alt+Home / Alt+B"),
    ("Resize the focused divider", "← → ↑ ↓"),
    ("Cycle panes and dividers", "Ctrl+Tab / Ctrl+Shift+Tab"),
    ("This overlay", "Alt+H"),
    ("The pane menu", "Right-click / Menu / Shift+F10"),
    ("Focus Editor / Agent / Changes / Commands", "Alt+1–4"),
    ("Change the focused pane's program", "Alt+P"),
    ("Search projects (and add one)", "Alt+N"),
    ("Zoom the focused pane", "Alt+F"),
    ("Refresh theme and git status", "Alt+R"),
    ("Quit", "Alt+Q"),
    ("Copy / paste in a terminal", "Alt+C / Alt+V"),
    ("Terminal font size — Alt+0 resets", "Alt+= / Alt+-"),
    ("Newline in the agent prompt", "Shift+Enter"),
];

/// What the Enter key does with a row.
#[derive(Clone)]
enum Kind {
    /// Open (or focus) the primitive.
    Primitive(Slot),
    /// Select the program used by one primitive. Boxed: the variant sits in
    /// a small enum beside primitive and card rows, and the payload is the
    /// only large one.
    Program(Slot, Box<Program>),
    /// Open the task's conversation without leaving the workspace.
    Card(i64, String),
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
    /// The project and optional task currently shown in this shared dialog.
    task_project: Cell<Option<i64>>,
    task_card: RefCell<Option<String>>,
    back: gtk::Button,
    creation: gtk::Box,
    details: gtk::Box,
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
        let back = gtk::Button::from_icon_name("go-previous-symbolic");
        back.add_css_class("flat");
        back.set_tooltip_text(Some("Back to to-dos"));
        back.set_action_name(Some("win.new-session"));
        back.set_visible(false);
        title.append(&back);
        let heading = gtk::Label::new(Some("Keys and primitives"));
        heading.add_css_class("heading");
        heading.set_xalign(0.0);
        heading.set_hexpand(true);
        title.append(&heading);
        let hint = gtk::Label::new(Some("Esc closes"));
        hint.add_css_class("caption");
        hint.add_css_class("dim-label");
        title.append(&hint);
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.set_tooltip_text(Some("Close dialog"));
        close.set_action_name(Some("win.hud"));
        title.append(&close);
        card.append(&title);
        let creation = gtk::Box::new(gtk::Orientation::Vertical, 0);
        creation.add_css_class("hud-creation");
        creation.set_visible(false);
        card.append(&creation);

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
        let details = gtk::Box::new(gtk::Orientation::Vertical, 0);
        details.add_css_class("hud-task");
        details.set_visible(false);
        card.append(&details);

        Rc::new(Hud {
            root,
            heading,
            search,
            scroll,
            list,
            rows: RefCell::new(Vec::new()),
            task_project: Cell::new(None),
            task_card: RefCell::new(None),
            back,
            creation,
            details,
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
        self.reset_tasks();
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
        self.reset_tasks();
        self.heading
            .set_text(&format!("Choose a {} program", label_for(slot)));
        self.search.set_placeholder_text(Some("Filter programs…"));
        self.rebuild_programs(app, slot);
        self.search.set_text("");
        self.apply_filter("");
        self.root.set_visible(true);
        self.search.grab_focus();
    }

    /// One workspace entry point for creating work and opening task conversations.
    pub fn present_cards(&self, app: &App, project_id: i64) {
        gtk::prelude::GtkWindowExt::set_focus(&app.window, None::<&gtk::Widget>);
        if self.task_project.get() != Some(project_id) {
            self.search.set_text("");
        }
        self.task_project.set(Some(project_id));
        self.task_card.borrow_mut().take();
        self.heading.set_text("To-dos & sessions");
        self.search.set_placeholder_text(Some("Filter to-dos…"));
        self.back.set_visible(false);
        self.details.set_visible(false);
        self.creation.set_visible(true);
        self.search.set_visible(true);
        self.scroll.set_visible(true);
        self.scroll
            .set_max_content_height((app.window.height() - 220).clamp(120, 460));
        self.refresh_tasks(app);
        self.root.set_visible(true);
        self.search.grab_focus();
    }

    pub fn present_task(&self, app: &App, project_id: i64, card_id: &str) {
        self.task_project.set(Some(project_id));
        *self.task_card.borrow_mut() = Some(card_id.to_string());
        self.heading.set_text("To-do conversation");
        self.back.set_visible(true);
        self.creation.set_visible(false);
        self.search.set_visible(false);
        self.scroll.set_visible(false);
        self.details.set_visible(true);
        self.details
            .set_height_request((app.window.height() - 160).clamp(240, 600));
        self.replace_task(app, project_id, card_id);
        self.root.set_visible(true);
        self.back.grab_focus();
    }

    fn replace_task(&self, app: &App, project_id: i64, card_id: &str) {
        if app
            .window
            .focus_widget()
            .is_some_and(|focus| focus.is_ancestor(&self.details))
        {
            gtk::prelude::GtkWindowExt::set_focus(&app.window, None::<&gtk::Widget>);
        }
        while let Some(child) = self.details.first_child() {
            self.details.remove(&child);
        }
        self.details
            .append(&super::card::detail(app, project_id, card_id));
    }

    pub fn refresh_tasks(&self, app: &App) {
        let Some(project_id) = self.task_project.get() else {
            return;
        };
        if app.home_focus_todo.get().is_none() {
            if let Some(focus) = app.window.focus_widget() {
                if (focus.is_ancestor(&self.creation) || focus.is_ancestor(&self.details))
                    && (focus.is::<gtk::Editable>() || focus.is::<gtk::TextView>())
                {
                    return;
                }
            }
        }
        let card_id = self.task_card.borrow().clone();
        if let Some(card_id) = card_id {
            self.replace_task(app, project_id, &card_id);
        } else {
            if app
                .window
                .focus_widget()
                .is_some_and(|focus| focus.is_ancestor(&self.creation))
            {
                gtk::prelude::GtkWindowExt::set_focus(&app.window, None::<&gtk::Widget>);
            }
            while let Some(child) = self.creation.first_child() {
                self.creation.remove(&child);
            }
            self.creation
                .append(&super::home::todo_add_entry(app, project_id));
            self.rebuild_cards(app, project_id);
            self.apply_filter(&self.search.text());
        }
    }

    fn reset_tasks(&self) {
        self.task_project.set(None);
        self.task_card.borrow_mut().take();
        self.creation.set_visible(false);
        self.details.set_visible(false);
        self.back.set_visible(false);
        self.search.set_visible(true);
        self.scroll.set_visible(true);
        self.scroll.set_max_content_height(460);
        while let Some(child) = self.details.first_child() {
            self.details.remove(&child);
        }
        while let Some(child) = self.creation.first_child() {
            self.creation.remove(&child);
        }
    }

    pub fn close_tasks(&self, app: &App) {
        if self.task_project.get().is_some() {
            self.close(app);
        }
    }

    pub fn close(&self, app: &App) {
        if !self.root.is_visible() {
            return;
        }
        let focus_in_hud = app
            .window
            .focus_widget()
            .is_some_and(|focus| focus.is_ancestor(&self.root));
        if focus_in_hud {
            gtk::prelude::GtkWindowExt::set_focus(&app.window, None::<&gtk::Widget>);
        }
        self.root.set_visible(false);
        self.search.set_text("");
        self.reset_tasks();
        // The keys go back to the program you were looking at — unless the
        // row just activated already moved them there. Opening a primitive
        // from a row focuses it; refocusing the workspace would yank the
        // keys out of the panel the row just opened.
        if focus_in_hud {
            app.refocus_workspace();
        }
    }

    /// Clear the filter first, then close the panel, no matter which of its
    /// widgets currently has keyboard focus.
    pub fn handle_escape(&self, app: &SharedApp) {
        if !self.root.is_visible() {
            return;
        }
        if self.task_card.borrow().is_some() || self.search.text().is_empty() {
            self.close(app);
        } else {
            self.search.set_text("");
        }
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
            image.set_pixel_size(16);
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

        // A primitive is "on screen" when any tab of that kind is: the HUD
        // speaks in kinds, and activating a row lands on the tab of the kind
        // that exists.
        let kinds = app
            .current_workspace()
            .map(|workspace| workspace.visible_kinds())
            .unwrap_or_default();

        self.section("Primitives");
        for slot in super::PRIMITIVES {
            let on = kinds.contains(&slot);
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

        self.section("Actions");
        for (label, keys, action, extra) in [
            (
                "Go home",
                "Alt+Home / Alt+B",
                "win.show-home",
                "all projects overview",
            ),
            (
                "To-dos & sessions…",
                "",
                "win.new-session",
                "create work choose agent conversation tasks",
            ),
            (
                "This project's board",
                "Alt+K",
                "win.workspace-project",
                "to-dos lanes kanban",
            ),
            (
                "Add a project…",
                "",
                "win.home-add-project",
                "create or add",
            ),
            ("Preferences…", "Alt+,", "win.preferences", "settings prefs"),
            (
                "Auto arrange panels",
                "",
                "win.workspace-auto-arrange",
                "reset layout fit panels",
            ),
            ("Zoom the focused pane", "Alt+F", "win.zoom", "maximize"),
            ("Refresh", "Alt+R", "win.refresh", "reload status"),
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
                .tab(workspace.resolve_tab(TabKey::first(slot)))
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
                self.push(row, haystack, Kind::Program(slot, Box::new(program)));
            }
        }
    }

    /// The project's open to-dos, Home's order, each row starting the
    /// project's default agent attached to that card.
    fn rebuild_cards(&self, app: &App, project_id: i64) {
        self.clear_rows();
        let rows = app
            .board_states
            .borrow()
            .get(&project_id)
            .map(card_rows)
            .unwrap_or_default();
        if rows.is_empty() {
            let row = self.row_widget(None, "No open to-dos — create one above", None, None);
            row.set_selectable(false);
            row.set_activatable(false);
            self.push(row, "no open to-dos".to_string(), Kind::Info);
            return;
        }
        for (id, title, note) in rows {
            let row = self.row_widget(None, &title, None, Some(&note));
            self.push(
                row,
                format!("{title} {note} {id}"),
                Kind::Card(project_id, id),
            );
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
                let _ = row
                    .activate_action("win.primitive-activate", Some(&slot.as_str().to_variant()));
                // Asking for an agent with none to show opens this panel's
                // to-do picker; the picker is the answer, so it stays up.
                if matches!(slot, Slot::Agent) && self.task_project.get().is_some() {
                    return;
                }
            }
            Kind::Program(slot, program) => {
                if let Some(workspace) = app.current_workspace() {
                    app.set_primitive_program(&workspace, slot, *program, true);
                    focus_program = Some((workspace, slot));
                }
            }
            Kind::Card(project_id, card_id) => {
                let _ = row.activate_action(
                    "win.open-card",
                    Some(&(project_id, card_id.as_str()).to_variant()),
                );
                return;
            }
            Kind::Action(action) => {
                let _ = row.activate_action(action, None);
                // The new-session row re-opens this panel as the picker.
                if action == "win.new-session" && self.task_project.get().is_some() {
                    return;
                }
            }
            Kind::Section | Kind::Info => {}
        }
        self.close(app);
        if let Some((workspace, slot)) = focus_program {
            // The choice landed on the kind's tab that exists — resolve the
            // same way the action did before focusing it.
            let key = workspace.resolve_tab(TabKey::first(slot));
            if let Some(primitive) = workspace.tab(key) {
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
                let any = rows
                    .iter()
                    .skip(index + 1)
                    .take_while(|(_, _, kind)| !matches!(kind, Kind::Section))
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
        let (view_top, view_bottom) = (
            adjustment.value(),
            adjustment.value() + adjustment.page_size(),
        );
        if top < view_top {
            adjustment.set_value(top);
        } else if bottom > view_bottom {
            adjustment.set_value(bottom - adjustment.page_size());
        }
    }
}

/// The to-do picker's rows from a board state: `(card id, title, note)`, the
/// project's open to-dos in Home's order, the lane — and the claim, when one
/// is held — as the note.
fn card_rows(state: &crate::session::board_store::BoardState) -> Vec<(String, String, String)> {
    super::home::open_and_done(&super::board::summarize(state))
        .0
        .into_iter()
        .map(|(lane_name, card)| {
            let note = match &card.claim {
                Some(claim) => format!("{lane_name} · {claim}"),
                None => lane_name,
            };
            (card.id, card.title, note)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::card_rows;
    use crate::session::board_store::{BoardState, Lane, StoredCard};

    fn card(id: &str, lane_id: i64, title: &str, claim: Option<&str>) -> StoredCard {
        StoredCard {
            id: id.to_string(),
            project_id: 1,
            lane_id,
            lane: String::new(),
            done: false,
            position: 0,
            title: title.to_string(),
            body: String::new(),
            claim: claim.map(str::to_string),
            revision: 1,
            created_at_millis: 0,
            updated_at_millis: 0,
        }
    }

    fn lane(id: i64, name: &str, kind: &str) -> Lane {
        Lane {
            id,
            name: name.to_string(),
            kind: kind.to_string(),
            position: 0,
        }
    }

    #[test]
    fn the_picker_lists_open_to_dos_in_home_order_with_claims_as_notes() {
        let state = BoardState {
            project_id: 1,
            lanes: vec![
                lane(1, "In progress", "in_progress"),
                lane(2, "Backlog", "open"),
                lane(3, "Done", "done"),
            ],
            cards: vec![
                card("c-done", 3, "Shipped work", Some("opencode-x")),
                card("c-todo", 2, "Untouched work", None),
                card("c-doing", 1, "Started work", Some("omp-y")),
            ],
        };
        let rows = card_rows(&state);
        assert_eq!(
            rows,
            vec![
                (
                    "c-doing".to_string(),
                    "Started work".to_string(),
                    "In progress · omp-y".to_string()
                ),
                (
                    "c-todo".to_string(),
                    "Untouched work".to_string(),
                    "Todo".to_string()
                ),
            ]
        );
    }

    #[test]
    fn a_board_without_open_to_dos_leaves_the_picker_rows_empty() {
        let state = BoardState {
            project_id: 1,
            lanes: vec![lane(3, "Done", "done")],
            cards: vec![card("c-done", 3, "Shipped work", None)],
        };
        assert!(card_rows(&state).is_empty());
    }
}
