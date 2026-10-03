//! Derived board facts: the board reads like the work, not like the last
//! move.
//!
//! Lanes stay stored intent — Beads claim and status, and every card move
//! keeps its semantics. On top of that, every board *read* derives live facts
//! at read time from the stores that own them: the card itself, the worker
//! session state, the ACP agent state, and the project's unresolved
//! attention. Nothing here is persisted, so there is no second place to edit
//! and no way for derived truth to drift from reality; when the underlying
//! facts change, the read changes with them, forwards and backwards.
//!
//! The one derived answer is [`DerivedCard::turn`] (AO's "which loop is
//! turning this card"): whose turn it is — the worker agent's, a person's, or
//! nobody's. An open approval or question reads `Human` no matter what lane
//! the card sits in; a live worker or working agent reads `Agent`; a card no
//! claim holds and that is not done reads `Nobody`. A claimed card whose
//! worker is not alive stays `Human`: the claimant restarts the work, answers
//! what is outstanding, or hands the card back.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::session::activity::Attention;
use crate::session::agent::AgentStatus;
use crate::session::board_store::{BoardState, StoredCard};
use crate::session::driver::Workers;
use crate::session::registry::{Lifecycle, Status};

/// Whose turn the loop is on a card, derived from live facts at read time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopTurn {
    /// The worker agent is mid-turn: a running process, or an ACP agent
    /// between prompt and echo.
    Agent,
    /// A person's next move: an open approval or question on the card, a
    /// review to give, or a claimed card whose worker has not been started.
    Human,
    /// A card no claim holds, done: nothing turns it.
    Nobody,
}

/// The live face of a card's worker, deduced without leaking driver internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFact {
    /// The bound PTY worker session runs.
    Running,
    /// The bound PTY worker session ended.
    Exited,
    /// The card's ACP agent is ready for a prompt.
    AgentReady,
    /// The card's ACP agent is starting up or mid-turn.
    AgentWorking,
    /// The card's ACP agent stopped (exited or failed).
    AgentStopped,
}

/// What a board read knows about one card beyond the stored card itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedCard {
    pub card_id: String,
    /// The claim name whose turn this is, when a turn is claimed at all.
    pub claim: Option<String>,
    pub turn: LoopTurn,
    pub worker: Option<WorkerFact>,
}

/// Derive one card's facts. `worker` is the PTY session bound to this card;
/// `agent` the ACP agent bound to it; `attention` the project's unresolved
/// request naming the card, if any. Open attention wins over both: a question
/// or approval waiting on a person is a person's turn by definition, whatever
/// the worker is doing.
pub fn derive_card(
    card: &StoredCard,
    worker: Option<&Status>,
    agent: Option<&AgentStatus>,
    attention: Option<&Attention>,
) -> DerivedCard {
    let attention_open = attention.is_some_and(Attention::is_unresolved);
    let worker = worker.map(|status| match status.lifecycle {
        Lifecycle::Running => WorkerFact::Running,
        Lifecycle::Exited(_) | Lifecycle::Failed(_) => WorkerFact::Exited,
    });
    let agent = agent.and_then(agent_liveness);
    let turn = if attention_open {
        // Highest precedence, whatever the worker is doing: an approval or
        // question waiting on a person is a person's turn by definition
        // (AO's blocked-never-injected rule).
        LoopTurn::Human
    } else if worker == Some(WorkerFact::Running) || agent == Some(WorkerFact::AgentWorking) {
        LoopTurn::Agent
    } else if card.done || card.claim.is_none() {
        LoopTurn::Nobody
    } else {
        LoopTurn::Human
    };
    DerivedCard {
        card_id: card.id.clone(),
        claim: card.claim.clone(),
        turn,
        worker: worker.or(agent),
    }
}

/// Derive every card of a board at once. `workers` joins sessions to cards
/// through the driver's launch bindings; `agents` through the card id its
/// launch carried; `attention` lists the project's unresolved requests.
pub fn derive_board(
    state: &BoardState,
    workers: &Workers,
    sessions: &[Status],
    agents: &[AgentStatus],
    attention: &[Attention],
) -> Vec<DerivedCard> {
    // session id -> card id, from the driver's launch records.
    let session_card: HashMap<String, String> = workers
        .launch_records()
        .into_iter()
        .map(|(session_id, worker)| (session_id, worker.card_id))
        .collect();
    state
        .cards
        .iter()
        .map(|card| {
            let worker = session_card
                .iter()
                .find(|(_, card_id)| card_id.as_str() == card.id)
                .and_then(|(session_id, _)| {
                    sessions.iter().find(|status| &status.id == session_id)
                });
            let agent = agents
                .iter()
                .find(|agent| agent.card_id.as_deref() == Some(card.id.as_str()));
            let open = attention
                .iter()
                .find(|request| request.card_id.as_deref() == Some(card.id.as_str()))
                .filter(|request| request.is_unresolved());
            derive_card(card, worker, agent, open)
        })
        .collect()
}

/// A live face for an ACP session, or nothing when the state is unknown.
fn agent_liveness(agent: &AgentStatus) -> Option<WorkerFact> {
    match agent.state.as_str() {
        "ready" => Some(WorkerFact::AgentReady),
        "starting" | "working" => Some(WorkerFact::AgentWorking),
        "exited" | "failed" => Some(WorkerFact::AgentStopped),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(claim: &str, done: bool) -> StoredCard {
        StoredCard {
            id: "card-1".into(),
            project_id: 1,
            lane_id: 2,
            lane: "In progress".into(),
            done,
            position: 0,
            title: "worker".into(),
            body: String::new(),
            claim: Some(claim.into()),
            revision: 1,
            created_at_millis: 0,
            updated_at_millis: 0,
        }
    }

    fn running(id: &str) -> Status {
        Status {
            id: id.into(),
            cwd: "/tmp".into(),
            pid: Some(1),
            lifecycle: Lifecycle::Running,
            title: None,
            stream_closed: false,
        }
    }

    fn agent_in(state: &str) -> AgentStatus {
        AgentStatus {
            id: "acp-1".into(),
            provider: "opencode".into(),
            cwd: "/tmp".into(),
            acp_session_id: None,
            state: state.into(),
            detail: None,
            card_id: Some("card-1".into()),
            capabilities: None,
            modes: None,
            config_options: Vec::new(),
        }
    }

    fn approval() -> Attention {
        let mut attention = Attention {
            id: "req-1".into(),
            source_event_id: "event-1".into(),
            project_id: 1,
            session_id: None,
            card_id: Some("card-1".into()),
            kind: crate::session::activity::AttentionKind::Approval,
            reason: "permission".into(),
            allowed_actions: vec![],
            created_at_millis: 0,
            seen_at_millis: None,
            acknowledged_at_millis: None,
            resolved_at_millis: None,
            resolution: None,
            revision: 1,
        };
        attention.resolved_at_millis = None;
        attention
    }

    #[test]
    fn open_attention_is_a_persons_turn_whatever_the_worker_does() {
        let attention = approval();
        let derived = derive_card(
            &card("op", false),
            Some(&running("s1")),
            None,
            Some(&attention),
        );
        assert_eq!(derived.turn, LoopTurn::Human);
    }

    #[test]
    fn a_running_worker_means_the_agent_is_turn() {
        let derived = derive_card(&card("op", false), Some(&running("s1")), None, None);
        assert_eq!(derived.turn, LoopTurn::Agent);
        assert_eq!(derived.worker, Some(WorkerFact::Running));
    }

    #[test]
    fn a_working_acp_agent_means_the_agent_is_turn() {
        let derived = derive_card(&card("op", false), None, Some(&agent_in("working")), None);
        assert_eq!(derived.turn, LoopTurn::Agent);
        assert_eq!(derived.worker, Some(WorkerFact::AgentWorking));
        let derived = derive_card(&card("op", false), None, Some(&agent_in("ready")), None);
        assert_eq!(derived.worker, Some(WorkerFact::AgentReady));
    }

    #[test]
    fn an_ended_worker_hands_the_card_to_a_person() {
        let mut ended = running("s1");
        ended.lifecycle = Lifecycle::Exited(crate::session::ExitInfo {
            code: 0,
            signal: None,
        });
        let derived = derive_card(&card("op", false), Some(&ended), None, None);
        assert_eq!(derived.turn, LoopTurn::Human);
        assert_eq!(derived.worker, Some(WorkerFact::Exited));
    }

    #[test]
    fn no_claim_or_done_means_nobody_turns_the_card() {
        let mut unclaimed = card("op", false);
        unclaimed.claim = None;
        assert_eq!(
            derive_card(&unclaimed, None, None, None).turn,
            LoopTurn::Nobody
        );
        assert_eq!(
            derive_card(&card("op", true), None, None, None).turn,
            LoopTurn::Nobody
        );
    }

    #[test]
    fn an_open_attention_outranks_an_ended_worker() {
        let attention = approval();
        let mut ended = running("s1");
        ended.lifecycle = Lifecycle::Failed("crash".into());
        let derived = derive_card(&card("op", false), Some(&ended), None, Some(&attention));
        assert_eq!(derived.turn, LoopTurn::Human);
    }
}
