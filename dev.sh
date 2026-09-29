#!/usr/bin/env bash
# Development loop: rebuild on save, restart the app, keep the last error visible.
#
#   ./dev.sh [features]     # sandbox: RADAR_HOME=$RADAR_DEV_HOME (default
#                           # /tmp/opencode/radar-gui-home), real data untouched
#   ./dev.sh --real [feat]  # restart YOUR installed radar: real ~/.local/share
#                           # and ~/.config state, the installed GUI is quit on
#                           # every rebuild (the session daemon keeps running,
#                           # so agent sessions reattach instead of dying)
#
# `--real` must find the GUI over D-Bus (busctl), since the window hides on
# close while its process holds the dev.omarchy.Radar application name.

set -uo pipefail
cd "$(dirname "$0")"

REAL=0
FEATURES=vte   # vte4 is installed; use "gui" on a machine without it
for arg in "$@"; do
	case "$arg" in
		--real) REAL=1 ;;
		*) FEATURES="$arg" ;;
	esac
done

if [[ "$REAL" == 1 ]]; then
	# Real mode: no RADAR_HOME override — the dev binary reads the same
	# ~/.local/share/radar and ~/.config/radar state the installed app does.
	unset RADAR_HOME
	APP_ID=dev.omarchy.Radar
else
	export RADAR_HOME="${RADAR_DEV_HOME:-/tmp/opencode/radar-gui-home}"
	mkdir -p "$RADAR_HOME"
fi
LOG="${RADAR_DEV_LOG:-/tmp/opencode/radar-dev.log}"
BUILD_LOG=/tmp/opencode/radar-build.log

echo "radar dev loop  |  features=$FEATURES  mode=$([[ $REAL == 1 ]] && echo real || echo sandbox)  home=${RADAR_HOME:-~/.local/share/radar}"
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

# Quit the installed GUI. D-Bus names the one process that holds the
# application; the session daemon and the web service do not hold it, so they
# are never touched.
quit_installed_gui() {
	[[ "$REAL" == 1 ]] || return 0
	local pid
	pid=$(busctl --user call org.freedesktop.DBus /org/freedesktop/DBus \
		org.freedesktop.DBus GetConnectionUnixProcessID s "$APP_ID" \
		2>/dev/null | grep -oE '[0-9]+') || return 0
	[[ -n "$pid" ]] || return 0
	kill "$pid" 2>/dev/null
	for _ in $(seq 1 50); do
		kill -0 "$pid" 2>/dev/null || return 0
		sleep 0.1
	done
	kill -9 "$pid" 2>/dev/null
}

start_app() {
	stop_app
	quit_installed_gui
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
# saves, so the watch is restarted in a loop. web/ is watched too: the client
# is embedded into the binary via include_str!/include_bytes! (src/web.rs).
while true; do
	inotifywait -q -r -e close_write,create,delete,move src web Cargo.toml 2>/dev/null
	sleep 0.4 # let a burst of writes settle
	while inotifywait -q -t 1 -r -e close_write,create,delete,move src web Cargo.toml 2>/dev/null; do :; done
	build_and_start
done
