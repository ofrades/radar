//! libghostty-vt mouse-event encoding: one wheel or button event becomes the
//! escape sequence the running program expects (SGR, X10, UTF-8, URxvt) — or
//! nothing, when the program never asked for mouse reports.
//!
//! The policy of *when* to forward a wheel event (alternate screen, mouse
//! tracking) lives with the view; this module is only the encoder, synced
//! from the terminal's mode state right before each event.

use std::ffi::c_void;

use super::Terminal;

/// `GHOSTTY_SUCCESS` — the only result that means the call worked.
const SUCCESS: i32 = 0;

/// Opaque `GhosttyMouseEncoder`.
type RawEncoder = *mut c_void;
/// Opaque `GhosttyMouseEvent`.
type RawEvent = *mut c_void;

/// `GHOSTTY_MOUSE_ENCODER_OPT_SIZE` (GhosttyMouseEncoderSize).
const OPT_SIZE: i32 = 2;

/// `GHOSTTY_MOUSE_ACTION_PRESS`.
const ACTION_PRESS: i32 = 0;
/// `GHOSTTY_MOUSE_BUTTON_FOUR`: the wheel's up tick.
const BUTTON_FOUR: i32 = 4;
/// `GHOSTTY_MOUSE_BUTTON_FIVE`: the wheel's down tick.
const BUTTON_FIVE: i32 = 5;

/// `GhosttyMousePosition`: a position in surface-space pixels.
#[repr(C)]
#[derive(Clone, Copy)]
struct Position {
    x: f32,
    y: f32,
}

/// `GhosttyMouseEncoderSize`: the geometry that maps surface pixels to cells.
#[repr(C)]
struct EncoderSize {
    size: usize,
    screen_width: u32,
    screen_height: u32,
    cell_width: u32,
    cell_height: u32,
    padding_top: u32,
    padding_bottom: u32,
    padding_right: u32,
    padding_left: u32,
}

extern "C" {
    fn ghostty_mouse_encoder_new(allocator: *const c_void, encoder: *mut RawEncoder) -> i32;
    fn ghostty_mouse_encoder_free(encoder: RawEncoder);
    fn ghostty_mouse_encoder_setopt(encoder: RawEncoder, option: i32, value: *const c_void);
    fn ghostty_mouse_encoder_setopt_from_terminal(
        encoder: RawEncoder,
        terminal: super::RawTerminal,
    );
    fn ghostty_mouse_encoder_encode(
        encoder: RawEncoder,
        event: RawEvent,
        out_buf: *mut u8,
        out_buf_size: usize,
        out_len: *mut usize,
    ) -> i32;
    fn ghostty_mouse_event_new(allocator: *const c_void, event: *mut RawEvent) -> i32;
    fn ghostty_mouse_event_free(event: RawEvent);
    fn ghostty_mouse_event_set_action(event: RawEvent, action: i32);
    fn ghostty_mouse_event_set_button(event: RawEvent, button: i32);
    fn ghostty_mouse_event_set_mods(event: RawEvent, mods: u16);
    fn ghostty_mouse_event_set_position(event: RawEvent, position: Position);
}

/// The mouse encoder, with one reusable event. A wheel tick becomes a
/// button-four (up) or button-five (down) press — the wheel is press-only,
/// like every terminal: a wheel has no release.
pub struct MouseEncoder {
    encoder: RawEncoder,
    event: RawEvent,
}

impl MouseEncoder {
    /// Create the encoder and its scratch event.
    pub fn new() -> Self {
        let mut encoder: RawEncoder = std::ptr::null_mut();
        let mut event: RawEvent = std::ptr::null_mut();
        let result = unsafe { ghostty_mouse_encoder_new(std::ptr::null(), &mut encoder) };
        assert_eq!(
            result, SUCCESS,
            "ghostty_mouse_encoder_new failed: {result}"
        );
        let result = unsafe { ghostty_mouse_event_new(std::ptr::null(), &mut event) };
        assert_eq!(result, SUCCESS, "ghostty_mouse_event_new failed: {result}");
        Self { encoder, event }
    }

    /// Sync the protocol state from the terminal — the tracking mode and
    /// output format a program sets with DECSET 1000/1002/1003/1006/… — and
    /// hand the encoder the geometry it needs to map `pos` pixels to a cell.
    /// Call right before encoding: the program can change modes at any write.
    pub fn sync(&mut self, term: &Terminal, screen_px: (u32, u32), cell_px: (u32, u32)) {
        unsafe {
            ghostty_mouse_encoder_setopt_from_terminal(self.encoder, term.raw());
        }
        let size = EncoderSize {
            size: std::mem::size_of::<EncoderSize>(),
            screen_width: screen_px.0.max(1),
            screen_height: screen_px.1.max(1),
            cell_width: cell_px.0.max(1),
            cell_height: cell_px.1.max(1),
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        };
        unsafe {
            ghostty_mouse_encoder_setopt(
                self.encoder,
                OPT_SIZE,
                &size as *const EncoderSize as *const c_void,
            );
        }
    }

    /// Encode one wheel tick as a press of button four (up) or five (down) at
    /// `pos`. Empty when the program has no mouse tracking on: the encoder
    /// emits nothing for a terminal that never asked, so the caller can fall
    /// back to its own scrollback.
    pub fn wheel(&mut self, up: bool, pos: (f32, f32)) -> Vec<u8> {
        unsafe {
            ghostty_mouse_event_set_action(self.event, ACTION_PRESS);
            ghostty_mouse_event_set_button(self.event, if up { BUTTON_FOUR } else { BUTTON_FIVE });
            ghostty_mouse_event_set_mods(self.event, 0);
            ghostty_mouse_event_set_position(self.event, Position { x: pos.0, y: pos.1 });
        }
        let mut buf = [0u8; 128];
        let mut written: usize = 0;
        let result = unsafe {
            ghostty_mouse_encoder_encode(
                self.encoder,
                self.event,
                buf.as_mut_ptr(),
                buf.len(),
                &mut written,
            )
        };
        if result != SUCCESS || written == 0 {
            return Vec::new();
        }
        buf[..written.min(buf.len())].to_vec()
    }
}

impl Default for MouseEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MouseEncoder {
    fn drop(&mut self) {
        unsafe {
            ghostty_mouse_event_free(self.event);
            ghostty_mouse_encoder_free(self.encoder);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A terminal with no mouse modes encodes nothing: forwarding a wheel
    /// event to a program that never asked would type garbage into its input.
    #[test]
    fn wheel_encodes_nothing_without_mouse_tracking() {
        let term = Terminal::new(10, 3);
        let mut encoder = MouseEncoder::new();
        encoder.sync(&term, (80, 24), (8, 16));
        assert!(
            encoder.wheel(true, (0.0, 0.0)).is_empty(),
            "no tracking: the wheel must not encode bytes"
        );
        assert!(
            encoder.wheel(false, (0.0, 0.0)).is_empty(),
            "no tracking: the wheel must not encode bytes"
        );
    }

    /// With normal tracking and SGR format the wheel is button four (up) and
    /// five (down) presses at the event's cell.
    #[test]
    fn wheel_encodes_sgr_button_presses_when_tracking() {
        let mut term = Terminal::new(10, 3);
        term.write(b"\x1b[?1000h\x1b[?1006h");
        let mut encoder = MouseEncoder::new();
        encoder.sync(&term, (80, 32), (8, 16));

        let up = encoder.wheel(true, (16.0, 16.0));
        assert_eq!(up, b"\x1b[<64;3;2M".to_vec(), "{up:?}");
        let down = encoder.wheel(false, (0.0, 0.0));
        assert_eq!(down, b"\x1b[<65;1;1M".to_vec(), "{down:?}");
    }

    /// X10 (normal tracking without SGR) falls back to the legacy byte form.
    #[test]
    fn wheel_encodes_the_legacy_byte_form_without_sgr() {
        let mut term = Terminal::new(10, 3);
        term.write(b"\x1b[?1000h");
        let mut encoder = MouseEncoder::new();
        encoder.sync(&term, (80, 32), (8, 16));
        let up = encoder.wheel(true, (8.0, 16.0));
        // X10: ESC [ M then button, cell coordinates, each offset by 32.
        // The wheel-up button is 64 (0x60); the position is cell 2,2 1-based.
        assert_eq!(up, vec![27, 91, 77, 96, 34, 34], "{up:?}");
    }
}
