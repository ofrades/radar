//! Git status for the sidebar.
//!
//! Just enough git: which branch, how far ahead/behind, and how many files
//! changed. Everything is read-only and never takes a lock.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// Summary shown next to a project in the sidebar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub is_repo: bool,
    pub root: Option<PathBuf>,
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub changed: u32,
    pub dirty: bool,
}

impl Status {
    pub fn not_a_repo() -> Status {
        Status {
            is_repo: false,
            root: None,
            branch: None,
            ahead: 0,
            behind: 0,
            changed: 0,
            dirty: false,
        }
    }

    /// Compact form for the sidebar's right hand side, e.g. `main ●3 ↑1`.
    pub fn summary(&self) -> String {
        if !self.is_repo {
            return "no repo".to_string();
        }
        let mut parts = vec![self.branch.clone().unwrap_or_else(|| "detached".into())];
        if self.changed > 0 {
            parts.push(format!("●{}", self.changed));
        }
        if self.ahead > 0 {
            parts.push(format!("↑{}", self.ahead));
        }
        if self.behind > 0 {
            parts.push(format!("↓{}", self.behind));
        }
        parts.join(" ")
    }
}

/// Read Git status within the selected project folder, never above it.
///
/// Returns a "not a repo" status rather than an error: a project without git is
/// a normal thing to have in the sidebar.
pub fn status(dir: impl AsRef<Path>) -> Status {
    let dir = dir.as_ref();
    if !dir.is_dir() {
        return Status::not_a_repo();
    }
    let output = git(
        dir,
        &["status", "--porcelain=v1", "-b", "--untracked-files=normal"],
    );
    let Some(output) = output else {
        return Status::not_a_repo();
    };
    let mut status = parse(&output);
    status.root = rev_parse_root(dir);
    status
}

/// Repository root within `dir`; an ancestor repository is not this project.
pub fn rev_parse_root(dir: impl AsRef<Path>) -> Option<PathBuf> {
    let dir = dir.as_ref();
    let output = git(dir, &["rev-parse", "--show-toplevel"])?;
    let trimmed = output.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// Run git, read-only and lock-free.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let dir = std::fs::canonicalize(dir).ok()?;
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .args(args)
        .current_dir(&dir)
        // A folder project may contain several independent service repos.
        // Never let discovery climb into a repository containing that folder.
        .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap_or(&dir))
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Parse `git status --porcelain=v1 -b`.
fn parse(output: &str) -> Status {
    let mut status = Status {
        is_repo: true,
        root: None,
        branch: None,
        ahead: 0,
        behind: 0,
        changed: 0,
        dirty: false,
    };
    for line in output.lines() {
        if let Some(head) = line.strip_prefix("## ") {
            let (branch, rest) = match head.split_once("...") {
                Some((branch, rest)) => (branch, rest),
                None => (head, ""),
            };
            let branch = branch.trim();
            status.branch = if branch.starts_with("HEAD (no branch)") {
                None
            } else if let Some(name) = branch.strip_prefix("No commits yet on ") {
                Some(name.trim().to_string())
            } else if let Some(name) = branch.strip_prefix("Initial commit on ") {
                Some(name.trim().to_string())
            } else {
                Some(branch.to_string())
            };
            status.ahead = extract_count(rest, "ahead ");
            status.behind = extract_count(rest, "behind ");
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        if line.len() >= 2 {
            status.changed += 1;
        }
    }
    status.dirty = status.changed > 0;
    status
}

fn extract_count(text: &str, needle: &str) -> u32 {
    let Some(start) = text.find(needle) else {
        return 0;
    };
    text[start + needle.len()..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(dir: &Path) {
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t.t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t.t")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            assert!(ok, "git {:?} failed", args);
        };
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "initial"]);
    }

    #[test]
    fn clean_repo_reports_branch() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let status = status(dir.path());
        assert!(status.is_repo);
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.changed, 0);
        assert!(!status.dirty);
        assert_eq!(
            status.root.as_deref(),
            std::fs::canonicalize(dir.path()).ok().as_deref()
        );
    }

    #[test]
    fn counts_modified_and_untracked_files() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        std::fs::write(dir.path().join("new.txt"), "hi\n").unwrap();
        let status = status(dir.path());
        assert_eq!(status.changed, 2);
        assert!(status.dirty);
        assert_eq!(status.summary(), "main ●2");
    }

    #[test]
    fn a_directory_without_git_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let status = status(dir.path());
        assert!(!status.is_repo);
        assert_eq!(status.summary(), "no repo");
    }

    #[test]
    fn missing_directory_is_not_a_repo() {
        let status = status("/definitely/not/here");
        assert!(!status.is_repo);
    }

    #[test]
    fn folder_project_does_not_inherit_an_ancestor_repository() {
        let parent = tempfile::tempdir().unwrap();
        init_repo(parent.path());
        let folder = parent.path().join("neuraspace");
        std::fs::create_dir(&folder).unwrap();
        assert!(!status(&folder).is_repo);
        assert_eq!(rev_parse_root(&folder), None);
    }

    #[test]
    fn service_repository_remains_visible_inside_a_folder_project() {
        let parent = tempfile::tempdir().unwrap();
        init_repo(parent.path());
        let folder = parent.path().join("neuraspace");
        let service = folder.join("web.platform");
        std::fs::create_dir_all(&service).unwrap();
        init_repo(&service);
        assert!(!status(&folder).is_repo);
        assert!(status(&service).is_repo);
        assert_eq!(
            rev_parse_root(&service),
            Some(service.canonicalize().unwrap())
        );
    }

    #[test]
    fn parses_ahead_and_behind() {
        let status = parse("## main...origin/main [ahead 2, behind 3]\n M a.txt\n");
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.ahead, 2);
        assert_eq!(status.behind, 3);
        assert_eq!(status.changed, 1);
        assert_eq!(status.summary(), "main ●1 ↑2 ↓3");
    }

    #[test]
    fn parses_a_fresh_repository_without_commits() {
        let status = parse("## No commits yet on main\n?? a.txt\n");
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.changed, 1);
    }

    #[test]
    fn parses_detached_head() {
        let status = parse("## HEAD (no branch)\n");
        assert!(status.is_repo);
        assert_eq!(status.branch, None);
        assert_eq!(status.summary(), "detached");
    }
}
