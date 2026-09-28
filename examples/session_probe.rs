//! Session probe: does the VTE widget work as a pure renderer?
//!
//! Run with: cargo run --example session_probe --features vte
//!
//! One pane, one session behind it, the widget given the bridge pty.
//! Prints the evidence to stdout and closes itself:
//! - the widget parsed the stream (its cursor moved off 0,0)
//! - the mirror sized the program's tty to the widget's grid
//! - the authoritative state saw the banner too

use std::io::Write as _;
use std::os::fd::FromRawFd;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use vte4::prelude::*;

fn main() {
    let app = gtk::Application::builder()
        .application_id("dev.omarchy.SessionProbe")
        .build();

    app.connect_activate(|app| {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Session probe")
            .default_width(900)
            .default_height(420)
            .build();
        let terminal = vte4::Terminal::new();
        terminal.set_vexpand(true);
        terminal.set_hexpand(true);
        window.set_child(Some(&terminal));

        let (sender, receiver) = async_channel::unbounded::<radar::session::SessionEvent>();
        glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = receiver.recv().await {
                if let radar::session::SessionEvent::Exit(info) = event {
                    println!("exit: code {} signal {:?}", info.code, info.signal);
                }
            }
        });

        let terminal_for_map = terminal.clone();
        terminal_for_map.connect_map(move |_| {
            let terminal = terminal.clone();
            let sender = sender.clone();
            glib::timeout_add_local_once(Duration::from_millis(50), move || {
                let banner = "SESSION-PROBE-BANNER-0123456789";
                let argv: Vec<String> = [
                    "bash",
                    "-lc",
                    &format!("echo {banner}; echo \"stty says: $(stty size)\"; sleep 20"),
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();
                let session = radar::session::Session::spawn(
                    &argv,
                    &[],
                    std::env::current_dir().unwrap().as_path(),
                    radar::session::Dims { cols: 80, rows: 24 },
                    move |event| {
                        let _ = sender.try_send(event);
                    },
                );
                match session {
                    Ok(session) => {
                        println!("tty of the program: {:?}", session.tty_name());
                        // A separate descriptor for the widget, the way
                        // the pane does it.
                        let fd = unsafe {
                            std::os::fd::OwnedFd::from_raw_fd(libc::dup(session.client_fd()))
                        };
                        match vte4::Pty::foreign_sync(fd, None::<&gtk::gio::Cancellable>) {
                            Ok(pty) => {
                                terminal.set_pty(Some(&pty));
                                println!("widget has the bridge pty");
                            }
                            Err(error) => println!("FOREIGN PTY FAILED: {error}"),
                        }
                        glib::timeout_add_seconds_local_once(3, move || {
                            let columns = terminal.column_count();
                            let rows = terminal.row_count();
                            let (cursor_col, cursor_row) = terminal.cursor_position();
                            println!(
                                "widget grid: {columns}x{rows}; cursor at {cursor_col}:{cursor_row}"
                            );
                            println!(
                                "widget parsed the stream? {}",
                                cursor_col > 0 || cursor_row > 0
                            );
                            // The authoritative state must have parsed
                            // the same stream — the banner on row 0,
                            // and the program's own stty report on row
                            // 1, which carries the size the mirror set.
                            let term = session.screen();
                            let row = |line: i32| {
                                (0..columns.min(100) as usize)
                                    .map(|c| {
                                        term.grid()[alacritty_terminal::index::Point {
                                            line: alacritty_terminal::index::Line(line),
                                            column: alacritty_terminal::index::Column(c),
                                        }]
                                        .c
                                    })
                                    .collect::<String>()
                            };
                            println!("state row 0: {:?}", row(0));
                            println!("state row 1: {:?}", row(1));
                            println!("probe finished");
                            std::process::exit(0);
                        });
                    }
                    Err(error) => {
                        println!("SESSION SPAWN FAILED: {error}");
                        std::process::exit(1);
                    }
                }
            });
        });

        window.present();
        std::io::stdout().flush().ok();
    });

    let _ = app.run_with_args(&["session_probe"]);
}
