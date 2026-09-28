use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use serde::Deserialize;

use crate::db::Project;
use crate::programs::{self, Program};
use crate::session::catalog::{self, CatalogFilter};
use crate::session::daemon::{self, Client, Command, Response};
use crate::session::registry::{Lifecycle, Status};

use super::{parse_stable_session_id, Slot};

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
    pub last_activity_at: i64,
    pub running: bool,
    pub archived: bool,
}

/// How the sidebar orders and presents entries: the newest activity first,
/// then title, then stable id — deterministic when timestamps tie.
pub(super) fn sort_sidebar_sessions(sessions: &mut [(AgentSession, String)]) {
    sessions.sort_by_cached_key(|(session, title)| {
        (
            std::cmp::Reverse(session.last_activity_at),
            title.to_lowercase(),
            session.program_id.to_lowercase(),
            session.id.clone(),
        )
    });
}

fn fallback_title(program_id: &str) -> String {
    programs::by_id(program_id)
        .map(|program| program.name)
        .unwrap_or_else(|| format!("{program_id} session"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ExternalTarget {
    pub pid: u32,
    pub start_ticks: u64,
    pub window_pid: u32,
}

/// Everything a project's sidebar should list: live Radar-managed agents,
/// the durable catalog history, and (Linux) agents running in external
/// terminals. Catalog rows carry the archive flag; rendering decides which
/// slice is visible.
pub(super) fn discover(projects: &[Project], session_home: &Path) -> Vec<AgentSession> {
    let now = crate::session::catalog::now_millis();
    let statuses = match Client::request(session_home, Command::List) {
        Ok(Response::Sessions(sessions)) => sessions,
        _ => Vec::new(),
    };
    let project_ids: HashSet<i64> = projects.iter().map(|project| project.id).collect();
    let roster: Vec<daemon::CatalogProject> = projects
        .iter()
        .map(|project| daemon::CatalogProject {
            id: project.id,
            path: project.path.clone(),
        })
        .collect();
    let entries = |filter, limit| match Client::request(
        session_home,
        Command::CatalogList {
            projects: roster.clone(),
            filter,
            query: None,
            limit,
        },
    ) {
        Ok(Response::Catalog(entries)) => entries,
        _ => Vec::new(),
    };
    // History is bounded per slice so one list can never bury the other.
    let mut catalog_entries = entries(CatalogFilter::Active, 500);
    catalog_entries.extend(entries(CatalogFilter::Archived, 200));

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
        let title = session_title(status.title.as_deref(), program.as_ref())
            .unwrap_or_else(|| "Starting session…".to_string());
        // A live session the catalog has no row for (pre-catalog session, or
        // the first scan after an upgrade): backfill once and remember it.
        let entry = live_catalog.remove(&status.id);
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
            radar_session_id: None,
            provider_session_id: Some(entry.provider_session_id.clone()),
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

    sessions
}

fn session_title(title: Option<&str>, program: Option<&Program>) -> Option<String> {
    let title = title?.trim();
    if generic_window_title(title)
        || program.is_some_and(|program| {
            title.eq_ignore_ascii_case(&program.name)
                || title.eq_ignore_ascii_case(&program.command)
        })
    {
        None
    } else {
        Some(title.to_string())
    }
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
            if !generic_window_title(title) {
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

fn generic_window_title(title: &str) -> bool {
    let title = title.trim();
    title.is_empty()
        || [
            "terminal",
            "foot",
            "ghostty",
            "kitty",
            "alacritty",
            "wezterm",
            "xterm",
            "radar",
        ]
        .iter()
        .any(|generic| title.eq_ignore_ascii_case(generic))
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
        explicit_session_id, generic_window_title, is_terminal_device, parse_process_stat,
        process_window_pid, session_title, CompositorWindow,
    };

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
        assert!(generic_window_title("Ghostty"));
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
    fn sidebar_sessions_sort_by_newest_activity_then_title() {
        let session = |id: &str, program_id: &str, last_activity_at: i64| super::AgentSession {
            project_id: 1,
            id: id.to_string(),
            title: String::new(),
            program_id: program_id.to_string(),
            tab_key: None,
            external: None,
            catalog_id: None,
            radar_session_id: None,
            provider_session_id: None,
            last_activity_at,
            running: false,
            archived: false,
        };
        let mut sessions = vec![
            (session("old", "codex", 100), "zeta".to_string()),
            (session("new", "claude", 900), "alpha".to_string()),
            (session("tie-a", "codex", 500), "ALPHA".to_string()),
            (session("tie-b", "codex", 500), "alpha".to_string()),
        ];

        super::sort_sidebar_sessions(&mut sessions);

        assert_eq!(
            sessions
                .iter()
                .map(|(session, _)| session.id.as_str())
                .collect::<Vec<_>>(),
            ["new", "tie-a", "tie-b", "old"],
            "newest activity first; title breaks timestamp ties case-insensitively"
        );
    }
}
