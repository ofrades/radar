//! One tab's content: a program running in a terminal.
//!
//! With the `vte` feature the program runs inside a VTE terminal, which is the
//! whole point of the app: the agent's TUI gets a real terminal, so its mouse,
//! clipboard and keyboard behaviour are the real thing. Without it we degrade
//! to a card that can launch the same command in a separate terminal window.

use std::path::Path;

use adw::prelude::*;

use super::theme::Theme;
use crate::programs::CommandSpec;

/// A running (or launchable) program in a tab.
pub struct Pane {
    widget: gtk::Widget,
    #[cfg(feature = "vte")]
    terminal: Option<vte4::Terminal>,
    command: String,
}

impl Pane {
    pub fn widget(&self) -> &gtk::Widget {
        &self.widget
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    /// Does this pane hold a live terminal?
    pub fn is_live(&self) -> bool {
        #[cfg(feature = "vte")]
        {
            self.terminal.is_some()
        }
        #[cfg(not(feature = "vte"))]
        {
            false
        }
    }

    /// Apply a freshly loaded theme (used when omarchy switches themes).
    pub fn apply_theme(&self, theme: &Theme) {
        #[cfg(feature = "vte")]
        if let Some(terminal) = &self.terminal {
            apply_terminal_theme(terminal, theme);
        }
        #[cfg(not(feature = "vte"))]
        {
            let _ = theme;
        }
    }

    /// Run `spec` in `cwd`.
    pub fn spawn(spec: &CommandSpec, cwd: &Path, theme: &Theme, title: &str) -> Pane {
        #[cfg(feature = "vte")]
        {
            Pane::spawn_embedded(spec, cwd, theme, title)
        }
        #[cfg(not(feature = "vte"))]
        {
            Pane::spawn_external(spec, cwd, theme, title)
        }
    }

    #[cfg(feature = "vte")]
    fn spawn_embedded(spec: &CommandSpec, cwd: &Path, theme: &Theme, title: &str) -> Pane {
        let terminal = vte4::Terminal::new();
        apply_terminal_theme(&terminal, theme);
        terminal.set_scrollback_lines(10_000);
        terminal.set_mouse_autohide(true);
        terminal.set_allow_hyperlink(true);
        terminal.set_hexpand(true);
        terminal.set_vexpand(true);

        // Copy and paste, handled on the terminal itself so we never steal keys
        // a TUI wants: only the Ctrl+Shift combinations are intercepted.
        let keys = gtk::EventControllerKey::new();
        let terminal_for_keys = terminal.clone();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let copy = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                && modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK)
                && (key == gtk::gdk::Key::c || key == gtk::gdk::Key::C);
            let paste = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                && modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK)
                && (key == gtk::gdk::Key::v || key == gtk::gdk::Key::V);
            if copy {
                terminal_for_keys.copy_clipboard_format(vte4::Format::Text);
                return glib::Propagation::Stop;
            }
            if paste {
                terminal_for_keys.paste_clipboard();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        terminal.add_controller(keys);

        let argv: Vec<&str> = spec.argv.iter().map(String::as_str).collect();
        let cwd_string = cwd.to_string_lossy().to_string();
        let terminal_for_exit = terminal.clone();
        let title = title.to_string();
        terminal.spawn_async(
            Some(&cwd_string),
            &argv,
            // VTE inherits our environment; PATH included, which is what the
            // agents need to find their own tools.
            &[],
            glib::SpawnFlags::DEFAULT,
            || {},
            0,
            None::<&gtk::gio::Cancellable>,
            move |result| match result {
                Ok(_pid) => {}
                Err(error) => {
                    let message = format!("{title}: {error}");
                    terminal_for_exit.feed_child(format!("echo {message:?}\r").as_bytes());
                }
            },
        );

        Pane {
            widget: terminal.clone().upcast(),
            terminal: Some(terminal),
            command: spec.display(),
        }
    }

    /// Without VTE: show what would run, and offer to open it elsewhere.
    #[cfg(not(feature = "vte"))]
    fn spawn_external(spec: &CommandSpec, cwd: &Path, _theme: &Theme, title: &str) -> Pane {
        let outer = gtk::Box::new(gtk::Orientation::Vertical, 18);
        outer.set_valign(gtk::Align::Center);
        outer.set_halign(gtk::Align::Center);
        outer.set_hexpand(true);
        outer.set_vexpand(true);
        outer.set_margin_top(24);
        outer.set_margin_bottom(24);
        outer.set_margin_start(24);
        outer.set_margin_end(24);

        let heading = gtk::Label::new(Some(title));
        heading.add_css_class("title-2");
        outer.append(&heading);

        let command = gtk::Label::new(Some(&spec.display()));
        command.add_css_class("monospace");
        command.add_css_class("dim-label");
        command.set_selectable(true);
        outer.append(&command);

        let where_ = gtk::Label::new(Some(&cwd.display().to_string()));
        where_.add_css_class("dim-label");
        where_.set_selectable(true);
        outer.append(&where_);

        let note = gtk::Label::new(Some(
            "Install the terminal widget to run this inside atlas:\n  omarchy pkg add vte4",
        ));
        note.add_css_class("dim-label");
        note.set_justify(gtk::Justification::Center);
        outer.append(&note);

        let launch = gtk::Button::with_label("Open in a terminal window");
        launch.add_css_class("suggested-action");
        launch.add_css_class("pill");
        let spec_for_launch = spec.clone();
        let cwd_for_launch = cwd.to_path_buf();
        launch.connect_clicked(move |_| {
            match spawn_external_window(&spec_for_launch, &cwd_for_launch) {
                Ok(()) => {}
                Err(error) => eprintln!("atlas: could not launch: {error}"),
            };
        });
        outer.append(&launch);

        Pane {
            widget: outer.upcast(),
            command: spec.display(),
        }
    }
}

/// Launch a program in its own terminal window, the way omarchy does.
///
/// Used when atlas is built without the `vte` feature, and by the "open
/// outside" action so a long-running program can escape the app.
pub fn spawn_external_window(spec: &CommandSpec, cwd: &Path) -> std::io::Result<()> {
    use std::process::Command;
    let cwd = cwd.to_string_lossy().to_string();
    // omarchy's launcher knows the user's terminal, its flags and its window
    // rules, so prefer it over guessing.
    if crate::config::have("omarchy-launch-tui") {
        return Command::new("omarchy-launch-tui")
            .args(&spec.argv)
            .current_dir(&cwd)
            .spawn()
            .map(|_| ());
    }
    for (program, flag) in [("alacritty", "-e"), ("foot", "-e"), ("kitty", "-e"), ("ghostty", "-e")] {
        if crate::config::have(program) {
            return Command::new(program)
                .arg("--working-directory")
                .arg(&cwd)
                .arg(flag)
                .args(&spec.argv)
                .spawn()
                .map(|_| ());
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no terminal emulator found (looked for omarchy-launch-tui, alacritty, foot, kitty, ghostty)",
    ))
}

#[cfg(feature = "vte")]
fn apply_terminal_theme(terminal: &vte4::Terminal, theme: &Theme) {
    let font = gtk::pango::FontDescription::new();
    font.set_family(&theme.font_family);
    font.set_size((theme.font_size * gtk::pango::SCALE as f64) as i32);
    terminal.set_font(Some(&font));
    terminal.set_colors(Some(&theme.foreground), Some(&theme.background), &theme.palette);
}
