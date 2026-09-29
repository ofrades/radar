//! The board: a project's kanban, stored as `BOARD.md` in the project root.
//!
//! The file is the board. Agents run in the project with no radar in sight, so
//! the format has to be plain markdown they can read and edit with the tools
//! they already have: columns are `## ` headings, cards are `- [ ]` lines, a
//! claim is an `@name` on the card's line, and indented lines under a card are
//! its notes. radar parses that into a [`Board`] for the GUI pane and the CLI,
//! and writes the same file back when a card is moved or edited there.
//!
//! Because agents edit the file directly, every mutation goes parse → change →
//! write atomically (temporary file + rename), re-reading when the file moved
//! under us, so a claim is not lost because another agent wrote first.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The board file, in the project root where agents and `git diff` see it.
pub const FILE_NAME: &str = "BOARD.md";
/// Scratch file for atomic writes; renamed over [`FILE_NAME`] when complete.
pub const TEMP_NAME: &str = ".BOARD.md.tmp";

/// Whether Radar's kanban is enabled for this project. Unregistered paths
/// have no override and retain the default-enabled behavior.
pub fn enabled(db: &crate::db::Db, project: &Path) -> Result<bool> {
    Ok(db.project_settings_for_path(project)?.board_enabled)
}

/// Persist a project's board preference in Radar's global database.
pub fn set_enabled(db: &crate::db::Db, project: &Path, enabled: bool) -> Result<()> {
    let project = db
        .project_by_path(project)?
        .context("project is not registered in Radar")?;
    db.set_project_board_enabled(project.id, enabled)
}

/// Columns a fresh board starts with.
pub const DEFAULT_COLUMNS: [&str; 4] = ["Backlog", "In progress", "Review", "Done"];

/// How many times a mutation re-reads the file before giving up.
const ATTEMPTS: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Board {
    /// Everything before the first column heading, kept verbatim: the title
    /// and the note that tells an agent how to use the file.
    pub header: String,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub cards: Vec<Card>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Card {
    /// Stable identity stored in a hidden HTML comment beside the card. Titles
    /// and claims can change without breaking activity or attention links.
    pub id: String,
    pub title: String,
    /// Notes, one line each, the indent stripped.
    pub body: Vec<String>,
    /// Who has picked the card up: the `@name` on its line.
    pub claimed_by: Option<String>,
    /// Written as `- [x]` and shown struck through; cosmetic — the column is
    /// the real state.
    pub done: bool,
}
/// One user-visible board transition, expressed using column names rather
/// than positions so inserting/reordering columns cannot fabricate moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoardChange {
    pub action: String,
    pub card_id: Option<String>,
    pub title: Option<String>,
    pub column: Option<String>,
    pub from_column: Option<String>,
}

/// Compare two persisted board states. The caller distinguishes first-seen
/// state from an absent board; `None` here therefore means a later deletion
/// or recreation, not a startup baseline.
pub(crate) fn changes(previous: Option<&Board>, current: Option<&Board>) -> Vec<BoardChange> {
    let mut result = Vec::new();
    match (previous, current) {
        (None, None) => {}
        (None, Some(board)) => {
            result.push(board_change("board_created", None, None, None, None));
            for (column, card) in cards(board) {
                result.push(board_change(
                    "added",
                    Some(card.id.clone()),
                    Some(card.title.clone()),
                    Some(column.to_string()),
                    None,
                ));
            }
        }
        (Some(board), None) => {
            for (column, card) in cards(board) {
                result.push(board_change(
                    "removed",
                    Some(card.id.clone()),
                    Some(card.title.clone()),
                    None,
                    Some(column.to_string()),
                ));
            }
            result.push(board_change("board_deleted", None, None, None, None));
        }
        (Some(before), Some(after)) => {
            for column in &before.columns {
                if !after.columns.iter().any(|value| value.name == column.name) {
                    result.push(board_change(
                        "column_removed",
                        None,
                        None,
                        Some(column.name.clone()),
                        None,
                    ));
                }
            }
            for column in &after.columns {
                if !before.columns.iter().any(|value| value.name == column.name) {
                    result.push(board_change(
                        "column_added",
                        None,
                        None,
                        Some(column.name.clone()),
                        None,
                    ));
                }
            }

            let before_cards: HashMap<&str, (&str, &Card)> = cards(before)
                .map(|(column, card)| (card.id.as_str(), (column, card)))
                .collect();
            let after_cards: HashMap<&str, (&str, &Card)> = cards(after)
                .map(|(column, card)| (card.id.as_str(), (column, card)))
                .collect();

            for (column, card) in cards(before) {
                let Some((new_column, new_card)) = after_cards.get(card.id.as_str()).copied()
                else {
                    result.push(board_change(
                        "removed",
                        Some(card.id.clone()),
                        Some(card.title.clone()),
                        None,
                        Some(column.to_string()),
                    ));
                    continue;
                };
                if column != new_column {
                    let was_done = column.eq_ignore_ascii_case("Done");
                    let is_done = new_column.eq_ignore_ascii_case("Done");
                    let action = if is_done && !was_done {
                        "completed"
                    } else if was_done && !is_done {
                        "reopened"
                    } else {
                        "moved"
                    };
                    result.push(board_change(
                        action,
                        Some(card.id.clone()),
                        Some(new_card.title.clone()),
                        Some(new_column.to_string()),
                        Some(column.to_string()),
                    ));
                } else if card.claimed_by != new_card.claimed_by {
                    result.push(board_change(
                        if new_card.claimed_by.is_some() {
                            "claimed"
                        } else {
                            "released"
                        },
                        Some(card.id.clone()),
                        Some(new_card.title.clone()),
                        Some(new_column.to_string()),
                        None,
                    ));
                } else if card.title != new_card.title {
                    result.push(board_change(
                        "renamed",
                        Some(card.id.clone()),
                        Some(new_card.title.clone()),
                        Some(new_column.to_string()),
                        None,
                    ));
                } else if card.body != new_card.body || card.done != new_card.done {
                    // `done` is a checkbox-only display detail. Completion is
                    // represented by moving to the named Done column above.
                    result.push(board_change(
                        "updated",
                        Some(card.id.clone()),
                        Some(new_card.title.clone()),
                        Some(new_column.to_string()),
                        None,
                    ));
                }
            }
            for (column, card) in cards(after) {
                if !before_cards.contains_key(card.id.as_str()) {
                    result.push(board_change(
                        "added",
                        Some(card.id.clone()),
                        Some(card.title.clone()),
                        Some(column.to_string()),
                        None,
                    ));
                }
            }
        }
    }
    result
}

fn cards(board: &Board) -> impl Iterator<Item = (&str, &Card)> {
    board.columns.iter().flat_map(|column| {
        column
            .cards
            .iter()
            .map(move |card| (column.name.as_str(), card))
    })
}

fn board_change(
    action: &str,
    card_id: Option<String>,
    title: Option<String>,
    column: Option<String>,
    from_column: Option<String>,
) -> BoardChange {
    BoardChange {
        action: action.to_string(),
        card_id,
        title,
        column,
        from_column,
    }
}

impl Card {
    pub fn new(title: impl Into<String>) -> Card {
        Card {
            id: new_card_id(),
            title: title.into(),
            body: Vec::new(),
            claimed_by: None,
            done: false,
        }
    }
}

impl Board {
    /// A board with the default columns and the how-to note.
    pub fn default_for(project_name: &str) -> Board {
        Board {
            header: default_header(project_name),
            columns: DEFAULT_COLUMNS
                .iter()
                .map(|name| Column {
                    name: (*name).to_string(),
                    cards: Vec::new(),
                })
                .collect(),
        }
    }

    pub fn column_named(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|column| column.name.eq_ignore_ascii_case(name))
    }

    /// First (column, card) whose title matches, exactly.
    pub fn find(&self, title: &str) -> Option<(usize, usize)> {
        for (c, column) in self.columns.iter().enumerate() {
            if let Some(i) = column.cards.iter().position(|card| card.title == title) {
                return Some((c, i));
            }
        }
        None
    }

    pub fn find_id(&self, id: &str) -> Option<(usize, usize)> {
        self.columns.iter().enumerate().find_map(|(column, value)| {
            value
                .cards
                .iter()
                .position(|card| card.id == id)
                .map(|card| (column, card))
        })
    }

    /// Render the canonical file: header, then one heading per column with its
    /// cards and invisible stable-ID comments.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.header);
        for column in &self.columns {
            if !out.is_empty() && !out.ends_with("\n\n") {
                out.push('\n');
            }
            out.push_str("## ");
            out.push_str(&column.name);
            out.push('\n');
            for card in &column.cards {
                out.push_str(if card.done { "- [x] " } else { "- [ ] " });
                out.push_str(&card.title);
                if let Some(who) = &card.claimed_by {
                    out.push_str(" @");
                    out.push_str(who);
                }
                out.push('\n');
                out.push_str("      <!-- radar:card-id:");
                out.push_str(&card.id);
                out.push_str(" -->\n");
                for line in &card.body {
                    out.push_str("      ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// Parse the markdown. Lenient on purpose: agents write free-form markdown, so
/// anything under a column that is not a card line becomes notes for the card
/// above it rather than being dropped. Everything before the first `## `
/// heading is the header, even lines that look like cards — that is what keeps
/// the how-to note (which contains examples) out of the columns.
pub fn parse(text: &str) -> Board {
    let mut board = Board {
        header: String::new(),
        columns: Vec::new(),
    };
    let mut in_header = true;
    let mut header = String::new();
    let mut last_card: Option<(usize, usize)> = None;

    for line in text.lines() {
        if let Some(name) = line.strip_prefix("## ") {
            in_header = false;
            board.columns.push(Column {
                name: name.trim().to_string(),
                cards: Vec::new(),
            });
            last_card = None;
            continue;
        }
        if in_header {
            header.push_str(line);
            header.push('\n');
            continue;
        }
        if let Some(card) = parse_card_line(line) {
            board.columns.last_mut().unwrap().cards.push(card);
            let c = board.columns.len() - 1;
            last_card = Some((c, board.columns[c].cards.len() - 1));
            continue;
        }
        // Notes: indented, or plain prose, under the card above.
        if let Some((c, i)) = last_card {
            let note = line.trim();
            if !note.is_empty() {
                if let Some(id) = parse_card_id(note) {
                    board.columns[c].cards[i].id = id.to_string();
                } else {
                    board.columns[c].cards[i].body.push(note.to_string());
                }
            }
        }
    }

    // Canonical header: no trailing blank lines; render puts the blank line
    // back, so parse is idempotent no matter how the file was spaced.
    let trimmed = header.trim_end().to_string();
    board.header = if trimmed.is_empty() {
        trimmed
    } else {
        format!("{trimmed}\n")
    };
    board
}

/// `- [ ] Title @who`, `- [x] Title`, even `- Title`: all cards. A title that
/// merely starts with `[` (say `- [WIP] refactor`) stays a title. Only a line
/// at column 0 is a card; an indented `- ` line is a note belonging to the
/// card above it, so Markdown lists live in notes instead of spawning cards.
fn parse_card_line(line: &str) -> Option<Card> {
    if line.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = line.trim_start().strip_prefix("- ")?;
    let mut rest = rest.trim_start();
    let mut done = false;
    if let Some(after) = rest.strip_prefix('[') {
        let (checked, after) = match after.strip_prefix('x').or_else(|| after.strip_prefix('X')) {
            Some(tail) => (true, tail),
            None => (false, after.strip_prefix(' ').unwrap_or(after)),
        };
        if let Some(tail) = after.strip_prefix(']') {
            done = checked;
            rest = tail.trim_start();
        }
    }
    if rest.is_empty() {
        return None;
    }
    let (title, claimed_by) = split_claim(rest);
    let mut card = Card::new(title);
    // A card line with no `radar:card-id` comment has no durable id yet. Give
    // it a content-derived one rather than a fresh random id, so repeated
    // parses agree and a thread keyed on it survives until the next write
    // persists the id into the file.
    card.id = stable_card_id(&card.title);
    card.done = done;
    card.claimed_by = claimed_by;
    Some(card)
}

/// A deterministic id for a card that has not been assigned one yet. The next
/// board write persists it as a `radar:card-id` comment, after which the real
/// id is read back.
fn stable_card_id(title: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    title.hash(&mut hasher);
    format!("card-auto-{:x}", hasher.finish())
}

fn parse_card_id(line: &str) -> Option<&str> {
    let id = line
        .strip_prefix("<!-- radar:card-id:")?
        .strip_suffix(" -->")?;
    if id.is_empty()
        || id.len() > 100
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return None;
    }
    Some(id)
}

fn new_card_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!(
        "card-{:x}-{:x}-{:x}",
        std::process::id(),
        timestamp,
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// The claim is a trailing `@name`, so an agent claims by appending text. All
/// trailing `@name` tokens come out of the title — the last one wins — so
/// claiming over someone else's claim is just appending your name. An `@`
/// mention in the middle of the title is prose, not a claim.
fn split_claim(title: &str) -> (String, Option<String>) {
    let mut claim = None;
    let mut bare = title;
    while let Some(token) = bare.split_whitespace().next_back() {
        let Some(name) = token.strip_prefix('@') else {
            break;
        };
        let valid = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-');
        if !valid {
            break;
        }
        if claim.is_none() {
            claim = Some(name.to_string());
        }
        bare = bare.strip_suffix(token).unwrap_or(bare).trim_end();
    }
    (bare.to_string(), claim)
}

/// The path of a project's board file.
pub fn file_path(project: &Path) -> PathBuf {
    project.join(FILE_NAME)
}

/// Fail when this project's board is disabled in Radar's global settings.
pub fn require_enabled(db: &crate::db::Db, project: &Path) -> Result<()> {
    if !enabled(db, project)? {
        bail!("the board is disabled for {}", project.display());
    }
    Ok(())
}

/// Create the board file if the project has none. Never overwrites: the file
/// belongs to the project once it exists.
pub fn ensure_file(project: &Path) -> Result<PathBuf> {
    let path = file_path(project);
    if path.exists() {
        ensure_card_ids(project)?;
        return Ok(path);
    }
    let name = project
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    std::fs::write(&path, Board::default_for(&name).render())
        .with_context(|| format!("creating {}", path.display()))?;
    Ok(path)
}

/// Check the global preference, then create the project's board if needed.
pub fn ensure_enabled_file(db: &crate::db::Db, project: &Path) -> Result<PathBuf> {
    require_enabled(db, project)?;
    ensure_file(project)
}

/// Give pre-migration cards a persistent identity without reformatting the
/// rest of an agent-authored markdown file. The marker is an HTML comment, so
/// it is invisible in rendered markdown and survives ordinary edits/moves.
pub(crate) fn ensure_card_ids(project: &Path) -> Result<()> {
    let path = file_path(project);
    for _ in 0..ATTEMPTS {
        let before = mtime(&path);
        let text = std::fs::read_to_string(&path)?;
        let lines: Vec<&str> = text.lines().collect();
        let mut output = String::with_capacity(text.len() + lines.len() * 48);
        let mut changed = false;
        let mut in_columns = false;
        let mut seen = HashSet::new();
        let mut index = 0;
        while index < lines.len() {
            let line = lines[index];
            output.push_str(line);
            output.push('\n');
            if line.starts_with("## ") {
                in_columns = true;
            }
            if !in_columns || parse_card_line(line).is_none() {
                index += 1;
                continue;
            }

            let existing = lines
                .get(index + 1)
                .and_then(|next| parse_card_id(next.trim()));
            if existing.is_some_and(|id| seen.insert(id.to_string())) {
                // Preserve the original indentation/spacing of existing metadata.
                output.push_str(lines[index + 1]);
                output.push('\n');
                index += 2;
                continue;
            }
            let mut id = parse_card_line(line).expect("checked card line").id;
            while !seen.insert(id.clone()) {
                id = new_card_id();
            }
            output.push_str("      <!-- radar:card-id:");
            output.push_str(&id);
            output.push_str(" -->\n");
            changed = true;
            index += if existing.is_some() { 2 } else { 1 };
        }
        if !changed {
            return Ok(());
        }
        if before != mtime(&path) {
            continue;
        }
        let temp = project.join(TEMP_NAME);
        std::fs::write(&temp, output).with_context(|| format!("writing {}", temp.display()))?;
        if before != mtime(&path) {
            let _ = std::fs::remove_file(&temp);
            continue;
        }
        std::fs::rename(&temp, &path)
            .with_context(|| format!("renaming over {}", path.display()))?;
        return Ok(());
    }
    bail!(
        "{} kept changing while migrating stable card IDs; nothing was written",
        path.display()
    )
}

fn normalize_card_ids(board: &mut Board) {
    let mut seen = HashSet::new();
    for card in board
        .columns
        .iter_mut()
        .flat_map(|column| &mut column.cards)
    {
        if card.id.is_empty() || !seen.insert(card.id.clone()) {
            card.id = new_card_id();
            seen.insert(card.id.clone());
        }
    }
}

/// Read the board. A missing file reads as an empty default board, so
/// `radar board next` works before anything has opened the project.
pub fn load(project: &Path) -> Result<Board> {
    let path = file_path(project);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let name = project
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "project".to_string());
            return Ok(Board::default_for(&name));
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    Ok(parse(&text))
}

/// Write the board atomically: temporary file, then rename, so a reader never
/// sees half a file.
pub fn save(project: &Path, board: &Board) -> Result<()> {
    let path = file_path(project);
    let temp = project.join(TEMP_NAME);
    std::fs::write(&temp, board.render()).with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, &path).with_context(|| format!("renaming over {}", path.display()))?;
    Ok(())
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Apply `change` to the board and save it, retrying when the file changed
/// under us. `Ok(None)` means "nothing to do" (no card by that name): no
/// write, no retry.
fn edit<T>(project: &Path, change: impl Fn(&mut Board) -> Result<Option<T>>) -> Result<Option<T>> {
    let path = file_path(project);
    for _ in 0..ATTEMPTS {
        let before = mtime(&path);
        let mut board = load(project)?;
        normalize_card_ids(&mut board);
        let Some(result) = change(&mut board)? else {
            return Ok(None);
        };
        if before == mtime(&path) {
            save(project, &board)?;
            return Ok(Some(result));
        }
    }
    bail!(
        "{} kept changing while editing; nothing was written",
        path.display()
    )
}

/// Add a card. `column` defaults to the first column; an unknown column name
/// is an error, not a silently new column.
pub fn add_card(
    project: &Path,
    column: Option<&str>,
    title: &str,
    body: &str,
    who: Option<&str>,
) -> Result<String> {
    let column_name = column.unwrap_or(DEFAULT_COLUMNS[0]).to_string();
    let card = Card {
        id: new_card_id(),
        title: title.trim().to_string(),
        body: body
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
        claimed_by: who.map(str::to_string),
        done: false,
    };
    let stored = card.clone();
    let unknown = format!("no column named “{}”", column_name);
    match edit(project, move |board| {
        let Some(index) = board.column_named(&column_name) else {
            anyhow::bail!("{}", unknown);
        };
        board.columns[index].cards.push(stored.clone());
        Ok(Some(card.title.clone()))
    })? {
        Some(title) => Ok(title),
        None => Err(anyhow::anyhow!(
            "no column named “{}”",
            column.unwrap_or(DEFAULT_COLUMNS[0])
        )),
    }
}

/// Move the first card titled `title` to the end of `to_column`. The claim
/// does not survive the move: a claim means "someone is on this", and a card
/// handed to another column — Review above all — is by definition no longer
/// the worker's. Dragging in the GUI and `card move` behave the same.
pub fn move_card(project: &Path, title: &str, to_column: &str) -> Result<bool> {
    let target = to_column.to_string();
    let title = title.to_string();
    edit(project, move |board| {
        let Some(to) = board.column_named(&target) else {
            anyhow::bail!("no column named “{}”", target);
        };
        let Some((from, at)) = board.find(&title) else {
            return Ok(None);
        };
        if from != to {
            let mut card = board.columns[from].cards.remove(at);
            card.claimed_by = None;
            board.columns[to].cards.push(card);
        }
        Ok(Some(true))
    })?
    .map_or(Ok(false), Ok)
}

/// Claim a card for `who`, or release it when `who` is `None`. Returns whether
/// a card was found.
pub fn claim_card(project: &Path, title: &str, who: Option<&str>) -> Result<bool> {
    let title = title.to_string();
    let who = who.map(str::to_string);
    edit(project, move |board| {
        let Some((c, i)) = board.find(&title) else {
            return Ok(None);
        };
        board.columns[c].cards[i].claimed_by = who.clone();
        Ok(Some(true))
    })?
    .map_or(Ok(false), Ok)
}

/// Mark a card done: checked, and moved to the last column.
pub fn finish_card(project: &Path, title: &str) -> Result<bool> {
    let title = title.to_string();
    edit(project, move |board| {
        let Some((c, i)) = board.find(&title) else {
            return Ok(None);
        };
        board.columns[c].cards[i].done = true;
        if c + 1 < board.columns.len() {
            let card = board.columns[c].cards.remove(i);
            board.columns.last_mut().unwrap().cards.push(card);
        }
        Ok(Some(true))
    })?
    .map_or(Ok(false), Ok)
}

/// Reopen a done card: unchecked, and moved back to the first column. The
/// inverse of [`finish_card`], for ticking a done to-do back open.
pub fn reopen_card(project: &Path, title: &str) -> Result<bool> {
    let title = title.to_string();
    edit(project, move |board| {
        let Some((c, i)) = board.find(&title) else {
            return Ok(None);
        };
        board.columns[c].cards[i].done = false;
        let card = board.columns[c].cards.remove(i);
        board.columns.first_mut().unwrap().cards.push(card);
        Ok(Some(true))
    })?
    .map_or(Ok(false), Ok)
}

/// Replace a card: the GUI's edit dialog. The card is found by its old title;
/// the updated card may carry a new title, notes, claim, or column.
pub fn update_card(
    project: &Path,
    title: &str,
    updated: Card,
    column: Option<&str>,
) -> Result<bool> {
    let title = title.to_string();
    let column = column.map(str::to_string);
    edit(project, move |board| {
        let Some((c, i)) = board.find(&title) else {
            return Ok(None);
        };
        let mut updated = updated.clone();
        updated.id = board.columns[c].cards[i].id.clone();
        if let Some(target) = &column {
            if let Some(to) = board.column_named(target) {
                if to != c {
                    // The old card is dropped: the updated one replaces it in
                    // the new column.
                    board.columns[c].cards.remove(i);
                    board.columns[to].cards.push(updated.clone());
                    return Ok(Some(true));
                }
            }
        }
        board.columns[c].cards[i] = updated.clone();
        Ok(Some(true))
    })?
    .map_or(Ok(false), Ok)
}

/// Delete a card. There is no undo: the file is the record (and, in a
/// repository, git history is the undo).
pub fn remove_card(project: &Path, title: &str) -> Result<bool> {
    let title = title.to_string();
    edit(project, move |board| {
        let Some((c, i)) = board.find(&title) else {
            return Ok(None);
        };
        board.columns[c].cards.remove(i);
        Ok(Some(true))
    })?
    .map_or(Ok(false), Ok)
}

/// The work primitive: claim the first unclaimed card and return it. Columns
/// are scanned in file order, so "next" means "top of the leftmost column that
/// still has unclaimed cards". A `column` name narrows the scan to that one
/// column — how a reviewer asks for review work — and an unknown name is an
/// error, so `--in Review` on a board without it fails loudly instead of
/// reading as "no work".
pub fn next_card(project: &Path, who: &str, column: Option<&str>) -> Result<Option<Card>> {
    let who = who.to_string();
    let column = column.map(str::to_string);
    edit(project, move |board| {
        let columns: &mut [Column] = match &column {
            Some(name) => {
                let Some(at) = board.column_named(name) else {
                    anyhow::bail!("no column named “{}”", name);
                };
                &mut board.columns[at..at + 1]
            }
            None => &mut board.columns[..],
        };
        for column in columns {
            if let Some(card) = column
                .cards
                .iter_mut()
                .find(|card| !card.done && card.claimed_by.is_none() && !card.title.is_empty())
            {
                card.claimed_by = Some(who.clone());
                return Ok(Some(card.clone()));
            }
        }
        Ok(None)
    })
}

/// Does `who` hold a live claim — any card they have claimed that is not yet
/// done? The guard's question: an agent with a claim may edit project files.
pub fn holds_claim(project: &Path, who: &str) -> Result<bool> {
    Ok(load(project)?.columns.iter().any(|column| {
        column
            .cards
            .iter()
            .any(|card| !card.done && card.claimed_by.as_deref() == Some(who))
    }))
}

fn default_header(project_name: &str) -> String {
    format!(
        "# Board — {name}\n\
         \n\
         This file is the project's kanban; radar renders it as a board.\n\
         Edit it directly:\n\
         \n\
         - Claim a card: add your name to the end of its line, like `@you`\n\
         - Move work along: move the card's line under another column\n\
         - Add work: a new `- [ ]` line under any column\n\
         - Notes for a card: indent lines under it\n",
        name = project_name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board_with(text: &str) -> Board {
        parse(text)
    }

    #[test]
    fn a_fresh_board_renders_the_default_columns() {
        let board = Board::default_for("api-server");
        let text = board.render();
        assert!(text.starts_with("# Board — api-server"));
        for column in DEFAULT_COLUMNS {
            assert!(text.contains(&format!("## {column}")));
        }
        // Round-trips: what we write is what we read.
        assert_eq!(parse(&text), board);
    }

    #[test]
    fn parse_reads_columns_cards_claims_and_notes() {
        let board = board_with(
            "# Board\n\n\
             ## Backlog\n\
             - [ ] Add rate limiting\n\
             ## In progress\n\
             - [ ] Fix login redirect @claude\n\
                   the 302 loop happens with a stale cookie\n\
                   check the middleware first\n\
             ## Done\n\
             - [x] Bump deps\n",
        );
        assert_eq!(board.columns.len(), 3);
        assert_eq!(board.columns[0].cards[0].title, "Add rate limiting");
        let claimed = &board.columns[1].cards[0];
        assert_eq!(claimed.claimed_by.as_deref(), Some("claude"));
        assert_eq!(claimed.body.len(), 2);
        assert_eq!(claimed.body[0], "the 302 loop happens with a stale cookie");
        assert!(board.columns[2].cards[0].done);
    }

    #[test]
    fn parse_accepts_a_bare_bullet_as_a_card() {
        let board = board_with("## Backlog\n- just an idea\n");
        assert_eq!(board.columns[0].cards[0].title, "just an idea");
        assert!(!board.columns[0].cards[0].done);
    }

    #[test]
    fn the_header_is_kept_and_render_is_stable() {
        let text = "# T\n\nfree-form\nnotes\n## Column\n- a card\n";
        let board = board_with(text);
        assert_eq!(board.header, "# T\n\nfree-form\nnotes\n");
        // Render canonicalises spacing, then holds still: parsing the render
        // gives the same board, and rendering again gives the same text.
        let rendered = board.render();
        assert_eq!(parse(&rendered), board);
        assert_eq!(parse(&rendered).render(), rendered);
    }

    #[test]
    fn prose_under_a_card_becomes_notes_instead_of_being_lost() {
        let board = board_with("## Backlog\n- a card\nplain note\n");
        assert_eq!(board.columns[0].cards[0].body, vec!["plain note"]);
    }

    #[test]
    fn a_claim_must_be_the_last_word_and_alphanumeric() {
        let (title, claim) = split_claim("fix the @claude");
        assert_eq!(title, "fix the");
        assert_eq!(claim.as_deref(), Some("claude"));

        let (title, claim) = split_claim("@claude fix it");
        assert_eq!(title, "@claude fix it");
        assert_eq!(claim, None);

        assert_eq!(split_claim("fix it @with space").1, None);

        let (title, claim) = split_claim("fix @agent-2");
        assert_eq!(title, "fix");
        assert_eq!(claim.as_deref(), Some("agent-2"));

        // Appending a name over someone's claim replaces it, token and all.
        let (title, claim) = split_claim("fix @claude @codex");
        assert_eq!(title, "fix");
        assert_eq!(claim.as_deref(), Some("codex"));

        // A mention in prose is not the claim.
        let (title, claim) = split_claim("email @bob about specs");
        assert_eq!(title, "email @bob about specs");
        assert_eq!(claim, None);
    }

    #[test]
    fn card_identity_is_invisible_persistent_and_survives_edits_and_moves() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join(FILE_NAME),
            "# Board\n\n## Backlog\n- [ ] Original title @agent\n  a note\n## In progress\n",
        )
        .unwrap();

        ensure_file(project.path()).unwrap();
        let original = load(project.path()).unwrap();
        let id = original.columns[0].cards[0].id.clone();
        let saved = std::fs::read_to_string(project.path().join(FILE_NAME)).unwrap();
        assert!(saved.contains(&format!("<!-- radar:card-id:{id} -->")));
        assert!(saved.contains("- [ ] Original title @agent\n"));
        assert_eq!(
            ensure_file(project.path()).unwrap(),
            project.path().join(FILE_NAME)
        );
        assert_eq!(load(project.path()).unwrap().columns[0].cards[0].id, id);

        let mut renamed = original.columns[0].cards[0].clone();
        renamed.title = "Renamed title".to_string();
        update_card(
            project.path(),
            "Original title",
            renamed,
            Some("In progress"),
        )
        .unwrap();
        let moved = load(project.path()).unwrap();
        assert_eq!(moved.find_id(&id), Some((1, 0)));
        assert_eq!(moved.columns[1].cards[0].title, "Renamed title");
    }

    #[test]
    fn card_id_migration_preserves_header_and_repairs_copied_ids() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join(FILE_NAME),
            "# Board\n- [ ] example in header\n\n## Backlog\n- [ ] First\n      <!-- radar:card-id:copied -->\n- [ ] Second\n      <!-- radar:card-id:copied -->\n",
        )
        .unwrap();

        ensure_file(project.path()).unwrap();
        let saved = std::fs::read_to_string(project.path().join(FILE_NAME)).unwrap();
        assert!(saved.starts_with("# Board\n- [ ] example in header\n\n"));
        assert_eq!(saved.matches("<!-- radar:card-id:").count(), 2);
        let board = load(project.path()).unwrap();
        let ids: HashSet<_> = board.columns[0]
            .cards
            .iter()
            .map(|card| card.id.as_str())
            .collect();
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn find_matches_the_first_card_with_that_title() {
        let board = board_with("## A\n- one\n## B\n- one\n- two\n");
        let (c, _) = board.find("one").unwrap();
        assert_eq!(board.columns[c].name, "A");
        assert!(board.find("missing").is_none());
    }

    #[test]
    fn moving_between_columns_reorders() {
        let board = board_with("## A\n- x\n- y\n## B\n\n");
        let mut moved = board.clone();
        let (from, at) = moved.find("x").unwrap();
        let card = moved.columns[from].cards.remove(at);
        moved.columns[1].cards.push(card);
        assert_eq!(moved.columns[1].cards[0].title, "x");
        assert_eq!(moved.columns[0].cards.len(), 1);
    }

    #[test]
    fn render_canonicalises_claims_and_notes() {
        let board = board_with("## A\n- [ ] fix @claude\nnote here\n");
        let text = board.render();
        assert!(text.contains("- [ ] fix @claude\n      <!-- radar:card-id:"));
        assert!(text.contains("      note here\n"));
        assert_eq!(parse(&text), board);
    }

    #[test]
    fn column_lookup_ignores_case() {
        let board = board_with("## In Progress\n- x\n");
        assert_eq!(board.column_named("in progress"), Some(0));
        assert_eq!(board.column_named("Done"), None);
    }

    #[test]
    fn everything_before_the_first_heading_is_header() {
        // The how-to note contains card-shaped lines; they must stay header.
        let board = board_with("- not a card, an example\n## Later\n- another\n");
        assert_eq!(board.columns.len(), 1);
        assert_eq!(board.columns[0].cards.len(), 1);
        assert!(board.header.contains("not a card, an example"));
    }

    // --- file operations, against a real directory ---

    fn project() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    #[test]
    fn board_policy_defaults_enabled_and_is_stored_only_in_global_database() {
        let (_dir, project) = project();
        let db = crate::db::Db::open_in_memory().unwrap();
        let stored = db.add_project(&project).unwrap();

        assert!(enabled(&db, &project).unwrap());
        set_enabled(&db, &project, false).unwrap();
        assert!(!enabled(&db, &project).unwrap());
        assert!(ensure_enabled_file(&db, &project).is_err());
        assert!(!file_path(&project).exists());
        assert!(!project.join(".radar.toml").exists());
        assert!(!db.project_settings(stored.id).unwrap().board_enabled);
    }

    #[test]
    fn project_file_config_does_not_override_global_board_policy() {
        let (_dir, project) = project();
        let db = crate::db::Db::open_in_memory().unwrap();
        db.add_project(&project).unwrap();
        std::fs::write(project.join(".radar.toml"), "[board]\nenabled = false\n").unwrap();

        assert!(enabled(&db, &project).unwrap());
        assert!(ensure_enabled_file(&db, &project).unwrap().exists());
    }

    #[test]
    fn ensure_file_creates_once_and_migrates_existing_cards() {
        let (_dir, project) = project();
        let path = ensure_file(&project).unwrap();
        assert!(path.exists());
        std::fs::write(&path, "## Custom\n- [x] kept @me\n").unwrap();
        ensure_file(&project).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("## Custom\n- [x] kept @me\n"));
        assert_eq!(text.matches("<!-- radar:card-id:").count(), 1);
        let board = load(&project).unwrap();
        assert_eq!(board.columns[0].cards[0].title, "kept");
        assert_eq!(board.columns[0].cards[0].claimed_by.as_deref(), Some("me"));
    }

    #[test]
    fn add_move_and_finish_cards_through_the_file() {
        let (_dir, project) = project();
        ensure_file(&project).unwrap();
        add_card(&project, None, "first task", "with a note", Some("claude")).unwrap();
        add_card(&project, Some("review"), "second task", "", None).unwrap();

        let board = load(&project).unwrap();
        assert_eq!(board.columns[0].cards[0].title, "first task");
        assert_eq!(board.columns[0].cards[0].body, vec!["with a note"]);
        assert_eq!(board.columns[2].cards[0].title, "second task");

        assert!(move_card(&project, "first task", "In progress").unwrap());
        let board = load(&project).unwrap();
        assert_eq!(board.columns[1].cards[0].title, "first task");
        // Notes travel with the card; the claim does not — moving a card is
        // handing it over.
        assert_eq!(board.columns[1].cards[0].body, vec!["with a note"]);
        assert_eq!(board.columns[1].cards[0].claimed_by, None);
        assert!(!move_card(&project, "no such card", "Done").unwrap());

        assert!(finish_card(&project, "first task").unwrap());
        let board = load(&project).unwrap();
        let (c, i) = board.find("first task").unwrap();
        assert_eq!(c, board.columns.len() - 1);
        assert!(board.columns[c].cards[i].done);
    }

    #[test]
    fn add_card_rejects_an_unknown_column() {
        let (_dir, project) = project();
        ensure_file(&project).unwrap();
        let err = add_card(&project, Some("Nope"), "x", "", None).unwrap_err();
        assert!(err.to_string().contains("Nope"));
    }

    #[test]
    fn claim_and_release_round_trip() {
        let (_dir, project) = project();
        ensure_file(&project).unwrap();
        add_card(&project, None, "task", "", None).unwrap();
        assert!(claim_card(&project, "task", Some("codex")).unwrap());
        let board = load(&project).unwrap();
        assert_eq!(
            board.columns[0].cards[0].claimed_by.as_deref(),
            Some("codex")
        );
        assert!(claim_card(&project, "task", None).unwrap());
        assert_eq!(load(&project).unwrap().columns[0].cards[0].claimed_by, None);
        assert!(!claim_card(&project, "ghost", Some("x")).unwrap());
    }

    #[test]
    fn next_card_claims_the_first_unclaimed_and_only_once() {
        let (_dir, project) = project();
        ensure_file(&project).unwrap();
        add_card(&project, None, "one", "", None).unwrap();
        add_card(&project, None, "two", "", None).unwrap();

        let first = next_card(&project, "claude", None).unwrap().unwrap();
        assert_eq!(first.title, "one");
        // The claimed card is not handed out twice.
        let second = next_card(&project, "codex", None).unwrap().unwrap();
        assert_eq!(second.title, "two");
        assert!(next_card(&project, "droid", None).unwrap().is_none());

        let board = load(&project).unwrap();
        assert_eq!(
            board.columns[0].cards[0].claimed_by.as_deref(),
            Some("claude")
        );
        assert_eq!(
            board.columns[0].cards[1].claimed_by.as_deref(),
            Some("codex")
        );
    }

    #[test]
    fn next_card_skips_done_cards() {
        let (_dir, project) = project();
        ensure_file(&project).unwrap();
        add_card(&project, None, "only", "", None).unwrap();
        finish_card(&project, "only").unwrap();
        assert!(next_card(&project, "claude", None).unwrap().is_none());
    }

    #[test]
    fn next_card_in_a_column_takes_only_from_there() {
        let (_dir, project) = project();
        ensure_file(&project).unwrap();
        add_card(&project, None, "backlog work", "", None).unwrap();
        add_card(&project, Some("Review"), "review work", "", None).unwrap();

        // The reviewer is handed the Review card, not the first in the file.
        let card = next_card(&project, "codex", Some("review"))
            .unwrap()
            .unwrap();
        assert_eq!(card.title, "review work");
        // And the backlog card stays unclaimed.
        let board = load(&project).unwrap();
        assert_eq!(board.columns[0].cards[0].claimed_by, None);

        // When the column runs dry, that is all it is — no work there.
        assert!(next_card(&project, "droid", Some("review"))
            .unwrap()
            .is_none());
        // A column that does not exist is an error, not "no work".
        let err = next_card(&project, "codex", Some("nope")).unwrap_err();
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn holds_claim_sees_a_live_claim_only() {
        let (_dir, project) = project();
        ensure_file(&project).unwrap();
        add_card(&project, None, "task", "", None).unwrap();
        assert!(!holds_claim(&project, "claude").unwrap());
        claim_card(&project, "task", Some("claude")).unwrap();
        assert!(holds_claim(&project, "claude").unwrap());
        assert!(!holds_claim(&project, "codex").unwrap());
        // A done card is a handed-over card, not a held one.
        finish_card(&project, "task").unwrap();
        assert!(!holds_claim(&project, "claude").unwrap());
    }

    #[test]
    fn a_board_that_does_not_exist_holds_no_claim() {
        let (_dir, fresh) = project();
        assert!(!holds_claim(&fresh, "claude").unwrap());
    }

    #[test]
    fn update_card_edits_moves_and_renames() {
        let (_dir, project) = project();
        ensure_file(&project).unwrap();
        add_card(&project, None, "old title", "a note", None).unwrap();

        // Rename and claim in place.
        let mut card = Card::new("new title");
        card.claimed_by = Some("claude".into());
        card.body = vec!["a note".into()];
        assert!(update_card(&project, "old title", card.clone(), None).unwrap());
        let board = load(&project).unwrap();
        assert_eq!(board.columns[0].cards[0].title, "new title");
        assert_eq!(
            board.columns[0].cards[0].claimed_by.as_deref(),
            Some("claude")
        );

        // Move by way of an update.
        assert!(update_card(&project, "new title", card, Some("Review")).unwrap());
        let board = load(&project).unwrap();
        assert_eq!(board.columns[2].cards[0].title, "new title");
        assert!(!update_card(&project, "ghost", Card::new("x"), None).unwrap());
    }

    #[test]
    fn remove_card_deletes_only_that_card() {
        let (_dir, project) = project();
        ensure_file(&project).unwrap();
        add_card(&project, None, "keep", "", None).unwrap();
        add_card(&project, None, "drop", "", None).unwrap();
        assert!(remove_card(&project, "drop").unwrap());
        let board = load(&project).unwrap();
        assert_eq!(board.columns[0].cards.len(), 1);
        assert_eq!(board.columns[0].cards[0].title, "keep");
        assert!(!remove_card(&project, "drop").unwrap());
    }

    #[test]
    fn ops_work_before_the_file_exists() {
        let (_dir, project) = project();
        add_card(&project, None, "early", "", None).unwrap();
        assert!(file_path(&project).exists());
        assert_eq!(load(&project).unwrap().columns[0].cards[0].title, "early");
    }
    #[test]
    fn board_changes_use_column_names_and_named_done_semantics() {
        let before = board_with(
            "## Backlog\n\
             - [ ] Ship fix\n\
                   <!-- radar:card-id:stable-card -->\n\
             ## In progress\n\
             ## Done\n",
        );
        let reordered = board_with(
            "## Done\n\
             ## Backlog\n\
             - [ ] Ship fix\n\
                   <!-- radar:card-id:stable-card -->\n\
             ## In progress\n",
        );
        assert!(changes(Some(&before), Some(&reordered)).is_empty());

        let checked = board_with(
            "## Backlog\n\
             - [x] Ship fix\n\
                   <!-- radar:card-id:stable-card -->\n\
             ## In progress\n\
             ## Done\n",
        );
        let checkbox_change = changes(Some(&before), Some(&checked));
        assert_eq!(checkbox_change.len(), 1);
        assert_eq!(checkbox_change[0].action, "updated");

        let moved_to_done = board_with(
            "## Backlog\n\
             ## In progress\n\
             ## Done\n\
             - [ ] Ship fix\n\
                   <!-- radar:card-id:stable-card -->\n",
        );
        let completion = changes(Some(&before), Some(&moved_to_done));
        assert_eq!(completion.len(), 1);
        assert_eq!(completion[0].action, "completed");
        assert_eq!(completion[0].column.as_deref(), Some("Done"));
        assert_eq!(completion[0].from_column.as_deref(), Some("Backlog"));
    }

    #[test]
    fn a_card_without_an_id_comment_keeps_a_stable_derived_id() {
        // Hand-authored cards (the skill invites editing BOARD.md directly)
        // have no id comment yet. Their id must not change between parses, or
        // a thread keyed on it would orphan.
        let text = "# Board\n\n## Backlog\n- [ ] Rate limiting\n";
        let first = parse(text);
        let second = parse(text);
        let id = first.columns[0].cards[0].id.clone();
        assert_eq!(id, second.columns[0].cards[0].id);
        assert!(id.starts_with("card-auto-"));

        // The next write persists that id; it is read back unchanged.
        let rendered = first.render();
        assert!(rendered.contains("radar:card-id:"));
        let reread = parse(&rendered);
        assert_eq!(reread.columns[0].cards[0].id, id);
    }

    #[test]
    fn an_indented_dash_line_is_a_note_not_a_card() {
        let board = board_with("## Backlog\n- [ ] Card\n      - a list item\n      - another\n");
        assert_eq!(board.columns[0].cards.len(), 1);
        assert_eq!(
            board.columns[0].cards[0].body,
            vec!["- a list item", "- another"]
        );
    }
}
