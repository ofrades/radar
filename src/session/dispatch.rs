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
    if let Some(claim) = &card.claim {
        return Ok(Dispatch::AlreadyClaimed {
            claim: claim.clone(),
        });
    }

    let global = db.preferences().unwrap_or_default();
    let preferences = db
        .project_settings(project_id)
        .unwrap_or_default()
        .apply_to(&global);
    let program = programs::for_slot(Slot::Agent, &preferences)
        .context("No agent is installed — set one in Preferences")?;
    crate::setup::install_default_session_hooks()?;
    let stamp = programs::launch::now_stamp();
    let claim = format!("{}-{stamp}", program.id);
    let session_id = format!("card-{}-{stamp}", card.id);
    // Naming a fresh conversation is a request, not proof of its identity.
    // Every provider reports the actual active ID through its lifecycle hook.
    let conversation = program
        .create_session
        .then(|| programs::launch::provider_session_id(project_id, &stamp));
    let spec = programs::launch::command_spec(
        &program,
        &LaunchOptions {
            prompt: Some(crate::session::board_store::work_prompt(
                &card.id,
                &card.title,
            )),
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
            paths.data_dir.to_string_lossy().into_owned(),
        ),
        ("RADAR_SESSION_ID".to_string(), session_id.clone()),
    ]);

    Client::request(
        &paths.data_dir,
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
        &paths.data_dir,
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
