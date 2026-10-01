# Session daemon: first client/server slice

## What runs today

`radar serve` runs a GTK-free, database-independent session daemon. It owns
the process registry, real PTYs, the Alacritty terminal parser, current screen,
10,000 lines of scrollback, and session lifecycle. Clients use a private Unix
socket under `<RADAR_HOME>/run/sessions.sock`.

This follows the Superlogical direction of long-lived server ownership and
snapshot-then-stream attachment. The GUI starts or connects to the daemon,
decodes the daemon's lossless libghostty-vt snapshot into its own engine, and
attaches each project/tab/program to the same persistent server session. Closing the pane or GUI detaches; the process remains in the
daemon. The daemon and CLI API can also be used independently.

## Agent conversations versus terminal sessions

The daemon shares a terminal process and its PTY stream. It does not make two
independently launched agent CLIs share a conversation. OpenCode has a
machine-readable session list, so Radar imports its project history and reopens
the exact provider session when selected. OMP, Pi, and Cursor support continuing
or resuming through their own CLI flags, but their history is not imported
without a supported machine-readable listing interface. A live external agent is
shown as an external-terminal row; selecting it focuses the terminal that owns it
rather than implying Radar has attached to its conversation or PTY.

If a later GUI attach finds the daemon socket missing or refused, the GUI starts
the daemon and retries that attach once. A daemon crash still loses its
in-memory sessions; restarting the daemon cannot restore processes it owned.

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

The server is foreground by design: the GUI autostarts it as a detached child,
or a supervisor/terminal can own it directly. Sessions survive **client** exits,
not daemon crashes or machine reboots.

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
- Listing captures retained session membership, then samples each session's
  status independently so a busy terminal cannot lock other sessions out of
  control commands. Concurrent forget/recreate operations can make a listed
  status stale; the list is not an atomic cross-session state snapshot.
- Fatal PTY I/O errors and expiry of the stop deadline are reported as
  `Failed`; a worker that could not confirm process exit never leaves its
  retained lifecycle marked `Running`.

### Environment

A spawned program is given a `PATH` that prepends the directories mise
reports (`mise bin-paths`) to the daemon's own `PATH`. The daemon is often
started by a desktop session or a systemd user unit whose `PATH` is not the
login shell's; the mise entries let an agent installed under the user's home
resolve, and let an omarchy wrapper started for it see the same tools. When
mise is absent the ambient `PATH` stands alone.

### Attach and ordering

The parser, display snapshot, and subscription registration share one lock.
The snapshot carries output watermark `N`; the first subsequent event is
`N+1`. Bytes are parsed before being published. Resize and stream closure use
the same sequence as bytes. Terminal bytes are unmodified.

The display snapshot carries the status (id, cwd, pid, lifecycle, title),
dimensions, the output watermark, and a lossless binary `terminal_snapshot` from
libghostty-vt. That snapshot is an ordered, CRC-protected record stream covering
the whole terminal — both screens, scrollback, tab stops, palette (including
overrides), scroll region, mode sets, cursor, title, and the unfinished VT/UTF-8
parser continuation at the cut. A client decodes it into its own libghostty-vt
terminal and feeds raw bytes after the snapshot watermark; it resumes exactly,
with no replay. See [libghostty-vt](libghostty-vt.md) for the pinned engine and
the snapshot contract.

### Backpressure and terminal queries

- Every output or feedback subscription has its own 32-frame bounded queue.
  Output chunks are at most 8 KiB. A slow subscriber is removed and receives
  `ResyncRequired`, or a socket disconnect if it cannot even read that marker.
- Feedback has an independent sequence/queue/socket. A terminal flood cannot
  consume its delivery budget. Project activity uses a separate durable journal
  and independent bounded watcher queues; terminal bytes never enter that feed.
- PTY reads and writes are nonblocking. User input is bounded and explicitly
  rejected when its queue is full. Input frames are at most 8 KiB. A control
  acknowledgement means **enqueued**, not that the program has consumed it.
- Dimensions are bounded to 2–500 columns and 1–300 rows. The last accepted
  resize wins; controlling-client arbitration is a later UI concern.
- Only the server answers terminal queries. DSR/DA come from the parser;
  headless colour queries use an xterm-style default palette and clipboard reads
  return empty. The daemon does not read or modify a desktop clipboard. The VTE
  attachment strips automatic query replies before forwarding user input.

### Wire protocol and recovery

Protocol v1 uses a four-byte big-endian byte length followed by UTF-8 JSON.
The initial request contains `version` and `command`. Each connection handles
one control request or becomes an `Attach`/`Watch` subscription. Separate
connections keep input, feedback and output independent.

Project activity has its own commands: `ActivitySnapshot`, `WatchActivity`,
`PublishActivity`, `CreateAttention`, and `ChangeAttention`. Events are stored
in `<RADAR_HOME>/run/activity.sqlite` and receive monotonically increasing
sequences per project. `WatchActivity(project_id, after_sequence)` atomically
returns the replay page and registers the live tail. A cursor more than 200
events behind receives `ResyncRequired`; clients take a recent bounded snapshot
and resume from its watermark. Watch queues hold 64 events; a slow watcher is
marked for resync without blocking journal writes or terminal transport.

### Session catalog

Beside the activity journal, the daemon keeps a durable session catalog at
`<RADAR_HOME>/run/catalog.sqlite`: one row per `(project, provider, provider
session id)` with `created_at`, `last_activity_at`, `ended_at`, lifecycle
(`running`/`ended`/`failed`), title, working directory, and an archive stamp.
It is the history the registry does not keep — sessions survive `forget` and
daemon restarts as catalog rows, without implying their processes survived.

- `Create` records a row for the session's stable id. Title changes seen by
  the parser bump the row's `last_activity_at`; when a session ends — or is
  missing from the registry at reconciliation time, e.g. after a daemon
  restart — the row ends. A running row is never a claim that the process is
  alive; it is corrected on the next list.
- `CatalogSeen` backfills a live session the daemon has no record for, and
  `CatalogBind` adopts the provider's own conversation id for a radar-spawned
  row (exit capture). Provider histories import as throttled upserts (60 s per
  project, currently OpenCode): the importer requests up to 10,000 rows,
  preserves the provider's `updated` timestamp as activity, and binds a
  conversation that matches a young running Radar row instead of duplicating
  it.
- Every catalog refresh archives ended or failed rows whose last activity is
  more than seven days old. Running rows are never auto-archived merely
  because their last title/activity update is old. `CatalogArchive(id,
  archived)` remains available for explicit presentation changes.
- Cursor Agent exposes `cursor-agent ls` as an interactive workspace chat
  picker, but the command has no documented machine-readable output or stable
  history-record schema (non-interactive use enters the TUI and cannot be
  imported safely). Radar therefore discovers Cursor while its process is
  running, but does not scrape private stores or parse the picker UI; durable
  old Cursor capture needs a provider-supported export/list contract.
- `CatalogList(projects, filter, query, limit)` returns rows newest-activity
  first; the client supplies the project roster. `CatalogArchive(id, archived)`
  is presentation state only — it hides a row from the active list and never
  deletes the conversation or another program's data.
- Catalog rows without a live radar session are history. The native sidebar
  reopens them exactly when the provider exposed a session id (resume
  template); otherwise the row says so instead of guessing "last".

Attention creation and its source event share one transaction. Requests survive
daemon restarts with stable request/source-event IDs, revision, reason, target,
allowed actions, and independent seen, acknowledged and resolved timestamps.
Mutations require the expected revision and a project-scoped command ID; retries
return the cached result instead of applying the action twice. Responses are
typed (`answer`, `approve`, `deny`, or `dismiss`) and must be allowed by the
request. The CLI surfaces these APIs through `radar activity`; radar-launched
programs receive `RADAR_PROJECT_ID`, `RADAR_SESSION_ID`, and `RADAR_HOME`.
Agents can use `radar activity request --wait` to publish a request and block for
the human response. The CLI watches from the request event sequence; if replay
requires a resnapshot, it reads the authoritative request record and resumes
from the fresh watermark, so a response cannot disappear during reconnect.
Board cards carry stable IDs in invisible `<!-- radar:card-id:… -->` comments;
JSON board output exposes those IDs for activity links.

The GUI subscribes to each sidebar project's activity stream independently of
terminal output. The board panel shows unresolved requests, explicit agent
states, and recent events; actions mark requests seen, acknowledge them, answer,
approve, deny, or dismiss through revision-checked commands. Project badges show
unresolved counts, feed entries and requests link to stable session IDs, and the
watcher replays/deduplicates after reconnect or replaces stale history after an
explicit resync. Board file changes observed while its pane is open also publish
card-linked activity events.

Vendor-specific agent hooks that automatically turn native prompts into these
requests, project-wide board-file monitoring while the board is closed, and
desktop notifications remain follow-up work.

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
idempotent session and activity commands, persistent attention, acknowledgements,
revision conflicts, activity replay/resync, protocol versions, gaps and truncated
frames.

`cargo test --test session_daemon` launches a real daemon process and checks
client-process exit/reconnect, ordered delivery, blocked socket isolation,
private endpoint permissions, singleton startup, invalid frame handling, and
(with `--features gui`) libghostty-vt snapshot attach, detach, and reattach
to the same process.

The libghostty-vt pin is pre-1.0: the C ABI and the snapshot format still
change. The pin and its bump policy live in
[`libghostty-vt.md`](libghostty-vt.md).
