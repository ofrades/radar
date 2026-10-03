//! Dispatch a board card to a worker agent through the session daemon.
//!
//! This is the orchestrator's core primitive, shared by the CLI (`radar card
//! start`) and the MCP surface: the daemon spawns the project's preferred
//! agent with the card's canonical work prompt as its first message, attached
//! to the card (`RADAR_CARD_ID`) and claiming it for the new instance.

use std::path::Path;

use anyhow::{Context, Result};

use crate::config::Paths;
use crate::db::{Db, Slot};
use crate::programs::{self, LaunchOptions};
use crate::session::board_store::StoredCard;
use crate::session::daemon::{self, Client, Command as Request};
use crate::session::registry::Spawn;
use crate::session::Dims;

/// The outcome of asking for a card's worker. Dispatch is idempotent: a card
/// someone already holds is never dispatched again.
#[derive(Debug)]
pub enum Dispatch {
    Started {
        claim: String,
        session_id: String,
        conversation: Option<String>,
    },
    AlreadyClaimed {
        claim: String,
    },
}

/// Spawn the agent for a card and claim it for the new instance.
pub fn start_card(
    paths: &Paths,
    db: &Db,
    project_id: i64,
    root: &Path,
    card: &StoredCard,
    command_id: &str,
) -> Result<Dispatch> {
    let program = db
        .project_settings(project_id)
        .unwrap_or_default()
        .apply_to(&db.preferences().unwrap_or_default());
    let program = programs::for_slot(Slot::Agent, &program)
        .context("No agent is installed — set one in Preferences")?;
    dispatch_card(
        &paths.data_dir,
        project_id,
        root,
        card,
        DispatchTarget {
            program: &program,
            session_prefix: "card-",
            prompt: crate::session::board_store::work_prompt(&card.id, &card.title),
        },
        command_id,
    )
}

/// Spawn the reviewer agent for a card the driver handed back. Which agent
/// reviews is a preference (project override, else global); the reviewer is
/// launched bound to the card and claims it in its own name — review claiming
/// keeps the card in Review, and `card done` stays the reviewer's word.
pub fn start_review(
    paths: &Paths,
    db: &Db,
    project_id: i64,
    root: &Path,
    card: &StoredCard,
    reviewer: Option<&str>,
    command_id: &str,
) -> Result<Dispatch> {
    use crate::programs::agents;
    let program = match reviewer {
        Some(id) => programs::by_id(id)
            .filter(|program| agents::is_supported(&program.id))
            .with_context(|| format!("reviewer {id} is not a supported agent"))?,
        None => {
            let preferences = db
                .project_settings(project_id)
                .unwrap_or_default()
                .apply_to(&db.preferences().unwrap_or_default());
            programs::for_slot(Slot::Agent, &preferences)
                .context("No agent is installed — set one in Preferences")?
        }
    };
    dispatch_card(
        &paths.data_dir,
        project_id,
        root,
        card,
        DispatchTarget {
            program: &program,
            session_prefix: "review-",
            prompt: crate::session::board_store::review_prompt(&card.id, &card.title),
        },
        command_id,
    )
}

/// What a card's dispatch launches: the program, the session-id prefix, and
/// the prompt the agent gets as its first message.
struct DispatchTarget<'a> {
    program: &'a crate::programs::Program,
    session_prefix: &'a str,
    prompt: String,
}

/// The shared spawn: bound to the card, claiming it, with the target's
/// prompt as the first message.
fn dispatch_card(
    data_dir: &Path,
    project_id: i64,
    root: &Path,
    card: &StoredCard,
    target: DispatchTarget<'_>,
    command_id: &str,
) -> Result<Dispatch> {
    let DispatchTarget {
        program,
        session_prefix,
        prompt,
    } = target;
    if let Some(claim) = &card.claim {
        return Ok(Dispatch::AlreadyClaimed {
            claim: claim.clone(),
        });
    }

    crate::setup::install_default_session_hooks()?;
    let stamp = programs::launch::now_stamp();
    let claim = format!("{}-{}", program.id, stamp);
    let session_id = format!("{session_prefix}{}-{stamp}", card.id);
    // Naming a fresh conversation is a request, not proof of its identity.
    // Every provider reports the actual active ID through its lifecycle hook.
    let conversation = program
        .create_session
        .then(|| programs::launch::provider_session_id(project_id, &stamp));
    let spec = programs::launch::command_spec(
        program,
        &LaunchOptions {
            prompt: Some(prompt),
            card: Some(card.id.clone()),
            agent_instance: Some(stamp),
            create_session: conversation.clone(),
            ..Default::default()
        },
    );

    let mut env = spec.env_set.clone();
    env.extend([
        ("RADAR_PROJECT_ID".to_string(), project_id.to_string()),
        (
            "RADAR_PROJECT_ROOT".to_string(),
            root.to_string_lossy().into_owned(),
        ),
        (
            "RADAR_HOME".to_string(),
            data_dir.to_string_lossy().into_owned(),
        ),
        ("RADAR_SESSION_ID".to_string(), session_id.clone()),
    ]);

    Client::request(
        data_dir,
        Request::Create(Spawn {
            id: session_id.clone(),
            argv: spec.argv,
            cwd: root.to_path_buf(),
            dims: Dims {
                cols: 120,
                rows: 32,
            },
            env,
            env_remove: spec.env_unset,
        }),
    )?;

    daemon::board_card_claim(
        data_dir,
        project_id,
        &card.id,
        Some(&claim),
        None,
        command_id,
    )?;
    Ok(Dispatch::Started {
        claim,
        session_id,
        conversation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;

    fn card() -> StoredCard {
        crate::session::board_store::BoardStore::open_in_memory().unwrap();
        StoredCard {
            id: "card-1".into(),
            project_id: 4,
            lane_id: 1,
            lane: "In progress".into(),
            done: false,
            position: 1,
            title: "Fix login".into(),
            body: String::new(),
            claim: Some("agent-1".into()),
            revision: 3,
            created_at_millis: 1,
            updated_at_millis: 1,
        }
    }

    #[test]
    fn a_claimed_card_is_never_dispatched_twice() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("one")).unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        let db = Db::open(&paths).unwrap();
        match start_review(&paths, &db, 4, dir.path(), &card(), Some("pi"), "cmd") {
            Ok(Dispatch::AlreadyClaimed { claim }) => assert_eq!(claim, "agent-1"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn an_unknown_reviewer_is_named_in_the_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("one")).unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        let db = Db::open(&paths).unwrap();
        let mut card = card();
        card.claim = None;
        let error = start_review(
            &paths,
            &db,
            4,
            dir.path(),
            &card,
            Some("not-an-agent"),
            "cmd",
        )
        .unwrap_err();
        assert!(error.to_string().contains("not-an-agent"));
    }
}
