# radar

A native workspace manager: **projects in a sidebar, one tab per tool**.

Open radar and you get a real window. The sidebar lists your projects with their
git state; picking one shows that project's tabs. Each tab runs one program —
your editor, your agent, your diff — with the project as its working directory.
Switching projects switches everything, and the tabs of projects you leave stay
alive, so a running agent is never interrupted.

```
┌────────────────┬──────────────────────────────────────────────┐
│ Projects       │  Neovim │ OpenCode │ Hunk                    │
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
| Add project… | `Ctrl+Shift+N` |
| Add any program as a tab | `Ctrl+Shift+P` |
| New Editor / Agent / Diff tab | `Ctrl+Shift+E` / `Ctrl+Shift+A` / `Ctrl+Shift+G` |
| Close tab | `Ctrl+Shift+W` |
| Preferences | `Ctrl+,` |
| Toggle sidebar | `F9` |
| Refresh status | `Ctrl+Shift+R` |

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
radar doctor                # environment check
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
cargo test                  # 81 tests: store, git parsing, registry, discovery
cargo clippy --all-targets  # clean
cargo build --features gui  # no VTE needed
```

Layout:

```
src/db/          SQLite: projects, tabs, settings, events  (+ migrations)
src/programs/    program registry, omarchy agent knowledge, argv building
src/discover/    directory discovery: fd or walk, fuzzy filtering
src/git.rs       branch / ahead / behind / changed, read-only
src/gui/         window, sidebar, tabs, dialogs, theme, terminal panes
src/main.rs      CLI
```

The core (`db`, `programs`, `discover`, `git`) has no GTK dependency, so it can
back other front-ends — a shell widget, a status bar, or the `--json` output of
the CLI — without duplicating any state.
