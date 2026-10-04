//! Feedback routing: a PR transition becomes a turn in the work's own agent
//! session.
//!
//! The PR observer notices changes; this module turns them into prompts for
//! the agent that owns the work, by resuming its bound conversation — the
//! exact session, not a fresh one. The message always says what changed and
//! asks for the fix; it never adds instructions. Every routed message is
//! stated on the card's thread, so a human sees why an agent picked work back
//! up on its own.
//!
//! One route per transition: the deciding cache updates before routing
//! starts, so a flapping check shows as one message, not a loop. A card with
//! an open attention gets no route — the person's answer comes first (the
//! same precedence the derived read settles by).

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::db::Db;
use crate::session::agent::AgentStart;
use crate::session::daemon::{Client, Command as Request, Response};
use crate::session::pr::{Feedback, PrFacts};

/// Route one piece of feedback to the card's bound conversation. The card's
/// claim names the agent instance; the binding stores its conversation.
/// Without either leg there is nothing to route to, and that says so.
pub fn route(
    home: &Path,
    db: &Db,
    project_id: i64,
    root: &Path,
    card_claim: Option<&str>,
    feedback: &Feedback,
) -> Result<String> {
    let Some(claim) = card_claim else {
        bail!(
            "card {} has no claim; nobody owns the work to route to",
            feedback.card_id
        );
    };
    let (provider, conversation) = db.bound_session(project_id, claim)?.context(
        "the card's agent has no bound conversation yet; start it once and routing works",
    )?;

    let prompt = feedback_prompt(&feedback.reason, &feedback.pr);
    // A bound ACP conversation resumes exactly, via the daemon's agent
    // surface; the agent id stays the claim, so its restart rebinding lands
    // on the same key again.
    let agent_id = claim.to_string();
    match Client::request(home, Request::AgentList)? {
        Response::Agents(agents) => {
            let running = agents
                .iter()
                .find(|agent| agent.card_id.as_deref() == Some(feedback.card_id.as_str()));
            if let Some(running) = running {
                if running.state == "working" {
                    return Ok(format!(
                        "card {} already has an agent mid-turn; feedback {} left for its echo",
                        feedback.card_id, feedback.reason
                    ));
                }
                Client::request(
                    home,
                    Request::AgentPrompt {
                        id: running.id.clone(),
                        text: prompt,
                    },
                )?;
                return Ok(format!(
                    "feedback routed into the running session {}",
                    running.id
                ));
            }
            Client::request(
                home,
                Request::AgentStart(AgentStart {
                    id: agent_id.clone(),
                    provider: provider.clone(),
                    program: provider,
                    args: Vec::new(),
                    cwd: root.to_path_buf(),
                    project_id,
                    session_id: None,
                    card_id: Some(feedback.card_id.clone()),
                    acp_session_id: Some(conversation),
                }),
            )?;
            Client::request(
                home,
                Request::AgentPrompt {
                    id: agent_id.clone(),
                    text: prompt,
                },
            )?;
            Ok(format!(
                "card {} resumed on feedback {}",
                feedback.card_id, feedback.reason
            ))
        }
        other => bail!("unexpected agent response while routing: {other:?}"),
    }
}

/// The message an agent receives on a PR transition: what changed, where,
/// and the one ask.
pub fn feedback_prompt(reason: &str, pr: &PrFacts) -> String {
    format!(
        "Board feedback for your pull request: {reason}.\n\
         PR #{} — {} ({})\n\
         The work is wanted. Fix what failed and push.",
        pr.number, pr.title, pr.url
    )
}
