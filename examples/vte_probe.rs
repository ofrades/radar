//! Minimal VTE spawn probe: is the timeout our bug or VTE's?
//!
//! Run with: cargo run --example vte_probe --features vte
//!
//! Prints the spawn result for each variant to stdout and shows the terminals in
//! a small window, so both the error and the rendering can be checked at once.
//! Cases A and B are the exact two ways radar has started a program.

use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use vte4::prelude::*;

fn main() {
    let app = gtk::Application::builder()
        .application_id("dev.omarchy.VteProbe")
        .build();

    app.connect_activate(|app| {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("VTE probe")
            .default_width(900)
            .default_height(420)
            .build();
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);

        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "/tmp".to_string());

        // A: what radar does now — /bin/sh -c 'cd "$1"; shift; exec "$@"', no
        // working_directory argument.
        let a = vte4::Terminal::new();
        a.set_vexpand(true);
        column.append(&a);
        {
            let terminal = a.clone();
            let cwd = cwd.clone();
            a.connect_map(move |_| {
                let terminal = terminal.clone();
                let cwd = cwd.clone();
                glib::timeout_add_local_once(Duration::from_millis(50), move || {
                    let argv: Vec<String> = [
                        "/bin/sh",
                        "-c",
                        r#"cd "$1" || exit 126; shift; exec "$@""#,
                        "radar",
                        &cwd,
                        "bash",
                        "-lc",
                        "echo A-OK: $(pwd); hunk --version | head -1; sleep 20",
                    ]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                    println!("A: wrapper, no working_directory");
                    terminal.spawn_async(
                        vte4::PtyFlags::DEFAULT,
                        None,
                        &refs,
                        &["TERM=xterm-256color", "COLORTERM=truecolor"],
                        glib::SpawnFlags::DEFAULT,
                        || {},
                        8_000,
                        None::<&gtk::gio::Cancellable>,
                        |result| match result {
                            Ok(pid) => println!("A ok, pid {pid:?}"),
                            Err(error) => println!("A FAILED: {error}"),
                        },
                    );
                });
            });
        }

        // B: what radar did before — working_directory handed to VTE.
        let b = vte4::Terminal::new();
        b.set_vexpand(true);
        column.append(&b);
        {
            let terminal = b.clone();
            let cwd = cwd.clone();
            b.connect_map(move |_| {
                let terminal = terminal.clone();
                let cwd = cwd.clone();
                glib::timeout_add_local_once(Duration::from_millis(50), move || {
                    let argv = ["bash", "-lc", "echo B-OK: $(pwd); sleep 20"];
                    println!("B: working_directory = Some({cwd})");
                    terminal.spawn_async(
                        vte4::PtyFlags::DEFAULT,
                        Some(&cwd),
                        &argv,
                        &["TERM=xterm-256color"],
                        glib::SpawnFlags::DEFAULT,
                        || {},
                        8_000,
                        None::<&gtk::gio::Cancellable>,
                        |result| match result {
                            Ok(pid) => println!("B ok, pid {pid:?}"),
                            Err(error) => println!("B FAILED: {error}"),
                        },
                    );
                });
            });
        }

        // C: B, but with other threads alive in the process (radar has workers).
        let c = vte4::Terminal::new();
        c.set_vexpand(true);
        column.append(&c);
        {
            let terminal = c.clone();
            let cwd = cwd.clone();
            c.connect_map(move |_| {
                for n in 0..3 {
                    std::thread::spawn(move || {
                        // Busy-ish workers, like the git status and agent fetches.
                        for _ in 0..20 {
                            std::thread::sleep(Duration::from_millis(50));
                            let _ = n;
                        }
                    });
                }
                let terminal = terminal.clone();
                let cwd = cwd.clone();
                glib::timeout_add_local_once(Duration::from_millis(50), move || {
                    let argv = ["bash", "-lc", "echo C-OK; sleep 20"];
                    println!("C: working_directory + worker threads");
                    terminal.spawn_async(
                        vte4::PtyFlags::DEFAULT,
                        Some(&cwd),
                        &argv,
                        &["TERM=xterm-256color"],
                        glib::SpawnFlags::DEFAULT,
                        || {},
                        8_000,
                        None::<&gtk::gio::Cancellable>,
                        |result| match result {
                            Ok(pid) => println!("C ok, pid {pid:?}"),
                            Err(error) => println!("C FAILED: {error}"),
                        },
                    );
                });
            });
        }

        window.set_child(Some(&column));
        window.present();

        glib::timeout_add_seconds_local_once(25, || {
            println!("probe finished");
            std::process::exit(0);
        });
    });

    let _ = app.run_with_args(&["vte_probe"]);
}
