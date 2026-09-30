//! One tab's content: a program running in a terminal.
//!
//! The daemon owns the program, PTY, scrollback and authoritative terminal
//! state. The pane decodes the daemon's lossless libghostty-vt snapshot into a
//! local engine, feeds the raw stream after the watermark, and paints its
//! `RenderState`; input and resizes are forwarded to the daemon.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use super::term::TerminalView;
use super::theme::Theme;
use crate::programs::CommandSpec;
use crate::session::{
    client::{ClientEvent, RemoteSession},
    Dims, ExitInfo,
};

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
    pub fn for_slot(slot: crate::db::Slot) -> ShiftEnter {
        if slot != crate::db::Slot::Agent {
            return ShiftEnter::Off;
        }
        // An escape hatch that does not depend on configuration.
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
    /// The libghostty-vt view.
    view: Option<TerminalView>,
    /// The session behind the terminal. `None` until the pane's first map
    /// spawns it — and forever when the spawn failed.
    session: Rc<RefCell<Option<RemoteSession>>>,
    /// What a re-launch needs, so an exited program can run again over the
    /// same widget.
    relaunch: RefCell<Option<Relaunch>>,
    command: String,
    /// Who to tell when this pane's program reports something live: its own
    /// window title, or that it has exited. `None` means "went quiet".
    observer: Rc<RefCell<Option<InfoObserver>>>,
    /// Who to tell when the program rings the terminal bell.
    bells: Rc<RefCell<Option<BellObserver>>>,
    /// Who to tell when the program has exited, after the header was told.
    exit_hook: Rc<RefCell<Option<ExitHandler>>>,
}

impl Pane {
    pub fn widget(&self) -> &gtk::Widget {
        &self.widget
    }

    /// Put keyboard focus on the terminal itself when it is embedded. The
    /// containing box is only layout; it is not the widget that receives keys.
    pub fn focus(&self) -> bool {
        if let Some(view) = &self.view {
            return view.widget().grab_focus();
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

    /// Watch for the program's exit, once the header has been told.
    pub fn set_exit_handler(&self, handler: impl Fn() + 'static) {
        *self.exit_hook.borrow_mut() = Some(Box::new(handler));
    }

    /// Does this pane hold a live terminal?
    #[allow(dead_code)]
    pub fn is_live(&self) -> bool {
        self.session.borrow().is_some()
    }

    /// The running program's process id, when one runs.
    pub fn session_pid(&self) -> Option<u32> {
        self.session
            .borrow()
            .as_ref()
            .and_then(RemoteSession::process_id)
    }

    /// Run the program again in this pane: a fresh session over the same
    /// widget, whose scrollback survives.
    pub fn respawn(&self, spec: &CommandSpec) {
        let Some(launch) = self.relaunch.borrow().clone() else {
            return;
        };
        launch.spawned.set(true); // the on-map first spawn never fires
        launch.trouble.set_visible(false);
        if let Some(view) = self.view.as_ref() {
            start_ghostty_session(
                view.clone(),
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
    }

    /// Apply a freshly loaded theme (used when omarchy switches themes).
    pub fn apply_theme(&self, theme: &Theme) {
        if let Some(view) = &self.view {
            view.apply_font(&theme.font_family, theme.font_size);
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
        Pane::spawn_ghostty(
            spec,
            cwd,
            theme,
            title,
            shift_enter,
            session_home,
            session_id,
        )
    }

    /// The pane owns a local terminal engine and paints its `RenderState`.
    /// The daemon hands it a lossless snapshot and the raw stream.
    #[allow(clippy::too_many_arguments)]
    fn spawn_ghostty(
        spec: &CommandSpec,
        cwd: &Path,
        theme: &Theme,
        title: &str,
        shift_enter: ShiftEnter,
        session_home: &Path,
        session_id: &str,
    ) -> Pane {
        let view = TerminalView::new(&theme.font_family, theme.font_size);
        view.set_shift_enter(shift_enter);

        let problem = gtk::Label::new(None);
        problem.add_css_class("error");
        problem.add_css_class("caption");
        problem.set_wrap(true);
        problem.set_selectable(true);
        problem.set_xalign(0.0);

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
        container.append(view.widget());

        let observer: Rc<RefCell<Option<InfoObserver>>> = Rc::new(RefCell::new(None));
        let bells: Rc<RefCell<Option<BellObserver>>> = Rc::new(RefCell::new(None));
        let exit_hook: Rc<RefCell<Option<ExitHandler>>> = Rc::new(RefCell::new(None));

        let (event_sender, event_receiver) = async_channel::unbounded::<ClientEvent>();
        let view_for_events = view.clone();
        let observer_for_events = observer.clone();
        let bells_for_events = bells.clone();
        let hook_for_events = exit_hook.clone();
        let problem_for_event = problem.clone();
        let trouble_for_event = trouble.clone();
        let fallback_for_event = fallback.clone();
        let fallback_spec = spec.clone();
        let fallback_cwd = cwd.to_path_buf();
        glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = event_receiver.recv().await {
                match event {
                    ClientEvent::Snapshot(bytes) => view_for_events.load_snapshot(&bytes),
                    ClientEvent::Bytes(bytes) => view_for_events.feed(&bytes),
                    ClientEvent::Title(title) => {
                        if let Some(emit) = observer_for_events.borrow().as_ref() {
                            emit(title);
                        }
                    }
                    ClientEvent::Bell => {
                        if let Some(ring) = bells_for_events.borrow().as_ref() {
                            ring();
                        }
                    }
                    ClientEvent::Exit(info) => {
                        if let Some(emit) = observer_for_events.borrow().as_ref() {
                            emit(Some(exit_text(info)));
                        }
                        if let Some(hook) = hook_for_events.borrow().as_ref() {
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

        let spawned = std::rc::Rc::new(std::cell::Cell::new(false));
        let session_slot: Rc<RefCell<Option<RemoteSession>>> = Rc::new(RefCell::new(None));
        let relaunch = Relaunch {
            cwd: cwd.to_path_buf(),
            title: title.to_string(),
            session_home: session_home.to_path_buf(),
            session_id: session_id.to_string(),
            problem: problem.clone(),
            fallback: fallback.clone(),
            trouble: trouble.clone(),
            events: event_sender.clone(),
            spawned: spawned.clone(),
        };
        let view_for_map = view.clone();
        let spec_for_map = spec.clone();
        let cwd_for_map = cwd.to_path_buf();
        let title_for_map = title.to_string();
        let home_for_map = session_home.to_path_buf();
        let id_for_map = session_id.to_string();
        let slot_for_map = session_slot.clone();
        view.widget().connect_map(move |_| {
            if spawned.get() {
                return;
            }
            spawned.set(true);
            let view = view_for_map.clone();
            let spec = spec_for_map.clone();
            let cwd = cwd_for_map.clone();
            let title = title_for_map.clone();
            let session_home = home_for_map.clone();
            let session_id = id_for_map.clone();
            let problem = problem.clone();
            let fallback = fallback.clone();
            let trouble = trouble.clone();
            let events = event_sender.clone();
            let slot = slot_for_map.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(50), move || {
                start_ghostty_session(
                    view,
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
            view: Some(view),
            session: session_slot,
            relaunch: RefCell::new(Some(relaunch)),
            command: spec.display(),
            observer,
            bells,
            exit_hook,
        }
    }
}

/// What a pane's header says once its program is gone.
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

/// Start a libghostty-vt pane's session: attach to the daemon, point the view's
/// input at the attachment, and let snapshots/bytes arrive as events.
#[allow(clippy::too_many_arguments)]
fn start_ghostty_session(
    view: TerminalView,
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
    slot.borrow_mut().take();
    let command = spec.display();
    let cwd_string = cwd.to_string_lossy().to_string();
    let spawn = crate::session::registry::Spawn {
        id: session_id.to_string(),
        argv: spec.argv.clone(),
        cwd: cwd.to_path_buf(),
        env: spec.env_set.clone(),
        env_remove: spec.env_unset.clone(),
        dims: Dims {
            cols: crate::session::DEFAULT_COLS,
            rows: crate::session::DEFAULT_ROWS,
        },
    };
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
            let fd = session.client_fd();
            view.set_input(move |bytes| write_fd(fd, bytes));
            view.set_resize(move |cols, rows| set_winsize(fd, cols, rows));
            slot.borrow_mut().replace(session);
        }
        Err(error) => {
            problem.set_text(&format!(
                "Could not start {title} inside radar: {error}\n\
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
}

/// Write input to the attachment's bridge, dropping it if the pipe would block
/// rather than stalling the GTK main loop.
fn write_fd(fd: libc::c_int, bytes: &[u8]) {
    let mut rest = bytes;
    while !rest.is_empty() {
        let written = unsafe { libc::write(fd, rest.as_ptr().cast(), rest.len()) };
        if written > 0 {
            rest = &rest[written as usize..];
            continue;
        }
        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        break;
    }
}

/// Report the pane's grid size to the bridge; the client's input loop forwards
/// the change to the daemon.
fn set_winsize(fd: libc::c_int, cols: u16, rows: u16) {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) };
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
    // The launcher and the program both come from the ambient PATH, which a
    // desktop- or service-launched radar may not share; the mise entries
    // make agents installed under the user's home reachable here too.
    command.env("PATH", crate::config::path_value());
}

#[cfg(test)]
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
}
