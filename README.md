# radar

<img src="packaging/radar.svg" alt="radar logo" width="96">

A native workspace manager: **projects in a sidebar, tabs for every tool**.

Open radar and you get a real window. The sidebar lists your projects with their
git state; picking one shows that project's tabs. Expand a project to see its
agent sessions, including ended conversations from the durable session catalog,
sorted by recent activity. Radar keeps **one agent panel**: selecting a session
shows it there — its tab joins the panel's header as a chip, and the agent
that was on screen is hidden but never stopped, its process and pty safe in
the session layer. A tab dragged out for a side-by-side goes home on the next
sidebar selection. Whichever session the agent panel is showing is marked in
the list, so the sidebar always says which agent is on screen. External
CLI sessions under terminal windows are discovered by working directory and show
the CLI or terminal-window title. Selecting an external session focuses its
terminal window. Catalog rows with a provider resume id reopen that exact
conversation; rows without one are kept as honest history instead of guessing.
Ended or failed history older than seven days moves to the archived view
automatically. The project's **+** starts another agent. Live sessions are
rediscovered after a Radar restart, and tabs of projects you leave stay alive,
so a running agent is never interrupted.

```
┌────────────────┬──────────────────────────────────────────────┐
│ Projects       │  Neovim │ OpenCode │ Lazygit                 │
│ ────────────── │ ──────────────────────────────────────────── │
│ api-server  ●3 │                                              │
│ web-app        │            (a real terminal,                  │
│ infra          │             running the program              │
│                │             for this project)                │
└────────────────┴──────────────────────────────────────────────┘
```

## Why a native app instead of a terminal plugin

Agents, editors and diff tools are terminal programs, and terminal programs
want a real terminal: mouse, clipboard, OSC 52, hyperlinks, key protocols. radar
gives each one an embedded terminal (VTE) inside a native window, instead of
imposing an editor's key handling on top of it.

Agents are run exactly as their authors shipped them — the CLI/TUI, in a
terminal, with the project as its working directory. No API adapters, no
reimplemented session browser: radar stays out of the way and the agent keeps
all of its own behaviour.

## Install / build

```bash
# the terminal widget the app embeds (Arch, via omarchy)
omarchy pkg add vte4

cd ~/Work/radar
cargo build --release --features vte     # full app, embedded terminals
./target/release/radar
```

`vte4` is optional: without it, build with `--features gui` and the app still
works — each tab then offers **Open in a terminal window** instead of embedding
the program. That keeps radar usable on a machine where you cannot install VTE.

## Using it

| Action | Shortcut |
| --- | --- |
| Keys & primitives overlay (open a primitive, read the keymap) | `Alt+H` |
| Home panel — programs, layout, new project | `Alt+Home` |
| Move between the panes on screen | `Alt+Arrows` |
| Cycle panes — the sidebar included | `Ctrl+Tab` / `Ctrl+Shift+Tab` |
| The focused pane's menu (group, move, zoom, close) | `Menu` / `Shift+F10` |
| Search projects (filter, or find one to add) | `Alt+N` |
| Show or hide a primitive | `Alt+E` / `A` / `G` / `K` / `T` |
| Change the focused pane's program | `Alt+P` |
| Focus Editor / Agent / Changes / Commands | `Alt+1` – `4` |
| Preferences | `Alt+,` |
| Toggle sidebar | `Alt+B` |
| Zoom the focused pane's font | `Alt+=` / `Alt+-` (or `Ctrl+scroll`); `Alt+0` resets |
| Zoom the focused pane to the whole window | `Alt+F` |
| Refresh status | `Alt+R` |

**The keyboard drives the app, but each primitive keeps its own keys.** All of
radar's chords sit on Alt — every Ctrl key a program wants, in any panel,
reaches it untouched. The one exception is `Ctrl+Tab` for cycling: the window
manager owns `Alt+Tab`, so it never reaches the app at all. And `Alt+Arrows`
come with a rule: a text cursor — in a search box, a dialog, an agent
prompt — keeps them. radar takes the chord only when the keys belong to a
pane. Opening a panel — with a chord, the dock, or the HUD — puts the keys
straight into it. Panes and the sidebar wear a quiet ring while they hold
the keys, so you can see where they are.

**The overlay** (`Alt+H`, also in the workspace menu) floats over the
workspace: a row per primitive — open one, or see that it is already on
screen — the settings, and the whole keymap. Type to filter, arrows to move,
`Enter` to run, `Esc` to go back to the program you were looking at.
Changing a pane's program from its menu or with `Alt+P` opens program
choices in this same overlay.

**Home** (`Alt+Home`, the button by the logo, or the first dock toggle) is the
workspace at rest — and the panel radar opens with when there are no projects
yet. It holds the program each slot uses, the layout a new project opens with,
and the two ways forward: **Find projects** (the sidebar's search) and **New
project…**, which picks or creates a folder, runs `git init` in it, and opens
it. Going home never stops a program; the panes keep running behind it.

**Adding a project** needs no dialog and no button: the sidebar's search box
does both jobs. It filters your projects, and below them it lists directories
under the scan root (usually `~/Work`) that match — repositories marked `git`,
each with its own **+**, so several projects can be added in one search.
Adding never leaves the search: the project joins the rows above the moment
it is added. The small `from ~/Work` label picks another directory to scan,
and `Esc` empties the search.

**Every pane header carries its own controls, right on the chip**: the
program's live info beside its name, a ▾ dropdown to change that program —
filtered to the chip's own primitive, so an agent chip lists agents and an
editor chip lists editors — a ＋ that adds **another tab of the same
primitive** (a second agent tab, grouped under the same header, its own
process; the list is the same kind-filtered one), and an × to close it — the
program keeps running in the background either way. Tabs of the same
primitive number from the second: "Agent 2". The ⋮ menu keeps the
pane-level actions: group with, split out, move, zoom, close.

**Adding a different primitive to a pane** is a drag of one header onto
another — or the pane menu's "Group with …". The dock and the chords keep
aiming at a primitive's first tab.

**Preferences** is where "preferred editor / agent / diff / shell" lives. Leave a
slot on *Auto* and radar picks the best installed one; the resolved choice is
shown next to it. There is also a switch for whether agents start with their
skip-permission flags, matching how omarchy's own keybinding launches them.

## State

Everything lives in SQLite at `~/.local/share/radar/radar.db`:

| table | what |
| --- | --- |
| `projects` | path, name, pinned, order, recency, open count |
| `tabs` | per project: slot, program, title, order, extra args |
| `workspace_state` | per project: primitive groups, active primitive, split tree, divider positions, zoom |
| `settings` | preferences (preferred programs, flag policy), UI state |
| `events` | what happened, for history and recents |

Closing the window keeps Radar running in the background, so embedded agents
remain alive. Use **Workspace → Quit** (or `Alt+Q`) to exit. The install
script adds a login autostart entry; after a reboot Radar relaunches the saved
workspace for the last selected project. Other projects restore their layouts
when selected. Their visible programs are launched again after a reboot; whether
an agent resumes its prior conversation depends on that agent's CLI support.

Set `RADAR_HOME` (or `--home`) to point at another directory — that is how the
tests stay off your real data.

## CLI

The app is also a CLI, which is handy for scripting and is how the core is
tested:

```bash
radar                       # open the app
radar list [--json]         # projects with branch, changes, tabs
radar add ~/Work/api ~/Work/web
radar add --scan ~/Work     # every repository under a directory
radar find api              # fuzzy directory candidates (fd + skim)
radar open api-server       # what its tabs resolve to, command per tab
radar agents                # agents, omarchy's default, what is installed
radar programs diff         # the registry, by kind
radar prefs                 # resolved preference per slot
radar prefs agent omp        # set one of OpenCode, OMP, or Cursor
radar web [--port 8787]     # responsive browser client on localhost
radar pin / move / rename / remove / prune
radar board                 # the project's kanban (BOARD.md)
radar card add / claim / release / move / done / next
radar hook guard            # the board's pre-edit check, for harness hooks
radar doctor                # environment check
```

New agent selections are limited to OpenCode, OMP (Oh My Pi), and Cursor. Other
registered agents remain available to restore existing tabs, but do not appear
in agent selectors or resolve as new/default agent choices.

## Persistent session daemon (client/server groundwork)

`radar serve` runs a GTK-free session daemon; `radar session spawn`, `list`,
`snapshot`, `stream`, `watch`, `input`, `resize`, `stop`, and `forget` expose its
client API. Daemon sessions survive client exits. Terminal output and lifecycle
feedback have separate bounded streams with explicit resync on overload.

The daemon also stores a replayable, project-sequenced activity journal and
revision-checked, persistent attention requests. Radar-launched programs receive
`RADAR_PROJECT_ID`, `RADAR_SESSION_ID`, and `RADAR_HOME`; agents can report
explicit state or create a request without mixing it into terminal output:

```sh
radar activity state --state working
radar activity request --kind question --reason "Which target?" \
  --allow answer --allow dismiss --command-id question-1 --wait
radar activity snapshot --project-id 7
radar activity watch --project-id 7 --after 12
radar activity respond attention-7-3 --revision 1 --action answer \
  --answer staging --command-id answer-1
```

`radar board --json` includes a stable ID for each card. `RADAR_CARD_ID` or
`--card-id` associates agent activity and requests with that card. Request
creation, seen/acknowledged state, and responses are persisted under the private
daemon run directory; reconnecting clients replay events or receive an explicit
resync requirement. `activity request --wait` keeps an agent-side command open
until a human responds, then prints the authoritative typed answer/approval as a
second JSON result. If the watcher reconnects or falls behind, it checks the
persistent request record before resuming, so a response is not lost to replay
limits.

See [the daemon guide](docs/session-daemon.md) for commands and protocol details.
The GUI autostarts the daemon; with VTE enabled, its local PTY is only the VTE
input adapter for daemon-owned sessions. Snapshot replay restores the active
screen, common modes and recent scrollback, but full parser-state restoration
remains the next terminal compatibility step.

Agent conversation history is provider-specific. Radar imports and reopens exact
OpenCode conversations from OpenCode's JSON session list. OpenCode, OMP, and
Cursor can resume the last conversation; exact resume flags are configured when
a provider session ID is available. OMP and Cursor sessions started in another
terminal appear as external-terminal rows that focus the original terminal;
Radar does not claim those separate processes are attached to its PTY. Their
history is not imported unless the CLI provides a supported machine-readable
session list.

## Remote browser client

`radar web` serves a responsive browser workspace on `127.0.0.1:8787`: choose a
project, see its Radar-launched sessions and activity, answer outstanding agent
requests, open running sessions interactively or ended sessions read-only, and
start a project shell. Detaching the web terminal leaves a running session alive.
The browser terminal is rendered with xterm.js; it is independent of the native
VTE/Ghostty renderer.

To keep the client running with your user session, install and enable the
packaged systemd user unit; see [the web client guide](docs/web-client.md).

To reach it from another device on your tailnet, make sure `radar web` is
running (or enable `radar-web.service`), then publish it under `/radar` on the
main HTTPS port:

```sh
tailscale serve --bg --https=443 --set-path=/radar 8787
```

Open `https://<device>.<tailnet>.ts.net/radar`. This path can coexist with other
Tailscale Serve routes on port 443. Access follows your tailnet ACLs. Radar
listens only on loopback; the browser gateway talks to the private session-daemon
socket locally. Avoid Tailscale Funnel for this service. See [the web client
guide](docs/web-client.md) for details.

## The board

The Board (Alt+K) opens as a full-width, full-height workspace panel. Its
columns expand with the window; it cannot be grouped or split into a tool tile.
Select a tool from the dock to return to the saved tool arrangement, or close
the board with Alt+K, its close button, or Alt+F. That arrangement is restored
when you reopen the project, even if you left Board open. Sessions keep running
while the board is open.

The client/server direction is **board-first interaction**: the server owns
session state, explicit agent activity and durable attention; the board presents
the live project feed, explicit agent states and actionable human requests.
Sidebar badges show unresolved requests across projects. The activity panel can
mark requests seen, acknowledge them, answer questions, approve/deny, dismiss,
and open the linked session. Agents can block on `radar activity request --wait`
to receive that typed response. Vendor-specific adapters and exact terminal-state
import remain follow-up work. See
[the review and delivery contracts](docs/board-interactivity.md).

Projects use an optional kanban stored as `BOARD.md` in the project root.
Boards are enabled by default. Toggle **Board** on a project's sidebar row to
opt that project out; the setting is stored in Radar's global database, not
the project tree.

Open **Project defaults** from a project's sidebar row to set its default
Editor, Agent, Diff, and Shell programs. Each pane inherits the corresponding
global **Preferences** choice until a project override is selected. Board and
pane defaults are per-project Radar settings and are never written to files
that need to be committed with the project.

When disabled, Radar does not initialize or open the board and its claim
guards allow edits and commits. Existing `BOARD.md` and skill files are left
untouched. When enabled, the board file is initialized as the project opens.
Columns are `## ` headings, cards are `- [ ]` lines, a claim
is an `@name` on the card's line, and indented lines under a card are its
notes. radar adds an invisible HTML comment with a stable card ID; ordinary
markdown rendering does not show it.

That plainness is the point: an agent already running in the project claims
work and moves it along by editing the file with the tools it already has — no
radar API, no adapter. radar's **Board** pane (Alt+K) renders the same
file as a native kanban: drag cards between columns, click to edit, and the
pane re-reads the file whenever anyone — an agent, the CLI, `git checkout` —
writes it. The file is the board; radar is one of its editors.

For scripts that want a lock-free answer to "what should I do next":

```bash
radar card next --by claude-mx7k2b1f   # claims the first unclaimed card, prints it
radar card next --in Review --by codex-9x3k1a   # a reviewer picks up review work
radar card move "Fix login" --to Review  # hands the card over: the claim drops
```

### The convention

When an agent pane opens in a board-enabled project, radar writes harness skill
files for opencode and Claude Code (`.opencode/skills/board/SKILL.md` and
`.claude/skills/board/SKILL.md`) and installs a pre-commit claim gate when the
repository has no conflicting hook. The setup never changes `AGENTS.md`.
A unique `RADAR_AGENT` environment variable — `claude-mx7k2b1f`, for example —
distinguishes concurrent instances so each board claim identifies its owner.

The loop the skill teaches: claim with `radar card next --by "$RADAR_AGENT"`,
work one card at a time, hand over by moving to **Review** with a note for
whoever checks, and prefer picking up Review work — a card is done when a
*different* agent closes it with `radar card done`. Moving a card drops its
claim, so the handover is real: the moment a worker's card reaches Review, the
worker's edit rights are gone until it claims again.

Claimed cards are links, too: clicking a card's `@name` opens that agent's
session in the agent panel. The match is exact where radar can be: the tab
whose program carries the claim as its own `RADAR_AGENT` (read from the
process), so two tabs of one program are told apart by their stamps; the
claim's leading program picks the tab when the process cannot be asked, and
any agent tab takes the rest — an agent radar did not launch works in the
agent panel all the same. The claimed conversation, when it is already the
one running, is never disturbed; anything else — an exited agent, a tab not
yet opened, another instance in the way — runs again resumed. And the
resume is bound to the claim: when an agent exits, radar captures the
conversation its CLI recorded for that instance and stores it (opencode's
session list; other agents fall back to the project's last conversation),
so the next click reopens **that exact conversation** with
`opencode -s <id>`. A displaced instance's conversation stays in the CLI's
own session store.

And the requirement has teeth, in the one place every agent already answers
to: **git**. radar installs a `pre-commit` hook in the repository's own hooks
directory (never touching a hook it did not write, never writing outside
`.git`), and the hook refuses a commit from a radar-launched agent that holds
no board claim — whatever tool the agent edited with. The refusal text is the
remedy, so the agent claims and retries. Humans in their own terminals set no
`RADAR_AGENT` and are never blocked. The check is `radar hook guard --commit`
— the same subcommand can judge a file edit for a harness that offers pre-edit
hooks, but nothing needs to be configured for the gate to hold.

## How it fits with omarchy

- **Agents**: the registry mirrors omarchy's list and its "don't stop to ask"
  flags (`claude --permission-mode auto`, `codex --approve-for-me`,
  `crush --yolo`, `cursor-agent --yolo --trust`, …). `omarchy default agent`
  decides which one leads the list; for that agent radar runs
  `omarchy agent --inline` so omarchy keeps owning the flags. `omarchy agent`
  and radar agree, always.
- **Theme**: colours come from
  `~/.local/state/omarchy/current/theme/colors.toml` and the font from your
  terminal config, so panes look like your terminal. A file monitor re-applies
  them when you switch themes.
- **Adding projects**: `fd` (with a hard-prune list and repository boundaries,
  so `node_modules` and the inside of a repo are never offered), scored with
  skim matching over name *and* path.

## Development

```bash
cargo test                  # 120 tests: store, board, skill, git parsing, registry, discovery
cargo clippy --all-targets  # clean
cargo build --features gui  # no VTE needed
```

Layout:

```
src/db/          SQLite: projects, tabs, settings, events  (+ migrations)
src/board.rs     the kanban: BOARD.md parse/render, atomic card ops
src/skill.rs     the convention: skill install, the pre-edit guard
src/programs/    program registry, omarchy agent knowledge, argv building
src/discover/    directory discovery: fd or walk, fuzzy filtering
src/git.rs       branch / ahead / behind / changed, read-only
src/gui/         window, sidebar, panes, dialogs, theme, terminal panes
src/main.rs      CLI
```

The core (`db`, `board`, `programs`, `discover`, `git`) has no GTK dependency,
so it can back other front-ends — a shell widget, a status bar, or the `--json`
output of the CLI — without duplicating any state.

## Development aids

Two environment variables exist because layout bugs are hard to see otherwise:

- `RADAR_SPLIT_ON_START=1` builds a split layout on startup (split right, then
  split down), so the pane geometry can be checked without clicking.
- `RADAR_TRACE=/tmp/radar.log` appends what happens when tabs are opened and
  panes are split, which is far easier to read than a screenshot when a widget
  does not appear.
- `RADAR_NEW_PROJECT=/some/path` runs the new-project flow on startup — folder,
  `git init`, add, open — without the file chooser.
