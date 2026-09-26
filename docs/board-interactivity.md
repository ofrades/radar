# Board-first client/server direction

The board is the project's full-workspace interaction surface. It presents
work, agent activity, and requests for human attention together. Opening a
session is a drill-down; returning to the board must preserve the tool layout
and leave sessions running. Notifications invite action without stealing focus.

## Review of the current split

The GTK-free PTY owner and parsed terminal state in `src/session/mod.rs` are
a useful transport foundation. They are not yet an independently running server.
Before calling phase 2 a detachable client/server implementation, address:

1. **Lifetime still belongs to a widget.** `Pane::session` owns `Session`, and
   dropping `Session` stops the child. Move ownership to a server session registry;
   attach/detach must only change subscriptions. Explicit stop ends a process.
2. **A renderer can stall the session.** `bridge_loop` writes synchronously to
   the widget PTY. A client that stops reading can block parsing, input, exit
   detection, and shutdown. Use bounded independent client delivery, disconnect
   lagging clients with an explicit resync requirement, and keep server event
   processing independent of terminal delivery.
3. **Feedback is view-dependent.** The GUI consumes only `Exit`; title and bell
   come from VTE. Use server events for feedback even without attached clients.
   The current GUI now queues only exit events rather than copying unused output
   into an unbounded GUI queue.
4. **Process existence is not agent state.** `Pane::is_live` checks whether a
   session was spawned, not whether it exited or needs input. A terminal title,
   bell, silence, or nonzero exit cannot reliably describe agent intent.
5. **There is no reconnect boundary.** Callback events lack session identity,
   sequence numbers, replay, or acknowledgement. A screen mutex is not an atomic
   snapshot-and-subscribe protocol. Process exit can also precede final PTY
   output; distinguish process exit from stream completion.
6. **Terminal replies still require a client.** DSR/DA/colour queries are ignored
   by the session parser. Assign one authoritative reply owner before supporting
   zero or multiple attached clients; never let each viewer reply independently.

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

**Next, before more transport-only work:**

1. Server registry, typed lifecycle, bounded client transport, query ownership.
   Verify detach with output flooding, blocked client, stop, exit, and reattach.
2. Sequenced project journal, atomic snapshot/replay, idempotent commands and
   persistent attention. Verify duplicates, stale revisions, cursor gaps,
   restart, two clients, and events produced with no client attached.
3. Agent adapter reports a real question/approval; board renders the request,
   opens the linked session, accepts a response, and reflects server resolution.
   Verify end-to-end latency under terminal load and no focus theft.
4. Board feed, project attention badges, and optional desktop delivery consume
   the same records. Verify cross-project attention and reconnect deduplication.

The feed, agent states, and persistent notifications described here are the
next implementation contracts; they are not implemented by the fullscreen change.
