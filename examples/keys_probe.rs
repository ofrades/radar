//! What does VTE actually send for Shift+Enter?
//!
//! Run with: cargo run --example keys_probe --features vte
//!
//! The terminal runs `cat -v`, which prints whatever it receives with control
//! characters made visible. Type Shift+Enter at it (or drive it with wtype) and
//! read the window: `^M` means VTE cannot tell Shift+Enter from Enter,
//! `^[[27;2;13~` is xterm's modifyOtherKeys, `^[[13;2u` is the kitty form.

use adw::prelude::*;
use gtk::glib;
use vte4::prelude::*;

fn main() {
    let app = gtk::Application::builder()
        .application_id("dev.omarchy.KeysProbe")
        .build();

    app.connect_activate(|app| {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("keys probe")
            .default_width(900)
            .default_height(300)
            .build();

        let terminal = vte4::Terminal::new();
        terminal.set_vexpand(true);
        window.set_child(Some(&terminal));

        let terminal_for_map = terminal.clone();
        terminal.connect_map(move |_| {
            let terminal = terminal_for_map.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(60), move || {
                let argv = ["bash", "-lc", "echo 'type shift+enter at me:'; cat -v"];
                terminal.spawn_async(
                    vte4::PtyFlags::DEFAULT,
                    None,
                    &argv,
                    &["TERM=xterm-256color"],
                    glib::SpawnFlags::DEFAULT,
                    || {},
                    8_000,
                    None::<&gtk::gio::Cancellable>,
                    |result| match result {
                        Ok(_) => {}
                        Err(error) => println!("spawn failed: {error}"),
                    },
                );
            });
        });

        window.present();
        glib::timeout_add_seconds_local_once(45, || std::process::exit(0));
    });

    let _ = app.run_with_args(&["keys_probe"]);
}
