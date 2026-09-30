//! Repro probe for the libghostty-vt terminal view's draw path.
//!
//! Restores a snapshot (the path the real GUI uses), moves the cursor, enters
//! and leaves the alternate screen, and scrolls, then quits. A crash here is
//! the draw-path bug.

use gtk::prelude::*;
use radar::ghostty::Terminal;
use radar::gui::term::TerminalView;

fn main() {
    let app = gtk::Application::builder()
        .application_id("org.radar.ghosttyprobe")
        .build();
    app.connect_activate(|app| {
        let view = TerminalView::new("monospace", 14.0);

        let mut source = Terminal::new(80, 24);
        source.write(b"\x1b]0;probe\x07");
        for i in 0..200 {
            source.write(format!("scrollback line {i}\r\n").as_bytes());
        }
        source.write(b"\x1b[5;40H\x1b[1;32mcursor\x1b[0m");
        let snapshot = source.snapshot();
        view.load_snapshot(&snapshot);
        view.feed(b"\x1b[?1049h alt screen with cursor\x1b[?1049l");
        view.feed(b"\x1b[?25l hidden \x1b[?25h");
        view.scroll(-40);
        view.scroll(-40);
        view.scroll_bottom();

        let window = gtk::ApplicationWindow::new(app);
        window.set_child(Some(view.widget()));
        window.set_default_size(800, 480);
        window.present();

        let ticking = view.clone();
        gtk::glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
            ticking.feed(b"tick \x1b[1;36mcyan\x1b[0m\r\n");
            ticking.scroll(-2);
            ticking.scroll_bottom();
            gtk::glib::ControlFlow::Continue
        });
        let pressing = view.clone();
        gtk::glib::timeout_add_local(std::time::Duration::from_millis(300), move || {
            pressing.emit_click_press(120.0, 60.0);
            gtk::glib::ControlFlow::Break
        });
        let quitting = app.clone();
        gtk::glib::timeout_add_local(std::time::Duration::from_secs(5), move || {
            quitting.quit();
            gtk::glib::ControlFlow::Break
        });
    });
    app.run();
}
