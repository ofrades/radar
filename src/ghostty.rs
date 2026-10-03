//! libghostty-vt — Ghostty's VT core — as radar's terminal engine.
//!
//! Radar links a pinned, source-built `libghostty-vt.a` (see `build.rs` and
//! `docs/libghostty-vt.md`) and drives it through the C API. This module owns
//! the whole `unsafe` FFI surface for that engine, so the rest of radar works
//! with safe Rust.
//!
//! Two properties matter for radar and are the reason this engine was chosen
//! over `alacritty_terminal`/VTE:
//!
//! * A **snapshot** encodes the complete terminal state — both screens,
//!   scrollback, modes, cursor, title, palette, and the unfinished VT/UTF-8
//!   parser continuation at the cut — into one CRC-protected byte stream that a
//!   fresh terminal can decode. That is what makes a lossless attach possible
//!   without replaying the session's history.
//! * A **RenderState** exposes the grid for a client to draw, so the daemon and
//!   every client can share one engine.
//!
//! A [`Terminal`] is `!Send`/`!Sync` (the C handle is single-threaded), which
//! matches the daemon's single-lock parser. The safe surface here is
//! deliberately small: lifecycle, input, snapshot, and the few reads the smoke
//! test and the daemon need. The renderer, key/mouse encoders and effects land
//! with their own tickets.

use std::ffi::c_void;

pub mod mouse;
pub mod render;

/// Opaque `GhosttyTerminal`.
type RawTerminal = *mut c_void;
/// Opaque `GhosttySnapshotDecoder`.
type RawDecoder = *mut c_void;

/// `GHOSTTY_SUCCESS` — the only result that means the call worked.
const SUCCESS: i32 = 0;

/// `GHOSTTY_TERMINAL_DATA_CURSOR_X` (uint16_t *).
const DATA_CURSOR_X: i32 = 3;
/// `GHOSTTY_TERMINAL_DATA_CURSOR_Y` (uint16_t *).
const DATA_CURSOR_Y: i32 = 4;
/// `GHOSTTY_TERMINAL_DATA_TITLE` (`GhosttyString`).
const DATA_TITLE: i32 = 12;
/// `GHOSTTY_TERMINAL_DATA_PWD` (`GhosttyString`).
const DATA_PWD: i32 = 13;
/// `GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN` (`GhosttyTerminalScreen *`).
const DATA_ACTIVE_SCREEN: i32 = 6;
/// `GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING` (bool *).
const DATA_MOUSE_TRACKING: i32 = 11;
/// `GHOSTTY_TERMINAL_DATA_MODE` (`GhosttyTerminalModeConfig *`).
const DATA_MODE: i32 = 37;

/// `GHOSTTY_TERMINAL_SCREEN_ALTERNATE`.
const SCREEN_ALTERNATE: i32 = 1;

/// `GHOSTTY_TERMINAL_OPT_CONTINUATION_MAX_BYTES` (size_t *). Enables the
/// continuation tracking that lets a snapshot carry a mid-escape/mid-UTF-8 cut.
const OPT_CONTINUATION_MAX_BYTES: i32 = 31;

/// Bytes of unfinished parser input a terminal retains for snapshots. The
/// snapshot format itself is bounded; this is the input-side budget, ample for
/// any real escape sequence.
const CONTINUATION_MAX_BYTES: usize = 4096;

/// `GhosttyString`: a borrowed UTF-8 byte range owned by the terminal.
#[repr(C)]
struct GhosttyString {
    ptr: *const u8,
    len: usize,
}

/// A terminal side effect the embedder must handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Bytes the terminal wants written back to the PTY (query replies).
    WritePty(Vec<u8>),
    /// The program rang the bell (BEL).
    Bell,
    /// The program changed the title (OSC 0/2).
    Title(String),
    /// The program reported a working directory (OSC 7).
    Pwd(String),
}

/// The queue the C effect callbacks push into. Boxed so its address is stable
/// for the terminal's lifetime.
struct Effects {
    queue: std::sync::Mutex<Vec<Effect>>,
}

/// Read a `GhosttyString` datum from a terminal.
fn read_string(terminal: RawTerminal, data: i32) -> String {
    let mut value = GhosttyString {
        ptr: std::ptr::null(),
        len: 0,
    };
    let result =
        unsafe { ghostty_terminal_get(terminal, data, &mut value as *mut _ as *mut c_void) };
    if result != SUCCESS || value.ptr.is_null() {
        return String::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(value.ptr, value.len) };
    String::from_utf8_lossy(bytes).into_owned()
}

extern "C" fn effect_write_pty(
    _terminal: RawTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    // SAFETY: userdata is the Effects box the terminal holds for its lifetime.
    let effects = unsafe { &*(userdata as *const Effects) };
    let bytes = if data.is_null() || len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(data, len) }.to_vec()
    };
    if let Ok(mut queue) = effects.queue.lock() {
        queue.push(Effect::WritePty(bytes));
    }
}

extern "C" fn effect_bell(_terminal: RawTerminal, userdata: *mut c_void) {
    let effects = unsafe { &*(userdata as *const Effects) };
    if let Ok(mut queue) = effects.queue.lock() {
        queue.push(Effect::Bell);
    }
}

extern "C" fn effect_title(terminal: RawTerminal, userdata: *mut c_void) {
    let title = read_string(terminal, DATA_TITLE);
    let effects = unsafe { &*(userdata as *const Effects) };
    if let Ok(mut queue) = effects.queue.lock() {
        queue.push(Effect::Title(title));
    }
}

extern "C" fn effect_pwd(terminal: RawTerminal, userdata: *mut c_void) {
    let pwd = read_string(terminal, DATA_PWD);
    let effects = unsafe { &*(userdata as *const Effects) };
    if let Ok(mut queue) = effects.queue.lock() {
        queue.push(Effect::Pwd(pwd));
    }
}

/// `GhosttyTerminalScrollViewport`: a tagged union for `scroll_viewport`.
#[repr(C)]
struct ScrollViewport {
    tag: i32,
    _padding: u32,
    value: [u64; 2],
}

/// `GHOSTTY_SCROLL_VIEWPORT_DELTA` (up is negative).
const SCROLL_VIEWPORT_DELTA: i32 = 2;
/// `GHOSTTY_SCROLL_VIEWPORT_BOTTOM`.
const SCROLL_VIEWPORT_BOTTOM: i32 = 1;

/// `GHOSTTY_TERMINAL_OPT_SELECTION`.
const OPT_SELECTION: i32 = 21;
/// `GHOSTTY_TERMINAL_OPT_USERDATA`.
const OPT_USERDATA: i32 = 0;
/// `GHOSTTY_TERMINAL_OPT_WRITE_PTY`.
const OPT_WRITE_PTY: i32 = 1;
/// `GHOSTTY_TERMINAL_OPT_BELL`.
const OPT_BELL: i32 = 2;
/// `GHOSTTY_TERMINAL_OPT_TITLE_CHANGED`.
const OPT_TITLE_CHANGED: i32 = 5;
/// `GHOSTTY_TERMINAL_OPT_PWD_CHANGED`.
const OPT_PWD_CHANGED: i32 = 25;
/// `GHOSTTY_POINT_TAG_VIEWPORT`.
const POINT_TAG_VIEWPORT: i32 = 1;
/// `GHOSTTY_FORMATTER_FORMAT_PLAIN`.
const FORMATTER_FORMAT_PLAIN: i32 = 0;

/// `GhosttyGridRef`: an untracked reference to a terminal cell.
#[repr(C)]
#[derive(Clone, Copy)]
struct GridRef {
    size: usize,
    node: *mut c_void,
    x: u16,
    y: u16,
}

impl GridRef {
    fn new() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            node: std::ptr::null_mut(),
            x: 0,
            y: 0,
        }
    }
}

/// `GhosttyPoint`: a tagged point in the terminal grid.
#[repr(C)]
#[derive(Clone, Copy)]
struct Point {
    tag: i32,
    _padding: u32,
    value: [u64; 2],
}

impl Point {
    /// A viewport point; `y` is the row within the visible viewport.
    ///
    /// Packed arithmetically, not through a cast: `PointCoordinate` carries
    /// two bytes of padding, and transmuting them into the value poisons the
    /// result, which the optimizer folds into `unreachable` under `-O2`.
    fn viewport(x: u16, y: u32) -> Self {
        let packed = (x as u64) | ((y as u64) << 32);
        Self {
            tag: POINT_TAG_VIEWPORT,
            _padding: 0,
            value: [packed, 0],
        }
    }
}

/// `GhosttySelection`: a sized selection over two grid references.
#[repr(C)]
#[derive(Clone, Copy)]
struct Selection {
    size: usize,
    start: GridRef,
    end: GridRef,
    rectangle: bool,
}

/// `GhosttyTerminalModeConfig`: one mode plus its value, for set and query.
/// `GhosttyMode` packs the mode number in bits 0–14 and the ANSI flag in bit
/// 15 (`ghostty_mode_new`); radar only queries DEC private modes, whose flag
/// bit is 0, so the packed mode is the plain number.
#[repr(C)]
struct ModeConfig {
    mode: u16,
    value: bool,
}

/// `GhosttyTerminalSelectionFormatOptions`.
#[repr(C)]
#[derive(Clone, Copy)]
struct SelectionFormatOptions {
    size: usize,
    emit: i32,
    unwrap: bool,
    trim: bool,
    selection: *const Selection,
}

extern "C" {
    fn ghostty_terminal_new(
        allocator: *const c_void,
        terminal: *mut RawTerminal,
        cols: u16,
        rows: u16,
    ) -> i32;
    fn ghostty_terminal_free(terminal: RawTerminal);
    fn ghostty_terminal_set(terminal: RawTerminal, option: i32, value: *const c_void) -> i32;
    fn ghostty_terminal_get(terminal: RawTerminal, data: i32, out: *mut c_void) -> i32;
    fn ghostty_terminal_vt_write(terminal: RawTerminal, data: *const u8, len: usize);
    fn ghostty_terminal_resize(
        terminal: RawTerminal,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> i32;
    fn ghostty_terminal_scroll_viewport(terminal: RawTerminal, behavior: ScrollViewport);
    fn ghostty_terminal_grid_ref(terminal: RawTerminal, point: Point, out_ref: *mut GridRef)
        -> i32;
    fn ghostty_terminal_select_all(terminal: RawTerminal, out: *mut Selection) -> i32;
    fn ghostty_terminal_selection_format_alloc(
        terminal: RawTerminal,
        allocator: *const c_void,
        options: SelectionFormatOptions,
        out_ptr: *mut *mut u8,
        out_len: *mut usize,
    ) -> i32;

    fn ghostty_snapshot_encode_alloc(
        terminal: RawTerminal,
        allocator: *const c_void,
        out_ptr: *mut *mut u8,
        out_len: *mut usize,
    ) -> i32;
    fn ghostty_snapshot_decoder_new_buf(
        allocator: *const c_void,
        decoder: *mut RawDecoder,
        ptr: *const u8,
        len: usize,
    ) -> i32;
    fn ghostty_snapshot_decoder_decode(decoder: RawDecoder, terminal: *mut RawTerminal) -> i32;
    fn ghostty_snapshot_decoder_free(decoder: RawDecoder);

    fn ghostty_free(allocator: *const c_void, ptr: *mut u8, len: usize);
}

/// A live libghostty-vt terminal.
///
/// Construct with [`Terminal::new`] (blank) or [`Terminal::from_snapshot`]
/// (restored). The type is intentionally neither `Send` nor `Sync`: the
/// engine is single-threaded, and the daemon serialises access with its own
/// lock.
pub struct Terminal {
    raw: RawTerminal,
    /// The effect queue, when effects are enabled. Boxed so its address is
    /// stable while the terminal's userdata points at it.
    effects: Option<Box<Effects>>,
}

impl Terminal {
    /// Create a blank terminal of `cols` × `rows`.
    pub fn new(cols: u16, rows: u16) -> Self {
        let mut raw: RawTerminal = std::ptr::null_mut();
        let result = unsafe { ghostty_terminal_new(std::ptr::null(), &mut raw, cols, rows) };
        assert_eq!(result, SUCCESS, "ghostty_terminal_new failed: {result}");

        let limit = CONTINUATION_MAX_BYTES;
        let result = unsafe {
            ghostty_terminal_set(
                raw,
                OPT_CONTINUATION_MAX_BYTES,
                &limit as *const usize as *const c_void,
            )
        };
        assert_eq!(
            result, SUCCESS,
            "enabling continuation tracking failed: {result}"
        );

        Self { raw, effects: None }
    }

    /// Restore a terminal from a snapshot produced by [`Terminal::snapshot`].
    ///
    /// The snapshot includes the unfinished parser input at the cut, so feeding
    /// the bytes that follow the cut continues exactly where the source left
    /// off.
    pub fn from_snapshot(bytes: &[u8]) -> Self {
        let mut decoder: RawDecoder = std::ptr::null_mut();
        let result = unsafe {
            ghostty_snapshot_decoder_new_buf(
                std::ptr::null(),
                &mut decoder,
                bytes.as_ptr(),
                bytes.len(),
            )
        };
        assert_eq!(
            result, SUCCESS,
            "ghostty_snapshot_decoder_new_buf failed: {result}"
        );

        let mut raw: RawTerminal = std::ptr::null_mut();
        let result = unsafe { ghostty_snapshot_decoder_decode(decoder, &mut raw) };
        unsafe { ghostty_snapshot_decoder_free(decoder) };
        assert_eq!(
            result, SUCCESS,
            "ghostty_snapshot_decoder_decode failed: {result}"
        );

        Self { raw, effects: None }
    }

    /// The raw `GhosttyTerminal`, for the FFI layers in this module tree.
    pub(crate) fn raw(&self) -> RawTerminal {
        self.raw
    }

    /// Enable terminal effects. The engine then queues query replies (for the
    /// PTY), bell, title and pwd changes for the embedder to drain with
    /// [`Terminal::drain_effects`] after each write.
    pub fn enable_effects(&mut self) {
        let mut effects = Box::new(Effects {
            queue: std::sync::Mutex::new(Vec::new()),
        });
        let userdata = &mut *effects as *mut Effects as *mut c_void;
        unsafe {
            ghostty_terminal_set(self.raw, OPT_USERDATA, userdata);
            ghostty_terminal_set(self.raw, OPT_WRITE_PTY, effect_write_pty as *const c_void);
            ghostty_terminal_set(self.raw, OPT_BELL, effect_bell as *const c_void);
            ghostty_terminal_set(self.raw, OPT_TITLE_CHANGED, effect_title as *const c_void);
            ghostty_terminal_set(self.raw, OPT_PWD_CHANGED, effect_pwd as *const c_void);
        }
        self.effects = Some(effects);
    }

    /// Drain the effects queued since the last call.
    pub fn drain_effects(&self) -> Vec<Effect> {
        match &self.effects {
            Some(effects) => effects
                .queue
                .lock()
                .map(|mut queue| std::mem::take(&mut *queue))
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// Feed raw PTY bytes to the parser.
    pub fn write(&mut self, bytes: &[u8]) {
        unsafe { ghostty_terminal_vt_write(self.raw, bytes.as_ptr(), bytes.len()) };
    }

    /// Resize the terminal grid. Cell pixel sizes are unknown to the daemon
    /// (no renderer), so they are zero.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let result = unsafe { ghostty_terminal_resize(self.raw, cols, rows, 0, 0) };
        assert_eq!(result, SUCCESS, "ghostty_terminal_resize failed: {result}");
    }

    /// Scroll the viewport by `delta` rows (negative scrolls up into history).
    pub fn scroll_viewport(&mut self, delta: i32) {
        let behavior = ScrollViewport {
            tag: SCROLL_VIEWPORT_DELTA,
            _padding: 0,
            value: [(delta as i64) as u64, 0],
        };
        unsafe { ghostty_terminal_scroll_viewport(self.raw, behavior) };
    }

    /// Snap the viewport back to the active area.
    pub fn scroll_viewport_bottom(&mut self) {
        let behavior = ScrollViewport {
            tag: SCROLL_VIEWPORT_BOTTOM,
            _padding: 0,
            value: [0, 0],
        };
        unsafe { ghostty_terminal_scroll_viewport(self.raw, behavior) };
    }

    /// Whether the alternate screen is active — a full-screen program (a TUI)
    /// owns the view and there is no scrollback to move through.
    pub fn alt_screen(&self) -> bool {
        let mut screen: i32 = 0;
        unsafe {
            ghostty_terminal_get(
                self.raw,
                DATA_ACTIVE_SCREEN,
                &mut screen as *mut i32 as *mut c_void,
            );
        }
        screen == SCREEN_ALTERNATE
    }

    /// Whether the program asked for any mouse reporting (X10, normal,
    /// button-event or any-event tracking).
    pub fn mouse_tracking(&self) -> bool {
        let mut on = false;
        unsafe {
            ghostty_terminal_get(
                self.raw,
                DATA_MOUSE_TRACKING,
                &mut on as *mut bool as *mut c_void,
            );
        }
        on
    }

    /// A DEC private mode's current value. The engine's mode packing has the
    /// ANSI flag in bit 15 and DEC modes are 0, so the packed mode is the
    /// plain number.
    pub fn dec_mode(&self, number: u16) -> bool {
        let mut config = ModeConfig {
            mode: number,
            value: false,
        };
        let result = unsafe {
            ghostty_terminal_get(
                self.raw,
                DATA_MODE,
                &mut config as *mut ModeConfig as *mut c_void,
            )
        };
        result == SUCCESS && config.value
    }

    /// Select the inclusive viewport range between two cells.
    pub fn select_viewport(&mut self, start: (u16, u32), end: (u16, u32)) -> bool {
        let mut from = GridRef::new();
        let mut to = GridRef::new();
        let ok = unsafe {
            ghostty_terminal_grid_ref(self.raw, Point::viewport(start.0, start.1), &mut from)
                == SUCCESS
                && ghostty_terminal_grid_ref(self.raw, Point::viewport(end.0, end.1), &mut to)
                    == SUCCESS
        };
        if !ok {
            return false;
        }
        let selection = Selection {
            size: std::mem::size_of::<Selection>(),
            start: from,
            end: to,
            rectangle: false,
        };
        let result = unsafe {
            ghostty_terminal_set(
                self.raw,
                OPT_SELECTION,
                &selection as *const Selection as *const c_void,
            )
        };
        result == SUCCESS
    }

    /// Select the whole screen and scrollback.
    pub fn select_all(&mut self) -> bool {
        let mut selection = Selection {
            size: std::mem::size_of::<Selection>(),
            start: GridRef::new(),
            end: GridRef::new(),
            rectangle: false,
        };
        if unsafe { ghostty_terminal_select_all(self.raw, &mut selection) } != SUCCESS {
            return false;
        }
        // select_all returns a snapshot; install it as the active selection.
        let result = unsafe {
            ghostty_terminal_set(
                self.raw,
                OPT_SELECTION,
                &selection as *const Selection as *const c_void,
            )
        };
        result == SUCCESS
    }

    /// Drop the active selection.
    pub fn clear_selection(&mut self) {
        unsafe { ghostty_terminal_set(self.raw, OPT_SELECTION, std::ptr::null()) };
    }

    /// Format the active selection as plain text, or `None` when there is none.
    pub fn selection_text(&mut self) -> Option<String> {
        let options = SelectionFormatOptions {
            size: std::mem::size_of::<SelectionFormatOptions>(),
            emit: FORMATTER_FORMAT_PLAIN,
            unwrap: true,
            trim: true,
            selection: std::ptr::null(),
        };
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len: usize = 0;
        let result = unsafe {
            ghostty_terminal_selection_format_alloc(
                self.raw,
                std::ptr::null(),
                options,
                &mut ptr,
                &mut len,
            )
        };
        if result != SUCCESS {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
        unsafe { ghostty_free(std::ptr::null(), ptr, len) };
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Encode the complete current state as a snapshot.
    pub fn snapshot(&self) -> Vec<u8> {
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len: usize = 0;
        let result = unsafe {
            ghostty_snapshot_encode_alloc(self.raw, std::ptr::null(), &mut ptr, &mut len)
        };
        assert_eq!(
            result, SUCCESS,
            "ghostty_snapshot_encode_alloc failed: {result}"
        );

        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
        unsafe { ghostty_free(std::ptr::null(), ptr, len) };
        bytes
    }

    /// The cursor position as `(column, row)`.
    pub fn cursor(&self) -> (u16, u16) {
        let mut x = 0u16;
        let mut y = 0u16;
        unsafe {
            ghostty_terminal_get(self.raw, DATA_CURSOR_X, &mut x as *mut u16 as *mut c_void);
            ghostty_terminal_get(self.raw, DATA_CURSOR_Y, &mut y as *mut u16 as *mut c_void);
        }
        (x, y)
    }

    /// The window title (OSC 0/2), empty when unset.
    pub fn title(&self) -> String {
        let mut value = GhosttyString {
            ptr: std::ptr::null(),
            len: 0,
        };
        let result = unsafe {
            ghostty_terminal_get(self.raw, DATA_TITLE, &mut value as *mut _ as *mut c_void)
        };
        if result != SUCCESS || value.ptr.is_null() {
            return String::new();
        }
        let bytes = unsafe { std::slice::from_raw_parts(value.ptr, value.len) };
        String::from_utf8_lossy(bytes).into_owned()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe { ghostty_terminal_free(self.raw) };
    }
}

// SAFETY: the terminal handle is not thread-affine. libghostty-vt is built for
// lock-serialized cross-thread use — the render API documents that a renderer
// may run on another thread "as long as a lock is held during the update call
// to ensure exclusive access to the terminal instance". Radar never touches a
// `Terminal` except under its session mutex, and the daemon moves the session
// (lock included) into the worker that owns the PTY. `Sync` is deliberately
// not asserted: shared references must not escape the lock.
unsafe impl Send for Terminal {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The state queries the wheel policy reads: active screen, mouse
    /// tracking, and DEC private modes with their defaults.
    #[test]
    fn terminal_reports_screen_mouse_tracking_and_modes() {
        let mut terminal = Terminal::new(10, 3);
        assert!(
            !terminal.alt_screen(),
            "a fresh terminal is on the primary screen"
        );
        assert!(
            !terminal.mouse_tracking(),
            "nothing asks for mouse events yet"
        );
        assert!(
            terminal.dec_mode(1007),
            "alt-scroll (DECSET 1007) defaults on"
        );
        assert!(!terminal.dec_mode(1), "cursor keys (DECCKM) default off");

        terminal.write(b"\x1b[?1049h\x1b[?1000h\x1b[?1h");
        assert!(
            terminal.alt_screen(),
            "1049h switches to the alternate screen"
        );
        assert!(terminal.mouse_tracking(), "1000h enables mouse reporting");
        assert!(terminal.dec_mode(1), "1h enables application cursor keys");

        terminal.write(b"\x1b[?1049l\x1b[?1000l");
        assert!(
            !terminal.alt_screen(),
            "1049l returns to the primary screen"
        );
        assert!(!terminal.mouse_tracking(), "1000l stops mouse reporting");
    }

    #[test]
    fn effects_capture_bell_title_pwd_and_query_replies() {
        let mut terminal = Terminal::new(20, 4);
        terminal.enable_effects();
        terminal.write(b"\x07");
        terminal.write(b"\x1b]0;my-title\x07");
        terminal.write(b"\x1b]7;file://host/tmp\x07");
        terminal.write(b"\x1b[6n");

        let effects = terminal.drain_effects();
        assert!(
            effects.iter().any(|e| matches!(e, Effect::Bell)),
            "{effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Title(t) if t == "my-title")),
            "{effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::WritePty(b) if !b.is_empty())),
            "{effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Pwd(p) if p.contains("/tmp"))),
            "{effects:?}"
        );
    }
}
