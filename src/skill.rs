//! The board convention: how agents are told about the board, and how the
//! board becomes a requirement rather than a suggestion.
//!
//! Two halves, one idea — an agent should meet the board at the moment it
//! starts working, and stumble the moment it tries to skip it:
//!
//! - [`install`] writes the convention where each agent harness looks for
//!   instructions: an agent skill for opencode and Claude Code, a pointer in
//!   `AGENTS.md` for the harnesses that only read that. radar runs it when an
//!   agent pane opens, so the files are always there before the agent is.
//! - [`guard_decision`] answers the harness hooks' one question — may this
//!   edit land? A tool call editing a project file without a live board claim
//!   is denied, with a reason that tells the agent exactly how to comply. The
//!   guard is a guardrail, not a wall: `BOARD.md` itself, agent-harness
//!   directories, and agents launched outside radar are always allowed
//!   through, because a guard that blocks its own remedy is a deadlock.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The canonical skill: written once, in radar, so every project teaches its
/// agents the same loop. Frontmatter is the agent-skill format both opencode
/// and Claude Code read.
pub const SKILL_MD: &str = r#"---
name: board
description: Claim work from the project's radar board (BOARD.md) before editing files, and hand finished work back through it. Required for every non-trivial change.
---

# The board

This project coordinates work through a kanban in the repo root: BOARD.md.
radar renders it, the `radar card` commands edit it atomically, and the file
can be read and edited with the tools you already have. Other agents may be
working in this repository at the same time — the board is how we stay out of
each other's way.

## The loop

1. Claim work before editing anything:
   `radar card next --by "$RADAR_AGENT"` — claims the first unclaimed card in
   your name and prints it.
2. Work one card at a time, and only what the card describes.
3. Blocked, or the card is wrong? Append a note saying why (an indented line
   under the card in BOARD.md), release with
   `radar card release --title "…"`, claim the next card.
4. Finished? Leave a short handover note for the reviewer under the card —
   what changed, how to check it — then move the card to Review:
   `radar card move --title "…" --to Review`. Moving hands your claim over:
   from that moment the card is not yours.

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
- Editing BOARD.md itself is always allowed — claims are managed on it.

If `$RADAR_AGENT` is unset you were not launched by radar: pick a short unique
name for `--by` (your model name plus a suffix) and follow the same loop.
"#;

/// The one line added to a project's `AGENTS.md`, for harnesses that only read
/// that. The marker makes the append idempotent.
const AGENTS_POINTER: &str = "## The board (required)\n\n\
    Claim work before editing files, and hand finished work back through the\n\
    board: read `BOARD.md`, or run `radar card next --by \"$RADAR_AGENT\"`.\n\
    The skill `.opencode/skills/board/SKILL.md` has the full loop.\n";

const AGENTS_MARKER: &str = "## The board (required)";

/// The skill files for each harness flavour, relative to the project root:
/// the same body, wherever that harness looks for instructions.
fn skill_files(project: &Path) -> Vec<PathBuf> {
    vec![
        project.join(".opencode/skills/board/SKILL.md"),
        project.join(".claude/skills/board/SKILL.md"),
    ]
}

/// Write the skill into the project, idempotently: files whose content already
/// matches are left alone (so an agent's editor does not see them change), and
/// an `AGENTS.md` gains the pointer once. Returns the paths written.
pub fn install(project: &Path) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for path in skill_files(project) {
        write_if_changed(&path, SKILL_MD)?;
        written.push(path);
    }
    let agents = project.join("AGENTS.md");
    let existing = std::fs::read_to_string(&agents).unwrap_or_default();
    if !existing.contains(AGENTS_MARKER) {
        let mut text = existing;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(AGENTS_POINTER);
        std::fs::write(&agents, text)
            .with_context(|| format!("updating {}", agents.display()))?;
    }
    written.push(agents);
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
pub fn guard_decision(project: &Path, who: Option<&str>, file: Option<&Path>) -> GuardDecision {
    let Some(who) = who else {
        return GuardDecision::Allow; // not launched by radar: the skill is the convention
    };
    let Some(file) = file else {
        return GuardDecision::Allow; // nothing we can judge (a bash call, say)
    };
    if claimable_path(project, file) {
        return GuardDecision::Allow; // the board itself, harness files, git internals
    }
    match crate::board::holds_claim(project, who) {
        Ok(true) => GuardDecision::Allow,
        _ => GuardDecision::Deny(format!(
            "no board claim — claim work before editing files:\n  \
             radar card next --by \"{who}\"\n  \
             (or review what is waiting: radar card next --in Review --by \"{who}\")\n\
             If this task is not board work, add a card for it and claim that.\n\
             Editing BOARD.md itself is always allowed."
        )),
    }
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

    #[test]
    fn install_writes_every_flavour_and_is_idempotent() {
        let (_dir, project) = project();
        let files = install(&project).unwrap();
        assert_eq!(files.len(), 3);
        for path in &files {
            assert!(path.exists(), "{} was not written", path.display());
        }
        let skill = std::fs::read_to_string(project.join(".opencode/skills/board/SKILL.md")).unwrap();
        assert_eq!(skill, SKILL_MD);
        let claude = std::fs::read_to_string(project.join(".claude/skills/board/SKILL.md")).unwrap();
        assert_eq!(claude, SKILL_MD);
        let agents = std::fs::read_to_string(project.join("AGENTS.md")).unwrap();
        assert!(agents.contains(AGENTS_MARKER));

        // A second install changes nothing — same content, one pointer.
        let before = std::fs::read_to_string(project.join("AGENTS.md")).unwrap();
        let mtime = std::fs::metadata(project.join(".claude/skills/board/SKILL.md"))
            .unwrap()
            .modified()
            .unwrap();
        install(&project).unwrap();
        assert_eq!(std::fs::read_to_string(project.join("AGENTS.md")).unwrap(), before);
        assert_eq!(
            std::fs::metadata(project.join(".claude/skills/board/SKILL.md"))
                .unwrap()
                .modified()
                .unwrap(),
            mtime,
            "an unchanged skill file was rewritten"
        );
    }

    #[test]
    fn install_appends_to_an_existing_agents_md_without_touching_it_elsewhere() {
        let (_dir, project) = project();
        std::fs::write(project.join("AGENTS.md"), "# House rules\n\nBe brief.\n").unwrap();
        install(&project).unwrap();
        let text = std::fs::read_to_string(project.join("AGENTS.md")).unwrap();
        assert!(text.starts_with("# House rules\n\nBe brief.\n"));
        assert!(text.contains(AGENTS_MARKER));
    }

    #[test]
    fn the_guard_allows_the_board_itself_and_harness_files() {
        let (_dir, project) = project();
        for file in ["BOARD.md", ".opencode/skills/board/SKILL.md", ".git/COMMIT_EDITMSG"] {
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
        board::ensure_file(&project).unwrap();
        board::add_card(&project, None, "task", "", None).unwrap();

        let src = Path::new("src/main.rs");
        assert!(matches!(
            guard_decision(&project, Some("claude-1"), Some(src)),
            GuardDecision::Deny(_)
        ));

        board::next_card(&project, "claude-1", None).unwrap();
        assert_eq!(
            guard_decision(&project, Some("claude-1"), Some(src)),
            GuardDecision::Allow
        );
        // A done card no longer holds the guard open.
        board::finish_card(&project, "task").unwrap();
        assert!(matches!(
            guard_decision(&project, Some("claude-1"), Some(src)),
            GuardDecision::Deny(_)
        ));
    }

    #[test]
    fn the_guard_never_blocks_what_it_cannot_judge() {
        let (_dir, project) = project();
        board::ensure_file(&project).unwrap();
        // No RADAR_AGENT: not launched by radar, the skill is the convention.
        assert_eq!(
            guard_decision(&project, None, Some(Path::new("src/main.rs"))),
            GuardDecision::Allow
        );
        // No file (a shell call): nothing to judge.
        assert_eq!(
            guard_decision(&project, Some("claude-1"), None),
            GuardDecision::Allow
        );
    }
}
