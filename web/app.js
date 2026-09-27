import { Terminal } from "./xterm.mjs";
import { FitAddon } from "./fit-addon.mjs";

const $ = (selector) => document.querySelector(selector);
const projectList = $("#project-list");
const dashboard = $("#dashboard");
const terminalView = $("#terminal-view");
const mainContent = $(".main-content");
const serverState = $("#server-state");
const pageError = $("#page-error");
let projects = [];
let selectedProject = null;
let currentSocket = null;
let terminal = null;
let terminalSession = null;
let fitAddon = null;
let resizeObserver = null;
let refreshTimer = null;
let reconnectTimer = null;
let reconnectAttempts = 0;
let socketGeneration = 0;
let sessionEnded = false;
let applyingRemoteResize = false;
let latestAttentionSnapshot = null;
const answerDrafts = new Map();

async function request(path, options = {}) {
  const url = new URL(path.replace(/^\/+/, ""), document.baseURI);
  const response = await fetch(url, {
    ...options,
    headers: { "content-type": "application/json", ...(options.headers || {}) },
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(body.error || `Request failed (${response.status})`);
  return body;
}

function showError(message) {
  pageError.textContent = message;
  pageError.classList.remove("hidden");
}

function clearError() {
  pageError.classList.add("hidden");
  pageError.textContent = "";
}

function showToast(message) {
  const toast = $("#toast");
  toast.textContent = message;
  toast.classList.remove("hidden");
  window.clearTimeout(showToast.timeout);
  showToast.timeout = window.setTimeout(() => toast.classList.add("hidden"), 2600);
}

function renderProjects(focusProjectId = null) {
  projectList.replaceChildren();
  $("#project-count").textContent = String(projects.length);

  for (const project of projects) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "project-link";
    button.dataset.projectId = String(project.id);
    button.title = project.path;
    button.setAttribute("aria-label", `${project.name}, ${project.path}`);
    if (project.id === selectedProject?.id) {
      button.classList.add("active");
      button.setAttribute("aria-current", "page");
    }

    const symbol = document.createElement("span");
    symbol.className = "project-symbol";
    symbol.setAttribute("aria-hidden", "true");
    symbol.textContent = "⌂";

    const copy = document.createElement("span");
    copy.className = "project-link-copy";
    const name = document.createElement("span");
    name.className = "project-link-name";
    name.textContent = project.name;
    const path = document.createElement("span");
    path.className = "project-link-path";
    path.textContent = project.path;
    copy.append(name, path);
    button.append(symbol, copy);
    projectList.append(button);
  }

  if (!projects.length) {
    const empty = document.createElement("p");
    empty.className = "project-empty";
    empty.textContent = "No projects yet";
    projectList.append(empty);
  }

  if (focusProjectId !== null) {
    projectList.querySelector(`[data-project-id="${focusProjectId}"]`)?.focus({ preventScroll: true });
  }
}

async function loadProjects() {
  clearError();
  projects = await request("/api/projects");
  serverState.classList.add("online");
  serverState.setAttribute("aria-label", "Server connected");

  const preferred = new URLSearchParams(location.search).get("project");
  selectedProject = projects.find((project) => String(project.id) === preferred) || projects[0] || null;
  renderProjects();

  if (!projects.length) {
    $("#project-title").textContent = "No projects yet";
    $("#project-path").textContent = "Add a project in Radar on your computer first.";
    renderSessions([]);
    renderActivity(null);
    renderAttention(null);
    $("#new-shell-button").disabled = true;
    return;
  }
  $("#new-shell-button").disabled = false;
  await loadProjectData();
}

async function loadProjectData() {
  const project = selectedProject;
  if (!project) return;
  $("#project-title").textContent = project.name;
  $("#project-path").textContent = project.path;
  try {
    const [sessions, snapshot] = await Promise.all([
      request(`/api/projects/${project.id}/sessions`),
      request(`/api/projects/${project.id}/activity`),
    ]);
    if (selectedProject?.id !== project.id) return;
    renderSessions(sessions);
    renderAttention(snapshot);
    renderActivity(snapshot);
  } catch (error) {
    if (selectedProject?.id !== project.id) return;
    showError(error.message);
  }
}

function renderSessions(items) {
  const list = $("#session-list");
  const empty = $("#sessions-empty");
  list.replaceChildren();
  empty.classList.toggle("hidden", items.length > 0);
  for (const session of items) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "session-card";
    button.disabled = session.state !== "running";
    const icon = document.createElement("span");
    icon.className = "session-icon";
    icon.textContent = session.slot === "agent" ? "✳" : session.slot === "editor" ? "▤" : "⌘";
    const copy = document.createElement("span");
    copy.className = "session-copy";
    const title = document.createElement("span");
    title.className = "session-title";
    title.textContent = session.title || session.label;
    const subtitle = document.createElement("span");
    subtitle.className = "session-subtitle";
    subtitle.textContent = `${session.label} · ${session.detail || (session.pid ? `PID ${session.pid}` : "Radar session")}`;
    copy.append(title, subtitle);
    const state = document.createElement("span");
    state.className = `session-state ${session.state}`;
    state.textContent = session.state;
    const arrow = document.createElement("span");
    arrow.className = "session-arrow";
    arrow.textContent = "›";
    button.append(icon, copy, state, arrow);
    button.addEventListener("click", () => attach(session));
    list.append(button);
  }
}

function attentionReason(item) {
  if (item.card_id) return `Card ${item.card_id} · ${item.kind}`;
  if (item.session_id) return `${item.session_id} · ${item.kind}`;
  return item.kind;
}

function renderAttention(snapshot) {
  latestAttentionSnapshot = snapshot;
  const list = $("#attention-list");
  if (list.contains(document.activeElement)) return;
  const empty = $("#attention-empty");
  const items = snapshot?.attention?.filter((item) => item.resolved_at_millis == null) || [];
  const unresolvedIds = new Set(items.map((item) => item.id));
  for (const answer of list.querySelectorAll("textarea[data-attention-id]")) {
    if (unresolvedIds.has(answer.dataset.attentionId)) {
      answerDrafts.set(answer.dataset.attentionId, answer.value);
    }
  }
  for (const id of answerDrafts.keys()) {
    if (!unresolvedIds.has(id)) answerDrafts.delete(id);
  }
  list.replaceChildren();
  empty.classList.toggle("hidden", items.length > 0);
  const count = $("#attention-count");
  count.textContent = String(items.length);
  count.classList.toggle("hidden", items.length === 0);

  for (const item of items) {
    const card = document.createElement("article");
    card.className = "attention-card";
    const title = document.createElement("h3");
    title.textContent = item.reason;
    const meta = document.createElement("div");
    meta.className = "attention-meta";
    meta.textContent = attentionReason(item);
    const actions = document.createElement("div");
    actions.className = "attention-actions";
    const allowed = new Set(item.allowed_actions || []);

    if (allowed.has("answer")) {
      const form = document.createElement("form");
      form.className = "answer-box";
      const answer = document.createElement("textarea");
      answer.dataset.attentionId = item.id;
      answer.placeholder = "Write a reply…";
      answer.setAttribute("aria-label", "Reply to agent");
      answer.rows = 1;
      answer.value = answerDrafts.get(item.id) || "";
      answer.addEventListener("input", () => answerDrafts.set(item.id, answer.value));
      const send = document.createElement("button");
      send.className = "small-button";
      send.type = "submit";
      send.textContent = "Reply";
      form.append(answer, send);
      form.addEventListener("submit", async (event) => {
        event.preventDefault();
        await resolveAttention(item, "answer", answer.value);
      });
      actions.append(form);
    }
    for (const action of ["approve", "deny", "dismiss"]) {
      if (!allowed.has(action)) continue;
      const button = document.createElement("button");
      button.type = "button";
      button.className = action === "approve" ? "primary-button compact" : "small-button";
      button.textContent = action[0].toUpperCase() + action.slice(1);
      button.addEventListener("click", () => resolveAttention(item, action));
      actions.append(button);
    }
    card.append(title, meta, actions);
    list.append(card);
  }
}

async function resolveAttention(item, action, answer) {
  const focused = document.activeElement;
  if (focused && $("#attention-list").contains(focused)) focused.blur();
  try {
    const payload = { revision: item.revision, action };
    if (answer !== undefined) payload.answer = answer;
    await request(`/api/projects/${selectedProject.id}/attention/${encodeURIComponent(item.id)}`, {
      method: "POST",
      body: JSON.stringify(payload),
    });
    answerDrafts.delete(item.id);
    showToast("Response sent");
    await loadProjectData();
  } catch (error) {
    showToast(error.message);
  }
}

function eventText(event) {
  const payload = event.payload?.data || {};
  switch (event.kind) {
    case "agent_state_changed":
      return `Agent ${payload.state || "changed state"}${payload.message ? ` · ${payload.message}` : ""}`;
    case "reported": return payload.text || "Agent update";
    case "attention_requested": return `Needs attention · ${payload.reason || "Agent request"}`;
    case "attention_resolved": return `Request resolved · ${payload.request_id || ""}`;
    case "session_lifecycle": return `Session ${payload.state || "updated"}${payload.detail ? ` · ${payload.detail}` : ""}`;
    case "board_changed": return `Board updated${payload.title ? ` · ${payload.title}` : ""}`;
    case "command_result": return payload.detail || "Command completed";
    default: return event.kind.replaceAll("_", " ");
  }
}

function relativeTime(millis) {
  const seconds = Math.max(0, Math.floor((Date.now() - millis) / 1000));
  if (seconds < 60) return "just now";
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
  return `${Math.floor(seconds / 86400)}d ago`;
}

function renderActivity(snapshot) {
  const list = $("#activity-list");
  const empty = $("#activity-empty");
  const events = snapshot?.events?.slice(-8).reverse() || [];
  list.replaceChildren();
  empty.classList.toggle("hidden", events.length > 0);
  for (const event of events) {
    const item = document.createElement("div");
    item.className = "activity-card";
    const marker = document.createElement("span");
    marker.className = "activity-marker";
    const copy = document.createElement("div");
    copy.className = "activity-copy";
    const text = document.createElement("p");
    text.className = "activity-text";
    text.textContent = eventText(event);
    const time = document.createElement("time");
    time.className = "activity-time";
    time.textContent = relativeTime(event.at_millis);
    copy.append(text, time);
    item.append(marker, copy);
    list.append(item);
  }
}

function terminalInputFilter(data) {
  return data
    .replace(/\x1b\[[0-9;?]*[Rcn]/g, "")
    .replace(/\x1b\][^\x07]*(?:\x07|\x1b\\)/gs, "")
    .replace(/\x1bP[\s\S]*?\x1b\\/g, "");
}

function sendTerminalInput(data) {
  if (!currentSocket || currentSocket.readyState !== WebSocket.OPEN) return;
  const filtered = terminalInputFilter(data);
  if (filtered) currentSocket.send(new TextEncoder().encode(filtered));
}

function sendTerminalBytes(bytes) {
  if (currentSocket?.readyState === WebSocket.OPEN && bytes.length) currentSocket.send(bytes);
}

function attach(session) {
  if (!selectedProject) return;
  clearError();
  dashboard.classList.add("hidden");
  terminalView.classList.remove("hidden");
  mainContent.classList.add("terminal-open");
  $("#terminal-title").textContent = session.title || session.label;
  setTerminalStatus("Connecting…");

  socketGeneration += 1;
  currentSocket?.close();
  currentSocket = null;
  if (terminal) terminal.dispose();
  terminalSession = session;
  sessionEnded = false;
  reconnectAttempts = 0;
  window.clearTimeout(reconnectTimer);
  reconnectTimer = null;
  terminal = new Terminal({
    cursorBlink: true,
    convertEol: false,
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
    fontSize: 14,
    scrollback: 5000,
    theme: {
      background: "#0d100d",
      foreground: "#e3e8df",
      cursor: "#b7d977",
      selectionBackground: "#65754e77",
      black: "#171a17",
      red: "#e78376",
      green: "#b7d977",
      yellow: "#e8c474",
      blue: "#8eacd0",
      magenta: "#c1a0cc",
      cyan: "#88c3b2",
      white: "#dce2d7",
      brightBlack: "#737d70",
      brightRed: "#f09387",
      brightGreen: "#c9eb8b",
      brightYellow: "#f1d28a",
      brightBlue: "#a6c2e4",
      brightMagenta: "#d6b4e2",
      brightCyan: "#a0d9c8",
      brightWhite: "#f4f7ef",
    },
  });
  fitAddon = new FitAddon();
  terminal.loadAddon(fitAddon);
  terminal.open($("#terminal"));
  fitAddon.fit();
  terminal.focus();

  terminal.onData(sendTerminalInput);
  terminal.onBinary((data) => sendTerminalBytes(Uint8Array.from(data, (char) => char.charCodeAt(0))));
  terminal.onResize(({ cols, rows }) => {
    if (!applyingRemoteResize) sendTerminalResize(cols, rows);
  });
  for (const button of document.querySelectorAll("[data-key]")) {
    button.onclick = () => {
      terminal?.focus();
      const key = {
        esc: "\x1b",
        tab: "\t",
        "ctrl-c": "\x03",
        "ctrl-d": "\x04",
        up: "\x1b[A",
        down: "\x1b[B",
        left: "\x1b[D",
        right: "\x1b[C",
      }[button.dataset.key];
      if (key) sendTerminalInput(key);
    };
  }
  resizeObserver?.disconnect();
  resizeObserver = new ResizeObserver(() => requestAnimationFrame(() => fitAddon?.fit()));
  resizeObserver.observe($(".terminal-frame"));
  window.addEventListener("resize", fitTerminal);
  window.visualViewport?.addEventListener("resize", fitTerminal);
  connectTerminal(session, false);
}

function connectTerminal(session, reset) {
  if (reset) terminal?.reset();
  sessionEnded = false;
  const socketUrl = new URL(
    `api/projects/${selectedProject.id}/sessions/${encodeURIComponent(session.id)}/terminal`,
    document.baseURI,
  );
  socketUrl.protocol = location.protocol === "https:" ? "wss:" : "ws:";
  const generation = ++socketGeneration;
  const socket = new WebSocket(socketUrl.toString());
  currentSocket = socket;
  socket.binaryType = "arraybuffer";
  socket.addEventListener("open", () => {
    if (generation !== socketGeneration) return;
    setTerminalStatus("Connected", true);
    fitAddon?.fit();
    sendTerminalResize(terminal?.cols, terminal?.rows);
  });
  socket.addEventListener("message", (message) => {
    if (generation !== socketGeneration) return;
    if (message.data instanceof ArrayBuffer) {
      terminal?.write(new Uint8Array(message.data));
      return;
    }
    try {
      const status = JSON.parse(message.data);
      if (status.type === "error") setTerminalStatus(status.message, false, true);
      if (status.type === "resync_required") setTerminalStatus("Refreshing terminal…", false, true);
      if (status.type === "resize") applyRemoteResize(status.cols, status.rows);
      if (status.type === "closed") {
        sessionEnded = true;
        terminalSession = null;
        setTerminalStatus("Session ended", false);
      }
    } catch {
      setTerminalStatus(String(message.data), false, true);
    }
  });
  socket.addEventListener("close", () => {
    if (generation !== socketGeneration) return;
    currentSocket = null;
    if (sessionEnded || !terminalSession) {
      if (sessionEnded) setTerminalStatus("Session ended", false);
      return;
    }
    void retryOrFinishSession(generation);
  });
  socket.addEventListener("error", () => {
    if (generation === socketGeneration) setTerminalStatus("Connection interrupted…", false, true);
  });
}

function sendTerminalResize(cols, rows) {
  if (currentSocket?.readyState === WebSocket.OPEN && cols && rows) {
    currentSocket.send(JSON.stringify({ type: "resize", cols, rows }));
  }
}

function applyRemoteResize(cols, rows) {
  if (!Number.isInteger(cols) || !Number.isInteger(rows) || cols < 2 || rows < 1) return;
  applyingRemoteResize = true;
  try {
    terminal?.resize(cols, rows);
  } finally {
    applyingRemoteResize = false;
  }
}

function scheduleReconnect() {
  if (reconnectTimer || !terminalSession || sessionEnded) return;
  const delay = Math.min(1000 * 2 ** reconnectAttempts, 8000);
  reconnectAttempts += 1;
  setTerminalStatus(`Reconnecting in ${Math.ceil(delay / 1000)}s…`, false, true);
  reconnectTimer = window.setTimeout(() => {
    reconnectTimer = null;
    if (terminalSession && !sessionEnded) connectTerminal(terminalSession, true);
  }, delay);
}

async function retryOrFinishSession(generation) {
  const session = terminalSession;
  if (!session) return;
  try {
    const sessions = await request(`/api/projects/${selectedProject.id}/sessions`);
    const current = sessions.find((item) => item.id === session.id);
    if (!current || current.state !== "running") {
      terminalSession = null;
      sessionEnded = true;
      setTerminalStatus(current?.state === "failed" ? "Session unavailable" : "Session ended", false, true);
      return;
    }
  } catch {
    // A temporary HTTP outage is handled by the regular reconnect backoff.
  }
  if (generation === socketGeneration && terminalSession) scheduleReconnect();
}

function reconnectNow() {
  if (!terminalSession) return;
  window.clearTimeout(reconnectTimer);
  reconnectTimer = null;
  reconnectAttempts = 0;
  const session = terminalSession;
  currentSocket?.close();
  currentSocket = null;
  connectTerminal(session, true);
}

function setTerminalStatus(message, connected = false, error = false) {
  const status = $("#terminal-connection");
  status.textContent = message;
  status.classList.toggle("connected", connected);
  status.classList.toggle("terminal-error", error);
}

function fitTerminal() {
  window.setTimeout(() => {
    fitAddon?.fit();
    terminal?.focus();
  }, 60);
}

function detach() {
  terminalSession = null;
  sessionEnded = false;
  socketGeneration += 1;
  window.clearTimeout(reconnectTimer);
  reconnectTimer = null;
  resizeObserver?.disconnect();
  resizeObserver = null;
  window.removeEventListener("resize", fitTerminal);
  window.visualViewport?.removeEventListener("resize", fitTerminal);
  if (currentSocket) {
    currentSocket.close();
    currentSocket = null;
  }
  terminal?.dispose();
  terminal = null;
  fitAddon = null;
  terminalView.classList.add("hidden");
  mainContent.classList.remove("terminal-open");
  dashboard.classList.remove("hidden");
  loadProjectData().catch((error) => showError(error.message));
}

async function createShell() {
  const project = selectedProject;
  if (!project) return;
  const button = $("#new-shell-button");
  button.disabled = true;
  try {
    const session = await request(`/api/projects/${project.id}/sessions`, {
      method: "POST",
      body: "{}",
    });
    if (selectedProject?.id === project.id) attach(session);
  } catch (error) {
    showError(error.message);
  } finally {
    button.disabled = false;
  }
}

projectList.addEventListener("click", (event) => {
  const button = event.target.closest("[data-project-id]");
  if (!button) return;
  const project = projects.find((item) => item.id === Number(button.dataset.projectId));
  if (!project) return;

  const alreadySelected = selectedProject?.id === project.id;
  selectedProject = project;
  renderProjects(project.id);
  const projectUrl = new URL(document.baseURI);
  projectUrl.searchParams.set("project", String(project.id));
  history.replaceState(null, "", projectUrl);
  if (!terminalView.classList.contains("hidden")) {
    detach();
  } else if (!alreadySelected) {
    clearError();
    loadProjectData().catch((error) => showError(error.message));
  }
});
$("#attention-list").addEventListener("focusout", () => {
  window.setTimeout(() => {
    if (!$("#attention-list").contains(document.activeElement) && latestAttentionSnapshot) {
      renderAttention(latestAttentionSnapshot);
    }
  }, 0);
});
$("#new-shell-button").addEventListener("click", createShell);
$("#refresh-button").addEventListener("click", () => loadProjectData().catch((error) => showError(error.message)));
$("#back-button").addEventListener("click", detach);
$("#detach-button").addEventListener("click", detach);
$("#reconnect-button").addEventListener("click", reconnectNow);
window.addEventListener("beforeunload", () => currentSocket?.close());

loadProjects().catch((error) => {
  serverState.classList.remove("online");
  serverState.setAttribute("aria-label", "Server unavailable");
  showError(error.message);
});
refreshTimer = window.setInterval(() => {
  if (!dashboard.classList.contains("hidden")) {
    loadProjectData().catch((error) => showError(error.message));
  }
}, 5000);
void refreshTimer;
