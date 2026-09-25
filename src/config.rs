//! Paths and user configuration.

use std::path::{Path, PathBuf};

/// Where radar keeps its state.
///
/// `RADAR_HOME` overrides everything, which is what the tests and the
/// `--home` flag use.
#[derive(Debug, Clone)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
}

impl Paths {
    /// Resolve from the environment, creating nothing.
    pub fn resolve() -> Paths {
        if let Ok(home) = std::env::var("RADAR_HOME") {
            let dir = PathBuf::from(home);
            return Paths {
                config_dir: dir.clone(),
                data_dir: dir,
            };
        }
        let data_dir = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("~/.local/share"))
            .join("radar");
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("~/.config"))
            .join("radar");
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

    /// `~/.local/share/radar/radar.db`
    pub fn database(&self) -> PathBuf {
        self.data_dir.join("radar.db")
    }

    /// Create data and config directories if they are missing.
    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(&self.config_dir)?;
        self.adopt_legacy_database();
        Ok(())
    }

    /// Carry the database over from the days the project was called `atlas`.
    ///
    /// Nothing is deleted: the old file stays where it is, so a copy is made
    /// only while `radar.db` does not exist yet.
    fn adopt_legacy_database(&self) {
        let target = self.database();
        if target.exists() {
            return;
        }
        for candidate in self.legacy_databases() {
            if candidate == target || !candidate.is_file() {
                continue;
            }
            if std::fs::copy(&candidate, &target).is_ok() {
                return;
            }
        }
    }

    /// Everywhere an `atlas.db` could be sitting.
    fn legacy_databases(&self) -> Vec<PathBuf> {
        let mut candidates = vec![self.data_dir.join("atlas.db")];
        if let Ok(home) = std::env::var("ATLAS_HOME") {
            candidates.push(PathBuf::from(home).join("atlas.db"));
        } else if let Some(dir) = dirs::data_dir() {
            candidates.push(dir.join("atlas").join("atlas.db"));
        }
        candidates
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

/// The directory `radar add` starts from: the home directory.
///
/// Whichever directory you pick in the add dialog is remembered in settings
/// (`ui.add_root`), so this is only the starting point.
pub fn default_project_root() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
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
    fn paths_use_radar_home_when_set() {
        std::env::set_var("RADAR_HOME", "/tmp/radar-test-home");
        let paths = Paths::resolve();
        assert_eq!(paths.database(), PathBuf::from("/tmp/radar-test-home/radar.db"));
        std::env::remove_var("RADAR_HOME");
    }

    #[test]
    fn first_run_adopts_the_old_atlas_database() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path());
        std::fs::write(dir.path().join("atlas.db"), b"old").unwrap();

        paths.ensure().unwrap();
        assert_eq!(std::fs::read(paths.database()).unwrap(), b"old");

        // Once radar has its own database, the legacy copy must not win again.
        std::fs::write(paths.database(), b"new").unwrap();
        paths.ensure().unwrap();
        assert_eq!(std::fs::read(paths.database()).unwrap(), b"new");
    }

    #[test]
    fn default_root_is_the_home_directory() {
        assert_eq!(default_project_root(), dirs::home_dir().unwrap());
    }
}
