//! Exercise the actual process boundary: the client and daemon do not share a
//! registry, a PTY, or a lifetime. Each test gets its own private state directory.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use radar::session::daemon::{socket_path, Client, Command as Request, Response, VERSION};
use radar::session::registry::{Feedback, Lifecycle, Output, Snapshot, Spawn};
use radar::session::Dims;

struct Daemon {
    home: tempfile::TempDir,
    child: Child,
}

impl Daemon {
    fn start() -> Self {
        let home = tempfile::tempdir().unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_radar"))
            .arg("--home")
            .arg(home.path())
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let daemon = Self { home, child };
        until(|| {
            matches!(
                Client::request(daemon.home.path(), Request::Ping),
                Ok(Response::Hello { version: VERSION })
            )
        });
        daemon
    }

    fn request(&self, request: Request) -> Response {
        Client::request(self.home.path(), request).unwrap()
    }

    fn spawn(&self, script: &str) -> Option<u32> {
        match self.request(Request::Create(Spawn {
            id: "test".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            cwd: self.home.path().to_owned(),
            env: vec![("RADAR_SESSION_TEST".into(), "yes".into())],
            env_remove: vec!["COLORTERM".into()],
            dims: Dims { cols: 80, rows: 24 },
        })) {
            Response::Status(status) => status.pid,
            other => panic!("unexpected response {other:?}"),
        }
    }

    fn attach(&self) -> (Snapshot, Client) {
        let mut client =
            Client::connect(self.home.path(), Request::Attach { id: "test".into() }).unwrap();
        match client.receive().unwrap() {
            Response::Snapshot(snapshot) => (*snapshot, client),
            other => panic!("unexpected response {other:?}"),
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = Client::request(self.home.path(), Request::Shutdown);
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "daemon did not reach expected state"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn text(snapshot: &Snapshot) -> String {
    (0..snapshot.grid.screen_lines())
        .flat_map(|row| {
            (0..snapshot.grid.columns())
                .map(move |col| snapshot.grid[Line(row as i32)][Column(col)].c)
        })
        .collect()
}

#[test]
fn cli_client_exit_does_not_end_session_and_reconnect_streams_in_order() {
    let daemon = Daemon::start();
    let pid = daemon.spawn("printf 'ready:%s:%s' \"$RADAR_SESSION_TEST\" \"${COLORTERM-unset}\"; read line; printf '\\r\\nreceived:%s' \"$line\"");
    until(|| text(&daemon.attach().0).contains("ready:yes:unset"));
    let output = Command::new(env!("CARGO_BIN_EXE_radar"))
        .arg("--home")
        .arg(daemon.home.path())
        .args(["session", "snapshot", "test"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let (snapshot, mut stream) = daemon.attach();
    assert_eq!(snapshot.status.pid, pid);
    assert_eq!(snapshot.status.lifecycle, Lifecycle::Running);
    daemon.request(Request::Input {
        id: "test".into(),
        bytes: b"hello\n".to_vec(),
    });
    let mut sequence = snapshot.sequence;
    let mut bytes = Vec::new();
    loop {
        match stream.receive().unwrap() {
            Response::Output(item) => {
                assert_eq!(item.sequence, sequence + 1);
                sequence = item.sequence;
                match item.event {
                    Output::Bytes(data) => bytes.extend(data),
                    Output::Closed => break,
                    _ => {}
                }
            }
            other => panic!("unexpected response {other:?}"),
        }
    }
    assert!(String::from_utf8_lossy(&bytes).contains("received:hello"));
    assert!(!String::from_utf8_lossy(&bytes).contains("ready:"));
    assert!(!daemon.home.path().join("radar.db").exists());
}

#[test]
fn blocked_socket_cannot_delay_feedback_or_control() {
    let daemon = Daemon::start();
    daemon.spawn("read line; head -c 8000000 /dev/zero; printf '\\007finished'");
    let (_snapshot, mut stalled) = daemon.attach();
    let mut watch =
        Client::connect(daemon.home.path(), Request::Watch { id: "test".into() }).unwrap();
    assert!(matches!(
        watch.receive().unwrap(),
        Response::Watching { .. }
    ));
    daemon.request(Request::Input {
        id: "test".into(),
        bytes: b"go\n".to_vec(),
    });
    let start = Instant::now();
    let mut bell = false;
    loop {
        match watch.receive().unwrap() {
            Response::Feedback(item) => match item.event {
                Feedback::Bell => bell = true,
                Feedback::Lifecycle(Lifecycle::Exited(_)) => break,
                _ => {}
            },
            other => panic!("unexpected response {other:?}"),
        }
    }
    assert!(bell);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(matches!(
        daemon.request(Request::List),
        Response::Sessions(_)
    ));
    // The old output subscription must end explicitly or disconnect. It must
    // never silently skip a frame and resume as if the renderer were current.
    loop {
        match stalled.receive() {
            Ok(Response::ResyncRequired) | Err(_) => break,
            Ok(Response::Output(_)) => {}
            other => panic!("unexpected response {other:?}"),
        }
    }
    assert!(text(&daemon.attach().0).contains("finished"));
}

#[test]
fn singleton_lock_private_socket_and_bad_frames_do_not_replace_daemon() {
    let daemon = Daemon::start();
    let path = socket_path(daemon.home.path());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let duplicate = Command::new(env!("CARGO_BIN_EXE_radar"))
        .arg("--home")
        .arg(daemon.home.path())
        .arg("serve")
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    let mut bad = UnixStream::connect(&path).unwrap();
    bad.write_all(&u32::MAX.to_be_bytes()).unwrap();
    drop(bad);
    assert!(matches!(
        daemon.request(Request::Ping),
        Response::Hello { .. }
    ));
}

#[test]
fn stream_and_watch_finish_when_attaching_to_an_ended_session() {
    let daemon = Daemon::start();
    daemon.spawn("printf done");
    until(
        || matches!(daemon.request(Request::List), Response::Sessions(statuses) if statuses[0].stream_closed && matches!(statuses[0].lifecycle, Lifecycle::Exited(_))),
    );
    for action in ["stream", "watch"] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_radar"))
            .arg("--home")
            .arg(daemon.home.path())
            .args(["session", action, "test"])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "{action} failed: {status}");
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{action} did not finish on an ended session");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
