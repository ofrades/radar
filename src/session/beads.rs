//! Beads (`bd`) as the board's backing store.
//!
//! Radar's board has always been a bespoke store. This module reads the same
//! board shape out of [Beads](https://github.com/gastownhall/beads), the issue
//! tracker agents already know: a `.beads` directory the project shares with
//! its agents, first-class claim leases, dependencies and comments.
//!
//! The mapping, one-to-one:
//!
//! | radar | Beads |
//! | --- | --- |
//! | card | issue |
//! | card title | issue title |
//! | card body | issue description |
//! | lane | issue status |
//! | lane kind | the status category: `active`→todo, `wip`→in_progress, `done`→done, `frozen`→custom |
//! | claim | assignee |
//! | done | a done-kind status (built-in `closed`) |
//!
//! Everything here is read-only: the daemon still owns mutations, and this is
//! the read half the daemon will adopt. `bd` is found on `PATH`, then in
//! `~/.local/bin`, and can be pinned with `RADAR_BD`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::board_store::{BoardState, Lane, StoredCard};

/// Environment variable that pins the `bd` binary.
const BD_ENV: &str = "RADAR_BD";

/// The four lanes every board shows, even when a project has no cards yet:
/// `(beads status, lane name, lane kind)`.
const CANONICAL_LANES: [(&str, &str, &str); 4] = [
    ("open", "Todo", "todo"),
    ("in_progress", "In progress", "in_progress"),
    ("review", "Review", "review"),
    ("closed", "Done", "done"),
];

/// A `bd` binary and the JSON contract it speaks.
#[derive(Debug, Clone)]
pub struct Beads {
    executable: PathBuf,
}

/// One valid status, from `bd statuses --json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusSpec {
    pub name: String,
    /// `active` | `wip` | `done` | `frozen`.
    pub category: String,
}

/// The subset of a Beads issue the board needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: String,
    pub assignee: Option<String>,
    pub priority: Option<i64>,
    pub created_at_millis: i64,
    pub updated_at_millis: i64,
}

impl Beads {
    /// Find `bd`: `RADAR_BD`, then `PATH`, then `~/.local/bin/bd`.
    pub fn locate() -> Result<Beads> {
        if let Some(path) = std::env::var_os(BD_ENV) {
            let path = PathBuf::from(path);
            if path.is_file() {
                return Ok(Beads::at(path));
            }
            bail!("{BD_ENV} points at {}, which is not a file", path.display());
        }
        if let Some(path) = find_on_path("bd") {
            return Ok(Beads::at(path));
        }
        if let Some(home) = dirs::home_dir() {
            let candidate = home.join(".local/bin/bd");
            if candidate.is_file() {
                return Ok(Beads::at(candidate));
            }
        }
        bail!("bd (Beads) was not found on PATH or in ~/.local/bin; install it or set {BD_ENV}")
    }

    /// Use a specific `bd` binary.
    pub fn at(executable: impl Into<PathBuf>) -> Beads {
        Beads {
            executable: executable.into(),
        }
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Every valid status in `dir`, built-in and configured custom.
    pub fn statuses(&self, dir: &Path) -> Result<Vec<StatusSpec>> {
        let json = self.run(dir, &["statuses", "--json"])?;
        let raw: RawStatuses = serde_json::from_str(&json)
            .with_context(|| format!("parsing `bd statuses --json` in {}", dir.display()))?;
        let mut statuses = Vec::new();
        for status in raw.built_in_statuses.into_iter().chain(raw.custom_statuses) {
            statuses.push(StatusSpec {
                name: status.name,
                category: status.category,
            });
        }
        Ok(statuses)
    }

    /// Every issue in `dir`, closed ones included.
    pub fn list(&self, dir: &Path) -> Result<Vec<Issue>> {
        let json = self.run(dir, &["list", "--all", "--json"])?;
        let envelope: ListEnvelope = serde_json::from_str(&json)
            .with_context(|| format!("parsing `bd list --all --json` in {}", dir.display()))?;
        Ok(envelope.data.into_iter().map(Issue::from_raw).collect())
    }

    /// One issue, by id.
    pub fn show(&self, dir: &Path, id: &str) -> Result<Issue> {
        let json = self.run(dir, &["show", id, "--json"])?;
        let issues: Vec<RawIssue> = serde_json::from_str(&json)
            .with_context(|| format!("parsing `bd show {id} --json` in {}", dir.display()))?;
        issues
            .into_iter()
            .next()
            .map(Issue::from_raw)
            .with_context(|| format!("bd show {id} returned no issue"))
    }

    /// Run `bd` in `dir` and return stdout. `BD_JSON_ENVELOPE=1` makes list
    /// output a `{ "data": [...] }` object rather than a bare array.
    pub(crate) fn run(&self, dir: &Path, args: &[&str]) -> Result<String> {
        let output = Command::new(&self.executable)
            .args(args)
            .current_dir(dir)
            .env("BD_JSON_ENVELOPE", "1")
            .output()
            .with_context(|| format!("running {} {:?}", self.executable.display(), args))?;
        if !output.status.success() {
            bail!(
                "bd {:?} in {} failed: {}",
                args,
                dir.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

impl Issue {
    fn from_raw(raw: RawIssue) -> Issue {
        Issue {
            id: raw.id,
            title: raw.title,
            description: raw.description.unwrap_or_default(),
            status: raw.status,
            assignee: raw.assignee.filter(|value| !value.trim().is_empty()),
            priority: raw.priority,
            created_at_millis: raw
                .created_at
                .as_deref()
                .and_then(parse_rfc3339_millis)
                .unwrap_or(0),
            updated_at_millis: raw
                .updated_at
                .as_deref()
                .and_then(parse_rfc3339_millis)
                .unwrap_or(0),
        }
    }
}

/// Build a [`BoardState`] from Beads issues and the status list.
///
/// The canonical four lanes are always present so an empty board still looks
/// like radar's; any other status that has cards joins them, ordered by its
/// place in the workflow. Cards keep their lane's order by priority, then
/// creation.
pub fn board_state(project_id: i64, statuses: &[StatusSpec], issues: &[Issue]) -> BoardState {
    let category_of = |status: &str| {
        statuses
            .iter()
            .find(|spec| spec.name == status)
            .map(|spec| spec.category.as_str())
    };

    // `(status, name, kind)`, canonical lanes first, then any status in use.
    let mut specs: Vec<(&str, String, String)> = CANONICAL_LANES
        .iter()
        .map(|(status, name, kind)| (*status, (*name).to_string(), (*kind).to_string()))
        .collect();
    for issue in issues {
        let status = issue.status.as_str();
        if specs.iter().any(|(known, _, _)| *known == status) {
            continue;
        }
        specs.push((
            status,
            display_name(status),
            lane_kind(category_of(status).unwrap_or_default()).to_string(),
        ));
    }
    specs.sort_by_key(|(status, _, _)| status_rank(status));

    let lanes: Vec<Lane> = specs
        .iter()
        .enumerate()
        .map(|(index, (_, name, kind))| Lane {
            id: index as i64 + 1,
            name: name.clone(),
            kind: kind.clone(),
            position: index as i64,
        })
        .collect();

    let mut cards = Vec::new();
    for (index, (status, _, _)) in specs.iter().enumerate() {
        let lane = &lanes[index];
        let mut in_lane: Vec<&Issue> = issues
            .iter()
            .filter(|issue| issue.status == *status)
            .collect();
        in_lane.sort_by(|a, b| {
            a.priority
                .unwrap_or(i64::MAX)
                .cmp(&b.priority.unwrap_or(i64::MAX))
                .then(a.created_at_millis.cmp(&b.created_at_millis))
                .then(a.id.cmp(&b.id))
        });
        for (position, issue) in in_lane.into_iter().enumerate() {
            cards.push(StoredCard {
                id: issue.id.clone(),
                project_id,
                lane_id: lane.id,
                lane: lane.name.clone(),
                done: lane.kind == "done",
                position: position as i64,
                title: issue.title.clone(),
                body: issue.description.clone(),
                claim: issue.assignee.clone(),
                // Beads carries a real revision on `bd show`; for the board
                // read we use the update time, which is monotonic per edit.
                revision: issue.updated_at_millis.max(1) as u64,
                created_at_millis: issue.created_at_millis,
                updated_at_millis: issue.updated_at_millis,
            });
        }
    }

    BoardState {
        project_id,
        lanes,
        cards,
    }
}

/// A status category as a lane kind. `review` is radar's own review lane and
/// keeps its kind even though Beads files it as `wip`.
fn lane_kind(category: &str) -> &'static str {
    match category {
        "active" => "todo",
        "wip" => "in_progress",
        "done" => "done",
        _ => "custom",
    }
}

/// The Beads status a radar lane name maps to. The canonical four lanes keep
/// their names; any other lane's name is its status, lowercased.
pub fn status_for_lane(lane: &str) -> String {
    match lane {
        "Todo" => "open".to_string(),
        "In progress" => "in_progress".to_string(),
        "Review" => "review".to_string(),
        "Done" => "closed".to_string(),
        other => other.to_lowercase().replace(' ', "_"),
    }
}

/// Where a status sits in the workflow, so lanes read left to right.
fn status_rank(status: &str) -> i32 {
    match status {
        "backlog" => 0,
        "open" => 1,
        "in_progress" => 2,
        "test" => 3,
        "review" => 4,
        "blocked" => 5,
        "deferred" => 6,
        "pinned" => 7,
        "hooked" => 8,
        "closed" => 20,
        _ => 10,
    }
}

/// `in_progress` → `In progress`.
fn display_name(status: &str) -> String {
    status
        .split('_')
        .map(|word| {
            let mut characters = word.chars();
            match characters.next() {
                Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Parse an RFC 3339 timestamp (`2026-09-30T14:37:28.079112337Z`) to epoch
/// milliseconds. Beads emits UTC, but offsets are handled too.
fn parse_rfc3339_millis(text: &str) -> Option<i64> {
    if text.len() < 19 {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    let month: i64 = text.get(5..7)?.parse().ok()?;
    let day: i64 = text.get(8..10)?.parse().ok()?;
    let hour: i64 = text.get(11..13)?.parse().ok()?;
    let minute: i64 = text.get(14..16)?.parse().ok()?;
    let second: i64 = text.get(17..19)?.parse().ok()?;

    let rest = &text[19..];
    let (fraction, zone) = match rest.strip_prefix('.') {
        Some(rest) => {
            let end = rest
                .find(|character: char| !character.is_ascii_digit())
                .unwrap_or(rest.len());
            (&rest[..end], &rest[end..])
        }
        None => ("", rest),
    };
    let mut milliseconds: i64 = 0;
    if !fraction.is_empty() {
        let mut digits: String = fraction.chars().take(3).collect();
        while digits.len() < 3 {
            digits.push('0');
        }
        milliseconds = digits.parse().ok()?;
    }
    let offset_seconds = parse_zone_offset(zone)?;

    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
    Some((seconds - offset_seconds) * 1_000 + milliseconds)
}

/// `Z`, empty, or `±HH:MM` as seconds east of UTC.
fn parse_zone_offset(zone: &str) -> Option<i64> {
    let zone = zone.trim();
    if zone.is_empty() || zone == "Z" || zone == "z" {
        return Some(0);
    }
    let (sign, rest) = match zone.as_bytes().first()? {
        b'+' => (1, &zone[1..]),
        b'-' => (-1, &zone[1..]),
        _ => return None,
    };
    let (hours, minutes) = rest.split_once(':')?;
    let hours: i64 = hours.parse().ok()?;
    let minutes: i64 = minutes.parse().ok()?;
    Some(sign * (hours * 3_600 + minutes * 60))
}

/// Days since 1970-01-01 (Howard Hinnant's civil-date algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[derive(Debug, Deserialize)]
struct RawStatuses {
    #[serde(default)]
    built_in_statuses: Vec<RawStatus>,
    #[serde(default)]
    custom_statuses: Vec<RawStatus>,
}

#[derive(Debug, Deserialize)]
struct RawStatus {
    name: String,
    category: String,
}

#[derive(Debug, Deserialize)]
struct ListEnvelope {
    #[serde(default)]
    data: Vec<RawIssue>,
}

#[derive(Debug, Deserialize)]
struct RawIssue {
    id: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    status: String,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    priority: Option<i64>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(name: &str, category: &str) -> StatusSpec {
        StatusSpec {
            name: name.to_string(),
            category: category.to_string(),
        }
    }

    fn issue(id: &str, title: &str, status: &str) -> Issue {
        Issue {
            id: id.to_string(),
            title: title.to_string(),
            description: String::new(),
            status: status.to_string(),
            assignee: None,
            priority: None,
            created_at_millis: 0,
            updated_at_millis: 0,
        }
    }

    fn built_in_statuses() -> Vec<StatusSpec> {
        vec![
            status("open", "active"),
            status("in_progress", "wip"),
            status("blocked", "wip"),
            status("deferred", "frozen"),
            status("closed", "done"),
            status("pinned", "frozen"),
            status("hooked", "wip"),
            status("review", "wip"),
        ]
    }

    #[test]
    fn lane_kind_follows_the_category() {
        assert_eq!(lane_kind("active"), "todo");
        assert_eq!(lane_kind("wip"), "in_progress");
        assert_eq!(lane_kind("done"), "done");
        assert_eq!(lane_kind("frozen"), "custom");
    }

    #[test]
    fn an_empty_board_still_has_the_canonical_four_lanes() {
        let state = board_state(7, &built_in_statuses(), &[]);
        let names: Vec<&str> = state.lanes.iter().map(|lane| lane.name.as_str()).collect();
        assert_eq!(names, vec!["Todo", "In progress", "Review", "Done"]);
        assert!(state.cards.is_empty());
    }

    #[test]
    fn issues_land_in_their_lane_with_the_right_kind_and_claim() {
        let statuses = built_in_statuses();
        let mut open = issue("beads-1", "Fix login", "open");
        open.description = "the 302 loop".to_string();
        open.assignee = Some("opencode-1".to_string());
        let closed = issue("beads-2", "Ship it", "closed");
        let state = board_state(3, &statuses, &[open, closed]);

        let todo = state.cards.iter().find(|card| card.lane == "Todo").unwrap();
        assert_eq!(todo.title, "Fix login");
        assert_eq!(todo.body, "the 302 loop");
        assert_eq!(todo.claim.as_deref(), Some("opencode-1"));
        assert!(!todo.done);

        let done = state.cards.iter().find(|card| card.lane == "Done").unwrap();
        assert!(done.done);
        assert_eq!(done.title, "Ship it");
    }

    #[test]
    fn review_keeps_its_own_kind_despite_being_wip() {
        let statuses = built_in_statuses();
        let state = board_state(1, &statuses, &[issue("beads-1", "Check", "review")]);
        let review = state
            .lanes
            .iter()
            .find(|lane| lane.name == "Review")
            .unwrap();
        assert_eq!(review.kind, "review");
        assert_eq!(state.cards[0].lane, "Review");
        assert!(!state.cards[0].done);
    }

    #[test]
    fn an_extra_status_with_cards_becomes_a_lane_in_workflow_order() {
        let mut statuses = built_in_statuses();
        statuses.push(status("backlog", "active"));
        let state = board_state(1, &statuses, &[issue("beads-1", "Later", "backlog")]);
        let names: Vec<&str> = state.lanes.iter().map(|lane| lane.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Backlog", "Todo", "In progress", "Review", "Done"]
        );
        assert_eq!(state.lanes[0].kind, "todo");
        assert_eq!(state.cards[0].lane, "Backlog");
    }

    #[test]
    fn cards_in_a_lane_order_by_priority_then_creation() {
        let statuses = built_in_statuses();
        let mut low = issue("beads-2", "Low", "open");
        low.priority = Some(3);
        low.created_at_millis = 10;
        let mut high = issue("beads-1", "High", "open");
        high.priority = Some(1);
        high.created_at_millis = 20;
        let state = board_state(1, &statuses, &[low, high]);
        let titles: Vec<&str> = state.cards.iter().map(|card| card.title.as_str()).collect();
        assert_eq!(titles, vec!["High", "Low"]);
        assert_eq!(state.cards[0].position, 0);
        assert_eq!(state.cards[1].position, 1);
    }

    #[test]
    fn parses_rfc3339_timestamps() {
        assert_eq!(parse_rfc3339_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_millis("2026-09-30T14:37:28.079112337Z"),
            Some(1_790_779_048_079)
        );
        // An offset is applied: 12:00+02:00 is 10:00 UTC.
        assert_eq!(
            parse_rfc3339_millis("2026-09-30T12:00:00+02:00"),
            parse_rfc3339_millis("2026-09-30T10:00:00Z")
        );
        assert_eq!(parse_rfc3339_millis("nope"), None);
    }

    /// A real `bd`, when one is installed. Skipped otherwise, so the suite
    /// stays green on a machine without Beads.
    fn beads_for_tests() -> Option<Beads> {
        Beads::locate().ok()
    }

    #[test]
    fn a_real_bd_round_trips_into_a_board_state() {
        let Some(beads) = beads_for_tests() else {
            eprintln!("skipping: bd is not installed");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status();
        assert!(git.map(|status| status.success()).unwrap_or(false));
        beads
            .run(root, &["init", "--quiet"])
            .expect("bd init in a scratch repo");

        let created = beads
            .run(root, &["create", "First card", "--json"])
            .unwrap();
        let created: serde_json::Value = serde_json::from_str(&created).unwrap();
        // With the envelope on, `bd create` nests the new issue under `data`.
        let id = created["data"]["id"]
            .as_str()
            .or_else(|| created["id"].as_str())
            .expect("bd create returns the new id")
            .to_string();
        beads
            .run(root, &["update", &id, "--claim"])
            .expect("claim the first card");
        beads
            .run(root, &["create", "Second card", "--json"])
            .unwrap();

        let statuses = beads.statuses(root).unwrap();
        let issues = beads.list(root).unwrap();
        let state = board_state(1, &statuses, &issues);

        let names: Vec<&str> = state.lanes.iter().map(|lane| lane.name.as_str()).collect();
        assert!(names.contains(&"Todo"), "{names:?}");
        assert!(names.contains(&"In progress"), "{names:?}");
        let claimed = state
            .cards
            .iter()
            .find(|card| card.title == "First card")
            .unwrap();
        assert_eq!(claimed.lane, "In progress");
        assert!(claimed.claim.is_some(), "the claim is the assignee");
    }
}
