//! The conversations agents have, as their CLIs record them.
//!
//! A radar-launched agent carries a claim name (`RADAR_AGENT`); when its
//! program exits, radar asks the CLI's own session store which
//! conversation that instance had, and binds it to the claim (see
//! `db::agent_sessions`). Clicking the claim later reopens exactly that
//! conversation. Capture is CLI-specific knowledge: today only opencode
//! exposes a session list radar can read.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Which CLI has a session store radar can read? Capture is per program;
/// the resume templates in the registry cover the rest.
fn can_capture(program_id: &str) -> bool {
    program_id == "opencode"
}

/// The CLI defaults to 100 rows; use a high bounded page so older history is
/// imported instead of silently disappearing from the sidebar.
const SESSION_LIST_ARGS: [&str; 6] = [
    "session",
    "list",
    "--format",
    "json",
    "--max-count",
    "10000",
];

#[derive(Deserialize)]
struct ListedSession {
    id: String,
    created: i64,
    #[serde(default)]
    updated: Option<i64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    directory: Option<String>,
}

/// The title OpenCode assigned to one exact conversation, when its store
/// exposes it. Other CLIs do not yet share this listing contract.
pub(crate) fn title_for(program_id: &str, cwd: &Path, id: &str) -> Option<String> {
    if !can_capture(program_id) {
        return None;
    }
    let output = std::process::Command::new(program_id)
        .args(SESSION_LIST_ARGS)
        .current_dir(cwd)
        // The daemon may lack the login shell's PATH; resolve the provider
        // CLI through the same entries a spawn uses.
        .env("PATH", crate::config::path_value())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sessions = parse_sessions(std::str::from_utf8(&output.stdout).ok()?);
    listed_title(&sessions, cwd, id)
}

/// One conversation as the provider's own store reports it. `created` keeps
/// the provider's raw stamp; the catalog normalizes units.
pub(crate) struct ProviderSession {
    pub id: String,
    pub title: Option<String>,
    pub created: i64,
    pub updated: Option<i64>,
}

/// Every conversation the provider's store lists for `cwd`. `None` when the
/// CLI has no readable store, the run failed, or the output did not parse.
pub(crate) fn list_provider_sessions(program_id: &str, cwd: &Path) -> Option<Vec<ProviderSession>> {
    if !can_capture(program_id) {
        return None;
    }
    let output = std::process::Command::new(program_id)
        .args(SESSION_LIST_ARGS)
        .current_dir(cwd)
        // The daemon may lack the login shell's PATH; resolve the provider
        // CLI through the same entries a spawn uses.
        .env("PATH", crate::config::path_value())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    Some(
        parse_sessions(std::str::from_utf8(&output.stdout).ok()?)
            .into_iter()
            .filter(|session| {
                session.directory.as_ref().is_none_or(|directory| {
                    let directory = std::fs::canonicalize(directory)
                        .unwrap_or_else(|_| PathBuf::from(directory));
                    directory == cwd
                })
            })
            .map(|session| ProviderSession {
                id: session.id,
                title: session.title,
                created: session.created,
                updated: session.updated,
            })
            .collect(),
    )
}

fn listed_title(sessions: &[ListedSession], cwd: &Path, id: &str) -> Option<String> {
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    sessions
        .iter()
        .find(|session| {
            session.id == id
                && session.directory.as_ref().is_none_or(|directory| {
                    let directory =
                        std::fs::canonicalize(directory).unwrap_or_else(|_| directory.into());
                    directory == cwd
                })
        })
        .and_then(|session| session.title.as_deref())
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
}
/// Parse a session list the way the CLI prints it (JSON, newest first).
fn parse_sessions(json: &str) -> Vec<ListedSession> {
    serde_json::from_str(json).unwrap_or_default()
}

/// The newest session the agent created in `cwd` after `since_ms`. How a
/// claimed card finds the conversation its agent had: the CLI's own
/// store, filtered to what appeared during the instance's lifetime.
/// `None` when the CLI has no readable store, the run failed, or the
/// agent never started a conversation.
pub fn newest_since(program_id: &str, cwd: &Path, since_ms: u128) -> Option<String> {
    if !can_capture(program_id) {
        return None;
    }
    let output = std::process::Command::new(program_id)
        .args(SESSION_LIST_ARGS)
        .current_dir(cwd)
        // The daemon may lack the login shell's PATH; resolve the provider
        // CLI through the same entries a spawn uses.
        .env("PATH", crate::config::path_value())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_sessions(std::str::from_utf8(&output.stdout).ok()?)
        .into_iter()
        .filter(|session| session.created as u128 >= since_ms)
        .max_by_key(|session| session.created)
        .map(|session| session.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_session_after_the_launch_wins() {
        let json = r#"[
            {"id": "ses_old", "created": 1000},
            {"id": "ses_mid", "created": 2000},
            {"id": "ses_new", "created": 3000}
        ]"#;
        // `parse_sessions` is what `newest_since` filters; assert the
        // selection rule directly (a live CLI list varies, the rule does
        // not).
        let sessions = parse_sessions(json);
        let picked = sessions
            .into_iter()
            .filter(|s| s.created as u128 >= 1500)
            .max_by_key(|s| s.created)
            .map(|s| s.id);
        assert_eq!(picked, Some("ses_new".to_string()));
    }

    #[test]
    fn exact_session_title_is_scoped_to_its_project_directory() {
        let sessions = parse_sessions(
            r#"[
                {"id":"ses_a","created":1,"title":"First task","directory":"/projects/radar"},
                {"id":"ses_b","created":2,"title":"Second task","directory":"/projects/radar"},
                {"id":"ses_c","created":3,"title":"Other project","directory":"/projects/other"}
            ]"#,
        );
        assert_eq!(
            listed_title(&sessions, Path::new("/projects/radar"), "ses_b"),
            Some("Second task".to_string())
        );
        assert_eq!(
            listed_title(&sessions, Path::new("/projects/other"), "ses_b"),
            None
        );
    }

    #[test]
    fn provider_updated_stamp_is_optional_and_preserved() {
        let sessions =
            parse_sessions(r#"[{"id":"ses_old","created":1700000000000,"updated":1700007000000}]"#);
        assert_eq!(sessions[0].created, 1_700_000_000_000);
        assert_eq!(sessions[0].updated, Some(1_700_007_000_000));
        assert_eq!(
            parse_sessions(r#"[{"id":"ses_legacy","created":1700000000}]"#)[0].updated,
            None
        );
    }

    #[test]
    fn provider_import_requests_history_beyond_the_cli_default() {
        assert_eq!(
            SESSION_LIST_ARGS,
            [
                "session",
                "list",
                "--format",
                "json",
                "--max-count",
                "10000"
            ]
        );
        let json = format!(
            "[{}]",
            (0..101)
                .map(|index| format!(r#"{{"id":"ses_{index}","created":{index}}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let sessions = parse_sessions(&json);
        assert_eq!(sessions.len(), 101);
        assert_eq!(sessions[100].id, "ses_100");
    }

    #[test]
    fn a_broken_list_is_no_sessions() {
        assert!(parse_sessions("not json").is_empty());
    }
}
