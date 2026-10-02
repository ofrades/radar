//! The conversations agents have, as their CLIs record them.
//!
//! Stores validate exact resume targets and supply history, never worker identity.
//! Live identity is reported by the provider's own lifecycle integration.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Which CLI has a session store radar can read? Capture is per program;
/// the resume templates in the registry cover the rest.
fn can_capture(program_id: &str) -> bool {
    program_id == "opencode"
}

/// The CLI defaults to 100 rows; use a high bounded page so older history is
/// imported instead of silently disappearing from the sidebar.
const SESSION_LIST_ARGS: [&str; 7] = [
    "session",
    "list",
    "--format",
    "json",
    "--max-count",
    "10000",
    "--standalone",
];

#[derive(Deserialize)]
struct ListedSession {
    id: String,
    created: i64,
    #[serde(default)]
    updated: Option<i64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    directory: Option<String>,
}

/// Run the CLI's session list and parse it. `None` when the CLI has no
/// readable store, the run failed, or the output did not parse.
fn run_session_list(program_id: &str, cwd: &Path) -> Option<Vec<ListedSession>> {
    if matches!(program_id, "pi" | "omp") {
        return read_jsonl_sessions(&session_root(program_id, cwd, &[], &[]).ok()?, cwd).ok();
    }
    if program_id == "cursor-agent" {
        return read_cursor_sessions(&cursor_root(&[]).ok()?, cwd).ok();
    }
    if !can_capture(program_id) {
        return None;
    }
    read_opencode_sessions(cwd, &[]).ok()
}

fn read_opencode_sessions(cwd: &Path, env: &[(String, String)]) -> Result<Vec<ListedSession>> {
    let output = std::process::Command::new("opencode")
        .args(SESSION_LIST_ARGS)
        .current_dir(cwd)
        // The daemon may lack the login shell's PATH; resolve the provider
        // CLI through the same entries a spawn uses.
        .envs(env.iter().map(|(key, value)| (key, value)))
        .env("PATH", crate::config::path_value())
        .output()
        .context("Reading OpenCode session history")?;
    if !output.status.success() {
        bail!("OpenCode could not read session history");
    }
    parse_sessions(std::str::from_utf8(&output.stdout)?)
}

/// The title the provider assigned to one exact conversation.
#[cfg(feature = "gui")]
pub(crate) fn title_for(program_id: &str, cwd: &Path, id: &str) -> Option<String> {
    let sessions = run_session_list(program_id, cwd)?;
    listed_title(&sessions, cwd, id)
}

/// One conversation as the provider's own store reports it. `created` keeps
/// the provider's raw stamp; the catalog normalizes units.
pub(crate) struct ProviderSession {
    pub id: String,
    pub title: Option<String>,
    pub created: i64,
    pub updated: Option<i64>,
}

/// Every conversation the provider's store lists for `cwd`. `None` when the
/// CLI has no readable store, the run failed, or the output did not parse.
pub(crate) fn list_provider_sessions(program_id: &str, cwd: &Path) -> Option<Vec<ProviderSession>> {
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    Some(
        run_session_list(program_id, cwd.as_path())?
            .into_iter()
            .filter(|session| {
                session.directory.as_ref().is_none_or(|directory| {
                    let directory = std::fs::canonicalize(directory)
                        .unwrap_or_else(|_| PathBuf::from(directory));
                    directory == cwd
                })
            })
            .map(|session| ProviderSession {
                id: session.id,
                title: session.title,
                created: session.created,
                updated: session.updated,
            })
            .collect(),
    )
}

/// Whether the provider's own store still lists this conversation — scoped
/// to `cwd`, the same project scoping every reader here applies. `None`
/// when radar cannot tell: the CLI has no readable store, the run failed,
/// or the output did not parse.
#[cfg(feature = "gui")]
pub(crate) fn has_provider_session(program_id: &str, cwd: &Path, id: &str) -> Option<bool> {
    let sessions = run_session_list(program_id, cwd)?;
    Some(listed_session_exists(&sessions, cwd, id))
}

/// The store's entry for `id` scoped to `cwd`, canonicalized the way the
/// CLI records directories.
fn scoped_find<'a>(
    sessions: &'a [ListedSession],
    cwd: &Path,
    id: &str,
) -> Option<&'a ListedSession> {
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    sessions.iter().find(|session| {
        session.id == id
            && session.directory.as_ref().is_none_or(|directory| {
                let directory =
                    std::fs::canonicalize(directory).unwrap_or_else(|_| directory.into());
                directory == cwd
            })
    })
}

#[cfg(any(test, feature = "gui"))]
fn listed_title(sessions: &[ListedSession], cwd: &Path, id: &str) -> Option<String> {
    scoped_find(sessions, cwd, id)
        .and_then(|session| session.title.as_deref())
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
}

/// Whether the store's list has `id` at all, scoped to `cwd`.
fn listed_session_exists(sessions: &[ListedSession], cwd: &Path, id: &str) -> bool {
    scoped_find(sessions, cwd, id).is_some()
}
/// Parse a session list the way the CLI prints it (JSON, newest first).
fn parse_sessions(json: &str) -> Result<Vec<ListedSession>> {
    Ok(serde_json::from_str(json)?)
}

/// Resume is never creation. An unreadable store is an error, not permission
/// to let a create-if-missing flag silently open an empty conversation.
pub fn validate_resume(
    program: &str,
    cwd: &Path,
    id: &str,
    env: &[(String, String)],
    args: &[String],
) -> Result<()> {
    let exists = match program {
        "pi" | "omp" => listed_session_exists(
            &read_jsonl_sessions(&session_root(program, cwd, env, args)?, cwd)?,
            cwd,
            id,
        ),
        "cursor-agent" => {
            listed_session_exists(&read_cursor_sessions(&cursor_root(env)?, cwd)?, cwd, id)
        }
        "opencode" => {
            if !listed_session_exists(&read_opencode_sessions(cwd, env)?, cwd, id) {
                false
            } else {
                let output = std::process::Command::new("opencode")
                    .args(["session", "export", "--standalone", id])
                    .current_dir(cwd)
                    .envs(env.iter().map(|(key, value)| (key, value)))
                    .env("PATH", crate::config::path_value())
                    .output()?;
                if !output.status.success() {
                    bail!("OpenCode could not read the exact saved transcript");
                }
                let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
                value["messages"]
                    .as_array()
                    .context("OpenCode export has no messages")?
                    .iter()
                    .any(|message| message["type"] == "user")
            }
        }
        _ => bail!("Exact resume validation is not supported for {program}"),
    };
    if !exists {
        bail!("The saved {program} conversation {id} is missing or has no conversation history. Resume was cancelled; no new conversation was created.");
    }
    Ok(())
}

fn env_value(env: &[(String, String)], key: &str) -> Option<String> {
    env.iter()
        .rev()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.clone())
        .or_else(|| std::env::var(key).ok())
}

fn session_root(
    program: &str,
    cwd: &Path,
    env: &[(String, String)],
    args: &[String],
) -> Result<PathBuf> {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--session-dir" {
            return args
                .get(i + 1)
                .map(|path| cwd.join(path))
                .context("--session-dir needs a path");
        }
        if let Some(path) = arg.strip_prefix("--session-dir=") {
            return Ok(cwd.join(path));
        }
    }
    if let Some(path) = env_value(env, "PI_CODING_AGENT_SESSION_DIR") {
        return Ok(cwd.join(path));
    }
    let home = env_value(env, "HOME").context("No home directory for provider history")?;
    let agent = env_value(env, "PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home).join(format!(".{program}/agent")));
    if program == "pi" {
        let mut configured = None;
        for settings in [agent.join("settings.json"), cwd.join(".pi/settings.json")] {
            if settings.exists() {
                let value: serde_json::Value =
                    serde_json::from_str(&std::fs::read_to_string(settings)?)?;
                if let Some(path) = value["sessionDir"].as_str() {
                    configured = Some(cwd.join(path));
                }
            }
        }
        if let Some(path) = configured {
            return Ok(path);
        }
    }
    Ok(agent.join("sessions"))
}

fn read_jsonl_sessions(root: &Path, cwd: &Path) -> Result<Vec<ListedSession>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let cwd = cwd.canonicalize().context("Provider project directory")?;
    let mut sessions = Vec::new();
    for entry in walkdir::WalkDir::new(root).max_depth(2) {
        let entry = entry?;
        if !entry.file_type().is_file() || entry.path().extension().is_none_or(|ext| ext != "jsonl")
        {
            continue;
        }
        let file = std::fs::File::open(entry.path())?;
        let mut lines = BufReader::new(file).lines();
        let mut header = None;
        let mut title = None;
        for line in lines.by_ref().take(2) {
            let value: serde_json::Value = serde_json::from_str(&line?)?;
            if value["type"] == "session" {
                header = Some(value);
                break;
            }
            if value["type"] == "title" {
                title = value["title"].as_str().map(str::to_string);
            }
        }
        let Some(header) = header else {
            continue;
        };
        let Some(directory) = header["cwd"].as_str() else {
            continue;
        };
        if Path::new(directory).canonicalize().ok().as_ref() != Some(&cwd) {
            continue;
        }
        let mut history = false;
        for line in lines {
            let line = line?;
            let value: serde_json::Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                // A writer may be appending its last entry. Do not infer identity from it.
                Err(_) => continue,
            };
            match value["type"].as_str() {
                Some("title") => title = value["title"].as_str().map(str::to_string),
                Some("session_info") => title = value["name"].as_str().map(str::to_string),
                Some("message") => {
                    history |= matches!(
                        value["message"]["role"].as_str(),
                        Some("user" | "assistant")
                    )
                }
                _ => {}
            }
        }
        if !history {
            continue;
        }
        let id = header["id"]
            .as_str()
            .context("Session header has no ID")?
            .to_string();
        let created = chrono::DateTime::parse_from_rfc3339(
            header["timestamp"]
                .as_str()
                .context("Session header has no timestamp")?,
        )?
        .timestamp_millis();
        let updated = i64::try_from(
            entry
                .metadata()?
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis(),
        )?;
        sessions.push(ListedSession {
            id,
            created,
            updated: Some(updated),
            title,
            directory: Some(directory.to_string()),
        });
    }
    Ok(sessions)
}

fn cursor_root(env: &[(String, String)]) -> Result<PathBuf> {
    if let Some(path) = env_value(env, "CURSOR_CONFIG_DIR").filter(|path| !path.trim().is_empty()) {
        return Ok(PathBuf::from(path).join("chats"));
    }
    if let Some(path) = env_value(env, "XDG_CONFIG_HOME").filter(|path| !path.trim().is_empty()) {
        return Ok(PathBuf::from(path).join("cursor/chats"));
    }
    let home = env_value(env, "HOME").context("No home directory for Cursor history")?;
    Ok(PathBuf::from(home).join(".cursor/chats"))
}

fn read_cursor_sessions(root: &Path, cwd: &Path) -> Result<Vec<ListedSession>> {
    let cwd = cwd.canonicalize()?;
    let project = root.join(format!(
        "{:x}",
        md5::compute(cwd.to_string_lossy().as_bytes())
    ));
    if !project.exists() {
        return Ok(Vec::new());
    }
    let mut sessions = Vec::new();
    for entry in std::fs::read_dir(project)? {
        let path = entry?.path().join("store.db");
        if !path.is_file() {
            continue;
        }
        let db = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let hex: String = db.query_row("SELECT value FROM meta WHERE key = '0'", [], |row| {
            row.get(0)
        })?;
        if !hex.is_ascii() || hex.len() % 2 != 0 {
            bail!("Invalid Cursor session metadata");
        }
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let Some(blob) = value["latestRootBlobId"].as_str() else {
            continue;
        };
        let has_history: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM blobs WHERE id = ?1 AND length(data) > 0)",
            [blob],
            |row| row.get(0),
        )?;
        if !has_history {
            continue;
        }
        sessions.push(ListedSession {
            id: value["agentId"]
                .as_str()
                .context("Cursor metadata has no ID")?
                .to_string(),
            created: value["createdAt"]
                .as_i64()
                .context("Cursor metadata has no creation stamp")?,
            updated: Some(i64::try_from(
                path.metadata()?
                    .modified()?
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_millis(),
            )?),
            title: value["name"].as_str().map(str::to_string),
            directory: Some(cwd.to_string_lossy().into_owned()),
        });
    }
    Ok(sessions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_history_is_not_a_worker_identity_signal() {
        let json = r#"[
            {"id": "ses_old", "created": 1000},
            {"id": "ses_mid", "created": 2000},
            {"id": "ses_new", "created": 3000}
        ]"#;
        assert_eq!(parse_sessions(json).unwrap().len(), 3);
    }

    #[test]
    fn exact_session_title_is_scoped_to_its_project_directory() {
        let sessions = parse_sessions(
            r#"[
                {"id":"ses_a","created":1,"title":"First task","directory":"/projects/radar"},
                {"id":"ses_b","created":2,"title":"Second task","directory":"/projects/radar"},
                {"id":"ses_c","created":3,"title":"Other project","directory":"/projects/other"}
            ]"#,
        )
        .unwrap();
        assert_eq!(
            listed_title(&sessions, Path::new("/projects/radar"), "ses_b"),
            Some("Second task".to_string())
        );
        assert_eq!(
            listed_title(&sessions, Path::new("/projects/other"), "ses_b"),
            None
        );
    }

    #[test]
    fn a_bound_conversation_exists_only_when_its_own_project_lists_it() {
        // `has_provider_session` reads the store the way `listed_title`
        // does: a binding to a conversation the store never wrote (a
        // crashed first turn) must read as lost, and one recorded under
        // another project stays invisible here.
        let sessions = parse_sessions(
            r#"[
                {"id":"ses_a","created":1,"directory":"/projects/radar"},
                {"id":"ses_c","created":3,"directory":"/projects/other"}
            ]"#,
        )
        .unwrap();
        assert!(listed_session_exists(
            &sessions,
            Path::new("/projects/radar"),
            "ses_a"
        ));
        assert!(!listed_session_exists(
            &sessions,
            Path::new("/projects/radar"),
            "ses_b"
        ));
        assert!(!listed_session_exists(
            &sessions,
            Path::new("/projects/radar"),
            "ses_c"
        ));
        assert!(listed_session_exists(
            &sessions,
            Path::new("/projects/other"),
            "ses_c"
        ));
    }

    #[test]
    fn provider_updated_stamp_is_optional_and_preserved() {
        let sessions =
            parse_sessions(r#"[{"id":"ses_old","created":1700000000000,"updated":1700007000000}]"#)
                .unwrap();
        assert_eq!(sessions[0].created, 1_700_000_000_000);
        assert_eq!(sessions[0].updated, Some(1_700_007_000_000));
        assert_eq!(
            parse_sessions(r#"[{"id":"ses_legacy","created":1700000000}]"#).unwrap()[0].updated,
            None
        );
    }

    #[test]
    fn provider_import_requests_history_beyond_the_cli_default() {
        assert_eq!(
            SESSION_LIST_ARGS,
            [
                "session",
                "list",
                "--format",
                "json",
                "--max-count",
                "10000",
                "--standalone"
            ]
        );
        let json = format!(
            "[{}]",
            (0..101)
                .map(|index| format!(r#"{{"id":"ses_{index}","created":{index}}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let sessions = parse_sessions(&json).unwrap();
        assert_eq!(sessions.len(), 101);
        assert_eq!(sessions[100].id, "ses_100");
    }

    #[test]
    fn pi_and_omp_resume_require_the_exact_saved_project_history() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        for provider in ["pi", "omp"] {
            let root = home
                .path()
                .join(format!(".{provider}/agent/sessions/project"));
            std::fs::create_dir_all(&root).unwrap();
            for (id, directory, history) in [
                ("original", cwd.path(), true),
                ("other", other.path(), true),
                ("empty", cwd.path(), false),
            ] {
                let mut entries = vec![
                    serde_json::json!({"type":"session", "version":3, "id":id,"cwd":directory,"timestamp":"2026-10-02T09:00:00Z"}),
                ];
                if provider == "omp" {
                    entries.insert(
                        0,
                        serde_json::json!({"type":"title","title":"Task history"}),
                    );
                }
                if history {
                    entries.push(serde_json::json!({"type":"message","message":{"role":"user","content":"original work"}}));
                }
                let text = entries
                    .iter()
                    .map(|entry| format!("{entry}\n"))
                    .collect::<String>();
                std::fs::write(root.join(format!("time_{id}.jsonl")), text).unwrap();
            }
            let env = vec![("HOME".into(), home.path().to_string_lossy().into_owned())];
            assert!(validate_resume(provider, cwd.path(), "original", &env, &[]).is_ok());
            for id in ["other", "empty", "missing"] {
                assert!(
                    validate_resume(provider, cwd.path(), id, &env, &[]).is_err(),
                    "{provider}: {id}"
                );
            }
            let before = std::fs::read_dir(&root).unwrap().count();
            assert!(validate_resume(provider, cwd.path(), "missing", &env, &[]).is_err());
            assert_eq!(
                std::fs::read_dir(&root).unwrap().count(),
                before,
                "Resume must not create history"
            );
        }
    }

    #[test]
    fn cursor_resume_checks_exact_project_store_and_saved_root() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let hash = format!(
            "{:x}",
            md5::compute(cwd.path().to_string_lossy().as_bytes())
        );
        let dir = root.path().join(hash).join("exact");
        std::fs::create_dir_all(&dir).unwrap();
        let db = rusqlite::Connection::open(dir.join("store.db")).unwrap();
        db.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT); CREATE TABLE blobs(id TEXT PRIMARY KEY, data BLOB);").unwrap();
        let metadata = serde_json::json!({"agentId":"exact","createdAt":1790950000000_i64,"latestRootBlobId":"root","name":"Original task"}).to_string();
        let hex = metadata
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        db.execute("INSERT INTO meta VALUES ('0', ?1)", [hex])
            .unwrap();
        assert!(read_cursor_sessions(root.path(), cwd.path())
            .unwrap()
            .is_empty());
        db.execute(
            "INSERT INTO blobs VALUES ('root', ?1)",
            [b"saved history".as_slice()],
        )
        .unwrap();
        let rows = read_cursor_sessions(root.path(), cwd.path()).unwrap();
        assert!(listed_session_exists(&rows, cwd.path(), "exact"));
        assert!(!listed_session_exists(&rows, cwd.path(), "missing"));
        let other = tempfile::tempdir().unwrap();
        assert!(read_cursor_sessions(root.path(), other.path())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_broken_list_is_an_error_not_a_missing_conversation() {
        assert!(parse_sessions("not json").is_err());
    }
}
