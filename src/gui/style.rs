//! A quiet, terminal-first visual system.
//!
//! Superlogical is a useful north star: a near-black canvas, restrained chrome,
//! clear type, and one warm accent. Surfaces still derive from the active GTK
//! theme and terminal colours still come from omarchy; the coral accent gives
//! radar's own controls a consistent identity across themes.

const COMPACT: &str = r#"
@define-color radar_accent #ff7958;
@define-color radar_accent_soft alpha(@radar_accent, 0.15);
@define-color radar_hairline alpha(@window_fg_color, 0.085);
@define-color radar_surface alpha(@window_fg_color, 0.045);
@define-color radar_surface_hover alpha(@window_fg_color, 0.085);

/* Let the chosen GTK theme own the canvas; keep its separators understated. */
window {
  color: @window_fg_color;
  background-color: @window_bg_color;
}
paned > separator {
  background-color: @radar_hairline;
}
paned.divider-focus > separator {
  background-color: alpha(@radar_accent, 0.9);
}
paned > separator:hover {
  background-color: alpha(@radar_accent, 0.55);
}
.projects-sidebar > separator,
.group-pane separator {
  background-color: @radar_hairline;
}

/* Sidebar: one calm surface, with a softly inset search and compact rows. */
.projects-sidebar {
  background-color: alpha(@window_fg_color, 0.025);
}
.projects-sidebar searchentry {
  margin: 6px 9px 8px;
  min-height: 32px;
  padding: 0 9px;
  border-radius: 8px;
  background-color: @radar_surface;
  box-shadow: inset 0 0 0 1px @radar_hairline;
}
.projects-sidebar searchentry:focus-within {
  box-shadow: inset 0 0 0 1px alpha(@radar_accent, 0.72);
}
.projects-sidebar list {
  padding: 4px 7px 8px;
}
.projects-sidebar row {
  padding: 3px 5px;
  margin: 2px 0;
  border-radius: 6px;
  border-left: 2px solid transparent;
  transition: background-color 120ms ease;
}
.projects-sidebar row:hover {
  background-color: @radar_surface_hover;
}
.projects-sidebar row:selected {
  color: @window_fg_color;
  background-color: @radar_accent_soft;
  border-left-color: @radar_accent;
}
.projects-sidebar row:selected:hover {
  background-color: @radar_accent_soft;
}
.projects-sidebar row > box {
  min-height: 36px;
}

/* Icons and metadata stay quiet until they carry useful state. */
.projects-sidebar row .row-icon {
  color: alpha(@window_fg_color, 0.55);
}
.projects-sidebar row:selected .row-icon {
  color: @radar_accent;
}
.projects-sidebar row .row-icon.missing {
  color: @warning_color;
}
.projects-sidebar row .pin-icon {
  color: alpha(@window_fg_color, 0.45);
}
.projects-sidebar row .badge {
  min-width: 12px;
  padding: 1px 6px;
  border-radius: 999px;
  font-size: 0.85em;
  font-weight: 700;
  color: @radar_accent;
  background-color: @radar_accent_soft;
}

/* Pane framing is intentionally thin; the running tool remains the focal point. */
.group-pane {
  border: 1px solid @radar_hairline;
  border-radius: 8px;
  background-color: @window_bg_color;
}
.group-pane.kbd-focus,
.group-pane.pointer-hover {
  border-color: alpha(@radar_accent, 0.82);
  box-shadow: 0 0 0 1px alpha(@radar_accent, 0.28);
}
.group-header {
  min-height: 30px;
  padding: 1px 4px;
  border-radius: 7px;
  background-color: @radar_surface;
}
.group-header.dragging {
  opacity: 0.55;
}
.group-header button.group-chip {
  min-height: 26px;
  padding: 0 8px;
  border-radius: 6px;
  color: alpha(@window_fg_color, 0.66);
}
.group-header button.group-chip:hover {
  color: @window_fg_color;
  background-color: @radar_surface_hover;
}
.group-header button.group-chip.active {
  color: @radar_accent;
  background-color: @radar_accent_soft;
}
.group-header button.flat,
.projects-sidebar button.flat {
  min-height: 26px;
  min-width: 26px;
  padding: 0 4px;
  border-radius: 6px;
}
.group-header.drop-header {
  background-color: @radar_accent_soft;
  color: @radar_accent;
}
.group-pane:drop(active) {
  box-shadow: none;
}
.drop-edge {
  border: 1px solid alpha(@radar_accent, 0.9);
  border-radius: 8px;
  background-color: alpha(@radar_accent, 0.2);
}

/* The bottom dock reads as one small control group; enabled panes use the accent. */
.dock {
  margin: 0 8px;
  padding: 4px;
  border-radius: 10px;
  background-color: @radar_surface;
}
.dock button {
  min-width: 30px;
  min-height: 30px;
  padding: 0;
  border-radius: 7px;
}
.dock button:hover {
  background-color: @radar_surface_hover;
}
.dock button:checked {
  color: @radar_accent;
  background-color: @radar_accent_soft;
}

/* Board: quiet columns, lightly outlined cards, and a single warm claim marker. */
.board-column {
  border: 1px solid @radar_hairline;
  border-radius: 9px;
  background-color: @radar_surface;
  padding: 10px;
}
.board-column.drop-hint {
  border-color: alpha(@radar_accent, 0.65);
  background-color: @radar_accent_soft;
}
.board-card {
  border: 1px solid alpha(@window_fg_color, 0.06);
  border-radius: 6px;
  background-color: alpha(@window_fg_color, 0.055);
  padding: 8px 9px;
}
.board-card:hover {
  border-color: alpha(@radar_accent, 0.35);
  background-color: alpha(@window_fg_color, 0.09);
}
.board-card-done {
  text-decoration: line-through;
  color: alpha(@window_fg_color, 0.55);
}
.board-claim {
  color: @radar_accent;
  font-weight: 700;
  background-color: @radar_accent_soft;
  border-radius: 999px;
  padding: 1px 6px;
}

/* Row actions stay out of the way until needed. */
.projects-sidebar row .row-action {
  opacity: 0;
  min-height: 22px;
  min-width: 22px;
  padding: 0 3px;
  margin-left: 2px;
  transition: opacity 120ms ease;
}
.projects-sidebar row:hover .row-action,
.projects-sidebar row:focus-within .row-action {
  opacity: 1;
}
.projects-sidebar row.sidebar-empty {
  background: none;
  border-left-color: transparent;
}
.suggested-action {
  color: #201611;
  background-color: @radar_accent;
  background-image: none;
  border-color: transparent;
}
.suggested-action:hover {
  background-color: #ff8e72;
}
.suggested-action:active {
  background-color: #ed684a;
}

button:focus,
entry:focus,
searchentry:focus,
textview:focus {
  outline-color: @radar_accent;
}

/* Toast text stays on one line. */
toast {
  font-size: 0.9em;
}

/* Keyboard: the pane (or sidebar) holding the keys wears a quiet ring, and
   the overlay panel floats above the workspace while the keyboard drives. */
.group-pane.kbd-focus {
  box-shadow: inset 0 0 0 1px alpha(@radar_accent, 0.7);
}
.projects-sidebar.kbd-focus {
  box-shadow: inset 0 0 0 1px alpha(@radar_accent, 0.7);
}
.hud-root {
  background: alpha(black, 0.45);
}
.hud-card {
  background-color: @window_bg_color;
  border-radius: 12px;
  padding: 12px 12px 14px;
  box-shadow: 0 12px 40px alpha(black, 0.55);
}
.hud-card .hud-title {
  padding: 0 4px 2px;
}
.hud-filter {
  margin: 8px 2px 6px;
}
.hud-list {
  background: none;
}
.hud-list row {
  border-radius: 8px;
  background: none;
}
.hud-list row:hover {
  background: alpha(currentColor, 0.06);
}
.hud-list row:selected {
  background: alpha(@radar_accent, 0.18);
}
.hud-list row.hud-section {
  padding: 10px 4px 2px;
  background: none;
}
.hud-list row.hud-section label {
  font-size: 0.8em;
  font-weight: 700;
  letter-spacing: 0.05em;
  opacity: 0.55;
}
.hud-list .hud-keys {
  font-family: monospace;
  font-size: 0.85em;
  opacity: 0.8;
}
"#;

/// Install the compact stylesheet for the current display.
pub fn install() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_data(COMPACT);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
