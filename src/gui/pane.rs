//! One tab's content: a program running in a terminal.
//!
//! With the `vte` feature the program runs inside a VTE terminal, which is the
//! whole point of the app: the agent's TUI gets a real terminal, so its mouse,
//! clipboard and keyboard behaviour are the real thing. Without it we degrade
//! to a card that can launch the same command in a separate terminal window.

use std::path::Path;

use adw::prelude::*;
#[cfg(feature = "vte")]
use gtk::glib;
#[cfg(feature = "vte")]
use vte4::prelude::*;

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
    #[allow(dead_code)]
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

        // A visible failure beats a black rectangle: if the program cannot be
        // started, say so in the pane.
        let problem = gtk::Label::new(None);
        problem.add_css_class("error");
        problem.add_css_class("caption");
        problem.set_wrap(true);
        problem.set_selectable(true);
        problem.set_xalign(0.0);

        // Embedded terminals can fail for reasons outside the program (VTE's
        // spawn helper, a busy compositor). Never leave a dead pane: offer the
        // same command in the user's own terminal, which always works.
        let fallback = gtk::Button::with_label("Open in a terminal window");
        fallback.add_css_class("pill");
        fallback.set_halign(gtk::Align::Start);
        fallback.set_visible(false);

        let trouble = gtk::Box::new(gtk::Orientation::Vertical, 8);
        trouble.set_margin_top(10);
        trouble.set_margin_bottom(10);
        trouble.set_margin_start(12);
        trouble.set_margin_end(12);
        trouble.append(&problem);
        trouble.append(&fallback);
        trouble.set_visible(false);

        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        container.append(&trouble);
        container.append(&terminal);

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

        // Spawn on first map, not at construction. Two reasons: the pty is sized
        // from a widget that now has its real size, and tabs that are never
        // opened do not start a process at all.
        let spawned = std::rc::Rc::new(std::cell::Cell::new(false));
        let spec_for_spawn = spec.clone();
        let cwd = cwd.to_path_buf();
        let title = title.to_string();
        let terminal_for_spawn: vte4::Terminal = terminal.clone();
        terminal.connect_map(move |_widget| {
            if spawned.get() {
                return;
            }
            spawned.set(true);
            // The signal handler is `Fn`, so everything the spawn needs is cloned
            // here rather than moved out of the captures.
            let terminal = terminal_for_spawn.clone();
            let argv = spec_for_spawn.argv.clone();
            let command = spec_for_spawn.display();
            let cwd_string = cwd.to_string_lossy().to_string();
            let problem = problem.clone();
            let fallback = fallback.clone();
            let trouble = trouble.clone();
            let title = title.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(50), move || {
                try_spawn(
                    terminal, argv, cwd_string, problem, fallback, trouble, title, command, 0,
                );
            });
        });

        Pane {
            widget: container.upcast(),
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
            "Install the terminal widget to run this inside radar:\n  omarchy pkg add vte4",
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
                Err(error) => eprintln!("radar: could not launch: {error}"),
            };
        });
        outer.append(&launch);

        Pane {
            widget: outer.upcast(),
            #[cfg(feature = "vte")]
            terminal: None,
            command: spec.display(),
        }
    }
}

/// Start a program in a VTE terminal, retrying once.
///
/// VTE spawns through a helper process and waits for it while the main loop
/// runs. A main loop that is busy — a synchronous `git status`, say — can make
/// that handshake time out, which looked like "the terminal does not work". A
/// roomy timeout plus one retry makes the pane reliable without hiding real
/// failures: the retry only happens for `Operation timed out`, and anything
/// else is reported immediately.
#[cfg(feature = "vte")]
#[allow(clippy::too_many_arguments)]
fn try_spawn(
    terminal: vte4::Terminal,
    argv: Vec<String>,
    cwd: String,
    problem: gtk::Label,
    fallback: gtk::Button,
    trouble: gtk::Box,
    title: String,
    command: String,
    attempt: u8,
) {
    const SPAWN_TIMEOUT_MS: i32 = 8_000;
    // The callback owns these, while the call itself borrows argv and cwd, so the
    // values it needs for a retry are cloned up front.
    let terminal_for_retry = terminal.clone();
    let argv_for_retry = argv.clone();
    let cwd_for_retry = cwd.clone();
    let problem_for_retry = problem.clone();
    let fallback_for_retry = fallback.clone();
    let trouble_for_retry = trouble.clone();
    let title_for_retry = title.clone();
    let command_for_retry = command.clone();
    // Start through a shell that moves into the project and `exec`s the program.
    //
    // VTE's own working-directory handling is the one difference between a
    // spawn that works and this one that timed out, and `exec` means the shell
    // is replaced by the program: same single process, same pty, no extra layer.
    let wrapped: Vec<String> = std::iter::once("/bin/sh".to_string())
        .chain([
            "-c".to_string(),
            "cd \"$1\" || exit 126; shift; exec \"$@\"".to_string(),
            "radar".to_string(),
            cwd.clone(),
        ])
        .chain(argv.iter().cloned())
        .collect();
    let argv_refs: Vec<&str> = wrapped.iter().map(String::as_str).collect();
    // A terminal launched from a menu has no TERM: programs would fall back to
    // something dumb and look broken. Set it explicitly, the way every terminal
    // emulator does. The rest of the environment is inherited, so PATH and the
    // agents' own configuration come along.
    let envv = ["TERM=xterm-256color", "COLORTERM=truecolor"];
    terminal.spawn_async(
        vte4::PtyFlags::DEFAULT,
        // Deliberately None: the wrapper cds. See above.
        None,
        &argv_refs,
        &envv,
        glib::SpawnFlags::DEFAULT,
        || {},
        SPAWN_TIMEOUT_MS,
        None::<&gtk::gio::Cancellable>,
        move |result| match result {
            Ok(_pid) => {}
            Err(error) => {
                let timed_out = error.to_string().contains("timed out");
                if timed_out && attempt < 2 {
                    glib::timeout_add_local_once(
                        std::time::Duration::from_millis(400),
                        move || {
                            try_spawn(
                                terminal_for_retry,
                                argv_for_retry,
                                cwd_for_retry,
                                problem_for_retry,
                                fallback_for_retry,
                                trouble_for_retry,
                                title_for_retry,
                                command_for_retry,
                                attempt + 1,
                            );
                        },
                    );
                } else {
                    // Only the clones are used here: the call still borrows the
                    // originals while this callback runs.
                    problem_for_retry.set_text(&format!(
                        "Could not start {title_for_retry} inside radar: {error}\n\
                         The same program in your own terminal:\n  cd {cwd_for_retry}\n  {command_for_retry}"
                    ));
                    // Wire the fallback once, then show it.
                    if !fallback_for_retry.has_css_class("wired") {
                        fallback_for_retry.add_css_class("wired");
                        let spec = CommandSpec {
                            argv: argv_for_retry.clone(),
                            env_unset: Vec::new(),
                        };
                        let cwd = std::path::PathBuf::from(&cwd_for_retry);
                        let toasts: Option<gtk::Widget> = None;
                        let _ = toasts;
                        fallback_for_retry.connect_clicked(move |_| {
                            if let Err(error) = spawn_external_window(&spec, &cwd) {
                                eprintln!("radar: could not open a terminal: {error}");
                            }
                        });
                    }
                    trouble_for_retry.set_visible(true);
                }
            }
        },
    );
}

/// Launch a program in its own terminal window, the way omarchy does.
///
/// Used when radar is built without the `vte` feature, and by the "open
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
    let mut font = gtk::pango::FontDescription::new();
    font.set_family(&theme.font_family);
    font.set_size((theme.font_size * gtk::pango::SCALE as f64) as i32);
    terminal.set_font(Some(&font));
    let palette: Vec<&gtk::gdk::RGBA> = theme.palette.iter().collect();
    terminal.set_colors(Some(&theme.foreground), Some(&theme.background), &palette);
}
