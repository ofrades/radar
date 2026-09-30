//! A quiet, terminal-first visual system.
//!
//! Surfaces derive from the active GTK theme; the accent is omarchy's own
//! theme accent, sobered toward the theme's greys so highlights whisper
//! rather than shout, and pane roundness matches Hyprland's live
//! `decoration:rounding` — the same value omarchy's shell uses for its
//! panels. `refresh()` re-renders the stylesheet when the omarchy theme
//! changes, so a theme switch recolours and reshapes radar live.

use std::cell::RefCell;

use gtk::gdk;

use super::theme::Theme;

// The stylesheet provider, kept so `refresh` can swap its data without
// re-registering it on the display. GTK objects are main-thread only, so a
// thread-local is as global as this needs to be.
thread_local! {
    static PROVIDER: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
}

const TEMPLATE: &str = r#"
@define-color radar_accent {accent};
@define-color radar_accent_soft alpha(@radar_accent, 0.15);
@define-color radar_bg {bg};
@define-color radar_fg {fg};
@define-color radar_muted {muted};
@define-color radar_warning {warning};
@define-color radar_hairline alpha(@radar_fg, 0.085);
@define-color radar_surface alpha(@radar_fg, 0.045);
@define-color radar_surface_hover alpha(@radar_fg, 0.085);

/* The omarchy theme owns the canvas: its background and foreground colour
   every surface below, and its font (the system mono, `omarchy font set`)
   is the app's UI face — omarchy's own shell dresses bar and menus in it.
   Terminals keep the explicit vte font. */
window {
  font-family: "{font_family}";
  color: @radar_fg;
  background-color: @radar_bg;
}
/* One hairline around the whole app, matching the panel separators. */
.app-frame {
  border: 1px solid @radar_hairline;
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
.panel-pane separator {
  background-color: @radar_hairline;
}

/* Pane framing is intentionally thin; the running tool remains the focal point.
   Panes take the terminal's own background so header and content read as one
   surface. Roundness follows Hyprland's decoration:rounding, inner surfaces
   tighten. */
.panel-pane {
  border: 1px solid @radar_hairline;
  border-radius: {panel_radius};
  background-color: {pane_bg};
}
.panel-pane.kbd-focus {
  border-color: alpha(@radar_accent, 0.82);
  box-shadow: 0 0 0 1px alpha(@radar_accent, 0.28);
}
.panel-header {
  min-height: 0;
  /* 4px here + 4px chip padding puts header titles at the same 8px inset
     as the sidebar header's title. */
  padding: 3px 4px;
  border-radius: {panel_radius} {panel_radius} 0 0;
  background-color: {pane_bg};
}
.panel-header.dragging {
  opacity: 0.55;
}
/* Chips are mini panel-headers, not buttons: icon, name, the program's live
   info, a program dropdown and a close each. The switch reads as plain text;
   the active member is accented, and a bell (an agent asking for attention)
   turns its whole chip accent until it is looked at. They stay buttons
   underneath — drag, hover, and keyboard focus still work. */
.panel-header .panel-chip {
  border-radius: {control_radius};
}
.panel-header .panel-chip button.chip-main {
  min-height: 26px;
  background: none;
  border: none;
  color: alpha(@radar_fg, 0.6);
}
.panel-header .panel-chip button.chip-main:hover {
  color: @radar_fg;
  background: none;
}
.panel-header .panel-chip.active button.chip-main {
  color: @radar_accent;
  background: none;
}
.panel-header .panel-chip.attention button.chip-main,
.panel-header .panel-chip.attention label.pane-info {
  color: @radar_accent;
}
/* Live info from the member's program, dim beside its label. */
.panel-header label.pane-info {
  margin: 0 2px;
}
/* A chip's own controls sit quiet until hovered. */
.panel-header .panel-chip button.chip-menu,
.panel-header .panel-chip button.chip-close {
  min-width: 22px;
  min-height: 22px;
  padding: 0;
  border-radius: {control_radius};
  color: alpha(@radar_fg, 0.45);
}
.panel-header .panel-chip button.chip-menu:hover,
.panel-header .panel-chip button.chip-close:hover {
  color: @radar_fg;
  background-color: @radar_surface_hover;
}
.panel-header .panel-chip button.chip-menu:checked {
  color: @radar_fg;
  background-color: @radar_surface_hover;
}
/* The to-do opens its card view: a button that reads as the dim text it
   wraps, with a quiet hover as the only affordance. */
.panel-header .panel-chip button.panel-todo {
  min-height: 0;
  min-width: 0;
  padding: 0;
  background: none;
  border: none;
}
.panel-header .panel-chip button.panel-todo:hover label.pane-info,
.panel-header .panel-chip button.panel-todo:active label.pane-info {
  color: @radar_fg;
}
.panel-header button.flat {
  min-height: 26px;
  min-width: 26px;
  padding: 0 4px;
  border-radius: {control_radius};
}
.panel-pane:drop(active) {
  box-shadow: none;
}
.drop-edge {
  border: 1px solid alpha(@radar_accent, 0.9);
  border-radius: {panel_radius};
  background-color: alpha(@radar_accent, 0.2);
}

/* Workspace-only navigation and tools; Home gets the whole window. */
.workspace-bar {
  padding: 6px 12px;
  border-bottom: 1px solid @radar_hairline;
  background-color: @radar_bg;
}
.dock {
  margin: 0;
  padding: 4px;
  border-radius: {control_radius};
  background-color: @radar_surface;
}
.dock button {
  min-width: 30px;
  min-height: 30px;
  padding: 0;
  border-radius: {control_radius};
}
.dock button:hover {
  background-color: @radar_surface_hover;
}
.dock button:checked {
  color: @radar_accent;
  background-color: @radar_accent_soft;
}

/* Home: the empty state with something to do — brand, setup rows, ways forward. */
.home > image.home-logo {
  opacity: 0.9;
}
/* The setup list reads as one card over the window's canvas, not a surface. */
.home list {
  background: none;
}

/* Home cockpit: a Basecamp-style portfolio. Projects are lanes; each lane
   carries its board counts, its open to-dos and its running sessions. */
.home-cockpit .cockpit-heading {
  margin-top: 6px;
}
/* One project's lane card: header, chips, its to-dos and its sessions. */
.home-cockpit .lane {
  padding: 12px;
  border: 1px solid @radar_hairline;
  border-radius: {panel_radius_inner};
  background-color: alpha(@radar_fg, 0.025);
}
.home-cockpit .lane:hover {
  border-color: alpha(@radar_accent, 0.5);
  background-color: alpha(@radar_fg, 0.04);
}
/* The New-project card that leads the Projects section: a big, dashed, inviting
   button in the same card language as the lanes. */
.home-cockpit button.add-project-card {
  padding: 16px;
  border: 1px dashed alpha(@radar_fg, 0.30);
  border-radius: {panel_radius_inner};
  background: none;
}
.home-cockpit button.add-project-card:hover {
  border-color: alpha(@radar_accent, 0.6);
  background-color: alpha(@radar_fg, 0.035);
}
/* The Add-a-project picker: one card holding the search and its rows. */
.add-project-view .lane {
  padding: 10px 12px;
}
.add-project-view searchentry,
.add-project-view entry {
  margin: 2px 0 6px;
  min-height: 32px;
  padding: 0 8px;
  border-radius: {control_radius};
  background-color: @radar_surface;
  background-image: none;
  box-shadow: inset 0 0 0 1px @radar_hairline;
}
.add-project-view searchentry:focus-within,
.add-project-view entry:focus-within {
  box-shadow: inset 0 0 0 1px alpha(@radar_accent, 0.72);
}
.add-project-view list {
  background: none;
}
.add-project-view list > row {
  padding: 0;
  background: none;
}
.add-project-view .add-row button {
  padding: 7px 8px;
  border-radius: {control_radius};
}
.add-project-view .add-row button:hover {
  background-color: @radar_surface;
}
.add-project-view .add-row-hint {
  color: @radar_accent;
}
/* The to-dos list scroller: flat, and only as tall as its contents allow. */
.home-cockpit .todo-scroll {
  background: none;
  border: none;
}
/* The project view: the board's lanes as side-by-side columns. */
.home-cockpit .project-column {
  padding-top: 8px;
  border-top: 1px solid @radar_hairline;
}
/* Activity signs: one status-dot vocabulary, shared by Home's project lanes,
   session rows, card chips and the workspace pane headers. The dot's colour is
   the state; work in flight and waiting breathe. */
.activity-dot {
  font-size: 0.8em;
}
.activity-dot.sign-needs-you,
.activity-dot.sign-waiting {
  color: @radar_warning;
}
.activity-dot.sign-working {
  color: @radar_accent;
}
.activity-dot.sign-running {
  color: alpha(@radar_accent, 0.6);
}
.activity-dot.sign-idle {
  color: alpha(@radar_fg, 0.45);
}
.activity-dot.sign-stopped {
  color: alpha(@radar_fg, 0.35);
}
.activity-dot.sign-unknown {
  color: alpha(@radar_fg, 0.28);
}
@keyframes radar-pulse {
  0% { opacity: 1; }
  50% { opacity: 0.3; }
  100% { opacity: 1; }
}
.activity-pulse {
  animation-name: radar-pulse;
  animation-duration: 1.8s;
  animation-timing-function: ease-in-out;
  animation-iteration-count: infinite;
}
/* Activity alerts: shadcn-Alert cards floating top-right, above everything.
   A bordered panel with an icon, title, body and its own actions. */
.alert-stack {
  background: none;
}
.activity-alert {
  padding: 12px 10px 12px 14px;
  border: 1px solid @radar_hairline;
  border-radius: 12px;
  background-color: @radar_bg;
  box-shadow: 0 8px 28px alpha(#000000, 0.4);
}
.activity-alert.info {
  border-color: alpha(@radar_fg, 0.2);
}
.activity-alert.success {
  border-color: alpha(@radar_accent, 0.55);
}
.activity-alert.warning {
  border-color: alpha(@radar_warning, 0.6);
}
.activity-alert.danger {
  border-color: alpha(@radar_warning, 0.9);
}
.activity-alert .alert-title {
  font-weight: 700;
}
.activity-alert .alert-body {
  color: alpha(@radar_fg, 0.72);
}
.activity-alert image.alert-icon.info {
  color: alpha(@radar_fg, 0.7);
}
.activity-alert image.alert-icon.success {
  color: @radar_accent;
}
.activity-alert image.alert-icon.warning,
.activity-alert image.alert-icon.danger {
  color: @radar_warning;
}
.activity-alert button.alert-action {
  min-height: 24px;
  padding: 1px 8px;
  border-radius: 99px;
  background-color: alpha(@radar_fg, 0.07);
}
.activity-alert button.alert-action:hover {
  background-color: @radar_surface_hover;
}
.activity-alert button.alert-close {
  min-width: 22px;
  min-height: 22px;
  padding: 0;
  color: alpha(@radar_fg, 0.5);
}
.activity-alert button.alert-close:hover {
  color: @radar_fg;
  background-color: @radar_surface_hover;
}
.home-cockpit button.lane-name {
  padding: 2px 6px;
  font-weight: 700;
  color: @radar_fg;
}
.home-cockpit button.lane-name:hover {
  background-color: @radar_surface_hover;
}
.home-cockpit .lane-pills {
  margin-bottom: 2px;
}
.home-cockpit .pill {
  padding: 1px 7px;
  border-radius: 99px;
  font-size: 0.85em;
  color: @radar_muted;
  background-color: alpha(@radar_fg, 0.07);
}
.home-cockpit .pill-active {
  color: @radar_accent;
  background-color: @radar_accent_soft;
}
.home-cockpit .pill-review {
  color: @radar_warning;
  background-color: alpha(@radar_warning, 0.14);
}
.home-cockpit .pill-done {
  color: alpha(@radar_fg, 0.5);
  background-color: alpha(@radar_fg, 0.04);
}
/* The quiet "To-dos" / "Sessions" mini-heading inside a lane. */
.home-cockpit .lane-section {
  margin-top: 4px;
  color: alpha(@radar_fg, 0.6);
  font-size: 0.85em;
  font-weight: 700;
}
.home-cockpit button.todo {
  padding: 3px 5px;
  border-radius: {control_radius};
}
.home-cockpit button.todo:hover {
  background-color: @radar_surface_hover;
}
.home-cockpit .todo-box {
  border: 1.5px solid alpha(@radar_fg, 0.35);
  border-radius: 4px;
}
.home-cockpit .todo-box.done {
  border-color: @radar_accent;
  background-color: @radar_accent;
}
.home-cockpit label.todo-done {
  color: @radar_muted;
  text-decoration-line: line-through;
}
.home-cockpit label.todo-claim {
  color: @radar_accent;
}
.home-cockpit .lane-foot {
  margin-top: 4px;
  padding-top: 7px;
  border-top: 1px solid @radar_hairline;
}
.home-cockpit button.cockpit-action {
  min-width: 26px;
  min-height: 26px;
  padding: 0 4px;
  color: alpha(@radar_fg, 0.55);
}
.home-cockpit button.cockpit-action:hover {
  color: @radar_fg;
  background-color: @radar_surface_hover;
}
.home-cockpit .attention-card {
  border: 1px solid alpha(@radar_warning, 0.32);
  border-radius: {control_radius};
  background-color: alpha(@radar_warning, 0.06);
}
.home-cockpit .agent-state-dot {
  font-size: 0.8em;
}
/* The running-agents view: one row per live session, grouped by project. */
.home-cockpit button.agent-row {
  padding: 5px 8px;
  border-radius: {control_radius};
}
.home-cockpit button.agent-row:hover {
  background-color: @radar_surface_hover;
}
/* The cockpit header's way into the agents view: a quiet count that reads
   as part of the pulse, not a toolbar button. */
.home-cockpit button.agents-pulse {
  padding: 2px 9px;
  border-radius: 99px;
  font-size: 0.9em;
  color: @radar_accent;
  background-color: @radar_accent_soft;
}
.home-cockpit button.agents-pulse:hover {
  background-color: alpha(@radar_accent, 0.18);
}
.home-cockpit button.todo-tick {
  min-width: 22px;
  min-height: 22px;
  padding: 2px 4px;
  border-radius: {control_radius};
}
.home-cockpit button.todo-tick:hover {
  background-color: @radar_surface_hover;
}
.home-cockpit label.todo-note {
  color: alpha(@radar_fg, 0.55);
}
/* The human's way in: an underlined input under a project's board chips. */
.home-cockpit entry.todo-add {
  padding: 3px 1px;
  min-height: 0;
  background-color: transparent;
  background-image: none;
  border: none;
  border-bottom: 1px solid alpha(@radar_fg, 0.22);
  border-radius: 0;
  box-shadow: none;
}
.home-cockpit entry.todo-add:focus {
  border-bottom-color: @radar_accent;
}

/* Card panel: a card opened as a conversation — thread, reply, controls. */
.card-panel {
  background-color: @radar_bg;
}
.card-panel-title {
  font-size: 1.15em;
  font-weight: 700;
}
.card-panel-body {
  color: alpha(@radar_fg, 0.75);
}
.card-panel .card-thread {
  border-top: 1px solid @radar_hairline;
}
.card-panel .card-panel-id {
  color: @radar_muted;
  font-family: monospace;
  font-size: 0.9em;
}
.card-panel .card-panel-copy {
  min-height: 22px;
  padding: 0 6px;
  font-size: 0.85em;
}
.card-panel .thread-row {
  padding: 6px 8px;
  border-radius: {control_radius};
  background-color: alpha(@radar_fg, 0.04);
}
.card-panel .thread-you {
  background-color: @radar_accent_soft;
}
.card-panel .thread-agent {
  background-color: alpha(@radar_fg, 0.05);
}
.card-panel .thread-author {
  font-weight: 700;
  font-size: 0.9em;
  color: @radar_muted;
}
.card-panel .thread-system {
  padding: 1px 2px;
}
.card-panel .attention-card {
  border: 1px solid alpha(@radar_warning, 0.32);
  border-radius: {control_radius};
  background-color: alpha(@radar_warning, 0.06);
}
.card-panel .error {
  color: @radar_warning;
}
/* The card detail is Home's right-hand rail. */
.card-panel-top {
  padding: 8px 10px;
  border-bottom: 1px solid @radar_hairline;
}
/* Home drill-downs: the board or a card opened inside Home, with Back. */
.home-view {
  background-color: @radar_bg;
}
.home-view-bar {
  padding: 12px 16px 6px;
}
.home-view-bar button {
  min-width: 28px;
  min-height: 28px;
  padding: 0 4px;
}
/* A card's session chip: the agent on it, its state, or a way to start one. */
.home-cockpit button.card-session {
  min-height: 20px;
  padding: 1px 7px;
  border-radius: 99px;
  background-color: alpha(@radar_fg, 0.05);
}
.home-cockpit button.card-session:hover {
  background-color: @radar_surface_hover;
}
.home-cockpit button.card-session label {
  font-size: 0.85em;
}
/* Markdown rendered inside a card. */
.markdown .md-heading {
  font-weight: 700;
}
.markdown .md-h1 {
  font-size: 1.3em;
}
.markdown .md-h2 {
  font-size: 1.15em;
}
.markdown .md-h3 {
  font-size: 1.05em;
}
.markdown .md-code-block {
  padding: 6px 8px;
  border-radius: {control_radius};
  background-color: alpha(@radar_fg, 0.06);
  font-family: monospace;
}
.markdown .md-quote {
  padding-left: 10px;
  border-left: 2px solid alpha(@radar_accent, 0.6);
  color: @radar_muted;
}
.markdown .md-bullet {
  color: @radar_muted;
}

/* Board: quiet columns, lightly outlined cards, and a single warm claim marker. */
/* Text on the accent flips by luminance, so light and dark themes both read. */
.suggested-action {
  color: {accent_text};
  background-color: @radar_accent;
  background-image: none;
  border-color: transparent;
}
.suggested-action:hover {
  background-color: {accent_hover};
}
.suggested-action:active {
  background-color: {accent_active};
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
.panel-pane.kbd-focus {
  box-shadow: inset 0 0 0 1px alpha(@radar_accent, 0.7);
}
/* Menus follow the omarchy menu card: the theme's canvas and panel
   roundness, a quiet border, and rows that highlight like the sidebar's.
   Chip dropdowns and context menus (gtk MenuButton / PopoverMenu) all
   render through `popover`. libadwaita paints the visible card on
   `popover > contents` (and its pointer on `popover > arrow`) over a
   transparent outer node, and resets the popover's font — so both nodes
   are dressed here, and the omarchy face is restored, or every menu would
   float in Adwaita's grey and its own typeface. */
popover.background {
  background-color: transparent;
  font-family: "{font_family}";
}
popover > arrow,
popover > contents {
  background-color: @radar_bg;
  color: @radar_fg;
  background-clip: padding-box;
  border: 1px solid alpha(@radar_fg, 0.12);
  box-shadow: 0 12px 34px alpha(black, 0.5);
}
popover > contents {
  border-radius: {panel_radius};
  padding: 4px;
}
/* A menu's own padding lives on its inner stack; the card keeps the
   hairline and roundness, and the rows sit tight inside it. */
popover.menu > contents {
  padding: 0;
}
popover.menu > contents > stack > box {
  padding: 4px;
}
popover.menu modelbutton {
  min-height: 28px;
  min-width: 0;
  padding: 0 10px;
  border-radius: {control_radius};
  background-color: transparent;
  color: @radar_fg;
}
popover.menu modelbutton:hover,
popover.menu modelbutton:selected {
  background-color: @radar_surface_hover;
}
popover.menu modelbutton:active {
  background-color: @radar_accent_soft;
}
popover.menu separator {
  margin: 4px 6px;
  min-height: 1px;
  background-color: @radar_hairline;
}

.hud-root {
  background: alpha(black, 0.45);
}
.hud-card {
  background-color: @radar_bg;
  border-radius: {panel_radius};
  /* Omarchy's menu cards carry the accent on their edge; radar's floating
     HUD does too, at a whisper. */
  border: 1px solid alpha(@radar_accent, 0.55);
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
  border-radius: {control_radius};
  background: none;
}
.hud-list row:hover {
  background: alpha(currentColor, 0.06);
}
.hud-list row:selected {
  background: @radar_accent_soft;
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

/// Render the stylesheet for a theme: omarchy's palette, font, and Hyprland's
/// panel rounding in, everything else derived from those. No GTK theme colour
/// tokens survive — radar conforms to omarchy, not to Adwaita.
fn stylesheet(theme: &Theme) -> String {
    let accent = sober(&theme.accent, &theme.foreground, &theme.background);
    let radius = |steps_down: i32| format!("{}px", (theme.panel_radius - steps_down).max(0));
    TEMPLATE
        .replace("{accent}", &hex_color(&accent))
        .replace("{bg}", &hex_color(&theme.background))
        .replace("{fg}", &hex_color(&theme.foreground))
        .replace("{muted}", &hex_color(&theme.muted))
        .replace("{warning}", &hex_color(&theme.warning))
        .replace("{font_family}", &theme.font_family)
        .replace("{pane_bg}", &hex_color(&theme.background))
        .replace("{accent_text}", &hex_color(&readable_on(&accent)))
        .replace(
            "{accent_hover}",
            &hex_color(&mix(&accent, &gdk::RGBA::WHITE, 0.14)),
        )
        .replace(
            "{accent_active}",
            &hex_color(&mix(&accent, &gdk::RGBA::BLACK, 0.16)),
        )
        .replace("{panel_radius}", &radius(0))
        .replace("{panel_radius_inner}", &radius(1))
        .replace("{control_radius}", &radius(2))
}

/// Install the stylesheet at startup; call [`refresh`] when the omarchy
/// theme changes.
pub fn install(theme: &Theme) {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_data(&stylesheet(theme));
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    PROVIDER.with(|slot| *slot.borrow_mut() = Some(provider));
}

/// Re-render the stylesheet in place: the omarchy theme (accent, panel
/// roundness) changed. Loading again replaces the provider's whole sheet.
pub fn refresh(theme: &Theme) {
    PROVIDER.with(|slot| {
        if let Some(provider) = slot.borrow().as_ref() {
            provider.load_from_data(&stylesheet(theme));
        }
    });
}

/// `#rrggbb` — a literal colour keeps the generated sheet readable.
fn hex_color(color: &gdk::RGBA) -> String {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        channel(color.red()),
        channel(color.green()),
        channel(color.blue())
    )
}

/// Blend `from` toward `to` by `amount` (0.0 = `from`, 1.0 = `to`).
fn mix(from: &gdk::RGBA, to: &gdk::RGBA, amount: f32) -> gdk::RGBA {
    let blend = |a: f32, b: f32| a + (b - a) * amount;
    gdk::RGBA::new(
        blend(from.red(), to.red()),
        blend(from.green(), to.green()),
        blend(from.blue(), to.blue()),
        1.0,
    )
}

/// How far the theme's accent is sobered before it colours anything: a share
/// toward the foreground (bleeding out saturation) and a share toward the
/// background (losing its punch). One knob for the whole app's temperature.
const SOBER: (f32, f32) = (0.30, 0.25);

/// Pull the accent toward the theme's own greys, so a loud omarchy theme
/// still reads quiet in radar. Every highlight — selection, checked toggles,
/// rings, badges — derives from this single softened colour.
fn sober(accent: &gdk::RGBA, fg: &gdk::RGBA, bg: &gdk::RGBA) -> gdk::RGBA {
    mix(&mix(accent, fg, SOBER.0), bg, SOBER.1)
}

/// Black or white text, whichever reads on `background` (YIQ luminance).
fn readable_on(background: &gdk::RGBA) -> gdk::RGBA {
    let luminance =
        0.299 * background.red() + 0.587 * background.green() + 0.114 * background.blue();
    if luminance > 0.6 {
        gdk::RGBA::parse("#1c1c1c").unwrap_or(gdk::RGBA::BLACK)
    } else {
        gdk::RGBA::parse("#f5f5f5").unwrap_or(gdk::RGBA::WHITE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stylesheet_fills_every_placeholder() {
        let css = stylesheet(&Theme::load());
        for token in [
            "{accent}",
            "{bg}",
            "{fg}",
            "{muted}",
            "{warning}",
            "{font_family}",
            "{pane_bg}",
            "{accent_text}",
            "{accent_hover}",
            "{accent_active}",
            "{panel_radius}",
            "{panel_radius_inner}",
            "{control_radius}",
        ] {
            assert!(!css.contains(token), "unfilled placeholder {token}");
        }
    }

    #[test]
    fn the_sheet_is_omarchy_keyed_not_gtk_keyed() {
        // Every surface takes the omarchy theme's colours; the GTK theme's
        // tokens must not leak into the rendered sheet.
        let css = stylesheet(&Theme::load());
        assert!(!css.contains("@window_bg_color"));
        assert!(!css.contains("@window_fg_color"));
        assert!(!css.contains("@warning_color"));
        assert!(css.contains("@radar_bg"));
        assert!(css.contains("@radar_fg"));
    }

    #[test]
    fn the_ui_wears_the_omarchy_font() {
        let theme = Theme {
            font_family: "JetBrainsMono Nerd Font".to_string(),
            ..Theme::default()
        };
        let css = stylesheet(&theme);
        assert!(css.contains("font-family: \"JetBrainsMono Nerd Font\";"));
    }

    #[test]
    fn menus_and_popovers_follow_the_panel_roundness() {
        let theme = Theme {
            panel_radius: 8,
            ..Theme::default()
        };
        let css = stylesheet(&theme);
        // libadwaita paints the card on `popover > contents`; the panel's
        // roundness must land there, not on the transparent outer node.
        assert!(css.contains("popover > contents {\n  border-radius: 8px;\n  padding: 4px;\n}"));
        // The menu rows highlight like the sidebar's, not Adwaita's grey.
        assert!(css.contains(
            "popover.menu modelbutton:hover,\npopover.menu modelbutton:selected {\n  background-color: @radar_surface_hover;\n}"
        ));
        // Adwaita resets the popover font; the omarchy face is restored.
        assert!(css.contains("font-family: \"monospace\";\n}"));
        // The HUD card rounds with the panel instead of a hardcoded 12px.
        assert!(css.contains(".hud-card {\n  background-color: @radar_bg;\n  border-radius: 8px;"));
    }

    #[test]
    fn radii_scale_down_from_the_panel() {
        let theme = Theme {
            panel_radius: 8,
            ..Theme::default()
        };
        let css = stylesheet(&theme);
        assert!(css.contains(
            ".panel-pane {\n  border: 1px solid @radar_hairline;\n  border-radius: 8px;"
        ));
        assert!(css.contains("border-radius: 8px 8px 0 0;\n  background-color: #111111;"));
        assert!(css.contains("border-radius: 6px;\n}"));
        // Square panels square everything off too, and nothing goes negative.
        let square = Theme {
            panel_radius: 0,
            ..Theme::default()
        };
        let css = stylesheet(&square);
        assert!(css.contains("border-radius: 0px;"));
        assert!(!css.contains("border-radius: -"));
    }

    #[test]
    fn accent_text_flips_by_luminance() {
        let dark_on_light = readable_on(&gdk::RGBA::parse("#7daea3").unwrap());
        let light_on_dark = readable_on(&gdk::RGBA::parse("#20306b").unwrap());
        assert!(dark_on_light.red() < 0.5, "dark text on a light accent");
        assert!(light_on_dark.red() > 0.5, "light text on a dark accent");
    }

    #[test]
    fn hover_and_active_bracket_the_accent() {
        let accent = gdk::RGBA::parse("#7daea3").unwrap();
        let hover = mix(&accent, &gdk::RGBA::WHITE, 0.14);
        let active = mix(&accent, &gdk::RGBA::BLACK, 0.16);
        assert!(hover.red() > accent.red() && hover.blue() > accent.blue());
        assert!(active.red() < accent.red() && active.blue() < accent.blue());
    }

    #[test]
    fn the_accent_is_sobered_toward_the_theme_greys() {
        let accent = gdk::RGBA::parse("#ff4500").unwrap(); // a deliberately loud orange
        let fg = gdk::RGBA::parse("#e8e8e8").unwrap();
        let bg = gdk::RGBA::parse("#111111").unwrap();
        let calm = sober(&accent, &fg, &bg);
        let chroma = |c: &gdk::RGBA| {
            let vals = [c.red(), c.green(), c.blue()];
            let max = vals.iter().cloned().fold(f32::MIN, f32::max);
            let min = vals.iter().cloned().fold(f32::MAX, f32::min);
            max - min
        };
        // Sobering must visibly desaturate, but not erase the hue: the accent
        // should still read as the accent, just quieter.
        let raw = chroma(&accent);
        let calm_chroma = chroma(&calm);
        assert!(calm_chroma < raw * 0.75, "sobering must desaturate");
        assert!(
            calm_chroma > raw * 0.3,
            "sobering must not grey out the accent"
        );
    }
}
