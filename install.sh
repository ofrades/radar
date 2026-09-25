#!/usr/bin/env bash
# Install radar for the current user: no root needed, everything under ~/.local.
#
#   ./install.sh              # release build, install, register with the launcher
#   ./install.sh --debug      # faster build, for iterating
#   ./install.sh --uninstall  # remove exactly what this script installed
#
# Installs:
#   ~/.local/bin/radar
#   ~/.local/share/applications/radar.desktop
#   ~/.local/share/icons/hicolor/scalable/apps/radar.svg

set -euo pipefail
cd "$(dirname "$0")"

BIN_DIR="${XDG_BIN_HOME:-$HOME/.local/bin}"
APP_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
ICON_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps"
PROFILE="release"

for arg in "$@"; do
	case "$arg" in
	--debug) PROFILE="debug" ;;
	--uninstall)
		rm -f "$BIN_DIR/radar" "$APP_DIR/radar.desktop" "$ICON_DIR/radar.svg"
		echo "removed radar from $BIN_DIR, $APP_DIR and $ICON_DIR"
		exit 0
		;;
	*)
		echo "unknown option: $arg" >&2
		exit 1
		;;
	esac
done

echo "building radar ($PROFILE, with embedded terminals)…"
if [[ $PROFILE == release ]]; then
	cargo build --release --features vte
	SRC=target/release/radar
else
	cargo build --features vte
	SRC=target/debug/radar
fi

install -Dm755 "$SRC" "$BIN_DIR/radar"
install -Dm644 packaging/radar.desktop "$APP_DIR/radar.desktop"
install -Dm644 packaging/radar.svg "$ICON_DIR/radar.svg"

# Make sure the launcher cache notices the new entry.
command -v update-desktop-database >/dev/null && update-desktop-database "$APP_DIR" 2>/dev/null || true
command -v gtk4-update-icon-cache >/dev/null && gtk4-update-icon-cache -f -t "${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor" 2>/dev/null || true

echo
echo "installed:"
echo "  $BIN_DIR/radar"
echo "  $APP_DIR/radar.desktop"
echo "  $ICON_DIR/radar.svg"
echo
case ":$PATH:" in
*":$BIN_DIR:"*) echo "run it:  radar" ;;
*) echo "run it:  $BIN_DIR/radar   (add $BIN_DIR to PATH to just run 'radar')" ;;
esac
echo "or launch \"Radar\" from the menu."
