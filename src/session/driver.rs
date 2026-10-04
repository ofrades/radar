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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::config::Paths;
use crate::db::Db;
use crate::session::board_store::BoardState;
use crate::session::daemon::{publish_board_change, Services};
use crate::session::registry::{Lifecycle, Status};

/// How often the driver re-checks its bound sessions.
const TICK: Duration = Duration::from_secs(2);
/// How often the PR observer refreshes its cache; reads never spawn gh.
const PR_TICK: Duration = Duration::from_secs(30);
/// How long a running worker may go without its first hook signal before the
/// derived read marks it quiet (AO's no_signal grace).
pub const HOOK_GRACE: Duration = Duration::from_secs(90);

/// Card bindings for daemon-spawned worker sessions: which card each session
/// was launched for, and where it runs. In-memory on purpose: sessions die
/// with the daemon, so bindings cannot outlive the sessions they name.
#[derive(Clone, Default)]
pub struct Workers {
    bindings: Arc<Mutex<HashMap<String, WorkerCard>>>,
    turn_ended: Arc<Mutex<HashSet<String>>>,
    /// Proof per incarnation: when the binding was noted, and when this run
    /// of the session first signalled (identify or turn end). Cleared on
    /// every note: a relaunch must re-prove its hook pipeline.
    receipts: Arc<Mutex<HashMap<String, Receipt>>>,
}

/// The hook-pipeline receipt for one worker incarnation.
#[derive(Debug, Clone, Copy)]
pub struct Receipt {
    pub noted_at: Instant,
    pub first_signal: Option<Instant>,
}

impl Receipt {
    /// Unproven for longer than the grace window: the operator should look.
    pub fn is_quiet(&self, grace: Duration, now: Instant) -> bool {
        self.first_signal.is_none() && now.duration_since(self.noted_at) > grace
    }
}

#[derive(Clone)]
pub struct WorkerCard {
    pub project_id: i64,
    pub card_id: String,
    pub root: std::path::PathBuf,
}

impl Workers {
    /// Remember which card a session was launched for. Launches without a
    /// card are ignored.
    pub fn note(&self, session_id: &str, project_id: i64, card_id: &str, root: std::path::PathBuf) {
        if card_id.is_empty() || session_id.is_empty() {
            return;
        }
        self.receipts.lock().unwrap().insert(
            session_id.to_string(),
            Receipt {
                noted_at: Instant::now(),
                first_signal: None,
            },
        );
        self.bindings.lock().unwrap().insert(
            session_id.to_string(),
            WorkerCard {
                project_id,
                card_id: card_id.to_string(),
                root,
            },
        );
    }

    /// A turn boundary on a live session: the driver hands the card back
    /// (once — the guard refuses already-reviewed cards) without dropping the
    /// binding, because the session is still alive. The turn end is also the
    /// proof that the hook pipeline works.
    pub fn turn_ended(&self, session_id: &str) {
        if self.bindings.lock().unwrap().contains_key(session_id) {
            self.turn_ended
                .lock()
                .unwrap()
                .insert(session_id.to_string());
            self.signal(session_id);
        }
    }

    /// Record the first signal from a session's hook pipeline. Later signals
    /// change nothing: the receipt is about the first proof, not the last.
    pub fn signal(&self, session_id: &str) {
        let mut receipts = self.receipts.lock().unwrap();
        if let Some(receipt) = receipts.get_mut(session_id) {
            if receipt.first_signal.is_none() {
                receipt.first_signal = Some(Instant::now());
            }
        }
    }

    /// A read view of one binding: which card the session was launched for.
    pub fn binding(&self, session_id: &str) -> Option<WorkerCard> {
        self.bindings.lock().unwrap().get(session_id).cloned()
    }

    /// Every launch record, for the derived-board read.
    pub fn launch_records(&self) -> Vec<(String, WorkerCard)> {
        self.bindings
            .lock()
            .unwrap()
            .iter()
            .map(|(session_id, worker)| (session_id.clone(), worker.clone()))
            .collect()
    }

    fn worker(&self, session_id: &str) -> Option<WorkerCard> {
        self.bindings.lock().unwrap().get(session_id).cloned()
    }

    fn take_turn_ended(&self) -> Vec<String> {
        let mut pending = self.turn_ended.lock().unwrap().drain().collect::<Vec<_>>();
        pending.retain(|session_id| self.bindings.lock().unwrap().contains_key(session_id));
        pending
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
        self.turn_ended.lock().unwrap().remove(session_id);
        self.receipts.lock().unwrap().remove(session_id);
    }

    /// The receipts, for the derived-board read.
    pub fn receipt(&self, session_id: &str) -> Option<Receipt> {
        self.receipts.lock().unwrap().get(session_id).copied()
    }
}

/// Runs until the daemon stops. Every tick hands back cards whose workers
/// ended a turn or exited, and dispatches a reviewer where one is configured.
/// Settings are opened lazily, so a daemon that never reviews never opens
/// the settings database.
pub(crate) fn run(services: Services) {
    let mut db: Option<Db> = None;
    let mut pr_tick = Instant::now();
    while !services.stopping.load(std::sync::atomic::Ordering::Acquire) {
        std::thread::sleep(TICK);
        if db.is_none() {
            match Db::open(&Paths::with_root(services.home.clone())) {
                Ok(opened) => db = Some(opened),
                Err(error) => eprintln!("radar driver: settings unavailable ({error:#})"),
            }
        }
        tick(&services, db.as_ref());
        // PR facts refresh on their slower schedule, so reads never spawn gh.
        if pr_tick.elapsed() >= PR_TICK {
            pr_tick = Instant::now();
            observe_prs(&services, db.as_ref());
        }
    }
}

/// Hand every ended worker's card back: claimed, still in progress, worker
/// gone — the card goes to Review and a configured reviewer is dispatched.
/// A card whose worker still runs, whose lane is not in progress, or that
/// is done is left alone.
fn tick(services: &Services, db: Option<&Db>) {
    // Turn boundaries first: the same handoff, but the session stays alive,
    // so the binding must survive for the exit pass to find it later.
    for session_id in services.workers.take_turn_ended() {
        let Some(worker) = services.workers.worker(&session_id) else {
            continue;
        };
        if let Err(error) = hand_off(services, db, &worker) {
            eprintln!("radar driver: {error:#}");
        }
    }
    for (session_id, worker) in services.workers.ended(&services.registry.list()) {
        if let Err(error) = hand_off(services, db, &worker) {
            eprintln!("radar driver: {error:#}");
        }
        services.workers.forget(&session_id);
    }
}

/// Returns whether the card was actually handed back.
fn hand_off(services: &Services, db: Option<&Db>, worker: &WorkerCard) -> Result<bool> {
    let state = services.board.state(worker.project_id)?;
    let Some(card) = state.cards.iter().find(|card| card.id == worker.card_id) else {
        // The card is no longer on the board: nothing to hand back.
        return Ok(false);
    };
    if card.done || card.claim.is_none() {
        return Ok(false);
    }
    if lane_kind(&state, card.lane_id) != Some("in_progress") {
        return Ok(false);
    }
    let review = state
        .lanes
        .iter()
        .find(|lane| lane.kind == "review")
        .map(|lane| lane.name.clone())
        .ok_or_else(|| anyhow::anyhow!("no review lane"))?;
    // An open attention on the card keeps the person's turn first: a worker
    // that died mid-question left a decision outstanding, and a reviewer
    // agent would be second in line behind the human's answer.
    let attention_open = services
        .activity
        .unresolved_attention(worker.project_id)
        .map(|requests| {
            requests
                .iter()
                .any(|request| request.card_id.as_deref() == Some(worker.card_id.as_str()))
        })
        .unwrap_or(false);
    let change = services
        .board
        .move_card(worker.project_id, &worker.card_id, &review, None)?;
    publish_board_change(
        &services.activity,
        worker.project_id,
        &crate::session::board::command_id("driver-handoff"),
        &change,
    )?;
    if let Some(db) = db {
        if attention_open {
            eprintln!(
                "radar driver: reviewer skipped for {}: an attention request is still open",
                worker.card_id
            );
            return Ok(true);
        }
        if let Err(error) = dispatch_reviewer(services, db, worker) {
            eprintln!(
                "radar driver: no reviewer dispatched for {}: {error:#}",
                worker.card_id
            );
        }
    }
    Ok(true)
}

/// When the project has a reviewer configured, claim the freshly handed-back
/// card for that agent's fresh session, carrying the review prompt.
fn dispatch_reviewer(services: &Services, db: &Db, worker: &WorkerCard) -> Result<()> {
    let Some(reviewer) = db.reviewer(worker.project_id)? else {
        return Ok(());
    };
    let paths = Paths::with_root(services.home.clone());
    let state = services.board.state(worker.project_id)?;
    let Some(card) = state
        .cards
        .into_iter()
        .find(|card| card.id == worker.card_id)
    else {
        return Ok(());
    };
    crate::session::dispatch::start_review(
        &paths,
        db,
        worker.project_id,
        &worker.root,
        &card,
        Some(&reviewer),
        &crate::session::board::command_id("driver-review"),
    )?;
    Ok(())
}

/// Refresh the PR facts of every project that has a repo. One project's
/// failure leaves the others and the last good read alone.
fn observe_prs(services: &Services, db: Option<&Db>) {
    let Some(db) = db else {
        return;
    };
    let Ok(projects) = db.projects() else {
        return;
    };
    for project in projects {
        let previous: Vec<crate::session::pr::PrFacts> = services.pr.facts(project.id);
        match crate::session::pr::observe(&project.path) {
            Ok(facts) => {
                route_transitions(services, db, project.id, &project.path, &previous, &facts);
                services.pr.put(project.id, facts);
            }
            Err(error) => {
                eprintln!(
                    "radar pr: {} on {}: {error:#}",
                    crate::session::pr::gh(),
                    project.path.display()
                );
            }
        }
    }
}

/// Turn PR transitions into turns in the owning agent's session. The deciding
/// cache updates before routing starts, so one transition routes at most
/// once; a card with an open attention keeps the person first.
fn route_transitions(
    services: &Services,
    db: &Db,
    project_id: i64,
    root: &std::path::Path,
    previous: &[crate::session::pr::PrFacts],
    facts: &[crate::session::pr::PrFacts],
) {
    let changes = crate::session::pr::transitions(previous, facts);
    if changes.is_empty() {
        return;
    }
    let Ok(state) = services.board.state(project_id) else {
        return;
    };
    // Reuse the read's branch join so a transition lands on the same card
    // the board shows it on.
    let mut branches: HashMap<String, String> = HashMap::new();
    for (session_id, worker) in services.workers.launch_records() {
        if let Some(session) = services.registry.list().iter().find(|s| s.id == session_id) {
            if let Some(branch) = crate::git::status(&session.cwd).branch {
                branches.insert(worker.card_id, branch);
            }
        }
    }
    let card_prs = crate::session::pr::join(&state.cards, facts, branches);
    let attention_open = |card_id: &str| {
        services
            .activity
            .unresolved_attention(project_id)
            .map(|requests| {
                requests
                    .iter()
                    .any(|request| request.card_id.as_deref() == Some(card_id))
            })
            .unwrap_or(false)
    };
    for feedback in crate::session::pr::feedback_on_cards(&changes, &card_prs) {
        if attention_open(&feedback.card_id) {
            eprintln!(
                "radar feedback: {} held; card {} has an open attention",
                feedback.reason, feedback.card_id
            );
            continue;
        }
        let claim = state
            .cards
            .iter()
            .find(|card| card.id == feedback.card_id)
            .and_then(|card| card.claim.clone());
        // The ACP agent's own id is what the binding keyed on: the daemon's
        // start binder writes spec.id. The running agent for this card is
        // that id; a claimed-but-not-running card still needs to find it.
        let agent_claim = services
            .agents
            .list()
            .iter()
            .find(|agent| agent.card_id.as_deref() == Some(feedback.card_id.as_str()))
            .map(|agent| agent.id.clone())
            .or(claim.map(|claim| format!("acp-{claim}")));
        match crate::session::feedback::route(
            &Paths::with_root(services.home.clone()).data_dir,
            db,
            project_id,
            root,
            agent_claim.as_deref(),
            &feedback,
        ) {
            Ok(outcome) => {
                let _ = services
                    .activity
                    .publish(crate::session::activity::PublishActivity {
                        project_id,
                        command_id: crate::session::board::command_id("feedback-route"),
                        session_id: None,
                        card_id: Some(feedback.card_id.clone()),
                        kind: crate::session::activity::ActivityKind::Reported,
                        payload: crate::session::activity::ActivityPayload::Message {
                            text: format!("[feedback] {}: {outcome}", feedback.reason),
                        },
                    });
            }
            Err(error) => {
                let _ = services
                    .activity
                    .publish(crate::session::activity::PublishActivity {
                        project_id,
                        command_id: crate::session::board::command_id("feedback-fail"),
                        session_id: None,
                        card_id: Some(feedback.card_id.clone()),
                        kind: crate::session::activity::ActivityKind::Reported,
                        payload: crate::session::activity::ActivityPayload::Message {
                            text: format!("[feedback] {}: {error:#}", feedback.reason),
                        },
                    });
            }
        }
    }
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
            pr: Arc::new(crate::session::pr::Cache::default()),
            home: std::env::temp_dir(),
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
        services
            .workers
            .note(session_id, project_id, &card_id, std::env::temp_dir());
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

        tick(&services, None);

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
        tick(&services, None);
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

        tick(&services, None);

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

        tick(&services, None);

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

        tick(&services, None);

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
        services
            .workers
            .note("card-s-5", 4, &change.card.id, std::env::temp_dir());

        tick(&services, None);

        assert_eq!(
            card_in(&services.board.state(4).unwrap(), "Fix login").lane,
            "Todo"
        );
    }

    fn live_session(services: &Services, id: &str) {
        let _ = services
            .registry
            .create(crate::session::registry::Spawn {
                id: id.into(),
                argv: vec!["/bin/sleep".into(), "60".into()],
                cwd: std::env::current_dir().unwrap(),
                dims: crate::session::Dims { cols: 80, rows: 24 },
                env: Vec::new(),
                env_remove: Vec::new(),
            })
            .unwrap();
    }

    #[test]
    fn a_turn_end_hands_the_card_back_while_the_binding_survives() {
        let services = services();
        bound_card(&services, 4, "card-s-7");
        live_session(&services, "card-s-7");
        services.workers.turn_ended("card-s-7");

        tick(&services, None);

        let state = services.board.state(4).unwrap();
        assert_eq!(card_in(&state, "Fix login").lane, "Review");
        // The session is still running, so the binding remains and the exit
        // pass will find the session's true end later.
        assert!(services.workers.worker("card-s-7").is_some());
    }

    #[test]
    fn a_turn_end_on_an_unbound_session_is_silent() {
        let services = services();
        bound_card(&services, 4, "card-s-8");
        live_session(&services, "card-s-8");
        let fresh = workers();
        fresh.turn_ended("card-s-8");

        tick(&services, None);

        assert_eq!(
            card_in(&services.board.state(4).unwrap(), "Fix login").lane,
            "In progress"
        );
    }

    fn workers() -> Workers {
        Workers::default()
    }

    #[test]
    fn a_note_without_a_card_is_ignored() {
        let workers = Workers::default();
        workers.note("s", 4, "", std::env::temp_dir().join("x"));
        assert!(workers.ended(&[]).is_empty());
    }

    #[test]
    fn a_binding_dies_with_its_session_by_design() {
        let workers = Workers::default();
        workers.note("s", 4, "c", std::env::temp_dir().join("x"));
    }
}

#[cfg(test)]
mod receipt_tests {
    use super::*;
    use crate::session::board_store::StoredCard;

    fn card() -> StoredCard {
        StoredCard {
            id: "card-1".into(),
            project_id: 1,
            lane_id: 2,
            lane: "In progress".into(),
            done: false,
            position: 0,
            title: "worker".into(),
            body: String::new(),
            claim: Some("op".into()),
            revision: 1,
            created_at_millis: 0,
            updated_at_millis: 0,
        }
    }

    fn status() -> Status {
        Status {
            id: "s1".into(),
            cwd: "/tmp".into(),
            pid: Some(1),
            lifecycle: Lifecycle::Running,
            title: None,
            stream_closed: false,
        }
    }

    #[test]
    fn a_noted_worker_is_proven_only_after_its_first_signal() {
        let workers = Workers::default();
        workers.note("s1", 1, "card-1", "/tmp".into());
        assert!(workers.receipt("s1").unwrap().first_signal.is_none());

        workers.signal("s1");
        assert!(workers.receipt("s1").unwrap().first_signal.is_some());
    }

    #[test]
    fn a_noted_worker_clears_the_receipt_and_must_reprove() {
        let workers = Workers::default();
        workers.note("s1", 1, "card-1", "/tmp".into());
        workers.signal("s1");
        assert!(workers.receipt("s1").unwrap().first_signal.is_some());

        // The same session relaunched: the receipt is a fresh promise.
        workers.note("s1", 1, "card-1", "/tmp".into());
        assert!(workers.receipt("s1").unwrap().first_signal.is_none());
    }

    #[test]
    fn quiet_derives_past_the_grace_and_un_quiets_on_signal() {
        let workers = Workers::default();
        workers.note("s1", 1, "card-1", "/tmp".into());

        // Inside the grace: still Running.
        let derived = crate::session::lane::derive_card(
            &card(),
            Some((&status(), workers.receipt("s1"))),
            None,
            None,
            None,
            false,
        );
        assert_eq!(
            derived.worker,
            Some(crate::session::lane::WorkerFact::Running)
        );

        // Once signalled: proven for the whole incarnation.
        workers.signal("s1");
        let derived = crate::session::lane::derive_card(
            &card(),
            Some((&status(), workers.receipt("s1"))),
            None,
            None,
            None,
            false,
        );
        assert_eq!(
            derived.worker,
            Some(crate::session::lane::WorkerFact::Running)
        );
    }

    #[test]
    fn unbound_sessions_signal_nothing() {
        let workers = Workers::default();
        workers.signal("ghost");
        assert!(workers.receipt("ghost").is_none());
    }
}
