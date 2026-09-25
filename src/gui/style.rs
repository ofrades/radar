//! Compact styling.
//!
//! libadwaita's defaults are generous: a tall tab strip, roomy entries, a header
//! bar. radar is a tool you keep open all day next to a terminal, so it asks for
//! tighter furniture — without fighting the theme (colours still come from
//! omarchy, only spacing and a few paddings change).

const COMPACT: &str = r#"
/* Tab strip: the default is ~40px tall, which is a whole line of code. */
tabbar > tabbox {
  min-height: 22px;
  padding: 0;
}
tabbar > tabbox > tab {
  min-height: 22px;
  padding: 1px 8px;
}
tabbar > tabbox > tab > button {
  min-height: 18px;
  min-width: 18px;
  padding: 0;
  margin: 0 2px;
}
tabbar {
  padding: 0 4px;
  border-bottom: 1px solid alpha(currentColor, 0.08);
}
tabbar > tabbox > tab > box {
  margin: 0;
}

/* Sidebar: header, filter box, project rows. Rows are inset rounded pills, so
   hover and selection read as cards; row actions stay hidden until the pointer
   or the keyboard finds them. */
.projects-sidebar {
  background: none;
}
.projects-sidebar searchentry {
  margin: 4px 8px 6px;
  min-height: 26px;
  padding: 0 8px;
  border-radius: 8px;
}
.projects-sidebar list {
  padding: 2px 6px 8px;
}
.projects-sidebar row {
  padding: 3px 4px;
  margin: 1px 0;
  border-radius: 8px;
}
.projects-sidebar row > box {
  min-height: 34px;
}

/* Leading icon: quiet normally, loud when the directory has vanished. */
.projects-sidebar row .row-icon {
  color: alpha(currentColor, 0.55);
}
.projects-sidebar row:selected .row-icon {
  color: alpha(currentColor, 0.95);
}
.projects-sidebar row .row-icon.missing {
  color: @warning_color;
}
.projects-sidebar row .pin-icon {
  color: alpha(currentColor, 0.45);
}

/* The changed-file count: a small pill that reads at a glance. */
.projects-sidebar row .badge {
  min-width: 12px;
  padding: 1px 6px;
  border-radius: 999px;
  font-size: 0.85em;
  font-weight: 700;
  color: @accent_color;
  background: alpha(@accent_bg_color, 0.18);
}

/* Trash button appears only while the row is hovered or focused. */
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

/* The "no projects" hint lives inside the list, but must not look like a row. */
.projects-sidebar row.sidebar-empty {
  background: none;
}

.projects-sidebar button.flat {
  min-height: 22px;
  min-width: 22px;
  padding: 0 4px;
}

/* Pane headers: small icons, no wasted height. */
.pane-strip button {
  min-height: 22px;
  min-width: 22px;
  padding: 0 3px;
}

/* Toast text, only so it stays on one line. */
toast {
  font-size: 0.9em;
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
