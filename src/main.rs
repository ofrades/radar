//! radar — native workspace manager.
//!
//! Run without arguments to open the app. Every subcommand exists so the same
//! state can be inspected, scripted and tested without a GUI.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use radar::board;
use radar::config::Paths;
use radar::db::{Db, Preferences, Slot, Tab};
use radar::programs::{self, agents, Kind, LaunchOptions};
use radar::{discover, git};

#[derive(Parser, Debug)]
#[command(
    name = "radar",
    version,
    about = "Native workspace manager: projects in a sidebar, tools in tabs",
    long_about = None
)]
struct Cli {
    /// Override the state directory (default: ~/.local/share/radar, or
    /// $RADAR_HOME)
    #[arg(long, global = true)]
    home: Option<PathBuf>,

    /// Machine readable output
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List projects with their git status
    List,
    /// Add one or more project directories
    Add {
        /// Directories to add. `~` is expanded; `.` works too.
        paths: Vec<PathBuf>,
        /// Add every repository found under this directory (default: ./)
        #[arg(long, conflicts_with = "paths")]
        scan: Option<Option<PathBuf>>,
        /// How deep to scan
        #[arg(long, default_value_t = 2)]
        depth: usize,
        /// Do not select the first added project
        #[arg(long)]
        no_select: bool,
    },
    /// Remove a project from the sidebar (never from disk)
    Remove { path: PathBuf },
    /// Mark a project as opened and show what its tabs resolve to
    Open { path: PathBuf },
    /// Pin or unpin a project
    Pin { path: PathBuf, #[arg(long)] off: bool },
    /// Move a project up or down in the sidebar
    Move {
        path: PathBuf,
        /// Negative moves up
        delta: i64,
    },
    /// Rename a project in the sidebar
    Rename { path: PathBuf, name: String },
    /// Drop projects whose directory has gone away
    Prune,
    /// Show or change the preferred program for a slot
    Prefs {
        /// One of: editor, agent, diff, shell
        slot: Option<String>,
        /// Program id to store; omit to just show
        program: Option<String>,
    },
    /// List agents, what omarchy picked, and what is installed
    Agents,
    /// List every program radar knows about
    Programs {
        /// Filter by kind: editor, agent, diff, shell, tool
        kind: Option<String>,
    },
    /// Fuzzy-find directories that could be added
    Find {
        query: Vec<String>,
        /// Where to search
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long, default_value_t = 2)]
        depth: usize,
        #[arg(long, default_value_t = 40)]
        limit: usize,
    },
    /// Check the environment radar needs
    Doctor,
    /// Show a project's board (BOARD.md), creating it if needed
    Board {
        /// Project directory (default: the current directory)
        path: Option<PathBuf>,
    },
    /// Work with cards on a project's board
    Card {
        #[command(subcommand)]
        action: CardAction,
    },
    /// Wiring for agent-harness hooks (the board as a requirement)
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },
    /// Open the native app (needs a build with --features gui)
    Gui,
}

#[derive(Subcommand, Debug)]
enum CardAction {
    /// Add a card (default column: Backlog)
    Add {
        /// Project directory (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
        /// The card's title
        title: String,
        /// Which column to put it in
        #[arg(long)]
        column: Option<String>,
        /// Notes for the card
        #[arg(long)]
        body: Option<String>,
        /// Claim it for this name right away
        #[arg(long)]
        by: Option<String>,
    },
    /// Release a card: drop its claim, nobody is on it
    Release {
        /// Project directory (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
        /// The card's title
        title: String,
    },
    /// Claim a card for a name
    Claim {
        /// Project directory (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
        /// The card's title
        title: String,
        /// Who is claiming it
        #[arg(long)]
        by: String,
    },
    /// Move a card to a column
    Move {
        /// Project directory (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
        /// The card's title
        title: String,
        /// Destination column
        #[arg(long)]
        to: String,
    },
    /// Mark a card done: checked, and moved to the last column
    Done {
        /// Project directory (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
        /// The card's title
        title: String,
    },
    /// Claim the first unclaimed card and print it — how an agent asks for work
    Next {
        /// Project directory (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
        /// Who is asking
        #[arg(long)]
        by: String,
        /// Only look in this column — how a reviewer picks up review work
        #[arg(long = "in")]
        in_column: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum HookAction {
    /// Judge one edit the way an agent harness's pre-edit hook would: deny
    /// (exit 2) unless the project's BOARD.md shows a live claim by
    /// $RADAR_AGENT. The tool call's JSON is read from stdin when piped, so
    /// the same command serves a hook and a human checking by hand.
    Guard {
        /// The file the agent wants to edit (default: from the hook's stdin
        /// JSON, `tool_input.file_path`)
        #[arg(long)]
        file: Option<PathBuf>,
        /// The project directory (default: the current directory)
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = match &cli.home {
        Some(root) => Paths::with_root(root),
        None => Paths::resolve(),
    };
    let db = Db::open(&paths)?;

    match cli.command {
        // No subcommand: this is the app.
        None | Some(Command::Gui) => run_gui(paths, db),
        Some(Command::List) => list(&db, cli.json),
        Some(Command::Add {
            paths: to_add,
            scan,
            depth,
            no_select,
        }) => add(&db, to_add, scan, depth, no_select, cli.json),
        Some(Command::Remove { path }) => {
            let project = db
                .project_by_path(&path)?
                .with_context(|| format!("{} is not in the sidebar", path.display()))?;
            db.remove_project(project.id)?;
            println!("removed {} (files untouched)", project.display_path());
            Ok(())
        }
        Some(Command::Open { path }) => open(&db, &path, cli.json),
        Some(Command::Pin { path, off }) => {
            let project = require_project(&db, &path)?;
            db.set_pinned(project.id, !off)?;
            println!(
                "{} {}",
                if off { "unpinned" } else { "pinned" },
                project.display_path()
            );
            Ok(())
        }
        Some(Command::Move { path, delta }) => {
            let project = require_project(&db, &path)?;
            db.move_project(project.id, delta)?;
            list(&db, cli.json)
        }
        Some(Command::Rename { path, name }) => {
            let project = require_project(&db, &path)?;
            db.rename_project(project.id, &name)?;
            println!("renamed to {}", db.project(project.id)?.unwrap().name);
            Ok(())
        }
        Some(Command::Prune) => {
            let pruned = db.prune_missing()?;
            if pruned.is_empty() {
                println!("nothing to prune");
            }
            for project in pruned {
                println!("pruned {}", project.display_path());
            }
            Ok(())
        }
        Some(Command::Prefs { slot, program }) => prefs(&db, slot, program, cli.json),
        Some(Command::Agents) => show_agents(&db, cli.json),
        Some(Command::Programs { kind }) => show_programs(kind, cli.json),
        Some(Command::Find {
            query,
            root,
            depth,
            limit,
        }) => {
            let root = match root {
                Some(root) => root,
                None => db.ui_prefs()?.resolved_add_root(),
            };
            find(&db, &query.join(" "), root, depth, limit, cli.json)
        }
        Some(Command::Doctor) => doctor(&paths, &db),
        Some(Command::Board { path }) => show_board(&db, path, cli.json),
        Some(Command::Card { action }) => match action {
            CardAction::Add {
                path,
                title,
                column,
                body,
                by,
            } => card_add(&db, path, &title, column.as_deref(), body.as_deref(), by.as_deref()),
            CardAction::Claim { path, title, by } => {
                card_claim(&db, path, &title, Some(&by), cli.json)
            }
            CardAction::Release { path, title } => card_claim(&db, path, &title, None, cli.json),
            CardAction::Move { path, title, to } => card_move(&db, path, &title, &to, cli.json),
            CardAction::Done { path, title } => card_done(&db, path, &title, cli.json),
            CardAction::Next {
                path,
                by,
                in_column,
            } => card_next(&db, path, &by, in_column.as_deref(), cli.json),
        },
        Some(Command::Hook { action }) => match action {
            HookAction::Guard { file, path } => hook_guard(&db, file, path),
        },
    }
}

fn require_project(db: &Db, path: &PathBuf) -> Result<radar::db::Project> {
    db.project_by_path(path)?
        .with_context(|| format!("{} is not in the sidebar", path.display()))
}

/// The directory a board command works on: the given path or the current
/// directory — agents run with the project as their working directory, so
/// `radar card next` just works from inside a pane.
fn board_dir(path: Option<PathBuf>) -> Result<PathBuf> {
    radar::db::normalize_path(path.unwrap_or_else(|| PathBuf::from(".")))
}

/// Log a board change against the project, when it is in the sidebar. A board
/// works in any directory; the event log is a bonus, not a requirement.
fn log_board(db: &Db, dir: &PathBuf, kind: &str, data: serde_json::Value) {
    if let Ok(Some(project)) = db.project_by_path(dir) {
        let _ = db.log_event(kind, Some(project.id), &data);
    }
}

fn show_board(_db: &Db, path: Option<PathBuf>, json: bool) -> Result<()> {
    let dir = board_dir(path)?;
    let _ = board::ensure_file(&dir);
    let b = board::load(&dir)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&b)?);
        return Ok(());
    }
    for column in &b.columns {
        println!("{} ({})", column.name, column.cards.len());
        for card in &column.cards {
            match &card.claimed_by {
                Some(who) => println!("  · {}  @{}", card.title, who),
                None => println!("  · {}", card.title),
            }
            for note in &card.body {
                println!("      {}", note);
            }
        }
    }
    Ok(())
}

fn card_add(
    db: &Db,
    path: Option<PathBuf>,
    title: &str,
    column: Option<&str>,
    body: Option<&str>,
    by: Option<&str>,
) -> Result<()> {
    let dir = board_dir(path)?;
    let _ = board::ensure_file(&dir);
    board::add_card(&dir, column, title, body.unwrap_or(""), by)?;
    log_board(
        db,
        &dir,
        "board_card_added",
        serde_json::json!({ "title": title, "column": column, "by": by }),
    );
    println!("added \"{}\"", title);
    Ok(())
}

fn card_claim(
    db: &Db,
    path: Option<PathBuf>,
    title: &str,
    by: Option<&str>,
    json: bool,
) -> Result<()> {
    let dir = board_dir(path)?;
    let found = board::claim_card(&dir, title, by)?;
    if !found {
        anyhow::bail!("no card titled \"{}\"", title);
    }
    log_board(
        db,
        &dir,
        if by.is_some() {
            "board_card_claimed"
        } else {
            "board_card_released"
        },
        serde_json::json!({ "title": title, "by": by }),
    );
    if json {
        print_card(&dir, title)?;
    } else {
        match by {
            Some(who) => println!("\"{}\" claimed by {}", title, who),
            None => println!("\"{}\" released", title),
        }
    }
    Ok(())
}

fn card_move(db: &Db, path: Option<PathBuf>, title: &str, to: &str, json: bool) -> Result<()> {
    let dir = board_dir(path)?;
    let found = board::move_card(&dir, title, to)?;
    if !found {
        anyhow::bail!("no card titled \"{}\"", title);
    }
    log_board(
        db,
        &dir,
        "board_card_moved",
        serde_json::json!({ "title": title, "to": to }),
    );
    if json {
        print_card(&dir, title)?;
    } else {
        println!("\"{}\" moved to {}", title, to);
    }
    Ok(())
}

fn card_done(db: &Db, path: Option<PathBuf>, title: &str, json: bool) -> Result<()> {
    let dir = board_dir(path)?;
    let found = board::finish_card(&dir, title)?;
    if !found {
        anyhow::bail!("no card titled \"{}\"", title);
    }
    log_board(
        db,
        &dir,
        "board_card_done",
        serde_json::json!({ "title": title }),
    );
    if json {
        print_card(&dir, title)?;
    } else {
        println!("\"{}\" done", title);
    }
    Ok(())
}

/// The work primitive: hand the agent the next unclaimed card, claimed in its
/// name. An agent's whole loop is `card next` → do it → move to Review. This
/// is also where an agent meets the convention: the skill that teaches the
/// loop is installed into the project here, so the first `card next` from a
/// fresh clone sets the board up on its own.
fn card_next(
    db: &Db,
    path: Option<PathBuf>,
    by: &str,
    in_column: Option<&str>,
    json: bool,
) -> Result<()> {
    let dir = board_dir(path)?;
    let _ = board::ensure_file(&dir);
    if let Err(error) = radar::skill::install(&dir) {
        eprintln!("radar: could not install the board skill: {error}");
    }
    let Some(card) = board::next_card(&dir, by, in_column)? else {
        anyhow::bail!("no unclaimed cards");
    };
    log_board(
        db,
        &dir,
        "board_card_claimed",
        serde_json::json!({ "title": card.title, "by": by, "via": "next" }),
    );
    if json {
        println!("{}", serde_json::to_string_pretty(&card)?);
    } else {
        println!("\"{}\" — claimed for {}", card.title, by);
        for note in &card.body {
            println!("      {}", note);
        }
    }
    Ok(())
}

/// The hook half of the convention: an agent harness asks, before an edit
/// lands, whether the agent holds a board claim. Denied calls come back to
/// the model as a tool error whose text is the remedy — claim work, then
/// retry — so the guard enforces without stranding the agent.
///
/// Claude Code's PreToolUse hook is a subprocess: it pipes the tool call as
/// JSON and reads the exit code (2 denies, stderr goes to the model). The
/// opencode plugin calls the same check in-process. `--file` covers both and
/// the human running it by hand.
fn hook_guard(_db: &Db, file: Option<PathBuf>, path: Option<PathBuf>) -> Result<()> {
    use std::io::IsTerminal;

    let mut input = String::new();
    if !std::io::stdin().is_terminal() {
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
    }
    let file = file.or_else(|| hook_file(&input));

    let dir = board_dir(path)?;
    let who = std::env::var("RADAR_AGENT").ok().filter(|s| !s.is_empty());
    match radar::skill::guard_decision(&dir, who.as_deref(), file.as_deref()) {
        radar::skill::GuardDecision::Allow => Ok(()),
        radar::skill::GuardDecision::Deny(reason) => {
            eprintln!("{reason}");
            std::process::exit(2);
        }
    }
}

/// The file a harness's tool-call JSON wants to edit: `tool_input.file_path`
/// in Claude Code's pre-tool-use payload.
fn hook_file(input: &str) -> Option<PathBuf> {
    let value: serde_json::Value = serde_json::from_str(input).ok()?;
    value
        .get("tool_input")?
        .get("file_path")?
        .as_str()
        .map(PathBuf::from)
}

/// Print one card's current state, for `--json` answers.
fn print_card(dir: &Path, title: &str) -> Result<()> {
    let b = board::load(dir)?;
    let card = b
        .find(title)
        .map(|(c, i)| &b.columns[c].cards[i])
        .with_context(|| format!("no card titled \"{}\"", title))?;
    println!("{}", serde_json::to_string_pretty(card)?);
    Ok(())
}

fn list(db: &Db, json: bool) -> Result<()> {
    let projects = db.projects()?;
    let selected = db.ui_prefs()?.last_project;

    let mut rows = Vec::with_capacity(projects.len());
    for project in &projects {
        let status = git::status(&project.path);
        let tabs = db.tabs(project.id)?.len();
        rows.push(serde_json::json!({
            "id": project.id,
            "name": project.name,
            "path": project.path,
            "display_path": project.display_path(),
            "pinned": project.pinned,
            "missing": project.is_missing(),
            "last_opened_at": project.last_opened_at,
            "open_count": project.open_count,
            "selected": selected == Some(project.id),
            "git": {
                "is_repo": status.is_repo,
                "branch": status.branch,
                "ahead": status.ahead,
                "behind": status.behind,
                "changed": status.changed,
                "summary": status.summary(),
            },
            "tabs": tabs,
        }));
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no projects yet — add one with `radar add <dir>`");
        return Ok(());
    }
    let width = projects
        .iter()
        .map(|p| p.name.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    for row in &rows {
        let tabs = row["tabs"].as_i64().unwrap_or(0);
        println!(
            "{marker} {name:width$}  {summary:<14}  {path}{tabs}",
            marker = if row["selected"] == true { "▸" } else { " " },
            name = row["name"].as_str().unwrap_or(""),
            width = width,
            summary = row["git"]["summary"].as_str().unwrap_or(""),
            path = row["display_path"].as_str().unwrap_or(""),
            tabs = if tabs > 0 {
                format!("  ({tabs} tabs)")
            } else {
                String::new()
            },
        );
    }
    Ok(())
}

fn add(
    db: &Db,
    paths: Vec<PathBuf>,
    scan: Option<Option<PathBuf>>,
    depth: usize,
    no_select: bool,
    json: bool,
) -> Result<()> {
    let mut added = Vec::new();
    let did_scan = scan.is_some();
    if let Some(root) = scan {
        let root = root.unwrap_or(std::env::current_dir()?);
        let candidates = discover::scan(&root, depth, 500);
        let repos: Vec<_> = candidates.into_iter().filter(|c| c.is_repo).collect();
        for candidate in repos {
            if db.project_by_path(&candidate.path)?.is_none() {
                added.push(db.add_project(&candidate.path)?);
            }
        }
    }
    for path in paths {
        added.push(db.add_project(&path)?);
    }
    if added.is_empty() && did_scan {
        println!("no new repositories found");
        return Ok(());
    }
    if let Some(first) = added.first() {
        db.log_event("project_added", Some(first.id), &serde_json::Value::Null)?;
        if !no_select {
            db.remember_last_project(Some(first.id))?;
        }
    }
    if json {
        let rows: Vec<_> = added
            .iter()
            .map(|p| serde_json::json!({ "id": p.id, "name": p.name, "path": p.path }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        for project in &added {
            println!("added {} ({})", project.name, project.display_path());
        }
    }
    Ok(())
}

/// Show what a project's tabs resolve to right now.
fn open(db: &Db, path: &PathBuf, json: bool) -> Result<()> {
    let project = require_project(db, path)?;
    db.touch_project(project.id)?;
    db.remember_last_project(Some(project.id))?;
    db.log_event("project_opened", Some(project.id), &serde_json::Value::Null)?;
    // Starting a project starts its board: the file is there before any agent
    // looks for it. A project that cannot carry a file still opens.
    let _ = board::ensure_file(&project.path);

    let preferences = db.preferences()?;
    let stored = db.tabs(project.id)?;
    let tabs = if stored.is_empty() {
        default_tabs(&preferences)
    } else {
        stored
    };
    let options = launch_options(&preferences, false);

    let resolved: Vec<serde_json::Value> = tabs
        .iter()
        .filter_map(|tab| {
            let program = programs::by_id(&tab.program_id)?;
            let mut options = options.clone();
            options.extra_args = tab.extra_args.clone();
            let spec = program.command_spec(&options);
            Some(serde_json::json!({
                "slot": tab.slot.as_str(),
                "program": program.id,
                "name": program.name,
                "exec": spec.argv,
                "cwd": project.path,
            }))
        })
        .collect();

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "project": { "id": project.id, "name": project.name, "path": project.path },
                "status": git::status(&project.path).summary(),
                "tabs": resolved,
            }))?
        );
    } else {
        println!(
            "{}  ·  {}  ·  {}",
            project.name,
            project.display_path(),
            git::status(&project.path).summary()
        );
        for tab in resolved {
            println!(
                "  {:<7} {:<16} {}",
                tab["slot"].as_str().unwrap_or(""),
                tab["name"].as_str().unwrap_or(""),
                tab["exec"]
                    .as_array()
                    .map(|argv| argv
                        .iter()
                        .map(|a| a.as_str().unwrap_or(""))
                        .collect::<Vec<_>>()
                        .join(" "))
                    .unwrap_or_default()
            );
        }
    }
    Ok(())
}

/// The tabs a brand new project starts with: editor, agent, diff.
fn default_tabs(preferences: &Preferences) -> Vec<Tab> {
    let mut tabs = Vec::new();
    for slot in [Slot::Editor, Slot::Agent, Slot::Diff] {
        if let Some(program) = programs::for_slot(slot, preferences) {
            tabs.push(Tab::new(slot, program.id));
        }
    }
    tabs
}

fn launch_options(preferences: &Preferences, safe: bool) -> LaunchOptions {
    LaunchOptions {
        // omarchy's keybinding runs agents unattended; match that by default.
        safe: safe || !preferences.agent_auto_flags,
        extra_args: Vec::new(),
        prompt: None,
        agent_instance: None,
    }
}

fn prefs(db: &Db, slot: Option<String>, program: Option<String>, json: bool) -> Result<()> {
    match (slot, program) {
        (Some(slot), Some(program)) => {
            let slot = match slot.as_str() {
                "editor" => Slot::Editor,
                "agent" => Slot::Agent,
                "diff" => Slot::Diff,
                "shell" => Slot::Shell,
                other => anyhow::bail!("unknown slot {other} (editor, agent, diff, shell)"),
            };
            let program = if program == "none" { None } else { Some(program) };
            if let Some(id) = program.as_deref() {
                anyhow::ensure!(
                    programs::by_id(id).is_some(),
                    "{id} is not a program radar knows about (see `radar programs`)"
                );
            }
            db.set_preference(slot, program.as_deref())?;
            db.log_event(
                "preference_changed",
                None,
                &serde_json::json!({ "slot": slot.as_str(), "program": program }),
            )?;
        }
        (Some(slot), None) => {
            // Show the resolved choice for this slot and exit.
            let slot = match slot.as_str() {
                "editor" => Slot::Editor,
                "agent" => Slot::Agent,
                "diff" => Slot::Diff,
                "shell" => Slot::Shell,
                other => anyhow::bail!("unknown slot {other}"),
            };
            let preferences = db.preferences()?;
            let resolved = programs::for_slot(slot, &preferences);
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "slot": slot.as_str(),
                        "preferred": preferences.get(slot),
                        "resolved": resolved.map(|p| p.id),
                    })
                );
            } else {
                println!(
                    "{}: {}",
                    slot.as_str(),
                    resolved.map(|p| p.name).unwrap_or_else(|| "nothing installed".into())
                );
            }
            return Ok(());
        }
        (None, _) => {}
    }

    let preferences = db.preferences()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&preferences)?);
        return Ok(());
    }
    for slot in [Slot::Editor, Slot::Agent, Slot::Diff, Slot::Shell] {
        let preferred = preferences.get(slot);
        let resolved = programs::for_slot(slot, &preferences);
        println!(
            "{:<7} {:<22} {}",
            slot.as_str(),
            preferred.unwrap_or("auto"),
            resolved
                .map(|p| format!("→ {} ({})", p.name, p.id))
                .unwrap_or_else(|| "→ nothing installed".into())
        );
    }
    println!(
        "\nagent permission flags: {}",
        if preferences.agent_auto_flags {
            "auto (skip prompts, like omarchy)"
        } else {
            "safe (ask)"
        }
    );
    Ok(())
}

fn show_agents(db: &Db, json: bool) -> Result<()> {
    let preferences = db.preferences()?;
    let default = agents::omarchy_default();
    let rows: Vec<serde_json::Value> = agents::programs()
        .iter()
        .map(|agent| {
            serde_json::json!({
                "id": agent.id,
                "name": agent.name,
                "installed": agent.installed(),
                "omarchy": agent.omarchy,
                "default": default.as_deref() == Some(agent.id.as_str()),
                "preferred": preferences.agent.as_deref() == Some(agent.id.as_str()),
                "permission_args": agent.auto_args,
            })
        })
        .collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    println!(
        "omarchy default agent: {}",
        default.unwrap_or_else(|| "(none set — `omarchy default agent <name>`)".into())
    );
    println!(
        "preferred in radar: {}",
        preferences.agent.as_deref().unwrap_or("(follow omarchy)")
    );
    println!();
    for row in rows {
        let mark = if row["default"] == true {
            "◆"
        } else if row["preferred"] == true {
            "▸"
        } else {
            " "
        };
        println!(
            "{mark} {:<18} {:<16} {}",
            row["id"].as_str().unwrap_or(""),
            row["name"].as_str().unwrap_or(""),
            if row["installed"] == true { "installed" } else { "not installed" }
        );
    }
    Ok(())
}

fn show_programs(kind: Option<String>, json: bool) -> Result<()> {
    let wanted: Option<Kind> = kind.as_deref().and_then(|k| match k {
        "editor" => Some(Kind::Editor),
        "agent" => Some(Kind::Agent),
        "diff" => Some(Kind::Diff),
        "shell" => Some(Kind::Shell),
        "tool" => Some(Kind::Tool),
        _ => None,
    });
    let programs = programs::registry();
    if json {
        let rows: Vec<_> = programs
            .iter()
            .filter(|p| wanted.is_none_or(|k| p.kind == k))
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "name": p.name,
                    "kind": p.kind,
                    "installed": p.installed(),
                    "external": p.external,
                    "args": p.args,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    for kind in Kind::ALL {
        if wanted.is_some_and(|k| k != kind) {
            continue;
        }
        let of_kind: Vec<_> = programs.iter().filter(|p| p.kind == kind).collect();
        if of_kind.is_empty() {
            continue;
        }
        println!("{}", kind.label());
        for program in of_kind {
            println!(
                "  {:<16} {:<18} {:<12} {}",
                program.id,
                program.name,
                if program.installed() { "installed" } else { "missing" },
                program.detail()
            );
        }
        println!();
    }
    Ok(())
}

fn find(db: &Db, query: &str, root: PathBuf, depth: usize, limit: usize, json: bool) -> Result<()> {
    let mut candidates = discover::scan(&root, depth, 500);
    let known: Vec<PathBuf> = db.projects()?.into_iter().map(|p| p.path).collect();
    discover::mark_known(&mut candidates, &known);
    let hits = discover::filter(&candidates, query);
    let hits: Vec<_> = hits.into_iter().take(limit).collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
        return Ok(());
    }
    if hits.is_empty() {
        println!("nothing matches {query:?} under {}", root.display());
        return Ok(());
    }
    let width = hits.iter().map(|c| c.name.chars().count()).max().unwrap_or(4);
    for candidate in hits {
        println!(
            "{marker} {name:width$}  {path}{known}",
            marker = if candidate.is_repo { "◆" } else { " " },
            name = candidate.name,
            width = width,
            path = candidate.display_path(),
            known = if candidate.known { "  (already added)" } else { "" },
        );
    }
    Ok(())
}

fn doctor(paths: &Paths, db: &Db) -> Result<()> {
    println!("radar {}", env!("CARGO_PKG_VERSION"));
    println!("state:  {}", paths.data_dir.display());
    println!("config: {}", paths.config_dir.display());
    println!(
        "db:     {} ({})",
        paths.database().display(),
        if paths.database().exists() {
            "present"
        } else {
            "will be created"
        }
    );
    println!(
        "schema: v{}",
        db.conn()
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?
    );
    println!("projects: {}", db.projects()?.len());
    println!();

    // name, present, what it gives us, how to get it when missing
    let checks: [(&str, bool, &str, &str); 8] = [
        ("git", radar::config::have("git"), "project status", "pacman -S git"),
        ("fd", radar::config::have("fd"), "fast directory scanning", "pacman -S fd"),
        ("rg", radar::config::have("rg"), "searching", "pacman -S ripgrep"),
        ("fzf", radar::config::have("fzf"), "external fuzzy picking", "pacman -S fzf"),
        (
            "omarchy",
            radar::config::have("omarchy"),
            "default agent + agent flags",
            "part of omarchy",
        ),
        ("lazygit", radar::config::have("lazygit"), "diff tabs", "pacman -S lazygit"),
        (
            "libvte-2.91-gtk4",
            vte_available(),
            "embedded terminals",
            "omarchy pkg add vte4",
        ),
        (
            "gtk4",
            gtk_available(),
            "the app itself",
            "omarchy pkg add gtk4 libadwaita",
        ),
    ];
    for (name, present, gives, remedy) in checks {
        if present {
            println!("ok   {name:<20} {gives}");
        } else {
            println!("miss {name:<20} {gives} — install with: {remedy}");
        }
    }
    println!();
    println!(
        "gui build:    {}",
        if cfg!(feature = "gui") {
            "included"
        } else {
            "not included (rebuild with --features gui)"
        }
    );
    let preferences = db.preferences()?;
    for slot in [Slot::Editor, Slot::Agent, Slot::Diff, Slot::Shell] {
        println!(
            "{:<13} {}",
            format!("{}:", slot.as_str()),
            programs::for_slot(slot, &preferences)
                .map(|p| format!("{} ({})", p.name, p.id))
                .unwrap_or_else(|| "nothing installed".into())
        );
    }
    Ok(())
}

/// Is the GTK4 VTE development file present, which is what the build needs?
fn vte_available() -> bool {
    ["/usr/lib/pkgconfig/vte-2.91-gtk4.pc", "/usr/lib64/pkgconfig/vte-2.91-gtk4.pc"]
        .iter()
        .any(|path| std::path::Path::new(path).exists())
        || std::env::var("PKG_CONFIG_PATH")
            .map(|paths| paths.split(':').any(|dir| {
                std::path::Path::new(dir).join("vte-2.91-gtk4.pc").exists()
            }))
            .unwrap_or(false)
}

fn gtk_available() -> bool {
    ["/usr/lib/pkgconfig/gtk4.pc", "/usr/lib64/pkgconfig/gtk4.pc"]
        .iter()
        .any(|path| std::path::Path::new(path).exists())
}

#[cfg(feature = "gui")]
fn run_gui(paths: Paths, db: Db) -> Result<()> {
    radar::gui::run(paths, db)
}

#[cfg(not(feature = "gui"))]
fn run_gui(_paths: Paths, _db: Db) -> Result<()> {
    eprintln!(
        "This build has no GUI.\n\n\
         The app needs the GTK4 VTE widget:\n  \
         omarchy pkg add vte4\n\
         then rebuild:\n  \
         cargo run --features gui\n\n\
         The CLI works already: radar list, radar find, radar doctor"
    );
    std::process::exit(2);
}
