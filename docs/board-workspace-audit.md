# Board/workspace primitive audit — r2-0d4

## Scope and findings

- **Board ownership is already sound.** The daemon owns durable card mutations,
  revisions, and `BoardChanged` events. SQLite and Beads satisfy the existing
  storage interface. Keep this seam; do not add a second GUI-owned board model.
- **Workspace restoration policy was buried in the GUI.** It reconciled panels
  but removed legacy board leaves only when a presentation flag was set. It
  also retained missing/duplicate leaves, throwing away usable split structure
  later during widget restoration.
- **Board/workspace synchronization duplicated its policy.** Existing and newly
  discovered sessions separately decided whether to show panels. Display names
  could override canonical lane kinds, contrary to the documented fallback.
- **Todo saves blocked GTK and hid errors in stderr.** The original dialog
  already sent an expected revision, but users could not see why saving failed.

## Changes

- `db::workspace_restore_plan` now reconciles saved state against available tool
  keys without GTK. It preserves panel order, excludes every Board instance,
  collapses missing/duplicate layout leaves, sanitizes split ratios, and drops
  invalid zoom targets. GTK still owns process attachment and widget layout.
- `BoardState::workspace_visibility` is the single synchronization policy:
  In progress/Review show; done/missing hide; Todo/custom preserve user choice.
  Canonical kinds win; joined lane names are used only without a lane record.
  Both existing and newly discovered sessions use it. Hiding does not stop an
  agent process.
- `gui::card::inline_edit_form` owns the editor UI and async save lifecycle.
  Saves run on a worker thread, duplicate clicks are disabled, failures remain
  visible with the draft intact, and revision checks remain mandatory. Unchanged
  cards leave edit mode without a mutation; blank titles get inline validation.
  Successful saves use the existing board refresh/synchronization path.
  (Originally a modal dialog; now the form swaps in where the card controls sit,
  so a card is edited inline in Home's card detail.)

## Verification

- `cargo test --features gui`: 290 library tests and 20 integration tests passed;
  two display-dependent tests skipped in this run.
- The new ignored `editor_validates_titles` test passed separately on a private
  D-Bus session with GTK Broadway. It checks validation and unchanged-save
  behavior without contacting or modifying a real board.
- Existing integration tests cover stale revisions without writes, card/event
  lifecycles, and persistence across daemon restarts. New headless tests cover
  restoration edge cases and synchronization policy, including misleading lane
  names.

### Reviewer UI checks

1. Edit a card; save a changed title/body. Confirm Home and its linked workspace
   reflect the new board snapshot.
2. Open the editor, edit the same card through the CLI, then save the dialog.
   Confirm the conflict is visible and the typed draft remains. Cancel/reopen
   to load the latest revision; no automatic conflict merge is implemented.
3. Move a linked card between In progress, Review, and Done. Panels should show,
   show, and hide respectively, without killing the agent process.
4. Resize the editor and restore a workspace containing multiple tool panels.

## Deliberately deferred

No new board pane, storage migration, UI redesign, or general-purpose workspace
framework. Board fetching, quick-add, and lane moves still use their existing
synchronous daemon calls; moving those off GTK is follow-up work. Unsaved drafts
are retained on save failures, not after closing the editor. Legacy serialized
fields remain readable for backward compatibility.

## Follow-up: session ↔ todo parity

The user clarified that bidirectional navigation is part of this card, not a
separate polish task. This follow-up adds:

- Todo detail lists associated running/stopped sessions with explicit Open or
  Resume controls. Missing exact conversation identities are disabled rather
  than silently opening the provider's last conversation. Claimed cards with
  legacy bindings can still open their exact claimed session.
- A session's todo chip records its originating pane. Back from that todo
  returns to the pane, rather than dropping the user at the Home cockpit.
  Opening Home explicitly clears this return context.
- The session catalog retains `card_id` from the registry's actual launch
  environment, independently of board claims and process lifetime. The v1→v2
  migration leaves unknown historic associations empty rather than guessing.
  Provider-row merges preserve the binding atomically, and exact resume on a
  new pane merges into one durable conversation record.
- Resume carries the todo binding into the relaunched process. History
  navigation cannot displace an unrelated live agent or reuse another
  provider's quiet pane. Repeated Start requests navigate to an existing
  worker/current claim instead of stealing a claim with another worker.

Verification: `cargo test --features gui` passed 294 library and 20 integration
tests. `scripts/home-smoke.sh` passed separately: its isolated GTK/daemon test
now repeats todo→session→todo→Back and asserts only one agent session exists.
Catalog tests cover migration, exit/restart retention, provider merge, exact
resume, and clearing a stale binding when a pane is reused without a todo.

### Runtime rollout

The GUI can attach to the existing daemon without killing running agents.
Durable launch-time bindings require the new daemon binary and take effect on
its next restart. Do not forcibly restart the real daemon while it owns live
work. Existing live processes still link via `RADAR_CARD_ID`; older history can
only link where an exact legacy claim binding survives. Unknown historic links
are not fabricated.

## Autonomous native journeys — r2-fzg

- Workspace **＋** opens **To-dos & sessions** in the shared HUD, not a sidebar.
  Creation, conversation, status and agent/conversation selection reuse the
  Tools & shortcuts shell. Panel-to-task navigation stays in the workspace.
  Auto arrange lives in Tools & shortcuts; the project label is not navigation.
- **Create & start** creates and assigns a task to the default agent. Enter
  creates without starting work. Board work does not launch unrelated layout
  presets, switch to a terminal, or change the last-project preference.
- The conversation is primary; terminal inspection is a collapsed optional
  section. **Send to agent** starts work or follows up on the same linked
  conversation, even after Review released its claim. Follow-ups on Done reopen
  the task. Outcomes still come from agent comments and board transitions, not
  guesses from process exit or terminal output.
- A selected stopped conversation resumes with a fresh runtime/claim and the
  new task's environment. Its exact catalog row and task association are merged
  together. Running workers cannot be reassigned or interrupted by this picker.
- Commands resolve live workers from the daemon, not a mapped GTK terminal.
  Conversation routing queries the catalog by card identity instead of relying
  on a possibly stale sidebar discovery snapshot. Persistent many-to-many task
  links survive claim release, exact-conversation reassignment and restart.
  Todo/In progress/Review use the same authoritative session-open action.
  Bare sessions become linked when their exact daemon-owned agent claims a task;
  recoverable claim bindings are persisted when opening them. Missing exact
  links expose reconnection controls instead of guessing the last conversation.
- Exact provider identity travels in the spawn environment and is bound by the
  daemon after Create records the catalog row; the GUI's earlier binding raced
  asynchronous creation. Beads board snapshots now expose the same effective
  revision the daemon validates, including multiple mutations in one timestamp.

Verification includes an isolated Broadway smoke with inert agent executables:
workspace creation/navigation; board create/start; live Review follow-up;
stopped exact resume; rendered agent summary; human completion and reopening;
conversation reassignment with durable task binding; and choosing a fresh Pi
agent. No real agent CLI is launched by this test. Separate daemon tests assert
exact spawn identity and task binding are recorded together, and a regression
forces coarse Beads timestamps to check snapshot revisions.

These changes require the new daemon for atomic exact binding and card-filtered
catalog queries. Install and restart it only at an agreed safe point: stopping
it terminates all its running sessions. A GUI-only reload cannot roll out the
server fixes. Web-client journey parity is not changed by this native work. This follow-up supersedes the earlier “no storage
migration” scope: it adds a backward-compatible catalog column, not a board
storage migration.
