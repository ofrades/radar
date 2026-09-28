//! Bindings from board claims to the agent conversations behind them.
//!
//! A claimed card names an agent instance (`opencode-mui9zs8a`); when that
//! agent's program exits, radar captures the session the CLI itself
//! recorded for the conversation it had, and binds it here. Clicking the
//! claim later reopens that exact conversation — across restarts, without
//! relying on "the last one" still being the right one.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use super::{now, Db};

impl Db {
    /// Bind a claim to the conversation its agent had. One binding per
    /// (project, claim): the newest capture wins.
    pub fn bind_session(
        &self,
        project_id: i64,
        claim: &str,
        program_id: &str,
        session_id: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO agent_sessions (project_id, claim, program_id, session_id, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(project_id, claim) DO UPDATE SET
                 program_id = excluded.program_id,
                 session_id = excluded.session_id,
                 updated_at = excluded.updated_at",
            params![project_id, claim, program_id, session_id, now()],
        )?;
        Ok(())
    }

    /// The conversation a claim's agent last had: `(program_id, session_id)`.
    pub fn bound_session(&self, project_id: i64, claim: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT program_id, session_id FROM agent_sessions
                 WHERE project_id = ?1 AND claim = ?2",
                params![project_id, claim],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::Db;

    #[test]
    fn a_bound_claim_returns_its_conversation() {
        let db = Db::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("one")).unwrap();
        let project = db.add_project(dir.path().join("one")).unwrap().id;
        assert!(db
            .bound_session(project, "opencode-mui9zs8a")
            .unwrap()
            .is_none());

        db.bind_session(project, "opencode-mui9zs8a", "opencode", "ses_abc")
            .unwrap();
        let bound = db.bound_session(project, "opencode-mui9zs8a").unwrap();
        assert_eq!(bound, Some(("opencode".into(), "ses_abc".into())));

        // Rebinding the same claim replaces the conversation.
        db.bind_session(project, "opencode-mui9zs8a", "opencode", "ses_def")
            .unwrap();
        assert_eq!(
            db.bound_session(project, "opencode-mui9zs8a").unwrap(),
            Some(("opencode".into(), "ses_def".into()))
        );
        // Another project's claim with the same name is its own.
        std::fs::create_dir_all(dir.path().join("two")).unwrap();
        let other = db.add_project(dir.path().join("two")).unwrap().id;
        assert!(db
            .bound_session(other, "opencode-mui9zs8a")
            .unwrap()
            .is_none());
    }
}
