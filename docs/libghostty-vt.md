# libghostty-vt: the shared terminal engine

Radar is adopting [libghostty-vt](https://libghostty.tip.ghostty.org/) — the
terminal state machine extracted from [Ghostty](https://ghostty.org) — as the
single terminal engine for the daemon and every client (GTK, web). This page
covers the pinned build behind the `ghostty` Cargo feature. The adoption itself
is tracked on the board; this feature only links the engine and proves the link.

## Why this engine

The daemon currently parses PTY output with `alacritty_terminal` and attaches
clients with a bounded ANSI replay. That replay cannot be exact: neither
`alacritty_terminal`'s `Term` nor VTE exposes its full parser state, so a client
cannot be handed the hidden state (inactive screen, tab stops, scroll region,
palette overrides, the unfinished escape/UTF-8 at the cut) or have it read back
and synthesised.

libghostty-vt ships the primitive that removes the boundary:

- `ghostty_snapshot_encode_alloc` and
  `ghostty_snapshot_decoder_{new_buf,ready,next,decode}` encode the complete
  terminal state — both screens, scrollback, tab stops, palette, scrolling
  region, all mode sets, cursor, title, and the unfinished VT/UTF-8 parser
  continuation at the cut — as one CRC-protected record stream that a fresh
  terminal decodes. No replay, and attach cost is independent of session age.
- A renderer-agnostic `RenderState` lets each client draw the decoded state,
  and key/mouse/focus/paste encoders keep input identical to the daemon.

A spike cut a terminal mid-UTF-8 (`E2 82` of `€`) and mid-CSI (`ESC[38;5`),
encoded a 1746-byte snapshot, decoded it into a fresh terminal, and fed the
continuation: the restored output was byte-identical to a reference that saw the
whole stream, including alternate-screen return, saved cursor, a custom tab
stop, scroll margins, a palette override, title, and bracketed-paste mode. The
same round trip works through Rust FFI, and a GTK4 `DrawingArea` renders the
restored `RenderState`. Evidence: `target/spike-libghostty/`.

## The pin

libghostty-vt is **pre-1.0**: both the C ABI and the snapshot format still
change, and the `ghostty` package on this machine ships 0.1.0, which has no
terminal or snapshot API at all. Radar therefore builds from a pinned revision
and moves the pin only on purpose:

| | |
|---|---|
| Ghostty commit | `4da7523faba68ccb4042ea20585817098a51c015` |
| Zig | `0.16.0` (exact; `build.zig.zon` compares major.minor for equality) |
| Snapshot version | v1 |

The commit is the one the fidelity spike validated. A bump means: update
`GHOSTTY_COMMIT` in `build.rs`, re-run the smoke test below **and** the
fidelity spike, and expect the snapshot bytes to change.

## Building

The engine is unconditional: `build.rs` fetches the pin (once per build tree),
runs `zig build` to produce a static `libghostty-vt.a` (and the browser's
`ghostty-vt.wasm`), and links it. Zig 0.16.0 is required:

```bash
mise install                              # fetches Zig 0.16.0 from .mise.toml
cargo test --test ghostty_snapshot
```

Environment knobs, for offline or packaged builds:

- `GHOSTTY_SOURCE_DIR` — build from an existing Ghostty checkout at the pin
  instead of fetching one.
- `ZIG` — path to the Zig binary (default: `zig` on `PATH`, resolved through
  version-manager shims).
- `LIBGHOSTTY_VT_OPTIMIZE` — Zig `OptimizeMode`; defaults to `ReleaseSmall`.

The build fixes `-Dcpu=baseline` so a distributed binary does not trap on an
older CPU than the build host.

## The smoke test

`tests/ghostty_snapshot.rs` proves the link and the property the adoption rests
on: a snapshot taken at a mid-escape, mid-UTF-8 cut restores a terminal that
continues the same stream identically (cursor and title match the source), the
restored terminal snapshots again, and the alternate screen's inactive primary
screen survives. Run it as above.

## Where this is going

`src/ghostty.rs` owns the `unsafe` FFI and a small safe wrapper (`Terminal`).
The remaining work, each on its own board ticket: the daemon parses with this
engine and encodes a snapshot under its existing lock/watermark; the GTK client
renders `RenderState` in place of the VTE replay; the web client decodes the
same snapshots via WASM; then `alacritty_terminal` and VTE are retired.
