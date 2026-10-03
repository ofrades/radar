//! Board glue shared by every non-GUI surface (CLI, web, MCP): resolving a
//! card command's project, finding cards in a snapshot, and command ids.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::db::Db;
use crate::session::board_store::{BoardState, StoredCard};

/// A card command's directory: the argument, else `$RADAR_PROJECT_ROOT`, else
/// the current directory.
pub fn dir(path: Option<PathBuf>) -> Result<PathBuf> {
    let requested = path
        .or_else(|| std::env::var_os("RADAR_PROJECT_ROOT").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    crate::db::normalize_path(requested)
}

/// A board command's project: its id and root directory. Prefers the
/// `RADAR_PROJECT_ID` a radar-launched pane carries, else the sidebar lookup.
pub fn context(db: &Db, path: Option<PathBuf>) -> Result<(i64, PathBuf)> {
    let dir = dir(path)?;
    if let Ok(raw) = std::env::var("RADAR_PROJECT_ID") {
        if let Ok(project_id) = raw.parse::<i64>() {
            return Ok((project_id, dir));
        }
    }
    let project = db.project_by_path(&dir)?.with_context(|| {
        format!(
            "{} is not in the sidebar — add it to radar, or run from a radar pane",
            dir.display()
        )
    })?;
    Ok((project.id, project.path))
}

/// A card by stable id, else by exact title.
pub fn find_card<'a>(state: &'a BoardState, needle: &str) -> Option<&'a StoredCard> {
    state
        .cards
        .iter()
        .find(|card| card.id == needle)
        .or_else(|| state.cards.iter().find(|card| card.title == needle))
}

/// A board command's id, unique per call, so the daemon journal can dedupe.
pub fn command_id(prefix: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("cli-{prefix}-{}-{now:x}", std::process::id())
}
