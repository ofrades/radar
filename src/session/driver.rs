//! The driver: the daemon's own card loop.
//!
//! One lane transition lives here: when a card's worker session ends while
//! the card is still in progress, the driver moves the card to Review and
//! publishes the board change, so finished work always reaches a human
//! review even when the worker never handed it back. The binding is made
//! when the daemon spawns a card-carrying launch (`RADAR_CARD_ID` from
//! `card start`, MCP `card_start` or a resumed worker) and consulted when
//! the session's lifecycle leaves Running.
//!
//! Review dispatch — claiming the finished card for a reviewer agent — is a
//! later card.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;

use crate::session::board_store::BoardState;
use crate::session::daemon::{publish_board_change, Services};
use crate::session::registry::{Lifecycle, Status};

/// How often the driver re-checks its bound sessions.
const TICK: Duration = Duration::from_secs(2);

/// Card bindings for daemon-spawned worker sessions: which card each session
/// was launched for. In-memory on purpose: sessions die with the daemon, so
/// bindings cannot outlive the sessions they name.
#[derive(Clone, Default)]
pub struct Workers {
    bindings: Arc<Mutex<HashMap<String, WorkerCard>>>,
}

#[derive(Clone)]
pub struct WorkerCard {
    pub project_id: i64,
    pub card_id: String,
}

impl Workers {
    /// Remember which card a session was launched for. Launches without a
    /// card are ignored.
    pub fn note(&self, session_id: &str, project_id: i64, card_id: &str) {
        if card_id.is_empty() || session_id.is_empty() {
            return;
        }
        self.bindings.lock().unwrap().insert(
            session_id.to_string(),
            WorkerCard {
                project_id,
                card_id: card_id.to_string(),
            },
        );
    }

    /// The bindings whose sessions are no longer running. The caller owns the
    /// cleanup: forget a binding only after its handoff was handled.
    fn ended(&self, running: &[Status]) -> Vec<(String, WorkerCard)> {
        self.bindings
            .lock()
            .unwrap()
            .iter()
            .filter(|(session_id, _)| {
                running
                    .iter()
                    .find(|status| &status.id == *session_id)
                    .is_none_or(|status| status.lifecycle != Lifecycle::Running)
            })
            .map(|(session_id, worker)| (session_id.clone(), worker.clone()))
            .collect()
    }

    fn forget(&self, session_id: &str) {
        self.bindings.lock().unwrap().remove(session_id);
    }
}

/// Runs until the daemon stops. Every tick reacts to ended worker sessions.
pub(crate) fn run(services: Services) {
    while !services.stopping.load(std::sync::atomic::Ordering::Acquire) {
        std::thread::sleep(TICK);
        if let Err(error) = tick(&services) {
            eprintln!("radar driver: {error:#}");
        }
    }
}

/// Hand every ended worker's card back: claimed, still in progress, worker
/// gone — the card goes to Review. A card whose worker still runs, whose lane
/// is not in progress, or that is done is left alone.
fn tick(services: &Services) -> Result<()> {
    for (session_id, worker) in services.workers.ended(&services.registry.list()) {
        if let Err(error) = hand_off(services, &worker) {
            eprintln!("radar driver: {error:#}");
        }
        services.workers.forget(&session_id);
    }
    Ok(())
}

fn hand_off(services: &Services, worker: &WorkerCard) -> Result<()> {
    let state = services.board.state(worker.project_id)?;
    let Some(card) = state.cards.iter().find(|card| card.id == worker.card_id) else {
        // The card is no longer on the board: nothing to hand back.
        return Ok(());
    };
    if card.done || card.claim.is_none() {
        return Ok(());
    }
    if lane_kind(&state, card.lane_id) != Some("in_progress") {
        return Ok(());
    }
    let review = state
        .lanes
        .iter()
        .find(|lane| lane.kind == "review")
        .map(|lane| lane.name.clone())
        .ok_or_else(|| anyhow::anyhow!("no review lane"))?;
    let change = services
        .board
        .move_card(worker.project_id, &worker.card_id, &review, None)?;
    publish_board_change(
        &services.activity,
        worker.project_id,
        &crate::session::board::command_id("driver-handoff"),
        &change,
    )?;
    Ok(())
}

fn lane_kind(state: &BoardState, lane_id: i64) -> Option<&str> {
    state
        .lanes
        .iter()
        .find(|lane| lane.id == lane_id)
        .map(|lane| lane.kind.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    use crate::session::activity::ActivityJournal;
    use crate::session::agent::AgentHost;
    use crate::session::board_store::{BoardStore, StoredCard};
    use crate::session::catalog::SessionCatalog;
    use crate::session::registry::Registry;
    use parking_lot::Mutex;

    fn services() -> Services {
        Services {
            registry: Arc::new(Registry::default()),
            activity: Arc::new(ActivityJournal::open_in_memory().unwrap()),
            catalog: Arc::new(SessionCatalog::open_in_memory().unwrap()),
            board: Arc::new(BoardStore::open_in_memory().unwrap()),
            agents: Arc::new(AgentHost::default()),
            imports: Arc::new(Mutex::new(HashMap::new())),
            stopping: Arc::new(AtomicBool::new(false)),
            workers: Workers::default(),
        }
    }

    /// A claimed in-progress card and its worker binding, on a board whose
    /// lanes were seeded by the first `state()` read.
    fn bound_card(services: &Services, project_id: i64, session_id: &str) -> StoredCard {
        let _seed = services.board.state(project_id).unwrap();
        let change = services
            .board
            .add_card(project_id, None, "Fix login", "the 302 loop", None)
            .unwrap();
        let card_id = change.card.id.clone();
        services
            .board
            .claim_card(project_id, &card_id, Some("agent-1"), None)
            .unwrap();
        services.workers.note(session_id, project_id, &card_id);
        services
            .board
            .state(project_id)
            .unwrap()
            .cards
            .into_iter()
            .find(|card| card.id == card_id)
            .unwrap()
    }

    fn card_in(state: &BoardState, needle: &str) -> StoredCard {
        state
            .cards
            .iter()
            .find(|card| card.title == needle)
            .unwrap()
            .clone()
    }

    #[test]
    fn an_ended_worker_hands_its_in_progress_card_to_review() {
        let services = services();
        bound_card(&services, 4, "card-s-1");

        tick(&services).unwrap();

        let state = services.board.state(4).unwrap();
        let card = card_in(&state, "Fix login");
        assert_eq!(card.lane, "Review");
        assert!(!card.done);
        // The move drops the claim, exactly as a worker's own handback does:
        // the reviewer's claim, and card_done, are someone else's word.
        assert_eq!(card.claim.as_deref(), None);
        // The handoff is published, so every watcher hears about it.
        let snapshot = services.activity.snapshot(4, None, 50).unwrap();
        assert!(snapshot.events.iter().any(|event| matches!(
            &event.payload,
            crate::session::activity::ActivityPayload::BoardChanged {
                action, column, from_column, ..
            } if action == "moved"
                && column.as_deref() == Some("Review")
                && from_column.as_deref() == Some("In progress")
        )));
        // The binding is spent: a second tick does nothing.
        let sequence = snapshot.watermark;
        tick(&services).unwrap();
        assert_eq!(
            services.activity.snapshot(4, None, 50).unwrap().watermark,
            sequence
        );
    }

    #[test]
    fn a_running_worker_holds_its_card() {
        let services = services();
        let _card = bound_card(&services, 4, "card-s-2");
        // A live session with that id: the driver waits.
        let _ = services
            .registry
            .create(crate::session::registry::Spawn {
                id: "card-s-2".into(),
                argv: vec!["/bin/sleep".into(), "30".into()],
                cwd: std::env::current_dir().unwrap(),
                dims: crate::session::Dims { cols: 80, rows: 24 },
                env: Vec::new(),
                env_remove: Vec::new(),
            })
            .unwrap();

        tick(&services).unwrap();

        let state = services.board.state(4).unwrap();
        assert_eq!(card_in(&state, "Fix login").lane, "In progress");
    }

    #[test]
    fn a_card_already_handed_back_is_not_moved_again() {
        let services = services();
        bound_card(&services, 4, "card-s-3");
        let card = card_in(&services.board.state(4).unwrap(), "Fix login");
        let review = services
            .board
            .state(4)
            .unwrap()
            .lanes
            .iter()
            .find(|lane| lane.kind == "review")
            .unwrap()
            .name
            .clone();
        services
            .board
            .move_card(4, &card.id, &review, None)
            .unwrap();

        tick(&services).unwrap();

        assert_eq!(
            card_in(&services.board.state(4).unwrap(), "Fix login").lane,
            "Review"
        );
    }

    #[test]
    fn a_done_card_is_left_alone() {
        let services = services();
        let card = bound_card(&services, 4, "card-s-4");
        services.board.complete_card(4, &card.id, None).unwrap();

        tick(&services).unwrap();

        assert!(card_in(&services.board.state(4).unwrap(), "Fix login").done);
    }

    #[test]
    fn an_unclaimed_card_is_never_handed_back() {
        let services = services();
        let _seed = services.board.state(4).unwrap();
        let change = services
            .board
            .add_card(4, None, "Fix login", "", None)
            .unwrap();
        services.workers.note("card-s-5", 4, &change.card.id);

        tick(&services).unwrap();

        assert_eq!(
            card_in(&services.board.state(4).unwrap(), "Fix login").lane,
            "Todo"
        );
    }

    #[test]
    fn a_note_without_a_card_is_ignored() {
        let workers = Workers::default();
        workers.note("s", 4, "");
        assert!(workers.ended(&[]).is_empty());
    }

    #[test]
    fn a_binding_dies_with_its_session_by_design() {
        let workers = Workers::default();
        workers.note("s", 4, "c");
    }
}
