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
@define-color radar_hairline alpha(@window_fg_color, 0.085);
@define-color radar_surface alpha(@window_fg_color, 0.045);
@define-color radar_surface_hover alpha(@window_fg_color, 0.085);

/* Let the chosen GTK theme own the canvas; keep its separators understated. */
window {
  color: @window_fg_color;
  background-color: @window_bg_color;
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
.projects-sidebar > separator,
.group-pane separator {
  background-color: @radar_hairline;
}

/* Sidebar: one calm surface, with a softly inset search and compact rows. */
.projects-sidebar {
  background-color: alpha(@window_fg_color, 0.025);
  /* The focus ring recolors this edge exactly like a pane's; the transparent
     border only reserves it, so gaining focus never shifts the layout. */
  border: 1px solid transparent;
}
/* The sidebar header is a strip like the pane headers: no surface of its own,
   content inset to the sidebar's shared 8px, height matched to the search
   field below it. */
.projects-sidebar .group-header {
  min-height: 32px;
  padding: 2px 8px;
  background-color: transparent;
}
/* The search is one of the sidebar's three floating surfaces (search, rows,
   dock): same 8px inset, same control radius. GtkSearchEntry renders through
   its inner `entry` node, so both nodes get the surface treatment. */
.projects-sidebar searchentry,
.projects-sidebar entry {
  margin: 6px 8px;
  min-height: 32px;
  padding: 0 8px;
  border-radius: {control_radius};
  background-color: @radar_surface;
  background-image: none;
  box-shadow: inset 0 0 0 1px @radar_hairline;
}
.projects-sidebar searchentry:focus-within,
.projects-sidebar entry:focus-within {
  box-shadow: inset 0 0 0 1px alpha(@radar_accent, 0.72);
}
.projects-sidebar list {
  padding: 4px 8px 8px;
  background: none;
}
.projects-sidebar list > row {
  padding: 3px 6px;
  margin: 2px 0;
  border-radius: {control_radius};
  transition: background-color 120ms ease;
}
.projects-sidebar list > row > box {
  background: none;
}
.projects-sidebar list > row:hover {
  background-color: @radar_surface_hover;
}
.projects-sidebar list > row:selected {
  color: @window_fg_color;
  background-color: @radar_accent_soft;
}
.projects-sidebar list > row:selected:hover {
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

/* Pane framing is intentionally thin; the running tool remains the focal point.
   Panes take the terminal's own background so header and content read as one
   surface. Roundness follows Hyprland's decoration:rounding, inner surfaces
   tighten. */
.group-pane {
  border: 1px solid @radar_hairline;
  border-radius: {panel_radius};
  background-color: {pane_bg};
}
.group-pane.kbd-focus {
  border-color: alpha(@radar_accent, 0.82);
  box-shadow: 0 0 0 1px alpha(@radar_accent, 0.28);
}
.group-header {
  min-height: 0;
  /* 4px here + 4px chip padding puts header titles at the same 8px inset
     as the sidebar header's title. */
  padding: 3px 4px;
  border-radius: {panel_radius} {panel_radius} 0 0;
  background-color: {pane_bg};
}
.group-header.dragging {
  opacity: 0.55;
}
/* Members read as plain text, not buttons: the active one is accented.
   They stay buttons underneath — drag, hover, and keyboard focus still work.
   Horizontal padding comes from the shared button.flat rule below. */
.group-header button.group-chip {
  min-height: 26px;
  background: none;
  border: none;
  color: alpha(@window_fg_color, 0.6);
}
.group-header button.group-chip:hover {
  color: @window_fg_color;
  background: none;
}
.group-header button.group-chip.active {
  color: @radar_accent;
  background: none;
}
.group-header button.flat,
.projects-sidebar button.flat {
  min-height: 26px;
  min-width: 26px;
  padding: 0 4px;
  border-radius: {control_radius};
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
  border-radius: {panel_radius};
  background-color: alpha(@radar_accent, 0.2);
}

/* The bottom dock is the third floating surface: same inset, same radius,
   its toggles spreading evenly across the full width. */
.dock {
  margin: 0 8px;
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

/* Board: quiet columns, lightly outlined cards, and a single warm claim marker. */
.board-column {
  border: 1px solid @radar_hairline;
  border-radius: {panel_radius_inner};
  background-color: @radar_surface;
  padding: 10px;
}
.board-column.drop-hint {
  border-color: alpha(@radar_accent, 0.65);
  background-color: @radar_accent_soft;
}
.board-card {
  border: 1px solid alpha(@window_fg_color, 0.06);
  border-radius: {control_radius};
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
}
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
.group-pane.kbd-focus {
  box-shadow: inset 0 0 0 1px alpha(@radar_accent, 0.7);
}
.projects-sidebar.kbd-focus {
  border-color: alpha(@radar_accent, 0.82);
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

/// Render the stylesheet for a theme: omarchy's accent and Hyprland's panel
/// rounding in, everything else derived from those.
fn stylesheet(theme: &Theme) -> String {
    let accent = sober(&theme.accent, &theme.foreground, &theme.background);
    let radius = |steps_down: i32| format!("{}px", (theme.panel_radius - steps_down).max(0));
    TEMPLATE
        .replace("{accent}", &hex_color(&accent))
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
    fn radii_scale_down_from_the_panel() {
        let theme = Theme {
            panel_radius: 8,
            ..Theme::default()
        };
        let css = stylesheet(&theme);
        assert!(css.contains(".group-pane {\n  border: 1px solid @radar_hairline;\n  border-radius: 8px;"));
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
        assert!(calm_chroma > raw * 0.3, "sobering must not grey out the accent");
    }
}
