//! Attach probe: does the VTE widget work as a pure renderer of a daemon session?
//!
//! Run with: cargo run --example attach_probe --features vte
//!
//! Scratch state via RADAR_PROBE_HOME (a temp dir by default); headless via
//! GDK_BACKEND=broadway plus `broadwayd :9`. Prints evidence to stdout and
//! exits 0 on success, nonzero on any failure:
//! - attach-before-create: a conflicting spawn spec reuses the live process
//! - the widget parsed the replay (banner cursor + program title)
//! - keystrokes reach the program through the bridge and echo back
//! - a widget resize is forwarded to the daemon and the program's tty
//! - dropping the attachment detaches; the process keeps running
//! - reattaching replays the latest screen and rediscovers the same PID
//! - the exit notification arrives exactly once when the program ends

use std::cell::RefCell;
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use vte4::prelude::*;

use radar::session::client::{ClientEvent, RemoteSession};
use radar::session::daemon::{Client, Command, Response};
use radar::session::registry::{Lifecycle, Spawn};
use radar::session::Dims;

fn request(home: &std::path::Path, command: Command) -> Response {
    Client::request(home, command).expect("daemon request")
}

/// ensure_running re-executes the current executable, which is this probe, not
/// radar. A probe therefore starts the built server binary itself and returns
/// the child so the caller can reap it after shutdown.
fn start_daemon(home: &std::path::Path) -> Option<std::process::Child> {
    if matches!(
        Client::request(home, Command::Ping),
        Ok(Response::Hello { .. })
    ) {
        return None;
    }
    let server = concat!(env!("CARGO_MANIFEST_DIR"), "/target/debug/radar");
    let child = std::process::Command::new(server)
        .arg("--home")
        .arg(home)
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start radar serve");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if matches!(
            Client::request(home, Command::Ping),
            Ok(Response::Hello { .. })
        ) {
            return Some(child);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    // The daemon never came up: kill and reap before giving up.
    let mut child = child;
    let _ = child.kill();
    let _ = child.wait();
    panic!("session daemon did not start within five seconds");
}

/// Attach is a streaming command; one frame in, then drop the connection.
/// Dropping a subscription never stops the process.
fn session_dims(home: &std::path::Path) -> Dims {
    let mut client = Client::connect(
        home,
        Command::Attach {
            id: "attach-probe".into(),
        },
    )
    .expect("attach for dims");
    match client.receive().expect("snapshot") {
        Response::Snapshot(snapshot) => snapshot.dims,
        other => panic!("unexpected attach response {other:?}"),
    }
}

fn main() {
    let home = std::env::var("RADAR_PROBE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("radar-attach-probe"));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let daemon_child = Rc::new(RefCell::new(start_daemon(&home)));

    let script = "printf '\\033[31mBANNER-ATTACH-PROBE\\033[0m\\r\\n'; \
                  printf '\\033]0;probe-ready\\007'; \
                  while IFS= read -r line; do \
                    printf 'echo:%s\\r\\n\\033]0;echo:%s\\007' \"$line\" \"$line\"; \
                    [ \"$line\" = quit ] && exit 7; \
                  done";
    let argv = vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()];
    let spawn = Spawn {
        id: "attach-probe".into(),
        argv: argv.clone(),
        cwd: std::env::current_dir().unwrap(),
        env: Vec::new(),
        env_remove: Vec::new(),
        dims: Dims { cols: 80, rows: 24 },
    };
    let Response::Status(status) = request(&home, Command::Create(spawn.clone())) else {
        panic!("create failed");
    };
    let original_pid = status.pid.expect("created session has a pid");

    // A probe skips gio::Application: no DBus registration, no single-instance
    // handoff — a plain GTK widget tree on this process's main thread.
    gtk::init().expect("gtk init");
    let window = gtk::Window::new();
    window.set_title(Some("Attach probe"));
    window.set_default_size(640, 320);
    let terminal = vte4::Terminal::new();
    terminal.set_size(80, 24);
    terminal.set_vexpand(true);
    window.set_child(Some(&terminal));
    window.present();

    let (sender, receiver) = async_channel::unbounded::<ClientEvent>();
    let state: Rc<RefCell<Option<RemoteSession>>> = Rc::new(RefCell::new(None));
    let exit_seen = Rc::new(RefCell::new(false));

    let finish: Rc<dyn Fn(bool, String)> = {
        let state = state.clone();
        let home = home.clone();
        let daemon_child = daemon_child.clone();
        Rc::new(move |ok: bool, message: String| {
            let verdict = if ok { "PASS" } else { "FAIL" };
            println!("{verdict}: {message}");
            // Detach before shutdown so the daemon stops a live process cleanly,
            // then reap the server child.
            state.borrow_mut().take();
            let _ = Client::request(&home, Command::Shutdown);
            if let Some(mut child) = daemon_child.borrow_mut().take() {
                let _ = child.wait();
            }
            std::process::exit(if ok { 0 } else { 1 });
        })
    };

    // The exit must be reported exactly once across both attachments, with the
    // program's own code.
    {
        let finish = finish.clone();
        let exit_seen = exit_seen.clone();
        glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = receiver.recv().await {
                match event {
                    ClientEvent::Exit(info) => {
                        let first = !*exit_seen.borrow();
                        *exit_seen.borrow_mut() = true;
                        if !first {
                            finish(false, "duplicate Exit event".into());
                        } else if info.code == 7 {
                            finish(true, "exit reported once with the program's code".into());
                        } else {
                            finish(
                                false,
                                format!("exit carried code {} instead of 7", info.code),
                            );
                        }
                    }
                    ClientEvent::Failed(message) => finish(false, message),
                }
            }
        });
    }

    // Pump the GTK main context until the condition holds; every step gets ten
    // seconds before the probe fails loudly.
    fn pump_until(
        finish: &Rc<dyn Fn(bool, String)>,
        mut condition: impl FnMut() -> bool,
        what: &str,
    ) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let context = glib::MainContext::default();
        while !condition() {
            while context.pending() {
                context.iteration(false);
            }
            if std::time::Instant::now() > deadline {
                finish(false, format!("{what} timed out"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // Attach with a deliberately conflicting spawn spec: same stable ID and
    // argv but a fresh agent stamp, so the live session must win and the
    // registry's conflict error must never recreate it.
    let conflicting = Spawn {
        id: "attach-probe".into(),
        argv: argv.clone(),
        cwd: std::env::current_dir().unwrap(),
        env: vec![("RADAR_AGENT".into(), "probe-new-stamp".into())],
        env_remove: Vec::new(),
        dims: Dims { cols: 80, rows: 24 },
    };
    let remote = RemoteSession::attach(&home, "attach-probe".into(), conflicting, false, {
        let sender = sender.clone();
        move |event| {
            let _ = sender.try_send(event);
        }
    })
    .expect("attach");
    // The PID arrives with the snapshot on the output worker, so give the
    // attachment a moment before asserting the process was reused.
    pump_until(
        &finish,
        || remote.process_id() == Some(original_pid),
        "attachment discovers the process",
    );
    println!("attach-before-create reused the live process {original_pid}");

    let raw_fd = unsafe { libc::dup(remote.client_fd()) };
    assert!(raw_fd >= 0);
    let pty = vte4::Pty::foreign_sync(
        unsafe { std::os::fd::OwnedFd::from_raw_fd(raw_fd) },
        None::<&gtk::gio::Cancellable>,
    )
    .expect("foreign pty");
    terminal.set_pty(Some(&pty));
    state.borrow_mut().replace(remote);

    pump_until(
        &finish,
        {
            let terminal = terminal.clone();
            move || terminal.window_title().as_deref() == Some("probe-ready")
        },
        "replay title",
    );
    let (_, row) = terminal.cursor_position();
    assert!(
        row > 0,
        "widget parsed the replayed banner: cursor at row {row}"
    );
    println!("widget parsed the replay: title probe-ready, cursor row {row}");

    terminal.feed_child(b"ping\n");
    pump_until(
        &finish,
        {
            let terminal = terminal.clone();
            move || terminal.window_title().as_deref() == Some("echo:ping")
        },
        "widget input round trip",
    );
    println!("keystrokes round-trip through the bridge: title echo:ping");

    terminal.set_size(93, 31);
    pump_until(
        &finish,
        {
            let home = home.clone();
            move || session_dims(&home) == Dims { cols: 93, rows: 31 }
        },
        "widget resize forwarded",
    );
    println!("widget resize forwarded: daemon session now 93x31");

    // Detach. The process must keep running under the daemon. The widget keeps
    // its (now stale) pty: libvte segfaults on set_pty(NULL), so production
    // never unsets — it drops the attachment and swaps ptys on relaunch.
    state.borrow_mut().take().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let Response::Sessions(running) = request(&home, Command::List) else {
        panic!("list failed");
    };
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].pid, Some(original_pid));
    assert!(matches!(running[0].lifecycle, Lifecycle::Running));
    println!("detach left the process running with the same pid {original_pid}");

    // Relaunch on the SAME widget: attaching hands the widget a fresh pty in
    // one swap (the fixed start_session path), and the replay restores the
    // latest screen.
    let remote = RemoteSession::attach(
        &home,
        "attach-probe".into(),
        Spawn {
            id: "attach-probe".into(),
            argv: vec!["/bin/false".into()],
            cwd: std::env::current_dir().unwrap(),
            env: Vec::new(),
            env_remove: Vec::new(),
            dims: Dims { cols: 93, rows: 31 },
        },
        false,
        {
            let sender = sender.clone();
            move |event| {
                let _ = sender.try_send(event);
            }
        },
    )
    .expect("reattach");
    pump_until(
        &finish,
        || remote.process_id() == Some(original_pid),
        "reattach discovers the process",
    );
    assert_eq!(
        remote.process_id(),
        Some(original_pid),
        "reattach kept the same process"
    );
    let raw_fd = unsafe { libc::dup(remote.client_fd()) };
    let pty = vte4::Pty::foreign_sync(
        unsafe { std::os::fd::OwnedFd::from_raw_fd(raw_fd) },
        None::<&gtk::gio::Cancellable>,
    )
    .expect("foreign pty 2");
    terminal.set_pty(Some(&pty));
    state.borrow_mut().replace(remote);

    pump_until(
        &finish,
        {
            let terminal = terminal.clone();
            move || terminal.window_title().as_deref() == Some("echo:ping")
        },
        "reattach replay",
    );
    println!("reattach replayed the latest screen: title echo:ping, same pid");

    // End the program through the daemon; the exit must arrive exactly once.
    request(
        &home,
        Command::Input {
            id: "attach-probe".into(),
            bytes: b"quit\n".to_vec(),
        },
    );
    pump_until(&finish, || *exit_seen.borrow(), "exit notification");
}
