//! One tab's content: a program running in a terminal.
//!
//! With the `vte` feature the daemon owns the program, PTY, scrollback and
//! authoritative terminal state. The pane attaches VTE through a local PTY
//! adapter: VTE renders the snapshot replay and raw stream, while its input is
//! forwarded to the daemon. Without the feature we degrade to a card that can
//! launch the same command in a separate terminal window.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

use adw::prelude::*;
#[cfg(feature = "vte")]
use gtk::glib;
#[cfg(feature = "vte")]
use vte4::prelude::*;

use super::theme::Theme;
use crate::programs::CommandSpec;
#[cfg(feature = "vte")]
use crate::session::{
    client::{ClientEvent, RemoteSession},
    Dims, ExitInfo,
};
#[cfg(feature = "vte")]
use std::os::fd::{FromRawFd, OwnedFd as OwnedFdT};

/// Who a pane's live signals talk to: the header routing in mod.rs.
type InfoObserver = Box<dyn Fn(Option<String>)>;
type BellObserver = Box<dyn Fn()>;
/// Who to tell when the program has exited, after the header was told.
type ExitHandler = Box<dyn Fn()>;

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

/// What a re-launch needs: the handles the first spawn used. The widget
/// and its session slot live on the pane; the rest is per pane, kept so
/// [`Pane::respawn`] can run an exited program again exactly as the
/// spawn-on-first-map path would.
#[cfg(feature = "vte")]
#[derive(Clone)]
struct Relaunch {
    cwd: std::path::PathBuf,
    title: String,
    session_home: std::path::PathBuf,
    session_id: String,
    problem: gtk::Label,
    fallback: gtk::Button,
    trouble: gtk::Box,
    events: async_channel::Sender<ClientEvent>,
    /// The spawn-on-first-map guard: a respawn pre-empts it.
    spawned: Rc<Cell<bool>>,
}

/// A running (or launchable) program in a tab.
pub struct Pane {
    widget: gtk::Widget,
    #[cfg(feature = "vte")]
    terminal: Option<vte4::Terminal>,
    /// The session behind the terminal: the pty, the child, the
    /// authoritative state. `None` until the pane's first map spawns
    /// it — and forever when the spawn failed.
    #[cfg(feature = "vte")]
    session: Rc<RefCell<Option<RemoteSession>>>,
    /// What a re-launch needs: the handles the first spawn used, so an
    /// exited program can run again over the same widget.
    #[cfg(feature = "vte")]
    relaunch: RefCell<Option<Relaunch>>,
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
    /// Who to tell when the program has exited, after the header was
    /// told. How an agent's conversation gets bound to its board claim.
    exit_hook: Rc<RefCell<Option<ExitHandler>>>,
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

    /// Watch for the program's exit, once the header has been told. How
    /// radar binds the conversation an agent had to its board claim: the
    /// CLI's session store is asked while the record is fresh.
    pub fn set_exit_handler(&self, handler: impl Fn() + 'static) {
        *self.exit_hook.borrow_mut() = Some(Box::new(handler));
    }

    /// Does this pane hold a live terminal?
    #[allow(dead_code)]
    pub fn is_live(&self) -> bool {
        #[cfg(feature = "vte")]
        {
            self.session.borrow().is_some()
        }
        #[cfg(not(feature = "vte"))]
        {
            false
        }
    }

    /// The running program's process id, when one runs. How the board's
    /// @claims find the exact tab: the child's own `RADAR_AGENT` names it.
    pub fn session_pid(&self) -> Option<u32> {
        #[cfg(feature = "vte")]
        {
            self.session
                .borrow()
                .as_ref()
                .and_then(RemoteSession::process_id)
        }
        #[cfg(not(feature = "vte"))]
        {
            None
        }
    }

    /// Run the program again in this pane: a fresh session over the same
    /// widget, whose scrollback survives. How a claimed card's agent
    /// re-opens — the tab keeps its place, the program comes back with
    /// its own resume flags and a fresh claim stamp.
    #[cfg(feature = "vte")]
    pub fn respawn(&self, spec: &CommandSpec) {
        let Some(launch) = self.relaunch.borrow().clone() else {
            return;
        };
        launch.spawned.set(true); // the on-map first spawn never fires
        launch.trouble.set_visible(false);
        let Some(terminal) = self.terminal.as_ref() else {
            return;
        };
        start_session(
            terminal.clone(),
            Rc::clone(&self.session),
            spec,
            &launch.cwd,
            launch.events,
            launch.problem,
            launch.fallback,
            launch.trouble,
            launch.title,
            &launch.session_home,
            &launch.session_id,
            true,
        );
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
        session_home: &Path,
        session_id: &str,
    ) -> Pane {
        #[cfg(feature = "vte")]
        {
            Pane::spawn_embedded(
                spec,
                cwd,
                theme,
                title,
                shift_enter,
                session_home,
                session_id,
            )
        }
        #[cfg(not(feature = "vte"))]
        {
            let _ = shift_enter;
            let _ = (session_home, session_id);
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
        session_home: &Path,
        session_id: &str,
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
                zoom_step(
                    &terminal_for_keys,
                    &base_font_for_keys,
                    &scale_for_keys,
                    1.0,
                );
                return glib::Propagation::Stop;
            }
            if alt && matches!(key, gtk::gdk::Key::minus | gtk::gdk::Key::KP_Subtract) {
                zoom_step(
                    &terminal_for_keys,
                    &base_font_for_keys,
                    &scale_for_keys,
                    -1.0,
                );
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

        // Live signals for the pane header: what the program says it is
        // (the window title TUIs broadcast) and the bell agent CLIs ring
        // when they finish. Both come from the widget's parse of the
        // stream, unchanged. Exit comes from the session instead: the
        // session owns the child now. The observers are installed
        // later, once the header that wants this exists; these handles
        // let the already-connected signals reach them.
        let observer: Rc<RefCell<Option<InfoObserver>>> = Rc::new(RefCell::new(None));
        let bells: Rc<RefCell<Option<BellObserver>>> = Rc::new(RefCell::new(None));
        let observer_for_title = observer.clone();
        terminal.connect_window_title_changed(move |terminal| {
            let text = terminal.window_title().map(|title| title.to_string());
            if let Some(emit) = observer_for_title.borrow().as_ref() {
                emit(text);
            }
        });
        let bells_for_bell = bells.clone();
        terminal.connect_bell(move |_| {
            if let Some(ring) = bells_for_bell.borrow().as_ref() {
                ring();
            }
        });

        // Session lifecycle events arrive independently of the byte stream.
        let exit_hook: Rc<RefCell<Option<ExitHandler>>> = Rc::new(RefCell::new(None));
        let (event_sender, event_receiver) = async_channel::unbounded::<ClientEvent>();
        let observer_for_exit = observer.clone();
        let hook_for_exit = exit_hook.clone();
        let problem_for_event = problem.clone();
        let trouble_for_event = trouble.clone();
        let fallback_for_event = fallback.clone();
        let fallback_spec = spec.clone();
        let fallback_cwd = cwd.to_path_buf();
        glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = event_receiver.recv().await {
                match event {
                    ClientEvent::Exit(info) => {
                        if let Some(emit) = observer_for_exit.borrow().as_ref() {
                            emit(Some(exit_text(info)));
                        }
                        if let Some(hook) = hook_for_exit.borrow().as_ref() {
                            hook();
                        }
                    }
                    ClientEvent::Failed(message) => {
                        problem_for_event.set_text(&message);
                        let spec = fallback_spec.clone();
                        let cwd = fallback_cwd.clone();
                        fallback_for_event.connect_clicked(move |_| {
                            if let Err(error) = spawn_external_window(&spec, &cwd) {
                                eprintln!("radar: could not open a terminal: {error}");
                            }
                        });
                        fallback_for_event.set_visible(true);
                        trouble_for_event.set_visible(true);
                    }
                }
            }
        });

        // Spawn on first map, not at construction. Two reasons: the
        // grid is sized from a widget that now has its real size, and
        // tabs that are never opened do not start a process at all.
        let spawned = std::rc::Rc::new(std::cell::Cell::new(false));
        let spec_for_spawn = spec.clone();
        let cwd = cwd.to_path_buf();
        let title = title.to_string();
        let session_home = session_home.to_path_buf();
        let session_id = session_id.to_string();
        let session_slot: Rc<RefCell<Option<RemoteSession>>> = Rc::new(RefCell::new(None));
        let terminal_for_spawn: vte4::Terminal = terminal.clone();
        let session_for_map = session_slot.clone();
        let relaunch = Relaunch {
            cwd: cwd.clone(),
            title: title.clone(),
            session_home: session_home.clone(),
            session_id: session_id.clone(),
            problem: problem.clone(),
            fallback: fallback.clone(),
            trouble: trouble.clone(),
            events: event_sender.clone(),
            spawned: spawned.clone(),
        };
        terminal.connect_map(move |_widget| {
            if spawned.get() {
                return;
            }
            spawned.set(true);
            // The signal handler is `Fn`, so everything the spawn needs
            // is cloned here rather than moved out of the captures.
            let terminal = terminal_for_spawn.clone();
            let spec = spec_for_spawn.clone();
            let cwd = cwd.clone();
            let title = title.clone();
            let session_home = session_home.clone();
            let session_id = session_id.clone();
            let problem = problem.clone();
            let fallback = fallback.clone();
            let trouble = trouble.clone();
            let events = event_sender.clone();
            let slot = session_for_map.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(50), move || {
                start_session(
                    terminal,
                    slot,
                    &spec,
                    &cwd,
                    events,
                    problem,
                    fallback,
                    trouble,
                    title,
                    &session_home,
                    &session_id,
                    false,
                );
            });
        });

        Pane {
            widget: container.upcast(),
            terminal: Some(terminal),
            session: session_slot,
            relaunch: RefCell::new(Some(relaunch)),
            command: spec.display(),
            font_scale,
            base_font,
            observer,
            bells,
            exit_hook,
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
            #[cfg(feature = "vte")]
            session: Rc::new(RefCell::new(None)),
            command: spec.display(),
            observer: Rc::new(RefCell::new(None)),
            bells: Rc::new(RefCell::new(None)),
            exit_hook: Rc::new(RefCell::new(None)),
        }
    }
}

/// What a pane's header says once its program is gone.
#[cfg(feature = "vte")]
fn exit_text(info: ExitInfo) -> String {
    match info.signal {
        // strsignal's own words: "Terminated", "Killed", "Segmentation fault".
        Some(signal) => signal,
        None => match info.code {
            0 => "exited".to_string(),
            code => format!("exited ({code})"),
        },
    }
}

/// Start the session behind a terminal pane, and hand the widget its
/// end of the bridge.
///
/// Failure is visible, not fatal: if the program cannot be started, the
/// pane says so and offers the same command in the user's own terminal,
/// which always works.
#[cfg(feature = "vte")]
#[allow(clippy::too_many_arguments)]
fn start_session(
    terminal: vte4::Terminal,
    slot: Rc<RefCell<Option<RemoteSession>>>,
    spec: &CommandSpec,
    cwd: &Path,
    events: async_channel::Sender<ClientEvent>,
    problem: gtk::Label,
    fallback: gtk::Button,
    trouble: gtk::Box,
    title: String,
    session_home: &Path,
    session_id: &str,
    replace_existing: bool,
) {
    // A re-launch detaches the prior view first; Stop/Forget/Create happens
    // through the daemon connection and never depends on widget ownership.
    slot.borrow_mut().take();
    terminal.set_pty(None::<&vte4::Pty>);
    terminal.reset(true, true);
    let cols = terminal.column_count();
    let rows = terminal.row_count();
    let dims = if cols > 0 && rows > 0 {
        Dims {
            cols: cols as u16,
            rows: rows as u16,
        }
    } else {
        Dims { cols: 80, rows: 24 }
    };

    let command = spec.display();
    let cwd_string = cwd.to_string_lossy().to_string();
    let spawn = crate::session::registry::Spawn {
        id: session_id.to_string(),
        argv: spec.argv.clone(),
        cwd: cwd.to_path_buf(),
        env: spec.env_set.clone(),
        env_remove: spec.env_unset.clone(),
        dims,
    };
    let mut failure = None;
    match RemoteSession::attach(
        session_home,
        session_id.to_string(),
        spawn,
        replace_existing,
        move |event| {
            let _ = events.try_send(event);
        },
    ) {
        Ok(session) => {
            let raw_fd = unsafe { libc::dup(session.client_fd()) };
            if raw_fd < 0 {
                failure = Some(format!(
                    "could not duplicate renderer PTY: {}",
                    std::io::Error::last_os_error()
                ));
            } else {
                let fd = unsafe { OwnedFdT::from_raw_fd(raw_fd) };
                match vte4::Pty::foreign_sync(fd, None::<&gtk::gio::Cancellable>) {
                    Ok(pty) => {
                        terminal.set_pty(Some(&pty));
                        slot.borrow_mut().replace(session);
                    }
                    Err(error) => {
                        failure = Some(format!(
                            "the terminal widget refused the daemon attachment: {error}"
                        ))
                    }
                }
            }
        }
        Err(error) => failure = Some(error.to_string()),
    }

    if let Some(reason) = failure {
        problem.set_text(&format!(
            "Could not start {title} inside radar: {reason}\n\
             The same program in your own terminal:\n  cd {cwd_string}\n  {command}"
        ));
        fallback.connect_clicked({
            let spec = spec.clone();
            let cwd = cwd.to_path_buf();
            move |_| {
                if let Err(error) = spawn_external_window(&spec, &cwd) {
                    eprintln!("radar: could not open a terminal: {error}");
                }
            }
        });
        trouble.set_visible(true);
    }
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
        let mut command = Command::new("omarchy-launch-tui");
        command.args(&spec.argv).current_dir(&cwd);
        apply_command_environment(&mut command, spec);
        return command.spawn().map(|_| ());
    }
    for (program, flag) in [
        ("alacritty", "-e"),
        ("foot", "-e"),
        ("kitty", "-e"),
        ("ghostty", "-e"),
    ] {
        if crate::config::have(program) {
            let mut command = Command::new(program);
            command
                .arg("--working-directory")
                .arg(&cwd)
                .arg(flag)
                .args(&spec.argv);
            apply_command_environment(&mut command, spec);
            return command.spawn().map(|_| ());
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no terminal emulator found (looked for omarchy-launch-tui, alacritty, foot, kitty, ghostty)",
    ))
}

fn apply_command_environment(command: &mut std::process::Command, spec: &CommandSpec) {
    for name in &spec.env_unset {
        command.env_remove(name);
    }
    command.envs(spec.env_set.iter().map(|(name, value)| (name, value)));
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
    fn external_launch_environment_preserves_radar_session_identity() {
        let spec = CommandSpec {
            argv: vec!["agent".into()],
            env_unset: vec!["COLORTERM".into()],
            env_set: vec![
                ("RADAR_PROJECT_ID".into(), "12".into()),
                ("RADAR_SESSION_ID".into(), "project-12-agent-0-agent".into()),
            ],
        };
        let mut command = std::process::Command::new("agent");
        apply_command_environment(&mut command, &spec);
        let environment: std::collections::HashMap<_, _> = command
            .get_envs()
            .map(|(name, value)| (name.to_os_string(), value.map(|value| value.to_os_string())))
            .collect();
        assert_eq!(
            environment.get(std::ffi::OsStr::new("COLORTERM")),
            Some(&None)
        );
        assert_eq!(
            environment.get(std::ffi::OsStr::new("RADAR_PROJECT_ID")),
            Some(&Some("12".into()))
        );
        assert_eq!(
            environment.get(std::ffi::OsStr::new("RADAR_SESSION_ID")),
            Some(&Some("project-12-agent-0-agent".into()))
        );
    }

    #[test]
    fn exit_text_reads_exit_codes_and_signals() {
        let exit = |code: u32, signal: Option<&str>| ExitInfo {
            code,
            signal: signal.map(str::to_string),
        };
        assert_eq!(exit_text(exit(0, None)), "exited");
        assert_eq!(exit_text(exit(1, None)), "exited (1)");
        assert_eq!(exit_text(exit(1, Some("Killed"))), "Killed");
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
