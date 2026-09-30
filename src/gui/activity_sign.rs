//! Activity signs: one small, shared vocabulary for the live state of a
//! project, a session, or a card.
//!
//! Radar already records every meaningful transition in the durable activity
//! journal — explicit agent state, session lifecycle, attention, board moves.
//! These signs are how that journal becomes visible on the surfaces that
//! already exist: a project's lane on Home, a session row, a card's session
//! chip, and a workspace pane header. They add no section; they annotate what
//! is there.
//!
//! Everything here is pure: the signs are derived from events and liveness,
//! never from terminal output, and they are unit-tested without a window.

use crate::session::activity::{ActivityEvent, ActivityPayload, AgentState};

/// The one sign a surface wears. Ordered by how loudly it asks to be noticed,
/// which is also the order a project aggregates its sessions in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Sign {
    /// Unresolved attention: a question, approval, failure or review handoff.
    NeedsYou,
    /// An agent waiting for input or approval.
    Waiting,
    /// An agent mid-turn.
    Working,
    /// A live session with nothing explicit to say.
    Running,
    /// A session that exited (or failed); the work is not live any more.
    Stopped,
    /// A live session that reported itself idle.
    Idle,
    /// No signal yet.
    Unknown,
}

impl Sign {
    pub(super) fn css_class(self) -> &'static str {
        match self {
            Sign::NeedsYou => "sign-needs-you",
            Sign::Waiting => "sign-waiting",
            Sign::Working => "sign-working",
            Sign::Running => "sign-running",
            Sign::Stopped => "sign-stopped",
            Sign::Idle => "sign-idle",
            Sign::Unknown => "sign-unknown",
        }
    }

    /// A short word for the sign, used as the label and the tooltip.
    pub(super) fn label(self) -> &'static str {
        match self {
            Sign::NeedsYou => "Needs you",
            Sign::Waiting => "Waiting",
            Sign::Working => "Working",
            Sign::Running => "Running",
            Sign::Stopped => "Stopped",
            Sign::Idle => "Idle",
            Sign::Unknown => "Unknown",
        }
    }

    /// Whether the sign should breathe: work in flight, or something waiting.
    pub(super) fn animates(self) -> bool {
        matches!(self, Sign::NeedsYou | Sign::Waiting | Sign::Working)
    }

    /// The sign's place in a project's aggregate ordering; lower wins.
    fn rank(self) -> u8 {
        match self {
            Sign::NeedsYou => 0,
            Sign::Waiting => 1,
            Sign::Working => 2,
            Sign::Running => 3,
            Sign::Stopped => 4,
            Sign::Idle => 5,
            Sign::Unknown => 6,
        }
    }
}

/// The newest signal a session gave, whether explicit state or lifecycle.
enum Latest {
    State(AgentState),
    Lifecycle,
}

fn is_exit(state: &str) -> bool {
    matches!(state, "exited" | "failed")
}

/// The newest state or lifecycle event in journal order (ascending sequence),
/// scanning backwards so the last word wins.
fn latest(events: &[&ActivityEvent]) -> Option<Latest> {
    events.iter().rev().find_map(|event| match &event.payload {
        ActivityPayload::AgentState { state, .. } => Some(Latest::State(*state)),
        ActivityPayload::SessionLifecycle { .. } => Some(Latest::Lifecycle),
        _ => None,
    })
}

/// One session's sign: liveness first, then the newest explicit state. A
/// session that is no longer live, but which the journal once showed, reads as
/// Stopped; a row the journal has never seen reads as Unknown.
pub(super) fn session_sign(live: bool, external: bool, events: &[&ActivityEvent]) -> Sign {
    match (live || external, latest(events)) {
        (true, Some(Latest::State(AgentState::Working))) => Sign::Working,
        (
            true,
            Some(Latest::State(AgentState::WaitingForInput | AgentState::WaitingForApproval)),
        ) => Sign::Waiting,
        (true, Some(Latest::State(AgentState::Idle))) => Sign::Idle,
        (true, _) => Sign::Running,
        (false, Some(_)) => Sign::Stopped,
        (false, None) => Sign::Unknown,
    }
}

/// A project's sign: unresolved attention outranks everything, then the
/// loudest state among its sessions. A project with nothing running is idle,
/// not unknown.
pub(super) fn project_sign(unresolved_attention: usize, sessions: &[Sign]) -> Sign {
    if unresolved_attention > 0 {
        return Sign::NeedsYou;
    }
    sessions
        .iter()
        .copied()
        .min_by_key(|sign| sign.rank())
        .unwrap_or(Sign::Idle)
}

/// How many distinct sessions ended (exited or failed) within `window_millis`
/// of `now_millis`, counting only each session's newest lifecycle event. This
/// is the "stopped" count a project lane shows beside its running agents.
pub(super) fn recent_exits(
    events: &[&ActivityEvent],
    now_millis: i64,
    window_millis: i64,
) -> usize {
    let mut newest: std::collections::HashMap<&str, (i64, bool)> = std::collections::HashMap::new();
    for event in events {
        let Some(session) = event.session_id.as_deref() else {
            continue;
        };
        if let ActivityPayload::SessionLifecycle { state, .. } = &event.payload {
            newest.insert(session, (event.at_millis, is_exit(state)));
        }
    }
    newest
        .values()
        .filter(|(at, exited)| *exited && now_millis.saturating_sub(*at) <= window_millis)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::activity::ActivityKind;

    fn event(sequence: u64, session: Option<&str>, payload: ActivityPayload) -> ActivityEvent {
        let kind = match &payload {
            ActivityPayload::AgentState { .. } => ActivityKind::AgentStateChanged,
            ActivityPayload::SessionLifecycle { .. } => ActivityKind::SessionLifecycle,
            _ => ActivityKind::Reported,
        };
        ActivityEvent {
            id: format!("evt-1-{sequence}"),
            project_id: 1,
            sequence,
            at_millis: sequence as i64 * 1_000,
            session_id: session.map(str::to_string),
            card_id: None,
            kind,
            payload,
        }
    }

    fn state(sequence: u64, session: &str, state: AgentState) -> ActivityEvent {
        event(
            sequence,
            Some(session),
            ActivityPayload::AgentState {
                state,
                message: None,
            },
        )
    }

    fn lifecycle(sequence: u64, session: &str, state: &str) -> ActivityEvent {
        event(
            sequence,
            Some(session),
            ActivityPayload::SessionLifecycle {
                state: state.to_string(),
                detail: None,
            },
        )
    }

    #[test]
    fn a_live_session_wears_its_newest_explicit_state() {
        let events = [
            state(1, "s", AgentState::Working),
            state(2, "s", AgentState::WaitingForInput),
        ];
        let refs: Vec<&ActivityEvent> = events.iter().collect();
        assert_eq!(session_sign(true, false, &refs), Sign::Waiting);

        let events = [state(1, "s", AgentState::Idle)];
        let refs: Vec<&ActivityEvent> = events.iter().collect();
        assert_eq!(session_sign(true, false, &refs), Sign::Idle);
    }

    #[test]
    fn a_live_session_with_no_state_reads_as_running() {
        assert_eq!(session_sign(true, false, &[]), Sign::Running);
        let events = [lifecycle(1, "s", "attached")];
        let refs: Vec<&ActivityEvent> = events.iter().collect();
        assert_eq!(session_sign(true, false, &refs), Sign::Running);
    }

    #[test]
    fn an_exit_after_a_state_reads_as_stopped() {
        let events = [
            state(1, "s", AgentState::Working),
            lifecycle(2, "s", "exited"),
        ];
        let refs: Vec<&ActivityEvent> = events.iter().collect();
        assert_eq!(session_sign(false, false, &refs), Sign::Stopped);
    }

    #[test]
    fn a_session_the_journal_never_saw_is_unknown_not_stopped() {
        assert_eq!(session_sign(false, false, &[]), Sign::Unknown);
    }

    #[test]
    fn attention_outranks_every_session_state() {
        assert_eq!(
            project_sign(1, &[Sign::Working, Sign::Running]),
            Sign::NeedsYou
        );
    }

    #[test]
    fn a_project_takes_the_loudest_session_sign() {
        assert_eq!(
            project_sign(0, &[Sign::Idle, Sign::Working, Sign::Running]),
            Sign::Working
        );
        assert_eq!(project_sign(0, &[Sign::Idle, Sign::Stopped]), Sign::Stopped);
        assert_eq!(project_sign(0, &[]), Sign::Idle);
    }

    #[test]
    fn recent_exits_count_distinct_sessions_once_and_only_inside_the_window() {
        let events = [
            lifecycle(1, "a", "attached"),
            lifecycle(2, "a", "exited"),
            lifecycle(3, "b", "exited"),
            lifecycle(4, "a", "attached"),
            lifecycle(5, "a", "exited"),
            lifecycle(6, "c", "exited"),
        ];
        let refs: Vec<&ActivityEvent> = events.iter().collect();
        // a (newest exit at 5s), b (3s) and c (6s) are all within 10s; a counts once.
        assert_eq!(recent_exits(&refs, 7_000, 10_000), 3);
        // Only c is within 1.5s of now.
        assert_eq!(recent_exits(&refs, 7_000, 1_500), 1);
    }
}
