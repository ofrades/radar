//! Paths and user configuration.

use std::path::{Path, PathBuf};

/// Where atlas keeps its state.
///
/// `ATLAS_HOME` overrides everything, which is what the tests and the
/// `--home` flag use.
#[derive(Debug, Clone)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
}

impl Paths {
    /// Resolve from the environment, creating nothing.
    pub fn resolve() -> Paths {
        if let Ok(home) = std::env::var("ATLAS_HOME") {
            let dir = PathBuf::from(home);
            return Paths {
                config_dir: dir.clone(),
                data_dir: dir,
            };
        }
        let data_dir = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("~/.local/share"))
            .join("atlas");
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("~/.config"))
            .join("atlas");
        Paths {
            data_dir,
            config_dir,
        }
    }

    pub fn with_root(root: impl Into<PathBuf>) -> Paths {
        let dir = root.into();
        Paths {
            config_dir: dir.clone(),
            data_dir: dir,
        }
    }

    /// `~/.local/share/atlas/atlas.db`
    pub fn database(&self) -> PathBuf {
        self.data_dir.join("atlas.db")
    }

    /// Create data and config directories if they are missing.
    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(&self.config_dir)?;
        Ok(())
    }

    /// omarchy's record of the chosen default agent, if any.
    pub fn omarchy_default_agent(&self) -> Option<String> {
        let file = dirs::home_dir()?.join(".config/omarchy/defaults/agent");
        read_trimmed(&file)
    }
}

fn read_trimmed(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// The directory `atlas add` starts from.
pub fn default_project_root() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let work = home.join("Work");
    if work.is_dir() {
        work
    } else {
        home
    }
}

/// `$SHELL`, falling back to a sane default.
pub fn login_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

/// `$EDITOR`, if it is a single executable word we can run.
pub fn preferred_editor_from_env() -> Option<String> {
    let editor = std::env::var("EDITOR").ok()?;
    let editor = editor.trim();
    if editor.is_empty() || editor.contains(char::is_whitespace) {
        return None;
    }
    which(editor).map(|_| editor.to_string())
}

/// Resolve a program on `PATH`.
pub fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return path.is_file().then_some(path);
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(program);
        if candidate.is_file() && is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Is a program on `PATH`?
pub fn have(program: &str) -> bool {
    which(program).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn have_finds_common_tools() {
        assert!(have("sh"));
        assert!(!have("definitely-not-a-real-program-xyz"));
    }

    #[test]
    fn which_resolves_an_absolute_path() {
        assert!(which("/bin/sh").is_some());
        assert!(which("/nope/nope").is_none());
    }

    #[test]
    fn paths_use_atlas_home_when_set() {
        std::env::set_var("ATLAS_HOME", "/tmp/atlas-test-home");
        let paths = Paths::resolve();
        assert_eq!(paths.database(), PathBuf::from("/tmp/atlas-test-home/atlas.db"));
        std::env::remove_var("ATLAS_HOME");
    }

    #[test]
    fn default_root_prefers_work_dir() {
        let root = default_project_root();
        let home = dirs::home_dir().unwrap();
        let work = home.join("Work");
        if work.is_dir() {
            assert_eq!(root, work);
        } else {
            assert_eq!(root, home);
        }
    }
}
