//! Review verdicts: the reviewer's word as a fact, not just a lane move.
//!
//! `card done` closes a card. What it *means* — approved, or sent back for
//! rework — previously lived nowhere: the thread read like a worker's `done`
//! report and the lane move carried no verdict. This module reads the
//! reviewer's closing words for the verb and records a first-class verdict
//! fact alongside the move, so the review pass becomes something the board
//! can show (a returned card reads `(returned)` beside the turn) and a
//! later layer can key on.
//!
//! Parsing is deliberately dumb and legible: word match on the reviewer's
//! own closing message. No word, no verdict fact, and the move still runs —
//! a verdict is info the reviewer gave, never a guess.

use serde::{Deserialize, Serialize};

/// The verdict a reviewer's closing words carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The work passes; the card is genuinely done.
    Approve,
    /// The work needs another pass; the card goes back to its worker.
    Rework,
}

impl Outcome {
    pub fn slug(&self) -> &'static str {
        match self {
            Outcome::Approve => "approve",
            Outcome::Rework => "rework",
        }
    }
}

/// Who closed the card: an agent reviewer (`card next --in Review`) or the
/// person themselves. Recorded so the board can tell the two apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictBy {
    Agent,
    Human,
}

/// One review verdict on one card, as the activity journal keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub by: VerdictBy,
    pub outcome: Outcome,
    /// The words the verdict came from, so the fact is checkable.
    pub words: String,
}

/// Find the verdict in a reviewer's closing message. Both direction lists
/// are grounded in how this project's own reviews are written (the review
/// prompt teaches `card done` with an approval or a what-failed note).
pub fn parse(words: &str) -> Option<Outcome> {
    let text = words.to_lowercase();
    let rework = [
        "rework",
        "needs work",
        "changes requested",
        "changes requested:",
        "sent back",
        "not approved",
        "fails",
        "failing",
        "still broken",
        "does not pass",
        "doesn't pass",
        "request changes",
    ];
    let approve = [
        "approved",
        "approve",
        "ship it",
        "lgtm",
        "looks good",
        "looks right",
        "passes",
        "passing now",
        "green now",
        "all green",
    ];
    if rework.iter().any(|word| text.contains(word)) {
        return Some(Outcome::Rework);
    }
    if approve.iter().any(|word| text.contains(word)) {
        return Some(Outcome::Approve);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_words_approve() {
        for words in [
            "approved; tests pass",
            "ship it",
            "LGTM",
            "looks good to me",
            "the build is all green now",
        ] {
            assert_eq!(parse(words), Some(Outcome::Approve), "{words}");
        }
    }

    #[test]
    fn rework_words_rework() {
        for words in [
            "rework: the login test still fails",
            "needs work, the snapshot check is flaky",
            "changes requested: naming",
            "sent back, conventions",
            "does not pass on windows",
        ] {
            assert_eq!(parse(words), Some(Outcome::Rework), "{words}");
        }
    }

    #[test]
    fn ordinary_closing_words_are_no_verdict_at_all() {
        assert_eq!(parse("done"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("merged the branch"), None);
    }

    #[test]
    fn rework_beats_approve_when_both_appear() {
        // A reviewer explaining an approval with a caveat ("approved the shape;
        // the failing test must go") still reads as rework if it asks for one.
        let words = "approved the shape, but the failing test must be fixed";
        assert_eq!(parse(words), Some(Outcome::Rework));
    }
}
