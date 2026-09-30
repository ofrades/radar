//! A GTK4 terminal view drawn from libghostty-vt `RenderState`.
//!
//! The pane owns a local libghostty-vt terminal. The daemon hands it a lossless
//! snapshot on attach and the raw byte stream after the snapshot watermark;
//! this view decodes/feeds both into that terminal and paints its render state
//! into a `GtkDrawingArea` with cairo. Input is encoded here and handed to a
//! callback the pane wires to the daemon attachment.
//!
//! This replaces the VTE widget (which renders a bounded ANSI replay) for
//! builds with the `ghostty` feature.

use std::cell::RefCell;
use std::rc::Rc;

use parking_lot::Mutex;

use gtk::cairo;
use gtk::glib;
use gtk::prelude::*;

use crate::ghostty::render::{Frame, RenderState, Rgb};
use crate::ghostty::Terminal;

use super::pane::ShiftEnter;

/// How the view maps cells to pixels and text.
struct Metrics {
    family: String,
    font_size: f64,
    cell_w: f64,
    cell_h: f64,
    /// Baseline from the cell's top: the font's rounded ascent, so glyphs in
    /// every cell land on the same pixel phase.
    baseline: f64,
}

impl Metrics {
    /// Measure the font on a scratch surface. Cell metrics come from the font
    /// itself and snap to whole pixels: fractional advances (the old 0.6·size
    /// guess) put every glyph on a different subpixel phase, which smeared the
    /// panel — the pixelization report — and broke box-drawing joins.
    fn new(family: &str, font_size: f64, scale: f64) -> Self {
        // The theme's size is in points (kitty font_size), like the Pango path
        // this renderer replaced: pixels at GTK's 96 dpi are 4/3 of a point.
        let size = font_size * scale * 4.0 / 3.0;
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 8, 8).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        cr.select_font_face(family, cairo::FontSlant::Normal, cairo::FontWeight::Normal);
        cr.set_font_size(size);
        let extents = cr.font_extents().unwrap();
        let sample = cr.text_extents("MMMMMMMMMM").unwrap();
        let cell_w = (sample.width() / 10.0).round().max(1.0);
        let ascent = extents.ascent().round().max(1.0);
        let descent = extents.descent().round().max(1.0);
        Self {
            family: family.to_string(),
            font_size: size,
            cell_w,
            cell_h: ascent + descent,
            baseline: ascent,
        }
    }

    fn cols(&self, width: i32) -> u16 {
        ((width as f64 / self.cell_w).floor() as i64).clamp(1, u16::MAX as i64) as u16
    }

    fn rows(&self, height: i32) -> u16 {
        ((height as f64 / self.cell_h).floor() as i64).clamp(1, u16::MAX as i64) as u16
    }
}

/// Where encoded input bytes go (the daemon attachment). A Mutex, not a
/// RefCell: a keystroke must never race a re-attach and get dropped — a
/// fallible borrow here silently ate keys (the abort fix made conflicts
/// skip instead of panic, and a skipped keystroke is a lost one).
type InputSink = Rc<Mutex<Option<Box<dyn Fn(&[u8]) + Send>>>>;
/// Where grid-size changes go (the daemon attachment's PTY). Same reasoning.
type ResizeSink = Rc<Mutex<Option<Box<dyn Fn(u16, u16) + Send>>>>;

struct Inner {
    term: Terminal,
    render: RenderState,
    metrics: Metrics,
    base_family: String,
    base_size: f64,
    scale: f64,
    /// The grid size last reported to the pane, so a resize is sent once.
    last_grid: (u16, u16),
}

/// The terminal widget: a drawing area plus the engine behind it.
#[derive(Clone)]
pub struct TerminalView {
    area: gtk::DrawingArea,
    inner: Rc<RefCell<Inner>>,
    input: InputSink,
    resize: ResizeSink,
    shift_enter: Rc<std::cell::Cell<ShiftEnter>>,
    /// Where a drag selection started, in viewport cells.
    anchor: std::cell::Cell<Option<(u16, u32)>>,
    /// Whether the primary button is currently held.
    dragging: Rc<std::cell::Cell<bool>>,
    /// The primary-button gesture, for probes that emit a synthetic press.
    click: gtk::GestureClick,
}

impl TerminalView {
    pub fn new(family: &str, font_size: f64) -> Self {
        let area = gtk::DrawingArea::new();
        area.set_hexpand(true);
        area.set_vexpand(true);
        area.set_focusable(true);
        area.set_can_focus(true);

        let inner = Rc::new(RefCell::new(Inner {
            term: Terminal::new(crate::session::DEFAULT_COLS, crate::session::DEFAULT_ROWS),
            render: RenderState::new(),
            metrics: Metrics::new(family, font_size, 1.0),
            base_family: family.to_string(),
            base_size: font_size,
            scale: 1.0,
            last_grid: (0, 0),
        }));

        let resize: ResizeSink = Rc::new(Mutex::new(None));
        let draw_inner = inner.clone();
        let draw_resize = resize.clone();
        area.set_draw_func(move |_area, cr, width, height| {
            draw(&draw_inner, &draw_resize, cr, width, height);
        });

        let input: InputSink = Rc::new(Mutex::new(None));
        let shift_enter = Rc::new(std::cell::Cell::new(ShiftEnter::Off));
        let click = gtk::GestureClick::new();
        click.set_button(1);

        let view = Self {
            area,
            inner,
            input,
            resize,
            shift_enter,
            anchor: std::cell::Cell::new(None),
            dragging: Rc::new(std::cell::Cell::new(false)),
            click: click.clone(),
        };

        // Keys: radar's own chords on Alt, everything else encoded for the
        // program. The view is focusable so it receives them directly.
        let keys = gtk::EventControllerKey::new();
        let view_for_keys = view.clone();
        keys.connect_key_pressed(move |_controller, key, _code, state| {
            let alt = state.contains(gtk::gdk::ModifierType::ALT_MASK);
            // Font zoom, per pane, on Alt+= / Alt+- / Alt+0.
            if alt
                && matches!(
                    key,
                    gtk::gdk::Key::plus | gtk::gdk::Key::equal | gtk::gdk::Key::KP_Add
                )
            {
                view_for_keys.zoom(1.0);
                return glib::Propagation::Stop;
            }
            if alt && matches!(key, gtk::gdk::Key::minus | gtk::gdk::Key::KP_Subtract) {
                view_for_keys.zoom(-1.0);
                return glib::Propagation::Stop;
            }
            if alt && matches!(key, gtk::gdk::Key::_0 | gtk::gdk::Key::KP_0) {
                view_for_keys.reset_zoom();
                return glib::Propagation::Stop;
            }
            // Alt+C copies the selection (or the whole screen when none).
            if alt && matches!(key, gtk::gdk::Key::c | gtk::gdk::Key::C) {
                view_for_keys.copy_selection();
                return glib::Propagation::Stop;
            }

            let shift_only = state.contains(gtk::gdk::ModifierType::SHIFT_MASK)
                && !state.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                && !alt;
            if shift_only && matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter) {
                let sequence: Option<&[u8]> = match view_for_keys.shift_enter.get() {
                    ShiftEnter::Off => None,
                    ShiftEnter::Kitty => Some(b"\x1b[13;2u"),
                    ShiftEnter::Meta => Some(b"\x1b\r"),
                };
                if let Some(sequence) = sequence {
                    let send = view_for_keys.input.lock();
                    if let Some(send) = send.as_ref() {
                        send(sequence);
                    }
                    return glib::Propagation::Stop;
                }
            }
            // Shift+PageUp/Down scroll the scrollback instead of paging the
            // program, the universal terminal convention.
            let shift = state.contains(gtk::gdk::ModifierType::SHIFT_MASK);
            if shift && matches!(key, gtk::gdk::Key::Page_Up | gtk::gdk::Key::Page_Down) {
                let rows = view_for_keys
                    .inner
                    .try_borrow()
                    .map(|inner| inner.last_grid.1.max(1) as i32)
                    .unwrap_or(24);
                let delta = if key == gtk::gdk::Key::Page_Up {
                    -rows
                } else {
                    rows
                };
                view_for_keys.scroll(delta);
                return glib::Propagation::Stop;
            }
            if let Some(bytes) = encode_key(key, state) {
                // Typing returns to the live view, like every terminal.
                view_for_keys.scroll_bottom();
                let send = view_for_keys.input.lock();
                if let Some(send) = send.as_ref() {
                    send(&bytes);
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        view.area.add_controller(keys);

        // Mouse wheel scrolls the scrollback.
        let wheel = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::DISCRETE,
        );
        let view_for_wheel = view.clone();
        wheel.connect_scroll(move |_controller, _dx, dy| {
            let delta = if dy < 0.0 { -3 } else { 3 };
            view_for_wheel.scroll(delta);
            glib::Propagation::Stop
        });
        view.area.add_controller(wheel);

        // Primary-button drag selects; Alt+C copies (handled with the keys).
        // A press also takes the keyboard: the panel you clicked owns every
        // key from now on — Escape included — instead of leaving focus on
        // radar's chrome, where keystrokes go nowhere.
        let view_for_press = view.clone();
        view.click.connect_pressed(move |_gesture, _n, px, py| {
            view_for_press.area.grab_focus();
            let cell = view_for_press.cell_at(px, py);
            view_for_press.anchor.set(Some(cell));
            view_for_press.dragging.set(true);
            view_for_press.select_cells(cell, cell);
        });
        let view_for_release = view.clone();
        view.click.connect_released(move |_gesture, _n, px, py| {
            if let Some(start) = view_for_release.anchor.get() {
                let cell = view_for_release.cell_at(px, py);
                view_for_release.select_cells(start, cell);
            }
            view_for_release.dragging.set(false);
        });
        view.area.add_controller(click.clone());

        let motion = gtk::EventControllerMotion::new();
        let view_for_motion = view.clone();
        motion.connect_motion(move |_controller, px, py| {
            if !view_for_motion.dragging.get() {
                return;
            }
            if let Some(start) = view_for_motion.anchor.get() {
                let cell = view_for_motion.cell_at(px, py);
                view_for_motion.select_cells(start, cell);
            }
        });
        view.area.add_controller(motion);

        // Paste with Alt+V (radar's chord vocabulary; Ctrl stays with the TUI).
        let paste = gtk::EventControllerKey::new();
        let view_for_paste = view.clone();
        paste.connect_key_pressed(move |_controller, key, _code, state| {
            let alt = state.contains(gtk::gdk::ModifierType::ALT_MASK);
            if alt && matches!(key, gtk::gdk::Key::v | gtk::gdk::Key::V) {
                let view = view_for_paste.clone();
                view.area.clipboard().read_text_async(
                    None::<&gtk::gio::Cancellable>,
                    move |result| {
                        if let Ok(Some(text)) = result.map(|text| text.map(|text| text.to_string()))
                        {
                            let send = view.input.lock();
                            if let Some(send) = send.as_ref() {
                                send(text.as_bytes());
                            }
                        }
                        view.area.queue_draw();
                    },
                );
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        view.area.add_controller(paste);

        view
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    /// Wire where encoded input goes (the daemon attachment).
    pub fn set_input(&self, send: impl Fn(&[u8]) + Send + 'static) {
        *self.input.lock() = Some(Box::new(send));
    }

    /// Wire where grid-size changes go (the daemon attachment's PTY).
    pub fn set_resize(&self, send: impl Fn(u16, u16) + Send + 'static) {
        *self.resize.lock() = Some(Box::new(send));
    }

    /// How Shift+Enter is encoded for this pane's program.
    pub fn set_shift_enter(&self, mode: ShiftEnter) {
        self.shift_enter.set(mode);
    }

    /// Replace the terminal with one restored from a daemon snapshot.
    pub fn load_snapshot(&self, bytes: &[u8]) {
        let Ok(mut inner) = self.inner.try_borrow_mut() else {
            return;
        };
        inner.term = Terminal::from_snapshot(bytes);
        // The restored grid may differ from the widget; re-fit on next draw.
        inner.last_grid = (0, 0);
        drop(inner);
        self.area.queue_draw();
    }

    /// Feed program output after the snapshot watermark.
    pub fn feed(&self, bytes: &[u8]) {
        let Ok(mut inner) = self.inner.try_borrow_mut() else {
            return;
        };
        inner.term.write(bytes);
        drop(inner);
        self.area.queue_draw();
    }

    /// Scroll the viewport by `delta` rows (negative scrolls into history).
    pub fn scroll(&self, delta: i32) {
        let Ok(mut inner) = self.inner.try_borrow_mut() else {
            return;
        };
        inner.term.scroll_viewport(delta);
        drop(inner);
        self.area.queue_draw();
    }

    /// Snap the viewport back to the active area.
    pub fn scroll_bottom(&self) {
        let Ok(mut inner) = self.inner.try_borrow_mut() else {
            return;
        };
        inner.term.scroll_viewport_bottom();
        drop(inner);
        self.area.queue_draw();
    }

    /// Emit a synthetic primary-button press through the real GTK gesture, so a
    /// probe exercises the same dispatch path as a click.
    pub fn emit_click_press(&self, px: f64, py: f64) {
        self.click.emit_by_name::<()>("pressed", &[&1i32, &px, &py]);
    }

    /// Mirror of the primary-button press handler, for probes and tests.
    pub fn simulate_press(&self, px: f64, py: f64) {
        let cell = self.cell_at(px, py);
        self.anchor.set(Some(cell));
        self.dragging.set(true);
        self.select_cells(cell, cell);
    }

    /// The viewport cell under a pixel position.
    fn cell_at(&self, px: f64, py: f64) -> (u16, u32) {
        let Ok(inner) = self.inner.try_borrow() else {
            return (0, 0);
        };
        let cols = inner.last_grid.0.max(1) as u32;
        let rows = inner.last_grid.1.max(1) as u32;
        let column = ((px / inner.metrics.cell_w).floor().max(0.0) as u32).min(cols - 1);
        let row = ((py / inner.metrics.cell_h).floor().max(0.0) as u32).min(rows - 1);
        (column as u16, row)
    }

    /// Install a selection between two viewport cells.
    fn select_cells(&self, start: (u16, u32), end: (u16, u32)) {
        let Ok(mut inner) = self.inner.try_borrow_mut() else {
            return;
        };
        inner.term.select_viewport(start, end);
        drop(inner);
        self.area.queue_draw();
    }

    /// Copy the active selection to the clipboard, or the whole screen when
    /// there is no selection.
    pub fn copy_selection(&self) {
        let text = {
            let Ok(mut inner) = self.inner.try_borrow_mut() else {
                return;
            };
            match inner.term.selection_text() {
                Some(text) if !text.is_empty() => Some(text),
                _ => {
                    inner.term.select_all();
                    inner.term.selection_text()
                }
            }
        };
        if let Some(text) = text {
            if !text.is_empty() {
                self.area.clipboard().set_text(&text);
            }
        }
        self.area.queue_draw();
    }

    /// Apply a theme's font.
    pub fn apply_font(&self, family: &str, font_size: f64) {
        let Ok(mut inner) = self.inner.try_borrow_mut() else {
            return;
        };
        inner.base_family = family.to_string();
        inner.base_size = font_size;
        let scale = inner.scale;
        inner.metrics = Metrics::new(family, font_size, scale);
        drop(inner);
        self.area.queue_draw();
    }

    /// One font-zoom step for this view.
    pub fn zoom(&self, direction: f64) {
        let Ok(mut inner) = self.inner.try_borrow_mut() else {
            return;
        };
        const STEP: f64 = 1.1;
        let factor = if direction > 0.0 { STEP } else { 1.0 / STEP };
        let scale = ((inner.scale * factor * 100.0).round() / 100.0).clamp(0.25, 4.0);
        inner.scale = scale;
        let (family, size) = (inner.base_family.clone(), inner.base_size);
        inner.metrics = Metrics::new(&family, size, scale);
        drop(inner);
        self.area.queue_draw();
    }

    pub fn reset_zoom(&self) {
        let Ok(mut inner) = self.inner.try_borrow_mut() else {
            return;
        };
        inner.scale = 1.0;
        let (family, size) = (inner.base_family.clone(), inner.base_size);
        inner.metrics = Metrics::new(&family, size, 1.0);
        drop(inner);
        self.area.queue_draw();
    }
}

fn set_rgb(cr: &cairo::Context, color: Rgb) {
    cr.set_source_rgb(
        color.r as f64 / 255.0,
        color.g as f64 / 255.0,
        color.b as f64 / 255.0,
    );
}

/// Paint one frame.
fn draw(
    inner: &Rc<RefCell<Inner>>,
    resize: &ResizeSink,
    cr: &cairo::Context,
    width: i32,
    height: i32,
) {
    let Ok(mut inner) = inner.try_borrow_mut() else {
        return;
    };
    if let Some(grid) = paint(&mut inner, cr, width, height) {
        let send = resize.lock();
        if let Some(send) = send.as_ref() {
            send(grid.0, grid.1);
        }
    }
}

/// Paint the engine's current frame. Returns the new grid when the widget size
/// changed the grid, so the caller can forward it to the daemon PTY.
fn paint(inner: &mut Inner, cr: &cairo::Context, width: i32, height: i32) -> Option<(u16, u16)> {
    let Inner {
        term,
        render,
        metrics,
        last_grid,
        ..
    } = inner;

    // A new widget size means a new grid.
    let grid = (metrics.cols(width), metrics.rows(height));
    let changed = if grid != *last_grid {
        *last_grid = grid;
        term.resize(grid.0, grid.1);
        Some(grid)
    } else {
        None
    };

    let frame = render.frame(term);

    // Background first, so every cell sits on the terminal's own colour.
    set_rgb(cr, frame.background);
    cr.rectangle(0.0, 0.0, width as f64, height as f64);
    cr.fill().ok();

    for row in &frame.lines {
        if row.y < 0 {
            continue;
        }
        let y = row.y as f64 * metrics.cell_h;
        if y >= height as f64 {
            break;
        }
        for (column, cell) in row.cells.iter().enumerate() {
            let x = column as f64 * metrics.cell_w;
            if x >= width as f64 {
                break;
            }
            paint_cell(cr, metrics, x, y, cell, &frame);
        }
    }

    if let Some(cursor) = frame.cursor {
        if cursor.visible {
            let x = cursor.x as f64 * metrics.cell_w;
            let y = cursor.y as f64 * metrics.cell_h;
            set_rgb(cr, frame.foreground);
            cr.set_line_width(2.0);
            cr.rectangle(x + 1.0, y + 1.0, metrics.cell_w - 2.0, metrics.cell_h - 2.0);
            cr.stroke().ok();
        }
    }

    changed
}

fn paint_cell(
    cr: &cairo::Context,
    metrics: &Metrics,
    x: f64,
    y: f64,
    cell: &crate::ghostty::render::Cell,
    frame: &Frame,
) {
    // Inverse swaps the resolved foreground/background.
    let (fg, bg) = if cell.inverse {
        (
            cell.bg.unwrap_or(frame.background),
            cell.fg.unwrap_or(frame.foreground),
        )
    } else {
        (
            cell.fg.unwrap_or(frame.foreground),
            cell.bg.unwrap_or(frame.background),
        )
    };

    if bg != frame.background {
        set_rgb(cr, bg);
        cr.rectangle(x, y, metrics.cell_w, metrics.cell_h);
        cr.fill().ok();
    }

    // A selection tint over the cell, drawn under the glyph.
    if cell.selected {
        cr.set_source_rgba(
            fg.r as f64 / 255.0,
            fg.g as f64 / 255.0,
            fg.b as f64 / 255.0,
            0.3,
        );
        cr.rectangle(x, y, metrics.cell_w, metrics.cell_h);
        cr.fill().ok();
    }

    if !cell.text.is_empty() {
        cr.select_font_face(
            &metrics.family,
            if cell.italic {
                cairo::FontSlant::Italic
            } else {
                cairo::FontSlant::Normal
            },
            if cell.bold {
                cairo::FontWeight::Bold
            } else {
                cairo::FontWeight::Normal
            },
        );
        cr.set_font_size(metrics.font_size);
        let mut color = fg;
        if cell.faint {
            color = Rgb {
                r: color.r / 2,
                g: color.g / 2,
                b: color.b / 2,
            };
        }
        set_rgb(cr, color);
        cr.move_to(x, y + metrics.baseline);
        cr.show_text(&cell.text).ok();
    }

    if cell.underline || cell.strikethrough {
        set_rgb(cr, fg);
        cr.set_line_width(1.0);
        let line_y = if cell.underline {
            y + metrics.cell_h - 2.0
        } else {
            y + metrics.cell_h / 2.0
        };
        cr.move_to(x, line_y);
        cr.line_to(x + metrics.cell_w, line_y);
        cr.stroke().ok();
    }
}

/// Encode a GTK key press as the bytes a program expects.
///
/// Covers the common cases (printable text, Ctrl+letter, Alt prefix, cursor and
/// editing keys, function keys). Kitty-keyboard-protocol fidelity (key release
/// reporting, modifyOtherKeys) is a follow-up; this is the classic xterm
/// vocabulary every TUI accepts.
fn encode_key(key: gtk::gdk::Key, state: gtk::gdk::ModifierType) -> Option<Vec<u8>> {
    use gtk::gdk::Key;

    let ctrl = state.contains(gtk::gdk::ModifierType::CONTROL_MASK);
    let alt = state.contains(gtk::gdk::ModifierType::ALT_MASK);
    let shift = state.contains(gtk::gdk::ModifierType::SHIFT_MASK);

    let special: Option<&'static [u8]> = match key {
        Key::Return | Key::KP_Enter => Some(b"\r"),
        Key::BackSpace => Some(b"\x7f"),
        Key::Tab => Some(b"\t"),
        Key::ISO_Left_Tab => Some(b"\x1b[Z"),
        Key::Escape => Some(b"\x1b"),
        Key::Up => Some(b"\x1b[A"),
        Key::Down => Some(b"\x1b[B"),
        Key::Right => Some(b"\x1b[C"),
        Key::Left => Some(b"\x1b[D"),
        Key::Home => Some(b"\x1b[H"),
        Key::End => Some(b"\x1b[F"),
        Key::Insert => Some(b"\x1b[2~"),
        Key::Delete => Some(b"\x1b[3~"),
        Key::Page_Up => Some(b"\x1b[5~"),
        Key::Page_Down => Some(b"\x1b[6~"),
        Key::F1 => Some(b"\x1bOP"),
        Key::F2 => Some(b"\x1bOQ"),
        Key::F3 => Some(b"\x1bOR"),
        Key::F4 => Some(b"\x1bOS"),
        Key::F5 => Some(b"\x1b[15~"),
        Key::F6 => Some(b"\x1b[17~"),
        Key::F7 => Some(b"\x1b[18~"),
        Key::F8 => Some(b"\x1b[19~"),
        Key::F9 => Some(b"\x1b[20~"),
        Key::F10 => Some(b"\x1b[21~"),
        Key::F11 => Some(b"\x1b[23~"),
        Key::F12 => Some(b"\x1b[24~"),
        _ => None,
    };
    if let Some(bytes) = special {
        return Some(prefix_alt(bytes, alt));
    }

    if ctrl {
        // Ctrl+letter is the control byte; Ctrl+symbols follow the classic
        // mapping. Ctrl+Shift+letter is the same byte as Ctrl+letter.
        if let Some(c) = key.to_unicode() {
            let c = c.to_ascii_lowercase();
            let byte = match c {
                'a'..='z' => Some(c as u8 - b'a' + 1),
                ' ' | '@' => Some(0),
                '[' => Some(0x1b),
                '\\' => Some(0x1c),
                ']' => Some(0x1d),
                '^' => Some(0x1e),
                '_' | '/' => Some(0x1f),
                _ => None,
            };
            if let Some(byte) = byte {
                return Some(prefix_alt(&[byte], alt));
            }
        }
        return None;
    }

    // Printable text. Shift is applied through the shifted keyval.
    let keyval = if shift { key.to_upper() } else { key };
    if let Some(c) = keyval.to_unicode() {
        if !c.is_control() {
            let mut bytes = String::from(c).into_bytes();
            if alt {
                bytes.insert(0, 0x1b);
            }
            return Some(bytes);
        }
    }
    None
}

fn prefix_alt(bytes: &[u8], alt: bool) -> Vec<u8> {
    if !alt {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len() + 1);
    out.push(0x1b);
    out.extend_from_slice(bytes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_inner() -> Inner {
        Inner {
            term: Terminal::new(10, 2),
            render: RenderState::new(),
            metrics: Metrics::new("monospace", 12.0, 1.0),
            base_family: "monospace".to_string(),
            base_size: 12.0,
            scale: 1.0,
            last_grid: (0, 0),
        }
    }

    #[test]
    fn paint_resizes_to_the_widget_and_fills_a_cell_background() {
        let mut inner = test_inner();
        // A red cell background (SGR 41) proves the renderer resolved the
        // style and painted it; the resize proves the grid follows the widget.
        inner.term.write(b"\x1b[41mhi\x1b[0m");

        let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 200, 60).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        let changed = paint(&mut inner, &cr, 200, 60);
        drop(cr);

        assert_eq!(
            changed,
            Some((inner.metrics.cols(200), inner.metrics.rows(60))),
            "the first paint reports the grid it fitted"
        );

        let data = surface.data().unwrap();
        let pixel = [data[0], data[1], data[2], data[3]];
        // ARgb32 on little-endian is B,G,R,A. The default red palette entry is
        // a muted red, so assert "clearly red" rather than a pure 255,0,0.
        assert!(
            pixel[2] > pixel[1] + 50 && pixel[2] > pixel[0] + 50,
            "expected a red cell background, got BGRA {pixel:?}"
        );
    }

    #[test]
    fn encode_key_maps_text_control_and_special_keys() {
        use gtk::gdk::ModifierType as Mods;
        let none = Mods::empty();
        assert_eq!(encode_key(gtk::gdk::Key::a, none), Some(b"a".to_vec()));
        assert_eq!(
            encode_key(gtk::gdk::Key::a, Mods::CONTROL_MASK),
            Some(vec![0x01])
        );
        assert_eq!(
            encode_key(gtk::gdk::Key::a, Mods::ALT_MASK),
            Some(b"\x1ba".to_vec())
        );
        assert_eq!(
            encode_key(gtk::gdk::Key::Return, none),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            encode_key(gtk::gdk::Key::Up, none),
            Some(b"\x1b[A".to_vec())
        );
    }

    /// Visual probe: paint a representative frame to a PNG when
    /// `RADAR_TERM_PNG` names a path, so a reviewer can inspect glyph quality
    /// (the pixelization report) without a display. Skipped otherwise.
    #[test]
    fn paint_dumps_a_frame_for_visual_inspection() {
        let Ok(path) = std::env::var("RADAR_TERM_PNG") else {
            eprintln!("skipping: RADAR_TERM_PNG is not set");
            return;
        };

        let family = std::env::var("RADAR_TERM_FONT")
            .unwrap_or_else(|_| "JetBrainsMono Nerd Font".to_string());
        let size = std::env::var("RADAR_TERM_SIZE")
            .ok()
            .and_then(|size| size.parse::<f64>().ok())
            .unwrap_or(11.0);
        let mut inner = Inner {
            term: Terminal::new(80, 24),
            render: RenderState::new(),
            metrics: Metrics::new(&family, size, 1.0),
            base_family: family,
            base_size: size,
            scale: 1.0,
            last_grid: (0, 0),
        };

        // What an agent panel actually shows: TUI chrome, styled text, a
        // spinner line, dim markdown and a diff.
        inner.term.write(
            concat!(
            "\x1b[38;5;244m╭─ agent \x1b[38;5;39mopencode\x1b[38;5;244m ─────────────╮\x1b[0m\r\n",
            "\x1b[1m❯\x1b[0m fix the pixelized panel\r\n",
            "\x1b[2m  Reading src/gui/term.rs…\x1b[0m\r\n",
            "\x1b[1;32m●\x1b[0m Edited \x1b[36msrc/gui/term.rs\x1b[0m +18 -4\r\n",
            "\x1b[33m⠸\x1b[0m thinking…\r\n",
            "\x1b[31m- old cell width guess\x1b[0m\r\n",
            "\x1b[32m+ measured from the font\x1b[0m\r\n",
            "ascii band: |il1 oO08 .,;'\r\n",
        )
            .as_bytes(),
        );

        let width = 80 * inner.metrics.cell_w as i32;
        let height = 24 * inner.metrics.cell_h as i32;
        let mut surface =
            cairo::ImageSurface::create(cairo::Format::ARgb32, width, height).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        paint(&mut inner, &cr, width, height);
        drop(cr);

        // cairo ARGB32 is premultiplied B,G,R,A on little-endian.
        let bytes = gtk::glib::Bytes::from(&surface.data().unwrap().to_vec());
        let texture = gtk::gdk::MemoryTexture::new(
            width,
            height,
            gtk::gdk::MemoryFormat::B8g8r8a8Premultiplied,
            &bytes,
            width as usize * 4,
        );
        texture.save_to_png(std::path::Path::new(&path)).unwrap();
        eprintln!("wrote {path}");
    }
}
