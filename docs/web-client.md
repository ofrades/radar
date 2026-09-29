# Radar's remote browser client

`radar web` starts a local HTTP/WebSocket client for the existing session daemon.
It binds to `127.0.0.1` by default (port `8787`) and does not expose the daemon's
private Unix socket. The web page uses xterm.js to render terminal bytes and
provides a responsive project view with sessions, activity, outstanding
attention requests, and expandable project-sidebar agent lists. Live and
registry-retained rows are “Open session” actions: running sessions open as
interactive terminals; ended daemon sessions open their retained terminal
screen read-only.

Opening a session switches to an immersive terminal view that hides project
navigation so the terminal uses the available viewport; **Workspace** returns
to the project view.

On mobile, the app shell follows the browser's visual viewport. When the
on-screen keyboard opens, the terminal reflows into the remaining space above
it.

The session endpoint also includes durable catalog history. Catalog-only rows
are sorted by their provider activity time and marked **ended**; they are
deliberately inert because the daemon no longer has a terminal to attach to.
The browser identifies that state instead of sending a request that can only
fail. Reopen such a conversation from the native Radar sidebar, where provider
resume links are available when the provider exposed a stable session id. Use
**Workspace** to return to the project and choose another live session.

The workspace refreshes each registered project's board, activity, and session
summary together. Project rows show completed work (the **Done** column alone
defines completion), active/review counts, unresolved requests, and explicit
reported agent state. Board claim links appear only when a claim resolves to one
exact attachable session; ambiguous or ended claims are not guessed. The
**Board progress** view shows the project's live card columns, while the
attention shortcut remains available in the terminal and jumps to the first
outstanding request without clearing it. Progress polling retains the last
snapshot and marks it unavailable when a refresh fails.

The board is served from the daemon's store, not a file: `/api/projects/{id}/board`
returns the project's lanes and cards, and card mutations (revision-checked)
publish a `BoardChanged` activity event so every open client refreshes. The
native desktop also delivers deduplicated attention notifications; opening one
takes you to the relevant Board, and the request stays unresolved until you
answer or explicitly dismiss it.

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
The server sends an atomic bounded ANSI display replay and then sequenced live
output. This restores the active screen and common modes, but not every hidden
emulator/parser state; see the compatibility boundary in
[`session-daemon.md`](session-daemon.md).

The inbox reads durable project attention requests and sends revision-checked
answers/approvals to the daemon. Requests created with
`radar activity request --wait` can receive those typed responses. Agent output
appears in the feed when the agent reports it through Radar's activity API.

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
They are served locally so the browser does not need a public CDN.
