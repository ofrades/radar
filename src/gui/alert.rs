//! In-app activity alerts.
//!
//! A small stack of cards, top-right, in the shadcn Alert shape: a bordered
//! panel with an icon, a title, a description and its own actions. They
//! announce activity that wants the human — a question, an approval, an agent
//! that stopped or failed — without stealing focus, and each plays a short
//! system sound. They are transient: each dismisses itself, or on its ✕.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

/// How loudly an alert reads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Tone {
    Info,
    Success,
    Warning,
    Danger,
}

impl Tone {
    fn css(self) -> &'static str {
        match self {
            Tone::Info => "info",
            Tone::Success => "success",
            Tone::Warning => "warning",
            Tone::Danger => "danger",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Tone::Info => "dialog-information-symbolic",
            Tone::Success => "emblem-ok-symbolic",
            Tone::Warning => "dialog-warning-symbolic",
            Tone::Danger => "dialog-error-symbolic",
        }
    }

    /// The freedesktop sound name for this tone.
    fn sound(self) -> &'static str {
        match self {
            Tone::Info => "message",
            Tone::Success => "complete",
            Tone::Warning => "dialog-warning",
            Tone::Danger => "dialog-error",
        }
    }
}

/// One action button on an alert: a label and the window action it runs.
pub(super) struct Action {
    pub label: String,
    pub action: String,
    pub target: Option<glib::Variant>,
}

impl Action {
    pub(super) fn new(label: &str, action: &str, target: Option<glib::Variant>) -> Self {
        Self {
            label: label.to_string(),
            action: action.to_string(),
            target,
        }
    }
}

/// The window's alert stack. Empty and inert until something is shown.
pub(super) struct Alerts {
    root: gtk::Box,
}

impl Alerts {
    pub fn new() -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
        root.add_css_class("alert-stack");
        root.set_halign(gtk::Align::End);
        root.set_valign(gtk::Align::Start);
        root.set_margin_top(14);
        root.set_margin_end(14);
        // The empty space around the cards is click-through.
        root.set_can_target(false);
        Rc::new(Self { root })
    }

    pub fn widget(&self) -> &gtk::Widget {
        self.root.upcast_ref()
    }

    /// Show an alert. The newest four stay; each dismisses itself after a
    /// while, or on its ✕.
    pub fn show(self: &Rc<Self>, tone: Tone, title: &str, body: &str, actions: Vec<Action>) {
        let card = self.card(tone, title, body, actions);
        self.root.prepend(&card);
        let mut child = self.root.first_child();
        let mut count = 0;
        while let Some(current) = child {
            count += 1;
            let next = current.next_sibling();
            if count > 4 {
                self.root.remove(&current);
            }
            child = next;
        }
        let root = self.root.clone();
        glib::timeout_add_seconds_local_once(10, move || {
            if card.parent().is_some() {
                root.remove(&card);
            }
        });
        sound(tone);
    }

    fn card(&self, tone: Tone, title: &str, body: &str, actions: Vec<Action>) -> gtk::Box {
        let card = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        card.add_css_class("activity-alert");
        card.add_css_class(tone.css());
        // A fixed width so the stack reads as a column; the text wraps inside.
        card.set_size_request(330, -1);

        let icon = gtk::Image::from_icon_name(tone.icon());
        icon.add_css_class("alert-icon");
        icon.add_css_class(tone.css());
        icon.set_valign(gtk::Align::Start);
        card.append(&icon);

        let texts = gtk::Box::new(gtk::Orientation::Vertical, 4);
        texts.set_hexpand(true);
        let title_label = gtk::Label::new(Some(title));
        title_label.add_css_class("alert-title");
        title_label.set_xalign(0.0);
        title_label.set_wrap(true);
        title_label.set_max_width_chars(40);
        texts.append(&title_label);
        if !body.is_empty() {
            let body_label = gtk::Label::new(Some(body));
            body_label.add_css_class("alert-body");
            body_label.set_xalign(0.0);
            body_label.set_wrap(true);
            body_label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            body_label.set_max_width_chars(48);
            body_label.set_lines(3);
            body_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            texts.append(&body_label);
        }
        if !actions.is_empty() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.set_margin_top(2);
            for action in actions {
                let button = gtk::Button::with_label(&action.label);
                button.add_css_class("flat");
                button.add_css_class("alert-action");
                button.set_action_name(Some(&action.action));
                if let Some(target) = &action.target {
                    button.set_action_target_value(Some(target));
                }
                row.append(&button);
            }
            texts.append(&row);
        }
        card.append(&texts);

        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Dismiss")
            .build();
        close.add_css_class("flat");
        close.add_css_class("alert-close");
        close.set_valign(gtk::Align::Start);
        let root = self.root.clone();
        let card_for_close = card.clone();
        close.connect_clicked(move |_| {
            if card_for_close.parent().is_some() {
                root.remove(&card_for_close);
            }
        });
        card.append(&close);
        card
    }
}

/// A short system sound for an alert, best-effort: a missing player is silent,
/// never an error.
fn sound(tone: Tone) {
    use std::process::{Command, Stdio};
    let _ = Command::new("canberra-gtk-play")
        .args(["-i", tone.sound()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}
