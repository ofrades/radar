//! One tab's content: a program running in a terminal.
//!
//! With the `vte` feature the program runs inside a VTE terminal, which is the
//! whole point of the app: the agent's TUI gets a real terminal, so its mouse,
//! clipboard and keyboard behaviour are the real thing. Without it we degrade
//! to a card that can launch the same command in a separate terminal window.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use adw::prelude::*;
#[cfg(feature = "vte")]
use gtk::glib;
#[cfg(feature = "vte")]
use vte4::prelude::*;

use super::theme::Theme;
use crate::programs::CommandSpec;

/// Who a pane's live signals talk to: the header routing in mod.rs.
type InfoObserver = Box<dyn Fn(Option<String>)>;
type BellObserver = Box<dyn Fn()>;

/// What Shift+Enter should send to a program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShiftEnter {
    /// Plain Enter, the standard terminal behaviour.
    Off,
    /// `ESC [ 1 3 ; 2 u`: the kitty keyboard encoding for Shift+Enter, which
    /// agent TUIs that ask for the protocol parse.
    Kitty,
    /// `ESC CR`: what terminals send for Meta+Enter, which many TUIs accept as
    /// "newline without submitting".
    Meta,
}

impl ShiftEnter {
    /// Only for the panes where it makes sense: an agent prompt wants a newline,
    /// a shell or an editor does not, and synthesising a key there would corrupt
    /// real input.
    ///
    /// Kitty form, because the agent TUIs people use ask for that protocol; VTE
    /// cannot provide it, so this is the missing half. Switch to `Meta` (ESC CR,
    /// the classic Meta+Enter) if a particular agent prefers that.
    pub fn for_slot(slot: crate::db::Slot) -> ShiftEnter {
        if slot != crate::db::Slot::Agent {
            return ShiftEnter::Off;
        }
        // An escape hatch that does not depend on configuration: some agents want
        // the classic Meta+Enter instead, some want nothing at all.
        match std::env::var("RADAR_SHIFT_ENTER").as_deref() {
            Ok("meta") => ShiftEnter::Meta,
            Ok("off") => ShiftEnter::Off,
            _ => ShiftEnter::Kitty,
        }
    }
}

/// A running (or launchable) program in a tab.
pub struct Pane {
    widget: gtk::Widget,
    #[cfg(feature = "vte")]
    terminal: Option<vte4::Terminal>,
    command: String,
    /// This pane's font zoom, 1.0 = the theme's size. Alt+= and Alt+-
    /// change it, Alt+0 resets it, and a theme reload keeps it.
    #[cfg(feature = "vte")]
    font_scale: std::rc::Rc<std::cell::Cell<f64>>,
    /// Family and unscaled size as the last theme application left them, so
    /// a zoom can rebuild the font without a theme in hand.
    #[cfg(feature = "vte")]
    base_font: std::rc::Rc<std::cell::RefCell<(String, f64)>>,
    /// Who to tell when this pane's program reports something live: its own
    /// window title, or that it has exited. `None` means "went quiet".
    observer: Rc<RefCell<Option<InfoObserver>>>,
    /// Who to tell when the program rings the terminal bell.
    bells: Rc<RefCell<Option<BellObserver>>>,
}

impl Pane {
    pub fn widget(&self) -> &gtk::Widget {
        &self.widget
    }

    /// Put keyboard focus on the terminal itself when it is embedded. The
    /// containing box is only layout; it is not the widget that receives keys.
    pub fn focus(&self) -> bool {
        #[cfg(feature = "vte")]
        if let Some(terminal) = &self.terminal {
            return terminal.grab_focus();
        }
        self.widget.grab_focus()
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    /// Watch this pane's live signals. `Some(text)` when the program's own
    /// title or its exit has something to say; `None` when it went quiet.
    pub fn set_info_observer(&self, observer: impl Fn(Option<String>) + 'static) {
        *self.observer.borrow_mut() = Some(Box::new(observer));
    }

    /// Watch for the terminal bell — how agent CLIs ask for attention.
    pub fn set_bell_observer(&self, bell: impl Fn() + 'static) {
        *self.bells.borrow_mut() = Some(Box::new(bell));
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
    ///
    /// The pane's zoom survives the reload: the new size becomes the base and
    /// the existing scale is applied on top of it.
    pub fn apply_theme(&self, theme: &Theme) {
        #[cfg(feature = "vte")]
        if let Some(terminal) = &self.terminal {
            apply_terminal_theme(terminal, theme);
            *self.base_font.borrow_mut() = (theme.font_family.clone(), theme.font_size);
            apply_scaled_font(terminal, &self.base_font, self.font_scale.get());
        }
        #[cfg(not(feature = "vte"))]
        {
            let _ = theme;
        }
    }

    /// Run `spec` in `cwd`.
    pub fn spawn(
        spec: &CommandSpec,
        cwd: &Path,
        theme: &Theme,
        title: &str,
        shift_enter: ShiftEnter,
    ) -> Pane {
        #[cfg(feature = "vte")]
        {
            Pane::spawn_embedded(spec, cwd, theme, title, shift_enter)
        }
        #[cfg(not(feature = "vte"))]
        {
            let _ = shift_enter;
            Pane::spawn_external(spec, cwd, theme, title)
        }
    }

    #[cfg(feature = "vte")]
    fn spawn_embedded(
        spec: &CommandSpec,
        cwd: &Path,
        theme: &Theme,
        title: &str,
        shift_enter: ShiftEnter,
    ) -> Pane {
        let terminal = vte4::Terminal::new();
        // The pane's own zoom state: 1.0 until Alt+= / Alt+- touch it. The
        // base font is remembered so zoom can rebuild the font later without
        // a theme, and so a theme reload re-applies the zoom on top of the
        // new size.
        let font_scale = std::rc::Rc::new(std::cell::Cell::new(1.0_f64));
        let base_font = std::rc::Rc::new(std::cell::RefCell::new((
            theme.font_family.clone(),
            theme.font_size,
        )));
        apply_terminal_theme(&terminal, theme);
        apply_scaled_font(&terminal, &base_font, font_scale.get());
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

        // Copy and paste, handled on the terminal itself. radar's chords live
        // on Alt, so the TUI's Ctrl vocabulary stays untouched.
        let keys = gtk::EventControllerKey::new();
        let terminal_for_keys = terminal.clone();
        let base_font_for_keys = base_font.clone();
        let scale_for_keys = font_scale.clone();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let alt = modifiers.contains(gtk::gdk::ModifierType::ALT_MASK);
            let copy = alt && (key == gtk::gdk::Key::c || key == gtk::gdk::Key::C);
            let paste = alt && (key == gtk::gdk::Key::v || key == gtk::gdk::Key::V);
            if copy {
                terminal_for_keys.copy_clipboard_format(vte4::Format::Text);
                return glib::Propagation::Stop;
            }
            if paste {
                terminal_for_keys.paste_clipboard();
                return glib::Propagation::Stop;
            }
            // Font zoom is per pane: Alt+= (or Alt++ — on most layouts that
            // is Alt+Shift+=) grows this pane's font, Alt+- shrinks it,
            // Alt+0 puts the theme size back. Only the Alt combinations are
            // intercepted, so the program still receives a bare `=`, `-` or
            // `0`, and keeps every Ctrl chord for itself.
            if alt
                && matches!(
                    key,
                    gtk::gdk::Key::plus | gtk::gdk::Key::equal | gtk::gdk::Key::KP_Add
                )
            {
                zoom_step(&terminal_for_keys, &base_font_for_keys, &scale_for_keys, 1.0);
                return glib::Propagation::Stop;
            }
            if alt && matches!(key, gtk::gdk::Key::minus | gtk::gdk::Key::KP_Subtract) {
                zoom_step(&terminal_for_keys, &base_font_for_keys, &scale_for_keys, -1.0);
                return glib::Propagation::Stop;
            }
            if alt && matches!(key, gtk::gdk::Key::_0 | gtk::gdk::Key::KP_0) {
                scale_for_keys.set(1.0);
                apply_scaled_font(&terminal_for_keys, &base_font_for_keys, 1.0);
                return glib::Propagation::Stop;
            }
            // VTE cannot report the shift itself, so a TUI that wants a newline
            // from Shift+Enter gets the sequence it parses.
            let shift_only = modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK)
                && !modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                && !modifiers.contains(gtk::gdk::ModifierType::ALT_MASK);
            if shift_only && (key == gtk::gdk::Key::Return || key == gtk::gdk::Key::KP_Enter) {
                match shift_enter {
                    ShiftEnter::Off => {}
                    ShiftEnter::Kitty => {
                        terminal_for_keys.feed_child(b"\x1b[13;2u");
                        return glib::Propagation::Stop;
                    }
                    ShiftEnter::Meta => {
                        terminal_for_keys.feed_child(b"\x1b\r");
                        return glib::Propagation::Stop;
                    }
                }
            }
            glib::Propagation::Proceed
        });
        terminal.add_controller(keys);

        // Ctrl+scroll zooms this pane's font — the mouse twin of the Alt+=
        // / Alt+- keys. The controller
        // only stops the event when Ctrl is held, so ordinary scrolling still
        // reaches VTE — scrollback, and mouse-aware programs keep their wheel.
        let scroll = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::DISCRETE,
        );
        // Smooth touchpad deltas arrive as fractions of a wheel notch;
        // accumulating them means one gentle swipe is one step, not ten.
        let wheel = std::cell::Cell::new(0.0_f64);
        let terminal_for_scroll = terminal.clone();
        let base_font_for_scroll = base_font.clone();
        let scale_for_scroll = font_scale.clone();
        scroll.connect_scroll(move |controller, _, dy| {
            if !controller
                .current_event_state()
                .contains(gtk::gdk::ModifierType::CONTROL_MASK)
            {
                return glib::Propagation::Proceed;
            }
            let mut acc = (wheel.get() + dy).clamp(-1.0, 1.0);
            // Scrolling up (a negative delta) grows the font, like every
            // other terminal.
            if acc <= -1.0 {
                zoom_step(
                    &terminal_for_scroll,
                    &base_font_for_scroll,
                    &scale_for_scroll,
                    1.0,
                );
                acc = 0.0;
            } else if acc >= 1.0 {
                zoom_step(
                    &terminal_for_scroll,
                    &base_font_for_scroll,
                    &scale_for_scroll,
                    -1.0,
                );
                acc = 0.0;
            }
            wheel.set(acc);
            glib::Propagation::Stop
        });
        terminal.add_controller(scroll);

        // Live signals for the pane header: what the program says it is (the
        // window title TUIs broadcast), when it is gone (child exit), and the
        // bell agent CLIs ring when they finish. The observers are installed
        // later, once the header that wants this exists; these handles let
        // the already-connected signals reach them.
        let observer: Rc<RefCell<Option<InfoObserver>>> = Rc::new(RefCell::new(None));
        let bells: Rc<RefCell<Option<BellObserver>>> = Rc::new(RefCell::new(None));
        let observer_for_title = observer.clone();
        terminal.connect_window_title_changed(move |terminal| {
            let text = terminal.window_title().map(|title| title.to_string());
            if let Some(emit) = observer_for_title.borrow().as_ref() {
                emit(text);
            }
        });
        let observer_for_exit = observer.clone();
        terminal.connect_child_exited(move |_, status| {
            if let Some(emit) = observer_for_exit.borrow().as_ref() {
                emit(Some(exit_text(status)));
            }
        });
        let bells_for_bell = bells.clone();
        terminal.connect_bell(move |_| {
            if let Some(ring) = bells_for_bell.borrow().as_ref() {
                ring();
            }
        });

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
            let extra_env: Vec<String> = spec_for_spawn
                .env_set
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect();
            let command = spec_for_spawn.display();
            let cwd_string = cwd.to_string_lossy().to_string();
            let problem = problem.clone();
            let fallback = fallback.clone();
            let trouble = trouble.clone();
            let title = title.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(50), move || {
                try_spawn(
                    terminal,
                    argv,
                    extra_env,
                    cwd_string,
                    problem,
                    fallback,
                    trouble,
                    title,
                    command,
                    0,
                );
            });
        });

        Pane {
            widget: container.upcast(),
            terminal: Some(terminal),
            command: spec.display(),
            font_scale,
            base_font,
            observer,
            bells,
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
        outer.set_focusable(true);
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
            observer: Rc::new(RefCell::new(None)),
            bells: Rc::new(RefCell::new(None)),
        }
    }
}

/// What a pane's header says once its program is gone. `status` is the
/// waitpid status VTE reports: exit code in the high byte, signal in the low
/// one.
#[cfg(feature = "vte")]
fn exit_text(status: i32) -> String {
    use std::os::unix::process::ExitStatusExt;
    let status = std::process::ExitStatus::from_raw(status);
    match status.code() {
        Some(0) => "exited".to_string(),
        Some(code) => format!("exited ({code})"),
        None => match status.signal() {
            Some(signal) => format!("killed by signal {signal}"),
            None => "exited".to_string(),
        },
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
    extra_env: Vec<String>,
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
    let extra_env_for_retry = extra_env.clone();
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
    // agents' own configuration come along — plus whatever this launch adds
    // (an agent's `RADAR_AGENT`, the name it claims board work under).
    let mut envv: Vec<String> = vec![
        "TERM=xterm-256color".to_string(),
        "COLORTERM=truecolor".to_string(),
    ];
    envv.extend(extra_env);
    let env_refs: Vec<&str> = envv.iter().map(String::as_str).collect();
    terminal.spawn_async(
        vte4::PtyFlags::DEFAULT,
        // Deliberately None: the wrapper cds. See above.
        None,
        &argv_refs,
        &env_refs,
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
                                extra_env_for_retry,
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
                            env_set: Vec::new(),
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
    // Colours only: the font is applied separately, scaled by each pane's
    // own zoom.
    let palette: Vec<&gtk::gdk::RGBA> = theme.palette.iter().collect();
    terminal.set_colors(Some(&theme.foreground), Some(&theme.background), &palette);
}

/// Zoom limits, as multiples of the theme's font size.
#[cfg(feature = "vte")]
const MIN_FONT_SCALE: f64 = 0.25;
#[cfg(feature = "vte")]
const MAX_FONT_SCALE: f64 = 4.0;

/// The scale after one zoom step: `direction > 0` grows the font, `< 0`
/// shrinks it.
///
/// Steps are multiplicative, like every other terminal, and rounded to two
/// decimals so zooming out as many times as in lands exactly back on 1.0.
#[cfg(feature = "vte")]
fn next_scale(current: f64, direction: f64) -> f64 {
    const STEP: f64 = 1.1;
    let factor = if direction > 0.0 { STEP } else { 1.0 / STEP };
    ((current * factor * 100.0).round() / 100.0).clamp(MIN_FONT_SCALE, MAX_FONT_SCALE)
}

/// Apply one zoom step to a terminal: recompute the scale, then redraw.
#[cfg(feature = "vte")]
fn zoom_step(
    terminal: &vte4::Terminal,
    base_font: &std::cell::RefCell<(String, f64)>,
    scale: &std::cell::Cell<f64>,
    direction: f64,
) {
    let zoom = next_scale(scale.get(), direction);
    scale.set(zoom);
    apply_scaled_font(terminal, base_font, zoom);
}

/// Draw a terminal with its base font scaled by `zoom`.
#[cfg(feature = "vte")]
fn apply_scaled_font(
    terminal: &vte4::Terminal,
    base_font: &std::cell::RefCell<(String, f64)>,
    zoom: f64,
) {
    let base = base_font.borrow();
    let mut font = gtk::pango::FontDescription::new();
    font.set_family(&base.0);
    font.set_size((base.1 * zoom * gtk::pango::SCALE as f64) as i32);
    terminal.set_font(Some(&font));
}

#[cfg(all(test, feature = "vte"))]
mod tests {
    use super::*;

    #[test]
    fn exit_text_reads_exit_codes_and_signals() {
        assert_eq!(exit_text(0 << 8), "exited");
        assert_eq!(exit_text(1 << 8), "exited (1)");
        assert_eq!(exit_text(9), "killed by signal 9");
    }

    #[test]
    fn zoom_round_trips_back_to_the_theme_size() {
        let mut scale = 1.0;
        for _ in 0..7 {
            scale = next_scale(scale, 1.0);
        }
        for _ in 0..7 {
            scale = next_scale(scale, -1.0);
        }
        assert!((scale - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn zoom_stays_within_limits() {
        let mut scale = 1.0;
        for _ in 0..60 {
            scale = next_scale(scale, 1.0);
        }
        assert_eq!(scale, MAX_FONT_SCALE);
        let mut scale = 1.0;
        for _ in 0..60 {
            scale = next_scale(scale, -1.0);
        }
        assert_eq!(scale, MIN_FONT_SCALE);
    }
}
