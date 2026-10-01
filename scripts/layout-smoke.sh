#!/usr/bin/env bash
# Native allocation checks need a Broadway browser client to drive frame ticks.
set -euo pipefail
cd "$(dirname "$0")/.."
command -v gtk4-broadwayd >/dev/null
command -v chromium >/dev/null
cargo test --features gui --lib responsive_panels_fit --no-run
dbus-run-session -- bash -euo pipefail -c '
  gtk4-broadwayd -p 8097 -a 127.0.0.1 :97 >/tmp/opencode/radar-layout-broadway.log 2>&1 &
  display=$!
  sleep 1
  chromium --headless=new --no-sandbox --disable-gpu --window-size=2600,1800 \
    http://127.0.0.1:8097/ >/tmp/opencode/radar-layout-browser.log 2>&1 &
  browser=$!
  trap "kill $display $browser 2>/dev/null || true" EXIT
  env -u WAYLAND_DISPLAY -u DISPLAY GDK_BACKEND=broadway BROADWAY_DISPLAY=:97 \
    cargo test --features gui --lib responsive_panels_fit -- --ignored --test-threads=1
'
