//! The conversations agents have, as their CLIs record them.
//!
//! A radar-launched agent carries a claim name (`RADAR_AGENT`); when its
//! program exits, radar asks the CLI's own session store which
//! conversation that instance had, and binds it to the claim (see
//! `db::agent_sessions`). Clicking the claim later reopens exactly that
//! conversation. Capture is CLI-specific knowledge: today only opencode
//! exposes a session list radar can read.

use serde::Deserialize;
use std::path::Path;

/// Which CLI has a session store radar can read? Capture is per program;
/// the resume templates in the registry cover the rest.
fn can_capture(program_id: &str) -> bool {
    program_id == "opencode"
}

#[derive(Deserialize)]
struct ListedSession {
    id: String,
    created: i64,
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
        .args(["session", "list", "--format", "json"])
        .current_dir(cwd)
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
    fn a_broken_list_is_no_sessions() {
        assert!(parse_sessions("not json").is_empty());
    }
}
