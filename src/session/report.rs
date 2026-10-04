//! The worker report envelope: card comments that carry a kind, so the loop
//! can act on what a worker said rather than infer it.
//!
//! A comment stays a comment — the thread reads as a conversation either
//! way. The kind is a typed prefix on the text (`[checkpoint] …`, `[blocked]
//! …`, `[done] …`, `[artifact:ref] …`), and the daemon holds the semantics:
//! `blocked` opens a question attention for the human, `done` hands the card
//! back to Review the same way the turn-end handoff does. Note and
//! checkpoint are statements only.
//!
//! Workers get this through the work prompt and the board skill; MCN and the
//! CLI both route through the daemon command so the semantics live in one
//! place.

use serde::{Deserialize, Serialize};

/// The kind of a worker report, as `radar card comment --kind` carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Progress made; nothing asked.
    Checkpoint,
    /// The work cannot continue without a person. Opens a question.
    Blocked,
    /// The worker considers the card handed back.
    Done,
    /// A durable reference (a PR, a document, a dashboard) worth keeping.
    Artifact,
}

impl Kind {
    pub fn parse(raw: &str) -> Option<Kind> {
        match raw {
            "checkpoint" => Some(Kind::Checkpoint),
            "blocked" => Some(Kind::Blocked),
            "done" => Some(Kind::Done),
            "artifact" => Some(Kind::Artifact),
            _ => None,
        }
    }

    pub fn slug(&self) -> &'static str {
        match self {
            Kind::Checkpoint => "checkpoint",
            Kind::Blocked => "blocked",
            Kind::Done => "done",
            Kind::Artifact => "artifact",
        }
    }
}

/// A report as it travels: kind, text, and the optional reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub kind: Kind,
    pub text: String,
    #[serde(default)]
    pub artifact: Option<String>,
}

/// The thread text a report becomes: typed on the first line, so the thread
/// stays a readable conversation and the kind survives with the words.
pub fn format(report: &Report) -> String {
    let mut first = match (&report.kind, report.artifact.as_deref()) {
        (Kind::Artifact, Some(reference)) => format!("[artifact:{reference}]"),
        (kind, _) => format!("[{}]", kind.slug()),
    };
    if !report.text.is_empty() {
        first.push(' ');
        first.push_str(&report.text);
    }
    // A second line names the artifact when the text did not already.
    match (&report.kind, report.artifact.as_deref()) {
        (Kind::Artifact, Some(reference)) if !report.text.contains(reference) => {
            format!("{first}\nartifact: {reference}")
        }
        _ => first,
    }
}

/// What happens for each kind, in one place (the daemon's CardReport handler
/// consults this).
#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    /// Statement only.
    Said,
    /// The work cannot continue: open an attention for the person.
    Blocked,
    /// The worker handed the card back: the same handoff the turn-end path
    /// runs (move to Review, reviewer dispatched where configured).
    HandedBack,
}

pub fn effect(kind: Kind) -> Effect {
    match kind {
        Kind::Checkpoint | Kind::Artifact => Effect::Said,
        Kind::Blocked => Effect::Blocked,
        Kind::Done => Effect::HandedBack,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_thread_text_stays_a_readable_conversation() {
        let text = format(&Report {
            kind: Kind::Checkpoint,
            text: "restarted the failing worker; snapshot clean".into(),
            artifact: None,
        });
        assert_eq!(
            text,
            "[checkpoint] restarted the failing worker; snapshot clean"
        );
    }

    #[test]
    fn artifacts_carry_the_reference_even_in_the_text() {
        let text = format(&Report {
            kind: Kind::Artifact,
            text: "the PR is up".into(),
            artifact: Some("https://github.com/ofrades/radar/pull/5".into()),
        });
        assert!(text.starts_with("[artifact:https://github.com/ofrades/radar/pull/5]"));
        assert!(text.contains("\nartifact: "));
    }

    #[test]
    fn effects_block_and_hand_back_nothing_else() {
        assert_eq!(effect(Kind::Checkpoint), Effect::Said);
        assert_eq!(effect(Kind::Artifact), Effect::Said);
        assert_eq!(effect(Kind::Blocked), Effect::Blocked);
        assert_eq!(effect(Kind::Done), Effect::HandedBack);
    }
}
