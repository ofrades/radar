#!/usr/bin/env bash
# Exercise native Home navigation on an isolated GTK display and session bus.
set -euo pipefail
cd "$(dirname "$0")/.."
command -v gtk4-broadwayd >/dev/null
cargo build --features gui
export RADAR_TEST_BIN="$PWD/target/debug/radar"
cargo test --features gui --lib home_navigation_replaces --no-run
dbus-run-session -- bash -euo pipefail -c '
  gtk4-broadwayd -p 8091 -a 127.0.0.1 :91 >/tmp/opencode/radar-home-broadway.log 2>&1 &
  display=$!
  trap "kill $display 2>/dev/null || true" EXIT
  env -u WAYLAND_DISPLAY -u DISPLAY GDK_BACKEND=broadway BROADWAY_DISPLAY=:91 \
    cargo test --features gui --lib home_navigation_replaces -- --ignored --test-threads=1
'
