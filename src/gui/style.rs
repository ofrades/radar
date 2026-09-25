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

/* Sidebar: a filter box and rows, nothing more. */
.projects-sidebar {
  background: none;
}
.projects-sidebar entry,
.projects-sidebar searchentry {
  min-height: 24px;
  padding: 0 6px;
}
.projects-sidebar list,
.projects-sidebar row {
  padding: 0;
  margin: 0;
}
.projects-sidebar row > box {
  min-height: 34px;
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
