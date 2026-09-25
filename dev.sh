#!/usr/bin/env bash
# Development loop: rebuild on save, restart the app, keep the last error visible.
#
#   ./dev.sh [features]     # default: vte   (use "gui" without vte4)
#
# The app runs with RADAR_HOME=$RADAR_DEV_HOME so your real project list is
# never touched while iterating.

set -uo pipefail
cd "$(dirname "$0")"

FEATURES="${1:-vte}"   # vte4 is installed; use "gui" on a machine without it
export RADAR_HOME="${RADAR_DEV_HOME:-/tmp/opencode/radar-gui-home}"
LOG="${RADAR_DEV_LOG:-/tmp/opencode/radar-dev.log}"
BUILD_LOG=/tmp/opencode/radar-build.log

mkdir -p "$RADAR_HOME"
echo "radar dev loop  |  features=$FEATURES  home=$RADAR_HOME"
echo "log: $LOG"

app_pid=""

stop_app() {
	[[ -n "$app_pid" ]] || return 0
	kill "$app_pid" 2>/dev/null
	for _ in $(seq 1 20); do
		kill -0 "$app_pid" 2>/dev/null || break
		sleep 0.1
	done
	kill -9 "$app_pid" 2>/dev/null
	app_pid=""
}

start_app() {
	stop_app
	setsid nohup ./target/debug/radar gui >"$LOG" 2>&1 &
	app_pid=$!
	sleep 1
	if ! kill -0 "$app_pid" 2>/dev/null; then
		echo "app exited immediately:"
		head -20 "$LOG"
		app_pid=""
		return 1
	fi
	echo "[$(date +%H:%M:%S)] app running (pid $app_pid)"
}

build_and_start() {
	echo "[$(date +%H:%M:%S)] building…"
	if cargo build --features "$FEATURES" >"$BUILD_LOG" 2>&1; then
		echo "[$(date +%H:%M:%S)] build ok"
		start_app
	else
		echo "[$(date +%H:%M:%S)] BUILD FAILED — app left as it was"
		grep -E "^(error|warning: unused)" -A6 "$BUILD_LOG" | head -40
	fi
}

trap 'stop_app; echo; echo "dev loop stopped"; exit 0' INT TERM
build_and_start

# Rebuild on any source change. inotifywait exits on some editors' replace-file
# saves, so the watch is restarted in a loop.
while true; do
	inotifywait -q -r -e close_write,create,delete,move src Cargo.toml 2>/dev/null
	sleep 0.4 # let a burst of writes settle
	while inotifywait -q -t 1 -r -e close_write,create,delete,move src Cargo.toml 2>/dev/null; do :; done
	build_and_start
done
