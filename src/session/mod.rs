//! A program in a pty, owned away from any particular view.
//!
//! [`registry`] and [`daemon`] implement the daemon-owned runtime and local
//! snapshot/stream API; [`client`] attaches a VTE renderer through a local PTY
//! adapter. The `Session` bridge below remains as the phase-1 compatibility
//! implementation used by probes and its original unit tests, not the GUI's
//! process owner.
//!
//! This is the seam for the client/server split: everything here is
//! GTK-free, so a session can outlive the widget that renders it — which
//! is what a daemon will need. The shape is the same one tmux and
//! Superlogical use: the server owns the pty and the authoritative
//! parsed state of the screen; the client renders the raw byte stream
//! with its own terminal engine.
//!
//! Today the client is the embedded VTE widget. It keeps every bit of
//! its input machinery — key encoding, mouse reporting, paste, IME — by
//! being handed a pty it treats as its own, but that pty is a bridge:
//!
//! ```text
//!                program
//!                   │ slave ¹
//!                real pty ── master ──┐
//!                                     │  bridge thread:
//!                                     │  real→fake relays output
//!                                     │  fake→real relays input
//!                                     │  mirrors resizes
//!                                     ▼
//!                fake pty ── master ──┘
//!                   │ dup            │ slave ²
//!                 VTE widget ³
//! ```
//!
//! ¹ the program's controlling terminal, spawned by portable-pty.
//! ² the bridge reads what the widget writes (keystrokes) here.
//! ³ a `foreign_sync` dup of the fake master: the widget reads output
//!   from it, writes input to it, and sizes it — the widget believes it
//!   owns a real terminal.
//!
//! The bridge also maintains the authoritative screen state with
//! alacritty's terminal engine: the same byte stream the widget renders
//! is parsed into a grid here. In phase 1 nothing reads that grid; it is
//! what a snapshot-then-stream reattach protocol (phase 2) and a
//! headless `attach` renderer (phase 3) will be built on.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{self, Processor};
use anyhow::{anyhow, bail, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

pub mod activity;
/// Legacy markdown board monitor. Retained until the markdown path is deleted
/// (it is no longer started: the board store is authoritative).
#[allow(dead_code)]
pub(super) mod board_monitor;
pub mod board_store;
pub mod catalog;
#[cfg(feature = "vte")]
pub mod client;
pub mod daemon;
pub mod registry;
mod schema;

/// The default grid size, before the client reports its own. Every
/// terminal starts here if nothing better is known in time.
pub const DEFAULT_COLS: u16 = 80;
pub const DEFAULT_ROWS: u16 = 24;

/// Scrollback the authoritative state keeps — matched to the widget's
/// own `set_scrollback_lines(10_000)`, so both ends agree on history.
const SCROLLBACK_LINES: usize = 10_000;

/// How long one `poll` waits before waking to check for resizes and
/// child exit. Small enough that a resize never feels late, large
/// enough that an idle session costs nothing.
const BRIDGE_POLL_MS: libc::c_int = 100;

/// What a session tells the world, in order.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Raw bytes the program wrote, in order. This is the stream a
    /// client renders — the widget today, a remote client later.
    Raw(Vec<u8>),
    /// The program set the window title (OSC 0 / 2).
    Title(String),
    /// The program reset the window title.
    ResetTitle,
    /// The program rang the terminal bell.
    Bell,
    /// The program offered clipboard content (OSC 52).
    ClipboardStore(String),
    /// The program is gone.
    Exit(ExitInfo),
}

/// How the program ended. `signal`, when present, is the readable name
/// from `strsignal` — "Terminated", "Killed", "Segmentation fault".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExitInfo {
    pub code: u32,
    pub signal: Option<String>,
}

/// The `EventListener` the authoritative state talks to.
///
/// Queries that want a reply written back to the program (DSR, DA,
/// colour requests — `Event::PtyWrite` and friends) are deliberately
/// left unanswered: the client's terminal engine sees the same bytes in
/// the stream and answers them itself, and answering twice would feed
/// the program duplicate replies. A headless session with no client
/// engine will need to answer instead; that is phase 2's job, behind a
/// flag on this type.
pub struct Proxy {
    emit: Arc<dyn Fn(SessionEvent) + Send + Sync>,
}

impl EventListener for Proxy {
    fn send_event(&self, event: Event) {
        let message = match event {
            Event::Title(title) => SessionEvent::Title(title),
            Event::ResetTitle => SessionEvent::ResetTitle,
            Event::Bell => SessionEvent::Bell,
            Event::ClipboardStore(_, text) => SessionEvent::ClipboardStore(text),
            _ => return,
        };
        (self.emit)(message);
    }
}

/// Grid dimensions for the authoritative state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Dims {
    pub cols: u16,
    pub rows: u16,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}

/// An owned raw descriptor, closed on drop. The bridge works with raw
/// descriptors — it polls and reads the master ends directly — so the
/// guards live on the session and outlive the thread.
struct FdGuard(libc::c_int);

impl Drop for FdGuard {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// Everything the bridge thread and the session handle share.
struct Inner {
    /// The real master: writing here sends input to the program.
    real_master: libc::c_int,
    /// The bridge pty: writing to the master delivers output to the
    /// widget; reading the slave collects the widget's input.
    fake_master: libc::c_int,
    fake_slave: libc::c_int,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    /// The authoritative screen state, parsed from the same bytes the
    /// client renders.
    term: Mutex<Term<Proxy>>,
    /// The last size mirrored onto the program, so neither the bridge
    /// nor [`Session::resize`] repeats work.
    mirrored: Mutex<Option<Dims>>,
    stop: AtomicBool,
    emit: Arc<dyn Fn(SessionEvent) + Send + Sync>,
}

impl Inner {
    /// Push a grid size onto the program: resize the real pty (the
    /// kernel delivers SIGWINCH) and the authoritative state with it.
    fn resize(&self, dims: Dims) {
        if *self.mirrored.lock().unwrap_or_else(|e| e.into_inner()) == Some(dims) {
            return;
        }
        let winsize = libc::winsize {
            ws_row: dims.rows,
            ws_col: dims.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(self.real_master, libc::TIOCSWINSZ, &winsize);
        }
        {
            let mut term = self.term.lock().unwrap_or_else(|e| e.into_inner());
            term.resize(dims);
        }
        *self.mirrored.lock().unwrap_or_else(|e| e.into_inner()) = Some(dims);
    }
}

/// A running program in a pty, plus the bridge that feeds its client.
pub struct Session {
    /// The program's pty master. Dropping it closes the pty, which
    /// SIGHUPs the program — the same semantics a terminal widget's own
    /// pty has always had. Dropped explicitly in `Drop`, after the
    /// bridge stops and before the reap, so the order is: bridge joins,
    /// hangup lands, child is reaped.
    master: Option<Box<dyn MasterPty + Send>>,
    inner: Arc<Inner>,
    /// The client-facing end: the widget wraps this in a foreign
    /// `vte4::Pty`. A dup of `inner.fake_master`, so the bridge and the
    /// widget hold separate descriptors of one end.
    client_fd: FdGuard,
    bridge: Option<JoinHandle<()>>,
}

impl Session {
    /// Start `argv` in `cwd` inside its own pty and spawn the bridge.
    ///
    /// `emit` receives the session's events in order; drop-semantics
    /// keep working as before — when the session goes, the pty closes
    /// and the program is SIGHUPed.
    pub fn spawn(
        argv: &[String],
        env_set: &[(String, String)],
        cwd: &Path,
        dims: Dims,
        emit: impl Fn(SessionEvent) + Send + Sync + 'static,
    ) -> Result<Session> {
        let Some((program, args)) = argv.split_first() else {
            bail!("cannot start an empty command");
        };
        let emit = Arc::new(emit);

        // The bridge pty: what the widget will believe is its own pty.
        // The bridge relays through the slave end — reading the
        // widget's keystrokes from it, writing the program's output to
        // it — so the slave is set to raw mode: no echo loop, no
        // canonical buffering, no signals swallowed before they reach
        // the program, whose own tty does that work. Both ends are
        // CLOEXEC so the program never inherits them.
        let (fake_master, fake_slave) = open_pty_pair(dims)?;
        for fd in [fake_master, fake_slave] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if flags == -1
                || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1
            {
                bail!("fcntl on bridge pty: {}", std::io::Error::last_os_error());
            }
        }
        unsafe {
            let mut tio: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fake_slave, &mut tio) == 0 {
                libc::cfmakeraw(&mut tio);
                libc::tcsetattr(fake_slave, libc::TCSANOW, &tio);
            }
        }

        // The real pty the program lives in — a session leader with the
        // pty as its controlling terminal, exactly what a terminal
        // widget's spawn does.
        let pair = native_pty_system().openpty(PtySize {
            rows: dims.rows,
            cols: dims.cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut command = CommandBuilder::new(program);
        for arg in args {
            command.arg(arg);
        }
        // A terminal launched from a menu has no TERM: programs would
        // fall back to something dumb and look broken. The caller's
        // additions come last, so they can override anything.
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        for (key, value) in env_set {
            command.env(key, value);
        }
        command.cwd(cwd);
        let child = pair.slave.spawn_command(command)?;
        let real_master = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| anyhow!("pty master has no descriptor"))?;

        let proxy = Proxy { emit: emit.clone() };
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Config::default()
        };
        let term = Mutex::new(Term::new(config, &dims, proxy));
        let inner = Arc::new(Inner {
            real_master,
            fake_master,
            fake_slave,
            child: Mutex::new(child),
            term,
            mirrored: Mutex::new(None),
            stop: AtomicBool::new(false),
            emit,
        });

        let thread_inner = inner.clone();
        let bridge = std::thread::Builder::new()
            .name("radar-session".to_string())
            .spawn(move || bridge_loop(thread_inner))?;

        // A separate descriptor for the widget, so the bridge keeps its
        // own even if the client side is closed (or not: whichever way
        // the foreign pty treats ownership).
        let client_fd = FdGuard(unsafe { libc::dup(fake_master) });
        if client_fd.0 == -1 {
            bail!("dup of bridge pty: {}", std::io::Error::last_os_error());
        }

        Ok(Session {
            master: Some(pair.master),
            inner,
            client_fd,
            bridge: Some(bridge),
        })
    }

    /// The descriptor a client wraps in its own pty object — the
    /// widget's `vte4::Pty::foreign_sync`, a future remote client's
    /// socket handoff.
    pub fn client_fd(&self) -> libc::c_int {
        self.client_fd.0
    }

    /// The device name of the program's tty, when the kernel knows it.
    pub fn tty_name(&self) -> Option<std::path::PathBuf> {
        self.master.as_ref().and_then(|master| master.tty_name())
    }

    /// The running program's process id, while it lives. How a board
    /// claim is matched to the exact tab that owns it: the child's own
    /// `RADAR_AGENT` environment tells the instances of a program apart.
    pub fn process_id(&self) -> Option<u32> {
        self.inner
            .child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .process_id()
    }

    /// Bytes a client writes on its own behalf: keystrokes for a
    /// headless client, test input, whatever the bridge does not see.
    /// The widget's input arrives through the bridge pty instead.
    pub fn input(&self, bytes: &[u8]) {
        write_all_fd(self.inner.real_master, bytes);
    }

    /// Report a new grid size on the program's behalf. The widget's
    /// size is picked up by the bridge automatically; this is for
    /// clients that report explicitly.
    pub fn resize(&self, dims: Dims) {
        self.inner.resize(dims);
    }

    /// The authoritative screen state. Phase 2's snapshots and phase
    /// 3's headless renderer read this; phase 1's tests assert on it.
    pub fn screen(&self) -> MutexGuard<'_, Term<Proxy>> {
        self.inner.term.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Stop the bridge before any descriptor it uses can close:
        // field drops happen after this body, but only `join` makes
        // that safe rather than hopeful.
        self.inner.stop.store(true, Ordering::Relaxed);
        if let Some(bridge) = self.bridge.take() {
            let _ = bridge.join();
        }
        // Close the pty: the program is SIGHUPed, the way a terminal
        // widget's child always was. Only then reap — reaping first
        // would miss the exit and leave a zombie.
        if let Some(master) = self.master.take() {
            drop(master);
        }
        for _ in 0..10 {
            if let Ok(mut child) = self.inner.child.try_lock() {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) => {}
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // A program that survives the hangup (it daemonised) is left to
        // init, the same as a terminal widget's child.
    }
}

/// The bridge: relay both directions, watch the client's size, watch
/// for the program's exit. Runs until the program's stream ends or the
/// session is dropped.
fn bridge_loop(inner: Arc<Inner>) {
    let mut parser: ansi::Processor = Processor::new();
    let mut client_open = true;
    let mut exited = false;
    let mut buf = [0_u8; 64 * 1024];

    loop {
        if inner.stop.load(Ordering::Relaxed) {
            return;
        }

        // Poll the readable ends: the program's output, and the
        // widget's input. The timeout is the heartbeat that notices
        // resizes and child exit without any GUI involvement.
        let mut fds = [
            libc::pollfd {
                fd: inner.real_master,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if client_open { inner.fake_slave } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, BRIDGE_POLL_MS) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }

        // Program output: parse into the authoritative state, then hand
        // the same bytes to the client. Parse first so the state is
        // never behind what clients see.
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let n = unsafe { libc::read(inner.real_master, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                // Hangup or error: the stream is over. Either the
                // program exited or every holder of the tty did; the
                // exit check below (and after the loop) settles it.
                break;
            }
            let bytes = &buf[..n as usize];
            {
                let mut term = inner.term.lock().unwrap_or_else(|e| e.into_inner());
                parser.advance(&mut *term, bytes);
            }
            (inner.emit)(SessionEvent::Raw(bytes.to_vec()));
            // Output goes in through the slave end: writes to a pty's
            // slave are what its master reads — and the widget holds
            // the master.
            write_all_fd(inner.fake_slave, bytes);
        }

        // Widget input: keystrokes, mouse reports, paste — already
        // encoded by the widget, needing only delivery.
        if client_open && fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let n = unsafe { libc::read(inner.fake_slave, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                // The client end is gone. The program lives on — a
                // closed pane keeps running — so stop relaying input
                // and keep streaming output.
                client_open = false;
            } else {
                write_all_fd(inner.real_master, &buf[..n as usize]);
            }
        }

        // Mirror the client's grid size: the widget sizes its own pty
        // from its allocation, so reading that size is how the bridge
        // learns of resizes without any GUI call. `resize` dedupes on
        // the shared record, so this and an explicit `Session::resize`
        // can never disagree about the program's size.
        if client_open {
            if let Some(dims) = winsize_of(inner.fake_master) {
                inner.resize(dims);
            }
        }

        // The direct program's exit. Checked after the I/O above so
        // trailing output lands in the stream before the Exit event.
        if !exited {
            let mut child = inner.child.lock().unwrap_or_else(|e| e.into_inner());
            if let Ok(Some(status)) = child.try_wait() {
                exited = true;
                (inner.emit)(SessionEvent::Exit(ExitInfo {
                    code: status.exit_code(),
                    signal: status.signal().map(str::to_string),
                }));
            }
        }
    }

    // The stream is over; the direct program usually is too. Reap it
    // with a short leash — a child that ignores the hangup (it
    // daemonised) is left to init rather than hung here.
    if !exited {
        for _ in 0..40 {
            let mut child = inner.child.lock().unwrap_or_else(|e| e.into_inner());
            if let Ok(Some(status)) = child.try_wait() {
                (inner.emit)(SessionEvent::Exit(ExitInfo {
                    code: status.exit_code(),
                    signal: status.signal().map(str::to_string),
                }));
                return;
            }
            if inner.stop.load(Ordering::Relaxed) {
                return;
            }
            drop(child);
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// A pty pair for the bridge, both ends CLOEXEC-free-but-fresh. The
/// caller sets CLOEXEC once it knows which side the program must never
/// see.
fn open_pty_pair(dims: Dims) -> Result<(libc::c_int, libc::c_int)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut winsize = libc::winsize {
        ws_row: dims.rows,
        ws_col: dims.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe {
        #[allow(clippy::unnecessary_mut_passed)]
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &mut winsize,
        )
    };
    if result != 0 {
        bail!("openpty: {}", std::io::Error::last_os_error());
    }
    Ok((master, slave))
}

/// The grid size a client has set on its end of the bridge, if it has
/// said anything sensible.
fn winsize_of(fd: libc::c_int) -> Option<Dims> {
    let mut winsize: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut winsize) } != 0 {
        return None;
    }
    (winsize.ws_col > 0 && winsize.ws_row > 0).then_some(Dims {
        cols: winsize.ws_col,
        rows: winsize.ws_row,
    })
}

/// Write every byte, restarting on signals. A short write on a pty
/// means the buffer is full; blocking fds make the write wait, which is
/// the backpressure a terminal wants.
fn write_all_fd(fd: libc::c_int, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        bytes = &bytes[n as usize..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A session with its events collected on a channel, plus the
    /// events seen so far — shared, so helpers can scan for what has
    /// arrived without draining the sender's knowledge.
    struct TestSession {
        session: Session,
        events: Arc<Mutex<Vec<SessionEvent>>>,
    }

    impl TestSession {
        fn spawn(argv: &[&str], cwd: &Path) -> TestSession {
            Self::spawn_with_env(argv, &[], cwd)
        }

        fn spawn_with_env(argv: &[&str], env_set: &[(String, String)], cwd: &Path) -> TestSession {
            let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            let events: Arc<Mutex<Vec<SessionEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let sink = events.clone();
            let session = Session::spawn(
                &argv,
                env_set,
                cwd,
                Dims { cols: 80, rows: 24 },
                move |event| sink.lock().unwrap().push(event),
            )
            .expect("session spawn");
            TestSession { session, events }
        }

        fn seen(&self) -> Vec<SessionEvent> {
            self.events.lock().unwrap().clone()
        }

        /// Everything the program has written so far, concatenated.
        fn output(&self) -> Vec<u8> {
            self.seen()
                .into_iter()
                .flat_map(|event| match event {
                    SessionEvent::Raw(bytes) => bytes,
                    _ => Vec::new(),
                })
                .collect()
        }

        /// The output so far, as text.
        fn output_text(&self) -> String {
            String::from_utf8_lossy(&self.output()).into_owned()
        }

        /// Wait until `predicate` holds for the events, or fail.
        fn wait_for(&self, what: &str, mut predicate: impl FnMut(&[SessionEvent]) -> bool) {
            for _ in 0..100 {
                if predicate(&self.seen()) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            panic!("timed out waiting for {what}; saw {:?}", self.seen());
        }

        /// Wait until the program's output contains `text`.
        fn wait_for_output(&self, what: &str, text: &str) {
            self.wait_for(what, |_| self.output_text().contains(text));
        }

        fn exit(&self) -> Option<ExitInfo> {
            self.seen().into_iter().find_map(|event| match event {
                SessionEvent::Exit(info) => Some(info),
                _ => None,
            })
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("radar-session-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn output_reaches_the_client_stream_and_exit_is_reported() {
        let session = TestSession::spawn(&["printf", "hello"], &scratch("printf"));
        session.wait_for("exit", |events| {
            events.iter().any(|e| matches!(e, SessionEvent::Exit(_)))
        });
        assert!(String::from_utf8_lossy(&session.output()).contains("hello"));
        let exit = session.exit().expect("exit event");
        assert_eq!(exit.code, 0);
        assert!(exit.signal.is_none());
    }

    #[test]
    fn input_reaches_the_program() {
        let session = TestSession::spawn(
            &["/bin/sh", "-c", "read line; echo \"got: $line\""],
            &scratch("read"),
        );
        // A moment for the program to start before typing to it; the
        // reply is what the assertion waits on, so this is not timing.
        std::thread::sleep(Duration::from_millis(200));
        session.session.input(b"abc\n");
        session.wait_for_output("the reply", "got: abc");
    }

    #[test]
    fn the_clients_size_reaches_the_program_and_the_state() {
        // What the widget does: set a winsize on ITS end of the bridge.
        // The mirror must pass it to the program and the state.
        let session = TestSession::spawn(&["/bin/sh"], &scratch("client-resize"));
        let winsize = libc::winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(session.session.client_fd(), libc::TIOCSWINSZ, &winsize);
        }
        // The mirror notices within one poll heartbeat.
        std::thread::sleep(Duration::from_millis(300));
        session.session.input(b"stty size\n");
        session.wait_for_output("stty output", "40 120");
        let term = session.session.screen();
        assert_eq!(term.columns(), 120);
        assert_eq!(term.screen_lines(), 40);
    }

    #[test]
    fn session_resize_updates_the_authoritative_state() {
        // The explicit resize a future client reports. The bridge's own
        // first mirror has settled after a heartbeat, and cannot fire
        // again while nothing touches the client end, so the state is
        // stable once set.
        let session = TestSession::spawn(&["sleep", "5"], &scratch("resize"));
        std::thread::sleep(Duration::from_millis(300));
        session.session.resize(Dims {
            cols: 100,
            rows: 30,
        });
        let term = session.session.screen();
        assert_eq!(term.columns(), 100);
        assert_eq!(term.screen_lines(), 30);
    }

    #[test]
    fn title_and_bell_arrive_as_events() {
        let session =
            TestSession::spawn(&["printf", "\\033]0;my title\\007\\007"], &scratch("title"));
        session.wait_for("title and bell", |events| {
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::Title(t) if t == "my title"))
                && events.iter().any(|e| matches!(e, SessionEvent::Bell))
        });
    }

    #[test]
    fn the_authoritative_state_parses_the_stream() {
        let session = TestSession::spawn(&["printf", "hello"], &scratch("state"));
        session.wait_for_output("parsed text", "hello");
        let term = session.session.screen();
        let hello: String = (0..5)
            .map(|col| {
                term.grid()[alacritty_terminal::index::Point {
                    line: alacritty_terminal::index::Line(0),
                    column: alacritty_terminal::index::Column(col),
                }]
                .c
            })
            .collect();
        assert_eq!(hello, "hello");
    }

    #[test]
    fn environment_and_directory_reach_the_program() {
        let env_set = vec![("RADAR_SESSION_TEST".to_string(), "42".to_string())];
        let session = TestSession::spawn_with_env(
            &["/bin/sh", "-c", "pwd; echo $RADAR_SESSION_TEST"],
            &env_set,
            &scratch("env"),
        );
        session.wait_for("pwd and env output", |_| {
            let text = session.output_text();
            text.contains("radar-session-env") && text.contains("42")
        });
    }

    #[test]
    fn empty_argv_is_refused() {
        let events: Arc<Mutex<Vec<SessionEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let result = Session::spawn(
            &[],
            &[],
            &scratch("empty"),
            Dims { cols: 80, rows: 24 },
            move |event| sink.lock().unwrap().push(event),
        );
        assert!(result.is_err());
    }

    /// A shell whose `read` blocks keeps the session's bridge thread
    /// busy; dropping the session must join it promptly and the program
    /// must be gone (SIGHUP).
    #[test]
    fn dropping_a_live_session_reclaims_the_program() {
        let session = TestSession::spawn(&["sleep", "60"], &scratch("drop"));
        let pid =
            // The bridge reaps the direct child; find it through the
            // process group the pty points at.
            unsafe { libc::tcgetpgrp(session.session.inner.real_master) };
        drop(session);
        // The SIGHUP lands on the program's group; give the kernel a
        // moment, then check the process is gone.
        std::thread::sleep(Duration::from_millis(300));
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "program survived the session being dropped");
    }
}
