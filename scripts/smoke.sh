#!/usr/bin/env bash
# Headless smoke for radar's GUI: build, run the app offscreen (broadway), and
# screenshot Home's board — the render path `cargo test` cannot cover — plus an
# optional check that a card question reaches the desktop notification bus.
#
#   ./scripts/smoke.sh              # render Home's board; print the screenshot path
#   ./scripts/smoke.sh --notify     # also ask a card question and assert the GUI
#                                   # emits a desktop Notify
#
# Everything runs against a scratch RADAR_HOME and a private D-Bus session, so
# your real state, running app and desktop are untouched. Requires:
# gtk4-broadwayd, chromium, git, python3.

set -euo pipefail
cd "$(dirname "$0")/.."

NOTIFY=0
for arg in "$@"; do
	case "$arg" in
	--notify) NOTIFY=1 ;;
	*) echo "unknown option: $arg" >&2; exit 2 ;;
	esac
done

for tool in gtk4-broadwayd chromium git python3; do
	command -v "$tool" >/dev/null || { echo "missing: $tool" >&2; exit 1; }
done

BASE="${RADAR_SMOKE_DIR:-/tmp/opencode/radar-smoke}"
RADAR_BIN="$PWD/target/debug/radar"
rm -rf "$BASE"
mkdir -p "$BASE/home" "$BASE/demo"
(
	cd "$BASE/demo"
	git init -q
	cat > BOARD.md <<'EOF'
# Board — demo

## Backlog
- [ ] A first to-do
## In progress
- [ ] Smoke the board
## Review
## Done
EOF
)

echo "building (vte)…"
cargo build --features vte >/dev/null 2>&1
"$RADAR_BIN" --home "$BASE/home" add "$BASE/demo" >/dev/null

export SMOKE_HOME="$BASE/home"
export SMOKE_BIN="$RADAR_BIN"
export SMOKE_PROJECT
SMOKE_PROJECT=$("$RADAR_BIN" --home "$SMOKE_HOME" list --json \
	| python3 -c 'import sys,json; print(json.load(sys.stdin)[0]["id"])')
export SMOKE_CARD
SMOKE_CARD=$("$RADAR_BIN" --home "$SMOKE_HOME" card show --path "$BASE/demo" "Smoke the board" --json \
	| python3 -c 'import sys,json; print(json.load(sys.stdin)["card"]["id"])')
export SMOKE_NOTIFY="$NOTIFY"
export SMOKE_DISPLAY=90
export SMOKE_PORT=8090
export SMOKE_BASE="$BASE"
echo "scratch project $SMOKE_PROJECT, card $SMOKE_CARD"

# The GUI must be the primary instance on its bus, so run it (and the notify
# monitor) in one private session; broadway serves the pixels over TCP, which
# the outer chromium reaches directly.
cat >"$BASE/inner.sh" <<'INNER'
set -u
gtk4-broadwayd -p "$SMOKE_PORT" -a 127.0.0.1 ":$SMOKE_DISPLAY" >"$SMOKE_BASE/broadway.log" 2>&1 &
BW=$!
sleep 1
env -u WAYLAND_DISPLAY -u DISPLAY GDK_BACKEND=broadway BROADWAY_DISPLAY=":$SMOKE_DISPLAY" \
	RADAR_HOME="$SMOKE_HOME" RADAR_HOME_PANEL=1 RADAR_OPEN_PROJECT="$SMOKE_PROJECT" \
	"$SMOKE_BIN" --home "$SMOKE_HOME" gui >"$SMOKE_BASE/gui.log" 2>&1 &
GUI=$!
trap 'kill $GUI $BW 2>/dev/null' EXIT
sleep 8

chromium --headless=new --no-sandbox --disable-gpu --hide-scrollbars \
	--window-size=1500,900 --virtual-time-budget=25000 \
	--screenshot="$SMOKE_BASE/board.png" "http://127.0.0.1:$SMOKE_PORT/" \
	>"$SMOKE_BASE/chromium.log" 2>&1 || true

if [[ "$SMOKE_NOTIFY" == 1 ]]; then
	timeout 8 dbus-monitor --session \
		"type='method_call',interface='org.freedesktop.Notifications',member='Notify'" \
		>"$SMOKE_BASE/notify.log" 2>&1 &
	MON=$!
	sleep 1
	"$SMOKE_BIN" --home "$SMOKE_HOME" activity request --project-id "$SMOKE_PROJECT" \
		--card-id "$SMOKE_CARD" --kind question --reason "smoke: is this notified?" \
		--allow answer >/dev/null 2>&1 || true
	sleep 4
	kill "$MON" 2>/dev/null || true
fi
INNER

dbus-run-session -- bash "$BASE/inner.sh"

echo
if grep -qiE "panic|Gtk-CRITICAL|parsing error" "$BASE/gui.log"; then
	echo "gui log has errors:"
	grep -iE "panic|Gtk-CRITICAL|parsing error" "$BASE/gui.log" | head
else
	echo "gui log clean"
fi
echo "screenshot: $BASE/board.png"

if [[ "$NOTIFY" == 1 ]]; then
	if grep -q "member=Notify" "$BASE/notify.log"; then
		echo "notification: OK (Notify seen on the private session bus)"
	else
		echo "notification: NOT seen"
	fi
fi
