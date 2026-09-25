//! Finding directories to add.
//!
//! The add-project picker is a fuzzy list over the directories near a root
//! (`~/Work` by default). We use `fd` when it is installed — it is the fastest
//! way to enumerate and it is what most people on this kind of setup have —
//! and fall back to a walk that prunes hard.

use std::path::{Path, PathBuf};
use std::process::Command;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use serde::{Deserialize, Serialize};

/// Directories never worth offering as a project, or descending into.
pub const SKIP_DIRS: &[&str] = &[
    ".git", ".hg", ".svn", "node_modules", "target", "dist", "build", "out", "vendor",
    "__pycache__", ".venv", "venv", ".cache", ".local", ".npm", ".cargo", ".rustup",
    ".gradle", ".next", ".nuxt", ".turbo", ".parcel-cache", "coverage", ".pytest_cache",
    ".mypy_cache", "site-packages", ".direnv", "tmp", ".Trash", "Library",
];

/// A directory that could become a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub path: PathBuf,
    pub name: String,
    /// Depth below the scanned root.
    pub depth: usize,
    /// Contains a `.git` entry: worth showing first.
    pub is_repo: bool,
    /// Already in the project list.
    pub known: bool,
}

impl Candidate {
    pub fn display_path(&self) -> String {
        crate::db::abbreviate(&self.path)
    }

    /// Sort key: repositories first, then shallower, then name.
    pub fn rank(&self) -> (bool, usize, String) {
        (!self.is_repo, self.depth, self.name.to_lowercase())
    }
}

/// Which scanner is available on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scanner {
    Fd,
    Walk,
}

pub fn scanner() -> Scanner {
    if crate::config::have("fd") {
        Scanner::Fd
    } else {
        Scanner::Walk
    }
}

/// Enumerate candidate directories under `root`.
///
/// Directories that are themselves repositories are not descended into: their
/// subdirectories belong to that project, not to the sidebar.
pub fn scan(root: impl AsRef<Path>, max_depth: usize, limit: usize) -> Vec<Candidate> {
    let root = root.as_ref();
    if !root.is_dir() {
        return Vec::new();
    }
    let raw = match scanner() {
        Scanner::Fd => match scan_with_fd(root, max_depth, limit) {
            Ok(found) if !found.is_empty() => found,
            // fd refused (a flag it does not know, say): fall back rather than
            // showing an empty list.
            _ => scan_with_walk(root, max_depth, limit),
        },
        Scanner::Walk => scan_with_walk(root, max_depth, limit),
    };
    let mut candidates: Vec<Candidate> = raw
        .into_iter()
        .filter(|candidate| !is_inside_repo(root, &candidate.path))
        .collect();
    candidates.sort_by_key(Candidate::rank);
    candidates.truncate(limit);
    candidates
}

/// Is any directory between `path` and `root` a repository?
///
/// `worker` itself is fine (it *is* a repository, so it is a project), while
/// `worker/src` is not: it belongs to `worker`.
fn is_inside_repo(root: &Path, path: &Path) -> bool {
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir == root || !dir.starts_with(root) {
            return false;
        }
        if dir.join(".git").exists() {
            return true;
        }
        current = dir.parent();
    }
    false
}

fn scan_with_fd(root: &Path, max_depth: usize, limit: usize) -> Result<Vec<Candidate>, ()> {
    let mut command = Command::new("fd");
    command
        .arg("--type")
        .arg("d")
        .arg("--max-depth")
        .arg(max_depth.to_string())
        .arg("--hidden")
        .arg("--no-ignore-vcs")
        .arg("--print0")
        .arg("--color")
        .arg("never");
    for skip in SKIP_DIRS {
        command.arg("--exclude").arg(skip);
    }
    command.arg(".").arg(root);
    let output = command.output().map_err(|_| ())?;
    if !output.status.success() {
        return Err(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut candidates = Vec::new();
    for entry in stdout.split('\0').filter(|s| !s.is_empty()) {
        let path = PathBuf::from(entry);
        if path == *root {
            continue;
        }
        candidates.push(make_candidate(root, &path));
        if candidates.len() >= limit {
            break;
        }
    }
    Ok(candidates)
}

fn scan_with_walk(root: &Path, max_depth: usize, limit: usize) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    let walker = walkdir::WalkDir::new(root)
        .max_depth(max_depth)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if !entry.file_type().is_dir() {
                return false;
            }
            if entry.depth() == 0 {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            !SKIP_DIRS.iter().any(|skip| *skip == name)
        });
    for entry in walker.flatten() {
        if entry.depth() == 0 || !entry.file_type().is_dir() {
            continue;
        }
        candidates.push(make_candidate(root, entry.path()));
        if candidates.len() >= limit {
            break;
        }
    }
    candidates
}

fn make_candidate(root: &Path, path: &Path) -> Candidate {
    // `fd` marks directories with a trailing slash; keep paths canonical.
    let cleaned: PathBuf = {
        let text = path.to_string_lossy();
        if text.len() > 1 && text.ends_with('/') {
            PathBuf::from(&text[..text.len() - 1])
        } else {
            path.to_path_buf()
        }
    };
    let path = cleaned.as_path();
    let depth = path
        .strip_prefix(root)
        .map(|rest| rest.components().count())
        .unwrap_or(0);
    Candidate {
        path: path.to_path_buf(),
        name: path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string()),
        depth,
        is_repo: path.join(".git").exists(),
        known: false,
    }
}

/// Mark candidates that are already in the sidebar.
pub fn mark_known(candidates: &mut [Candidate], known_paths: &[PathBuf]) {
    for candidate in candidates.iter_mut() {
        candidate.known = known_paths.iter().any(|known| known == &candidate.path);
    }
}

/// Fuzzy filter, best match first. An empty query keeps the ranked order.
pub fn filter(candidates: &[Candidate], query: &str) -> Vec<Candidate> {
    let query = query.trim();
    if query.is_empty() {
        let mut all = candidates.to_vec();
        all.sort_by_key(Candidate::rank);
        return all;
    }
    let matcher = SkimMatcherV2::default().ignore_case();
    let mut scored: Vec<(i64, Candidate)> = candidates
        .iter()
        .filter_map(|candidate| {
            let name_score = matcher.fuzzy_match(&candidate.name, query);
            let path_score = matcher
                .fuzzy_match(&candidate.display_path(), query)
                .map(|score| score - 20);
            let score = name_score.max(path_score.or(name_score))?;
            Some((score, candidate.clone()))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.rank().cmp(&b.1.rank()))
    });
    scored.into_iter().map(|(_, candidate)| candidate).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for path in [
            "api-server",
            "web-app",
            "infra/terraform",
            "infra/k8s",
            "node_modules/pkg",
            "api-server/target/debug",
        ] {
            std::fs::create_dir_all(root.join(path)).unwrap();
        }
        std::fs::create_dir_all(root.join("worker/.git")).unwrap();
        std::fs::create_dir_all(root.join("worker/src")).unwrap();
        dir
    }

    #[test]
    fn scan_finds_directories_and_skips_noise() {
        let dir = fixture();
        let found = scan(dir.path(), 3, 100);
        let names: Vec<&str> = found.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"api-server"));
        assert!(names.contains(&"web-app"));
        assert!(names.contains(&"terraform"), "{names:?}");
        assert!(!names.contains(&"node_modules"), "noise must be skipped");
        assert!(!names.contains(&"debug"), "target must be skipped");
    }

    #[test]
    fn scan_respects_max_depth() {
        let dir = fixture();
        let shallow = scan(dir.path(), 1, 100);
        assert!(shallow.iter().all(|c| c.depth <= 1));
        assert!(shallow.iter().any(|c| c.name == "api-server"));
        assert!(!shallow.iter().any(|c| c.name == "terraform"));
    }

    #[test]
    fn scan_marks_repositories() {
        let dir = fixture();
        let found = scan(dir.path(), 3, 100);
        let worker = found.iter().find(|c| c.name == "worker").expect("worker");
        assert!(worker.is_repo);
        assert!(!found.iter().find(|c| c.name == "web-app").unwrap().is_repo);
    }

    #[test]
    fn scan_does_not_descend_into_repositories() {
        let dir = fixture();
        let found = scan(dir.path(), 4, 100);
        let names: Vec<&str> = found.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"worker"), "the repo itself is a project");
        assert!(!names.contains(&"src"), "files inside a repo belong to it");
    }

    #[test]
    fn scan_marks_known_paths() {
        let dir = fixture();
        let mut found = scan(dir.path(), 2, 100);
        let target = found
            .iter()
            .find(|c| c.name == "api-server")
            .unwrap()
            .path
            .clone();
        mark_known(&mut found, std::slice::from_ref(&target));
        assert!(found.iter().find(|c| c.path == target).unwrap().known);
    }

    #[test]
    fn scan_of_a_missing_root_is_empty_not_an_error() {
        assert!(scan("/definitely/not/here", 2, 10).is_empty());
    }

    #[test]
    fn filter_is_fuzzy_and_ranks_the_best_match_first() {
        let candidates: Vec<Candidate> = ["api-server", "web-app", "infrastructure"]
            .iter()
            .map(|name| Candidate {
                path: PathBuf::from(format!("/tmp/{name}")),
                name: name.to_string(),
                depth: 1,
                is_repo: false,
                known: false,
            })
            .collect();
        let hits = filter(&candidates, "api");
        assert_eq!(hits[0].name, "api-server");
        // Subsequence matching: w-a-p exists in "web-app".
        let scattered = filter(&candidates, "wap");
        assert_eq!(scattered[0].name, "web-app", "subsequence matching should work");
        assert!(filter(&candidates, "zzzz").is_empty());
        assert!(filter(&candidates, "app").iter().any(|c| c.name == "web-app"));
    }

    #[test]
    fn filter_can_match_the_path() {
        let candidates = vec![
            Candidate {
                path: PathBuf::from("/tmp/work/infra/terraform"),
                name: "terraform".to_string(),
                depth: 2,
                is_repo: false,
                known: false,
            },
            Candidate {
                path: PathBuf::from("/tmp/work/web"),
                name: "web".to_string(),
                depth: 1,
                is_repo: false,
                known: false,
            },
        ];
        let hits = filter(&candidates, "infra");
        assert_eq!(hits[0].name, "terraform");
    }

    #[test]
    fn empty_query_keeps_repositories_first() {
        let candidates = vec![
            Candidate {
                path: PathBuf::from("/tmp/a"),
                name: "a".to_string(),
                depth: 1,
                is_repo: false,
                known: false,
            },
            Candidate {
                path: PathBuf::from("/tmp/z"),
                name: "z".to_string(),
                depth: 1,
                is_repo: true,
                known: false,
            },
        ];
        assert_eq!(filter(&candidates, "")[0].name, "z");
    }

    #[test]
    fn scanner_reports_something_usable() {
        assert!(matches!(scanner(), Scanner::Fd | Scanner::Walk));
    }
}
