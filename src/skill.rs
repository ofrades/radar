//! The board convention: how agents are told about the board, and how the
//! board becomes a requirement rather than a suggestion.
//!
//! Two halves, one idea — an agent should meet the board at the moment it
//! starts working, and stumble the moment it tries to skip it:
//!
//! - [`install_global_skill`] writes the board skill once to the user's global
//!   skills home (`~/.agents/skills`), which cursor, opencode and omp all
//!   discover — never into a repository. [`install`] also drops the repository's
//!   local pre-commit gate (untracked). Nothing edits `AGENTS.md`.
//! - [`guard_decision`] answers the harness hooks' one question — may this
//!   edit land? A tool call editing a project file without a live board claim
//!   is denied, with a reason that tells the agent exactly how to comply. The
//!   guard is a guardrail, not a wall: agent-harness directories, and agents
//!   launched outside radar, are always allowed through, because a guard that
//!   blocks its own remedy is a deadlock.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The canonical skill, installed once per machine in the user's global
/// skills home (`~/.agents/skills`) — never in a repository. Cursor, opencode
/// and omp all discover skills there, so one file teaches them all. It is
/// inert unless the agent was launched by radar in a project with a board.
pub const SKILL_MD: &str = r#"---
name: board
description: Claim work from radar's board before editing files, and hand it back through it. Only for radar-launched agents in a project whose board is enabled; inert otherwise.
---

# The board

This skill applies only to a **radar-launched agent** in a project whose
**board is enabled**. Check first, and ignore this skill if either is false:

- `$RADAR_AGENT` must be set (radar launched you). If it is unset, work
  normally — this project's work is not tracked by radar.
- `radar board` must succeed for the project. If it reports the board
  disabled, work normally.

When it applies, radar owns this project's work board: cards are records in
radar's store, and `radar card` is how you read and change them. Other agents
may be working in the repository at the same time — the board is how we stay
out of each other's way. There is no file to edit.

## The loop

1. Claim work before editing anything:
   `radar card next --by "$RADAR_AGENT"` — claims the first unclaimed card in
   your name and prints it.
2. Work one card at a time, and only what the card describes.
3. Blocked, or the card is wrong? Leave a note on it
   (`radar card comment "<card id or title>" "…"`), release it with
   `radar card release --title "…"`, and claim the next card.
4. Finished? Leave a short summary for the reviewer as a comment on the card —
   what changed, how to check it — then move it to Review:
   `radar card move --title "…" --to Review`. Moving hands your claim over:
   from that moment the card is not yours.

## The conversation

A card carries a thread. Read it with `radar card show "<id or title>"`: it
lists the card and everything said about it. Post with
`radar card comment "<id or title>" "…"` — a human reply is a comment with no
session, yours carries your session, so the thread reads as a conversation.
When you need a decision, ask and wait:
`radar activity request --card-id <id> --kind question --reason "…" --allow answer --wait`.

## Review — the handoff between agents

Your card is not finished when you say so; it is finished when a *different*
agent has reviewed it. When claiming work, prefer reviewing first:

    radar card next --in Review --by "$RADAR_AGENT"

Approve with `radar card done --title "…"`, or send it back to In progress
with a note saying what failed. If your own card comes back, reclaim it by
title: `radar card claim --title "…" --by "$RADAR_AGENT"`. `card done` is the
reviewer's word, never the worker's.

## Rules

- Never touch a card another agent has claimed.
- Never move your own card past Review.
- Non-trivial work always goes through a card. If the board has none that
  fits, add one (`radar card add --title "…" --body "…"`) and claim it.
- Reading is free; changing a card goes through `radar card`.
- Committing requires a claim: the project's git pre-commit hook refuses a
  commit from you when radar shows no live claim by your name. Claim first,
  commit after — the hook's refusal text says how.
"#;

/// The git pre-commit hook radar installs: the one gate that holds whatever
/// tool the agent edited with. Fails open when radar is not on PATH — the
/// requirement belongs to radar-managed sessions, not to the machine.
const GIT_HOOK_MARKER: &str = "# radar: board claim required to commit";

const GIT_HOOK: &str = "# radar: board claim required to commit —\
 see the board skill\n\
 # Agents launched by radar must claim work before committing; humans in\n\
 # their own terminals are not affected (no RADAR_AGENT is set).\n\
 command -v radar >/dev/null 2>&1 || exit 0\n\
 [ -n \"$RADAR_AGENT\" ] || exit 0\n\
 exec radar hook guard --commit\n";

/// The user-level skills home every harness here discovers: `~/.agents/skills`.
/// `RADAR_SKILLS_HOME` overrides it (tests, non-standard layouts).
fn skills_home() -> Option<PathBuf> {
    std::env::var_os("RADAR_SKILLS_HOME")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
}

/// Write the board skill into a global skills home. Idempotent: an unchanged
/// skill is not rewritten. The file lives outside any project.
pub fn install_global_skill(home: &Path) -> Result<PathBuf> {
    let path = home.join(".agents/skills/board/SKILL.md");
    write_if_changed(&path, SKILL_MD)?;
    Ok(path)
}

/// Install the board skill into the default global skills home. Idempotent.
pub fn install_default_skill() -> Result<PathBuf> {
    let home = skills_home().context("no home directory for the global board skill")?;
    install_global_skill(&home)
}

/// Install the convention for a board-enabled project. Everything is written
/// to the machine's global config — the skill, the harness edit-gate plugins,
/// and the git commit-gate dispatcher — never into the repository.
pub fn install(db: &crate::db::Db, project: &Path) -> Result<Vec<PathBuf>> {
    if !crate::board::enabled(db, project)? {
        return Ok(Vec::new());
    }
    let Some(home) = skills_home() else {
        return Ok(Vec::new());
    };
    let installed = crate::setup::install_global(&home)?;
    let mut written: Vec<PathBuf> = [
        installed.skill,
        installed.opencode,
        installed.omp,
        installed.claude,
        installed.git_hooks_dir.map(|dir| dir.join("pre-commit")),
    ]
    .into_iter()
    .flatten()
    .collect();
    written.shrink_to_fit();
    Ok(written)
}

fn write_if_changed(path: &Path, content: &str) -> Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == content) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

/// What a harness hook does with a tool call: let it through, or refuse it
/// with the reason the agent will read.
#[derive(Debug, PartialEq, Eq)]
pub enum GuardDecision {
    Allow,
    Deny(String),
}

/// Paths the guard never blocks, relative or absolute: the board itself (or
/// the guard would deny the very edit that creates a claim), agent-harness
/// state, and git internals.
fn claimable_path(project: &Path, file: &Path) -> bool {
    let file = if file.is_absolute() {
        file.to_path_buf()
    } else {
        project.join(file)
    };
    let Ok(file) = file.strip_prefix(project) else {
        return true; // outside the project is not the board's business
    };
    let top = file
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .unwrap_or_default();
    top == crate::board::FILE_NAME || top.starts_with('.')
}

/// The guard's whole judgement: `who` is the live claim to look for (the
/// `RADAR_AGENT` of the launched agent; `None` means the agent was not
/// launched by radar and is allowed through with a reminder), and `file` the
/// path the tool call wants to edit. Reading is never blocked — only a file
/// edit can be.
pub fn guard_decision(
    db: &crate::db::Db,
    project: &Path,
    who: Option<&str>,
    file: Option<&Path>,
) -> GuardDecision {
    let holds = who.map(|who| crate::board::holds_claim(project, who).unwrap_or(true));
    guard_decision_for(db, project, who, file, holds)
}

/// The guard's judgement given whether the agent holds a live claim, read from
/// whatever store the caller uses (the board store in production). `holds`
/// is `None` when the claim could not be judged — no daemon, not in the
/// sidebar — which is allowed through, because a guard must never block what
/// it cannot see.
pub fn guard_decision_for(
    db: &crate::db::Db,
    project: &Path,
    who: Option<&str>,
    file: Option<&Path>,
    holds: Option<bool>,
) -> GuardDecision {
    if let Some(decision) = board_policy_decision(db, project) {
        return decision;
    }
    let Some(who) = who else {
        return GuardDecision::Allow;
    };
    let Some(file) = file else {
        return GuardDecision::Allow;
    };
    if claimable_path(project, file) {
        return GuardDecision::Allow;
    }
    match holds {
        Some(false) => deny_claim(who),
        Some(true) | None => GuardDecision::Allow,
    }
}

/// The commit gate's judgement given a store-read claim. See
/// [`guard_decision_for`] for the `holds` convention.
pub fn commit_decision_for(
    db: &crate::db::Db,
    project: &Path,
    who: Option<&str>,
    holds: Option<bool>,
) -> GuardDecision {
    if let Some(decision) = board_policy_decision(db, project) {
        return decision;
    }
    match who {
        None => GuardDecision::Allow,
        Some(who) => match holds {
            Some(false) => deny_claim(who),
            Some(true) | None => GuardDecision::Allow,
        },
    }
}

fn deny_claim(who: &str) -> GuardDecision {
    GuardDecision::Deny(format!(
        "no board claim — claim work before editing files:\n  \
         radar card next --by \"{who}\"\n  \
         (or review what is waiting: radar card next --in Review --by \"{who}\")\n\
         If this task is not board work, add a card for it and claim that."
    ))
}

/// The commit gate's judgement: when boards are disabled, commits are not
/// subject to a board claim.
pub fn commit_decision(db: &crate::db::Db, project: &Path, who: Option<&str>) -> GuardDecision {
    let holds = who.map(|who| crate::board::holds_claim(project, who).unwrap_or(true));
    commit_decision_for(db, project, who, holds)
}

fn board_policy_decision(db: &crate::db::Db, project: &Path) -> Option<GuardDecision> {
    match crate::board::enabled(db, project) {
        Ok(true) => None,
        Ok(false) => Some(GuardDecision::Allow),
        Err(error) => Some(GuardDecision::Deny(format!(
            "cannot read board policy: {error}"
        ))),
    }
}

/// The one hook every harness respects: git. Installed into the repository's
/// own hooks directory, it refuses a commit from a radar-launched agent that
/// holds no board claim — whatever tool the agent edited with — and lets
/// everyone else through. Never touches a hook radar did not write, and only
/// ever writes under a `.git` directory, so hook managers like husky are
/// left exactly as they are.
pub fn install_git_hook(db: &crate::db::Db, project: &Path) -> Result<Option<PathBuf>> {
    if !crate::board::enabled(db, project)? {
        return Ok(None);
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(project)
        .args(["rev-parse", "--git-path", "hooks"])
        .output();
    let Ok(output) = output else {
        return Ok(None); // no git in PATH: no gate, the skill carries it
    };
    if !output.status.success() {
        return Ok(None); // not a repository: nothing to gate
    }
    let hooks = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let hooks = if hooks.is_absolute() {
        hooks
    } else {
        project.join(hooks)
    };
    // Hook managers own their directory; radar only moves into git's own.
    if !hooks
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new(".git"))
    {
        return Ok(None);
    }
    let hook = hooks.join("pre-commit");
    match std::fs::read_to_string(&hook) {
        // Ours: keep it current.
        Ok(text) if text.contains(GIT_HOOK_MARKER) => {}
        // Someone else's hook: it wins, unseen and unedited.
        Ok(_) => return Ok(None),
        // No hook yet: ours goes in.
        Err(_) => {}
    }
    if let Some(parent) = hook.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&hook, GIT_HOOK).with_context(|| format!("writing {}", hook.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("making {} executable", hook.display()))?;
    }
    Ok(Some(hook))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board;

    fn project() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    fn database(project: &Path) -> crate::db::Db {
        let db = crate::db::Db::open_in_memory().unwrap();
        db.add_project(project).unwrap();
        db
    }

    #[test]
    fn the_skill_installs_globally_and_never_in_the_repo() {
        let (_dir, project) = project();
        let home = tempfile::tempdir().unwrap();
        let path = install_global_skill(home.path()).unwrap();
        assert_eq!(path, home.path().join(".agents/skills/board/SKILL.md"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SKILL_MD);

        // Idempotent: an unchanged skill is not rewritten.
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        install_global_skill(home.path()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            mtime,
            "an unchanged skill was rewritten"
        );

        // Nothing is written into the project tree.
        assert!(!project.join(".opencode").exists());
        assert!(!project.join(".claude").exists());
    }

    #[test]
    fn disabled_project_does_not_install_skills_or_modify_agents_md() {
        let (_dir, project) = project();
        let db = database(&project);
        let project_id = db.project_by_path(&project).unwrap().unwrap().id;
        db.set_project_board_enabled(project_id, false).unwrap();
        let agents = project.join("AGENTS.md");
        let original = b"team-owned instructions\r\n";
        std::fs::write(&agents, original).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&project)
            .output()
            .unwrap();

        assert!(install(&db, &project).unwrap().is_empty());
        assert!(board::ensure_enabled_file(&db, &project).is_err());
        assert!(!board::file_path(&project).exists());
        assert_eq!(std::fs::read(&agents).unwrap(), original);
        assert!(!project.join(".opencode/skills/board/SKILL.md").exists());
        assert!(!project.join(".claude/skills/board/SKILL.md").exists());
        assert!(install_git_hook(&db, &project).unwrap().is_none());
        assert!(!project.join(".git/hooks/pre-commit").exists());
        assert!(!project.join(".radar.toml").exists());
    }

    #[test]
    fn disabled_project_allows_edits_and_commits_without_a_board_claim() {
        let (_dir, project) = project();
        let db = database(&project);
        let project_id = db.project_by_path(&project).unwrap().unwrap().id;
        db.set_project_board_enabled(project_id, false).unwrap();
        assert_eq!(
            guard_decision(
                &db,
                &project,
                Some("claude-1"),
                Some(Path::new("src/main.rs"))
            ),
            GuardDecision::Allow
        );
        assert_eq!(
            commit_decision(&db, &project, Some("claude-1")),
            GuardDecision::Allow
        );
    }

    #[test]
    fn the_guard_allows_the_board_itself_and_harness_files() {
        let (_dir, project) = project();
        for file in [
            "BOARD.md",
            ".opencode/skills/board/SKILL.md",
            ".git/COMMIT_EDITMSG",
        ] {
            assert!(
                claimable_path(&project, Path::new(file)),
                "{file} should be claimable"
            );
        }
        assert!(!claimable_path(&project, Path::new("src/main.rs")));
        // Absolute paths, the way hooks report them.
        assert!(claimable_path(&project, &project.join("BOARD.md")));
        assert!(!claimable_path(&project, &project.join("src/main.rs")));
        assert!(claimable_path(&project, Path::new("/etc/passwd")));
    }

    #[test]
    fn the_guard_denies_until_a_claim_exists_then_allows() {
        let (_dir, project) = project();
        let db = database(&project);
        board::ensure_file(&project).unwrap();
        board::add_card(&project, None, "task", "", None).unwrap();

        let src = Path::new("src/main.rs");
        assert!(matches!(
            guard_decision(&db, &project, Some("claude-1"), Some(src)),
            GuardDecision::Deny(_)
        ));

        board::next_card(&project, "claude-1", None).unwrap();
        assert_eq!(
            guard_decision(&db, &project, Some("claude-1"), Some(src)),
            GuardDecision::Allow
        );
        board::finish_card(&project, "task").unwrap();
        assert!(matches!(
            guard_decision(&db, &project, Some("claude-1"), Some(src)),
            GuardDecision::Deny(_)
        ));
    }

    #[test]
    fn the_guard_never_blocks_what_it_cannot_judge() {
        let (_dir, project) = project();
        let db = database(&project);
        board::ensure_file(&project).unwrap();
        assert_eq!(
            guard_decision(&db, &project, None, Some(Path::new("src/main.rs"))),
            GuardDecision::Allow
        );
        assert_eq!(
            guard_decision(&db, &project, Some("claude-1"), None),
            GuardDecision::Allow
        );
    }

    #[test]
    fn the_commit_gate_asks_only_for_a_claim() {
        let (_dir, project) = project();
        let db = database(&project);
        board::ensure_file(&project).unwrap();
        board::add_card(&project, None, "task", "", None).unwrap();

        assert_eq!(commit_decision(&db, &project, None), GuardDecision::Allow);
        assert!(matches!(
            commit_decision(&db, &project, Some("claude-1")),
            GuardDecision::Deny(_)
        ));

        board::next_card(&project, "claude-1", None).unwrap();
        assert_eq!(
            commit_decision(&db, &project, Some("claude-1")),
            GuardDecision::Allow
        );
        board::move_card(&project, "task", "Review").unwrap();
        assert!(matches!(
            commit_decision(&db, &project, Some("claude-1")),
            GuardDecision::Deny(_)
        ));
    }

    #[test]
    fn the_git_hook_is_installed_once_and_never_clobbers() {
        let (_dir, project) = project();
        let db = database(&project);
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&project)
            .output()
            .unwrap();

        // A repository gets the gate; a plain directory gets nothing.
        let hook = install_git_hook(&db, &project).unwrap().unwrap();
        assert_eq!(hook, project.join(".git/hooks/pre-commit"));
        let text = std::fs::read_to_string(&hook).unwrap();
        assert!(text.contains(GIT_HOOK_MARKER));
        assert!(text.contains("radar hook guard --commit"));
        assert!(text.contains("exit 0"), "a missing radar must fail open");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&hook).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "the hook must be executable");

        // A hook radar did not write is never touched.
        let foreign = project.join(".git/hooks/pre-commit");
        std::fs::write(&foreign, "#!/bin/sh\n# mine\necho linting\n").unwrap();
        assert!(install_git_hook(&db, &project).unwrap().is_none());
        assert_eq!(
            std::fs::read_to_string(&foreign).unwrap(),
            "#!/bin/sh\n# mine\necho linting\n"
        );

        // ...but radar's own hook is refreshed in place.
        std::fs::write(&foreign, format!("{GIT_HOOK_MARKER}\nold body\n")).unwrap();
        assert_eq!(install_git_hook(&db, &project).unwrap().unwrap(), hook);
        assert!(std::fs::read_to_string(&hook).unwrap().contains(GIT_HOOK));
    }

    #[test]
    fn a_non_repository_has_nothing_to_gate() {
        let (_dir, plain) = project();
        let db = database(&plain);
        assert!(install_git_hook(&db, &plain).unwrap().is_none());
    }
}
