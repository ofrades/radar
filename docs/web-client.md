# Radar's remote browser client

`radar web` starts a local HTTP/WebSocket client for the existing session daemon.
It binds to `127.0.0.1` by default (port `8787`) and does not expose the daemon's
private Unix socket. The page uses xterm.js — or the libghostty-vt WASM engine
when the build ships it — to render terminal bytes.

## Information architecture (native Home parity)

The browser client is the native Home, remotely. It is project-first and has no
permanent sidebar; navigation is a drill-down with a real Back:

- **Home** (`#/`) — the cockpit. A hero line ("N projects · M need you"), an
  **Agents** destination card, a **Needs you** lane of every unresolved request
  across every project with its answer/approve/deny/dismiss/mark-seen/
  acknowledge actions, and a **Projects** lane: one card per project with its
  board pills (lane name and open count), a to-do input, its open to-dos
  (tick to close, open to read), each carrying its claim and latest agent note,
  and a footer pulse of running and stopped agents.
- **Project** (`#/project/{id}`) — the project's board as columns (Todo, In
  progress, Review; the Done lane is where finished work leaves the board).
  A to-do opens its conversation.
- **Card** (`#/project/{id}/card/{cardId}`) — the card as a conversation: its
  markdown body, its stable id with a copy button, controls to edit, close or
  reopen and move it between lanes, its linked sessions, its thread (human and
  agent messages, board transitions, resolved answers, and any unresolved
  request with its actions), and a reply box.
- **Agents** (`#/agents`) — every project's sessions in one place; a running or
  retained session opens interactively, catalog-only history is inert.
- **Session** (`#/project/{id}/session/{sessionId}`) — a deep-linkable terminal.
  Opening a session hides navigation and gives the terminal the viewport;
  **Back** returns to where you were, and the browser's own back/forward work.

Running sessions open as interactive terminals; ended daemon sessions open
their retained terminal screen read-only. Catalog-only rows are marked
**ended** and deliberately inert, because the daemon no longer has a terminal to
attach to; reopen such a conversation from the native Radar sidebar, where
provider resume links are available when the provider exposed a stable session
id.

On mobile, the app shell follows the browser's visual viewport. When the
on-screen keyboard opens, the terminal reflows into the remaining space above
it.

The workspace refreshes each registered project's board, activity, and session
summary together. Board claim links appear only when a claim resolves to one
exact attachable session; ambiguous or ended claims are not guessed. Progress
polling retains the last snapshot and marks it unavailable when a refresh fails,
and never rebuilds a view under a control you are typing in.

The board is served from the daemon's store, not a file:
`/api/projects/{id}/board` returns the project's lanes and cards. Mutations are
revision-checked and publish a `BoardChanged` activity event so every open
client refreshes:

- `POST /api/projects/{id}/board` — add a to-do.
- `POST /api/projects/{id}/cards/{card}` — one mutation: `update` (title/body),
  `move` (lane), `claim`, `release`, `complete`, `reopen`, or `remove`.
- `POST /api/projects/{id}/cards/{card}/comments` — a human reply on the card's
  thread (a durable activity event keyed by the card's stable id).
- `POST /api/projects/{id}/attention/{request}` — an answer/approval/denial/
  dismissal, or the non-resolving `seen` / `acknowledge` change.

The native desktop also delivers deduplicated attention notifications; opening
one takes you to the relevant Board, and the request stays unresolved until you
answer or explicitly dismiss it.

## Visual system

The browser client wears the same visual system as the native app: the mono UI
face, a near-black canvas, hairline borders, faint fills over the canvas, and
omarchy's panel roundness. The tokens in `web/app.css` mirror the active omarchy
theme (`~/.local/state/omarchy/current/theme/colors.toml`) and Hyprland's live
`decoration:rounding`; the accent is the theme's accent sobered toward the
foreground and background exactly as `src/gui/style.rs` does. On a machine whose
rounding is `0` every surface is square, like the native app. Update the tokens
when the theme changes (reading them live over an endpoint is a follow-up).

## Run locally

```sh
cargo build
radar web
```

Open <http://127.0.0.1:8787>. Use `radar web --port 9000` to select another
loopback port. The daemon starts automatically if it is not already running.
Radar-launched agent/editor sessions appear in their project. **New terminal**
starts the user's login shell in the selected project. Closing the browser or
detaching a terminal leaves its process in the daemon.

## Keep it running with your user session

The packaged user unit runs the installed Radar binary on loopback and restarts
it if it exits unexpectedly. From the repository, install/update Radar and
enable the unit once:

```sh
./install.sh
install -Dm644 packaging/radar-web.service \
  "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/radar-web.service"
systemctl --user daemon-reload
systemctl --user enable --now radar-web.service
```

Check or restart it with `systemctl --user status radar-web.service` or
`systemctl --user restart radar-web.service`. To stop it and disable future
starts, run `systemctl --user disable --now radar-web.service`. Re-run
`./install.sh` when updating Radar; the unit continues to use the installed
`~/.local/bin/radar` executable.

Terminal input and resize are sent to the shared session; as with native attach,
the most recent resize sets the terminal dimensions for all attached views.
When the build ships the libghostty-vt WASM module the server sends a lossless
binary snapshot on attach and the raw byte stream after it, so the browser
restores the exact screen, modes, scrollback and cursor — the same snapshot the
native clients consume (see [`libghostty-vt.md`](libghostty-vt.md)). Without the
WASM module the xterm.js fallback restores the active screen and common modes
through the daemon's bounded ANSI replay; see the compatibility boundary in
[`session-daemon.md`](session-daemon.md).

The inbox reads durable project attention requests and sends revision-checked
answers/approvals to the daemon. Requests created with
`radar activity request --wait` can receive those typed responses. Agent output
appears in a card's thread and in the project feed when the agent reports it
through Radar's activity API.

## Reach it over Tailscale

On the host running Radar:

```sh
systemctl --user enable --now radar-web.service
tailscale serve --bg --https=443 --set-path=/radar 8787
```

Open the private HTTPS URL at
`https://<device>.<tailnet>.ts.net/radar` on a phone or other tailnet device.
The `/radar` mount can share port 443 with other Tailscale Serve paths. Tailscale
Serve requires HTTPS certificates to be enabled for the tailnet; its access
controls apply to the service. Radar remains bound to loopback so requests must
go through the local service proxy. Keep this a tailnet-only Serve configuration;
do not enable Funnel for Radar.

To verify or remove the Serve mapping, use `tailscale serve status` and
`tailscale serve --https=443 --set-path=/radar off`. The former `:8443` mapping
can be removed with `tailscale serve --https=8443 off`. Serve configuration
belongs to Tailscale and may remain active after `radar web` exits, so stop the
web process when the client should no longer be reachable.

## Browser assets

The terminal frontend vendors `@xterm/xterm` 6.0.0 and `@xterm/addon-fit`
0.11.0 from npm under `web/vendor/`, with their MIT license files alongside.
They are served locally at `/assets/xterm.mjs`, `/assets/fit-addon.mjs` and
`/assets/xterm.css`, so the browser needs no public CDN. The libghostty-vt WASM
module (`/assets/ghostty-vt.wasm`) is built by `build.rs`; when it loads, the
page uses it instead of xterm.js.

The WASM engine runs on a 32-bit target, so the sized structs it reads
(`GhosttyRenderStateColors`, `GhosttyRenderStateCursor`,
`GhosttyTerminalScrollViewport`) are laid out with a 4-byte `size_t`. The
offsets in `web/ghostty-terminal.mjs` must match that wasm32 layout, not the
host's 64-bit layout.
