//! Pull-request facts: an observer that sees between handoff and merge.
//!
//! The board itself never talks to GitHub. A daemon-side observer thread
//! polls `gh pr list` once per interval for each project that has a repo, and
//! the derived board read joins those facts onto cards afterwards. Nothing is
//! persisted: the cache lives with the daemon and dies with it, exactly like
//! the worker bindings — and the loop can always refresh, whether or not
//! `gh`'s rate-limit is friendly.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Where a PR's checks sit, decided on completions counted once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiState {
    /// Some check is not completed yet.
    Pending,
    /// All completed checks passed and none is still pending or failing.
    Passing,
    /// A completed check failed.
    Failing,
    /// No checks at all.
    None,
}

/// One open pull request, as concise as a board read ever needs. A PR's
/// bodies and check lists stay behind: the URL carries them for anyone who
/// wants to open the review itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrFacts {
    pub number: i64,
    pub url: String,
    pub title: String,
    /// The head branch name — the join key onto a card.
    pub branch: String,
    pub draft: bool,
    /// `OPEN` | `CLOSED` | `MERGED`.
    pub state: String,
    pub ci: CiState,
    /// Names of completed failing checks, bounded to what a read needs.
    pub failing: Vec<String>,
    /// Review decision slug (`APPROVED`, `CHANGES_REQUESTED`, …) or empty.
    pub review: String,
    /// `MERGEABLE` | `CONFLICTING` | `UNKNOWN` — the platform's own word.
    pub mergeable: String,
    pub updated_at: Option<String>,
}

/// The cached facts the observer keeps, per project id.
#[derive(Default)]
pub struct Cache {
    facts: Mutex<HashMap<i64, Vec<PrFacts>>>,
}

impl Cache {
    /// What the last observation saw, or nothing when it has not run.
    pub fn facts(&self, project_id: i64) -> Vec<PrFacts> {
        self.facts
            .lock()
            .get(&project_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Replace one project's cache.
    pub(crate) fn put(&self, project_id: i64, facts: Vec<PrFacts>) {
        self.facts.lock().insert(project_id, facts);
    }

    /// Drop one project's cache (a project was removed).
    pub fn clear(&self, project_id: i64) {
        self.facts.lock().remove(&project_id);
    }
}

/// Run `gh pr list` in the project directory and reduce it to facts.
/// Anything that fails — `gh` missing, not authenticated, not a repository —
/// leaves the caller's last read alone; the note is stderr, not a modal.
pub fn observe(cwd: &Path) -> Result<Vec<PrFacts>, String> {
    let output = Command::new(gh())
        .arg("pr")
        .arg("list")
        .args([
            "--json",
            "number,title,url,isDraft,state,headRefName,statusCheckRollup,reviewDecision,mergeable,updatedAt",
        ])
        .arg("--limit")
        .arg("40")
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("gh is unusable from there: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr.lines().next().unwrap_or("gh failed").to_string();
        return Err(reason);
    }
    parse(std::str::from_utf8(&output.stdout).unwrap_or_default())
}

/// `gh` being on PATH is the only contract; nothing is configured otherwise.
pub fn gh() -> &'static str {
    "gh"
}

/// Reduce `gh`'s JSON into concise facts. Check rollups list every shard of
/// a matrix run — the count and the *failing* names are the only part a
/// board read needs.
pub fn parse(raw: &str) -> Result<Vec<PrFacts>, String> {
    #[derive(Deserialize)]
    struct Check {
        #[serde(default)]
        status: String,
        #[serde(default)]
        conclusion: String,
        #[serde(default)]
        name: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Rich {
        number: i64,
        title: String,
        url: String,
        #[serde(default)]
        is_draft: bool,
        #[serde(default)]
        state: String,
        #[serde(default)]
        head_ref_name: String,
        #[serde(default)]
        status_check_rollup: Vec<Check>,
        #[serde(default)]
        review_decision: String,
        #[serde(default)]
        mergeable: String,
        #[serde(default)]
        updated_at: Option<String>,
    }

    let rich: Vec<Rich> = serde_json::from_str(raw)
        .map_err(|error| format!("gh pr list JSON is not what we ask for: {error}"))?;
    Ok(rich
        .into_iter()
        .map(|pr| {
            let mut pending = false;
            let mut failed: Vec<String> = Vec::new();
            let mut passed_only = true;
            for check in &pr.status_check_rollup {
                match check.status.as_str() {
                    "IN_PROGRESS" | "QUEUED" => {
                        pending = true;
                    }
                    "COMPLETED" => match check.conclusion.as_str() {
                        "SUCCESS" => {}
                        "FAILURE" | "CANCELLED" | "TIMED_OUT" => {
                            passed_only = false;
                            if failed.len() < 8 {
                                failed.push(check.name.clone());
                            }
                        }
                        // SKIPPED and NEUTRAL do not fail a run.
                        _ => {}
                    },
                    _ => {}
                }
            }
            let ci = if failed.is_empty() && pending {
                CiState::Pending
            } else if failed.is_empty()
                && !pending
                && (passed_only || pr.status_check_rollup.is_empty())
            {
                if pr.status_check_rollup.is_empty() {
                    CiState::None
                } else {
                    CiState::Passing
                }
            } else {
                CiState::Failing
            };
            PrFacts {
                number: pr.number,
                url: pr.url,
                title: pr.title,
                branch: pr.head_ref_name,
                draft: pr.is_draft,
                state: pr.state,
                ci,
                failing: failed,
                review: pr.review_decision,
                mergeable: pr.mergeable,
                updated_at: Some(pr.updated_at.unwrap_or_default()).filter(|v| !v.is_empty()),
            }
        })
        .collect())
}

/// Join facts onto projects: each session's worktree branch is its card's
/// branch when the launch bindings say so, plus what a `card/<id>` branch
/// names directly.
pub fn join(
    cards: &[crate::session::board_store::StoredCard],
    facts: &[PrFacts],
    branches: HashMap<String, String>,
) -> HashMap<String, PrFacts> {
    let mut hits = HashMap::new();
    for card in cards {
        if let Some(claim_branch) = branches.get(&card.id) {
            if let Some(found) = facts
                .iter()
                .find(|pr| *pr.branch == *claim_branch && !pr.is_terminal())
            {
                hits.insert(card.id.clone(), found.clone());
                continue;
            }
        }
        if let Some(found) = facts
            .iter()
            .find(|pr| pr.branch == format!("card/{}", card.id) && !pr.is_terminal())
        {
            hits.insert(card.id.clone(), found.clone());
        }
    }
    hits
}

impl PrFacts {
    /// A terminal PR does not drive a card: another branch (or none) is live.
    fn is_terminal(&self) -> bool {
        self.state == "MERGED" || self.state == "CLOSED"
    }
}

/// Parent project id's per project facts for the read route.
pub type SharedCache = Arc<Cache>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gh_json_reduces_to_concise_facts() {
        let raw = r#"[
          {"number": 21, "title": "fix: solver", "url": "u1", "isDraft": true, "state": "OPEN",
           "headRefName": "card/x", "reviewDecision": "APPROVED", "mergeable": "MERGEABLE",
           "updatedAt": "t1",
           "statusCheckRollup": [
             {"status": "COMPLETED", "conclusion": "SUCCESS", "name": "lint"},
             {"status": "COMPLETED", "conclusion": "FAILURE", "name": "unit (shard 3)"},
             {"status": "QUEUED", "name": "deploy"}
           ]},
          {"number": 22, "title": "clean", "url": "u2", "state": "OPEN",
           "headRefName": "card/y", "mergeable": "CONFLICTING",
           "statusCheckRollup": [{"status": "COMPLETED", "conclusion": "SUCCESS", "name": "lint"}]}
        ]"#;
        let facts = parse(raw).unwrap();
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].ci, CiState::Failing);
        assert_eq!(facts[0].failing, vec!["unit (shard 3)".to_string()]);
        assert!(facts[0].draft);
        assert_eq!(facts[0].review, "APPROVED");
        assert_eq!(facts[1].ci, CiState::Passing);
        assert_eq!(facts[1].mergeable, "CONFLICTING");
        assert_eq!(facts[1].updated_at, None);
    }

    #[test]
    fn no_checks_at_all_read_none_not_passing() {
        let raw = r#"[{"number": 3, "title": "docs", "url": "u", "state": "OPEN",
            "headRefName": "card/z", "mergeable": "MERGEABLE"}]"#;
        let facts = parse(raw).unwrap();
        assert_eq!(facts[0].ci, CiState::None);
    }
}
