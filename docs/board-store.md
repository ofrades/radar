# The board store: cards in radar's control

Implementation update: the store is live. The daemon owns lanes and cards
(`board.sqlite`); `radar card …`, the GUI (Home's board, the card view and the
workspace Board pane) and the web client all read and write it; a legacy
`BOARD.md` is imported once and then left untouched. Git/sync remains out of
scope. See [`board-interactivity.md`](board-interactivity.md) and
[`session-daemon.md`](session-daemon.md) for the surrounding architecture.

## Problem Statement

Today a project's board is a markdown file, `BOARD.md`, in the project root.
Radar treats the file as the source of truth, and every part of the board is a
consequence of parsing and rewriting it:

- **Card identity is a hack.** `ensure_card_ids` rewrites the file to stamp
  invisible `<!-- radar:card-id:… -->` comments, because markdown has no ids.
- **Structure is heuristic.** A `- ` line is a card at column 0 but a note when
  indented; "done" is one column's heading *and* a checkbox, and the two can
  disagree; columns are names, not states.
- **Edits are whole-file rewrites** with mtime-retry, so concurrent agents race
  and every change risks reformatting an agent-authored file.
- **Fields are fixed** to title, claim, body and done; there is no room for
  order, status kinds, assignees, or per-card metadata.
- **The UI parses too.** Home and the board pane re-read and re-parse the file,
  and a separate daemon monitor watches it to journal transitions.

Meanwhile the rest of the board's state already lives in the daemon's SQLite:
the activity thread, human attention, agent-session bindings, and project
settings. Only the cards are still a file, and every "board fix" so far has
been a workaround for that.

The user does not want their board stored in the repository, and does not need
git to be the database. They want a local board radar controls.

## Solution

Make the daemon's store the source of truth for cards, exactly where the thread
already lives. A project's board becomes rows — lanes and cards — owned by the
server, with the CLI, GUI and web client as views. The `radar card …` surface
keeps working for agents and scripts; it writes the store instead of a file.
`BOARD.md` stops being read; existing boards are imported once, and the file is
left untouched. A generated export (and, later, an optional git-backed board)
is explicitly out of scope here.

## User Stories

1. As a radar user, I want my board stored by radar, so that it is not
   committed to my repository.
2. As a radar user, I want cards to have real ids and a real status, so that
   moves, claims and edits are exact rather than parsed.
3. As a radar user, I want to reorder cards within a lane and move them between
   lanes, so that the board reflects how I actually think about the work.
4. As a radar user, I want lanes I can name, so that my workflow is not one of
   four fixed headings.
5. As a radar user, I want the Done lane to be the single definition of done,
   so that a card is never both "done" and "not done".
6. As a radar user, I want a card's body to be markdown and its thread to be
   the conversation I already have, so that reading a card is pleasant.
7. As an agent, I want `radar card next / claim / move / done` to work exactly
   as before, so that the convention does not change.
8. As an agent, I want `radar card show` and `radar card comment` to read and
   write the thread, so that I can report and ask without a file.
9. As an agent, I want a claim to survive concurrent work by other agents, so
   that two of us never hold the same card.
10. As a radar user, I want my existing `BOARD.md` imported once, so that no
    work is lost when the store takes over.
11. As a radar user, I want the pre-commit guard and the board skill to keep
    working, so that the convention still holds without the file.
12. As a radar user, I want Home and the board view to read the store directly,
    so that they are instant and never stale.
13. As a radar user, I want the daemon board monitor and markdown parsing
    deleted, so that there is one path, not two.
14. As a radar user, I want boards to survive an agent crash or a radar
    restart, so that the board is durable.
15. As a radar user, I want to opt out per project, so that repositories where
    radar's board is inappropriate get nothing.
16. As a future radar user, I want an export/git-backed board to be possible
    without a rewrite, so that the store's shape does not block it.

## Implementation Decisions

- **Ownership.** The daemon owns the store; it already owns the activity
  journal, attention and agent-session bindings, and the server is the only
  writer. CLI, GUI and web are clients. Mutations are commands; reads are
  snapshots; both go over the existing daemon protocol.
- **Placement.** Cards live in the daemon's SQLite store, alongside the
  activity journal (same database file or a sibling in the daemon run
  directory). The thread continues to be activity events keyed by card id — no
  second thread model.
- **Schema (shape, not final DDL).** Two tables:
  - lanes: id, project, name, kind (`backlog` | `in_progress` | `review` |
    `done` | `custom`), position.
  - cards: id (stable, generated once), project, lane, position, title,
    `body` (markdown text), claim, done, created/updated timestamps, revision.
  A project's lanes are seeded with the four defaults on first use.
- **Status.** A card's lane is its status; `done` is true exactly while the
  card sits in a done-kind lane, and clears when it leaves. One definition,
  replacing the column-heading-vs-checkbox ambiguity.
- **Claim.** Moving a card drops its claim, as today; a claim still names the
  agent by `RADAR_AGENT`, and the existing claim-to-conversation binding is
  unchanged.
- **Concurrency.** Mutations are revision-checked server-side (the pattern the
  attention store already uses); a stale write returns a conflict the client
  can retry. No whole-file rewrites, no mtime races.
- **Protocol.** Add a board snapshot command and card mutation commands to the
  daemon protocol. Successful mutations publish the existing `BoardChanged`
  activity event (action, card id, column, from-column) so every watcher —
  Home, the board pane, the web client — refreshes through the stream it
  already listens to.
- **CLI.** `radar card add / claim / release / move / done / next / show /
  comment / edit` keep their names and semantics, now store-backed. `radar
  board` renders the board from the store. `radar board import` imports a
  `BOARD.md` once. The `radar hook guard` surface is unchanged.
- **Skill and guard.** The board skill stops telling agents to edit
  `BOARD.md`; it describes the board as radar-owned and uses `radar card …`
  only. The guard and pre-commit hook already shell out to `radar hook guard`,
  so they only need their store calls updated.
- **Import.** On first board use for a project (or `radar board import`), if
  the store has no lanes for that project and a `BOARD.md` exists, parse it
  once into lanes and cards, preserving existing card ids. Idempotent, never
  destructive, and never deletes the file.
- **Removals.** The markdown parser/renderer, `ensure_card_ids`, the daemon
  board monitor's file watching and reconciliation, and the GUI's
  summary-from-parse path are deleted once the store is live.
- **Opt-out.** The existing per-project board-enabled setting is unchanged and
  now simply means "no store board for this project".

## Testing Decisions

A good test here asserts external behaviour — the board a client sees after a
command — not the storage internals.

- **Store (unit, in-memory SQLite).** Prior art: the activity journal's tests.
  Cover create/move/claim/complete/reorder, the done-from-lane rule, claim
  cleared on move, revision conflicts, and lane seeding.
- **Import (unit).** Prior art: the board parser's tests. Cover columns→lanes,
  cards with and without ids, notes, claims, and idempotence.
- **CLI end-to-end (real daemon).** Prior art: `tests/session_daemon.rs`.
  `radar card next/claim/move/done/show/comment` against a live daemon, plus
  restart durability.
- **Concurrency.** Two clients mutating one card: the stale write is refused,
  not silently clobbered.
- **GUI.** Headless render smoke (the broadway + chromium approach already
  used) that Home and the board view read the store with no file present.

## Out of Scope

- Git-backed or synced boards, and any two-way file reconciliation.
- A generated `BOARD.md` export (possible later; the schema is designed not to
  block it).
- Multi-user or remote boards.
- Reworking the activity thread or attention model.
- Richer card fields (assignee, due date, priority) beyond title, body, claim,
  lane, position — the schema leaves room but they are not built here.

## Further Notes

- This is a deliberate retreat from the file-as-source-of-truth promise in
  `board-interactivity.md`; that document should be updated when this lands.
- The `radar:card-id` comment format stops being an input but remains a valid
  way to read an id back from a historical file during import.
- The move is net-negative code: it deletes a parser, a file monitor, an id
  migration and a whole class of races, and adds one table pair and one
  protocol surface.
