//! A Model Context Protocol server for a project's board.
//!
//! `radar mcp serve` speaks stdio MCP — newline-delimited JSON-RPC on
//! stdin/stdout — and exposes the card surface as tools, so any MCP-capable
//! agent harness can work the board: claim work, report on it, hand it back,
//! and dispatch workers. This is the same loop the board skill teaches, as
//! callable tools. Nothing but protocol responses may be written to stdout;
//! every diagnostic goes to stderr.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::Result;
use serde_json::{json, Value};

use crate::config::Paths;
use crate::db::Db;
use crate::session::board::find_card;
use crate::session::board_store::{BoardState, StoredCard};
use crate::session::daemon as board_api;
use crate::session::daemon::{Client, Command as Request, Response};

/// The protocol versions this server speaks.
const SUPPORTED: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];
const SERVER_VERSION: &str = "2025-06-18";

/// Read requests from stdin, answer on stdout, until the client hangs up.
pub fn serve(paths: Paths, project: Option<PathBuf>) -> Result<()> {
    let db = Db::open(&paths)?;
    let ctx = Ctx { paths, project, db };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let incoming = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(error) => {
                write_response(
                    &stdout,
                    error_response(Value::Null, -32700, format!("parse error: {error}")),
                )?;
                continue;
            }
        };
        if let Some(response) = handle(&ctx, &incoming) {
            write_response(&stdout, response)?;
        }
    }
    Ok(())
}

struct Ctx {
    paths: Paths,
    /// The board this server is pinned to, when `--project` was given.
    project: Option<PathBuf>,
    db: Db,
}

/// One JSON-RPC request in, one response out; notifications are silent.
fn handle(ctx: &Ctx, request: &Value) -> Option<Value> {
    // A request has an id; a notification has none and is answered by silence.
    let id = request.get("id")?.clone();
    let params = request.get("params").cloned().unwrap_or(json!({}));
    match request["method"].as_str().unwrap_or_default() {
        "initialize" => Some(result_response(
            id,
            json!({
                "protocolVersion": request["params"]["protocolVersion"]
                    .as_str()
                    .filter(|version| SUPPORTED.contains(version))
                    .unwrap_or(SERVER_VERSION),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "radar", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "Work one card at a time: card_next claims the next unclaimed \
                 card, card_show reads it, card_comment reports to the human, card_move hands \
                 it back. card_done is the reviewer's word, never the worker's. lane 'review' \
                 in card_next prefers reviewing first.",
            }),
        )),
        "ping" => Some(result_response(id, json!({}))),
        "tools/list" => Some(result_response(id, json!({ "tools": tools() }))),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default().to_string();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match call_tool(ctx, &name, &args) {
                Ok(text) => Some(result_response(
                    id,
                    json!({ "content": [{ "type": "text", "text": text }] }),
                )),
                Err(error) => Some(result_response(
                    id,
                    json!({
                        "content": [{ "type": "text", "text": error }],
                        "isError": true,
                    }),
                )),
            }
        }
        other => Some(error_response(
            id,
            -32601,
            format!("method not found: {other}"),
        )),
    }
}

/// The tool surface: the card loop, as the board skill teaches it.
fn tools() -> Value {
    fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
        json!({
            "name": name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": required,
            },
        })
    }
    let card_arg = || json!({ "type": "string", "description": "Stable card id or its title" });
    let project = || json!({ "project": { "type": "string", "description": "Project directory; default: the server's --project, $RADAR_PROJECT_ROOT, or the working directory" } });
    Value::Array(vec![
        tool(
            "board_list",
            "List a project's board: lanes with their cards.",
            project(),
            &[],
        ),
        tool(
            "card_add",
            "Add a card to the board (default lane: Todo).",
            json!({
                "title": { "type": "string", "description": "The card's title" },
                "notes": { "type": "string", "description": "Notes for the card" },
                "lane": { "type": "string", "description": "Which lane to put it in" },
                "project": project()["project"].clone(),
            }),
            &["title"],
        ),
        tool(
            "card_next",
            "Claim the first unclaimed card in your name and get it — how an \
             agent asks for work. Claims start Todo cards; Review cards stay \
             in Review (claiming starts the review, and `card done` is the \
             reviewer's word).",
            json!({
                "by": { "type": "string", "description": "Claim it for this name; default $RADAR_AGENT, else mcp" },
                "lane": { "type": "string", "description": "Only consider cards in this lane (use 'review' to review first)" },
                "project": project()["project"].clone(),
            }),
            &[],
        ),
        tool(
            "card_claim",
            "Claim a card for a name.",
            json!({
                "card": card_arg(),
                "by": { "type": "string", "description": "Claim it for this name; default $RADAR_AGENT, else mcp" },
                "project": project()["project"].clone(),
            }),
            &["card"],
        ),
        tool(
            "card_release",
            "Release a card: drop its claim, nobody is on it.",
            json!({ "card": card_arg(), "project": project()["project"].clone() }),
            &["card"],
        ),
        tool(
            "card_move",
            "Move a card to a lane.",
            json!({
                "card": card_arg(),
                "to": { "type": "string", "description": "The lane's name" },
                "project": project()["project"].clone(),
            }),
            &["card", "to"],
        ),
        tool(
            "card_done",
            "Mark a card done: checked, and moved to the last lane. The \
             reviewer's word, never the worker's.",
            json!({ "card": card_arg(), "project": project()["project"].clone() }),
            &["card"],
        ),
        tool(
            "card_show",
            "Read a card: its lane, claim, notes and conversation thread.",
            json!({ "card": card_arg(), "project": project()["project"].clone() }),
            &["card"],
        ),
        tool(
            "card_comment",
            "Post a report on a card's thread. kind: checkpoint (progress; \
             the default), blocked (cannot continue — opens a question for \
             the human), done (hands the card back like the turn-end path), \
             artifact (a durable reference with \"artifact\").",
            json!({
                "card": card_arg(),
                "text": { "type": "string", "description": "The report" },
                "kind": { "type": "string", "description": "checkpoint | blocked | done | artifact" },
                "artifact": { "type": "string", "description": "An opaque durable reference (URL, path)" },
                "project": project()["project"].clone(),
            }),
            &["card", "text"],
        ),
        tool(
            "card_edit",
            "Edit a card's title and/or notes.",
            json!({
                "card": card_arg(),
                "title": { "type": "string", "description": "New title" },
                "notes": { "type": "string", "description": "New notes" },
                "project": project()["project"].clone(),
            }),
            &["card"],
        ),
        tool(
            "card_start",
            "Dispatch a worker agent for a card: the daemon spawns it with the \
             card's work prompt, attached to the card and claiming it. \
             Idempotent: a card already claimed is left alone, so an \
             orchestrator can call it repeatedly.",
            json!({ "card": card_arg(), "project": project()["project"].clone() }),
            &["card"],
        ),
    ])
}

/// Dispatch one tool call to the board, returning the text an agent reads.
fn call_tool(ctx: &Ctx, name: &str, args: &Value) -> Result<String, String> {
    let project = args.get("project").and_then(Value::as_str);
    let resolve = |ctx: &Ctx, project: Option<&str>| -> Result<(i64, PathBuf), String> {
        let (project_id, root) = crate::session::board::context(
            &ctx.db,
            project.map(PathBuf::from).or_else(|| ctx.project.clone()),
        )
        .map_err(|error| error.to_string())?;
        ctx.db
            .require_board_enabled(&root)
            .map_err(|error| error.to_string())?;
        Ok((project_id, root))
    };
    let claim_name = |args: &Value| -> String {
        args.get("by")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| std::env::var("RADAR_AGENT").ok())
            .unwrap_or_else(|| "mcp".to_string())
    };
    let home = &ctx.paths.data_dir;
    let card_id = |project_id: i64, needle: &str| -> Result<String, String> {
        let board = board_api::board_state(home, project_id).map_err(|error| error.to_string())?;
        find_card(&board.state, needle)
            .map(|card| card.id.clone())
            .ok_or_else(|| format!("no card matching \"{needle}\""))
    };

    match name {
        "board_list" => {
            let (project_id, _) = resolve(ctx, project)?;
            let board = board_api::board_state(home, project_id).map_err(|e| e.to_string())?;
            Ok(pretty(board_json(&board.state)))
        }
        "card_add" => {
            let title = required(args, "title")?;
            let (project_id, _) = resolve(ctx, project)?;
            let change = board_api::board_card_add(
                home,
                project_id,
                args.get("lane").and_then(Value::as_str),
                &title,
                args.get("notes").and_then(Value::as_str).unwrap_or(""),
                None,
                &crate::session::board::command_id("mcp-add"),
            )
            .map_err(|e| e.to_string())?;
            Ok(pretty(serde_json::to_value(&change.card).unwrap()))
        }
        "card_next" => {
            let (project_id, root) = resolve(ctx, project)?;
            if let Err(error) = crate::skill::install(&ctx.db, &root) {
                eprintln!("radar: could not install the board skill: {error}");
            }
            match board_api::board_card_next(
                home,
                project_id,
                &claim_name(args),
                args.get("lane").and_then(Value::as_str),
                &crate::session::board::command_id("mcp-next"),
            )
            .map_err(|e| e.to_string())?
            {
                Some(change) => Ok(pretty(serde_json::to_value(&change.card).unwrap())),
                None => Ok("no unclaimed cards".to_string()),
            }
        }
        "card_claim" => {
            let needle = required(args, "card")?;
            let (project_id, _) = resolve(ctx, project)?;
            let card_id = card_id(project_id, &needle)?;
            let change = board_api::board_card_claim(
                home,
                project_id,
                &card_id,
                Some(&claim_name(args)),
                None,
                &crate::session::board::command_id("mcp-claim"),
            )
            .map_err(|e| e.to_string())?;
            Ok(pretty(serde_json::to_value(&change.card).unwrap()))
        }
        "card_release" => {
            let needle = required(args, "card")?;
            let (project_id, _) = resolve(ctx, project)?;
            let card_id = card_id(project_id, &needle)?;
            let change = board_api::board_card_claim(
                home,
                project_id,
                &card_id,
                None,
                None,
                &crate::session::board::command_id("mcp-release"),
            )
            .map_err(|e| e.to_string())?;
            Ok(pretty(serde_json::to_value(&change.card).unwrap()))
        }
        "card_move" => {
            let needle = required(args, "card")?;
            let to = required(args, "to")?;
            let (project_id, _) = resolve(ctx, project)?;
            let card_id = card_id(project_id, &needle)?;
            let change = board_api::board_card_move(
                home,
                project_id,
                &card_id,
                &to,
                None,
                &crate::session::board::command_id("mcp-move"),
            )
            .map_err(|e| e.to_string())?;
            Ok(pretty(serde_json::to_value(&change.card).unwrap()))
        }
        "card_done" => {
            let needle = required(args, "card")?;
            let (project_id, _) = resolve(ctx, project)?;
            let card_id = card_id(project_id, &needle)?;
            let change = board_api::board_card_complete(
                home,
                project_id,
                &card_id,
                None,
                &crate::session::board::command_id("mcp-done"),
            )
            .map_err(|e| e.to_string())?;
            Ok(pretty(serde_json::to_value(&change.card).unwrap()))
        }
        "card_show" => {
            let needle = required(args, "card")?;
            let (project_id, _) = resolve(ctx, project)?;
            let board = board_api::board_state(home, project_id).map_err(|e| e.to_string())?;
            let card = find_card(&board.state, &needle)
                .cloned()
                .ok_or_else(|| format!("no card matching \"{needle}\""))?;
            let lane = board
                .state
                .lanes
                .iter()
                .find(|lane| lane.id == card.lane_id)
                .map(|lane| lane.name.clone())
                .unwrap_or_default();
            let thread = match Client::request(
                home,
                Request::ActivitySnapshot {
                    project_id,
                    after_sequence: None,
                    limit: 200,
                },
            ) {
                Ok(Response::ActivitySnapshot(snapshot)) => thread_json(&snapshot, &card.id),
                _ => Vec::new(),
            };
            Ok(pretty(
                json!({ "card": card, "lane": lane, "thread": thread }),
            ))
        }
        "card_comment" => {
            let needle = required(args, "card")?;
            let text = required(args, "text")?;
            let (project_id, root) = resolve(ctx, project)?;
            let card_id = card_id(project_id, &needle)?;
            let kind = args
                .get("kind")
                .and_then(Value::as_str)
                .map(|given| {
                    crate::session::report::Kind::parse(given).ok_or_else(|| {
                        format!("kind {given} is not one of: checkpoint, blocked, done, artifact")
                    })
                })
                .transpose()?;
            let report = crate::session::report::Report {
                kind: kind.unwrap_or(crate::session::report::Kind::Checkpoint),
                text,
                artifact: args
                    .get("artifact")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            };
            let command = Request::CardReport {
                project_id,
                card_id,
                session_id: std::env::var("RADAR_SESSION_ID").ok(),
                root,
                report,
                command_id: crate::session::board::command_id("mcp-comment"),
            };
            match Client::request(home, command) {
                Ok(Response::CardReported(reported)) => {
                    let mut said = format!("reported (event {})", reported.event["sequence"]);
                    if let Some(id) = &reported.attention {
                        said.push_str(&format!(" - question {id} opened for the human"));
                    }
                    if let Some(what) = &reported.handoff {
                        said.push_str(&format!(" - handoff: {what}"));
                    }
                    Ok(said)
                }
                Ok(other) => Err(format!("unexpected daemon response: {other:?}")),
                Err(error) => Err(error.to_string()),
            }
        }
        "card_edit" => {
            let needle = required(args, "card")?;
            let (project_id, _) = resolve(ctx, project)?;
            let card_id = card_id(project_id, &needle)?;
            let change = board_api::board_card_update(
                home,
                project_id,
                &card_id,
                args.get("title").and_then(Value::as_str),
                args.get("notes").and_then(Value::as_str),
                None,
                &crate::session::board::command_id("mcp-edit"),
            )
            .map_err(|e| e.to_string())?;
            Ok(pretty(serde_json::to_value(&change.card).unwrap()))
        }
        "card_start" => {
            let needle = required(args, "card")?;
            let (project_id, root) = resolve(ctx, project)?;
            let board = board_api::board_state(home, project_id).map_err(|e| e.to_string())?;
            let card = find_card(&board.state, &needle)
                .cloned()
                .ok_or_else(|| format!("no card matching \"{needle}\""))?;
            match crate::session::dispatch::start_card(
                &ctx.paths,
                &ctx.db,
                project_id,
                &root,
                &card,
                &crate::session::board::command_id("mcp-start"),
            )
            .map_err(|e| e.to_string())?
            {
                crate::session::dispatch::Dispatch::Started {
                    claim, session_id, ..
                } => Ok(pretty(json!({
                    "created": true,
                    "cardId": card.id,
                    "claim": claim,
                    "sessionId": session_id,
                }))),
                crate::session::dispatch::Dispatch::AlreadyClaimed { claim } => Ok(pretty(json!({
                    "created": false,
                    "cardId": card.id,
                    "claim": claim,
                }))),
            }
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// The board as lanes with their cards, in lane order.
fn board_json(state: &BoardState) -> Value {
    let lanes: Vec<Value> = state
        .lanes
        .iter()
        .map(|lane| {
            json!({
                "id": lane.id,
                "name": lane.name,
                "kind": lane.kind,
                "cards": state
                    .cards
                    .iter()
                    .filter(|card| card.lane_id == lane.id)
                    .collect::<Vec<&StoredCard>>(),
            })
        })
        .collect();
    json!({ "project_id": state.project_id, "lanes": lanes })
}

fn required(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing required argument: {key}"))
}

fn pretty(value: Value) -> String {
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
}

/// A card's thread as flat entries.
/// A card's thread as flat entries: what `card show` prints and the MCP
/// `card_show` returns.
pub fn thread_json(
    snapshot: &crate::session::activity::ActivitySnapshot,
    card_id: &str,
) -> Vec<Value> {
    use crate::session::activity::ActivityPayload;
    let mut events: Vec<_> = snapshot
        .events
        .iter()
        .filter(|event| event.card_id.as_deref() == Some(card_id))
        .collect();
    events.sort_by_key(|event| event.sequence);
    events
        .into_iter()
        .filter_map(|event| {
            let (author, text) = match &event.payload {
                ActivityPayload::Message { text } => (
                    if event.session_id.is_some() {
                        "agent"
                    } else {
                        "human"
                    },
                    text.clone(),
                ),
                ActivityPayload::AttentionRequested { reason, .. } => ("agent", reason.clone()),
                ActivityPayload::AttentionResolved { response, .. } => {
                    ("system", format!("answered: {response:?}"))
                }
                ActivityPayload::BoardChanged { action, column, .. } => ("system", {
                    let label = match action.as_str() {
                        "added" | "board_card_added" => "added",
                        "moved" | "board_card_moved" => "moved",
                        "claimed" | "board_card_claimed" => "claimed",
                        "released" | "board_card_released" => "released",
                        "done" | "board_card_done" => "closed",
                        "edited" | "board_card_edited" => "edited",
                        "removed" | "board_card_removed" => "removed",
                        other => other,
                    };
                    match column {
                        Some(column) => format!("{label} · {column}"),
                        None => label.to_string(),
                    }
                }),
                _ => return None,
            };
            Some(json!({
                "author": author,
                "text": text,
                "at_millis": event.at_millis,
                "sequence": event.sequence,
            }))
        })
        .collect()
}

fn result_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

fn write_response(stdout: &std::io::Stdout, response: Value) -> Result<()> {
    let mut out = stdout.lock();
    writeln!(out, "{response}")?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> (tempfile::TempDir, Ctx) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        let db = Db::open(&paths).unwrap();
        (
            dir,
            Ctx {
                paths,
                project: None,
                db,
            },
        )
    }

    fn request(id: Value, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    #[test]
    fn initialize_echoes_a_supported_protocol_version() {
        let (_dir, ctx) = ctx();
        let response = handle(
            &ctx,
            &request(
                json!(1),
                "initialize",
                json!({
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": { "name": "pi", "version": "1" },
                }),
            ),
        )
        .unwrap();
        assert_eq!(response["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(response["result"]["serverInfo"]["name"], "radar");
        assert!(response["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn an_unsupported_protocol_version_gets_the_server_latest() {
        let (_dir, ctx) = ctx();
        let response = handle(
            &ctx,
            &request(
                json!(1),
                "initialize",
                json!({ "protocolVersion": "1999-01-01" }),
            ),
        )
        .unwrap();
        assert_eq!(response["result"]["protocolVersion"], SERVER_VERSION);
    }

    #[test]
    fn notifications_are_silent() {
        let (_dir, ctx) = ctx();
        assert!(handle(
            &ctx,
            &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
        )
        .is_none());
    }

    #[test]
    fn tools_list_covers_the_card_loop() {
        let (_dir, ctx) = ctx();
        let response = handle(&ctx, &request(json!(2), "tools/list", json!({}))).unwrap();
        let names: Vec<&str> = response["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "board_list",
                "card_add",
                "card_next",
                "card_claim",
                "card_release",
                "card_move",
                "card_done",
                "card_show",
                "card_comment",
                "card_edit",
                "card_start",
            ]
        );
    }

    #[test]
    fn an_unknown_method_is_a_json_rpc_error() {
        let (_dir, ctx) = ctx();
        let response = handle(&ctx, &request(json!(3), "resources/list", json!({}))).unwrap();
        assert_eq!(response["error"]["code"], -32601);
    }

    #[test]
    fn an_unknown_tool_fails_inside_the_call_result() {
        let (_dir, ctx) = ctx();
        let response = handle(
            &ctx,
            &request(
                json!(4),
                "tools/call",
                json!({ "name": "card_nixt", "arguments": {} }),
            ),
        )
        .unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert!(response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
    }

    #[test]
    fn a_missing_required_argument_fails_inside_the_call_result() {
        let (_dir, ctx) = ctx();
        let response = handle(
            &ctx,
            &request(
                json!(5),
                "tools/call",
                json!({ "name": "card_add", "arguments": {} }),
            ),
        )
        .unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert_eq!(
            response["result"]["content"][0]["text"],
            "missing required argument: title"
        );
    }

    #[test]
    fn a_project_off_the_sidebar_errors_without_touching_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("not-a-project");
        std::fs::create_dir_all(&project).unwrap();
        let (_ctx_dir, ctx) = ctx();
        let response = handle(
            &ctx,
            &request(
                json!(6),
                "tools/call",
                json!({
                    "name": "board_list",
                    "arguments": { "project": project.to_string_lossy() },
                }),
            ),
        )
        .unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert!(response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not in the sidebar"));
    }

    #[test]
    fn ping_answers_an_empty_result() {
        let (_dir, ctx) = ctx();
        let response = handle(&ctx, &request(json!(7), "ping", json!({}))).unwrap();
        assert_eq!(response["result"], json!({}));
    }
}
