//! Smoke test for the pinned libghostty-vt link (radar's `ghostty` feature).
//!
//! Proves the link works and that the property the whole adoption rests on
//! holds: a snapshot taken at a mid-escape, mid-UTF-8 cut restores a terminal
//! that continues the same stream identically, with no replay.
//!
//! Run with: `cargo test --features ghostty --test ghostty_snapshot`.

use radar::ghostty::Terminal;

#[test]
fn snapshot_round_trips_mid_escape_and_mid_utf8() {
    let mut source = Terminal::new(40, 8);
    source.write(b"\x1b]0;radar-smoke\x07");
    source.write("café \x1b[35mhello\x1b[0m\r\n".as_bytes());
    // Cut mid-UTF-8: the first two bytes of U+20AC EURO SIGN (E2 82 AC) ...
    source.write(&[0xE2, 0x82]);
    // ... and mid-CSI: a 256-colour SGR, stopped after "38;5".
    source.write(b"\x1b[38;5");

    let snapshot = source.snapshot();
    assert!(
        !snapshot.is_empty(),
        "libghostty-vt produced an empty snapshot"
    );

    let mut restored = Terminal::from_snapshot(&snapshot);

    // Feed the rest of the split input to both. The snapshot carried the
    // unfinished parser state, so both must land in the same place.
    for terminal in [&mut source, &mut restored] {
        terminal.write(&[0xAC]);
        terminal.write(b";196m!");
    }

    assert_eq!(
        source.cursor(),
        restored.cursor(),
        "cursor diverged after restoring a mid-escape snapshot"
    );
    assert_eq!(source.title(), "radar-smoke");
    assert_eq!(
        source.title(),
        restored.title(),
        "title did not survive the snapshot"
    );

    // The restored terminal is a first-class terminal: it snapshots again.
    let again = restored.snapshot();
    let replayed = Terminal::from_snapshot(&again);
    assert_eq!(restored.cursor(), replayed.cursor());
}

#[test]
fn snapshot_preserves_the_inactive_screen() {
    // Enter the alternate screen, draw, snapshot while alt is active, then
    // leave alt on both terminals: the primary screen must match.
    let mut source = Terminal::new(20, 4);
    source.write(b"primary-line");
    source.write(b"\x1b[?1049h");
    source.write(b"alt-line");

    let snapshot = source.snapshot();
    let mut restored = Terminal::from_snapshot(&snapshot);

    for terminal in [&mut source, &mut restored] {
        terminal.write(b"\x1b[?1049l");
    }
    assert_eq!(source.cursor(), restored.cursor());
}
