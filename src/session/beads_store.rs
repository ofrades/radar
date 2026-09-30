//! The board, backed by a radar-managed Beads workspace.
//!
//! One `bd` workspace per project, under a radar-managed root (never the
//! project's own repository), so adopting Beads does not litter a project with
//! `.beads`, hooks or agent skills. The daemon stays the only writer: every
//! mutation shells out to `bd` and re-reads the board.
//!
//! The first time a project's workspace is created, its existing SQLite board
//! (if any) is migrated in — cards become issues, lanes become statuses,
//! claims become assignees, and done cards are closed. A `.migrated-from-sqlite`
//! marker makes that run exactly once.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;

use super::beads::{self, lane_kind, lane_name, Beads, StatusSpec};
use super::board_store::{
    check_revision, Board, BoardChange, BoardState, BoardStore, Lane, StoredCard,
};

/// The custom statuses a radar board needs: `review` is a lane of its own, and
/// `backlog` and `test` are the workflow radar's board has always had.
const CUSTOM_STATUSES: &str = "backlog:active,test:wip,review:wip";

/// A board stored in a radar-managed Beads workspace.
pub struct BeadsBoardStore {
    beads: Beads,
    root: PathBuf,
    /// The old SQLite board, read once per project to migrate its cards.
    legacy: Option<BoardStore>,
    /// One lock per project, so creating a workspace and migrating its old
    /// board serialize while unrelated projects stay parallel.
    init: Mutex<HashMap<i64, Arc<Mutex<()>>>>,
    /// Beads stamps updates at second granularity (millis in the board
    /// contract), so two mutations inside one second read the same revision
    /// and a stale write cannot be told from a fresh one. The daemon is the
    /// single writer: remember the highest revision it issued per card and
    /// never let a card's revision go backwards or repeat.
    issued: Mutex<HashMap<(i64, String), u64>>,
    /// Valid statuses per workspace, cached: they change only through
    /// `bd config set`, which radar runs once at workspace creation. Each
    /// `bd` call pays the embedded-Dolt startup (~0.2s), so this halves every
    /// board read.
    statuses: Mutex<HashMap<PathBuf, Arc<Vec<StatusSpec>>>>,
    /// The last board state per project. The daemon is the store's only
    /// writer, so mutations can patch this in place and reads are free; a
    /// card is re-read with `bd show` after each mutation to stay honest.
    states: Mutex<HashMap<i64, BoardState>>,
}

impl BeadsBoardStore {
    /// Open the store over `root`, one workspace per project.
    pub fn open(root: impl Into<PathBuf>, legacy: Option<BoardStore>) -> Result<Self> {
        let beads = Beads::locate()?;
        let root = root.into();
        fs::create_dir_all(&root)
            .with_context(|| format!("creating the beads root {}", root.display()))?;
        Ok(Self {
            beads,
            root,
            legacy,
            init: Mutex::new(HashMap::new()),
            issued: Mutex::new(HashMap::new()),
            statuses: Mutex::new(HashMap::new()),
            states: Mutex::new(HashMap::new()),
        })
    }

    fn project_dir(&self, project_id: i64) -> PathBuf {
        self.root.join(project_id.to_string())
    }

    /// The lock guarding a project's one-time setup.
    fn init_lock(&self, project_id: i64) -> Arc<Mutex<()>> {
        Arc::clone(self.init.lock().entry(project_id).or_default())
    }

    /// Whether a project's workspace exists and has been migrated.
    fn is_ready(dir: &Path) -> bool {
        dir.join(".beads").is_dir() && dir.join(".migrated-from-sqlite").exists()
    }

    /// Ensure the project's workspace exists and has been migrated, then return
    /// its directory.
    ///
    /// The daemon serves each connection on its own thread, so two board reads
    /// for a project can enter together the first time it is touched. Creating
    /// the workspace and importing the old SQLite board must happen once, not
    /// once per thread: without the lock each entrant would import every card
    /// and duplicate the board.
    fn ensure(&self, project_id: i64) -> Result<PathBuf> {
        let dir = self.project_dir(project_id);
        if Self::is_ready(&dir) {
            return Ok(dir);
        }
        let lock = self.init_lock(project_id);
        let _guard = lock.lock();
        // Re-check under the lock: another thread may have finished the work
        // while this one waited.
        if !dir.join(".beads").is_dir() {
            fs::create_dir_all(&dir)
                .with_context(|| format!("creating the board directory {}", dir.display()))?;
            let prefix = format!("r{project_id}");
            self.beads.run(
                &dir,
                &[
                    "init",
                    "--prefix",
                    &prefix,
                    "--init-if-missing",
                    "--non-interactive",
                    "--quiet",
                ],
            )?;
            self.beads
                .run(&dir, &["config", "set", "status.custom", CUSTOM_STATUSES])?;
        }
        let marker = dir.join(".migrated-from-sqlite");
        if !marker.exists() {
            self.migrate(project_id, &dir)?;
            fs::write(&marker, b"1").with_context(|| format!("writing {}", marker.display()))?;
        }
        Ok(dir)
    }

    /// Import the project's old SQLite cards, once.
    fn migrate(&self, project_id: i64, dir: &Path) -> Result<()> {
        let Some(legacy) = &self.legacy else {
            return Ok(());
        };
        let state = legacy.state(project_id)?;
        for card in state.cards {
            let status = beads::status_for_lane(&card.lane);
            let id = self
                .create_issue(
                    dir,
                    &card.title,
                    &card.body,
                    Some(&status),
                    card.claim.as_deref(),
                )?
                .id;
            if card.done {
                self.beads.run(dir, &["close", &id, "--force"])?;
            }
        }
        Ok(())
    }

    /// Create an issue and return its full JSON (the same shape `bd show`
    /// reports), so the caller can patch the state without another spawn.
    fn create_issue(
        &self,
        dir: &Path,
        title: &str,
        body: &str,
        status: Option<&str>,
        claim: Option<&str>,
    ) -> Result<beads::Issue> {
        let mut args: Vec<String> = vec![
            "create".to_string(),
            title.to_string(),
            "--json".to_string(),
        ];
        if !body.is_empty() {
            args.push("--description".to_string());
            args.push(body.to_string());
        }
        if let Some(status) = status {
            args.push("--status".to_string());
            args.push(status.to_string());
        }
        if let Some(claim) = claim {
            args.push("--assignee".to_string());
            args.push(claim.to_string());
        }
        let output = self.run_args(dir, &args)?;
        let issue = beads::issue_from_json(&output)
            .with_context(|| format!("parsing `bd create` in {}", dir.display()))?;
        if issue.id.is_empty() {
            bail!("bd create returned no issue id");
        }
        Ok(issue)
    }

    fn run_args(&self, dir: &Path, args: &[String]) -> Result<String> {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.beads.run(dir, &refs)
    }

    /// Valid statuses in the workspace, from the cache after the first read.
    fn statuses(&self, dir: &Path) -> Result<Arc<Vec<StatusSpec>>> {
        let mut cache = self.statuses.lock();
        if let Some(statuses) = cache.get(dir) {
            return Ok(Arc::clone(statuses));
        }
        let statuses = Arc::new(self.beads.statuses(dir)?);
        cache.insert(dir.to_path_buf(), Arc::clone(&statuses));
        Ok(statuses)
    }

    fn board_state(&self, project_id: i64, dir: &Path) -> Result<BoardState> {
        let statuses = self.statuses(dir)?;
        let issues = self.beads.list(dir)?;
        Ok(beads::board_state(project_id, &statuses, &issues))
    }

    /// The project's board: the cached state when there is one, one full read
    /// when there is not.
    fn cached_state(&self, project_id: i64, dir: &Path) -> Result<BoardState> {
        if let Some(state) = self.states.lock().get(&project_id) {
            return Ok(state.clone());
        }
        let state = self.board_state(project_id, dir)?;
        self.states.lock().insert(project_id, state.clone());
        Ok(state)
    }

    /// Fold a freshly read card into the cached state, adding its lane when a
    /// status is used for the first time.
    fn patch_state(&self, project_id: i64, dir: &Path, issue: beads::Issue) -> Result<()> {
        let statuses = self.statuses(dir)?;
        let mut states = self.states.lock();
        let state = states
            .entry(project_id)
            .or_insert_with(|| beads::board_state(project_id, &statuses, &[]));
        let status = issue.status.clone();
        let kind = |status: &str| {
            statuses
                .iter()
                .find(|spec| spec.name == *status)
                .map(|spec| lane_kind(&spec.category))
                .unwrap_or("custom")
        };
        let lane_id = match state
            .lanes
            .iter()
            .find(|lane| lane.name == lane_name(&status))
        {
            Some(lane) => lane.id,
            None => {
                // A status with cards becomes a lane, ordered by workflow.
                let id = state.lanes.iter().map(|lane| lane.id).max().unwrap_or(0) + 1;
                let position = state.lanes.len() as i64;
                let name = lane_name(&status);
                state.lanes.push(Lane {
                    id,
                    name: name.clone(),
                    kind: kind(&status).to_string(),
                    position,
                });
                state.lanes.sort_by_key(|lane| lane.position);
                // Reindex positions after the sort so a later insert stays stable.
                for (index, lane) in state.lanes.iter_mut().enumerate() {
                    lane.position = index as i64;
                }
                id
            }
        };
        state.cards.retain(|card| card.id != issue.id);
        state.cards.push(StoredCard {
            id: issue.id.clone(),
            project_id,
            lane_id,
            lane: lane_name(&status),
            done: kind(&status) == "done",
            position: state.cards.len() as i64,
            title: issue.title.clone(),
            body: issue.description.clone(),
            claim: issue.assignee.clone(),
            revision: issue.updated_at_millis.max(1) as u64,
            created_at_millis: issue.created_at_millis,
            updated_at_millis: issue.updated_at_millis,
        });
        Ok(())
    }

    /// Drop a deleted card from the cached state.
    fn forget_card(&self, project_id: i64, card_id: &str) {
        if let Some(state) = self.states.lock().get_mut(&project_id) {
            state.cards.retain(|card| card.id != card_id);
        }
    }

    /// The card and its lane kind, so a claim can start a Todo card and leave a
    /// card in Review where it is. Reads the cached state (no spawn); the
    /// state is patched after every mutation, so it is current.
    fn locate(&self, project_id: i64, dir: &Path, card_id: &str) -> Result<(StoredCard, String)> {
        let state = self.cached_state(project_id, dir)?;
        let card = state
            .cards
            .iter()
            .find(|card| card.id == card_id)
            .cloned()
            .with_context(|| format!("no card {card_id}"))?;
        let kind = state
            .lanes
            .iter()
            .find(|lane| lane.id == card.lane_id)
            .map(|lane| lane.kind.clone())
            .unwrap_or_default();
        let mut card = card;
        card.revision = self.effective_revision(project_id, &card);
        Ok((card, kind))
    }

    /// The revision a card carries now: the store's stamp, but never below a
    /// revision this daemon already handed out (beads cannot bump within one
    /// of its own stamps).
    fn effective_revision(&self, project_id: i64, card: &StoredCard) -> u64 {
        let issued = *self
            .issued
            .lock()
            .get(&(project_id, card.id.clone()))
            .unwrap_or(&0);
        card.revision.max(issued)
    }

    /// Record the revision a mutation hands out, keeping it strictly above
    /// every revision this card had before.
    fn note_revision(&self, project_id: i64, card_id: &str, revision: u64) {
        let mut issued = self.issued.lock();
        let slot = issued.entry((project_id, card_id.to_string())).or_default();
        *slot = (*slot).max(revision);
    }

    /// The fresh card after a mutation: one `bd show` (a full re-list costs
    /// another Dolt startup for nothing), folded into the cached state.
    fn change(
        &self,
        project_id: i64,
        dir: &Path,
        card_id: &str,
        action: &str,
        from_lane: Option<String>,
    ) -> Result<BoardChange> {
        // The floor this card already reached, before reading the fresh stamp.
        let floor = self
            .issued
            .lock()
            .get(&(project_id, card_id.to_string()))
            .copied()
            .unwrap_or(0);
        let issue = self.beads.show(dir, card_id)?;
        self.patch_state(project_id, dir, issue)?;
        let mut card = self
            .states
            .lock()
            .get(&project_id)
            .and_then(|state| state.cards.iter().find(|card| card.id == card_id).cloned())
            .with_context(|| format!("card {card_id} vanished after {action}"))?;
        // A store stamp equal to the floor is an unchanged stamp, not an
        // unchanged card: this mutation must still be conflict-visible.
        card.revision = card.revision.max(floor + 1);
        self.note_revision(project_id, card_id, card.revision);
        Ok(BoardChange {
            card,
            action: action.to_string(),
            from_lane,
        })
    }

    pub fn state(&self, project_id: i64) -> Result<BoardState> {
        let dir = self.ensure(project_id)?;
        self.cached_state(project_id, &dir)
    }

    pub fn add_card(
        &self,
        project_id: i64,
        lane: Option<&str>,
        title: &str,
        body: &str,
        claim: Option<&str>,
    ) -> Result<BoardChange> {
        let dir = self.ensure(project_id)?;
        let status = lane.map(beads::status_for_lane);
        let issue = self.create_issue(&dir, title, body, status.as_deref(), claim)?;
        // `bd create` already reported the full issue; no re-read needed.
        let floor = self
            .issued
            .lock()
            .get(&(project_id, issue.id.clone()))
            .copied()
            .unwrap_or(0);
        self.patch_state(project_id, &dir, issue.clone())?;
        let mut card = self
            .states
            .lock()
            .get(&project_id)
            .and_then(|state| state.cards.iter().find(|card| card.id == issue.id).cloned())
            .with_context(|| format!("card {} vanished after added", issue.id))?;
        card.revision = card.revision.max(floor + 1);
        self.note_revision(project_id, &issue.id, card.revision);
        Ok(BoardChange {
            card,
            action: "added".to_string(),
            from_lane: None,
        })
    }

    pub fn update_card(
        &self,
        project_id: i64,
        card_id: &str,
        title: Option<&str>,
        body: Option<&str>,
        expected_revision: Option<u64>,
    ) -> Result<BoardChange> {
        let dir = self.ensure(project_id)?;
        let (card, _) = self.locate(project_id, &dir, card_id)?;
        check_revision(&card, expected_revision)?;
        let mut args = vec!["update".to_string(), card_id.to_string()];
        if let Some(title) = title {
            args.push("--title".to_string());
            args.push(title.to_string());
        }
        if let Some(body) = body {
            args.push("--description".to_string());
            args.push(body.to_string());
        }
        if args.len() > 2 {
            self.run_args(&dir, &args)?;
        }
        self.change(project_id, &dir, card_id, "updated", None)
    }

    pub fn move_card(
        &self,
        project_id: i64,
        card_id: &str,
        lane: &str,
        expected_revision: Option<u64>,
    ) -> Result<BoardChange> {
        let dir = self.ensure(project_id)?;
        let (card, _) = self.locate(project_id, &dir, card_id)?;
        check_revision(&card, expected_revision)?;
        let target = beads::status_for_lane(lane);
        if target != beads::status_for_lane(&card.lane) {
            self.set_status(&dir, card_id, &target)?;
            // Moving a card drops its claim: a card handed to another column is
            // by definition no longer the worker's. Clearing an absent claim
            // would buy another Dolt startup for nothing.
            if card.claim.is_some() {
                self.run_args(
                    &dir,
                    &[
                        "assign".to_string(),
                        card_id.to_string(),
                        String::new(),
                        "--force".to_string(),
                    ],
                )?;
            }
        }
        self.change(project_id, &dir, card_id, "moved", Some(card.lane))
    }

    pub fn claim_card(
        &self,
        project_id: i64,
        card_id: &str,
        claim: Option<&str>,
        expected_revision: Option<u64>,
    ) -> Result<BoardChange> {
        let dir = self.ensure(project_id)?;
        let (card, kind) = self.locate(project_id, &dir, card_id)?;
        check_revision(&card, expected_revision)?;
        match claim {
            Some(name) => {
                // Claiming starts a Todo card and leaves any other lane alone.
                if kind == "todo" {
                    self.run_args(
                        &dir,
                        &[
                            "update".to_string(),
                            card_id.to_string(),
                            "-s".to_string(),
                            "in_progress".to_string(),
                            "-a".to_string(),
                            name.to_string(),
                            "--force".to_string(),
                        ],
                    )?;
                } else {
                    self.run_args(
                        &dir,
                        &[
                            "assign".to_string(),
                            card_id.to_string(),
                            name.to_string(),
                            "--force".to_string(),
                        ],
                    )?;
                }
                let from_lane = (kind == "todo").then(|| card.lane.clone());
                self.change(project_id, &dir, card_id, "claimed", from_lane)
            }
            None => {
                if card.claim.is_some() {
                    self.run_args(
                        &dir,
                        &[
                            "assign".to_string(),
                            card_id.to_string(),
                            String::new(),
                            "--force".to_string(),
                        ],
                    )?;
                }
                if kind == "in_progress" {
                    self.run_args(
                        &dir,
                        &[
                            "update".to_string(),
                            card_id.to_string(),
                            "-s".to_string(),
                            "open".to_string(),
                        ],
                    )?;
                }
                let from_lane = (kind == "in_progress").then(|| card.lane.clone());
                self.change(project_id, &dir, card_id, "released", from_lane)
            }
        }
    }

    pub fn complete_card(
        &self,
        project_id: i64,
        card_id: &str,
        expected_revision: Option<u64>,
    ) -> Result<BoardChange> {
        let dir = self.ensure(project_id)?;
        let (card, _) = self.locate(project_id, &dir, card_id)?;
        check_revision(&card, expected_revision)?;
        self.run_args(
            &dir,
            &[
                "close".to_string(),
                card_id.to_string(),
                "--force".to_string(),
            ],
        )?;
        self.change(project_id, &dir, card_id, "completed", Some(card.lane))
    }

    pub fn reopen_card(
        &self,
        project_id: i64,
        card_id: &str,
        expected_revision: Option<u64>,
    ) -> Result<BoardChange> {
        let dir = self.ensure(project_id)?;
        let (card, _) = self.locate(project_id, &dir, card_id)?;
        check_revision(&card, expected_revision)?;
        self.run_args(&dir, &["reopen".to_string(), card_id.to_string()])?;
        self.change(project_id, &dir, card_id, "reopened", Some(card.lane))
    }

    pub fn remove_card(&self, project_id: i64, card_id: &str) -> Result<BoardChange> {
        let dir = self.ensure(project_id)?;
        let (card, _) = self.locate(project_id, &dir, card_id)?;
        self.run_args(
            &dir,
            &[
                "delete".to_string(),
                card_id.to_string(),
                "--force".to_string(),
            ],
        )?;
        self.forget_card(project_id, card_id);
        Ok(BoardChange {
            card,
            action: "removed".to_string(),
            from_lane: None,
        })
    }

    pub fn next_card(
        &self,
        project_id: i64,
        who: &str,
        lane: Option<&str>,
    ) -> Result<Option<BoardChange>> {
        let dir = self.ensure(project_id)?;
        let state = self.cached_state(project_id, &dir)?;
        let filter = lane.map(beads::status_for_lane);
        let found = state
            .cards
            .iter()
            .find(|card| {
                !card.done
                    && card.claim.is_none()
                    && filter
                        .as_deref()
                        .map_or(true, |status| beads::status_for_lane(&card.lane) == status)
            })
            .cloned();
        let Some(card) = found else {
            return Ok(None);
        };
        let kind = state
            .lanes
            .iter()
            .find(|lane| lane.id == card.lane_id)
            .map(|lane| lane.kind.clone())
            .unwrap_or_default();
        if kind == "todo" {
            self.run_args(
                &dir,
                &[
                    "update".to_string(),
                    card.id.clone(),
                    "-s".to_string(),
                    "in_progress".to_string(),
                    "-a".to_string(),
                    who.to_string(),
                    "--force".to_string(),
                ],
            )?;
        } else {
            self.run_args(
                &dir,
                &[
                    "assign".to_string(),
                    card.id.clone(),
                    who.to_string(),
                    "--force".to_string(),
                ],
            )?;
        }
        let from_lane = (kind == "todo").then(|| card.lane.clone());
        Ok(Some(self.change(
            project_id, &dir, &card.id, "claimed", from_lane,
        )?))
    }

    /// Move an issue to a status, closing when the target is the done status.
    fn set_status(&self, dir: &Path, card_id: &str, status: &str) -> Result<()> {
        if status == "closed" {
            self.run_args(
                dir,
                &[
                    "close".to_string(),
                    card_id.to_string(),
                    "--force".to_string(),
                ],
            )?;
        } else {
            self.run_args(
                dir,
                &[
                    "update".to_string(),
                    card_id.to_string(),
                    "-s".to_string(),
                    status.to_string(),
                ],
            )?;
        }
        Ok(())
    }
}

impl Board for BeadsBoardStore {
    fn state(&self, project_id: i64) -> Result<BoardState> {
        BeadsBoardStore::state(self, project_id)
    }
    fn add_card(
        &self,
        project_id: i64,
        lane: Option<&str>,
        title: &str,
        body: &str,
        claim: Option<&str>,
    ) -> Result<BoardChange> {
        BeadsBoardStore::add_card(self, project_id, lane, title, body, claim)
    }
    fn update_card(
        &self,
        project_id: i64,
        card_id: &str,
        title: Option<&str>,
        body: Option<&str>,
        expected_revision: Option<u64>,
    ) -> Result<BoardChange> {
        BeadsBoardStore::update_card(self, project_id, card_id, title, body, expected_revision)
    }
    fn move_card(
        &self,
        project_id: i64,
        card_id: &str,
        lane: &str,
        expected_revision: Option<u64>,
    ) -> Result<BoardChange> {
        BeadsBoardStore::move_card(self, project_id, card_id, lane, expected_revision)
    }
    fn claim_card(
        &self,
        project_id: i64,
        card_id: &str,
        claim: Option<&str>,
        expected_revision: Option<u64>,
    ) -> Result<BoardChange> {
        BeadsBoardStore::claim_card(self, project_id, card_id, claim, expected_revision)
    }
    fn complete_card(
        &self,
        project_id: i64,
        card_id: &str,
        expected_revision: Option<u64>,
    ) -> Result<BoardChange> {
        BeadsBoardStore::complete_card(self, project_id, card_id, expected_revision)
    }
    fn reopen_card(
        &self,
        project_id: i64,
        card_id: &str,
        expected_revision: Option<u64>,
    ) -> Result<BoardChange> {
        BeadsBoardStore::reopen_card(self, project_id, card_id, expected_revision)
    }
    fn remove_card(&self, project_id: i64, card_id: &str) -> Result<BoardChange> {
        BeadsBoardStore::remove_card(self, project_id, card_id)
    }
    fn next_card(
        &self,
        project_id: i64,
        who: &str,
        lane: Option<&str>,
    ) -> Result<Option<BoardChange>> {
        BeadsBoardStore::next_card(self, project_id, who, lane)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(legacy: Option<BoardStore>) -> Option<(BeadsBoardStore, tempfile::TempDir)> {
        let dir = tempfile::tempdir().ok()?;
        let store = BeadsBoardStore::open(dir.path(), legacy).ok()?;
        Some((store, dir))
    }

    #[test]
    fn a_card_round_trips_through_the_whole_lifecycle() {
        let Some((store, _dir)) = store(None) else {
            eprintln!("skipping: bd is not installed");
            return;
        };
        let added = store
            .add_card(9, None, "Fix login", "the 302 loop", None)
            .unwrap();
        assert_eq!(added.action, "added");
        assert_eq!(added.card.lane, "Todo");
        let id = added.card.id.clone();

        let claimed = store.claim_card(9, &id, Some("opencode-1"), None).unwrap();
        assert_eq!(claimed.action, "claimed");
        assert_eq!(claimed.card.lane, "In progress");
        assert_eq!(claimed.card.claim.as_deref(), Some("opencode-1"));
        assert_eq!(claimed.from_lane.as_deref(), Some("Todo"));

        let moved = store.move_card(9, &id, "Review", None).unwrap();
        assert_eq!(moved.card.lane, "Review");
        // Moving drops the claim.
        assert_eq!(moved.card.claim, None);

        let done = store.complete_card(9, &id, None).unwrap();
        assert_eq!(done.card.lane, "Done");
        assert!(done.card.done);

        let reopened = store.reopen_card(9, &id, None).unwrap();
        assert_eq!(reopened.card.lane, "Todo");
        assert!(!reopened.card.done);
    }

    #[test]
    fn claiming_starts_a_todo_card_and_leaves_review_alone() {
        let Some((store, _dir)) = store(None) else {
            eprintln!("skipping: bd is not installed");
            return;
        };
        let review = store
            .add_card(3, Some("Review"), "Check", "", None)
            .unwrap();
        assert_eq!(review.card.lane, "Review");
        let claimed = store
            .claim_card(3, &review.card.id, Some("reviewer"), None)
            .unwrap();
        assert_eq!(claimed.card.lane, "Review");
        assert_eq!(claimed.from_lane, None);
        assert_eq!(claimed.card.claim.as_deref(), Some("reviewer"));

        let released = store.claim_card(3, &review.card.id, None, None).unwrap();
        assert_eq!(released.card.lane, "Review");
        assert_eq!(released.card.claim, None);
    }

    #[test]
    fn next_claims_the_first_open_card() {
        let Some((store, _dir)) = store(None) else {
            eprintln!("skipping: bd is not installed");
            return;
        };
        store.add_card(1, Some("Todo"), "first", "", None).unwrap();
        store
            .add_card(1, Some("In progress"), "second", "", None)
            .unwrap();
        let claimed = store.next_card(1, "codex-1", None).unwrap().unwrap();
        assert_eq!(claimed.card.title, "first");
        assert_eq!(claimed.card.lane, "In progress");
        assert_eq!(claimed.card.claim.as_deref(), Some("codex-1"));
    }

    #[test]
    fn a_stale_revision_is_refused() {
        let Some((store, _dir)) = store(None) else {
            eprintln!("skipping: bd is not installed");
            return;
        };
        let card = store.add_card(1, None, "Task", "", None).unwrap().card;
        store
            .update_card(1, &card.id, Some("Renamed"), None, Some(card.revision))
            .unwrap();
        let error = store
            .update_card(1, &card.id, Some("Again"), None, Some(card.revision))
            .unwrap_err();
        assert!(error.to_string().contains("changed"), "{error}");
    }

    #[test]
    fn the_old_sqlite_board_is_migrated_once() {
        let legacy = BoardStore::open_in_memory().unwrap();
        let card = legacy
            .add_card(5, None, "Legacy card", "old body", None)
            .unwrap();
        legacy
            .claim_card(5, &card.card.id, Some("opencode-1"), None)
            .unwrap();
        legacy
            .add_card(5, Some("Done"), "Shipped", "", None)
            .unwrap();

        let Some((store, _dir)) = store(Some(legacy)) else {
            eprintln!("skipping: bd is not installed");
            return;
        };
        let state = store.state(5).unwrap();
        assert_eq!(state.cards.len(), 2);
        let migrated = state
            .cards
            .iter()
            .find(|card| card.title == "Legacy card")
            .unwrap();
        assert_eq!(migrated.lane, "In progress");
        assert_eq!(migrated.claim.as_deref(), Some("opencode-1"));
        assert_eq!(migrated.body, "old body");
        let shipped = state
            .cards
            .iter()
            .find(|card| card.title == "Shipped")
            .unwrap();
        assert!(shipped.done);

        // A second read does not duplicate the migration.
        assert_eq!(store.state(5).unwrap().cards.len(), 2);
    }

    #[test]
    fn concurrent_first_reads_migrate_the_old_board_once() {
        let legacy = BoardStore::open_in_memory().unwrap();
        for index in 0..8 {
            legacy
                .add_card(11, None, &format!("Legacy {index}"), "", None)
                .unwrap();
        }

        let Some((store, _dir)) = store(Some(legacy)) else {
            eprintln!("skipping: bd is not installed");
            return;
        };
        let store = Arc::new(store);

        // The daemon serves each connection on its own thread, so the first
        // board reads for a project arrive together. Each must see the one
        // migration, not run its own and duplicate every card.
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || store.state(11).unwrap().cards.len())
            })
            .collect();
        for reader in readers {
            assert_eq!(reader.join().unwrap(), 8);
        }
        assert_eq!(store.state(11).unwrap().cards.len(), 8);
    }
}
