# radar

<img src="packaging/radar.svg" alt="radar logo" width="96">

A native workspace manager: **projects in a sidebar, one tab per tool**.

Open radar and you get a real window. The sidebar lists your projects with their
git state; picking one shows that project's tabs. Each tab runs one program —
your editor, your agent, your diff — with the project as its working directory.
Switching projects switches everything, and the tabs of projects you leave stay
alive, so a running agent is never interrupted.

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
| Find a project to add (sidebar search) | `Ctrl+Shift+N` |
| Add any program as a tab | `Ctrl+Shift+P` |
| New Editor / Agent / Diff tab | `Ctrl+Shift+E` / `Ctrl+Shift+A` / `Ctrl+Shift+G` |
| New Board / Commands tab | `Ctrl+Shift+B` / `Ctrl+Shift+T` |
| Close tab | `Ctrl+Shift+W` |
| Preferences | `Ctrl+,` |
| Toggle sidebar | `F9` |
| Zoom the focused pane's font | `Ctrl+=` / `Ctrl+-` (or `Ctrl+scroll`); `Ctrl+0` resets |
| Refresh status | `Ctrl+Shift+R` |

**Adding a project** opens no dialog: the sidebar flips into find mode. The same
search box now filters directories under the scan root (usually `~/Work`) —
repositories are marked `git`, ones already in the sidebar say `added`, and
clicking a row (or pressing Enter) adds the project and opens it, so several can
be added in one pass. `Esc` — or the **+** again — brings the project list back,
and the small `from ~/Work` label above the list picks another directory to
scan.

The **+** on the tab bar offers your preferred editor, agent, diff and shell plus
"Choose program…" (every terminal program radar can find, grouped by kind).

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
| `settings` | preferences (preferred programs, flag policy), UI state |
| `events` | what happened, for history and recents |

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
radar prefs agent claude    # set one
radar pin / move / rename / remove / prune
radar board                 # the project's kanban (BOARD.md)
radar card add / claim / release / move / done / next
radar doctor                # environment check
```

## The board

Every project has a kanban, and the kanban is a file: `BOARD.md` in the project
root, created when the project opens. Columns are `## ` headings, cards are
`- [ ]` lines, a claim is a `@name` on the card's line, indented lines under a
card are its notes.

That plainness is the point: an agent already running in the project claims
work and moves it along by editing the file with the tools it already has — no
radar API, no adapter. radar's **Board** pane (Ctrl+Shift+B) renders the same
file as a native kanban: drag cards between columns, click to edit, and the
pane re-reads the file whenever anyone — an agent, the CLI, `git checkout` —
writes it. The file is the board; radar is one of its editors.

For scripts that want a lock-free answer to "what should I do next":

```bash
radar card next --by claude   # claims the first unclaimed card, prints it
radar card done "Fix login"   # marks it done, moves it to the last column
```

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
cargo test                  # 104 tests: store, board, git parsing, registry, discovery
cargo clippy --all-targets  # clean
cargo build --features gui  # no VTE needed
```

Layout:

```
src/db/          SQLite: projects, tabs, settings, events  (+ migrations)
src/board.rs     the kanban: BOARD.md parse/render, atomic card ops
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
