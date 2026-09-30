//! Daemon-owned sessions. Attach is an atomic display snapshot + sequenced raw
//! stream subscription. Dropping a subscription never drops the process.
//!
//! All PTY I/O is nonblocking. Terminal delivery and lifecycle/attention have
//! independent bounded queues; a lagging subscriber must explicitly resnapshot.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};

use super::{Dims, ExitInfo};
use crate::ghostty::{Effect, Terminal};

const QUEUE_CAPACITY: usize = 32;
const CHUNK: usize = 8192;
const INPUT_LIMIT: usize = 64 * 1024;
const POLL_MS: i32 = 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Spawn {
    pub id: String,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub env_remove: Vec<String>,
    pub dims: Dims,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Lifecycle {
    Running,
    Exited(ExitInfo),
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub id: String,
    #[serde(default)]
    pub cwd: PathBuf,
    pub pid: Option<u32>,
    pub lifecycle: Lifecycle,
    pub title: Option<String>,
    /// Child exit and PTY EOF are separate: descendants may still hold the PTY.
    pub stream_closed: bool,
}

/// An attach snapshot: the daemon's lossless libghostty-vt terminal state plus
/// the output watermark. A client decodes the snapshot into its own engine and
/// feeds raw bytes after `sequence`; there is no ANSI replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub status: Status,
    pub dims: Dims,
    pub sequence: u64,
    /// Lossless libghostty-vt snapshot of the daemon's authoritative terminal,
    /// taken under the same lock as `sequence`. It covers both screens,
    /// scrollback, modes, cursor, title and the unfinished parser input at the
    /// cut, so the client resumes exactly.
    pub terminal_snapshot: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Output {
    Bytes(Vec<u8>),
    Resize(Dims),
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Feedback {
    Title(Option<String>),
    Bell,
    Lifecycle(Lifecycle),
    StreamClosed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sequenced<T> {
    pub sequence: u64,
    pub event: T,
}

/// Overload is never a silent dropped byte. Check this before consuming queued
/// items; after ResyncRequired the entire old subscription must be discarded.
#[derive(Debug, PartialEq, Eq)]
pub enum ReceiveError {
    Empty,
    ResyncRequired,
    Closed,
}

pub struct Subscription<T> {
    rx: Receiver<Sequenced<T>>,
    lagged: Arc<AtomicBool>,
    attached: Arc<AtomicBool>,
}

impl<T> Drop for Subscription<T> {
    fn drop(&mut self) {
        self.attached.store(false, Ordering::Release);
    }
}

impl<T> Subscription<T> {
    pub fn try_recv(&self) -> std::result::Result<Sequenced<T>, ReceiveError> {
        if self.lagged.load(Ordering::Acquire) {
            return Err(ReceiveError::ResyncRequired);
        }
        let result = self.rx.try_recv().map_err(|error| match error {
            TryRecvError::Empty => ReceiveError::Empty,
            TryRecvError::Disconnected => ReceiveError::Closed,
        });
        if self.lagged.load(Ordering::Acquire) {
            return Err(ReceiveError::ResyncRequired);
        }
        result
    }
}

struct Subscriber<T> {
    tx: SyncSender<Sequenced<T>>,
    lagged: Arc<AtomicBool>,
    attached: Arc<AtomicBool>,
}

struct Bus<T> {
    sequence: u64,
    clients: Vec<Subscriber<T>>,
}

impl<T: Clone> Bus<T> {
    fn new() -> Self {
        Self {
            sequence: 0,
            clients: Vec::new(),
        }
    }

    fn subscribe(&mut self) -> Subscription<T> {
        self.clients
            .retain(|client| client.attached.load(Ordering::Acquire));
        let (tx, rx) = mpsc::sync_channel(QUEUE_CAPACITY);
        let lagged = Arc::new(AtomicBool::new(false));
        let attached = Arc::new(AtomicBool::new(true));
        self.clients.push(Subscriber {
            tx,
            lagged: lagged.clone(),
            attached: attached.clone(),
        });
        Subscription {
            rx,
            lagged,
            attached,
        }
    }

    fn publish(&mut self, event: T) {
        self.sequence += 1;
        let item = Sequenced {
            sequence: self.sequence,
            event,
        };
        self.clients
            .retain(|client| match client.tx.try_send(item.clone()) {
                Ok(()) => true,
                Err(mpsc::TrySendError::Full(_)) => {
                    client.lagged.store(true, Ordering::Release);
                    false
                }
                Err(mpsc::TrySendError::Disconnected(_)) => false,
            });
    }
}

struct State {
    /// The daemon's authoritative terminal: libghostty-vt. Its snapshot is what
    /// clients restore, and its effects carry title/bell/query replies.
    term: Terminal,
    dims: Dims,
    status: Status,
    output: Bus<Output>,
    feedback: Bus<Feedback>,
}

enum Input {
    Bytes(Vec<u8>),
    Resize(Dims),
}

/// Own resources before launching the worker as well as during its run. Even a
/// thread-creation failure or panic must not strand an unreaped child.
struct Process {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    reaped: bool,
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        if let Some(fd) = self.master.as_raw_fd() {
            signal_groups(fd, self.child.process_id(), libc::SIGKILL);
        }
        let _ = self.child.kill();
        for _ in 0..50 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Owned by the registry, never by a client connection.
pub struct ManagedSession {
    spec: Spawn,
    state: Arc<Mutex<State>>,
    input: SyncSender<Input>,
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl ManagedSession {
    fn spawn(spec: Spawn) -> Result<Self> {
        validate_dims(spec.dims)?;
        let (program, args) = spec.argv.split_first().context("empty command")?;
        let pair = native_pty_system().openpty(pty_size(spec.dims))?;
        let fd = pair.master.as_raw_fd().context("PTY has no descriptor")?;
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut command = CommandBuilder::new(program);
        command.args(args);
        command.cwd(&spec.cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        // The daemon is often started by a desktop session or a systemd unit
        // whose PATH is not the login shell's; the mise entries let it find
        // agents installed under the user's home.
        command.env("PATH", crate::config::path_value());
        for name in &spec.env_remove {
            command.env_remove(name);
        }
        for (name, value) in &spec.env {
            command.env(name, value);
        }
        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let mut term = Terminal::new(spec.dims.cols, spec.dims.rows);
        term.enable_effects();
        let state = Arc::new(Mutex::new(State {
            term,
            dims: spec.dims,
            status: Status {
                id: spec.id.clone(),
                cwd: spec.cwd.clone(),
                pid: child.process_id(),
                lifecycle: Lifecycle::Running,
                title: None,
                stream_closed: false,
            },
            output: Bus::new(),
            feedback: Bus::new(),
        }));
        let (input, rx) = mpsc::sync_channel(QUEUE_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let state_for_worker = state.clone();
        let stop_for_worker = stop.clone();
        let process = Process {
            master: pair.master,
            child,
            reaped: false,
        };
        let worker = std::thread::Builder::new()
            .name(format!("session-{}", spec.id))
            .spawn(move || {
                run(process, state_for_worker, rx, stop_for_worker);
            })?;
        Ok(Self {
            spec,
            state,
            input,
            stop,
            worker: Mutex::new(Some(worker)),
        })
    }

    /// Cloning the display and installing the subscription share the parser's
    /// lock: the first event is exactly snapshot.sequence + 1.
    pub fn attach(&self) -> (Snapshot, Subscription<Output>) {
        let mut state = self.state.lock().unwrap();
        let snapshot = Snapshot {
            status: state.status.clone(),
            dims: state.dims,
            sequence: state.output.sequence,
            terminal_snapshot: state.term.snapshot(),
        };
        (snapshot, state.output.subscribe())
    }

    pub fn watch(&self) -> (Status, u64, Subscription<Feedback>) {
        let mut state = self.state.lock().unwrap();
        (
            state.status.clone(),
            state.feedback.sequence,
            state.feedback.subscribe(),
        )
    }

    pub fn status(&self) -> Status {
        self.state.lock().unwrap().status.clone()
    }

    /// The launch-time todo binding, retained even after the child exits.
    pub fn card_id(&self) -> Option<&str> {
        self.spec
            .env
            .iter()
            .rev()
            .find(|(name, _)| name == "RADAR_CARD_ID")
            .map(|(_, value)| value.as_str())
            .filter(|value| !value.is_empty())
    }

    pub fn input(&self, bytes: Vec<u8>) -> Result<()> {
        if bytes.len() > CHUNK {
            bail!("input frame exceeds {CHUNK} bytes");
        }
        self.send(Input::Bytes(bytes))
    }

    /// The last accepted resize wins; viewers should not resize unless they
    /// intend to take control of the shared terminal dimensions.
    pub fn resize(&self, dims: Dims) -> Result<()> {
        validate_dims(dims)?;
        self.send(Input::Resize(dims))
    }

    fn send(&self, input: Input) -> Result<()> {
        if self.stop.load(Ordering::Acquire) || self.status().stream_closed {
            bail!("session is stopping or closed");
        }
        self.input
            .try_send(input)
            .map_err(|error| anyhow!("session input unavailable: {error}"))
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Drop for ManagedSession {
    fn drop(&mut self) {
        self.stop();
        if let Some(worker) = self.worker.get_mut().unwrap().take() {
            let _ = worker.join();
        }
    }
}

#[derive(Default)]
pub struct Registry {
    sessions: Mutex<HashMap<String, Arc<ManagedSession>>>,
    stopping: AtomicBool,
}

impl Registry {
    /// Stable IDs make create-or-attach idempotent, including after process exit.
    /// A conflicting command is an error, never an implicit restart.
    pub fn create(&self, mut spec: Spawn) -> Result<Arc<ManagedSession>> {
        if spec.id.is_empty()
            || spec.id.len() > 128
            || !spec
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c))
        {
            bail!("invalid session ID");
        }
        spec.cwd = spec
            .cwd
            .canonicalize()
            .context("session working directory")?;
        if !spec.cwd.is_dir() {
            bail!("working directory is not a directory");
        }
        let mut sessions = self.sessions.lock().unwrap();
        if self.stopping.load(Ordering::Acquire) {
            bail!("session registry is shutting down");
        }
        if let Some(session) = sessions.get(&spec.id) {
            if session.spec.argv != spec.argv
                || session.spec.cwd != spec.cwd
                || session.spec.env != spec.env
                || session.spec.env_remove != spec.env_remove
            {
                bail!("session ID already belongs to a different command or environment");
            }
            return Ok(session.clone());
        }
        if sessions.len() >= 128 {
            bail!("session limit reached; forget an ended session first");
        }
        let session = Arc::new(ManagedSession::spawn(spec.clone())?);
        sessions.insert(spec.id, session.clone());
        Ok(session)
    }

    pub fn get(&self, id: &str) -> Result<Arc<ManagedSession>> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .context("unknown session")
    }

    pub fn list(&self) -> Vec<Status> {
        let sessions: Vec<_> = self.sessions.lock().unwrap().values().cloned().collect();
        // A session may be snapshotting its terminal. Never hold the registry
        // lock while waiting for that session's parser lock.
        let mut statuses: Vec<_> = sessions.iter().map(|session| session.status()).collect();
        statuses.sort_by(|a, b| a.id.cmp(&b.id));
        statuses
    }

    /// Explicitly release an ended session's retained screen/history and ID.
    /// Clients cannot accidentally replace a live process by reusing its ID.
    pub fn forget(&self, id: &str) -> Result<()> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions.get(id).context("unknown session")?;
        if !session
            .worker
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            bail!("session has not finished; stop it and wait for stream closure first");
        }
        sessions.remove(id);
        Ok(())
    }

    pub fn stop_all(&self) {
        let sessions = self.sessions.lock().unwrap();
        self.stopping.store(true, Ordering::Release);
        for session in sessions.values() {
            session.stop();
        }
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.stop_all();
    }
}

fn validate_dims(dims: Dims) -> Result<()> {
    if !(2..=500).contains(&dims.cols) || !(1..=300).contains(&dims.rows) {
        bail!("terminal dimensions must be 2..500 columns and 1..300 rows");
    }
    Ok(())
}

fn pty_size(dims: Dims) -> PtySize {
    PtySize {
        cols: dims.cols,
        rows: dims.rows,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn transition(state: &mut State, lifecycle: Lifecycle) {
    state.status.lifecycle = lifecycle.clone();
    state.feedback.publish(Feedback::Lifecycle(lifecycle));
}

fn write_pending(fd: i32, pending: &mut VecDeque<u8>) -> io::Result<()> {
    let bytes = pending.make_contiguous();
    let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if n > 0 {
        pending.drain(..n as usize);
        Ok(())
    } else if n == 0 {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "PTY write made no progress",
        ))
    } else {
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(()),
            _ => Err(error),
        }
    }
}

fn read_pty(fd: i32, bytes: &mut [u8]) -> io::Result<Option<usize>> {
    let n = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
    if n >= 0 {
        return Ok(Some(n as usize));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EIO) {
        // Linux PTY masters report EIO when the last slave descriptor closes.
        return Ok(Some(0));
    }
    match error.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(None),
        _ => Err(error),
    }
}

fn close_stream(state: &mut State) {
    if !state.status.stream_closed {
        state.status.stream_closed = true;
        state.output.publish(Output::Closed);
        state.feedback.publish(Feedback::StreamClosed);
    }
}

fn stop_deadline_reached(state: &mut State) {
    if matches!(state.status.lifecycle, Lifecycle::Running) {
        transition(
            state,
            Lifecycle::Failed("process did not exit before the stop deadline".into()),
        );
    }
    close_stream(state);
}

fn run(
    mut process: Process,
    shared: Arc<Mutex<State>>,
    input: Receiver<Input>,
    stop: Arc<AtomicBool>,
) {
    let fd = process
        .master
        .as_raw_fd()
        .expect("validated PTY descriptor");
    let mut pending = VecDeque::<u8>::new();
    let mut buf = [0; CHUNK];
    let mut eof = false;
    let mut exited = false;
    let mut stopping = None;
    loop {
        if stop.load(Ordering::Acquire) && stopping.is_none() {
            // portable-pty creates a session leader. Signal the foreground group
            // as well as the shell's group, then escalate without blocking I/O.
            signal_groups(
                fd,
                if exited {
                    None
                } else {
                    process.child.process_id()
                },
                libc::SIGTERM,
            );
            stopping = Some(Instant::now());
        }
        if stopping.is_some_and(|at: Instant| at.elapsed() >= Duration::from_millis(500)) {
            signal_groups(
                fd,
                if exited {
                    None
                } else {
                    process.child.process_id()
                },
                libc::SIGKILL,
            );
            if !exited {
                let _ = process.child.kill();
            }
        }
        if pending.len() <= INPUT_LIMIT - CHUNK {
            // Bounded work per tick: input cannot starve output or lifecycle.
            // Leave room for a whole accepted frame before dequeuing it.
            for _ in 0..8 {
                if pending.len() > INPUT_LIMIT - CHUNK {
                    break;
                }
                match input.try_recv() {
                    Ok(Input::Bytes(bytes)) => pending.extend(bytes),
                    Ok(Input::Resize(dims)) => {
                        let mut state = shared.lock().unwrap();
                        if dims != state.dims {
                            if let Err(error) = process.master.resize(pty_size(dims)) {
                                transition(&mut state, Lifecycle::Failed(error.to_string()));
                                stop.store(true, Ordering::Release);
                            } else {
                                state.term.resize(dims.cols, dims.rows);
                                state.dims = dims;
                                state.output.publish(Output::Resize(dims));
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        let mut pollfd = libc::pollfd {
            fd: if eof { -1 } else { fd },
            events: libc::POLLIN | if pending.is_empty() { 0 } else { libc::POLLOUT },
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pollfd, 1, POLL_MS) };
        if ready < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            let mut state = shared.lock().unwrap();
            transition(
                &mut state,
                Lifecycle::Failed(io::Error::last_os_error().to_string()),
            );
            stop.store(true, Ordering::Release);
        }
        if pollfd.revents & libc::POLLOUT != 0 && !pending.is_empty() {
            if let Err(error) = write_pending(fd, &mut pending) {
                transition(
                    &mut shared.lock().unwrap(),
                    Lifecycle::Failed(format!("PTY input failed: {error}")),
                );
                pending.clear();
                stop.store(true, Ordering::Release);
            }
        }
        if pollfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            match read_pty(fd, &mut buf) {
                Ok(Some(n)) if n > 0 => {
                    let bytes = &buf[..n];
                    let mut state = shared.lock().unwrap();
                    // The daemon owns the terminal: parse the bytes, then act
                    // on the side effects libghostty-vt reports.
                    state.term.write(bytes);
                    for effect in state.term.drain_effects() {
                        match effect {
                            Effect::Title(title) => {
                                let title = (!title.is_empty()).then_some(title);
                                state.status.title = title.clone();
                                state.feedback.publish(Feedback::Title(title));
                            }
                            Effect::Bell => state.feedback.publish(Feedback::Bell),
                            // The working directory is already mirrored through
                            // the session's own cwd; nothing to publish yet.
                            Effect::Pwd(_) => {}
                            Effect::WritePty(reply) => {
                                if pending.len() + reply.len() <= INPUT_LIMIT {
                                    pending.extend(reply);
                                } else {
                                    transition(
                                        &mut state,
                                        Lifecycle::Failed("terminal reply queue overflow".into()),
                                    );
                                    stop.store(true, Ordering::Release);
                                }
                            }
                        }
                    }
                    state.output.publish(Output::Bytes(bytes.to_vec()));
                }
                Ok(Some(_)) => {
                    eof = true;
                    close_stream(&mut shared.lock().unwrap());
                }
                Ok(None) => {}
                Err(error) => {
                    let mut state = shared.lock().unwrap();
                    transition(
                        &mut state,
                        Lifecycle::Failed(format!("PTY output failed: {error}")),
                    );
                    stop.store(true, Ordering::Release);
                    eof = true;
                    close_stream(&mut state);
                }
            }
        }
        if !exited {
            match process.child.try_wait() {
                Ok(Some(status)) => {
                    exited = true;
                    process.reaped = true;
                    let mut state = shared.lock().unwrap();
                    if !matches!(state.status.lifecycle, Lifecycle::Failed(_)) {
                        transition(
                            &mut state,
                            Lifecycle::Exited(ExitInfo {
                                code: status.exit_code(),
                                signal: status.signal().map(str::to_string),
                            }),
                        );
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    transition(
                        &mut shared.lock().unwrap(),
                        Lifecycle::Failed(error.to_string()),
                    );
                    stop.store(true, Ordering::Release);
                }
            }
        }
        if eof && exited {
            break;
        }
        if stopping.is_some_and(|at| at.elapsed() >= Duration::from_secs(2)) {
            // No descriptor or client write can keep shutdown waiting forever.
            stop_deadline_reached(&mut shared.lock().unwrap());
            break;
        }
    }
}

fn signal_groups(fd: i32, pid: Option<u32>, signal: i32) {
    unsafe {
        let foreground = libc::tcgetpgrp(fd);
        if foreground > 0 {
            libc::kill(-foreground, signal);
        }
        if let Some(pid) = pid {
            libc::kill(-(pid as i32), signal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec(id: &str, script: &str) -> Spawn {
        Spawn {
            id: id.into(),
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            cwd: std::env::current_dir().unwrap(),
            env: Vec::new(),
            env_remove: Vec::new(),
            dims: Dims { cols: 80, rows: 24 },
        }
    }

    fn until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "session did not reach expected state"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The daemon's attach snapshot is a real libghostty-vt snapshot: a client
    /// that decodes it and feeds what followed the cut lands exactly where a
    /// terminal that saw the whole stream lands.
    #[test]
    fn daemon_snapshot_resumes_a_mid_escape_cut_exactly() {
        use crate::ghostty::Terminal;

        // Write a title (so we can tell the prefix was parsed), content, then
        // the start of a 256-colour SGR — and stop mid-sequence.
        let session = ManagedSession::spawn(spec(
            "ghostty-cut",
            "printf '\\033]0;ready\\007abc\\033[38;5'; sleep 5",
        ))
        .unwrap();
        until(|| session.status().title.as_deref() == Some("ready"));

        let (snapshot, _subscription) = session.attach();
        let bytes = snapshot.terminal_snapshot.clone();
        assert!(!bytes.is_empty(), "the daemon encoded an empty snapshot");

        let mut restored = Terminal::from_snapshot(&bytes);
        restored.write(b";196m!");
        let mut reference = Terminal::new(80, 24);
        reference.write(b"\x1b]0;ready\x07abc\x1b[38;5;196m!");

        assert_eq!(
            restored.cursor(),
            reference.cursor(),
            "restored snapshot diverged from the full stream"
        );
        assert_eq!(restored.title(), "ready");

        session.stop();
    }

    #[test]
    fn listing_a_busy_session_does_not_block_unrelated_control() {
        let registry = Arc::new(Registry::default());
        let busy = registry.create(spec("busy", "read line")).unwrap();
        let other = registry.create(spec("other", "read line")).unwrap();
        // Model a snapshot holding the parser lock while a list waits for it.
        let parser = busy.state.lock().unwrap();
        let retained = Arc::strong_count(&busy);
        let listing_registry = registry.clone();
        let listing = std::thread::spawn(move || listing_registry.list());
        // Wait until list has retained its handles, rather than guessing when
        // the listing thread was scheduled. It then blocks on our parser lock.
        until(|| Arc::strong_count(&busy) > retained);
        let (done, received) = mpsc::channel();
        let controlling_registry = registry.clone();
        let control = std::thread::spawn(move || {
            controlling_registry.get("other").unwrap().stop();
            done.send(()).unwrap();
        });
        let result = received.recv_timeout(Duration::from_millis(250));
        // Release before asserting so even a regression cannot hang teardown.
        drop(parser);
        control.join().unwrap();
        listing.join().unwrap();
        assert!(
            result.is_ok(),
            "a busy session blocked another session's stop"
        );
        until(|| other.status().stream_closed);
    }

    #[test]
    fn stopping_an_unreaped_process_reports_failure_even_after_pty_eof() {
        for closed in [false, true] {
            let dims = Dims { cols: 80, rows: 24 };
            let mut state = State {
                term: Terminal::new(dims.cols, dims.rows),
                dims,
                status: Status {
                    id: "unreaped".into(),
                    cwd: PathBuf::new(),
                    pid: None,
                    lifecycle: Lifecycle::Running,
                    title: None,
                    stream_closed: closed,
                },
                output: Bus::new(),
                feedback: Bus::new(),
            };
            let feedback = state.feedback.subscribe();
            stop_deadline_reached(&mut state);
            assert!(state.status.stream_closed);
            assert!(matches!(state.status.lifecycle, Lifecycle::Failed(_)));
            assert!(matches!(
                feedback.try_recv().unwrap().event,
                Feedback::Lifecycle(Lifecycle::Failed(_))
            ));
            if !closed {
                assert!(matches!(
                    feedback.try_recv().unwrap().event,
                    Feedback::StreamClosed
                ));
            }
            // Cleanup is idempotent and preserves the recorded failure reason.
            let lifecycle = state.status.lifecycle.clone();
            stop_deadline_reached(&mut state);
            assert_eq!(state.status.lifecycle, lifecycle);
            assert_eq!(feedback.try_recv().unwrap_err(), ReceiveError::Empty);
            state.status.lifecycle = Lifecycle::Exited(ExitInfo {
                code: 0,
                signal: None,
            });
            stop_deadline_reached(&mut state);
            assert!(matches!(state.status.lifecycle, Lifecycle::Exited(_)));
        }
    }

    #[test]
    fn pty_reads_distinguish_retry_eof_and_fatal_errors() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;

        let (reader, mut writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let mut bytes = [0; CHUNK];
        assert_eq!(read_pty(reader.as_raw_fd(), &mut bytes).unwrap(), None);
        writer.write_all(b"data").unwrap();
        assert_eq!(read_pty(reader.as_raw_fd(), &mut bytes).unwrap(), Some(4));
        assert_eq!(&bytes[..4], b"data");
        writer.shutdown(std::net::Shutdown::Both).unwrap();
        assert_eq!(read_pty(reader.as_raw_fd(), &mut bytes).unwrap(), Some(0));
        assert_eq!(
            read_pty(-1, &mut bytes).unwrap_err().raw_os_error(),
            Some(libc::EBADF)
        );

        let pair = native_pty_system()
            .openpty(pty_size(Dims { cols: 80, rows: 24 }))
            .unwrap();
        drop(pair.slave);
        assert_eq!(
            read_pty(pair.master.as_raw_fd().unwrap(), &mut bytes).unwrap(),
            Some(0)
        );
    }

    #[test]
    fn pending_input_retries_backpressure_but_reports_broken_transport() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;

        let (mut writer, reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        loop {
            match writer.write(&[0; CHUNK]) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("unexpected write failure: {error}"),
            }
        }
        let mut pending = VecDeque::from(b"input".to_vec());
        write_pending(writer.as_raw_fd(), &mut pending).unwrap();
        assert_eq!(pending.iter().copied().collect::<Vec<_>>(), b"input");
        reader.shutdown(std::net::Shutdown::Both).unwrap();
        drop(reader);
        assert!(write_pending(writer.as_raw_fd(), &mut pending).is_err());
        assert_eq!(pending.iter().copied().collect::<Vec<_>>(), b"input");
    }

    /// Decode the attach snapshot and read the visible screen as text.
    fn text(snapshot: &Snapshot) -> String {
        let terminal = Terminal::from_snapshot(&snapshot.terminal_snapshot);
        let mut render = crate::ghostty::render::RenderState::new();
        let frame = render.frame(&terminal);
        let mut text = String::new();
        for row in &frame.lines {
            for cell in &row.cells {
                text.push_str(&cell.text);
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn detach_retains_process_and_reattach_has_no_snapshot_stream_gap() {
        let registry = Registry::default();
        let session = registry
            .create(spec(
                "shell",
                "printf before; read line; printf '\\r\\nafter:%s' \"$line\"; sleep 1",
            ))
            .unwrap();
        until(|| text(&session.attach().0).contains("before"));
        let pid = session.status().pid;
        drop(session.attach());
        drop(session);
        let session = registry.get("shell").unwrap();
        assert_eq!(session.status().pid, pid);
        let (snapshot, stream) = session.attach();
        assert!(text(&snapshot).contains("before"));
        session.input(b"hello\n".to_vec()).unwrap();
        let mut sequence = snapshot.sequence;
        let mut bytes = Vec::new();
        until(|| {
            while let Ok(event) = stream.try_recv() {
                assert_eq!(event.sequence, sequence + 1);
                sequence = event.sequence;
                if let Output::Bytes(data) = event.event {
                    bytes.extend(data);
                }
            }
            String::from_utf8_lossy(&bytes).contains("after:hello")
        });
        assert!(!String::from_utf8_lossy(&bytes).contains("before"));
    }

    #[test]
    fn stalled_output_requires_resync_without_delaying_feedback_or_exit() {
        let registry = Registry::default();
        let session = registry
            .create(spec(
                "flood",
                "read line; head -c 4000000 /dev/zero; printf '\\033]2;finished\\007\\007done'",
            ))
            .unwrap();
        let (_, output) = session.attach();
        let (_, _, feedback) = session.watch();
        session.input(b"go\n".to_vec()).unwrap();
        until(|| {
            session.status().stream_closed
                && matches!(session.status().lifecycle, Lifecycle::Exited(_))
        });
        assert_eq!(output.try_recv().unwrap_err(), ReceiveError::ResyncRequired);
        let mut bell = false;
        let mut exit = false;
        let mut title = false;
        while let Ok(event) = feedback.try_recv() {
            match event.event {
                Feedback::Bell => bell = true,
                Feedback::Lifecycle(Lifecycle::Exited(_)) => exit = true,
                Feedback::Title(Some(value)) if value == "finished" => title = true,
                _ => {}
            }
        }
        assert!(bell && exit && title);
        assert!(text(&session.attach().0).contains("done"));
    }

    #[test]
    fn stop_is_bounded_with_flooding_and_a_child_ignoring_term() {
        let registry = Registry::default();
        let session = registry.create(spec("stop", "trap '' TERM; printf ready; while :; do printf xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done")).unwrap();
        let _stalled = session.attach();
        until(|| session.attach().0.sequence > 0);
        let before = Instant::now();
        session.stop();
        until(|| {
            matches!(session.status().lifecycle, Lifecycle::Exited(_))
                && session.status().stream_closed
        });
        assert!(before.elapsed() < Duration::from_secs(3));
        assert!(session.input(b"ignored".to_vec()).is_err());
    }

    #[test]
    fn headless_terminal_answers_queries_once_without_a_client() {
        let registry = Registry::default();
        let session = registry
            .create(spec(
                "query",
                "stty raw -echo; printf '\\033[6n'; dd bs=1 count=6 2>/dev/null | od -An -tx1",
            ))
            .unwrap();
        until(|| session.status().stream_closed);
        assert!(text(&session.attach().0).contains("1b 5b 31 3b 31 52"));
    }

    #[test]
    fn resize_is_ordered_with_output_and_reaches_the_child() {
        let registry = Registry::default();
        let session = registry
            .create(spec("resize", "read line; stty size"))
            .unwrap();
        let (snapshot, stream) = session.attach();
        session
            .resize(Dims {
                cols: 111,
                rows: 37,
            })
            .unwrap();
        session.input(b"go\n".to_vec()).unwrap();
        until(|| session.status().stream_closed);
        let mut sequence = snapshot.sequence;
        let mut resized = false;
        while let Ok(event) = stream.try_recv() {
            assert_eq!(event.sequence, sequence + 1);
            sequence = event.sequence;
            if let Output::Resize(dims) = event.event {
                assert_eq!(
                    dims,
                    Dims {
                        cols: 111,
                        rows: 37
                    }
                );
                resized = true;
            }
        }
        assert!(resized);
        assert!(text(&session.attach().0).contains("37 111"));
    }

    #[test]
    fn create_is_idempotent_and_exited_sessions_are_not_implicitly_restarted() {
        let registry = Registry::default();
        let request = spec("once", "printf done");
        let first = registry.create(request.clone()).unwrap();
        until(|| {
            first.status().stream_closed && matches!(first.status().lifecycle, Lifecycle::Exited(_))
        });
        let second = registry.create(request).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(registry.create(spec("once", "sleep 20")).is_err());
        let mut invalid = spec("bad", "true");
        invalid.dims.cols = 0;
        assert!(registry.create(invalid).is_err());
    }

    #[test]
    fn attach_snapshot_round_trips_the_wire_and_decodes() {
        let registry = Registry::default();
        let session = registry
            .create(spec("snapshot", "printf 'hello\\r\\nworld'; read line"))
            .unwrap();
        until(|| text(&session.attach().0).contains("world"));
        let (snapshot, _) = session.attach();
        let decoded: Snapshot =
            serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert_eq!(decoded.sequence, snapshot.sequence);
        assert!(!decoded.terminal_snapshot.is_empty());
        // The snapshot decodes into a terminal on its own.
        let terminal = Terminal::from_snapshot(&decoded.terminal_snapshot);
        assert_eq!(terminal.title(), "");
    }

    #[test]
    fn process_exit_does_not_truncate_descendant_output() {
        let registry = Registry::default();
        let session = registry
            .create(spec(
                "tail",
                "trap '' HUP; (sleep 0.2; printf trailing) & exit 0",
            ))
            .unwrap();
        until(|| matches!(session.status().lifecycle, Lifecycle::Exited(_)));
        assert!(!session.status().stream_closed);
        until(|| session.status().stream_closed);
        assert!(text(&session.attach().0).contains("trailing"));
    }

    #[test]
    fn full_input_queue_is_rejected_and_registry_shutdown_stops_retained_handles() {
        let registry = Registry::default();
        let session = registry
            .create(spec(
                "blocked-input",
                "stty raw -echo; printf ready; sleep 30",
            ))
            .unwrap();
        until(|| text(&session.attach().0).contains("ready"));
        assert!((0..1000).any(|_| session.input(vec![b'x'; CHUNK]).is_err()));
        assert!(registry.forget("blocked-input").is_err());
        registry.stop_all();
        assert!(registry.create(spec("too-late", "true")).is_err());
        until(|| {
            session.status().stream_closed
                && matches!(session.status().lifecycle, Lifecycle::Exited(_))
        });
        until(|| registry.forget("blocked-input").is_ok());
        assert!(registry.list().is_empty());
    }
}
