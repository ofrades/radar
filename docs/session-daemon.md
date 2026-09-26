# Session daemon: first client/server slice

## What runs today

`radar serve` runs a GTK-free, database-independent session daemon. It owns
the process registry, real PTYs, the Alacritty terminal parser, current screen,
10,000 lines of scrollback, and session lifecycle. Clients use a private Unix
socket under `<RADAR_HOME>/run/sessions.sock`.

This follows the Superlogical direction of long-lived server ownership and
snapshot-then-stream attachment. It is **not yet Superlogical's full terminal
replication**: the v1 snapshot is a display snapshot, not an importable image of
the terminal state machine. The existing GUI still uses the phase-1 local VTE
bridge. The daemon and CLI API can be used and tested independently now.

### Try it

Build with `cargo build` (no GUI feature required). In one terminal:

```sh
radar --home /tmp/opencode/radar-server-demo serve
```

In another:

```sh
radar --home /tmp/opencode/radar-server-demo session spawn demo --cwd "$PWD" -- /bin/sh
radar --home /tmp/opencode/radar-server-demo session list
radar --home /tmp/opencode/radar-server-demo session input demo $'printf "hello\\n"\n'
radar --home /tmp/opencode/radar-server-demo session snapshot demo
radar --home /tmp/opencode/radar-server-demo session watch demo
```

`snapshot` prints one JSON display snapshot and disconnects. `stream` prints
that snapshot followed by sequenced raw-output/resize/EOF frames as JSON.
Neither command is an interactive terminal renderer. Closing these commands
leaves the shell running. `watch` has its own socket and publishes title, bell,
process lifecycle, and stream closure independently of terminal output.

```sh
radar --home /tmp/opencode/radar-server-demo session resize demo 120 40
radar --home /tmp/opencode/radar-server-demo session stop demo
# After the worker has finished:
radar --home /tmp/opencode/radar-server-demo session forget demo
radar --home /tmp/opencode/radar-server-demo session shutdown
```

The server is foreground by design: a supervisor or your terminal owns the
daemon process. GUI autostart/reconnection is part of the renderer integration.
Sessions survive **client** exits, not daemon crashes or machine reboots.

## Contracts

### Ownership and identity

- A `Registry` retains each `ManagedSession` until explicit `forget`, after its
  worker has ended. Dropping a socket/subscription never stops the process.
- `Create` uses a caller-provided stable ID. Retrying the same command,
  environment and canonical working directory returns the same session, even
  after exit. A conflicting request fails; there is no implicit restart.
- `Stop` signals the terminal foreground group and the direct process group,
  escalates to SIGKILL after 500 ms, and never waits for a renderer to drain.
- Process exit and PTY stream closure are separate events. Final output can
  arrive after the direct child exits, including output from descendants.
- Spawn errors are command errors; no fictitious running session is registered.
  Runtime failures have a typed `Failed` lifecycle. Agent intent remains unknown:
  a bell or an exit is not a question, approval, or completed board card.
- Shutdown rejects further creation and stops all sessions. A daemon holds at
  most 128 retained sessions; `forget` explicitly frees history and the ID.

### Attach and ordering

The parser, display snapshot, and subscription registration share one lock.
The snapshot carries output watermark `N`; the first subsequent event is
`N+1`. Bytes are parsed before being published. Resize and stream closure use
the same sequence as bytes. Terminal bytes are unmodified.

The display snapshot includes the visible grid with cell attributes, explicit
cursor coordinates (Alacritty's grid serde skips its cursor), terminal mode
bits, dimensions, title/lifecycle, and available history length. Scrollback is
paged separately with `History(id, sequence, offset, limit)`, newest row first,
at most 200 rows per page. A stale sequence is rejected rather than mixing
different terminal revisions. Immutable history backfill under continuous
output is a future improvement.

**Renderer integration gate:** this snapshot does not include an in-progress
escape sequence/UTF-8 decoder, inactive screen, saved cursor/charset state,
scroll margins, tab stops, title stack, or full terminal mode configuration.
Feeding its cells to VTE and then forwarding a raw suffix is not lossless.
Before switching GUI ownership, implement a complete snapshot/import contract
using compatible terminal engines, or an explicitly separate compatibility
renderer protocol. Test split escape/UTF-8 sequences, alternate-screen return,
saved cursor, margins, tabs, wide/combining characters, palette and input modes.
The current output watermark guarantees delivery order; it does not claim to
solve cross-engine state restoration.

### Backpressure and terminal queries

- Every output or feedback subscription has its own 32-frame bounded queue.
  Output chunks are at most 8 KiB. A slow subscriber is removed and receives
  `ResyncRequired`, or a socket disconnect if it cannot even read that marker.
- Feedback has an independent sequence/queue/socket. A terminal flood cannot
  consume its delivery budget. These are live events, not yet a durable project
  activity journal or persistent attention records.
- PTY reads and writes are nonblocking. User input is bounded and explicitly
  rejected when its queue is full. Input frames are at most 8 KiB. A control
  acknowledgement means **enqueued**, not that the program has consumed it.
- Dimensions are bounded to 2–500 columns and 1–300 rows. The last accepted
  resize wins; controlling-client arbitration is a later UI concern.
- Only the server answers terminal queries. DSR/DA come from the parser;
  headless colour queries use an xterm-style default palette and clipboard reads
  return empty. The daemon does not read or modify a desktop clipboard. A future
  client must suppress its own automatic query replies.

### Wire protocol and recovery

Protocol v1 uses a four-byte big-endian byte length followed by UTF-8 JSON.
The initial request contains `version` and `command`. Each connection handles
one control request or becomes an `Attach`/`Watch` subscription. Separate
connections keep input, feedback and output independent.

Request frames are capped at 128 KiB; responses at 128 MiB (large display
snapshots/history pages). The socket has mode 0600 in a 0700 directory. A held
file lock serializes startup and stale socket removal. A second daemon cannot
unlink the first daemon's endpoint. Unknown versions fail before command
execution; clients never replace an incompatible running daemon automatically.

The supplied `Client` validates stream sequence numbers. A gap, truncated frame,
read failure, or resync marker invalidates the connection: obtain a new
attachment. Reusing a half-read frame after a timeout would corrupt framing.
Control reads/writes time out after two seconds; streaming clients may disable
their read timeout after the initial snapshot/status. Server socket writes also
time out, bounding the resources held by a stalled peer. At most 128 connections
are serviced concurrently.

## Verification

`cargo test session::` covers registry ownership, exact snapshot-to-event
sequencing, flood/resync, independent feedback, headless queries, resize,
bounded stop, final descendant output, history revisions, input overload,
idempotent creation, protocol versions, gaps and truncated frames.

`cargo test --test session_daemon` launches a real daemon process and checks
client-process exit/reconnect, ordered delivery, blocked socket isolation,
private endpoint permissions, singleton startup and invalid frame handling.

Next delivery: complete renderer snapshot/import and GUI attach; then the
project activity journal and board attention features in
[`board-interactivity.md`](board-interactivity.md).
