# Board — radar

This file is the project's kanban; radar renders it as a board.
Edit it directly:

- Claim a card: add your name to the end of its line, like `@you`
- Move work along: move the card's line under another column
- Add work: a new `- [ ]` line under any column
- Notes for a card: indent lines under it

## Backlog
- [ ] Complete terminal snapshot/import and GUI daemon attachment
      Required before calling the client/server split a complete Superlogical-style integration. The daemon's v1 snapshot is display-only: complete parser/UTF-8 state, alternate screen, saved cursor/charsets, margins, tab stops, title stack, palette and input modes must round-trip through a compatible renderer. Connect GUI session identity, startup, attach/reconnect, input/resize and query-reply suppression; remove widget-owned process lifetime. Test closing/reopening the GUI preserves the same live program and exact terminal state. See docs/session-daemon.md. Depends on server-owned session registry.

## In progress

## Review
- [ ] Server-owned session registry and nonblocking client delivery
      Claimed for the Superlogical-style client/server split: daemon owns PTYs, session lifetime, scrollback and authoritative parsed terminal state; clients attach with an atomic snapshot/watermark then consume the live stream, forward input and report resizes. Detach must leave programs running; explicit stop ends a session.
      Follow docs/board-interactivity.md: typed process lifecycle, bounded independent per-client delivery, explicit resync for lagging clients, and authoritative terminal query ownership. Keep domain/session feedback separate from terminal bytes so the board's future feed and attention requests remain responsive. Test detach/flood/stalled renderer/stop/reattach and snapshot-to-stream continuity. Builds on the phase-1 session core currently in Review.
      Implementation ready for handoff: GTK-free daemon/registry with stable IDs, nonblocking PTY I/O, bounded independent output/feedback subscriptions, snapshot watermark plus raw stream, paged scrollback, authoritative query replies, explicit stop/forget/shutdown, private versioned Unix socket protocol and CLI. Display snapshot is deliberately NOT a full parser image; GUI retains phase-1 bridge pending the new renderer integration card. docs/session-daemon.md documents the boundary and commands.
      Verification: cargo test --features vte (182 unit + 4 real-daemon integration tests passed); cargo clippy --all-targets --features vte clean. Independent standards/spec reviews found shutdown acknowledgement and EOF completion bugs; both fixed with regression tests. Known spec partial: GUI attachment/full terminal-state restoration is the explicit follow-up above. Files: src/session/{mod,registry,daemon}.rs, src/main.rs, tests/session_daemon.rs, docs/session-daemon.md, README.md.
      Commit includes the phase-1 session module, dependencies and probe needed by this backend. Isolated staged backend passes cargo check --all-targets. Isolated GUI build exposes an existing HEAD baseline issue (src/gui/mod.rs references uncommitted home.rs/NewWorkspaceLayout); the combined working tree passes the GUI-enabled suite above. Home-panel work remains owned by its existing card.

## Done
