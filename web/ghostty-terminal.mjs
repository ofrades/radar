// A browser terminal drawn from libghostty-vt RenderState (WASM).
//
// The daemon sends a lossless libghostty-vt snapshot on attach and the raw
// byte stream after it. This module decodes both into a real libghostty-vt
// terminal in the browser and paints its render state to a canvas, replacing
// the xterm.js ANSI-replay path.
//
// The API mirrors the slice of xterm.js that app.js uses: constructor,
// open(), fit(), write(), onData/onBinary/onResize, cols/rows, focus(),
// reset(), dispose().

// --- Render state data kinds (must match include/ghostty/vt/render.h) --------
const DATA_COLS = 1;
const DATA_ROWS = 2;
const DATA_ROW_ITERATOR = 4;
const DATA_CURSOR = 18;
const DATA_COLORS = 19;
// --- Terminal data kinds (must match include/ghostty/vt/terminal.h) ----------
const DATA_ACTIVE_SCREEN = 6;
const DATA_MOUSE_TRACKING = 11;
const DATA_MODE = 37;
const SCREEN_ALTERNATE = 1;
// DEC private modes the wheel policy reads.
const CURSOR_KEYS_MODE = 1; // DECCKM
const ALT_SCROLL_MODE = 1007; // wheel = cursor keys on the alternate screen
const ROW_DATA_CELLS = 3;
const ROW_DATA_VIEWPORT_Y = 6;
const CELLS_DATA_STYLE = 2;
const CELLS_DATA_GRAPHEMES_LEN = 3;
const CELLS_DATA_GRAPHEMES_BUF = 4;
const CELLS_DATA_BG_COLOR = 5;
const CELLS_DATA_FG_COLOR = 6;
const CELLS_DATA_SELECTED = 7;
// --- Mouse encoder values (must match include/ghostty/vt/mouse/) ------------
const MOUSE_ACTION_PRESS = 0;
const MOUSE_BUTTON_FOUR = 4; // wheel up
const MOUSE_BUTTON_FIVE = 5; // wheel down
const MOUSE_ENCODER_OPT_SIZE = 2;
const SUCCESS = 0;

// Struct sizes/offsets from the pinned ABI (repr(C)).
const COLORS_SIZE = 8 + 3 + 3 + 3 + 1 + 768;
const STYLE_SIZE = 72;
const CURSOR_SIZE = 24;
const SCROLL_DELTA = 2;
// GhosttyTerminalModeConfig { mode: u16, value: bool }.
const MODE_CONFIG_SIZE = 4;

let enginePromise = null;

/** Load (once) and return the libghostty-vt WASM exports. */
export function loadGhostty(url = "assets/ghostty-vt.wasm") {
  if (!enginePromise) {
    enginePromise = (async () => {
      const response = await fetch(url);
      if (!response.ok) throw new Error(`ghostty wasm: ${response.status}`);
      const bytes = await response.arrayBuffer();
      const { instance } = await WebAssembly.instantiate(bytes, {});
      return instance.exports;
    })();
  }
  return enginePromise;
}

class Engine {
  constructor(exports) {
    this.e = exports;
  }

  bytes() {
    return new Uint8Array(this.e.memory.buffer);
  }

  view() {
    return new DataView(this.e.memory.buffer);
  }

  allocOpaque() {
    const slot = this.e.ghostty_wasm_alloc_opaque();
    if (!slot) throw new Error("ghostty wasm: out of memory");
    return slot;
  }

  terminal(cols, rows) {
    const slot = this.allocOpaque();
    const result = this.e.ghostty_terminal_new(0, slot, cols, rows);
    if (result !== SUCCESS) throw new Error(`ghostty_terminal_new: ${result}`);
    return this.e.ghostty_wasm_take_opaque(slot);
  }

  decodeSnapshot(bytes) {
    const ptr = this.e.ghostty_wasm_alloc(bytes.length);
    this.bytes().set(bytes, ptr);
    const slot = this.allocOpaque();
    const created = this.e.ghostty_snapshot_decoder_new_buf(0, slot, ptr, bytes.length);
    if (created !== SUCCESS) throw new Error(`snapshot decoder: ${created}`);
    const decoder = this.e.ghostty_wasm_take_opaque(slot);
    const out = this.allocOpaque();
    const decoded = this.e.ghostty_snapshot_decoder_decode(decoder, out);
    this.e.ghostty_snapshot_decoder_free(decoder);
    this.e.ghostty_wasm_free(ptr, bytes.length);
    if (decoded !== SUCCESS) throw new Error(`snapshot decode: ${decoded}`);
    return this.e.ghostty_wasm_take_opaque(out);
  }

  renderState() {
    const slot = this.allocOpaque();
    const result = this.e.ghostty_render_state_new(0, slot);
    if (result !== SUCCESS) throw new Error(`render state: ${result}`);
    return this.e.ghostty_wasm_take_opaque(slot);
  }

  /**
   * A mouse encoder and its reusable event, for turning wheel ticks into the
   * wheel-button reports a program asked for.
   */
  mouseEncoder() {
    const slot = this.allocOpaque();
    const result = this.e.ghostty_mouse_encoder_new(0, slot);
    if (result !== SUCCESS) throw new Error(`mouse encoder: ${result}`);
    const encoder = this.e.ghostty_wasm_take_opaque(slot);
    const eventSlot = this.allocOpaque();
    const event = this.e.ghostty_mouse_event_new(0, eventSlot);
    if (event !== SUCCESS) throw new Error(`mouse event: ${event}`);
    return { encoder, event: this.e.ghostty_wasm_take_opaque(eventSlot) };
  }

  /**
   * An i32 terminal datum, or null when the engine has no value for it.
   */
  int32(terminal, data) {
    const ptr = this.e.ghostty_wasm_alloc(8);
    this.bytes().fill(0, ptr, ptr + 8);
    const ok = this.e.ghostty_terminal_get(terminal, data, ptr) === SUCCESS;
    const value = this.view().getInt32(ptr, true);
    this.e.ghostty_wasm_free(ptr, 8);
    return ok ? value : null;
  }

  /**
   * A boolean terminal datum, or null.
   */
  flag(terminal, data) {
    const ptr = this.e.ghostty_wasm_alloc(4);
    this.bytes().fill(0, ptr, ptr + 4);
    const ok = this.e.ghostty_terminal_get(terminal, data, ptr) === SUCCESS;
    const value = this.bytes()[ptr] !== 0;
    this.e.ghostty_wasm_free(ptr, 4);
    return ok ? value : null;
  }

  /**
   * A DEC private mode's value, or null. GhosttyTerminalModeConfig is
   * { mode: u16, value: bool } with the value at offset 2.
   */
  mode(terminal, number) {
    const ptr = this.e.ghostty_wasm_alloc(MODE_CONFIG_SIZE);
    this.view().setUint16(ptr, number, true);
    this.bytes()[ptr + 2] = 0;
    const ok = this.e.ghostty_terminal_get(terminal, DATA_MODE, ptr) === SUCCESS;
    const value = this.bytes()[ptr + 2] !== 0;
    this.e.ghostty_wasm_free(ptr, MODE_CONFIG_SIZE);
    return ok ? value : null;
  }

  /** Read one frame. `terminal` is the handle. */
  frame(state, terminal, palette) {
    if (this.e.ghostty_render_state_update(state, terminal) !== SUCCESS) {
      throw new Error("render state update failed");
    }
    const colsPtr = this.e.ghostty_wasm_alloc(2);
    const rowsPtr = this.e.ghostty_wasm_alloc(2);
    this.e.ghostty_render_state_get(state, DATA_COLS, colsPtr);
    this.e.ghostty_render_state_get(state, DATA_ROWS, rowsPtr);
    const cols = this.view().getUint16(colsPtr, true);
    const rows = this.view().getUint16(rowsPtr, true);
    this.e.ghostty_wasm_free(colsPtr, 2);
    this.e.ghostty_wasm_free(rowsPtr, 2);

    const colorsPtr = this.e.ghostty_wasm_alloc(COLORS_SIZE);
    this.view().setBigUint64(colorsPtr, BigInt(COLORS_SIZE), true);
    this.e.ghostty_render_state_get(state, DATA_COLORS, colorsPtr);
    const u8 = this.bytes();
    const background = [u8[colorsPtr + 8], u8[colorsPtr + 9], u8[colorsPtr + 10]];
    const foreground = [u8[colorsPtr + 11], u8[colorsPtr + 12], u8[colorsPtr + 13]];
    for (let i = 0; i < 256; i++) {
      const o = colorsPtr + 18 + i * 3;
      palette[i] = [u8[o], u8[o + 1], u8[o + 2]];
    }
    this.e.ghostty_wasm_free(colorsPtr, COLORS_SIZE);

    const cursorPtr = this.e.ghostty_wasm_alloc(CURSOR_SIZE);
    this.view().setBigUint64(cursorPtr, BigInt(CURSOR_SIZE), true);
    this.e.ghostty_render_state_get(state, DATA_CURSOR, cursorPtr);
    const cursor = {
      hasValue: u8[cursorPtr + 8] !== 0,
      x: this.view().getUint16(cursorPtr + 10, true),
      y: this.view().getUint16(cursorPtr + 12, true),
      visible: u8[cursorPtr + 15] !== 0,
    };
    this.e.ghostty_wasm_free(cursorPtr, CURSOR_SIZE);

    const itSlot = this.allocOpaque();
    this.e.ghostty_render_state_row_iterator_new(0, itSlot);
    this.e.ghostty_render_state_get(state, DATA_ROW_ITERATOR, itSlot);
    const iterator = this.view().getUint32(itSlot, true);
    const cellsSlot = this.allocOpaque();
    this.e.ghostty_render_state_row_cells_new(0, cellsSlot);

    const lines = [];
    while (this.e.ghostty_render_state_row_iterator_next(iterator)) {
      const yPtr = this.e.ghostty_wasm_alloc(4);
      this.e.ghostty_render_state_row_get(iterator, ROW_DATA_VIEWPORT_Y, yPtr);
      const y = this.view().getInt32(yPtr, true);
      this.e.ghostty_wasm_free(yPtr, 4);
      this.e.ghostty_render_state_row_get(iterator, ROW_DATA_CELLS, cellsSlot);
      const cells = this.view().getUint32(cellsSlot, true);
      const row = [];
      while (this.e.ghostty_render_state_row_cells_next(cells)) {
        const lenPtr = this.e.ghostty_wasm_alloc(4);
        this.e.ghostty_render_state_row_cells_get(cells, CELLS_DATA_GRAPHEMES_LEN, lenPtr);
        const len = this.view().getUint32(lenPtr, true);
        this.e.ghostty_wasm_free(lenPtr, 4);
        let text = "";
        if (len > 0) {
          const bufPtr = this.e.ghostty_wasm_alloc(len * 4);
          this.e.ghostty_render_state_row_cells_get(cells, CELLS_DATA_GRAPHEMES_BUF, bufPtr);
          const dv = this.view();
          for (let i = 0; i < len; i++) text += String.fromCodePoint(dv.getUint32(bufPtr + i * 4, true));
          this.e.ghostty_wasm_free(bufPtr, len * 4);
        }
        const stylePtr = this.e.ghostty_wasm_alloc(STYLE_SIZE);
        this.view().setBigUint64(stylePtr, BigInt(STYLE_SIZE), true);
        this.e.ghostty_render_state_row_cells_get(cells, CELLS_DATA_STYLE, stylePtr);
        const flags = this.bytes();
        const style = {
          bold: flags[stylePtr + 56] !== 0,
          italic: flags[stylePtr + 57] !== 0,
          faint: flags[stylePtr + 58] !== 0,
          inverse: flags[stylePtr + 60] !== 0,
          strikethrough: flags[stylePtr + 62] !== 0,
          underline: this.view().getInt32(stylePtr + 64, true) !== 0,
        };
        this.e.ghostty_wasm_free(stylePtr, STYLE_SIZE);
        row.push({
          text,
          fg: this.color(cells, CELLS_DATA_FG_COLOR),
          bg: this.color(cells, CELLS_DATA_BG_COLOR),
          selected: this.bool(cells, CELLS_DATA_SELECTED),
          ...style,
        });
      }
      this.e.ghostty_render_state_row_cells_free(cells);
      lines.push({ y, cells: row });
    }
    // The cells handle is per-row (block scope), so it is freed inside.
    this.e.ghostty_render_state_row_iterator_free(iterator);
    this.e.ghostty_render_state_clean(state);
    return { cols, rows, background, foreground, cursor, lines };
  }

  color(cells, kind) {
    const ptr = this.e.ghostty_wasm_alloc(3);
    const result = this.e.ghostty_render_state_row_cells_get(cells, kind, ptr);
    if (result !== SUCCESS) {
      this.e.ghostty_wasm_free(ptr, 3);
      return null;
    }
    const u8 = this.bytes();
    const color = [u8[ptr], u8[ptr + 1], u8[ptr + 2]];
    this.e.ghostty_wasm_free(ptr, 3);
    return color;
  }

  bool(cells, kind) {
    const ptr = this.e.ghostty_wasm_alloc(1);
    const result = this.e.ghostty_render_state_row_cells_get(cells, kind, ptr);
    const value = result === SUCCESS && this.bytes()[ptr] !== 0;
    this.e.ghostty_wasm_free(ptr, 1);
    return value;
  }
}

function rgb(color) {
  return color ? `rgb(${color[0]},${color[1]},${color[2]})` : null;
}

/** Encode a keydown as the bytes a program expects. */
function encodeKey(event) {
  const { key, ctrlKey, altKey } = event;
  const special = {
    Enter: "\r",
    Backspace: "\x7f",
    Tab: "\t",
    Escape: "\x1b",
    ArrowUp: "\x1b[A",
    ArrowDown: "\x1b[B",
    ArrowRight: "\x1b[C",
    ArrowLeft: "\x1b[D",
    Home: "\x1b[H",
    End: "\x1b[F",
    Insert: "\x1b[2~",
    Delete: "\x1b[3~",
    PageUp: "\x1b[5~",
    PageDown: "\x1b[6~",
    F1: "\x1bOP",
    F2: "\x1bOQ",
    F3: "\x1bOR",
    F4: "\x1bOS",
    F5: "\x1b[15~",
    F6: "\x1b[17~",
    F7: "\x1b[18~",
    F8: "\x1b[19~",
    F9: "\x1b[20~",
    F10: "\x1b[21~",
    F11: "\x1b[23~",
    F12: "\x1b[24~",
  };
  if (special[key]) return altKey ? `\x1b${special[key]}` : special[key];
  if (ctrlKey && key.length === 1) {
    const lower = key.toLowerCase();
    if (lower >= "a" && lower <= "z") return String.fromCharCode(lower.charCodeAt(0) - 96);
    const control = { " ": "\x00", "@": "\x00", "[": "\x1b", "\\": "\x1c", "]": "\x1d", "^": "\x1e", "_": "\x1f" };
    if (control[key]) return control[key];
    return null;
  }
  if (key.length === 1) return altKey ? `\x1b${key}` : key;
  return null;
}

/** A canvas terminal over a libghostty-vt terminal. */
export class GhosttyTerminal {
  constructor(options = {}) {
    this.options = options;
    this.fontFamily = options.fontFamily || "ui-monospace, monospace";
    this.fontSize = options.fontSize || 14;
    this.cols = 80;
    this.rows = 24;
    this.cellWidth = this.fontSize * 0.6;
    this.cellHeight = Math.round(this.fontSize * 1.25);
    this.palette = new Array(256).fill([0, 0, 0]);
    this.dataListeners = [];
    this.resizeListeners = [];
    this.binaryListeners = [];
    this.pending = [];
    this.pendingWheel = 0;
    this.disposed = false;
    this.renderScheduled = false;
    this.ready = (async () => {
      this.engine = new Engine(await loadGhostty());
      this.terminal = this.engine.terminal(this.cols, this.rows);
      this.state = this.engine.renderState();
      this.mouse = this.engine.mouseEncoder();
      for (const bytes of this.pending) this.write(bytes);
      this.pending = [];
      this.render();
    })().catch((error) => {
      console.error("ghostty terminal unavailable:", error);
      throw error;
    });
  }

  open(element) {
    this.element = element;
    element.innerHTML = "";
    const canvas = document.createElement("canvas");
    canvas.className = "ghostty-canvas";
    canvas.tabIndex = 0;
    canvas.style.width = "100%";
    canvas.style.height = "100%";
    canvas.style.outline = "none";
    element.appendChild(canvas);
    this.canvas = canvas;
    this.context = canvas.getContext("2d");
    this.measure();
    canvas.addEventListener("keydown", (event) => {
      // Shift+PageUp/Down scroll the scrollback, not the program.
      if (event.shiftKey && (event.key === "PageUp" || event.key === "PageDown")) {
        event.preventDefault();
        this.scroll(event.key === "PageUp" ? -this.rows : this.rows);
        return;
      }
      const bytes = encodeKey(event);
      if (bytes === null) return;
      event.preventDefault();
      this.scrollBottom();
      for (const listener of this.dataListeners) listener(bytes);
    });
    canvas.addEventListener(
      "wheel",
      (event) => {
        event.preventDefault();
        this.wheel(event.deltaY, event.deltaMode);
      },
      { passive: false },
    );
    this.fit();
  }

  measure() {
    if (!this.context) return;
    this.context.font = `${this.fontSize}px ${this.fontFamily}`;
    this.cellWidth = Math.max(1, this.context.measureText("M").width);
    this.cellHeight = Math.round(this.fontSize * 1.25);
  }

  fit() {
    if (!this.element || !this.canvas) return;
    const width = this.element.clientWidth || this.canvas.clientWidth;
    const height = this.element.clientHeight || this.canvas.clientHeight;
    if (!width || !height) return;
    const cols = Math.max(1, Math.floor(width / this.cellWidth));
    const rows = Math.max(1, Math.floor(height / this.cellHeight));
    const dpr = window.devicePixelRatio || 1;
    this.canvas.width = Math.floor(width * dpr);
    this.canvas.height = Math.floor(height * dpr);
    this.context.setTransform(dpr, 0, 0, dpr, 0, 0);
    if (cols !== this.cols || rows !== this.rows) {
      this.cols = cols;
      this.rows = rows;
      this.ready.then(() => {
        this.engine.e.ghostty_terminal_resize(this.terminal, cols, rows, 0, 0);
        for (const listener of this.resizeListeners) listener({ cols, rows });
        this.render();
      });
    } else {
      this.render();
    }
  }

  write(bytes) {
    if (!this.terminal) {
      this.pending.push(bytes);
      return;
    }
    const ptr = this.engine.e.ghostty_wasm_alloc(bytes.length);
    this.engine.bytes().set(bytes, ptr);
    this.engine.e.ghostty_terminal_vt_write(this.terminal, ptr, bytes.length);
    this.engine.e.ghostty_wasm_free(ptr, bytes.length);
    this.scheduleRender();
  }

  loadSnapshot(bytes) {
    this.ready.then(() => {
      this.terminal = this.engine.decodeSnapshot(bytes);
      this.scheduleRender();
    });
  }

  scheduleRender() {
    if (this.renderScheduled || this.disposed) return;
    this.renderScheduled = true;
    requestAnimationFrame(() => {
      this.renderScheduled = false;
      this.render();
    });
  }

  render() {
    if (!this.context || !this.state || this.disposed) return;
    const frame = this.engine.frame(this.state, this.terminal, this.palette);
    const ctx = this.context;
    ctx.fillStyle = rgb(frame.background);
    ctx.fillRect(0, 0, this.canvas.width, this.canvas.height);
    ctx.textBaseline = "top";
    for (const line of frame.lines) {
      if (line.y < 0) continue;
      const y = line.y * this.cellHeight;
      for (let column = 0; column < line.cells.length; column++) {
        const cell = line.cells[column];
        const x = column * this.cellWidth;
        let fg = cell.fg || frame.foreground;
        let bg = cell.bg || frame.background;
        if (cell.inverse) [fg, bg] = [bg, fg];
        if (cell.selected) {
          ctx.fillStyle = `rgba(${fg[0]},${fg[1]},${fg[2]},0.3)`;
          ctx.fillRect(x, y, this.cellWidth, this.cellHeight);
        } else if (cell.bg) {
          ctx.fillStyle = rgb(bg);
          ctx.fillRect(x, y, this.cellWidth, this.cellHeight);
        }
        if (cell.text) {
          ctx.font = `${cell.italic ? "italic " : ""}${cell.bold ? "bold " : ""}${this.fontSize}px ${this.fontFamily}`;
          ctx.fillStyle = rgb(cell.faint ? fg.map((c) => Math.floor(c / 2)) : fg);
          ctx.fillText(cell.text, x, y + (this.cellHeight - this.fontSize) / 2);
        }
        if (cell.underline || cell.strikethrough) {
          ctx.strokeStyle = rgb(fg);
          ctx.lineWidth = 1;
          const lineY = cell.underline ? y + this.cellHeight - 2 : y + this.cellHeight / 2;
          ctx.beginPath();
          ctx.moveTo(x, lineY);
          ctx.lineTo(x + this.cellWidth, lineY);
          ctx.stroke();
        }
      }
    }
    if (frame.cursor?.hasValue && frame.cursor.visible) {
      ctx.strokeStyle = rgb(frame.foreground);
      ctx.lineWidth = 2;
      ctx.strokeRect(
        frame.cursor.x * this.cellWidth + 1,
        frame.cursor.y * this.cellHeight + 1,
        this.cellWidth - 2,
        this.cellHeight - 2,
      );
    }
  }

  scroll(delta) {
    this.ready.then(() => {
      // The struct is passed by value; in wasm that is an indirect pointer.
      const ptr = this.engine.e.ghostty_wasm_alloc(24);
      this.engine.view().setInt32(ptr, SCROLL_DELTA, true);
      this.engine.view().setBigInt64(ptr + 8, BigInt(delta), true);
      this.engine.e.ghostty_terminal_scroll_viewport(this.terminal, ptr);
      this.engine.e.ghostty_wasm_free(ptr, 24);
      this.scheduleRender();
    });
  }

  /**
   * One wheel event. Deltas arrive in three units (pixel, line, page); all
   * become rows and queue through the shared accumulator, so fractional
   * trackpad deltas add up to whole rows.
   */
  wheel(deltaY, deltaMode) {
    if (deltaMode === 1) {
      // Lines.
      this.pendingWheel += deltaY;
    } else if (deltaMode === 2) {
      // Pages.
      this.pendingWheel += deltaY * this.rows;
    } else {
      // Pixels: a row per cell height of travel.
      this.pendingWheel += deltaY / this.cellHeight;
    }
    const rows = Math.trunc(this.pendingWheel);
    this.pendingWheel -= rows;
    if (rows !== 0) this.wheelRows(rows);
  }

  /**
   * A wheel gesture of `rows` rows (negative scrolls up into history).
   *
   * A TUI on the alternate screen has no scrollback to move, so the wheel is
   * program input — upstream Ghostty's Surface.scrollCallback policy: with
   * alt-scroll (DECSET 1007) and no mouse reporting it becomes cursor keys in
   * the mode DECCKM chose; with mouse reporting it becomes an encoded wheel
   * report and the program scrolls itself. Otherwise the primary screen's
   * scrollback scrolls, as on any terminal.
   */
  wheelRows(rows) {
    this.ready.then(() => {
      const engine = this.engine;
      const alt = engine.int32(this.terminal, DATA_ACTIVE_SCREEN) === SCREEN_ALTERNATE;
      const tracking = engine.flag(this.terminal, DATA_MOUSE_TRACKING);
      if (alt && tracking === false && engine.mode(this.terminal, ALT_SCROLL_MODE) === true) {
        const sequence = engine.mode(this.terminal, CURSOR_KEYS_MODE)
          ? (rows < 0 ? "\x1bOA" : "\x1bOB")
          : (rows < 0 ? "\x1b[A" : "\x1b[B");
        for (let i = 0; i < Math.abs(rows); i++) this.emit(sequence);
        return;
      }
      if (tracking === true) {
        // One report per row: a program scrolls per event received.
        for (let i = 0; i < Math.abs(rows); i++) {
          const report = this.wheelReport(rows < 0);
          if (!report) break;
          this.emit(report);
        }
        return;
      }
      this.scroll(rows);
    });
  }

  /**
   * Encode one wheel tick as a wheel-button press. Empty (null) when the
   * program has no mouse reporting on — the encoder emits nothing for a
   * terminal that never asked.
   *
   * The event position stays at the default (cell 1,1): the wasm ABI hands
   * the position struct to set_position indirectly and the JS side cannot
   * reproduce that pointer, and wheel handlers key off the button, not the
   * cell. The encoder's protocol state re-syncs from the terminal on every
   * call, so mode changes are picked up immediately.
   */
  wheelReport(up) {
    const e = this.engine.e;
    e.ghostty_mouse_encoder_setopt_from_terminal(this.mouse.encoder, this.terminal);
    e.ghostty_mouse_event_set_action(this.mouse.event, MOUSE_ACTION_PRESS);
    e.ghostty_mouse_event_set_button(this.mouse.event, up ? MOUSE_BUTTON_FOUR : MOUSE_BUTTON_FIVE);
    const out = e.ghostty_wasm_alloc(64);
    const lenPtr = e.ghostty_wasm_alloc(4);
    const result = e.ghostty_mouse_encoder_encode(this.mouse.encoder, this.mouse.event, out, 64, lenPtr);
    const len = this.engine.view().getUint32(lenPtr, true);
    const bytes =
      result === SUCCESS && len > 0 ? this.engine.bytes().slice(out, out + len) : null;
    e.ghostty_wasm_free(out, 64);
    e.ghostty_wasm_free(lenPtr, 4);
    return bytes ? String.fromCharCode(...bytes) : null;
  }

  /** Hand program input to the data listeners (a string of char codes). */
  emit(text) {
    for (const listener of this.dataListeners) listener(text);
  }

  scrollBottom() {
    this.ready.then(() => {
      const ptr = this.engine.e.ghostty_wasm_alloc(24);
      this.engine.view().setInt32(ptr, 1, true); // SCROLL_VIEWPORT_BOTTOM
      this.engine.e.ghostty_terminal_scroll_viewport(this.terminal, ptr);
      this.engine.e.ghostty_wasm_free(ptr, 24);
      this.scheduleRender();
    });
  }

  onData(listener) {
    this.dataListeners.push(listener);
    return { dispose: () => {} };
  }

  onBinary(listener) {
    this.binaryListeners.push(listener);
    return { dispose: () => {} };
  }

  onResize(listener) {
    this.resizeListeners.push(listener);
    return { dispose: () => {} };
  }

  loadAddon() {}

  focus() {
    this.canvas?.focus();
  }

  reset() {
    this.ready.then(() => {
      this.terminal = this.engine.terminal(this.cols, this.rows);
      this.scheduleRender();
    });
  }

  dispose() {
    this.disposed = true;
    if (this.mouse) {
      this.engine.e.ghostty_mouse_event_free(this.mouse.event);
      this.engine.e.ghostty_mouse_encoder_free(this.mouse.encoder);
      this.mouse = null;
    }
    this.element && (this.element.innerHTML = "");
  }
}
