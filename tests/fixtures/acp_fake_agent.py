#!/usr/bin/env python3
"""A minimal ACP v1 agent, for radar's daemon tests.

Speaks newline-delimited JSON-RPC on stdin/stdout, exactly what the
`agent-client-protocol` SDK's stdio transport expects. It is deliberately
small: enough of `initialize`, `session/new`, `session/prompt`,
`session/cancel`, `session/update` and `session/request_permission` to prove
radar's adapter end to end without a real model or network.

Behaviour:
  * a prompt that contains "ask" first asks the client for permission and
    reports the chosen option in its reply;
  * every prompt streams one `agent_message_chunk` ("echo: <prompt>") before
    it answers with `stopReason: end_turn`;
  * `session/load` replays nothing and just reuses the given session id.
"""

import json
import os
import re
import sys
import threading

_next_id = [1]
_lock = threading.Lock()
_write_lock = threading.Lock()
_responses = {}
_response_event = threading.Event()
_cancel_event = threading.Event()
_config_options = [
    {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "model-a", "options": [{"value": "model-a", "name": "Model A"}, {"value": "model-b", "name": "Model B"}]},
    {"id": "thinking", "name": "Thinking", "type": "boolean", "currentValue": False},
]


def modes(current):
    return {"currentModeId": current, "availableModes": [{"id": "review", "name": "Review"}, {"id": "work", "name": "Work"}]}



def send(message):
    with _write_lock:
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()


def reply(request_id, result):
    send({"jsonrpc": "2.0", "id": request_id, "result": result})


def notify(method, params):
    send({"jsonrpc": "2.0", "method": method, "params": params})


def next_id():
    with _lock:
        value = _next_id[0]
        _next_id[0] += 1
        return value


def prompt_text(params):
    parts = []
    for block in params.get("prompt", []):
        if block.get("type") == "text":
            parts.append(block.get("text", ""))
    return " ".join(parts)


def stream(session_id, text):
    notify(
        "session/update",
        {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text},
            },
        },
    )


def wait_for_response(request_id):
    while True:
        with _lock:
            if request_id in _responses:
                outcome = _responses.pop(request_id)
                break
        _response_event.wait(0.05)
        _response_event.clear()
    if not isinstance(outcome, dict):
        return "cancelled"
    # RequestPermissionResponse wraps the tagged outcome under `outcome`.
    inner = outcome.get("outcome")
    if not isinstance(inner, dict) or inner.get("outcome") == "cancelled":
        return "cancelled"
    return str(inner.get("optionId", "unknown"))


def handle_prompt(request_id, params):
    session_id = params["sessionId"]
    text = prompt_text(params)
    stream(session_id, "echo: " + text)
    if "show radar environment" in text:
        stream(session_id, " " + json.dumps({key: os.environ.get(key) for key in ["RADAR_AGENT", "RADAR_HOME", "RADAR_PROJECT_ID", "RADAR_PROJECT_ROOT", "RADAR_CARD_ID", "RADAR_SESSION_ID"]}))
    if "wait for cancellation" in text:
        _cancel_event.clear()
        stream(session_id, " (waiting for cancellation)")
        _cancel_event.wait(20)
        reply(request_id, {"stopReason": "cancelled"})
        return

    if re.search(r"\bask\b", text):
        permission_id = next_id()
        send(
            {
                "jsonrpc": "2.0",
                "id": permission_id,
                "method": "session/request_permission",
                "params": {
                    "sessionId": session_id,
                    "toolCall": {
                        "toolCallId": "tc-1",
                        "title": "Run a risky command",
                    },
                    "options": [
                        {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                        {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
                    ],
                },
            }
        )
        stream(session_id, " (permission: " + wait_for_response(permission_id) + ")")

    reply(request_id, {"stopReason": "end_turn"})


def reader():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if "method" not in message:
            # A response to one of our requests.
            print("RESP " + line, file=sys.stderr, flush=True)
            with _lock:
                _responses[message.get("id")] = message.get("result")
            _response_event.set()
            continue
        method = message["method"]
        request_id = message.get("id")
        params = message.get("params") or {}
        if method == "initialize":
            reply(
                request_id,
                {
                    "protocolVersion": 1,
                    "agentCapabilities": {
                        "loadSession": True,
                        "sessionCapabilities": {"list": {}},
                    },
                    "agentInfo": {"name": "fake-agent", "version": "0.1.0"},
                },
            )
        elif method == "session/new":
            reply(request_id, {"sessionId": "sess_fake_1", "modes": modes("work"), "configOptions": _config_options})
        elif method == "session/list":
            # The current conversation and one created outside Radar.
            reply(
                request_id,
                {
                    "sessions": [
                        {
                            "sessionId": "sess_fake_1",
                            "cwd": params.get("cwd") or "/tmp",
                            "title": "the fake conversation",
                        },
                        {
                            "sessionId": "sess_imported_2",
                            "cwd": params.get("cwd") or "/tmp",
                            "title": "an existing conversation",
                            "updatedAt": "2026-10-04T12:00:00Z",
                        },
                    ]
                },
            )
        elif method == "session/load":
            # Reuse the session and tell the client its restored mode.
            reply(
                request_id,
                {
                    "modes": modes("review"),
                    "configOptions": _config_options,
                },
            )
        elif method == "session/set_mode":
            notify("session/update", {"sessionId": params["sessionId"], "update": {"sessionUpdate": "current_mode_update", "currentModeId": params["modeId"]}})
            reply(request_id, {})
        elif method == "session/set_config_option":
            for option in _config_options:
                if option["id"] == params["configId"]:
                    option["currentValue"] = params["value"]
            reply(request_id, {"configOptions": _config_options})
        elif method == "session/prompt":
            threading.Thread(
                target=handle_prompt, args=(request_id, params), daemon=True
            ).start()
        elif method == "session/cancel":
            _cancel_event.set()
        elif request_id is not None:
            send(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "error": {"code": -32601, "message": "method not found"},
                }
            )


if __name__ == "__main__":
    reader()
