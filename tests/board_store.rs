//! The board store end-to-end, against a real daemon: the CLI, GUI and guard
//! all talk to the same process boundary, so these exercise it directly.
//!
//! Each test gets its own private state directory. The daemon is the only
//! writer; the client only sends commands and reads snapshots.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use radar::session::activity::{ActivityKind, ActivityPayload, PublishActivity};
use radar::session::board_store::{BoardChange, BoardState};
use radar::session::daemon::{Client, Command as Request, Response, VERSION};

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
            "daemon did not reach the expected state"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn state(daemon: &Daemon, project_id: i64) -> BoardState {
    match daemon.request(Request::BoardState { project_id }) {
        Response::BoardState(state) => state,
        other => panic!("unexpected board state response: {other:?}"),
    }
}

fn change(response: Response) -> BoardChange {
    match response {
        Response::CardChanged(change) => *change,
        other => panic!("unexpected card response: {other:?}"),
    }
}

fn add(daemon: &Daemon, project_id: i64, lane: &str, title: &str) -> BoardChange {
    change(daemon.request(Request::CardAdd {
        project_id,
        lane: Some(lane.into()),
        title: title.into(),
        body: String::new(),
        claim: None,
        command_id: format!("add-{title}"),
    }))
}

#[test]
fn the_card_lifecycle_and_conversation_round_trip() {
    let daemon = Daemon::start();
    let project = daemon.home.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    state(&daemon, 3);

    let added = add(&daemon, 3, "Todo", "Fix login");
    assert_eq!(added.action, "added");
    assert_eq!(added.card.lane, "Todo");
    let card_id = added.card.id.clone();

    // Edit (title and body) keeps the id.
    let edited = change(daemon.request(Request::CardUpdate {
        project_id: 3,
        card_id: card_id.clone(),
        title: Some("Fix login (v2)".into()),
        body: Some("# Notes\n- a body".into()),
        expected_revision: None,
        command_id: "edit-1".into(),
    }));
    assert_eq!(edited.action, "updated");
    assert_eq!(edited.card.id, card_id);
    assert_eq!(edited.card.title, "Fix login (v2)");
    assert!(edited.card.body.contains("a body"));

    // Claim, then move: claiming a Todo card starts it, and the move drops the
    // claim.
    let claimed = change(daemon.request(Request::CardClaim {
        project_id: 3,
        card_id: card_id.clone(),
        claim: Some("claude-1".into()),
        expected_revision: None,
        command_id: "claim-1".into(),
    }));
    assert_eq!(claimed.card.claim.as_deref(), Some("claude-1"));
    assert_eq!(claimed.card.lane, "In progress");
    assert_eq!(claimed.from_lane.as_deref(), Some("Todo"));
    let moved = change(daemon.request(Request::CardMove {
        project_id: 3,
        card_id: card_id.clone(),
        lane: "Review".into(),
        expected_revision: None,
        command_id: "move-1".into(),
    }));
    assert_eq!(moved.card.lane, "Review");
    assert_eq!(moved.card.claim, None);

    // Conversation: an agent comment and a human comment on the card.
    for (text, session) in [
        ("agent: picked it up", Some("session-x")),
        ("human: keep going", None),
    ] {
        match daemon.request(Request::PublishActivity(PublishActivity {
            project_id: 3,
            command_id: format!("comment-{text}"),
            session_id: session.map(str::to_string),
            card_id: Some(card_id.clone()),
            kind: ActivityKind::Reported,
            payload: ActivityPayload::Message { text: text.into() },
        })) {
            Response::ActivityPublished(_) => {}
            other => panic!("unexpected publish response: {other:?}"),
        }
    }
    let snapshot = match daemon.request(Request::ActivitySnapshot {
        project_id: 3,
        after_sequence: None,
        limit: 50,
    }) {
        Response::ActivitySnapshot(snapshot) => snapshot,
        other => panic!("unexpected snapshot response: {other:?}"),
    };
    let messages: Vec<(&str, bool)> = snapshot
        .events
        .iter()
        .filter(|event| event.card_id.as_deref() == Some(card_id.as_str()))
        .filter_map(|event| match &event.payload {
            ActivityPayload::Message { text } => Some((text.as_str(), event.session_id.is_some())),
            _ => None,
        })
        .collect();
    assert!(messages
        .iter()
        .any(|(t, agent)| *t == "agent: picked it up" && *agent));
    assert!(messages
        .iter()
        .any(|(t, agent)| *t == "human: keep going" && !*agent));

    // Complete then reopen: done follows the lane, and reopening leaves Done.
    let done = change(daemon.request(Request::CardComplete {
        project_id: 3,
        card_id: card_id.clone(),
        expected_revision: None,
        command_id: "done-1".into(),
    }));
    assert!(done.card.done);
    assert_eq!(done.card.lane, "Done");
    let reopened = change(daemon.request(Request::CardReopen {
        project_id: 3,
        card_id,
        expected_revision: None,
        command_id: "reopen-1".into(),
    }));
    assert!(!reopened.card.done);
    assert_ne!(reopened.card.lane, "Done");
}

#[test]
fn a_stale_revision_is_refused_and_nothing_is_written() {
    let daemon = Daemon::start();
    let project = daemon.home.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    state(&daemon, 5);
    let card = add(&daemon, 5, "Todo", "Task").card;

    // A fresh revision succeeds.
    change(daemon.request(Request::CardUpdate {
        project_id: 5,
        card_id: card.id.clone(),
        title: Some("Renamed".into()),
        body: None,
        expected_revision: Some(card.revision),
        command_id: "edit-ok".into(),
    }));

    // The now-stale revision is refused (surfaced as a request error).
    let error = Client::request(
        daemon.home.path(),
        Request::CardUpdate {
            project_id: 5,
            card_id: card.id.clone(),
            title: Some("Again".into()),
            body: None,
            expected_revision: Some(card.revision),
            command_id: "edit-stale".into(),
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("changed"), "{error}");

    let board = state(&daemon, 5);
    assert_eq!(board.cards[0].title, "Renamed");
}

#[test]
fn the_board_survives_a_daemon_restart() {
    let mut daemon = Daemon::start();
    let project = daemon.home.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    state(&daemon, 9);
    add(&daemon, 9, "Todo", "Persisted");
    let moved = add(&daemon, 9, "In progress", "Also here");
    change(daemon.request(Request::CardMove {
        project_id: 9,
        card_id: moved.card.id.clone(),
        lane: "Review".into(),
        expected_revision: None,
        command_id: "move".into(),
    }));

    daemon.restart();

    let board = state(&daemon, 9);
    assert_eq!(board.cards.len(), 2);
    let also = board.cards.iter().find(|c| c.title == "Also here").unwrap();
    assert_eq!(also.lane, "Review");
    // The schema migrated and stamped itself.
    assert!(board.cards.iter().any(|c| c.title == "Persisted"));
}
