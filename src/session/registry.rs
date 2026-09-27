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

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{
    cell::{Cell, Flags, LineLength},
    Config, Term, TermMode,
};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};
use anyhow::{anyhow, bail, Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};

use super::{Dims, ExitInfo};

const QUEUE_CAPACITY: usize = 32;
const CHUNK: usize = 8192;
const INPUT_LIMIT: usize = 64 * 1024;
const POLL_MS: i32 = 20;
/// A compatibility replay seeds the visible VTE renderer. Oldest scrollback
/// rows are omitted rather than making initial attachment an unbounded frame.
const SNAPSHOT_REPLAY_LIMIT: usize = 8 * 1024 * 1024;

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
    pub pid: Option<u32>,
    pub lifecycle: Lifecycle,
    pub title: Option<String>,
    /// Child exit and PTY EOF are separate: descendants may still hold the PTY.
    pub stream_closed: bool,
}

/// A display snapshot plus a bounded ANSI replay for a renderer with compatible
/// terminal semantics. It does not encode Alacritty's private parser internals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub status: Status,
    pub dims: Dims,
    pub sequence: u64,
    pub grid: Grid<Cell>,
    /// Grid's serde implementation skips its cursor; send it explicitly.
    pub cursor: (usize, usize),
    /// Scrollback is paged separately, so attaching never serializes the entire
    /// history before showing the current screen.
    pub history_lines: usize,
    pub mode: u32,
    /// Bounded ANSI rehydration for a compatible interactive renderer. The
    /// authoritative raw stream starts immediately after `sequence`.
    pub replay: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct History {
    pub sequence: u64,
    pub offset: usize,
    /// Newest row first; offset zero is the row immediately above the screen.
    pub rows: Vec<Vec<Cell>>,
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

#[derive(Clone)]
struct ParserEvents(Arc<Mutex<Vec<Event>>>);

impl EventListener for ParserEvents {
    fn send_event(&self, event: Event) {
        // This callback never does I/O or calls a client while the terminal is locked.
        self.0.lock().unwrap().push(event);
    }
}

struct State {
    term: Term<ParserEvents>,
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
        for name in &spec.env_remove {
            command.env_remove(name);
        }
        for (name, value) in &spec.env {
            command.env(name, value);
        }
        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let events = ParserEvents(Arc::new(Mutex::new(Vec::new())));
        let term = Term::new(
            Config {
                scrolling_history: 10_000,
                ..Config::default()
            },
            &spec.dims,
            events.clone(),
        );
        let state = Arc::new(Mutex::new(State {
            term,
            dims: spec.dims,
            status: Status {
                id: spec.id.clone(),
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
                run(process, state_for_worker, events, rx, stop_for_worker);
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
        let source = state.term.grid();
        let mut grid = Grid::new(source.screen_lines(), source.columns(), 0);
        for line in 0..source.screen_lines() {
            grid[Line(line as i32)] = source[Line(line as i32)].clone();
        }
        grid.cursor = source.cursor.clone();
        grid.saved_cursor = source.saved_cursor.clone();
        let snapshot = Snapshot {
            status: state.status.clone(),
            dims: state.dims,
            sequence: state.output.sequence,
            cursor: (
                source.cursor.point.line.0 as usize,
                source.cursor.point.column.0,
            ),
            history_lines: source.history_size(),
            grid,
            mode: state.term.mode().bits(),
            replay: replay_term(&state.term, state.status.title.as_deref()),
        };
        (snapshot, state.output.subscribe())
    }

    /// Reject stale paging instead of mixing history from different terminal
    /// revisions. Immutable historical checkpoints/backfill are a later layer.
    pub fn history(&self, sequence: u64, offset: usize, limit: usize) -> Result<History> {
        if limit == 0 || limit > 200 {
            bail!("history page must contain 1..200 rows");
        }
        let state = self.state.lock().unwrap();
        if sequence != state.output.sequence {
            bail!("history snapshot is stale; attach again");
        }
        let grid = state.term.grid();
        let rows = (offset.min(grid.history_size())
            ..offset.saturating_add(limit).min(grid.history_size()))
            .map(|index| {
                (0..grid.columns())
                    .map(|column| grid[Line(-1 - index as i32)][Column(column)].clone())
                    .collect()
            })
            .collect();
        Ok(History {
            sequence,
            offset,
            rows,
        })
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
        let mut statuses: Vec<_> = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .map(|session| session.status())
            .collect();
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
        self.stopping.store(true, Ordering::Release);
        for session in self.sessions.lock().unwrap().values() {
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

/// Rebuild the visible screen and the newest scrollback in a fresh ANSI renderer.
/// The replay is made under the same lock/watermark as the raw stream, so no
/// output can fall between the display image and its first stream sequence.
fn replay_term(term: &Term<ParserEvents>, title: Option<&str>) -> Vec<u8> {
    let mode = term.mode();
    let grid = term.grid();
    let mut out = Vec::with_capacity(1024);
    out.extend_from_slice(b"\x1bc");

    // When the application is on the alternate screen, recreate that screen
    // first; primary scrollback remains available to the daemon's history API.
    if mode.contains(TermMode::ALT_SCREEN) {
        out.extend_from_slice(b"\x1b[?1049h");
    } else {
        let mut history = Vec::new();
        let mut used = 0;
        for index in 1..=grid.history_size() {
            let (row, soft_wrap) = encode_row(&grid[Line(-(index as i32))]);
            if used + row.len() > SNAPSHOT_REPLAY_LIMIT / 2 {
                break;
            }
            used += row.len();
            history.push((row, soft_wrap));
        }
        for (row, soft_wrap) in history.iter().rev() {
            out.extend_from_slice(row);
            if !soft_wrap {
                out.extend_from_slice(b"\r\n");
            }
        }
    }

    // Painting the last column would otherwise leave a pending wrap, which
    // can spuriously scroll one blank row when the replay moves its cursor.
    out.extend_from_slice(b"\x1b[?7l\x1b[0m\x1b[2J\x1b[H");
    for row in 0..grid.screen_lines() {
        out.extend_from_slice(format!("\x1b[{};1H", row + 1).as_bytes());
        out.extend_from_slice(&encode_row(&grid[Line(row as i32)]).0);
    }
    out.extend_from_slice(b"\x1b[0m");

    // Restore the common DEC modes that govern subsequent raw input/output.
    for (flag, sequence) in [
        (TermMode::APP_CURSOR, b"\x1b[?1h".as_slice()),
        (TermMode::ORIGIN, b"\x1b[?6h".as_slice()),
        (TermMode::LINE_WRAP, b"\x1b[?7h".as_slice()),
        (TermMode::MOUSE_REPORT_CLICK, b"\x1b[?1000h".as_slice()),
        (TermMode::MOUSE_DRAG, b"\x1b[?1002h".as_slice()),
        (TermMode::MOUSE_MOTION, b"\x1b[?1003h".as_slice()),
        (TermMode::FOCUS_IN_OUT, b"\x1b[?1004h".as_slice()),
        (TermMode::UTF8_MOUSE, b"\x1b[?1005h".as_slice()),
        (TermMode::SGR_MOUSE, b"\x1b[?1006h".as_slice()),
        (TermMode::ALTERNATE_SCROLL, b"\x1b[?1007h".as_slice()),
        (TermMode::BRACKETED_PASTE, b"\x1b[?2004h".as_slice()),
        (TermMode::LINE_FEED_NEW_LINE, b"\x1b[20h".as_slice()),
        (TermMode::INSERT, b"\x1b[4h".as_slice()),
    ] {
        if mode.contains(flag) {
            out.extend_from_slice(sequence);
        }
    }
    if mode.contains(TermMode::APP_KEYPAD) {
        out.extend_from_slice(b"\x1b=");
    }
    if !mode.contains(TermMode::LINE_WRAP) {
        out.extend_from_slice(b"\x1b[?7l");
    }
    if !mode.contains(TermMode::SHOW_CURSOR) {
        out.extend_from_slice(b"\x1b[?25l");
    }
    let kitty = ((mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) as u8)
        | ((mode.contains(TermMode::REPORT_EVENT_TYPES) as u8) << 1)
        | ((mode.contains(TermMode::REPORT_ALTERNATE_KEYS) as u8) << 2)
        | ((mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) as u8) << 3)
        | ((mode.contains(TermMode::REPORT_ASSOCIATED_TEXT) as u8) << 4)) as u32;
    if kitty != 0 {
        out.extend_from_slice(format!("\x1b[>{kitty}u").as_bytes());
    }
    if let Some(title) = title {
        // OSC content must not be allowed to inject a terminator.
        let safe = title.replace(['\x1b', '\x07'], "�");
        out.extend_from_slice(format!("\x1b]0;{safe}\x07").as_bytes());
    }
    let cursor = grid.cursor.point;
    out.extend_from_slice(
        format!(
            "\x1b[{};{}H",
            cursor.line.0.max(0) as usize + 1,
            cursor.column.0 + 1
        )
        .as_bytes(),
    );
    if out.len() > SNAPSHOT_REPLAY_LIMIT {
        // Screen dimensions are capped and should fit comfortably. Defensive
        // truncation leaves a reset prefix and a bounded message if not.
        out.truncate(SNAPSHOT_REPLAY_LIMIT);
    }
    out
}

fn encode_row(row: &alacritty_terminal::grid::Row<Cell>) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut previous: Option<Vec<u16>> = None;
    for cell in row[..].iter().take(row.line_length().0) {
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        let sgr = cell_sgr(cell);
        if previous.as_ref() != Some(&sgr) {
            out.extend_from_slice(b"\x1b[");
            for (index, code) in sgr.iter().enumerate() {
                if index > 0 {
                    out.push(b';');
                }
                out.extend_from_slice(code.to_string().as_bytes());
            }
            out.push(b'm');
            previous = Some(sgr);
        }
        let mut utf8 = [0; 4];
        out.extend_from_slice(cell.c.encode_utf8(&mut utf8).as_bytes());
        if let Some(extra) = cell.zerowidth() {
            for character in extra {
                let mut utf8 = [0; 4];
                out.extend_from_slice(character.encode_utf8(&mut utf8).as_bytes());
            }
        }
    }
    let soft_wrap = row
        .last()
        .is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE));
    (out, soft_wrap)
}

fn cell_sgr(cell: &Cell) -> Vec<u16> {
    let mut codes = vec![0];
    for (flag, code) in [
        (Flags::BOLD, 1),
        (Flags::DIM, 2),
        (Flags::ITALIC, 3),
        (Flags::UNDERLINE, 4),
        (Flags::INVERSE, 7),
        (Flags::HIDDEN, 8),
        (Flags::STRIKEOUT, 9),
    ] {
        if cell.flags.contains(flag) {
            codes.push(code);
        }
    }
    codes.extend(color_sgr(cell.fg, true));
    codes.extend(color_sgr(cell.bg, false));
    codes
}

fn color_sgr(color: Color, foreground: bool) -> Vec<u16> {
    match color {
        Color::Spec(Rgb { r, g, b }) => vec![
            if foreground { 38 } else { 48 },
            2,
            r as u16,
            g as u16,
            b as u16,
        ],
        Color::Indexed(index) => vec![if foreground { 38 } else { 48 }, 5, index as u16],
        Color::Named(named) => {
            let base = if foreground { 30 } else { 40 };
            let bright = if foreground { 90 } else { 100 };
            match named {
                NamedColor::Black => vec![base],
                NamedColor::Red => vec![base + 1],
                NamedColor::Green => vec![base + 2],
                NamedColor::Yellow => vec![base + 3],
                NamedColor::Blue => vec![base + 4],
                NamedColor::Magenta => vec![base + 5],
                NamedColor::Cyan => vec![base + 6],
                NamedColor::White => vec![base + 7],
                NamedColor::BrightBlack => vec![bright],
                NamedColor::BrightRed => vec![bright + 1],
                NamedColor::BrightGreen => vec![bright + 2],
                NamedColor::BrightYellow => vec![bright + 3],
                NamedColor::BrightBlue => vec![bright + 4],
                NamedColor::BrightMagenta => vec![bright + 5],
                NamedColor::BrightCyan => vec![bright + 6],
                NamedColor::BrightWhite => vec![bright + 7],
                NamedColor::DimBlack => vec![2, base],
                NamedColor::DimRed => vec![2, base + 1],
                NamedColor::DimGreen => vec![2, base + 2],
                NamedColor::DimYellow => vec![2, base + 3],
                NamedColor::DimBlue => vec![2, base + 4],
                NamedColor::DimMagenta => vec![2, base + 5],
                NamedColor::DimCyan => vec![2, base + 6],
                NamedColor::DimWhite => vec![2, base + 7],
                NamedColor::Foreground | NamedColor::DimForeground => {
                    vec![if foreground { 39 } else { 49 }]
                }
                NamedColor::Background => vec![if foreground { 39 } else { 49 }],
                NamedColor::BrightForeground => vec![if foreground { 97 } else { 107 }],
                NamedColor::Cursor => vec![if foreground { 39 } else { 49 }],
            }
        }
    }
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

fn run(
    mut process: Process,
    shared: Arc<Mutex<State>>,
    events: ParserEvents,
    input: Receiver<Input>,
    stop: Arc<AtomicBool>,
) {
    let fd = process
        .master
        .as_raw_fd()
        .expect("validated PTY descriptor");
    let mut parser: Processor = Processor::new();
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
        if pending.len() < INPUT_LIMIT {
            // Bounded work per tick: input cannot starve output or lifecycle.
            for _ in 0..8 {
                if pending.len() >= INPUT_LIMIT {
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
                                state.term.resize(dims);
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
            let bytes = pending.make_contiguous();
            let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
            if n > 0 {
                pending.drain(..n as usize);
            }
        }
        if pollfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                let bytes = &buf[..n as usize];
                let mut state = shared.lock().unwrap();
                parser.advance(&mut state.term, bytes);
                // Flush synchronized updates too: the authoritative display must
                // represent every byte covered by this output watermark.
                parser.stop_sync(&mut state.term);
                for event in events.0.lock().unwrap().drain(..) {
                    let reply = match event {
                        Event::Title(title) => {
                            state.status.title = Some(title.clone());
                            state.feedback.publish(Feedback::Title(Some(title)));
                            None
                        }
                        Event::ResetTitle => {
                            state.status.title = None;
                            state.feedback.publish(Feedback::Title(None));
                            None
                        }
                        Event::Bell => {
                            state.feedback.publish(Feedback::Bell);
                            None
                        }
                        Event::PtyWrite(text) => Some(text),
                        Event::TextAreaSizeRequest(format) => Some(format(WindowSize {
                            num_lines: state.dims.rows,
                            num_cols: state.dims.cols,
                            cell_width: 0,
                            cell_height: 0,
                        })),
                        // Headless query policy: fixed default palette and empty
                        // clipboard. Clients never provide automatic PTY replies.
                        Event::ColorRequest(index, format) => Some(format(default_color(index))),
                        Event::ClipboardLoad(_, format) => Some(format("")),
                        _ => None,
                    };
                    if let Some(reply) = reply {
                        if pending.len() + reply.len() <= INPUT_LIMIT {
                            pending.extend(reply.bytes());
                        } else {
                            transition(
                                &mut state,
                                Lifecycle::Failed("terminal reply queue overflow".into()),
                            );
                            stop.store(true, Ordering::Release);
                        }
                    }
                }
                state.output.publish(Output::Bytes(bytes.to_vec()));
            } else if n == 0
                || (n < 0
                    && !matches!(
                        io::Error::last_os_error().kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ))
            {
                eof = true;
                let mut state = shared.lock().unwrap();
                state.status.stream_closed = true;
                state.output.publish(Output::Closed);
                state.feedback.publish(Feedback::StreamClosed);
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
            let mut state = shared.lock().unwrap();
            if !state.status.stream_closed {
                state.status.stream_closed = true;
                state.output.publish(Output::Closed);
                state.feedback.publish(Feedback::StreamClosed);
            }
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

fn default_color(index: usize) -> Rgb {
    const BASIC: [[u8; 3]; 16] = [
        [0, 0, 0],
        [205, 0, 0],
        [0, 205, 0],
        [205, 205, 0],
        [0, 0, 238],
        [205, 0, 205],
        [0, 205, 205],
        [229, 229, 229],
        [127, 127, 127],
        [255, 0, 0],
        [0, 255, 0],
        [255, 255, 0],
        [92, 92, 255],
        [255, 0, 255],
        [0, 255, 255],
        [255, 255, 255],
    ];
    let [r, g, b] = match index {
        0..=15 => BASIC[index],
        16..=231 => {
            let i = index - 16;
            let level = |v: usize| if v == 0 { 0 } else { (55 + 40 * v) as u8 };
            [level(i / 36), level(i / 6 % 6), level(i % 6)]
        }
        232..=255 => [8 + 10 * (index - 232) as u8; 3],
        257 => [0, 0, 0],
        _ => [229, 229, 229],
    };
    Rgb { r, g, b }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};

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

    fn text(snapshot: &Snapshot) -> String {
        let grid = &snapshot.grid;
        let mut text = String::new();
        for line in -(grid.history_size() as i32)..grid.screen_lines() as i32 {
            for column in 0..grid.columns() {
                text.push(grid[Line(line)][Column(column)].c);
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
    fn snapshots_page_history_and_preserve_wire_cursor_coordinates() {
        let registry = Registry::default();
        let session = registry.create(spec("history", "i=0; while [ $i -lt 100 ]; do printf 'row-%s\\n' $i; i=$((i+1)); done; printf end; read line")).unwrap();
        until(|| text(&session.attach().0).contains("end"));
        let (snapshot, _) = session.attach();
        assert!(snapshot.history_lines > 0);
        assert_eq!(snapshot.grid.history_size(), 0);
        let decoded: Snapshot =
            serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert_eq!(decoded.cursor, snapshot.cursor);
        assert!(snapshot.cursor.1 >= 3);
        let history = session.history(snapshot.sequence, 0, 3).unwrap();
        assert_eq!(history.rows.len(), 3);
        assert!(history.rows[0]
            .iter()
            .map(|cell| cell.c)
            .collect::<String>()
            .starts_with("row-"));
        session.input(b"go\n".to_vec()).unwrap();
        until(|| session.status().stream_closed);
        assert!(session.history(snapshot.sequence, 0, 3).is_err());
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

    #[test]
    fn ansi_replay_reconstructs_scrollback_visible_cells_attributes_cursor_and_modes() {
        use alacritty_terminal::vte::ansi::Processor;

        let dims = Dims { cols: 12, rows: 4 };
        let source_events = ParserEvents(Arc::new(Mutex::new(Vec::new())));
        let mut source = Term::new(
            Config {
                scrolling_history: 40,
                ..Config::default()
            },
            &dims,
            source_events,
        );
        let mut source_parser: Processor = Processor::new();
        source_parser.advance(
            &mut source,
            b"old-one\r\nold-two\r\nline-three\r\nline-four\r\n\x1b[31;1mred\x1b[0m\r\nactive\x1b[?1h\x1b[?2004h",
        );
        let replay = replay_term(&source, Some("snapshot title"));

        let target_events = ParserEvents(Arc::new(Mutex::new(Vec::new())));
        let mut target = Term::new(
            Config {
                scrolling_history: 40,
                ..Config::default()
            },
            &dims,
            target_events.clone(),
        );
        let mut target_parser: Processor = Processor::new();
        target_parser.advance(&mut target, &replay);

        // ANSI replay can leave one empty sentinel row in Alacritty's history;
        // visible content and the source history lines remain intact.
        assert!(target.grid().history_size() >= source.grid().history_size());
        for line in 0..source.grid().screen_lines() as i32 {
            for column in 0..dims.cols as usize {
                let before = &source.grid()[Line(line)][Column(column)];
                let after = &target.grid()[Line(line)][Column(column)];
                assert_eq!(after.c, before.c, "cell differs at {line}:{column}");
                assert_eq!(after.fg, before.fg, "foreground differs at {line}:{column}");
                assert_eq!(after.bg, before.bg, "background differs at {line}:{column}");
                assert_eq!(
                    after.flags.bits() & !Flags::WRAPLINE.bits(),
                    before.flags.bits() & !Flags::WRAPLINE.bits()
                );
            }
        }
        for offset in 1..=source.grid().history_size() {
            for column in 0..dims.cols as usize {
                assert_eq!(
                    target.grid()[Line(-(offset as i32))][Column(column)].c,
                    source.grid()[Line(-(offset as i32))][Column(column)].c,
                    "scrollback differs at row -{offset}, column {column}",
                );
            }
        }
        assert_eq!(target.grid().cursor.point, source.grid().cursor.point);
        assert_eq!(
            target.mode().bits() & (TermMode::APP_CURSOR | TermMode::BRACKETED_PASTE).bits(),
            (TermMode::APP_CURSOR | TermMode::BRACKETED_PASTE).bits()
        );
        assert!(target_events
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, Event::Title(title) if title == "snapshot title")));
    }
}
