# Radar's remote browser client

`radar web` starts a local HTTP/WebSocket client for the existing session daemon.
It binds to `127.0.0.1` by default (port `8787`) and does not expose the daemon's
private Unix socket. The web page uses xterm.js to render terminal bytes and
provides a responsive project view with sessions, activity, and outstanding
attention requests.

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
radar web
tailscale serve --bg --https=8443 8787
```

The Serve command prints the private HTTPS URL (for example,
`https://laptop.example.ts.net:8443/`) to open on a phone or other tailnet
device. Using a separate HTTPS port preserves an existing default-port Serve
route. Tailscale Serve requires HTTPS certificates to be enabled for the
tailnet; its access controls apply to the service. Radar remains bound to
loopback so requests must go through the local service proxy. Keep this a
tailnet-only Serve configuration; do not enable Funnel for Radar.

To verify or remove the Serve mapping, use `tailscale serve status` and
`tailscale serve --https=8443 off`. Serve configuration belongs to Tailscale and
may remain active after `radar web` exits, so stop the web process when the
client should no longer be reachable.

## Browser assets

The terminal frontend vendors `@xterm/xterm` 6.0.0 and `@xterm/addon-fit`
0.11.0 from npm under `web/vendor/`, with their MIT license files alongside.
They are served locally so the browser does not need a public CDN.
