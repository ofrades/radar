//! Paths and user configuration.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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
            if std::fs::copy(&candidate, &target).is_err() {
                continue;
            }
            // SQLite keeps recent rows in the write-ahead log; copying the
            // database alone would leave them behind in the old directory.
            for suffix in ["-wal", "-shm"] {
                let from = append_suffix(&candidate, suffix);
                if from.is_file() {
                    let _ = std::fs::copy(&from, append_suffix(&target, suffix));
                }
            }
            return;
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

/// `file` + `-wal` — SQLite's write-ahead log next to its database.
fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
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

/// Resolve a program on `PATH`, including the entries mise contributes.
pub fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return path.is_file().then_some(path);
    }
    find_in(&search_entries(), program)
}

/// Resolve a program on the ambient `PATH` alone. Used to find `mise` itself,
/// so it must not consult the entries mise adds.
fn which_ambient(program: &str) -> Option<PathBuf> {
    find_in(&ambient_entries(), program)
}

/// First executable `program` in `entries`.
fn find_in(entries: &[PathBuf], program: &str) -> Option<PathBuf> {
    entries.iter().find_map(|dir| {
        let candidate = dir.join(program);
        (candidate.is_file() && is_executable(&candidate)).then_some(candidate)
    })
}

/// The directories of the ambient `PATH`, in order.
fn ambient_entries() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|dir| !dir.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The directories to search for a program, and to hand a spawned program as
/// its `PATH`: what mise installs, then the ambient `PATH`.
///
/// Radar is often started by a desktop session or a systemd user unit whose
/// `PATH` is not the login shell's, while agents (opencode, omp, claude, …)
/// are installed through mise under the user's home. Prepending mise's bin
/// paths lets radar find and launch those tools however it was started,
/// without shelling out to a login shell. An omarchy wrapper spawned with
/// this `PATH` sees the same tools, so it no longer reports them missing.
pub fn search_entries() -> Vec<PathBuf> {
    let mut entries = mise_bin_paths().to_vec();
    entries.extend(ambient_entries());
    entries
}

/// The `PATH` value to give a spawned program.
pub fn path_value() -> OsString {
    std::env::join_paths(search_entries())
        .unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

/// The bin directories mise reports for the user's installed tools.
///
/// Asked once per process and cached: the answer is the user's global mise
/// configuration, read from the home directory so it does not depend on
/// wherever radar happened to start. Empty when mise is absent or says
/// nothing, in which case the ambient `PATH` stands alone.
fn mise_bin_paths() -> &'static [PathBuf] {
    static PATHS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    PATHS.get_or_init(|| {
        let Some(mise) = mise_binary() else {
            return Vec::new();
        };
        let mut command = std::process::Command::new(mise);
        command.arg("bin-paths");
        if let Some(home) = dirs::home_dir() {
            command.current_dir(home);
        }
        let Ok(output) = command.output() else {
            return Vec::new();
        };
        if !output.status.success() {
            return Vec::new();
        }
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
            .collect()
    })
}

/// Where the `mise` binary is: on the ambient `PATH`, or one of the usual
/// install locations when radar was started without the login shell's.
fn mise_binary() -> Option<PathBuf> {
    if let Some(path) = which_ambient("mise") {
        return Some(path);
    }
    let mut candidates = Vec::new();
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local/bin/mise"));
        candidates.push(home.join(".local/share/mise/bin/mise"));
    }
    candidates.push(PathBuf::from("/usr/bin/mise"));
    candidates.push(PathBuf::from("/usr/local/bin/mise"));
    candidates.into_iter().find(|path| path.is_file())
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
    fn find_in_skips_non_executables() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("tool");
        std::fs::write(&tool, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(find_in(&[dir.path().to_path_buf()], "tool"), None);
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_in(&[dir.path().to_path_buf()], "tool"), Some(tool));
    }

    #[test]
    fn search_entries_prepend_mise_and_keep_the_ambient_path() {
        let ambient = ambient_entries();
        let mise = mise_bin_paths();
        let entries = search_entries();
        assert_eq!(entries.len(), mise.len() + ambient.len());
        assert_eq!(&entries[..mise.len()], mise);
        assert_eq!(&entries[mise.len()..], &ambient[..]);
    }

    #[test]
    fn path_value_is_the_search_entries_joined() {
        let value = path_value();
        let parsed: Vec<PathBuf> = std::env::split_paths(&value).collect();
        assert_eq!(parsed, search_entries());
    }

    #[test]
    fn paths_use_radar_home_when_set() {
        std::env::set_var("RADAR_HOME", "/tmp/radar-test-home");
        let paths = Paths::resolve();
        assert_eq!(
            paths.database(),
            PathBuf::from("/tmp/radar-test-home/radar.db")
        );
        std::env::remove_var("RADAR_HOME");
    }

    #[test]
    fn first_run_adopts_the_old_atlas_database() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path());
        std::fs::write(dir.path().join("atlas.db"), b"old").unwrap();
        std::fs::write(dir.path().join("atlas.db-wal"), b"log").unwrap();

        paths.ensure().unwrap();
        assert_eq!(std::fs::read(paths.database()).unwrap(), b"old");
        assert_eq!(
            std::fs::read(append_suffix(&paths.database(), "-wal")).unwrap(),
            b"log",
            "the write-ahead log has to travel with the database"
        );

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
