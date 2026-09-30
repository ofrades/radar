//! The desktop-notification policy for human attention.
//!
//! Kept pure and separate from the GTK `send_notification` call, so the rules
//! that actually matter — raise once per request, never re-raise, withdraw when
//! resolved — are testable without a GApplication or a desktop bus.

use std::collections::HashSet;

use crate::session::activity::{Attention, AttentionKind};

/// The `(project, request)` pairs already raised, which must never be raised
/// twice or after they resolve.
pub(super) type Notified = HashSet<(i64, String)>;

/// A notification to raise.
pub(super) struct Raise {
    pub request_id: String,
    pub kind: AttentionKind,
    pub reason: String,
    /// The card and session the request points at, so a notification (desktop
    /// or in-app) can open exactly what asked.
    pub card_id: Option<String>,
    pub session_id: Option<String>,
}

/// For a project snapshot: which previously-raised requests to withdraw, and
/// which outstanding ones to raise. A request raises when it is unresolved,
/// not yet seen, not yet acknowledged, and not already raised.
pub(super) fn plan_snapshot(
    project_id: i64,
    notified: &Notified,
    outstanding: &[Attention],
) -> (Vec<String>, Vec<Raise>) {
    let alive: HashSet<&str> = outstanding
        .iter()
        .map(|attention| attention.id.as_str())
        .collect();
    let withdraw = notified
        .iter()
        .filter(|(id, request)| *id == project_id && !alive.contains(request.as_str()))
        .map(|(_, request)| request.clone())
        .collect();
    let raise = outstanding
        .iter()
        .filter(|attention| {
            attention.is_unresolved()
                && attention.seen_at_millis.is_none()
                && attention.acknowledged_at_millis.is_none()
        })
        .filter(|attention| !notified.contains(&(project_id, attention.id.clone())))
        .map(|attention| Raise {
            request_id: attention.id.clone(),
            kind: attention.kind,
            reason: attention.reason.clone(),
            card_id: attention.card_id.clone(),
            session_id: attention.session_id.clone(),
        })
        .collect();
    (withdraw, raise)
}

/// For a single `AttentionRequested` event: the notification to raise, unless
/// the request is already outstanding or already raised.
pub(super) fn plan_request(
    project_id: i64,
    notified: &Notified,
    already_outstanding: bool,
    request_id: &str,
    kind: AttentionKind,
    reason: &str,
    card_id: Option<&str>,
    session_id: Option<&str>,
) -> Option<Raise> {
    if already_outstanding || notified.contains(&(project_id, request_id.to_string())) {
        return None;
    }
    Some(Raise {
        request_id: request_id.to_string(),
        kind,
        reason: reason.to_string(),
        card_id: card_id.map(str::to_string),
        session_id: session_id.map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attention(id: &str, kind: AttentionKind) -> Attention {
        Attention {
            id: id.to_string(),
            source_event_id: format!("evt-{id}"),
            project_id: 1,
            session_id: None,
            card_id: None,
            kind,
            reason: format!("why {id}"),
            allowed_actions: Vec::new(),
            created_at_millis: 0,
            seen_at_millis: None,
            acknowledged_at_millis: None,
            resolved_at_millis: None,
            resolution: None,
            revision: 1,
        }
    }

    #[test]
    fn a_snapshot_raises_new_requests_and_withdraws_resolved_ones() {
        let notified: Notified = [(1, "attention-1".to_string())].into_iter().collect();
        let mut resolved = attention("attention-1", AttentionKind::Question);
        resolved.resolved_at_millis = Some(1);
        // attention-1 is no longer outstanding; attention-2 is new.
        let outstanding = vec![attention("attention-2", AttentionKind::Approval)];

        let (withdraw, raise) = plan_snapshot(1, &notified, &outstanding);
        assert_eq!(withdraw, vec!["attention-1".to_string()]);
        assert_eq!(raise.len(), 1);
        assert_eq!(raise[0].request_id, "attention-2");
        assert_eq!(raise[0].kind, AttentionKind::Approval);
    }

    #[test]
    fn a_seen_or_acknowledged_request_does_not_raise() {
        let mut seen = attention("attention-1", AttentionKind::Question);
        seen.seen_at_millis = Some(5);
        let mut acknowledged = attention("attention-2", AttentionKind::Question);
        acknowledged.acknowledged_at_millis = Some(5);
        let (_, raise) = plan_snapshot(1, &Notified::new(), &[seen, acknowledged]);
        assert!(raise.is_empty());
    }

    #[test]
    fn a_request_already_raised_is_not_raised_again() {
        let notified: Notified = [(1, "attention-1".to_string())].into_iter().collect();
        let (_, raise) = plan_snapshot(
            1,
            &notified,
            &[attention("attention-1", AttentionKind::Question)],
        );
        assert!(raise.is_empty());
    }

    #[test]
    fn an_event_raises_once_and_never_when_outstanding() {
        let mut notified = Notified::new();
        let first = plan_request(
            1,
            &notified,
            false,
            "attention-9",
            AttentionKind::Question,
            "why",
            Some("card-1"),
            Some("session-1"),
        );
        assert!(first.is_some());
        assert_eq!(first.unwrap().card_id.as_deref(), Some("card-1"));
        notified.insert((1, "attention-9".to_string()));
        assert!(plan_request(
            1,
            &notified,
            false,
            "attention-9",
            AttentionKind::Question,
            "why",
            None,
            None,
        )
        .is_none());
        assert!(plan_request(
            1,
            &Notified::new(),
            true,
            "attention-9",
            AttentionKind::Question,
            "why",
            None,
            None,
        )
        .is_none());
    }
}
