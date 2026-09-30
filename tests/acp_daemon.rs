//! The daemon drives a real ACP agent process over stdio: updates become
//! project activity, a permission prompt becomes an attention request, and the
//! human's response travels back to the agent. Each test gets its own private
//! state directory and a fixture agent — no model, no network.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use radar::session::activity::{
    ActivityPayload, ActivitySnapshot, Attention, AttentionChange, AttentionResponse,
    ChangeAttention,
};
use radar::session::agent::AgentStart;
use radar::session::daemon::{Client, Command as Request, Response, VERSION};

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

    fn agents(&self) -> Vec<radar::session::agent::AgentStatus> {
        match self.request(Request::AgentList) {
            Response::Agents(list) => list,
            other => panic!("unexpected response {other:?}"),
        }
    }

    fn snapshot(&self, project_id: i64) -> ActivitySnapshot {
        match self.request(Request::ActivitySnapshot {
            project_id,
            after_sequence: None,
            limit: 200,
        }) {
            Response::ActivitySnapshot(snapshot) => snapshot,
            other => panic!("unexpected response {other:?}"),
        }
    }

    fn reported(&self, project_id: i64, needle: &str) -> bool {
        self.snapshot(project_id).events.iter().any(|event| {
            matches!(&event.payload, ActivityPayload::Message { text } if text.contains(needle))
        })
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
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "daemon did not reach expected state"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn fixture() -> String {
    format!(
        "{}/tests/fixtures/acp_fake_agent.py",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn start_fixture(daemon: &Daemon, id: &str, project_id: i64) -> std::path::PathBuf {
    let cwd = daemon.home.path().join(format!("project-{project_id}"));
    std::fs::create_dir_all(&cwd).unwrap();
    let response = daemon.request(Request::AgentStart(AgentStart {
        id: id.to_string(),
        provider: "fake".to_string(),
        program: "python3".to_string(),
        args: vec![fixture()],
        cwd: cwd.clone(),
        project_id,
        session_id: Some(format!("project-{project_id}-agent-0-opencode")),
        card_id: Some("card-acp-1".to_string()),
    }));
    match response {
        Response::AgentStatus(status) => assert_eq!(status.state, "starting"),
        other => panic!("unexpected response {other:?}"),
    }
    cwd
}

fn wait_state(daemon: &Daemon, id: &str, state: &str) {
    until(|| {
        daemon
            .agents()
            .iter()
            .any(|agent| agent.id == id && agent.state == state)
    });
}

fn first_attention(daemon: &Daemon, project_id: i64) -> Attention {
    let mut found = None;
    until(|| {
        found = daemon.snapshot(project_id).attention.into_iter().next();
        found.is_some()
    });
    found.unwrap()
}

#[test]
fn acp_agent_streams_updates_and_round_trips_a_permission() {
    let daemon = Daemon::start();
    let project_id = 7;
    let id = "acp-stream";
    start_fixture(&daemon, id, project_id);
    wait_state(&daemon, id, "ready");

    // The session id the agent assigned is reported back, ready for resume.
    until(|| {
        daemon
            .agents()
            .iter()
            .find(|agent| agent.id == id)
            .and_then(|agent| agent.acp_session_id.clone())
            .as_deref()
            == Some("sess_fake_1")
    });

    daemon.request(Request::AgentPrompt {
        id: id.to_string(),
        text: "hello".to_string(),
    });
    until(|| daemon.reported(project_id, "echo: hello"));
    wait_state(&daemon, id, "ready");

    // A prompt that needs permission becomes a durable approval request; the
    // human's response is delivered to the agent as its permission outcome.
    daemon.request(Request::AgentPrompt {
        id: id.to_string(),
        text: "please ask first".to_string(),
    });
    let attention = first_attention(&daemon, project_id);
    assert_eq!(
        attention.kind,
        radar::session::activity::AttentionKind::Approval
    );
    assert_eq!(attention.card_id.as_deref(), Some("card-acp-1"));
    daemon.request(Request::ChangeAttention(ChangeAttention {
        project_id,
        request_id: attention.id.clone(),
        command_id: "approve-permission".to_string(),
        expected_revision: attention.revision,
        change: AttentionChange::Respond(AttentionResponse::Approve),
    }));
    until(|| daemon.reported(project_id, "(permission: allow)"));

    // Denying picks the reject option instead.
    daemon.request(Request::AgentPrompt {
        id: id.to_string(),
        text: "ask again".to_string(),
    });
    let attention = first_attention(&daemon, project_id);
    daemon.request(Request::ChangeAttention(ChangeAttention {
        project_id,
        request_id: attention.id.clone(),
        command_id: "deny-permission".to_string(),
        expected_revision: attention.revision,
        change: AttentionChange::Respond(AttentionResponse::Deny),
    }));
    until(|| daemon.reported(project_id, "(permission: reject)"));

    daemon.request(Request::AgentStop { id: id.to_string() });
    wait_state(&daemon, id, "exited");
}

/// The CLI is how a human drives an agent, so exercise the real command shape.
#[test]
fn acp_cli_starts_prompts_and_lists_an_agent() {
    let daemon = Daemon::start();
    let project_id = 11;
    let cwd = daemon.home.path().join("project-11");
    std::fs::create_dir_all(&cwd).unwrap();
    let id = "acp-cli";

    let start = Command::new(env!("CARGO_BIN_EXE_radar"))
        .arg("--home")
        .arg(daemon.home.path())
        .args(["acp", "start", id, "--program", "python3", "--arg"])
        .arg(fixture())
        .arg("--cwd")
        .arg(cwd.to_str().unwrap())
        .arg("--project-id")
        .arg(project_id.to_string())
        .output()
        .unwrap();
    assert!(
        start.status.success(),
        "{}",
        String::from_utf8_lossy(&start.stderr)
    );

    let prompt = Command::new(env!("CARGO_BIN_EXE_radar"))
        .arg("--home")
        .arg(daemon.home.path())
        .args(["acp", "prompt", id, "hello cli"])
        .output()
        .unwrap();
    assert!(
        prompt.status.success(),
        "{}",
        String::from_utf8_lossy(&prompt.stderr)
    );
    until(|| daemon.reported(project_id, "echo: hello cli"));

    let list = Command::new(env!("CARGO_BIN_EXE_radar"))
        .arg("--home")
        .arg(daemon.home.path())
        .args(["acp", "list"])
        .output()
        .unwrap();
    assert!(list.status.success());
    let listing: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(listing["Agents"][0]["id"], id);
}
