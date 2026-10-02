#!/usr/bin/env python3
"""Real provider identity/resume acceptance; requires all four CLIs and Cursor login.

Run after cargo build: python scripts/provider-resume-smoke.py
Uses a private HOME/daemon, copies Cursor's existing login privately, and blocks
Cursor's test prompt before any model request. Pi/OMP use RPC without prompts.
"""
import json
import os
from pathlib import Path
import shutil
import socket
import struct
import subprocess
import tempfile
import threading
import time
import uuid


class Harness:
    def __init__(self, home, binary):
        self.home = home
        self.binary = binary
        self.data = home / "data"
        self.project = home / "project"
        self.project.mkdir()
        (home / "bin").mkdir()
        (home / "bin/radar").symlink_to(binary)
        self.env = {k: v for k, v in os.environ.items() if not k.startswith(("RADAR_", "PI_"))}
        self.env.update(HOME=str(home), RADAR_HOME=str(self.data),
                        PATH=f"{home / 'bin'}:{self.env['PATH']}",
                        XDG_CONFIG_HOME=str(home / ".config"),
                        XDG_DATA_HOME=str(home / ".local/share"))
        self.run([binary, "add", str(self.project)])
        self.run([binary, "setup"])
        self.log = (home / "daemon.log").open("w")
        self.start()

    def run(self, args):
        result = subprocess.run(args, cwd=self.project, env=self.env, capture_output=True, text=True)
        assert result.returncode == 0, result.stderr
        return result.stdout

    def start(self):
        self.daemon = subprocess.Popen([self.binary, "serve"], env=self.env,
                                       stdout=self.log, stderr=self.log)
        self.wait(lambda: self.request("Ping") == {"Hello": {"version": 4}})

    def stop(self):
        self.request("Shutdown")
        self.daemon.wait(timeout=10)

    def restart(self):
        self.stop()
        self.start()

    def connect(self, command):
        stream = socket.socket(socket.AF_UNIX)
        stream.settimeout(15)
        stream.connect(str(self.data / "run/sessions.sock"))
        body = json.dumps({"version": 4, "command": command}).encode()
        stream.sendall(struct.pack(">I", len(body)) + body)
        return stream

    @staticmethod
    def read(stream, size):
        result = b""
        while len(result) < size:
            chunk = stream.recv(size - len(result))
            assert chunk, "provider stream closed"
            result += chunk
        return result

    def receive(self, stream):
        size = struct.unpack(">I", self.read(stream, 4))[0]
        return json.loads(self.read(stream, size))

    def request(self, command):
        with self.connect(command) as stream:
            response = self.receive(stream)
        assert "Error" not in response, response
        return response

    def wait(self, check, seconds=30):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            try:
                result = check()
                if result:
                    return result
            except (OSError, KeyError):
                pass
            time.sleep(0.1)
        raise AssertionError("provider acceptance timed out")

    def rows(self, provider):
        response = self.request({"CatalogList": {"projects": [{"id": 1, "path": str(self.project)}],
                                 "filter": "all", "query": None, "limit": 1000}})
        return [row for row in response["Catalog"] if row["provider"] == provider]

    def active(self, provider, runtime, conversation=None):
        return next((row["provider_session_id"] for row in self.rows(provider)
                     if row["radar_session_id"] == runtime and row["provider_session_id"] != runtime
                     and (conversation is None or row["provider_session_id"] == conversation)), None)

    def spec(self, provider, argv, extra=None):
        runtime = f"project-1-agent-0-{provider}"
        env = {key: self.env[key] for key in ["HOME", "RADAR_HOME", "PATH", "XDG_CONFIG_HOME", "XDG_DATA_HOME"]}
        env.update(RADAR_AGENT=f"{provider}-smoke", RADAR_SESSION_PROVIDER=provider,
                   RADAR_PROJECT_ID="1", RADAR_SESSION_ID=runtime, RADAR_CARD_ID="task")
        env.update(extra or {})
        return {"id": runtime, "argv": argv, "cwd": str(self.project), "env": list(env.items()),
                "env_remove": [], "dims": {"cols": 160, "rows": 40}}

    def spawn(self, spec):
        self.request({"Create": spec})
        stream = self.connect({"Attach": {"id": spec["id"]}})
        self.receive(stream)
        return stream

    def input(self, runtime, text):
        self.request({"Input": {"id": runtime, "bytes": list(text.encode())}})

    def forget(self, runtime, stream):
        self.request({"Stop": {"id": runtime}})
        stream.close()
        self.wait(lambda: self.ended(runtime))
        self.request({"Forget": {"id": runtime}})

    def ended(self, runtime):
        status = next(row for row in self.request("List")["Sessions"] if row["id"] == runtime)
        return status["lifecycle"] != "Running" and status["stream_closed"]

    def rpc(self, stream, runtime, command):
        self.input(runtime, json.dumps(command) + "\n")
        text = ""
        while True:
            frame = self.receive(stream)
            event = frame.get("Output", {}).get("event", {})
            assert event != "Closed", "provider exited before RPC response"
            if isinstance(event, dict) and "Bytes" in event:
                text += bytes(event["Bytes"]).decode(errors="replace")
            for line in text.splitlines():
                try:
                    response = json.loads(line)
                except ValueError:
                    continue
                if response.get("type") == "response" and response.get("command") == command["type"]:
                    assert response["success"], response
                    return response

    def collect(self, stream):
        chunks = []
        def read_output():
            try:
                while True:
                    frame = self.receive(stream)
                    event = frame.get("Output", {}).get("event", {})
                    if isinstance(event, dict) and "Bytes" in event:
                        chunks.append(bytes(event["Bytes"]).decode(errors="replace"))
            except (OSError, AssertionError):
                return
        threading.Thread(target=read_output, daemon=True).start()
        return chunks


def opencode(h):
    now = int(time.time() * 1000)
    for name in ["initial", "original"]:
        fixture = {"info": {"id": f"ses_smoke_{name}", "projectID": "global", "directory": str(h.project),
                            "location": {"directory": str(h.project)}, "title": f"{name.title()} smoke task",
                            "agent": "build", "model": {"id": "gpt-4o", "providerID": "openai"},
                            "cost": 0, "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}},
                            "outcome": "succeeded", "time": {"created": now, "updated": now}},
                   "messages": [{"id": f"msg_{name}", "time": {"created": now}, "text": f"{name}-work-sentinel",
                                 "files": [], "agents": [], "type": "user"}]}
        path = h.home / "opencode-fixture.json"
        path.write_text(json.dumps(fixture))
        h.run(["opencode", "session", "import", "--standalone", "--directory", str(h.project), str(path)])
    spec = h.spec("opencode", [shutil.which("opencode"), "--standalone", "--session", "ses_smoke_initial"],
                  {"RADAR_RESUME_SESSION_ID": "ses_smoke_initial"})
    stream = h.spawn(spec)
    chunks = h.collect(stream)
    h.wait(lambda: h.active("opencode", spec["id"], "ses_smoke_initial"))
    time.sleep(1)
    # Dismiss onboarding; command palette -> session picker -> original task.
    for text, pause in [("\x1b", .3), ("\x10", .3), ("Switch session", .3), ("\r", .7),
                        ("Original smoke task", .3), ("\r", .3)]:
        h.input(spec["id"], text)
        time.sleep(pause)
    h.wait(lambda: h.active("opencode", spec["id"], "ses_smoke_original"))
    h.wait(lambda: "original-work-sentinel" in "".join(chunks))
    h.forget(spec["id"], stream)
    h.restart()
    spec["argv"][-1] = "ses_smoke_original"
    spec["env"] = [(key, "ses_smoke_original" if key == "RADAR_RESUME_SESSION_ID" else value)
                   for key, value in spec["env"]]
    stream = h.spawn(spec)
    chunks = h.collect(stream)
    h.wait(lambda: h.active("opencode", spec["id"], "ses_smoke_original"))
    h.wait(lambda: "original-work-sentinel" in "".join(chunks))
    h.forget(spec["id"], stream)
    print("PASS OpenCode: real picker switch -> daemon restart -> original messages restored", flush=True)


def cursor(h):
    config = os.environ.get("CURSOR_CONFIG_DIR")
    if not config:
        config = str(Path(os.environ["XDG_CONFIG_HOME"]) / "cursor") if os.environ.get("XDG_CONFIG_HOME") else str(Path.home() / ".cursor")
    auth = Path(config) / "auth.json"
    target = h.home / ".config/cursor/auth.json"
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(auth, target)
    target.chmod(0o600)
    settings = h.home / ".cursor/hooks.json"
    hooks = json.loads(settings.read_text())
    hooks["hooks"]["beforeSubmitPrompt"].append({"command": "printf '%s' '{\"continue\":false,\"user_message\":\"Smoke: no model request\"}'"})
    settings.write_text(json.dumps(hooks))
    spec = h.spec("cursor-agent", [shutil.which("cursor-agent"), "--trust"])
    stream = h.spawn(spec)
    h.collect(stream)
    initial = h.wait(lambda: h.active("cursor-agent", spec["id"]))
    time.sleep(2)
    # Cursor reports /new's identity before the next prompt, not on navigation.
    for text, pause in [("/new", .25), ("\r", .7), ("blocked-local-only", .25), ("\r", .25)]:
        h.input(spec["id"], text)
        time.sleep(pause)
    h.wait(lambda: (current := h.active("cursor-agent", spec["id"])) and current != initial)
    h.forget(spec["id"], stream)
    print("PASS Cursor: real session/prompt hooks track /new before model execution", flush=True)


def pi_family(h, provider):
    root = h.home / f"{provider}-history"
    root.mkdir()
    ids = [str(uuid.uuid4()), str(uuid.uuid4())]
    files = []
    for i, conversation in enumerate(ids):
        entries = [{"type": "session", "version": 3, "id": conversation, "timestamp": "2026-10-02T09:00:00.000Z", "cwd": str(h.project)},
                   {"type": "message", "id": "user0001", "parentId": None, "timestamp": "2026-10-02T09:01:00.000Z",
                    "message": {"role": "user", "content": "original-work-sentinel" if i else "initial-session", "timestamp": 1790931660000}}]
        path = root / f"2026-10-02T09-00-00-000Z_{conversation}.jsonl"
        path.write_text("".join(json.dumps(entry) + "\n" for entry in entries))
        files.append(path)
    hook = h.home / (".pi/agent/extensions/radar-session.js" if provider == "pi" else ".omp/agent/extensions/radar-session.ts")
    argv = [shutil.which(provider), "--mode", "rpc", "--no-extensions", "--no-skills", "--session-dir", str(root), "-e", str(hook)]
    argv += ["--offline", "--no-context-files", "--session-id", ids[0]] if provider == "pi" else ["--no-lsp", "--no-pty", f"--resume={files[0]}"]
    extra = {"OPENAI_API_KEY": "smoke-not-a-real-key"} if provider == "omp" else {}
    spec = h.spec(provider, argv, extra)
    stream = h.spawn(spec)
    h.wait(lambda: h.active(provider, spec["id"], ids[0]))
    h.rpc(stream, spec["id"], {"type": "switch_session", "sessionPath": str(files[1])})
    h.wait(lambda: h.active(provider, spec["id"], ids[1]))
    assert "original-work-sentinel" in json.dumps(h.rpc(stream, spec["id"], {"type": "get_messages"}))
    h.forget(spec["id"], stream)
    h.restart()
    spec["argv"] = argv[:-2] + ["--session", ids[1]] if provider == "pi" else argv[:-1] + [f"--resume={ids[1]}"]
    spec["env"].append(("RADAR_RESUME_SESSION_ID", ids[1]))
    stream = h.spawn(spec)
    h.wait(lambda: h.active(provider, spec["id"], ids[1]))
    assert "original-work-sentinel" in json.dumps(h.rpc(stream, spec["id"], {"type": "get_messages"}))
    h.forget(spec["id"], stream)
    print(f"PASS {provider}: real RPC switch -> daemon restart -> original messages restored", flush=True)


if __name__ == "__main__":
    binary = str(Path("target/debug/radar").resolve())
    for provider in ["opencode", "cursor-agent", "pi", "omp"]:
        assert shutil.which(provider), f"Missing provider: {provider}"
    with tempfile.TemporaryDirectory(prefix="radar-resume-acceptance-") as temporary:
        h = Harness(Path(temporary), binary)
        try:
            opencode(h)
            cursor(h)
            pi_family(h, "pi")
            pi_family(h, "omp")
        finally:
            try:
                h.stop()
            finally:
                h.log.close()
