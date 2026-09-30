# ACP agent sessions

radar can drive an agent that speaks the [Agent Client Protocol][acp] (ACP)
instead of only rendering its terminal. `opencode acp` is the first such agent;
the adapter is agent-agnostic.

The point is not a prettier terminal. A terminal only tells radar what an agent
*printed*; ACP tells it what the agent is *doing*. The board has always wanted
that: an explicit agent state, real approval prompts, and the conversation id
for exact resume. This adapter supplies all three from the agent itself.

[acp]: https://agentclientprotocol.com/

## What runs today

`radar acp start` asks the session daemon to own one ACP agent session. The
daemon spawns the agent as a subprocess and speaks newline-delimited JSON-RPC
over its stdin/stdout using the [`agent-client-protocol`][sdk] Rust SDK.

[sdk]: https://crates.io/crates/agent-client-protocol

- **Ownership.** `AgentHost` (in `src/session/agent.rs`) owns every agent,
  keyed by a stable id, exactly as the registry owns PTYs. One blocking worker
  thread per agent runs the SDK connection under
  `futures_lite::future::block_on`, so the daemon gains no async runtime.
- **Events.** `session/update` notifications become project activity:
  `agent_message_chunk` is accumulated into one `Reported` message per turn,
  a `tool_call` becomes a `Reported` "tool: …" line, and turn boundaries publish
  explicit `AgentStateChanged` (working → idle).
- **Attention.** `session/request_permission` becomes a durable approval
  attention request through the existing `ActivityJournal`. The human's
  `approve`/`deny` response is mapped back onto the agent's offered
  `PermissionOption`s and sent as the protocol's permission outcome.
- **Lifecycle.** Start, ready, exited, and failed are published as
  `SessionLifecycle` activity, so the board and every watcher see the agent
  come and go.

## Mapping

| ACP | radar |
| --- | --- |
| `initialize` / `session/new` | worker start; `acp_session_id` in `AgentStatus` |
| `session/update` `agent_message_chunk` | `Reported` message at turn end |
| `session/update` `tool_call` | `Reported` "tool: {title}" |
| prompt turn start / `stopReason` | `AgentStateChanged` working / idle |
| `session/request_permission` | approval `Attention` (approve, deny) |
| permission response | `RequestPermissionOutcome::Selected` / `Cancelled` |
| agent process exit | `SessionLifecycle` exited / failed |

A permission prompt blocks the connection's dispatch loop while it waits. That
is the protocol's own backpressure — the agent is paused waiting for the
decision — and `radar acp stop` unblocks it so the worker can exit.

## Usage

```sh
# Start an agent for the current project (RADAR_PROJECT_ID is required).
radar acp start my-agent --program opencode

# Send a turn. Updates and state appear on the project feed.
radar acp prompt my-agent "summarise the failing test"

# Approve or deny a permission prompt (the id comes from the feed).
radar activity respond attention-2-9 --project-id 2 --revision 1 --action approve

radar acp cancel my-agent
radar acp list
radar acp stop my-agent
```

`--arg` overrides the default `acp` argument shape for agents other than
opencode; the working directory is sent in the ACP `session/new` request, not on
the command line. `--project-id`, `--session-id`, and `--card-id` default to the
`RADAR_PROJECT_ID`, `RADAR_SESSION_ID`, and `RADAR_CARD_ID` a radar-launched pane
carries, so an agent started from a card links its activity to that card.

## Tests

`tests/fixtures/acp_fake_agent.py` is a minimal ACP v1 agent (no model, no
network). `tests/acp_daemon.rs` starts the real daemon binary against it and
proves the whole loop: streamed updates, a permission prompt round-tripped to
the human and back as `allow`, a denied prompt as `reject`, and clean shutdown.
`examples/acp_probe.rs` drives the same fixture directly through the SDK — a
one-shot demonstration with no daemon:

```sh
cargo run --example acp_probe -- tests/fixtures/acp_fake_agent.py "hello"
```

## Deliberately not here yet

- **Resume.** The ACP session id is reported in `AgentStatus`, but nothing yet
  calls `session/load` to reopen it, and the id is not written to the session
  catalog. A follow-up binds it so a card claim reopens the exact conversation.
- **Cancel during a permission prompt.** Cancelling while a permission prompt
  is outstanding does not yet answer it as `Cancelled`; stop does.
- **Attention cleanup.** If an agent dies with a permission prompt unanswered,
  the attention request stays outstanding for the human to dismiss.
- **MSRV.** The SDK requires Rust 1.88; the crate's declared `rust-version`
  (1.82) must move with it.

## Protocol note

`Command` gained `AgentStart`, `AgentPrompt`, `AgentCancel`, `AgentStop`, and
`AgentList`, and `Response` gained `AgentStatus` and `Agents`. The daemon
protocol version is unchanged: the additions are backward compatible, and an old
client keeps working against a new daemon.
