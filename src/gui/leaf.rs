//! A leaf of the split tree: one tab strip, one tab view.
//!
//! Splitting the window is splitting a leaf into a pair, each half a leaf like
//! this one. Tabs live in leaves, and every leaf has its own menu for the things
//! you want from where you are: another tab, another split, or the workspace.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use super::pane::Pane;
use crate::db::{Slot, Tab};

/// One open tab: the widget libadwaita manages, plus what it was opened as.
pub struct Page {
    pub page: adw::TabPage,
    pub slot: Slot,
    pub program_id: String,
    pub title: String,
    pub extra_args: Vec<String>,
    pub pane: Rc<Pane>,
}

/// A pane of the window: a tab strip over a tab view.
pub struct Leaf {
    pub widget: gtk::Box,
    pub tab_view: adw::TabView,
    pub menu_button: gtk::MenuButton,
    pub pages: RefCell<Vec<Page>>,
}

impl Leaf {
    pub fn new() -> Rc<Leaf> {
        let tab_view = adw::TabView::new();
        tab_view.set_shortcuts(adw::TabViewShortcuts::ALL_SHORTCUTS);
        tab_view.set_vexpand(true);
        tab_view.set_hexpand(true);

        let tab_bar = adw::TabBar::builder().view(&tab_view).build();
        tab_bar.set_expand_tabs(false);

        // The pane's own menu: new tabs, splits, workspace actions.
        let menu_button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("Pane menu")
            .build();
        menu_button.add_css_class("flat");
        menu_button.set_valign(gtk::Align::Center);

        let strip = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        strip.add_css_class("pane-strip");
        strip.append(&tab_bar);
        strip.append(&menu_button);

        // Space for a window drag / status, kept empty so the bar reads as one
        // line rather than a widget tray.
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        strip.append(&spacer);
        tab_bar.set_hexpand(true);

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&strip);
        widget.append(&tab_view);

        Rc::new(Leaf {
            widget,
            tab_view,
            menu_button,
            pages: RefCell::new(Vec::new()),
        })
    }

    /// Register a tab whose `adw::TabPage` came from this pane's tab view.
    pub fn add_page(&self, page: Page) {
        self.tab_view.set_selected_page(&page.page);
        self.pages.borrow_mut().push(page);
    }

    /// Forget a tab, returning it so the caller can store the change.
    pub fn remove_page(&self, page: &adw::TabPage) -> Option<Page> {
        let mut pages = self.pages.borrow_mut();
        let index = pages.iter().position(|candidate| candidate.page == *page)?;
        Some(pages.remove(index))
    }

    pub fn selected_page(&self) -> Option<adw::TabPage> {
        self.tab_view.selected_page()
    }

    pub fn is_empty(&self) -> bool {
        self.pages.borrow().is_empty()
    }

    /// The tab filling a slot, if this pane has one.
    pub fn page_for_slot(&self, slot: Slot) -> Option<adw::TabPage> {
        self.pages
            .borrow()
            .iter()
            .find(|page| page.slot == slot)
            .map(|page| page.page.clone())
    }

    /// Tabs in strip order, ready to store.
    pub fn to_tabs(&self, project_id: i64) -> Vec<Tab> {
        self.pages
            .borrow()
            .iter()
            .enumerate()
            .map(|(index, page)| {
                let mut tab = Tab::new(page.slot, page.program_id.clone());
                tab.title = Some(page.title.clone());
                tab.extra_args = page.extra_args.clone();
                tab.sort_order = index as i64;
                let _ = project_id;
                tab
            })
            .collect()
    }

    /// Focus this pane's tab view.
    pub fn focus(&self) {
        self.tab_view.grab_focus();
    }
}
