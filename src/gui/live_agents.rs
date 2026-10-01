use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::Context;
use serde::Deserialize;

use crate::db::Project;
use crate::programs::{self, Program};
use crate::session::catalog::{self, CatalogFilter};
use crate::session::daemon::{self, Client, Command, Response};
use crate::session::registry::{Lifecycle, Status};

use super::{parse_stable_session_id, Slot};

pub(super) const ACTIVE_CATALOG_LIMIT: usize = 500;
pub(super) const ARCHIVED_CATALOG_LIMIT: usize = 200;

pub(super) fn catalog_history_is_limited(active_count: usize, archived_count: usize) -> bool {
    active_count >= ACTIVE_CATALOG_LIMIT || archived_count >= ARCHIVED_CATALOG_LIMIT
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AgentSession {
    pub project_id: i64,
    /// Stable identity within the project: the radar session id, a catalog
    /// row (`catalog-<id>`), or an external process (`external-<pid>-<start>`).
    pub id: String,
    pub title: String,
    pub program_id: String,
    pub tab_key: Option<String>,
    pub external: Option<ExternalTarget>,
    /// Catalog row backing this sidebar entry, when one exists.
    pub catalog_id: Option<i64>,
    /// The daemon session this entry is attached to, when live.
    pub radar_session_id: Option<String>,
    /// The provider's own conversation id, when known — the precise resume target.
    pub provider_session_id: Option<String>,
    /// Exact `RADAR_AGENT` claim carried by a live process, when readable.
    pub claim_id: Option<String>,
    /// The board to-do the process was launched for (`RADAR_CARD_ID`), when
    /// readable. Stable for the process's life, unlike the card's claim.
    pub card_id: Option<String>,
    /// The process behind the entry, when there is one: a live registry
    /// session's child, or an external process. A new pid under the same
    /// stable id means the session was respawned — views attached to the old
    /// process re-attach.
    pub pid: Option<u32>,
    pub last_activity_at: i64,
    pub running: bool,
    pub archived: bool,
}

/// Whether an entry belongs in the running section of the sidebar.
pub(super) fn sidebar_session_is_live(session: &AgentSession) -> bool {
    session.running || session.external.is_some()
}

fn fallback_title(program_id: &str) -> String {
    programs::by_id(program_id)
        .map(|program| program.name)
        .unwrap_or_else(|| format!("{program_id} session"))
}

/// A live agent row's title. The program's own terminal title wins when it
/// says something real; otherwise the conversation the catalog already
/// knows for this stable session — the previous run's, or a
/// provider-imported one — is what the row is about. A session with no
/// conversation anywhere is honestly new, not "starting": nothing more is
/// coming until it is used, so the placeholder says that.
fn live_row_title(
    status_title: Option<&str>,
    catalog_title: Option<&str>,
    program: Option<&Program>,
) -> String {
    session_title(status_title, program)
        .or_else(|| session_title(catalog_title, program))
        .unwrap_or_else(|| match program {
            Some(program) => format!("New {} session", program.name),
            None => "New session".to_string(),
        })
}

/// The exact-resume conversation id a sidebar row may honestly offer: one
/// the provider itself reported. Radar-created catalog rows backfill
/// radar's own stable session id (e.g. `project-3-agent-2-omp`) until a
/// provider reports a conversation id — that value names radar's tab, not
/// a provider conversation, and resuming it asks the CLI for a session it
/// has never heard of (omp: `Session "project-…-omp" not found`).
pub(super) fn exact_provider_session_id(session: &AgentSession) -> Option<&str> {
    match &session.provider_session_id {
        Some(id) if Some(id.as_str()) != session.radar_session_id.as_deref() => Some(id),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ExternalTarget {
    pub pid: u32,
    pub start_ticks: u64,
    pub window_pid: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DiscoverySnapshot {
    pub sessions: Vec<AgentSession>,
    pub history_limited: bool,
}

#[derive(Debug, Default)]
pub(super) struct SessionIndex {
    pub by_project: HashMap<i64, Vec<AgentSession>>,
    pub history_limited: bool,
    pub has_snapshot: bool,
    pub error: Option<String>,
}

impl SessionIndex {
    /// Replace rows only after a complete discovery succeeds; failures keep
    /// the last usable snapshot and report that it may be stale.
    pub(super) fn apply(&mut self, result: Result<DiscoverySnapshot, String>) -> bool {
        match result {
            Ok(snapshot) => {
                let mut by_project: HashMap<i64, Vec<AgentSession>> = HashMap::new();
                for session in snapshot.sessions {
                    by_project
                        .entry(session.project_id)
                        .or_default()
                        .push(session);
                }
                let changed = by_project != self.by_project;
                self.by_project = by_project;
                self.history_limited = snapshot.history_limited;
                self.has_snapshot = true;
                self.error = None;
                changed
            }
            Err(error) => {
                self.error = Some(error);
                false
            }
        }
    }

    /// What the index wants the human to know: discovery failures, or a
    /// history slice too large to have fetched completely. The sidebar used
    /// to surface this as a banner; the running-only list has no place for
    /// it now, so the strings live here for the tests and the next reader.
    #[allow(dead_code)]
    pub(super) fn notice(&self) -> Option<&'static str> {
        if self.error.is_some() {
            Some(if self.has_snapshot {
                "Session discovery failed; showing the last successful results."
            } else {
                "Session discovery failed; no session snapshot is available."
            })
        } else if self.history_limited {
            Some("Session history is limited; older entries may be omitted.")
        } else {
            None
        }
    }
}

/// Everything a project's sidebar should list: live Radar-managed agents,
/// the durable catalog history, and (Linux) agents running in external
/// terminals. Catalog rows carry the archive flag; rendering decides which
/// slice is visible.
pub(super) fn discover(
    projects: &[Project],
    session_home: &Path,
) -> anyhow::Result<DiscoverySnapshot> {
    let now = crate::session::catalog::now_millis();
    let statuses = match Client::request(session_home, Command::List)
        .context("requesting live sessions from the daemon")?
    {
        Response::Sessions(sessions) => sessions,
        _ => anyhow::bail!("daemon returned an unexpected response to List"),
    };
    let project_ids: HashSet<i64> = projects.iter().map(|project| project.id).collect();
    let roster: Vec<daemon::CatalogProject> = projects
        .iter()
        .map(|project| daemon::CatalogProject {
            id: project.id,
            path: project.path.clone(),
        })
        .collect();
    let entries = |filter, limit| -> anyhow::Result<Vec<catalog::Entry>> {
        match Client::request(
            session_home,
            Command::CatalogList {
                projects: roster.clone(),
                filter,
                query: None,
                limit,
            },
        )? {
            Response::Catalog(entries) => Ok(entries),
            _ => anyhow::bail!("daemon returned an unexpected response to CatalogList"),
        }
    };
    let active_entries = entries(CatalogFilter::Active, ACTIVE_CATALOG_LIMIT as u32)
        .context("requesting active session history")?;
    let archived_entries = entries(CatalogFilter::Archived, ARCHIVED_CATALOG_LIMIT as u32)
        .context("requesting archived session history")?;
    let history_limited = catalog_history_is_limited(active_entries.len(), archived_entries.len());
    let mut catalog_entries = active_entries;
    catalog_entries.extend(archived_entries);

    let mut sessions = Vec::new();
    let mut managed_ids = HashSet::new();
    let mut managed_pids = HashSet::new();
    let mut live_catalog: HashMap<String, catalog::Entry> = catalog_entries
        .iter()
        .filter_map(|entry| {
            entry
                .radar_session_id
                .clone()
                .map(|radar_id| (radar_id, entry.clone()))
        })
        .collect();
    let mut consumed: HashSet<String> = HashSet::new();

    for status in &statuses {
        let Some((project_id, key, program_id)) = parse_stable_session_id(&status.id) else {
            continue;
        };
        if key.slot != Slot::Agent
            || !project_ids.contains(&project_id)
            || status.lifecycle != Lifecycle::Running
        {
            continue;
        }
        managed_ids.insert(status.id.clone());
        if let Some(pid) = status.pid {
            managed_pids.insert(pid);
        }
        let program = programs::by_id(&program_id);
        // A live session the catalog has no row for (pre-catalog session, or
        // the first scan after an upgrade): backfill once and remember it.
        let entry = live_catalog.remove(&status.id);
        let title = live_row_title(
            status.title.as_deref(),
            entry.as_ref().and_then(|entry| entry.title.as_deref()),
            program.as_ref(),
        );
        if entry.is_some() {
            consumed.insert(status.id.clone());
        } else {
            let _ = Client::request(
                session_home,
                Command::CatalogSeen {
                    project_id,
                    radar_id: status.id.clone(),
                    program: program_id.clone(),
                    cwd: status.cwd.clone(),
                },
            );
        }
        sessions.push(AgentSession {
            project_id,
            id: status.id.clone(),
            title,
            program_id,
            tab_key: Some(key.as_str().to_string()),
            external: None,
            catalog_id: entry.as_ref().map(|entry| entry.id),
            claim_id: status.pid.and_then(programs::launch::radar_agent_of),
            pid: status.pid,
            card_id: status
                .pid
                .and_then(programs::launch::radar_card_of)
                .or_else(|| entry.as_ref().and_then(|entry| entry.card_id.clone())),
            radar_session_id: Some(status.id.clone()),
            provider_session_id: entry
                .as_ref()
                .map(|entry| entry.provider_session_id.clone()),
            last_activity_at: entry.map_or(now, |entry| entry.last_activity_at),
            running: true,
            archived: false,
        });
    }

    // Catalog rows without a live counterpart are history: ended runs, or
    // conversations imported from the provider's own store.
    for entry in &catalog_entries {
        if entry
            .radar_session_id
            .as_ref()
            .is_some_and(|radar_id| consumed.contains(radar_id))
        {
            continue;
        }
        if entry.radar_session_id.is_some() && entry.lifecycle == "running" {
            // The registry has not seen this id: it predates this daemon or
            // died with one. Reconcile ends it server-side; skip until then.
            continue;
        }
        let program = programs::by_id(&entry.provider);
        let title = session_title(entry.title.as_deref(), program.as_ref())
            .unwrap_or_else(|| fallback_title(&entry.provider));
        sessions.push(AgentSession {
            project_id: entry.project_id,
            id: format!("catalog-{}", entry.id),
            title,
            program_id: entry.provider.clone(),
            tab_key: None,
            external: None,
            catalog_id: Some(entry.id),
            radar_session_id: entry.radar_session_id.clone(),
            provider_session_id: Some(entry.provider_session_id.clone()),
            claim_id: None,
            pid: None,
            card_id: entry.card_id.clone(),
            last_activity_at: entry.last_activity_at,
            running: entry.lifecycle == "running",
            archived: entry.archived_at.is_some(),
        });
    }

    #[cfg(target_os = "linux")]
    sessions.extend(external_sessions(
        projects,
        &statuses,
        &managed_ids,
        &managed_pids,
        now,
    ));

    Ok(DiscoverySnapshot {
        sessions,
        history_limited,
    })
}

fn session_title(title: Option<&str>, program: Option<&Program>) -> Option<String> {
    let title = title?.trim();
    (!catalog::is_generic_session_title(title, program)).then(|| title.to_string())
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
struct ProcessInfo {
    pid: u32,
    start_ticks: u64,
    cwd: PathBuf,
    argv: Vec<String>,
    program: Program,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Deserialize)]
struct CompositorWindow {
    pid: u32,
    title: String,
    address: String,
}

#[cfg(target_os = "linux")]
fn external_sessions(
    projects: &[Project],
    statuses: &[Status],
    managed_ids: &HashSet<String>,
    managed_pids: &HashSet<u32>,
    now: i64,
) -> Vec<AgentSession> {
    let roots: Vec<(i64, PathBuf)> = projects
        .iter()
        .filter_map(|project| {
            std::fs::canonicalize(&project.path)
                .ok()
                .map(|path| (project.id, path))
        })
        .collect();
    if roots.is_empty() {
        return Vec::new();
    }

    let programs: Vec<Program> = programs::agents::programs();
    let windows = compositor_windows();
    let status_by_pid: HashMap<u32, &Status> = statuses
        .iter()
        .filter_map(|status| status.pid.map(|pid| (pid, status)))
        .collect();
    let mut parents = HashMap::new();
    let mut candidates = Vec::new();

    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let directory = entry.path();
        let Ok(stat) = std::fs::read_to_string(directory.join("stat")) else {
            continue;
        };
        let Some((parent, start_ticks)) = parse_process_stat(&stat) else {
            continue;
        };
        parents.insert(pid, parent);

        let Ok(command_line) = std::fs::read(directory.join("cmdline")) else {
            continue;
        };
        let argv: Vec<String> = command_line
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
            .map(|argument| String::from_utf8_lossy(argument).into_owned())
            .collect();
        let Some(program) = identify_agent(&argv, &programs) else {
            continue;
        };
        if managed_pids.contains(&pid) {
            continue;
        }
        let Ok(cwd) = std::fs::read_link(directory.join("cwd")) else {
            continue;
        };
        let Ok(cwd) = std::fs::canonicalize(cwd) else {
            continue;
        };
        if !roots.iter().any(|(_, root)| cwd.starts_with(root))
            || !has_interactive_terminal(&directory)
        {
            continue;
        }
        let session_id = read_session_id(&directory.join("environ"));
        if session_id
            .as_ref()
            .is_some_and(|id| managed_ids.contains(id))
            || managed_agent_ancestor(pid, &parents, &status_by_pid, managed_ids)
        {
            continue;
        }
        candidates.push(ProcessInfo {
            pid,
            start_ticks,
            cwd,
            argv,
            program,
        });
    }

    let mut sessions = Vec::with_capacity(candidates.len());
    for process in candidates {
        let Some((project_id, _)) = roots
            .iter()
            .filter(|(_, root)| process.cwd.starts_with(root))
            .max_by_key(|(_, root)| root.components().count())
        else {
            continue;
        };
        let Some(window_pid) = process_window_pid(process.pid, &parents, &windows) else {
            continue;
        };
        let provider_title =
            explicit_session_id(&process.program, &process.argv).and_then(|session_id| {
                programs::sessions::title_for(&process.program.id, &process.cwd, &session_id)
            });
        let terminal_title = process_context_title(process.pid, &parents, &status_by_pid, &windows);
        let title = provider_title
            .and_then(|title| session_title(Some(&title), Some(&process.program)))
            .or_else(|| session_title(terminal_title.as_deref(), Some(&process.program)))
            .unwrap_or_else(|| format!("{} session", process.program.name));

        sessions.push(AgentSession {
            project_id: *project_id,
            id: format!("external-{}-{}", process.pid, process.start_ticks),
            title,
            program_id: process.program.id.clone(),
            tab_key: None,
            external: Some(ExternalTarget {
                pid: process.pid,
                start_ticks: process.start_ticks,
                window_pid,
            }),
            catalog_id: None,
            radar_session_id: None,
            provider_session_id: explicit_session_id(&process.program, &process.argv),
            claim_id: programs::launch::radar_agent_of(process.pid),
            pid: Some(process.pid),
            card_id: programs::launch::radar_card_of(process.pid),
            last_activity_at: now,
            running: true,
            archived: false,
        });
    }
    sessions
}

#[cfg(target_os = "linux")]
fn identify_agent(argv: &[String], programs: &[Program]) -> Option<Program> {
    programs.iter().find_map(|program| {
        let command = Path::new(&program.command).file_name()?.to_string_lossy();
        argv.iter()
            .take(4)
            .any(|argument| {
                Path::new(argument)
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy() == command)
            })
            .then(|| program.clone())
    })
}

#[cfg(target_os = "linux")]
fn explicit_session_id(program: &Program, argv: &[String]) -> Option<String> {
    let template: Vec<&str> = program.resume_session.split_whitespace().collect();
    let id_index = template.iter().position(|part| part.contains("{id}"))?;
    let id_template = template[id_index];
    if id_template == "{id}" {
        let last = *template[..id_index].last()?;
        return argv.iter().enumerate().find_map(|(index, argument)| {
            if argument == last {
                return argv
                    .get(index + 1)
                    .filter(|value| !value.is_empty())
                    .cloned();
            }
            argument
                .split_once('=')
                .filter(|(name, value)| *name == last && !value.is_empty())
                .map(|(_, value)| value.to_string())
        });
    }
    let (prefix, suffix) = id_template.split_once("{id}")?;
    argv.iter()
        .filter_map(|argument| argument.strip_prefix(prefix))
        .filter_map(|value| value.strip_suffix(suffix))
        .find(|value| !value.is_empty())
        .map(str::to_string)
}

fn process_context_title(
    pid: u32,
    parents: &HashMap<u32, u32>,
    statuses: &HashMap<u32, &Status>,
    windows: &HashMap<u32, CompositorWindow>,
) -> Option<String> {
    let mut current = pid;
    for _ in 0..64 {
        if let Some(status) = statuses.get(&current) {
            if let Some(title) = session_title(status.title.as_deref(), None) {
                return Some(title);
            }
        }
        if let Some(title) = windows.get(&current).map(|window| window.title.as_str()) {
            if !catalog::is_generic_session_title(title, None) {
                return Some(title.trim().to_string());
            }
        }
        let parent = *parents.get(&current)?;
        if parent == 0 || parent == current {
            return None;
        }
        current = parent;
    }
    None
}

#[cfg(target_os = "linux")]
fn process_window_pid(
    pid: u32,
    parents: &HashMap<u32, u32>,
    windows: &HashMap<u32, CompositorWindow>,
) -> Option<u32> {
    let mut current = pid;
    for _ in 0..64 {
        if windows.contains_key(&current) {
            return Some(current);
        }
        let parent = *parents.get(&current)?;
        if parent == 0 || parent == current {
            return None;
        }
        current = parent;
    }
    None
}
#[cfg(target_os = "linux")]
fn has_interactive_terminal(directory: &Path) -> bool {
    std::fs::read_link(directory.join("fd/0"))
        .ok()
        .is_some_and(|device| is_terminal_device(&device))
}

#[cfg(target_os = "linux")]
fn is_terminal_device(device: &Path) -> bool {
    device == Path::new("/dev/tty") || device.starts_with("/dev/pts")
}

fn compositor_windows() -> HashMap<u32, CompositorWindow> {
    let output = ProcessCommand::new("hyprctl")
        .args(["-j", "clients"])
        .output();
    let Ok(output) = output else {
        return HashMap::new();
    };
    if !output.status.success() {
        return HashMap::new();
    }
    serde_json::from_slice::<Vec<CompositorWindow>>(&output.stdout)
        .unwrap_or_default()
        .into_iter()
        .filter(|window| !window.title.trim().is_empty() && !window.address.is_empty())
        .map(|window| (window.pid, window))
        .collect()
}

#[cfg(target_os = "linux")]
pub(super) fn focus_external(pid: u32, start_ticks: u64, window_pid: u32) -> Result<(), String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|_| "The agent session is no longer running".to_string())?;
    if parse_process_stat(&stat).map(|(_, start)| start) != Some(start_ticks) {
        return Err("The agent session is no longer running".to_string());
    }
    let windows = compositor_windows();
    let address = windows
        .get(&window_pid)
        .map(|window| window.address.as_str())
        .filter(|address| !address.is_empty())
        .ok_or_else(|| "The terminal window is no longer available".to_string())?;
    let target = format!("address:{address}");
    let output = ProcessCommand::new("hyprctl")
        .args(["dispatch", "focuswindow", &target])
        .output()
        .map_err(|error| format!("Could not focus the terminal window: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err("Could not focus the terminal window".to_string())
    }
}

#[cfg(target_os = "linux")]
fn read_session_id(path: &Path) -> Option<String> {
    let environ = std::fs::read(path).ok()?;
    environ
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(b"RADAR_SESSION_ID="))
        .and_then(|value| String::from_utf8(value.to_vec()).ok())
}

#[cfg(target_os = "linux")]
fn managed_agent_ancestor(
    pid: u32,
    parents: &HashMap<u32, u32>,
    statuses: &HashMap<u32, &Status>,
    managed_ids: &HashSet<String>,
) -> bool {
    let mut current = pid;
    for _ in 0..64 {
        if let Some(status) = statuses.get(&current) {
            if managed_ids.contains(&status.id) {
                return true;
            }
        }
        let Some(parent) = parents.get(&current).copied() else {
            return false;
        };
        if parent == 0 || parent == current {
            return false;
        }
        current = parent;
    }
    false
}

#[cfg(target_os = "linux")]
fn parse_process_stat(stat: &str) -> Option<(u32, u64)> {
    let fields = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let parent = fields.get(1)?.parse().ok()?;
    let start_ticks = fields.get(19)?.parse().ok()?;
    Some((parent, start_ticks))
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::{
        explicit_session_id, is_terminal_device, parse_process_stat, process_window_pid,
        session_title, CompositorWindow,
    };
    struct DiscoveryDaemon {
        home: tempfile::TempDir,
        server: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
    }

    impl DiscoveryDaemon {
        fn start() -> Self {
            use std::time::{Duration, Instant};

            let home = tempfile::tempdir().unwrap();
            let server = super::daemon::Server::bind(home.path()).unwrap();
            let server = std::thread::spawn(move || server.run());
            let daemon = Self {
                home,
                server: Some(server),
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            while !matches!(
                super::daemon::Client::request(daemon.home.path(), super::daemon::Command::Ping),
                Ok(super::daemon::Response::Hello {
                    version: super::daemon::VERSION
                })
            ) {
                assert!(Instant::now() < deadline, "session daemon did not start");
                std::thread::sleep(Duration::from_millis(10));
            }
            daemon
        }
    }

    impl Drop for DiscoveryDaemon {
        fn drop(&mut self) {
            let _ =
                super::daemon::Client::request(self.home.path(), super::daemon::Command::Shutdown);
            if let Some(server) = self.server.take() {
                let _ = server.join();
            }
        }
    }

    #[test]
    fn discovery_uses_catalog_title_when_live_terminal_title_is_generic() {
        use std::time::{Duration, Instant};

        let daemon = DiscoveryDaemon::start();
        let project_path = daemon.home.path().join("project");
        std::fs::create_dir(&project_path).unwrap();
        let project = crate::db::Project {
            id: 3,
            path: project_path.clone(),
            name: "project".to_string(),
            pinned: false,
            sort_order: 0,
            added_at: 0,
            last_opened_at: None,
            open_count: 0,
            archived: false,
        };
        let radar_id = "project-3-agent-0-opencode";
        let response = super::daemon::Client::request(
            daemon.home.path(),
            super::daemon::Command::Create(crate::session::registry::Spawn {
                id: radar_id.to_string(),
                cwd: project_path.clone(),
                argv: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "printf '\\033]0;OpenCode\\007'; sleep 30".to_string(),
                ],
                dims: crate::session::Dims { cols: 80, rows: 24 },
                env: Vec::new(),
                env_remove: Vec::new(),
            }),
        )
        .unwrap();
        assert!(matches!(response, super::daemon::Response::Status(_)));

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let has_generic_title = matches!(
                super::daemon::Client::request(daemon.home.path(), super::daemon::Command::List),
                Ok(super::daemon::Response::Sessions(statuses))
                    if statuses.iter().any(|status| {
                        status.id == radar_id && status.title.as_deref() == Some("OpenCode")
                    })
            );
            if has_generic_title {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "session did not publish its terminal title"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let _initial = super::discover(std::slice::from_ref(&project), daemon.home.path()).unwrap();
        let imported_at = super::catalog::now_millis();
        let catalog =
            super::catalog::SessionCatalog::open(&daemon.home.path().join("run/catalog.sqlite"))
                .unwrap();
        assert_eq!(
            catalog
                .import_provider(
                    project.id,
                    "opencode",
                    &project_path,
                    &[super::catalog::Imported {
                        id: "ses_42".to_string(),
                        title: Some("Earlier work".to_string()),
                        created_ms: imported_at,
                        last_activity_ms: imported_at,
                    }],
                    imported_at,
                )
                .unwrap(),
            1
        );
        drop(catalog);

        let snapshot = super::discover(std::slice::from_ref(&project), daemon.home.path()).unwrap();
        let live = snapshot
            .sessions
            .iter()
            .find(|session| session.id == radar_id)
            .expect("live session should appear in discovery");
        assert_eq!(live.title, "Earlier work");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_stat_parses_parent_and_start_time_with_parentheses_in_comm() {
        let mut fields = vec!["S".to_string(), "41".to_string()];
        fields.resize(20, "0".to_string());
        fields[19] = "98765".to_string();
        let stat = format!("27 (agent (worker)) {}", fields.join(" "));
        assert_eq!(parse_process_stat(&stat), Some((41, 98765)));
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn external_agents_must_belong_to_a_compositor_window() {
        let parents = std::collections::HashMap::from([(101, 200), (200, 300)]);
        let windows = std::collections::HashMap::from([(
            300,
            CompositorWindow {
                pid: 300,
                title: "Project terminal".to_string(),
                address: "0x123".to_string(),
            },
        )]);
        assert_eq!(process_window_pid(101, &parents, &windows), Some(300));
        assert_eq!(
            process_window_pid(101, &parents, &std::collections::HashMap::new()),
            None
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn background_processes_without_a_terminal_stdin_are_excluded() {
        assert!(is_terminal_device(std::path::Path::new("/dev/pts/55")));
        assert!(is_terminal_device(std::path::Path::new("/dev/tty")));
        assert!(!is_terminal_device(std::path::Path::new("/dev/null")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_generic_terminal_or_agent_name_is_not_a_session_title() {
        let program = crate::programs::by_id("opencode").unwrap();
        assert_eq!(session_title(Some(" OpenCode "), Some(&program)), None);
        assert_eq!(session_title(Some(" foot "), Some(&program)), None);
        assert_eq!(
            session_title(Some("Investigate the flaky test"), Some(&program)),
            Some("Investigate the flaky test".to_string())
        );
        assert_eq!(session_title(Some("Ghostty"), None), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn explicit_session_ids_follow_flag_and_positional_resume_templates() {
        let codex = crate::programs::by_id("codex").unwrap();
        let argv = ["codex", "resume", "thread-42"]
            .map(str::to_string)
            .to_vec();
        assert_eq!(
            explicit_session_id(&codex, &argv),
            Some("thread-42".to_string())
        );

        let opencode = crate::programs::by_id("opencode").unwrap();
        let argv = ["opencode", "--session=ses_42"]
            .map(str::to_string)
            .to_vec();
        assert_eq!(
            explicit_session_id(&opencode, &argv),
            Some("ses_42".to_string())
        );

        let omp = crate::programs::by_id("omp").unwrap();
        let argv = ["omp", "--resume=01omp42"].map(str::to_string).to_vec();
        assert_eq!(
            explicit_session_id(&omp, &argv),
            Some("01omp42".to_string())
        );

        // Pi's exact-session flag is a separate value, like Codex's.
        let pi = crate::programs::by_id("pi").unwrap();
        let argv = ["pi", "--session-id", "ses_radar2abc"]
            .map(str::to_string)
            .to_vec();
        assert_eq!(
            explicit_session_id(&pi, &argv),
            Some("ses_radar2abc".to_string())
        );

        let cursor = crate::programs::by_id("cursor-agent").unwrap();
        let argv = ["cursor-agent", "--resume", "chat-42"]
            .map(str::to_string)
            .to_vec();
        assert_eq!(
            explicit_session_id(&cursor, &argv),
            Some("chat-42".to_string())
        );
        let argv = ["cursor-agent", "--resume", ""]
            .map(str::to_string)
            .to_vec();
        assert_eq!(explicit_session_id(&cursor, &argv), None);
    }
    #[test]
    fn initial_session_scan_failure_does_not_claim_to_show_previous_results() {
        let mut index = super::SessionIndex::default();

        assert!(!index.apply(Err("daemon unavailable".to_string())));
        assert_eq!(
            index.notice(),
            Some("Session discovery failed; no session snapshot is available.")
        );
    }
    #[test]
    fn catalog_history_limit_is_triggered_at_the_slice_boundary() {
        assert!(!super::catalog_history_is_limited(499, 199));
        assert!(super::catalog_history_is_limited(500, 0));
        assert!(super::catalog_history_is_limited(0, 200));
        assert!(super::catalog_history_is_limited(501, 201));
    }
    #[test]
    fn failed_session_discovery_keeps_the_last_snapshot_and_cap_notice() {
        let session = super::AgentSession {
            project_id: 7,
            id: "catalog-7".to_string(),
            title: "Saved conversation".to_string(),
            program_id: "codex".to_string(),
            tab_key: None,
            external: None,
            catalog_id: Some(7),
            radar_session_id: None,
            provider_session_id: Some("thread-7".to_string()),
            claim_id: None,
            pid: None,
            card_id: None,
            last_activity_at: 0,
            running: false,
            archived: false,
        };
        let mut index = super::SessionIndex::default();

        assert!(index.apply(Ok(super::DiscoverySnapshot {
            sessions: vec![session],
            history_limited: true,
        })));
        assert_eq!(index.by_project[&7][0].id, "catalog-7");
        assert_eq!(
            index.notice(),
            Some("Session history is limited; older entries may be omitted.")
        );

        assert!(!index.apply(Err("daemon unavailable".to_string())));
        assert_eq!(index.by_project[&7][0].id, "catalog-7");
        assert_eq!(
            index.notice(),
            Some("Session discovery failed; showing the last successful results.")
        );
        assert_eq!(index.error.as_deref(), Some("daemon unavailable"));
        assert!(index.history_limited);

        assert!(index.apply(Ok(super::DiscoverySnapshot {
            sessions: Vec::new(),
            history_limited: false,
        })));
        assert!(index.by_project.is_empty());
        assert_eq!(index.notice(), None);
        assert!(index.error.is_none());
    }

    #[test]
    fn a_live_row_titles_from_the_program_then_the_catalog_then_new() {
        let opencode = crate::programs::by_id("opencode").unwrap();
        // The program's own title wins when it says something real.
        assert_eq!(
            super::live_row_title(
                Some("Fix the flaky test"),
                Some("Old talk"),
                Some(&opencode)
            ),
            "Fix the flaky test"
        );
        // A generic terminal title (just the program's name) is not real:
        // the conversation the catalog already knows titles the row.
        assert_eq!(
            super::live_row_title(
                Some("OpenCode"),
                Some("Fix the flaky test"),
                Some(&opencode)
            ),
            "Fix the flaky test"
        );
        // No live title at all (fresh after a restart): the catalog speaks.
        assert_eq!(
            super::live_row_title(None, Some("Planning next steps"), Some(&opencode)),
            "Planning next steps"
        );
        // A generic catalog title is not a conversation either.
        assert_eq!(
            super::live_row_title(None, Some("OpenCode"), Some(&opencode)),
            "New OpenCode session"
        );
        // No conversation anywhere: honestly new, named for its program.
        assert_eq!(
            super::live_row_title(None, None, Some(&opencode)),
            "New OpenCode session"
        );
        // An unknown program still gets a readable placeholder.
        assert_eq!(super::live_row_title(None, None, None), "New session");
    }

    #[test]
    fn exact_resume_needs_a_provider_reported_conversation_id() {
        let session = |radar: Option<&str>, provider: Option<&str>| super::AgentSession {
            project_id: 3,
            id: "catalog-1".to_string(),
            title: String::new(),
            program_id: "omp".to_string(),
            tab_key: None,
            external: None,
            catalog_id: None,
            radar_session_id: radar.map(str::to_string),
            claim_id: None,
            pid: None,
            card_id: None,
            provider_session_id: provider.map(str::to_string),
            last_activity_at: 0,
            running: false,
            archived: false,
        };

        // Radar-created rows backfill radar's own stable id: not a
        // conversation the provider ever reported, so no exact reopen.
        assert_eq!(
            super::exact_provider_session_id(&session(
                Some("project-3-agent-2-omp"),
                Some("project-3-agent-2-omp")
            )),
            None,
            "the backfilled radar id must not reach the provider's resume flag"
        );
        // No radar session and no provider id (external/import edges): none.
        assert_eq!(super::exact_provider_session_id(&session(None, None)), None);
        assert_eq!(
            super::exact_provider_session_id(&session(Some("project-3-agent-0-opencode"), None)),
            None
        );
        // A conversation id the provider itself reported (provider-history
        // import, or a radar row the provider later filled): exact.
        assert_eq!(
            super::exact_provider_session_id(&session(None, Some("ses_f22a1495d"))),
            Some("ses_f22a1495d")
        );
        assert_eq!(
            super::exact_provider_session_id(&session(
                Some("project-3-agent-0-opencode"),
                Some("ses_f22a1495d")
            )),
            Some("ses_f22a1495d")
        );
    }
}
