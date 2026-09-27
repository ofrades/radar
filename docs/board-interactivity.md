# Board-first client/server direction

Implementation update: the daemon-owned registry and VTE attachment now keep
terminal processes alive across client exits; see
[`session-daemon.md`](session-daemon.md) for its tested protocol and limitations.
A durable project activity journal, bounded replay/watch protocol,
revision-checked attention store, stable card IDs, and CLI are also in place.
Lossless terminal-state import and the live board interaction flow remain follow-up.

The board is the project's full-workspace interaction surface. It presents
work, agent activity, and requests for human attention together. Opening a
session is a drill-down; returning to the board must preserve the tool layout
and leave sessions running. Notifications invite action without stealing focus.

## Current ownership and remaining gaps

The original phase-1 concerns about widget-owned process lifetime, blocking
renderer delivery, view-dependent lifecycle feedback, missing replay cursors,
and client-owned terminal query replies are addressed by the session daemon and
VTE attachment described in [`session-daemon.md`](session-daemon.md). Project
activity now has its own atomic snapshot/replay boundary, durable attention
records, and separate bounded watcher stream. Process lifecycle, explicit agent
activity, and each client connection remain distinct state axes.

The remaining gaps are exact terminal parser-state import, vendor-specific hooks
that automatically turn native agent prompts into activity requests,
project-wide board-file monitoring while the board pane is closed, and desktop
notifications. Agents can already submit requests and wait for typed responses
with `radar activity request --wait`. Do not infer agent intent from terminal
output, process existence, silence, or a bell.

## Ownership and state

- **Server:** session registry, lifecycle, agent adapter state, board mutations,
  activity journal, outstanding attention requests, command results.
- **Board UI:** presentation, sorting, filters, navigation, keyboard focus, and
  the human's path from a request to the relevant card/session.
- **BOARD.md:** source of truth for work cards. Do not append terminal output or
  high-frequency activity to it. Keep runtime events and attention records in
  server storage, linked to cards using stable identity. Titles and `@agent`
  alone are not durable identities; design migration before shipping links.

Keep three independent axes:

| Axis | States | Source |
| --- | --- | --- |
| Process lifecycle | starting, running, exited, failed-to-start | server process owner |
| Agent activity | unknown, working, waiting-for-input, waiting-for-approval, idle | explicit agent adapter events |
| Client connection | connected, reconnecting, disconnected | each client |

Unknown is a valid agent state. Do not infer “working” from output or “waiting”
from silence. A terminal bell is an advisory attention event, not an approval
request. Completing an agent turn does not automatically complete its work card.

## Feed and attention contract

Each domain event carries an event ID, monotonic project sequence, timestamp,
project ID, optional session/card IDs, kind, and typed payload. Terminal bytes
use a separate stream and cursor so a noisy terminal cannot delay attention.

The board contains:

- **Needs attention:** unresolved approvals, questions, failures, and review
  handoffs. Show reason, age, agent, card, and a direct action.
- **Activity feed:** ordered meaningful transitions, board moves, session starts
  and exits, request resolution, and command results. Coalesce noisy progress;
  raw terminal output belongs in the session view.
- **Work columns:** current cards, with linked session state where available.

Attention records have stable IDs, source-event IDs, reason, target, allowed
actions, and separate seen/acknowledged/resolved state. Reading or focusing a
pane must not resolve an approval. Responding sends a typed command; the server
validates that the request is still outstanding and publishes its result.
Duplicate delivery must not duplicate notifications or execute an action twice.
Badge counts reflect unresolved requests, including projects not currently open.
Desktop notifications link back to that same request and are a secondary channel.

## Fast feedback and reconnect

1. Client subscribes with its last project sequence. Server returns replay or an
   atomic snapshot with a watermark followed by events after that watermark.
   Expired cursors force an explicit resnapshot; gaps must never be silent.
2. Mutations carry command IDs and expected revisions. Show pending immediately;
   confirm on the authoritative result, or show the conflict/error with a retry.
   Never show a failed approval response as accepted.
3. Reconnect retains unresolved attention and client acknowledgement state.
   Mark stale data visibly while offline. Resuming must not re-notify old seen
   events or lose requests created during disconnection.
4. Aim for visible local feedback within 100 ms, and server-to-board transitions
   within 250 ms under normal load. Measure these with a flooding terminal and
   a stalled client, not just an idle happy path.

## Delivery and acceptance

**Implemented now:** board opens full workspace width and height, its columns
expand into available space, tool selection restores the tool arrangement, and
board grouping/split drops cannot shrink it into a tile. Board/Alt+K, its close
button, or Alt+F returns to tools. Existing grouped layouts are normalized when
rendered. Existing card editing, adding, and dragging remain available.
Claimed cards link to their agent: a card's `@claim` opens the matching agent
session — matched exactly by the process's own `RADAR_AGENT`, or by the
claim's leading program — and an exited agent re-opens resumed. When the
agent exits, radar captures the conversation its CLI recorded (opencode's
session list) and binds it to the claim in the database; the next click
reopens that exact conversation with `--session <id>`.

**Implemented:** server-owned registry and bounded terminal/feedback transport;
sequenced, persistent project activity; stable session/card identities;
idempotent commands and revision-checked persistent attention with independent
seen, acknowledged and resolved state. Tests cover restart, duplicate commands,
stale revisions, cursor resync, multiple watchers, terminal flooding, and an
agent CLI reporting with its radar-provided project/session environment.

**Implemented:** the board renders the live project feed and explicit states,
shows actionable unresolved requests with seen/acknowledge/answer/approve/deny/
dismiss actions, opens linked stable sessions, reflects authoritative resolution,
and shows unresolved counts for every sidebar project. GUI watching reconnects,
replays and deduplicates activity; file changes observed while the board pane is
open publish card-linked events.

**Next:** vendor-specific hooks can translate native agent prompts into the
existing request/wait flow; project-wide board-file monitoring while the pane is
closed and optional desktop delivery consume the same journal. Verify these with
real agent CLIs and no focus theft.

The journal, attention storage and live board presentation are implemented
independently of the fullscreen change; vendor-specific prompt hooks and desktop
notifications remain follow-up work.
