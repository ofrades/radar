//! Exercise the actual process boundary: the client and daemon do not share a
//! registry, a PTY, or a lifetime. Each test gets its own private state directory.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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
        let child = Self::launch(home.path());
        let daemon = Self { home, child };
        until(|| {
            matches!(
                Client::request(daemon.home.path(), Request::Ping),
                Ok(Response::Hello { version: VERSION })
            )
        });
        daemon
    }

    fn launch(home: &Path) -> Child {
        Command::new(env!("CARGO_BIN_EXE_radar"))
            .arg("--home")
            .arg(home)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap()
    }

    fn restart(&mut self) {
        let _ = Client::request(self.home.path(), Request::Shutdown);
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.child = Self::launch(self.home.path());
        until(|| {
            matches!(
                Client::request(self.home.path(), Request::Ping),
                Ok(Response::Hello { version: VERSION })
            )
        });
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
    let terminal = radar::ghostty::Terminal::from_snapshot(&snapshot.terminal_snapshot);
    let mut render = radar::ghostty::render::RenderState::new();
    let frame = render.frame(&terminal);
    let mut text = String::new();
    for row in &frame.lines {
        for cell in &row.cells {
            text.push_str(&cell.text);
        }
        text.push('\n');
    }
    text
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
fn maximum_screen_with_full_history_can_attach_within_client_timeout() {
    let daemon = Daemon::start();
    assert!(matches!(
        daemon.request(Request::Create(Spawn {
            id: "test".into(),
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "awk 'BEGIN { for (i=0; i<11000; i++) printf \"%0499d\\r\\n\", 0 }'; printf '\\033]0;ready\\007'; read line".into(),
            ],
            cwd: daemon.home.path().to_owned(),
            env: Vec::new(),
            env_remove: Vec::new(),
            dims: Dims { cols: 500, rows: 300 },
        })),
        Response::Status(_)
    ));
    until(|| match daemon.request(Request::List) {
        Response::Sessions(sessions) => sessions[0].title.as_deref() == Some("ready"),
        other => panic!("unexpected response {other:?}"),
    });
    // Use the real client's normal timeout: a valid snapshot must remain
    // attachable even with a full scrollback, without extending timeouts.
    let (snapshot, _stream) = daemon.attach();
    assert_eq!(
        snapshot.dims,
        Dims {
            cols: 500,
            rows: 300
        }
    );
    assert!(!snapshot.terminal_snapshot.is_empty());
    // The full-history snapshot decodes into a terminal on its own.
    let _terminal = radar::ghostty::Terminal::from_snapshot(&snapshot.terminal_snapshot);
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
    let mut exited = false;
    let mut closed = false;
    loop {
        match watch.receive().unwrap() {
            Response::Feedback(item) => match item.event {
                Feedback::Bell => bell = true,
                Feedback::Lifecycle(Lifecycle::Exited(_)) => exited = true,
                Feedback::StreamClosed => closed = true,
                _ => {}
            },
            other => panic!("unexpected response {other:?}"),
        }
        if exited && closed {
            break;
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
    let activity_db = path.parent().unwrap().join("activity.sqlite");
    assert_eq!(
        std::fs::metadata(activity_db).unwrap().permissions().mode() & 0o777,
        0o600
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

#[test]
fn daemon_activity_is_replayable_and_attention_commands_are_idempotent() {
    use radar::session::activity::{
        ActivityKind, ActivityPayload, AgentState, AttentionActionKind, AttentionChange,
        AttentionKind, AttentionResponse, ChangeAttention, CreateAttention, PublishActivity,
    };

    let mut daemon = Daemon::start();
    let created = match daemon.request(Request::CreateAttention(CreateAttention {
        project_id: 42,
        command_id: "request-question-1".into(),
        session_id: Some("project-42-agent-0-opencode".into()),
        card_id: Some("card-stable-7".into()),
        kind: AttentionKind::Question,
        reason: "Which deployment target should I use?".into(),
        allowed_actions: vec![AttentionActionKind::Answer, AttentionActionKind::Dismiss],
    })) {
        Response::AttentionCreated(result) => result,
        other => panic!("unexpected response: {other:?}"),
    };
    assert_eq!(created.event.sequence, 1);

    let retried = match daemon.request(Request::CreateAttention(CreateAttention {
        project_id: 42,
        command_id: "request-question-1".into(),
        session_id: Some("project-42-agent-0-opencode".into()),
        card_id: Some("card-stable-7".into()),
        kind: AttentionKind::Question,
        reason: "Which deployment target should I use?".into(),
        allowed_actions: vec![AttentionActionKind::Answer, AttentionActionKind::Dismiss],
    })) {
        Response::AttentionCreated(result) => result,
        other => panic!("unexpected response: {other:?}"),
    };
    assert!(retried.duplicate);
    assert_eq!(retried.attention.id, created.attention.id);

    let mut watcher = Client::connect(
        daemon.home.path(),
        Request::WatchActivity {
            project_id: 42,
            after_sequence: 0,
        },
    )
    .unwrap();
    match watcher.receive().unwrap() {
        Response::ActivityWatching { snapshot, .. } => {
            assert_eq!(snapshot.watermark, 1);
            assert_eq!(snapshot.events, vec![created.event]);
            assert_eq!(snapshot.attention, vec![created.attention.clone()]);
        }
        other => panic!("unexpected response: {other:?}"),
    }

    let state_event = match daemon.request(Request::PublishActivity(PublishActivity {
        project_id: 42,
        command_id: "state-1".into(),
        session_id: Some("project-42-agent-0-opencode".into()),
        card_id: Some("card-stable-7".into()),
        kind: ActivityKind::AgentStateChanged,
        payload: ActivityPayload::AgentState {
            state: AgentState::WaitingForInput,
            message: Some("Need deployment choice".into()),
        },
    })) {
        Response::ActivityPublished(event) => event,
        other => panic!("unexpected response: {other:?}"),
    };
    assert_eq!(state_event.sequence, 2);
    assert!(
        matches!(watcher.receive().unwrap(), Response::Activity(event) if event == state_event)
    );

    let changed = match daemon.request(Request::ChangeAttention(ChangeAttention {
        project_id: 42,
        request_id: created.attention.id.clone(),
        command_id: "answer-1".into(),
        expected_revision: 1,
        change: AttentionChange::Respond(AttentionResponse::Answer(
            "Use the staging target".into(),
        )),
    })) {
        Response::AttentionChanged(result) => result,
        other => panic!("unexpected response: {other:?}"),
    };
    assert!(!changed.attention.is_unresolved());
    assert_eq!(changed.attention.revision, 2);
    assert!(matches!(watcher.receive().unwrap(), Response::Activity(event) if event.sequence == 3));

    let snapshot = match daemon.request(Request::ActivitySnapshot {
        project_id: 42,
        after_sequence: None,
        limit: 20,
    }) {
        Response::ActivitySnapshot(snapshot) => snapshot,
        other => panic!("unexpected response: {other:?}"),
    };
    assert_eq!(snapshot.watermark, 3);
    assert!(snapshot.attention.is_empty());

    drop(watcher);
    let pending = match daemon.request(Request::CreateAttention(CreateAttention {
        project_id: 42,
        command_id: "request-question-2".into(),
        session_id: Some("project-42-agent-0-opencode".into()),
        card_id: Some("card-stable-7".into()),
        kind: AttentionKind::Question,
        reason: "Should I continue with the optional cleanup?".into(),
        allowed_actions: vec![AttentionActionKind::Answer, AttentionActionKind::Dismiss],
    })) {
        Response::AttentionCreated(result) => result,
        other => panic!("unexpected response: {other:?}"),
    };
    let acknowledged = match daemon.request(Request::ChangeAttention(ChangeAttention {
        project_id: 42,
        request_id: pending.attention.id.clone(),
        command_id: "ack-question-2".into(),
        expected_revision: 1,
        change: AttentionChange::Acknowledge,
    })) {
        Response::AttentionChanged(result) => result,
        other => panic!("unexpected response: {other:?}"),
    };
    assert!(acknowledged.attention.is_unresolved());
    assert_eq!(acknowledged.attention.revision, 2);

    daemon.restart();
    let restored = match daemon.request(Request::ActivitySnapshot {
        project_id: 42,
        after_sequence: None,
        limit: 20,
    }) {
        Response::ActivitySnapshot(snapshot) => snapshot,
        other => panic!("unexpected response: {other:?}"),
    };
    assert_eq!(restored.watermark, 5);
    assert_eq!(restored.attention, vec![acknowledged.attention]);

    let resolved = match daemon.request(Request::ChangeAttention(ChangeAttention {
        project_id: 42,
        request_id: pending.attention.id,
        command_id: "answer-question-2".into(),
        expected_revision: 2,
        change: AttentionChange::Respond(AttentionResponse::Answer("Continue with cleanup".into())),
    })) {
        Response::AttentionChanged(result) => result,
        other => panic!("unexpected response: {other:?}"),
    };
    assert_eq!(resolved.event.unwrap().sequence, 6);
    assert!(resolved.attention.resolved_at_millis.is_some());
}

#[test]
fn activity_cli_uses_radar_project_and_session_environment() {
    let daemon = Daemon::start();
    let output = Command::new(env!("CARGO_BIN_EXE_radar"))
        .arg("--home")
        .arg(daemon.home.path())
        .args(["activity", "state", "--state", "waiting-for-approval"])
        .env("RADAR_PROJECT_ID", "73")
        .env("RADAR_SESSION_ID", "project-73-agent-0-claude")
        .env("RADAR_CARD_ID", "card-legacy-stable")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["ActivityPublished"]["project_id"], 73);
    assert_eq!(
        response["ActivityPublished"]["session_id"],
        "project-73-agent-0-claude"
    );
    assert_eq!(
        response["ActivityPublished"]["card_id"],
        "card-legacy-stable"
    );

    let question = Command::new(env!("CARGO_BIN_EXE_radar"))
        .arg("--home")
        .arg(daemon.home.path())
        .args([
            "activity",
            "request",
            "--kind",
            "question",
            "--reason",
            "Use the staging endpoint?",
            "--allow",
            "answer",
            "--allow",
            "dismiss",
            "--command-id",
            "cli-question-1",
        ])
        .env("RADAR_PROJECT_ID", "73")
        .env("RADAR_SESSION_ID", "project-73-agent-0-claude")
        .env("RADAR_CARD_ID", "card-legacy-stable")
        .output()
        .unwrap();
    assert!(
        question.status.success(),
        "{}",
        String::from_utf8_lossy(&question.stderr)
    );
    let question: serde_json::Value = serde_json::from_slice(&question.stdout).unwrap();
    let request_id = question["AttentionCreated"]["attention"]["id"]
        .as_str()
        .unwrap();
    let answered = Command::new(env!("CARGO_BIN_EXE_radar"))
        .arg("--home")
        .arg(daemon.home.path())
        .args([
            "activity",
            "respond",
            request_id,
            "--project-id",
            "73",
            "--revision",
            "1",
            "--action",
            "answer",
            "--answer",
            "Use staging",
            "--command-id",
            "cli-answer-1",
        ])
        .output()
        .unwrap();
    assert!(
        answered.status.success(),
        "{}",
        String::from_utf8_lossy(&answered.stderr)
    );
    let answered: serde_json::Value = serde_json::from_slice(&answered.stdout).unwrap();
    assert_eq!(
        answered["AttentionChanged"]["attention"]["resolution"]["action"],
        "answer"
    );
    assert_eq!(
        answered["AttentionChanged"]["attention"]["resolution"]["value"],
        "Use staging"
    );
    assert!(answered["AttentionChanged"]["attention"]["resolved_at_millis"].is_number());
}

#[test]
fn activity_request_wait_returns_human_approval_to_the_agent_process() {
    use radar::session::activity::{
        ActivityJournal, ActivityKind, ActivityPayload, AttentionChange, AttentionResponse,
        ChangeAttention, PublishActivity,
    };

    let mut daemon = Daemon::start();
    let agent = Command::new(env!("CARGO_BIN_EXE_radar"))
        .arg("--home")
        .arg(daemon.home.path())
        .args([
            "activity",
            "request",
            "--kind",
            "approval",
            "--reason",
            "Deploy this change?",
            "--allow",
            "approve",
            "--allow",
            "deny",
            "--wait",
        ])
        .env("RADAR_PROJECT_ID", "81")
        .env("RADAR_SESSION_ID", "project-81-agent-0-opencode")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let pending = loop {
        let snapshot = match daemon.request(Request::ActivitySnapshot {
            project_id: 81,
            after_sequence: None,
            limit: 20,
        }) {
            Response::ActivitySnapshot(snapshot) => snapshot,
            other => panic!("unexpected response: {other:?}"),
        };
        if let Some(attention) = snapshot.attention.into_iter().next() {
            break attention;
        }
        assert!(
            Instant::now() < deadline,
            "agent did not publish its request"
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    // Stop the daemon while the agent is blocked, then let more than the
    // bounded replay window and the response land before restarting it. The
    // waiter must recover the durable resolved record after ResyncRequired.
    let _ = Client::request(daemon.home.path(), Request::Shutdown);
    let deadline = Instant::now() + Duration::from_secs(5);
    while matches!(daemon.child.try_wait(), Ok(None)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        daemon.child.try_wait().unwrap().is_some(),
        "daemon did not stop"
    );
    let _ = daemon.child.wait();

    let journal = ActivityJournal::open(&daemon.home.path().join("run/activity.sqlite")).unwrap();
    for index in 0..205 {
        journal
            .publish(PublishActivity {
                project_id: 81,
                command_id: format!("after-restart-{index}"),
                session_id: None,
                card_id: None,
                kind: ActivityKind::Reported,
                payload: ActivityPayload::Message {
                    text: format!("event {index}"),
                },
            })
            .unwrap();
    }
    let response = journal
        .change_attention(ChangeAttention {
            project_id: 81,
            request_id: pending.id.clone(),
            command_id: "human-approve-after-outage".into(),
            expected_revision: pending.revision,
            change: AttentionChange::Respond(AttentionResponse::Approve),
        })
        .unwrap();
    drop(journal);
    assert_eq!(
        response.attention.resolution,
        Some(AttentionResponse::Approve)
    );

    daemon.child = Daemon::launch(daemon.home.path());
    until(|| {
        matches!(
            Client::request(daemon.home.path(), Request::Ping),
            Ok(Response::Hello { version: VERSION })
        )
    });

    let response = match daemon.request(Request::AttentionStatus {
        project_id: 81,
        request_id: pending.id.clone(),
    }) {
        Response::AttentionStatus(response) => response,
        other => panic!("unexpected response: {other:?}"),
    };
    assert_eq!(response.resolution, Some(AttentionResponse::Approve));

    let output = agent.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "agent output: {lines:?}");
    assert_eq!(lines[0]["AttentionCreated"]["attention"]["id"], pending.id);
    assert_eq!(
        lines[1]["AttentionStatus"]["resolution"]["action"],
        "approve"
    );
    assert!(lines[1]["AttentionStatus"]["resolved_at_millis"].is_number());
}

#[test]
fn activity_mutation_stays_responsive_with_a_stalled_terminal_renderer() {
    use radar::session::activity::{ActivityKind, ActivityPayload, PublishActivity};

    let daemon = Daemon::start();
    daemon.spawn("yes terminal-flood");
    let mut stalled_output =
        Client::connect(daemon.home.path(), Request::Attach { id: "test".into() }).unwrap();
    assert!(matches!(
        stalled_output.receive().unwrap(),
        Response::Snapshot(_)
    ));
    std::thread::sleep(Duration::from_millis(150));

    let started = Instant::now();
    let response = daemon.request(Request::PublishActivity(PublishActivity {
        project_id: 91,
        command_id: "under-load".into(),
        session_id: Some("project-91-agent-0-opencode".into()),
        card_id: None,
        kind: ActivityKind::Reported,
        payload: ActivityPayload::Message {
            text: "attention path is live".into(),
        },
    }));
    assert!(matches!(response, Response::ActivityPublished(_)));
    assert!(started.elapsed() < Duration::from_secs(1));

    let _ = daemon.request(Request::Stop { id: "test".into() });
    drop(stalled_output);
}

fn catalog_entries(daemon: &Daemon, filter: Request) -> Vec<radar::session::catalog::Entry> {
    match daemon.request(filter) {
        Response::Catalog(entries) => entries,
        other => panic!("unexpected catalog response {other:?}"),
    }
}

fn catalog_list(daemon: &Daemon, filter: radar::session::catalog::CatalogFilter) -> Request {
    Request::CatalogList {
        projects: vec![radar::session::daemon::CatalogProject {
            id: 99,
            path: daemon.home.path().join("project-99"),
        }],
        filter,
        query: None,
        limit: 100,
    }
}

/// A created session lands in the catalog immediately, inherits its
/// terminal's title, and ends when its process does.
#[test]
fn created_sessions_are_cataloged_and_end_with_their_process() {
    use radar::session::catalog::CatalogFilter;

    let daemon = Daemon::start();
    let id = "project-99-agent-0-opencode";
    std::fs::create_dir_all(daemon.home.path().join("project-99")).unwrap();
    match daemon.request(Request::Create(Spawn {
        id: id.into(),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf ready; sleep 60".into(),
        ],
        cwd: daemon.home.path().join("project-99"),
        env: vec![("RADAR_CARD_ID".into(), "todo-99".into())],
        env_remove: Vec::new(),
        dims: Dims { cols: 80, rows: 24 },
    })) {
        Response::Status(_) => {}
        other => panic!("unexpected create response {other:?}"),
    }

    let entries = catalog_entries(&daemon, catalog_list(&daemon, CatalogFilter::Active));
    assert_eq!(entries.len(), 1, "the spawned session is cataloged");
    assert_eq!(entries[0].radar_session_id.as_deref(), Some(id));
    assert_eq!(entries[0].provider, "opencode");
    assert_eq!(entries[0].lifecycle, "running");
    assert_eq!(entries[0].card_id.as_deref(), Some("todo-99"));
    let catalog_id = entries[0].id;

    daemon.request(Request::Stop { id: id.into() });
    until(|| {
        catalog_entries(&daemon, catalog_list(&daemon, CatalogFilter::Active))
            .first()
            .is_some_and(|entry| entry.lifecycle == "ended")
    });

    // Archiving hides it from the active list without deleting it.
    daemon.request(Request::CatalogArchive {
        id: catalog_id,
        archived: true,
    });
    assert!(catalog_entries(&daemon, catalog_list(&daemon, CatalogFilter::Active)).is_empty());
    let archived = catalog_entries(&daemon, catalog_list(&daemon, CatalogFilter::Archived));
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].id, catalog_id);
    assert_eq!(archived[0].card_id.as_deref(), Some("todo-99"));
}

/// Sessions that predate the catalog are backfilled by CatalogSeen; the
/// catalog — unlike the registry — survives a daemon restart, and a row
/// whose session is not live never lingers as running.
#[test]
fn catalog_backfill_survives_a_daemon_restart() {
    use radar::session::catalog::CatalogFilter;

    let mut daemon = Daemon::start();
    daemon.request(Request::CatalogSeen {
        project_id: 99,
        radar_id: "project-99-agent-1-opencode".into(),
        program: "opencode".into(),
        cwd: daemon.home.path().join("project-99"),
    });
    let entries = catalog_entries(&daemon, catalog_list(&daemon, CatalogFilter::All));
    assert_eq!(entries.len(), 1, "the backfilled session is cataloged");
    assert_eq!(
        entries[0].lifecycle, "ended",
        "a backfill for a session the registry has never seen ends immediately"
    );

    daemon.restart();

    let entries = catalog_entries(&daemon, catalog_list(&daemon, CatalogFilter::All));
    assert_eq!(
        entries.len(),
        1,
        "the catalog outlives the daemon that wrote it"
    );
    assert_eq!(
        entries[0].lifecycle, "ended",
        "the row stays ended across the restart"
    );
}

/// When the CLI's own store reveals which conversation a run had, the
/// catalog row adopts it — the sidebar then resumes that exact conversation.
#[test]
fn provider_binding_upgrades_a_session_row() {
    use radar::session::catalog::CatalogFilter;

    let daemon = Daemon::start();
    let id = "project-99-agent-2-opencode";
    std::fs::create_dir_all(daemon.home.path().join("project-99")).unwrap();
    daemon.request(Request::Create(Spawn {
        id: id.into(),
        argv: vec!["/bin/sh".into(), "-c".into(), "sleep 60".into()],
        cwd: daemon.home.path().join("project-99"),
        env: vec![
            ("RADAR_AGENT".into(), "worker".into()),
            ("RADAR_SESSION_PROVIDER".into(), "opencode".into()),
        ],
        env_remove: Vec::new(),
        dims: Dims { cols: 80, rows: 24 },
    }));
    daemon.request(Request::SessionIdentify {
        radar_id: id.into(),
        instance: "worker".into(),
        provider: "opencode".into(),
        conversation: "ses_manual".into(),
        reporter_pid: 0,
    });

    let entries = catalog_entries(&daemon, catalog_list(&daemon, CatalogFilter::Active));
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].provider_session_id, "ses_manual");
    assert_eq!(entries[0].radar_session_id.as_deref(), Some(id));
    let _ = daemon.request(Request::Stop { id: id.into() });
}

#[test]
fn provider_report_and_todo_binding_are_recorded_together() {
    use radar::session::catalog::CatalogFilter;
    let daemon = Daemon::start();
    std::fs::create_dir_all(daemon.home.path().join("project-99")).unwrap();
    for (id, card) in [
        ("project-99-agent-1-opencode", "todo-first"),
        ("project-99-agent-2-opencode", "todo-next"),
    ] {
        daemon.request(Request::Create(Spawn {
            id: id.into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 60".into()],
            cwd: daemon.home.path().join("project-99"),
            env: vec![
                ("RADAR_CARD_ID".into(), card.into()),
                (
                    "RADAR_PROVIDER_SESSION_ID".into(),
                    "ses_unverified_launch".into(),
                ),
                ("RADAR_SESSION_PROVIDER".into(), "opencode".into()),
                ("RADAR_AGENT".into(), id.into()),
            ],
            env_remove: Vec::new(),
            dims: Dims { cols: 80, rows: 24 },
        }));
        daemon.request(Request::SessionIdentify {
            radar_id: id.into(),
            instance: id.into(),
            provider: "opencode".into(),
            conversation: "ses_exact".into(),
            reporter_pid: 0,
        });
        let entries = catalog_entries(&daemon, catalog_list(&daemon, CatalogFilter::All));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].provider_session_id, "ses_exact");
        assert_eq!(entries[0].card_id.as_deref(), Some(card));
        assert_eq!(entries[0].radar_session_id.as_deref(), Some(id));
        let mut query = catalog_list(&daemon, CatalogFilter::All);
        if let Request::CatalogList { query, .. } = &mut query {
            *query = Some(card.to_string());
        }
        assert_eq!(catalog_entries(&daemon, query).len(), 1);
        daemon.request(Request::Stop { id: id.into() });
    }
}

#[test]
fn all_supported_provider_switches_survive_restart_without_guessing_or_losing_history() {
    use radar::session::catalog::CatalogFilter;
    let mut daemon = Daemon::start();
    let cwd = daemon.home.path().join("project-99");
    std::fs::create_dir_all(&cwd).unwrap();
    for provider in radar::programs::agents::SUPPORTED_AGENT_IDS {
        let id = format!("project-99-agent-0-{provider}");
        let response = daemon.request(Request::Create(Spawn {
            id: id.clone(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 60".into()],
            cwd: cwd.clone(),
            env: vec![
                ("RADAR_CARD_ID".into(), "task".into()),
                ("RADAR_AGENT".into(), "worker-one".into()),
                ("RADAR_SESSION_PROVIDER".into(), provider.to_string()),
                ("RADAR_PROVIDER_SESSION_ID".into(), "stale-launch-id".into()),
            ],
            env_remove: Vec::new(),
            dims: Dims { cols: 80, rows: 24 },
        }));
        let Response::Status(status) = response else {
            panic!("expected session")
        };
        let entries = catalog_entries(&daemon, catalog_list(&daemon, CatalogFilter::All));
        assert!(!entries
            .iter()
            .any(|entry| entry.provider_session_id == "stale-launch-id"));
        let report = |conversation: &str, instance: &str, reporter_pid| Request::SessionIdentify {
            radar_id: id.clone(),
            instance: instance.into(),
            provider: provider.to_string(),
            conversation: conversation.into(),
            reporter_pid,
        };
        assert!(Client::request(
            daemon.home.path(),
            report("wrong", "previous-worker", status.pid.unwrap())
        )
        .is_err());
        if matches!(*provider, "pi" | "omp") {
            assert!(Client::request(daemon.home.path(), report("child", "worker-one", 0)).is_err());
        }
        for conversation in ["original", "switched", "original"] {
            daemon.request(report(conversation, "worker-one", status.pid.unwrap()));
        }
        daemon.request(Request::CatalogSeen {
            project_id: 99,
            radar_id: id.clone(),
            program: provider.to_string(),
            cwd: cwd.clone(),
        });
        let entries = catalog_entries(&daemon, catalog_list(&daemon, CatalogFilter::All));
        let rows: Vec<_> = entries
            .iter()
            .filter(|entry| entry.provider == *provider)
            .collect();
        assert_eq!(rows.len(), 2, "{provider}");
        let original = rows
            .iter()
            .find(|entry| entry.provider_session_id == "original")
            .unwrap();
        assert_eq!(original.radar_session_id.as_deref(), Some(id.as_str()));
        assert_eq!(original.card_ids, ["task"]);
        let switched = rows
            .iter()
            .find(|entry| entry.provider_session_id == "switched")
            .unwrap();
        assert_eq!(switched.radar_session_id, None);
        assert_eq!(switched.card_ids, ["task"]);
    }
    daemon.restart();
    let entries = catalog_entries(&daemon, catalog_list(&daemon, CatalogFilter::All));
    assert_eq!(entries.len(), 8);
    assert!(entries
        .iter()
        .all(|entry| entry.lifecycle == "ended" && entry.card_ids == ["task"]));
}

#[test]
fn missing_resume_is_refused_before_any_process_or_empty_transcript_is_created() {
    let daemon = Daemon::start();
    for provider in radar::programs::agents::SUPPORTED_AGENT_IDS {
        let id = format!("project-99-agent-0-{provider}");
        let marker = daemon.home.path().join(format!("{provider}-launched"));
        let result = Client::request(
            daemon.home.path(),
            Request::Create(Spawn {
                id,
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!("touch {}", marker.display()),
                ],
                cwd: daemon.home.path().to_path_buf(),
                env: vec![
                    (
                        "HOME".into(),
                        daemon.home.path().to_string_lossy().into_owned(),
                    ),
                    ("RADAR_SESSION_PROVIDER".into(), provider.to_string()),
                    (
                        "RADAR_RESUME_SESSION_ID".into(),
                        "definitely-missing".into(),
                    ),
                ],
                env_remove: Vec::new(),
                dims: Dims { cols: 80, rows: 24 },
            }),
        );
        assert!(
            result.is_err(),
            "{provider}: missing Resume must fail closed"
        );
        assert!(!marker.exists(), "{provider}: the child must not run");
    }
    let Response::Sessions(sessions) = daemon.request(Request::List) else {
        panic!("expected list")
    };
    assert!(sessions.is_empty());
}

/// The daemon's attach snapshot is a real libghostty-vt snapshot: detach and
/// reattach mid-escape, decode, and the restored terminal continues exactly
/// where a terminal that saw the whole stream would.
#[test]
fn daemon_attach_snapshot_resumes_a_mid_escape_cut_after_reattach() {
    use radar::ghostty::Terminal;

    let daemon = Daemon::start();
    daemon.spawn("printf '\\033]0;ready\\007abc\\033[38;5'; sleep 60");

    // Wait until the daemon has parsed the prefix (the partial SGR follows it
    // in the same write, so "abc" in the grid means the cut is in the parser).
    until(|| {
        let (snapshot, client) = daemon.attach();
        let seen = text(&snapshot).contains("abc");
        drop(client);
        seen
    });

    // Close the attachment, then reopen it while the program keeps running.
    let (first, client) = daemon.attach();
    assert!(
        !first.terminal_snapshot.is_empty(),
        "attach did not carry a snapshot"
    );
    drop(client);
    let (reopened, _client) = daemon.attach();
    assert!(matches!(reopened.status.lifecycle, Lifecycle::Running));

    let bytes = reopened.terminal_snapshot.clone();
    let mut restored = Terminal::from_snapshot(&bytes);
    restored.write(b";196m!");
    let mut reference = Terminal::new(80, 24);
    reference.write(b"\x1b]0;ready\x07abc\x1b[38;5;196m!");

    assert_eq!(
        restored.cursor(),
        reference.cursor(),
        "reattached snapshot diverged from the full stream"
    );
    assert_eq!(restored.title(), "ready");
}
