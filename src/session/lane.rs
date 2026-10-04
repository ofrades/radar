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

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::session::activity::Attention;
use crate::session::agent::AgentStatus;
use crate::session::board_store::{BoardState, StoredCard};
use crate::session::driver::Workers;
use crate::session::pr::PrFacts;
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

/// Where a git-backed card sits in the delivery lifecycle, derived from PR
/// facts plus the turn (AO's column vocabulary). Only cards whose work
/// flows through an open PR get one: local-only work keeps the turn
/// vocabulary and never gets a fake lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    /// The PR is open and something is turning it: a live worker or agent,
    /// or the PR is still a draft.
    Validating,
    /// The PR is in its review cycle and no loop is turning it: a person's
    /// turn — review to give, feedback to answer, or a failing check to
    /// decide about.
    NeedsReview,
    /// The PR is mergeable or approved: a merge decision.
    Ready,
}

/// The live face of a card's worker, deduced without leaking driver internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFact {
    /// The bound PTY worker session runs and its hook pipeline has proven
    /// itself this incarnation.
    Running,
    /// The bound PTY worker session runs, but no hook signal arrived past
    /// the grace window: the pipeline may be broken. Still the agent's turn,
    /// but the operator should look.
    Quiet,
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
    /// The open PR the card's work flows through, when one is live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrFacts>,
    /// The delivery-lifecycle placement for PR-bearing cards, derived at
    /// read time and never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<Column>,
    /// A reviewer sent this card back for rework, from the last recorded
    /// verdict fact. Nothing about the stored lane changes; the board
    /// merely shows the word `(returned)` until the work moves on.
    #[serde(default, skip_serializing_if = "is_false")]
    pub returned: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Derive one card's facts. `worker` is the PTY session bound to this card
/// with its hook-pipeline receipt; `agent` the ACP agent bound to it;
/// `attention` the project's unresolved request naming the card, if any. Open
/// attention wins over both: a question or approval waiting on a person is a
/// person's turn by definition, whatever the worker is doing.
pub fn derive_card(
    card: &StoredCard,
    worker: Option<(&Status, Option<crate::session::driver::Receipt>)>,
    agent: Option<&AgentStatus>,
    attention: Option<&Attention>,
    pr: Option<PrFacts>,
    returned: bool,
) -> DerivedCard {
    let attention_open = attention.is_some_and(Attention::is_unresolved);
    let worker = worker.map(|(status, receipt)| match status.lifecycle {
        Lifecycle::Running => {
            if receipt.is_some_and(|receipt| {
                receipt.is_quiet(crate::session::driver::HOOK_GRACE, Instant::now())
            }) {
                WorkerFact::Quiet
            } else {
                WorkerFact::Running
            }
        }
        Lifecycle::Exited(_) | Lifecycle::Failed(_) => WorkerFact::Exited,
    });
    let agent = agent.and_then(agent_liveness);
    let turn = if attention_open {
        // Highest precedence, whatever the worker is doing: an approval or
        // question waiting on a person is a person's turn by definition
        // (AO's blocked-never-injected rule).
        LoopTurn::Human
    } else if worker == Some(WorkerFact::Running)
        || worker == Some(WorkerFact::Quiet)
        || agent == Some(WorkerFact::AgentWorking)
    {
        LoopTurn::Agent
    } else if card.done || card.claim.is_none() {
        LoopTurn::Nobody
    } else {
        LoopTurn::Human
    };
    let column = column_of(&turn, pr.as_ref());
    DerivedCard {
        card_id: card.id.clone(),
        claim: card.claim.clone(),
        turn,
        worker: worker.or(agent),
        pr,
        column,
        returned,
    }
}

/// The delivery-lifecycle column of a PR-bearing card, from the same facts
/// the turn came out of. The agent's live turn wins first (AO's validating:
/// own loop is turning it), then the person's moments, then the merge.
fn column_of(turn: &LoopTurn, pr: Option<&PrFacts>) -> Option<Column> {
    let pr = pr?;
    if pr.state == "MERGED" || pr.state == "CLOSED" {
        // A terminal PR drives nothing; the card shows on turn facts alone.
        return None;
    }
    let column = match turn {
        LoopTurn::Agent => Column::Validating,
        _ if pr.draft => Column::Validating,
        _ if pr.ci == crate::session::pr::CiState::Failing => Column::NeedsReview,
        _ if pr.review == "CHANGES_REQUESTED" => Column::NeedsReview,
        _ if pr.mergeable == "MERGEABLE" || pr.review == "APPROVED" => Column::Ready,
        _ => Column::NeedsReview,
    };
    Some(column)
}

/// Derive every card of a board at once. `workers` joins sessions to cards
/// through the driver's launch bindings; `agents` through the card id its
/// launch carried; `attention` lists the project's unresolved requests; `prs`
/// maps card id to the open PR its branch carries, joined upstream.
#[allow(clippy::too_many_arguments)]
pub fn derive_board(
    state: &BoardState,
    workers: &Workers,
    sessions: &[Status],
    agents: &[AgentStatus],
    attention: &[Attention],
    prs: &HashMap<String, PrFacts>,
    returned: &HashSet<String>,
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
                    sessions
                        .iter()
                        .find(|status| &status.id == session_id)
                        .map(|status| (status, workers.receipt(session_id)))
                });
            let agent = agents
                .iter()
                .find(|agent| agent.card_id.as_deref() == Some(card.id.as_str()));
            let open = attention
                .iter()
                .find(|request| request.card_id.as_deref() == Some(card.id.as_str()))
                .filter(|request| request.is_unresolved());
            let pr = prs.get(&card.id).cloned();
            let retried = returned.contains(&card.id);
            derive_card(card, worker, agent, open, pr, retried)
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

    pub(super) fn card(claim: &str, done: bool) -> StoredCard {
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

    pub(super) fn running(id: &str) -> Status {
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
            Some((&running("s1"), None)),
            None,
            Some(&attention),
            None,
            false,
        );
        assert_eq!(derived.turn, LoopTurn::Human);
    }

    #[test]
    fn a_running_worker_means_the_agent_is_turn() {
        let derived = derive_card(
            &card("op", false),
            Some((&running("s1"), None)),
            None,
            None,
            None,
            false,
        );
        assert_eq!(derived.turn, LoopTurn::Agent);
        assert_eq!(derived.worker, Some(WorkerFact::Running));
    }

    #[test]
    fn a_working_acp_agent_means_the_agent_is_turn() {
        let derived = derive_card(
            &card("op", false),
            None,
            Some(&agent_in("working")),
            None,
            None,
            false,
        );
        assert_eq!(derived.turn, LoopTurn::Agent);
        assert_eq!(derived.worker, Some(WorkerFact::AgentWorking));
        let derived = derive_card(
            &card("op", false),
            None,
            Some(&agent_in("ready")),
            None,
            None,
            false,
        );
        assert_eq!(derived.worker, Some(WorkerFact::AgentReady));
    }

    #[test]
    fn an_ended_worker_hands_the_card_to_a_person() {
        let mut ended = running("s1");
        ended.lifecycle = Lifecycle::Exited(crate::session::ExitInfo {
            code: 0,
            signal: None,
        });
        let derived = derive_card(
            &card("op", false),
            Some((&ended, None)),
            None,
            None,
            None,
            false,
        );
        assert_eq!(derived.turn, LoopTurn::Human);
        assert_eq!(derived.worker, Some(WorkerFact::Exited));
    }

    #[test]
    fn no_claim_or_done_means_nobody_turns_the_card() {
        let mut unclaimed = card("op", false);
        unclaimed.claim = None;
        assert_eq!(
            derive_card(&unclaimed, None, None, None, None, false).turn,
            LoopTurn::Nobody
        );
        assert_eq!(
            derive_card(&card("op", true), None, None, None, None, false).turn,
            LoopTurn::Nobody
        );
    }

    #[test]
    fn an_open_attention_outranks_an_ended_worker() {
        let attention = approval();
        let mut ended = running("s1");
        ended.lifecycle = Lifecycle::Failed("crash".into());
        let derived = derive_card(
            &card("op", false),
            Some((&ended, None)),
            None,
            Some(&attention),
            None,
            false,
        );
        assert_eq!(derived.turn, LoopTurn::Human);
    }
}

#[cfg(test)]
mod column_tests {
    use super::*;
    use crate::session::lane::tests::{card, running};
    use crate::session::pr::CiState;

    fn pr_facts(ci: CiState, review: &str, mergeable: &str, draft: bool, state: &str) -> PrFacts {
        PrFacts {
            number: 7,
            url: "u7".into(),
            title: "t".into(),
            branch: "card/x".into(),
            draft,
            state: state.into(),
            ci,
            failing: if ci == CiState::Failing {
                vec!["unit".into()]
            } else {
                Vec::new()
            },
            review: review.into(),
            mergeable: mergeable.into(),
            updated_at: None,
        }
    }

    fn open(ci: CiState, review: &str) -> PrFacts {
        pr_facts(ci, review, "UNKNOWN", false, "OPEN")
    }

    #[test]
    fn a_live_turn_validates_whatever_the_pr_says() {
        let mut derived = derive_card(
            &card("op", false),
            Some((&running("s1"), None)),
            None,
            None,
            Some(open(CiState::Passing, "")),
            false,
        );
        assert_eq!(derived.column, Some(Column::Validating));

        // Even a mergeable PR keeps validating while the loop is turning it.
        derived = derive_card(
            &card("op", false),
            Some((&running("s1"), None)),
            None,
            None,
            Some(open(CiState::Passing, "APPROVED")),
            false,
        );
        assert_eq!(derived.column, Some(Column::Validating));
    }

    #[test]
    fn a_draft_pr_validates_on_its_own() {
        let derived = derive_card(
            &card("op", false),
            None,
            None,
            None,
            Some(pr_facts(CiState::None, "", "UNKNOWN", true, "OPEN")),
            false,
        );
        assert_eq!(derived.column, Some(Column::Validating));
    }

    #[test]
    fn failing_ci_and_changes_requested_are_a_persons_moment() {
        let failing = open(CiState::Failing, "");
        assert_eq!(
            derive_card(
                &card("op", false),
                None,
                None,
                None,
                Some(failing.clone()),
                false
            )
            .column,
            Some(Column::NeedsReview)
        );
        let changes = open(CiState::Passing, "CHANGES_REQUESTED");
        assert_eq!(
            derive_card(&card("op", false), None, None, None, Some(changes), false).column,
            Some(Column::NeedsReview)
        );
    }

    #[test]
    fn mergeable_or_approved_is_ready() {
        for pr in [
            open(CiState::Passing, ""),
            open(CiState::Passing, "APPROVED"),
        ] {
            let mut pr = pr;
            pr.mergeable = "MERGEABLE".into();
            assert_eq!(
                derive_card(&card("op", false), None, None, None, Some(pr), false).column,
                Some(Column::Ready)
            );
        }
    }

    #[test]
    fn in_the_cycle_with_nobody_turning_is_needs_review() {
        let pr = open(CiState::Pending, "");
        assert_eq!(
            derive_card(&card("op", false), None, None, None, Some(pr), false).column,
            Some(Column::NeedsReview)
        );
    }

    #[test]
    fn a_terminal_pr_drives_no_column() {
        for state in ["MERGED", "CLOSED"] {
            let pr = pr_facts(CiState::Passing, "APPROVED", "MERGEABLE", false, state);
            assert_eq!(
                derive_card(&card("op", false), None, None, None, Some(pr), false).column,
                None
            );
        }
    }

    #[test]
    fn local_only_work_is_never_faked_into_a_column() {
        assert_eq!(
            derive_card(
                &card("op", false),
                Some((&running("s1"), None)),
                None,
                None,
                None,
                false,
            )
            .column,
            None,
        );
    }
}
