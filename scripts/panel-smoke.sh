#!/usr/bin/env bash
# Exercise split-first panels without touching the live GUI or session daemon.
set -euo pipefail
cd "$(dirname "$0")/.."
command -v gtk4-broadwayd >/dev/null
cargo build --features gui
export RADAR_TEST_BIN="$PWD/target/debug/radar"
cargo test --features gui --lib panels_split_before_choosing_content --no-run
log=$(mktemp)
export RADAR_PANEL_SMOKE_LOG="$log"
trap 'rm -f "$log"' EXIT
dbus-run-session -- bash -euo pipefail -c '
  gtk4-broadwayd -p 8096 -a 127.0.0.1 :96 >"$RADAR_PANEL_SMOKE_LOG" 2>&1 &
  display=$!
  trap "kill $display 2>/dev/null || true" EXIT
  sleep 1
  env -u WAYLAND_DISPLAY -u DISPLAY -u RADAR_PROJECT_ID -u RADAR_AGENT \
    -u RADAR_CARD_ID -u RADAR_SESSION_ID GDK_BACKEND=broadway BROADWAY_DISPLAY=:96 \
    cargo test --features gui --lib panels_split_before_choosing_content -- --ignored --test-threads=1
'
