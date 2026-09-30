//! Board request policy and transport, independent of widgets. One read per
//! project and one mutation per target; changes during a read request a fresh
//! snapshot instead of publishing an already-obsolete one.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc::SyncSender;

use crate::session::daemon;

#[derive(Debug, Clone)]
pub(super) enum Mutation {
    Add { title: String },
    Move { card_id: String, column: String, revision: u64 },
    Toggle { card_id: String, was_done: bool, revision: u64 },
}

impl Mutation {
    fn key(&self) -> &str {
        match self {
            Self::Add { .. } => "add",
            Self::Move { card_id, .. } | Self::Toggle { card_id, .. } => card_id,
        }
    }

    fn run(&self, home: &Path, project_id: i64) -> Result<(), String> {
        let command = super::gui_command_id("board");
        let result = match self {
            Self::Add { title } => daemon::board_card_add(home, project_id, None, title, "", None, &command),
            Self::Move { card_id, column, revision } => daemon::board_card_move(home, project_id, card_id, column, Some(*revision), &command),
            Self::Toggle { card_id, was_done, revision } => {
                if *was_done {
                    daemon::board_card_reopen(home, project_id, card_id, Some(*revision), &command)
                } else {
                    daemon::board_card_complete(home, project_id, card_id, Some(*revision), &command)
                }
            }
        };
        result.map(|_| ()).map_err(|error| error.to_string())
    }
}

#[derive(Default)]
pub(super) struct Requests {
    reads: HashSet<i64>,
    dirty: HashSet<i64>,
    writes: HashSet<(i64, String)>,
    pub errors: HashMap<i64, String>,
}

impl Requests {
    pub fn begin_read(&mut self, project_id: i64) -> bool {
        if self.reads.insert(project_id) {
            true
        } else {
            self.dirty.insert(project_id);
            false
        }
    }

    /// True means a change arrived during the read: discard its result and
    /// fetch again. Do not confuse a stale read with an authoritative deletion.
    pub fn finish_read(&mut self, project_id: i64) -> bool {
        self.reads.remove(&project_id);
        self.dirty.remove(&project_id)
    }

    pub fn pending(&self, project_id: i64, key: &str) -> bool {
        self.writes.contains(&(project_id, key.to_string()))
    }

    pub fn begin_write(&mut self, project_id: i64, mutation: &Mutation) -> bool {
        self.writes.insert((project_id, mutation.key().to_string()))
    }

    pub fn finish_write(&mut self, project_id: i64, mutation: &Mutation) {
        self.writes.remove(&(project_id, mutation.key().to_string()));
    }
}

pub(super) fn read(home: &Path, project_id: i64, tx: &SyncSender<super::ActivityNotice>) {
    let (home, tx) = (home.to_path_buf(), tx.clone());
    std::thread::spawn(move || {
        let result = daemon::board_state(&home, project_id).map_err(|error| error.to_string());
        let _ = tx.send(super::ActivityNotice::BoardLoaded { project_id, result });
    });
}

pub(super) fn mutate(home: &Path, project_id: i64, mutation: Mutation, tx: &SyncSender<super::ActivityNotice>) {
    let (home, tx) = (home.to_path_buf(), tx.clone());
    std::thread::spawn(move || {
        let result = mutation.run(&home, project_id);
        let _ = tx.send(super::ActivityNotice::BoardMutation { project_id, mutation, result });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_during_a_read_are_coalesced_without_losing_the_refresh() {
        let mut requests = Requests::default();
        assert!(requests.begin_read(1));
        assert!(!requests.begin_read(1));
        assert!(!requests.begin_read(1));
        assert!(requests.begin_read(2));
        assert!(requests.finish_read(1));
        assert!(requests.begin_read(1));
        assert!(!requests.finish_read(1));
        assert!(!requests.finish_read(2));
    }

    #[test]
    fn mutations_are_serialized_per_card_not_per_project() {
        let mut requests = Requests::default();
        let toggle = Mutation::Toggle { card_id: "card".into(), was_done: false, revision: 1 };
        let movement = Mutation::Move { card_id: "card".into(), column: "Review".into(), revision: 1 };
        assert!(requests.begin_write(1, &toggle));
        assert!(!requests.begin_write(1, &movement));
        assert!(requests.begin_write(2, &movement));
        assert!(requests.begin_write(1, &Mutation::Add { title: "New".into() }));
        requests.finish_write(1, &toggle);
        assert!(requests.begin_write(1, &movement));
    }
}
