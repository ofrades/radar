//! Server-owned BOARD.md capture for every registered, board-enabled project.
//!
//! The activity journal stores the last observed board beside its events, so
//! a daemon restart is a continuation rather than a fresh, noisy baseline.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};

use crate::board;
use crate::config::Paths;
use crate::db::Db;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use super::activity::ActivityJournal;

const SCAN_INTERVAL: Duration = Duration::from_millis(500);
#[derive(Debug, PartialEq, Eq)]
struct BoardFingerprint {
    modified: SystemTime,
    len: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
}

#[derive(Debug, PartialEq, Eq)]
enum BoardScanState {
    Disabled,
    Enabled(Option<BoardFingerprint>),
}

type BoardScanCache = HashMap<i64, BoardScanState>;

impl BoardFingerprint {
    fn read(path: &Path) -> anyhow::Result<Option<Self>> {
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() {
            anyhow::bail!("{} is not a regular file", path.display());
        }
        Ok(Some(Self {
            modified: metadata.modified()?,
            len: metadata.len(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanoseconds: metadata.ctime_nsec(),
        }))
    }
}

/// Poll registered projects independently of any board pane or web client.
pub(super) fn run(home: &Path, activity: Arc<ActivityJournal>, stopping: Arc<AtomicBool>) {
    let paths = Paths::with_root(home.to_path_buf());
    while !stopping.load(Ordering::Acquire) {
        // The board monitor is also started by session-only daemon clients.
        // Do not create Radar's project database just to discover there are
        // no registered projects; CLI/GUI startup creates it when needed.
        if paths.database().is_file() {
            match Db::open(&paths) {
                Ok(db) => {
                    let mut cache = BoardScanCache::new();
                    while !stopping.load(Ordering::Acquire) {
                        if let Err(error) = scan_registered(&db, &activity, &mut cache) {
                            eprintln!("radar board monitor: {error:#}");
                        }
                        let mut slept = Duration::ZERO;
                        while slept < SCAN_INTERVAL && !stopping.load(Ordering::Acquire) {
                            let step = Duration::from_millis(50).min(SCAN_INTERVAL - slept);
                            thread::sleep(step);
                            slept += step;
                        }
                    }
                    return;
                }
                Err(error) => eprintln!("radar board monitor database: {error:#}"),
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn scan_registered(
    db: &Db,
    activity: &ActivityJournal,
    cache: &mut BoardScanCache,
) -> anyhow::Result<()> {
    let projects = db.projects()?;
    let registered: HashSet<i64> = projects.iter().map(|project| project.id).collect();
    cache.retain(|project_id, _| registered.contains(project_id));

    for project in projects {
        let settings = db.project_settings(project.id)?;
        if !settings.board_enabled {
            if cache.get(&project.id) != Some(&BoardScanState::Disabled) {
                if let Err(error) = activity.forget_board(project.id) {
                    eprintln!("radar board monitor: project {}: {error:#}", project.id);
                    continue;
                }
                cache.insert(project.id, BoardScanState::Disabled);
            }
            continue;
        }
        if matches!(cache.get(&project.id), Some(BoardScanState::Disabled)) {
            cache.remove(&project.id);
        }

        let file = board::file_path(&project.path);
        let fingerprint = match BoardFingerprint::read(&file) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                eprintln!("radar board monitor: {}: {error:#}", file.display());
                continue;
            }
        };
        let Some(fingerprint) = fingerprint else {
            record_missing(project.id, &file, activity, cache);
            continue;
        };
        if matches!(
            cache.get(&project.id),
            Some(BoardScanState::Enabled(Some(cached))) if cached == &fingerprint
        ) {
            continue;
        }

        if let Err(error) = board::ensure_card_ids(&project.path) {
            eprintln!("radar board monitor: {}: {error:#}", file.display());
            continue;
        }
        let after_migration = match BoardFingerprint::read(&file) {
            Ok(Some(fingerprint)) => fingerprint,
            Ok(None) => {
                record_missing(project.id, &file, activity, cache);
                continue;
            }
            Err(error) => {
                eprintln!("radar board monitor: {}: {error:#}", file.display());
                continue;
            }
        };
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                record_missing(project.id, &file, activity, cache);
                continue;
            }
            Err(error) => {
                eprintln!("radar board monitor: {}: {error}", file.display());
                continue;
            }
        };
        match BoardFingerprint::read(&file) {
            Ok(Some(after_read)) if after_read == after_migration => {}
            Ok(None) => {
                record_missing(project.id, &file, activity, cache);
                continue;
            }
            Ok(Some(_)) => continue,
            Err(error) => {
                eprintln!("radar board monitor: {}: {error:#}", file.display());
                continue;
            }
        }

        let snapshot = board::parse(&text);
        match activity.reconcile_board(project.id, Some(&snapshot)) {
            Ok(_) => {
                cache.insert(project.id, BoardScanState::Enabled(Some(after_migration)));
            }
            Err(error) => eprintln!(
                "radar board monitor: project {} ({}): {error:#}",
                project.id,
                file.display()
            ),
        }
    }
    Ok(())
}

fn record_missing(
    project_id: i64,
    file: &Path,
    activity: &ActivityJournal,
    cache: &mut BoardScanCache,
) {
    if cache.get(&project_id) == Some(&BoardScanState::Enabled(None)) {
        return;
    }
    match activity.reconcile_board(project_id, None) {
        Ok(_) => {
            cache.insert(project_id, BoardScanState::Enabled(None));
        }
        Err(error) => eprintln!(
            "radar board monitor: project {project_id} ({}): {error:#}",
            file.display()
        ),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::activity::ActivityPayload;

    #[test]
    fn scans_registered_projects_without_creating_boards_and_captures_atomic_edits() {
        let root = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        let activity = ActivityJournal::open_in_memory().unwrap();
        let mut cache = BoardScanCache::new();

        // The registry is live before this project exists; the next scan
        // must pick up a project added later.
        scan_registered(&db, &activity, &mut cache).unwrap();
        let project = db.add_project(root.path()).unwrap();
        scan_registered(&db, &activity, &mut cache).unwrap();
        let file = board::file_path(root.path());
        assert!(!file.exists(), "monitoring must not create an absent board");
        assert!(activity
            .snapshot(project.id, None, 100)
            .unwrap()
            .events
            .is_empty());

        std::fs::write(
            &file,
            "## Backlog\n- [ ] Ship fix\n      <!-- radar:card-id:stable-card -->\n## Review\n",
        )
        .unwrap();
        scan_registered(&db, &activity, &mut cache).unwrap();
        let first = activity.snapshot(project.id, None, 100).unwrap();
        assert_eq!(first.events.len(), 2);
        assert!(matches!(
            &first.events[0].payload,
            ActivityPayload::BoardChanged { action, .. } if action == "board_created"
        ));
        assert!(matches!(
            &first.events[1].payload,
            ActivityPayload::BoardChanged { action, card_id: Some(id), .. }
                if action == "added" && id == "stable-card"
        ));

        let replacement = root.path().join(".board-monitor-replacement");
        std::fs::write(
            &replacement,
            "## Review\n- [ ] Ship fix\n      <!-- radar:card-id:stable-card -->\n## Backlog\n",
        )
        .unwrap();
        std::fs::rename(&replacement, &file).unwrap();
        scan_registered(&db, &activity, &mut cache).unwrap();
        let moved = activity.snapshot(project.id, None, 100).unwrap();
        assert_eq!(moved.events.len(), 3);
        assert!(matches!(
            &moved.events[2].payload,
            ActivityPayload::BoardChanged {
                action,
                column,
                from_column,
                ..
            } if action == "moved"
                && column.as_deref() == Some("Review")
                && from_column.as_deref() == Some("Backlog")
        ));
        let backup = root.path().join(".board-monitor-backup");
        std::fs::rename(&file, &backup).unwrap();
        std::fs::create_dir(&file).unwrap();
        scan_registered(&db, &activity, &mut cache).unwrap();
        assert_eq!(
            activity.snapshot(project.id, None, 100).unwrap().watermark,
            moved.watermark,
            "an unreadable board path must not be reported as a deletion"
        );
        std::fs::remove_dir(&file).unwrap();
        std::fs::rename(&backup, &file).unwrap();
        scan_registered(&db, &activity, &mut cache).unwrap();
        assert_eq!(
            activity.snapshot(project.id, None, 100).unwrap().watermark,
            moved.watermark
        );

        scan_registered(&db, &activity, &mut cache).unwrap();
        assert_eq!(
            activity.snapshot(project.id, None, 100).unwrap().watermark,
            moved.watermark,
            "an unchanged rescan must not duplicate journal events"
        );
    }
}
