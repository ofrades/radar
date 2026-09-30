//! libghostty-vt `RenderState`: the renderer-agnostic view of a terminal.
//!
//! The daemon owns terminal state; a client draws it. `RenderState` is the
//! drawing surface libghostty-vt exposes: it is updated from a [`Terminal`]
//! and yields the viewport's colors, cursor and cells (graphemes + style) in
//! one pass. This module owns the FFI for that API and presents a plain
//! [`Frame`] so the GTK renderer (and, later, the web renderer) hold no C
//! types.
//!
//! [`Terminal`]: super::Terminal

use std::ffi::c_void;

type RawRenderState = *mut c_void;
type RawRowIterator = *mut c_void;
type RawRowCells = *mut c_void;

const SUCCESS: i32 = 0;

// GhosttyRenderStateData
const DATA_COLS: i32 = 1;
const DATA_ROWS: i32 = 2;
const DATA_ROW_ITERATOR: i32 = 4;
const DATA_CURSOR: i32 = 18;
const DATA_COLORS: i32 = 19;

// GhosttyRenderStateRowData
const ROW_DATA_CELLS: i32 = 3;
const ROW_DATA_VIEWPORT_Y: i32 = 6;

// GhosttyRenderStateRowCellsData
const CELLS_DATA_STYLE: i32 = 2;
const CELLS_DATA_GRAPHEMES_LEN: i32 = 3;
const CELLS_DATA_GRAPHEMES_BUF: i32 = 4;
const CELLS_DATA_BG_COLOR: i32 = 5;
const CELLS_DATA_FG_COLOR: i32 = 6;
const CELLS_DATA_SELECTED: i32 = 7;

/// An 8-bit-per-channel colour.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// `GhosttyRenderStateColors` — the default colors and the active palette.
#[repr(C)]
#[derive(Clone, Copy)]
struct RenderStateColors {
    size: usize,
    background: Rgb,
    foreground: Rgb,
    cursor: Rgb,
    cursor_has_value: bool,
    palette: [Rgb; 256],
}

impl RenderStateColors {
    fn new() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            ..unsafe { std::mem::zeroed() }
        }
    }
}

/// `GhosttyRenderStateCursor`.
#[repr(C)]
#[derive(Clone, Copy)]
struct RenderStateCursor {
    size: usize,
    viewport_has_value: bool,
    viewport_x: u16,
    viewport_y: u16,
    wide_tail: bool,
    visible: bool,
    blinking: bool,
    password_input: bool,
    visual_style: i32,
}

impl RenderStateCursor {
    fn new() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            ..unsafe { std::mem::zeroed() }
        }
    }
}

/// `GhosttyStyle`. The three colour fields are opaque here: the renderer reads
/// resolved colors per cell instead of resolving style tags itself.
#[repr(C)]
#[derive(Clone, Copy)]
struct Style {
    size: usize,
    _fg_color: [u64; 2],
    _bg_color: [u64; 2],
    _underline_color: [u64; 2],
    bold: bool,
    italic: bool,
    faint: bool,
    blink: bool,
    inverse: bool,
    invisible: bool,
    strikethrough: bool,
    overline: bool,
    underline: i32,
}

impl Style {
    fn new() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            ..unsafe { std::mem::zeroed() }
        }
    }
}

extern "C" {
    fn ghostty_render_state_new(allocator: *const c_void, out: *mut RawRenderState) -> i32;
    fn ghostty_render_state_free(state: RawRenderState);
    fn ghostty_render_state_update(state: RawRenderState, terminal: super::RawTerminal) -> i32;
    fn ghostty_render_state_clean(state: RawRenderState) -> i32;
    fn ghostty_render_state_get(state: RawRenderState, data: i32, out: *mut c_void) -> i32;

    fn ghostty_render_state_row_iterator_new(
        allocator: *const c_void,
        out: *mut RawRowIterator,
    ) -> i32;
    fn ghostty_render_state_row_iterator_free(iterator: RawRowIterator);
    fn ghostty_render_state_row_iterator_next(iterator: RawRowIterator) -> bool;
    fn ghostty_render_state_row_get(iterator: RawRowIterator, data: i32, out: *mut c_void) -> i32;

    fn ghostty_render_state_row_cells_new(allocator: *const c_void, out: *mut RawRowCells) -> i32;
    fn ghostty_render_state_row_cells_free(cells: RawRowCells);
    fn ghostty_render_state_row_cells_next(cells: RawRowCells) -> bool;
    fn ghostty_render_state_row_cells_get(cells: RawRowCells, data: i32, out: *mut c_void) -> i32;
}

/// A renderer's view of one terminal frame.
#[derive(Debug, Clone)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub background: Rgb,
    pub foreground: Rgb,
    /// The active 256-color palette.
    pub palette: [Rgb; 256],
    pub cursor: Option<Cursor>,
    /// The viewport's rows, top first.
    pub lines: Vec<Row>,
}

/// Where and how to draw the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
    pub blinking: bool,
}

/// One viewport row.
#[derive(Debug, Clone)]
pub struct Row {
    /// Position relative to the top of the viewport; 0 is the top row.
    pub y: i32,
    pub cells: Vec<Cell>,
}

/// One cell: its grapheme cluster and resolved style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub inverse: bool,
    /// Whether the cell is inside the terminal's active selection.
    pub selected: bool,
}

/// A reusable render state for one terminal.
pub struct RenderState {
    raw: RawRenderState,
}

impl RenderState {
    pub fn new() -> Self {
        let mut raw: RawRenderState = std::ptr::null_mut();
        let result = unsafe { ghostty_render_state_new(std::ptr::null(), &mut raw) };
        assert_eq!(result, SUCCESS, "ghostty_render_state_new failed: {result}");
        Self { raw }
    }

    /// Read the current frame from `terminal`. This updates the render state
    /// and consumes the terminal's dirty state.
    pub fn frame(&mut self, terminal: &super::Terminal) -> Frame {
        let result = unsafe { ghostty_render_state_update(self.raw, terminal.raw()) };
        assert_eq!(
            result, SUCCESS,
            "ghostty_render_state_update failed: {result}"
        );
        let frame = self.read();
        unsafe { ghostty_render_state_clean(self.raw) };
        frame
    }

    fn read(&self) -> Frame {
        let (mut cols, mut rows) = (0u16, 0u16);
        self.get(DATA_COLS, &mut cols);
        self.get(DATA_ROWS, &mut rows);

        let mut colors = RenderStateColors::new();
        self.get(DATA_COLORS, &mut colors);

        let mut cursor = RenderStateCursor::new();
        self.get(DATA_CURSOR, &mut cursor);
        let cursor = cursor.viewport_has_value.then_some(Cursor {
            x: cursor.viewport_x,
            y: cursor.viewport_y,
            visible: cursor.visible,
            blinking: cursor.blinking,
        });

        let mut rows_out = Vec::new();
        let mut iterator: RawRowIterator = std::ptr::null_mut();
        let mut cells: RawRowCells = std::ptr::null_mut();
        unsafe {
            assert_eq!(
                ghostty_render_state_row_iterator_new(std::ptr::null(), &mut iterator),
                SUCCESS
            );
            self.get(DATA_ROW_ITERATOR, &mut iterator);
            assert_eq!(
                ghostty_render_state_row_cells_new(std::ptr::null(), &mut cells),
                SUCCESS
            );
            while ghostty_render_state_row_iterator_next(iterator) {
                let mut y = 0i32;
                ghostty_render_state_row_get(
                    iterator,
                    ROW_DATA_VIEWPORT_Y,
                    &mut y as *mut _ as *mut c_void,
                );
                ghostty_render_state_row_get(
                    iterator,
                    ROW_DATA_CELLS,
                    &mut cells as *mut _ as *mut c_void,
                );
                rows_out.push(Row {
                    y,
                    cells: read_row(cells),
                });
            }
            ghostty_render_state_row_cells_free(cells);
            ghostty_render_state_row_iterator_free(iterator);
        }

        Frame {
            cols,
            rows,
            background: colors.background,
            foreground: colors.foreground,
            palette: colors.palette,
            cursor,
            lines: rows_out,
        }
    }

    fn get<T>(&self, data: i32, out: &mut T) {
        let result =
            unsafe { ghostty_render_state_get(self.raw, data, out as *mut T as *mut c_void) };
        assert_eq!(result, SUCCESS, "render state data {data} failed: {result}");
    }
}

impl Default for RenderState {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for RenderState {
    fn drop(&mut self) {
        unsafe { ghostty_render_state_free(self.raw) };
    }
}

/// Read every cell in the row's `cells` container.
///
/// # Safety
/// `cells` must be populated by `ghostty_render_state_row_get` for a live row.
unsafe fn read_row(cells: RawRowCells) -> Vec<Cell> {
    let mut out = Vec::new();
    let mut graphemes: Vec<u32> = Vec::new();
    unsafe {
        while ghostty_render_state_row_cells_next(cells) {
            let mut style = Style::new();
            ghostty_render_state_row_cells_get(
                cells,
                CELLS_DATA_STYLE,
                &mut style as *mut _ as *mut c_void,
            );

            let mut len = 0u32;
            ghostty_render_state_row_cells_get(
                cells,
                CELLS_DATA_GRAPHEMES_LEN,
                &mut len as *mut _ as *mut c_void,
            );
            graphemes.clear();
            graphemes.resize(len as usize, 0);
            if len > 0 {
                ghostty_render_state_row_cells_get(
                    cells,
                    CELLS_DATA_GRAPHEMES_BUF,
                    graphemes.as_mut_ptr() as *mut c_void,
                );
            }
            let text: String = graphemes
                .iter()
                .filter_map(|&cp| char::from_u32(cp))
                .collect();

            out.push(Cell {
                text,
                fg: resolved_color(cells, CELLS_DATA_FG_COLOR),
                bg: resolved_color(cells, CELLS_DATA_BG_COLOR),
                bold: style.bold,
                italic: style.italic,
                faint: style.faint,
                underline: style.underline != 0,
                strikethrough: style.strikethrough,
                inverse: style.inverse,
                selected: read_bool(cells, CELLS_DATA_SELECTED),
            });
        }
    }
    out
}

/// Read a resolved color, `None` when the cell has no explicit value.
///
/// # Safety
/// `cells` must be positioned on a cell.
unsafe fn resolved_color(cells: RawRowCells, data: i32) -> Option<Rgb> {
    let mut rgb = Rgb::default();
    let result = unsafe {
        ghostty_render_state_row_cells_get(cells, data, &mut rgb as *mut Rgb as *mut c_void)
    };
    (result == SUCCESS).then_some(rgb)
}

/// Read a boolean cell datum, `false` when unavailable.
///
/// # Safety
/// `cells` must be positioned on a cell.
unsafe fn read_bool(cells: RawRowCells, data: i32) -> bool {
    let mut value = false;
    let result = unsafe {
        ghostty_render_state_row_cells_get(cells, data, &mut value as *mut bool as *mut c_void)
    };
    result == SUCCESS && value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ghostty::Terminal;

    fn text_of(frame: &Frame) -> String {
        frame
            .lines
            .iter()
            .flat_map(|row| row.cells.iter().map(|cell| cell.text.as_str()))
            .collect()
    }

    #[test]
    fn frame_reports_text_style_colors_and_cursor() {
        let mut terminal = Terminal::new(20, 4);
        terminal.write(b"\x1b[1;32mgreen bold\x1b[0m plain");

        let mut render = RenderState::new();
        let frame = render.frame(&terminal);

        assert_eq!(frame.cols, 20);
        assert_eq!(frame.rows, 4);
        assert!(text_of(&frame).starts_with("green bold plain"));

        let first = &frame.lines[0].cells[0];
        assert!(first.bold, "SGR 1 must survive as bold");
        assert!(first.fg.is_some(), "SGR 32 sets an explicit foreground");

        // After the reset the cell has no explicit colour.
        let after_reset = &frame.lines[0].cells["green bold ".len()];
        assert!(!after_reset.bold);
        assert_eq!(after_reset.fg, None);

        let cursor = frame.cursor.expect("the cursor is in the viewport");
        assert_eq!(cursor.x, "green bold plain".len() as u16);
        assert!(cursor.visible);
    }

    #[test]
    fn frame_tracks_a_cursor_hidden_by_a_mode() {
        let mut terminal = Terminal::new(10, 2);
        terminal.write(b"\x1b[?25lhi");
        let mut render = RenderState::new();
        let frame = render.frame(&terminal);
        let cursor = frame.cursor.expect("cursor still has a viewport position");
        assert!(!cursor.visible);
    }

    #[test]
    fn scrolling_moves_the_viewport_through_history() {
        let mut terminal = Terminal::new(8, 2);
        for i in 0..20 {
            terminal.write(format!("L{i:02}\r\n").as_bytes());
        }
        let mut render = RenderState::new();
        let bottom_top: String = render.frame(&terminal).lines[0]
            .cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect();

        terminal.scroll_viewport(-5);
        let scrolled_top: String = render.frame(&terminal).lines[0]
            .cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect();
        assert_ne!(
            bottom_top, scrolled_top,
            "scrolling up must reveal older rows"
        );

        terminal.scroll_viewport_bottom();
        let back_top: String = render.frame(&terminal).lines[0]
            .cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect();
        assert_eq!(
            bottom_top, back_top,
            "scroll bottom returns to the live view"
        );
    }

    #[test]
    fn select_all_formats_plain_text_and_marks_cells() {
        let mut terminal = Terminal::new(12, 3);
        terminal.write(b"hello\r\nworld");
        assert!(terminal.select_all(), "select_all must succeed");

        let text = terminal.selection_text().expect("a selection formats");
        assert!(text.contains("hello"), "got {text:?}");
        assert!(text.contains("world"), "got {text:?}");

        let mut render = RenderState::new();
        let frame = render.frame(&terminal);
        let selected = frame
            .lines
            .iter()
            .flat_map(|row| row.cells.iter())
            .any(|cell| cell.selected);
        assert!(selected, "selected cells must be reported to the renderer");
    }

    #[test]
    fn viewport_selection_and_render_survive_edge_points() {
        let mut terminal = Terminal::new(20, 4);
        terminal.write(b"hello world\r\nsecond line\r\nthird");
        assert!(terminal.select_viewport((0, 0), (4, 0)));
        assert!(terminal.selection_text().unwrap().contains("hello"));
        let mut render = RenderState::new();
        let _ = render.frame(&terminal);

        // Out-of-bounds rows and columns must not corrupt the terminal.
        terminal.select_viewport((19, 3), (0, 0));
        terminal.select_viewport((0, 99), (19, 99));
        terminal.select_viewport((99, 0), (99, 3));
        let _ = render.frame(&terminal);

        // And after scrolling the viewport.
        terminal.scroll_viewport(-2);
        terminal.select_viewport((0, 0), (5, 1));
        let _ = render.frame(&terminal);
        terminal.scroll_viewport_bottom();
        let _ = render.frame(&terminal);
    }
}
