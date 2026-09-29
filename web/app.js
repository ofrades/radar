import { Terminal } from "./xterm.mjs";
import { FitAddon } from "./fit-addon.mjs";

const $ = (selector) => document.querySelector(selector);
const projectList = $("#project-list");
const dashboard = $("#dashboard");
const terminalView = $("#terminal-view");
const mainContent = $(".main-content");
const appShell = $(".app-shell");
const serverState = $("#server-state");
const pageError = $("#page-error");
let projects = [];
let projectSessions = new Map();
let expandedProjects = new Set();
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
let applyingRemoteResize = false;
let latestAttentionSnapshot = null;
const answerDrafts = new Map();
const projectProgress = new Map();
const pendingResponses = new Set();
let refreshInFlight = null;
const noticedAttention = new Set();
const initializedAttention = new Set();

function syncAppViewport() {
  const viewport = window.visualViewport;
  if (!viewport) return;
  appShell.style.setProperty("--visual-viewport-height", `${viewport.height}px`);
  appShell.style.setProperty("--visual-viewport-offset-top", `${viewport.offsetTop}px`);
}

syncAppViewport();
window.addEventListener("resize", syncAppViewport);
window.visualViewport?.addEventListener("resize", syncAppViewport);
window.visualViewport?.addEventListener("scroll", syncAppViewport);

function updateAttentionShortcut() {
  let count = 0;
  for (const project of projects) {
    const snapshot = projectProgress.get(project.id)?.snapshot;
    if (!snapshot) continue;
    const unresolved = snapshot.attention.filter((item) => item.resolved_at_millis == null);
    count += unresolved.length;
    const fresh = unresolved.filter((item) => !noticedAttention.has(item.id) && item.seen_at_millis == null);
    if (initializedAttention.has(project.id) && fresh.length) {
      showToast(`${project.name}: ${fresh.length} new request${fresh.length === 1 ? "" : "s"} need your attention`);
    }
    for (const item of unresolved) noticedAttention.add(item.id);
    initializedAttention.add(project.id);
  }
  for (const selector of ["#attention-shortcut", "#terminal-attention-shortcut"]) {
    const button = $(selector);
    button.classList.toggle("hidden", count === 0);
    button.textContent = `${count} need you`;
    button.setAttribute("aria-label", `${count} unresolved requests. Open project attention.`);
  }
}

async function openAttention() {
  const project = projects.find((item) => projectProgress.get(item.id)?.snapshot?.attention.some((request) => request.resolved_at_millis == null));
  if (!project) return;
  selectedProject = project;
  expandedProjects.add(project.id);
  const url = new URL(document.baseURI);
  url.searchParams.set("project", String(project.id));
  history.replaceState(null, "", url);
  if (!terminalView.classList.contains("hidden")) detach();
  await loadProjectData();
  $("#attention-section").scrollIntoView({ block: "start" });
  $("#attention-list button, #attention-list textarea")?.focus({ preventScroll: true });
}

function boardProgress(board) {
  const columns = board?.columns || [];
  const total = columns.reduce((sum, column) => sum + column.cards.length, 0);
  const count = (name) => columns.filter((column) => column.name.trim().toLowerCase() === name)
    .reduce((sum, column) => sum + column.cards.length, 0);
  return { total, done: count("done"), active: count("in progress"), review: count("review") };
}

function agentProgress(projectId, session) {
  const snapshot = projectProgress.get(projectId)?.snapshot;
  const waiting = snapshot?.attention?.filter((item) =>
    item.session_id === session.id && item.resolved_at_millis == null) || [];
  if (waiting.length) return `${waiting.length} awaiting response`;
  if (session.state !== "running") return session.state;
  const event = snapshot?.events?.findLast((item) =>
    item.session_id === session.id && item.kind === "agent_state_changed");
  return event ? `${event.payload.data.state.replaceAll("_", " ")} · ${relativeTime(event.at_millis)}` : "Activity unknown";
}

function agentWork(projectId, session) {
  if (!session.claim) return "";
  const columns = projectProgress.get(projectId)?.board?.columns || [];
  return columns.flatMap((column) => {
    const count = column.cards.filter((card) => card.claimed_by === session.claim).length;
    return count ? [`${count} ${column.name.toLowerCase()}`] : [];
  }).join(" · ");
}

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
  const focused = projectList.contains(document.activeElement) ? { ...document.activeElement.dataset } : null;
  projectList.replaceChildren();
  $("#project-count").textContent = String(projects.length);

  for (const project of projects) {
    const entry = document.createElement("div");
    entry.className = "project-entry";
    const projectId = String(project.id);
    const sessions = projectSessions.get(project.id) || [];
    const agents = sessions.filter((session) => session.slot === "agent");
    const expanded = expandedProjects.has(project.id);

    const row = document.createElement("div");
    row.className = "project-row";
    const button = document.createElement("button");
    button.type = "button";
    button.className = "project-link";
    button.dataset.projectId = projectId;
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
    const progress = projectProgress.get(project.id);
    const summary = document.createElement("span");
    summary.className = "project-progress";
    if (progress?.snapshot) {
      const { total, done, active, review } = boardProgress(progress.board);
      const waiting = progress.snapshot.attention.filter((item) => item.resolved_at_millis == null).length;
      summary.textContent = `${progress.stale ? "Offline · " : ""}${progress.board ? `${done}/${total} done${active ? ` · ${active} in progress` : ""}${review ? ` · ${review} review` : ""}` : "No board"}${waiting ? ` · ${waiting} need you` : ""}`;
      summary.classList.toggle("needs-attention", waiting > 0);
      button.title += `\n${summary.textContent}`;
    } else {
      summary.textContent = progress?.stale ? "Progress unavailable" : "Loading progress…";
    }
    copy.append(summary);

    const toggle = document.createElement("button");
    toggle.type = "button";
    toggle.className = "project-toggle";
    toggle.dataset.toggleProjectId = projectId;
    toggle.setAttribute("aria-expanded", String(expanded));
    toggle.setAttribute("aria-label", `${expanded ? "Collapse" : "Expand"} agents for ${project.name}`);
    toggle.textContent = expanded ? "⌄" : "›";
    row.append(button, toggle);
    entry.append(row);

    if (expanded) {
      const agentList = document.createElement("div");
      agentList.className = "project-agents";
      agentList.setAttribute("aria-label", `Agents in ${project.name}`);
      if (agents.length) {
        for (const agent of agents) {
          const agentButton = document.createElement("button");
          agentButton.type = "button";
          agentButton.className = "project-agent";
          agentButton.dataset.sessionId = agent.id;
          agentButton.dataset.agentProjectId = projectId;
          agentButton.disabled = agent.attachable === false;
          agentButton.title = agentButton.disabled ? "Ended conversation — reopen from Radar desktop" : `Open session: ${agent.title || agent.program || agent.label}`;
          const status = document.createElement("span");
          status.className = `project-agent-status ${agent.state}`;
          status.setAttribute("aria-hidden", "true");
          const label = document.createElement("span");
          label.className = "project-agent-name";
          label.textContent = agent.title || agent.program || agent.label;
          const agentCopy = document.createElement("span");
          agentCopy.className = "project-agent-copy";
          const progressLabel = document.createElement("span");
          progressLabel.className = "project-agent-progress";
          progressLabel.textContent = agentProgress(project.id, agent);
          agentCopy.append(label, progressLabel);
          const work = agentWork(project.id, agent);
          if (work) {
            const workLabel = document.createElement("span");
            workLabel.className = "project-agent-progress";
            workLabel.textContent = work;
            agentCopy.append(workLabel);
          }
          agentButton.append(status, agentCopy);
          agentList.append(agentButton);
        }
      } else {
        const empty = document.createElement("span");
        empty.className = "project-agent-empty";
        empty.textContent = "No agents";
        agentList.append(empty);
      }
      entry.append(agentList);
    }
    projectList.append(entry);
  }

  if (!projects.length) {
    const empty = document.createElement("p");
    empty.className = "project-empty";
    empty.textContent = "No projects yet";
    projectList.append(empty);
  }

  if (focusProjectId !== null) {
    projectList.querySelector(`[data-project-id="${focusProjectId}"]`)?.focus({ preventScroll: true });
  } else if (focused) {
    const key = focused.sessionId ? "sessionId" : focused.toggleProjectId ? "toggleProjectId" : "projectId";
    [...projectList.querySelectorAll("button")].find((button) =>
      button.dataset[key] === focused[key] && (key !== "sessionId" || button.dataset.agentProjectId === focused.agentProjectId))?.focus({ preventScroll: true });
  }
}

async function loadProjects() {
  clearError();
  projects = await request("/api/projects");
  serverState.classList.add("online");
  serverState.setAttribute("aria-label", "Server connected");

  const preferred = new URLSearchParams(location.search).get("project");
  selectedProject = projects.find((project) => String(project.id) === preferred) || projects[0] || null;
  if (selectedProject) expandedProjects.add(selectedProject.id);
  renderProjects();

  if (!projects.length) {
    $("#project-title").textContent = "No projects yet";
    $("#project-path").textContent = "Add a project in Radar on your computer first.";
    renderSessions([]);
    renderActivity(null);
    renderAttention(null);
    renderBoard(null);
    $("#new-shell-button").disabled = true;
    return;
  }
  $("#new-shell-button").disabled = false;
  await loadProjectData();
}

async function refreshProjectSessions() {
  if (refreshInFlight) return refreshInFlight;
  refreshInFlight = (async () => {
    const results = await Promise.all(projects.map(async (project) => {
      try {
        const [sessions, snapshot, board] = await Promise.all([
          request(`/api/projects/${project.id}/sessions`),
          request(`/api/projects/${project.id}/activity`),
          request(`/api/projects/${project.id}/board`),
        ]);
        projectSessions.set(project.id, sessions);
        projectProgress.set(project.id, { snapshot, board, stale: false });
        return true;
      } catch (error) {
        const progress = projectProgress.get(project.id);
        if (progress) progress.stale = true;
        else projectProgress.set(project.id, { stale: true });
        if (selectedProject?.id === project.id) showError(`Progress unavailable: ${error.message}`);
        return false;
      }
    }));
    const online = results.every(Boolean);
    serverState.classList.toggle("online", online);
    serverState.setAttribute("aria-label", online ? "Progress up to date" : "Progress unavailable — retrying");
    if (online) clearError();
    updateAttentionShortcut();
    renderProjects();
  })().finally(() => { refreshInFlight = null; });
  return refreshInFlight;
}

async function loadProjectData() {
  const project = selectedProject;
  if (!project) return;
  $("#project-title").textContent = project.name;
  $("#project-path").textContent = project.path;
  await refreshProjectSessions();
  if (selectedProject?.id !== project.id) return;
  const progress = projectProgress.get(project.id);
  renderSessions(projectSessions.get(project.id) || []);
  renderAttention(progress?.snapshot);
  renderActivity(progress?.snapshot);
  renderBoard(progress?.board);
}

function renderBoard(board) {
  const list = $("#board-columns");
  const scrollTop = list.scrollTop;
  list.replaceChildren();
  const { total, done } = boardProgress(board);
  $("#board-summary").textContent = board ? `${done} of ${total} done` : "No board";
  $("#todo-add-form").classList.toggle("hidden", !board);
  $("#board-empty").classList.toggle("hidden", total > 0);
  $("#board-empty").textContent = board ? "No to-dos yet. Type one above." : "This project has no enabled board.";
  for (const column of board?.columns || []) {
    const section = document.createElement("section");
    section.className = "board-column";
    const heading = document.createElement("h3");
    heading.textContent = `${column.name} · ${column.cards.length}`;
    section.append(heading);
    for (const card of column.cards) {
      const item = document.createElement("div");
      item.className = "board-card";
      const title = document.createElement("span");
      title.textContent = card.title;
      item.append(title);
      if (card.claimed_by) {
        const matches = (projectSessions.get(selectedProject.id) || []).filter((session) =>
          session.claim === card.claimed_by && session.attachable !== false);
        const claim = document.createElement(matches.length === 1 ? "button" : "small");
        claim.textContent = `@${card.claimed_by}`;
        if (matches.length === 1) {
          claim.type = "button";
          claim.className = "board-claim";
          claim.title = "Open this agent’s session";
          claim.addEventListener("click", () => attach(matches[0]));
        }
        item.append(claim);
      }
      section.append(item);
    }
    list.append(section);
  }
  list.scrollTop = scrollTop;
}

function renderSessions(items) {
  const list = $("#session-list");
  const empty = $("#sessions-empty");
  list.replaceChildren();
  // Server order is authoritative (newest activity first); the client-side
  // sort keeps old cached payloads from jumping around before refresh.
  const ordered = [...items].sort(
    (a, b) => (b.last_activity_ms || 0) - (a.last_activity_ms || 0),
  );
  empty.classList.toggle("hidden", ordered.length > 0);
  for (const session of ordered) {
    // Catalog-only history has no live PTY: the terminal route must never
    // see its id. Render it as an inert record, not an attach button.
    const attachable = session.attachable !== false;
    const button = document.createElement("button");
    button.type = "button";
    button.className = "session-card";
    button.disabled = !attachable;
    button.title = attachable
      ? "Open session"
      : "Ended conversation — reopen it from the Radar desktop app";
    button.setAttribute(
      "aria-label",
      `${attachable ? "Open session" : "Ended conversation"}: ${session.title || session.label}`,
    );
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
    const age = session.last_activity_ms ? ` · ${relativeTime(session.last_activity_ms)}` : "";
    subtitle.textContent = `${session.label}${age} · ${session.detail || (session.pid ? `PID ${session.pid}` : session.program || "Radar session")}`;
    copy.append(title, subtitle);
    const state = document.createElement("span");
    state.className = `session-state ${session.state}`;
    state.textContent = session.state;
    const arrow = document.createElement("span");
    arrow.className = "session-arrow";
    arrow.textContent = attachable ? "›" : "";
    button.append(icon, copy, state, arrow);
    if (attachable) button.addEventListener("click", () => attach(session));
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
    card.dataset.attentionId = item.id;
    card.setAttribute("aria-busy", String(pendingResponses.has(item.id)));
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
    if (pendingResponses.has(item.id)) {
      for (const control of card.querySelectorAll("button, textarea")) control.disabled = true;
      meta.textContent = "Sending response…";
    }
    list.append(card);
  }
}

async function resolveAttention(item, action, answer) {
  if (pendingResponses.has(item.id)) return;
  const projectId = item.project_id;
  pendingResponses.add(item.id);
  const focused = document.activeElement;
  if (focused && $("#attention-list").contains(focused)) focused.blur();
  renderAttention(latestAttentionSnapshot);
  try {
    const payload = { revision: item.revision, action };
    if (answer !== undefined) payload.answer = answer;
    await request(`/api/projects/${projectId}/attention/${encodeURIComponent(item.id)}`, {
      method: "POST",
      body: JSON.stringify(payload),
    });
    answerDrafts.delete(item.id);
    showToast("Response sent");
    await loadProjectData();
  } catch (error) {
    showToast(error.message);
  } finally {
    pendingResponses.delete(item.id);
    if (selectedProject?.id === projectId) await loadProjectData();
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
    case "board_changed": {
      const action = (payload.action || "board updated").replaceAll("_", " ");
      const transition = payload.from_column && payload.column
        ? ` · ${payload.from_column} → ${payload.column}`
        : payload.column ? ` · ${payload.column}` : "";
      return `${action[0].toUpperCase()}${action.slice(1)}${payload.title ? ` · ${payload.title}` : ""}${transition}`;
    }
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
  if (terminalSession?.state !== "running" || !currentSocket || currentSocket.readyState !== WebSocket.OPEN) return;
  const filtered = terminalInputFilter(data);
  if (filtered) currentSocket.send(new TextEncoder().encode(filtered));
}

function sendTerminalBytes(bytes) {
  if (terminalSession?.state === "running" && currentSocket?.readyState === WebSocket.OPEN && bytes.length) currentSocket.send(bytes);
}

function attach(session) {
  if (!selectedProject) return;
  clearError();
  appShell.classList.add("session-open");
  dashboard.classList.add("hidden");
  terminalView.classList.remove("hidden");
  mainContent.classList.add("terminal-open");
  $("#terminal-title").textContent = session.title || session.label;
  const readOnly = session.state !== "running";
  $("#reconnect-button").disabled = readOnly;
  document.querySelectorAll("[data-key]").forEach((button) => { button.disabled = readOnly; });
  setTerminalStatus(readOnly ? "Read-only · use Workspace to choose another session" : "Connecting…", false, readOnly);

  socketGeneration += 1;
  currentSocket?.close();
  currentSocket = null;
  if (terminal) terminal.dispose();
  terminalSession = session;
  reconnectAttempts = 0;
  window.clearTimeout(reconnectTimer);
  reconnectTimer = null;
  terminal = new Terminal({
    cursorBlink: true,
    disableStdin: readOnly,
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
    if (!applyingRemoteResize && !readOnly) sendTerminalResize(cols, rows);
  });
  for (const button of document.querySelectorAll("[data-key]")) {
    button.disabled = readOnly;
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
    setTerminalStatus(
      session.state === "running"
        ? "Connected"
        : "Read-only · use Workspace to choose another session",
      session.state === "running",
    );
    fitAddon?.fit();
    if (session.state === "running") sendTerminalResize(terminal?.cols, terminal?.rows);
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
      if (status.type === "closed") setTerminalReadOnly("exited");
    } catch {
      setTerminalStatus(String(message.data), false, true);
    }
  });
  socket.addEventListener("close", () => {
    if (generation !== socketGeneration) return;
    currentSocket = null;
    if (!terminalSession) return;
    void retryOrFinishSession(generation);
  });
  socket.addEventListener("error", () => {
    if (generation === socketGeneration) setTerminalStatus("Connection interrupted…", false, true);
  });
}

function sendTerminalResize(cols, rows) {
  if (terminalSession?.state === "running" && currentSocket?.readyState === WebSocket.OPEN && cols && rows) {
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
  if (reconnectTimer || terminalSession?.state !== "running") return;
  const delay = Math.min(1000 * 2 ** reconnectAttempts, 8000);
  reconnectAttempts += 1;
  setTerminalStatus(`Reconnecting in ${Math.ceil(delay / 1000)}s…`, false, true);
  reconnectTimer = window.setTimeout(() => {
    reconnectTimer = null;
    if (terminalSession?.state === "running") connectTerminal(terminalSession, true);
  }, delay);
}

async function retryOrFinishSession(generation) {
  const session = terminalSession;
  if (!session || session.state !== "running") return;
  try {
    const sessions = await request(`/api/projects/${selectedProject.id}/sessions`);
    const current = sessions.find((item) => item.id === session.id);
    if (!current) {
      detach();
      return;
    }
    if (current.state !== "running") {
      setTerminalReadOnly(current.state);
      return;
    }
  } catch {
    // A temporary HTTP outage is handled by the regular reconnect backoff.
  }
  if (generation === socketGeneration && terminalSession?.state === "running") scheduleReconnect();
}

function reconnectNow() {
  if (terminalSession?.state !== "running") return;
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
}
function setTerminalReadOnly(state) {
  if (terminalSession) terminalSession = { ...terminalSession, state };
  $("#reconnect-button").disabled = true;
  document.querySelectorAll("[data-key]").forEach((button) => { button.disabled = true; });
  setTerminalStatus("Read-only · use Workspace to choose another session");
}

function fitTerminal() {
  window.setTimeout(() => {
    fitAddon?.fit();
    terminal?.focus();
  }, 60);
}

function detach() {
  terminalSession = null;
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
  appShell.classList.remove("session-open");
  mainContent.classList.remove("terminal-open");
  dashboard.classList.remove("hidden");
  loadProjectData().catch((error) => showError(error.message));
}

async function addTodo(title) {
  const project = selectedProject;
  const trimmed = title.trim();
  if (!project || !trimmed) return;
  try {
    const board = await request(`/api/projects/${project.id}/board`, {
      method: "POST",
      body: JSON.stringify({ title: trimmed }),
    });
    const progress = projectProgress.get(project.id);
    if (progress) progress.board = board;
    if (selectedProject?.id === project.id) {
      renderBoard(board);
      renderProjects();
    }
    $("#todo-add-input")?.focus({ preventScroll: true });
  } catch (error) {
    showToast(error.message);
  }
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
  const agentButton = event.target.closest("[data-session-id]");
  if (agentButton) {
    const projectId = Number(agentButton.dataset.agentProjectId);
    const sessions = projectSessions.get(projectId) || [];
    const session = sessions.find((item) => item.id === agentButton.dataset.sessionId);
    const project = projects.find((item) => item.id === projectId);
    if (session && session.attachable !== false && project) {
      selectedProject = project;
      const url = new URL(document.baseURI);
      url.searchParams.set("project", String(projectId));
      history.replaceState(null, "", url);
      renderProjects();
      attach(session);
    }
    return;
  }

  const toggle = event.target.closest("[data-toggle-project-id]");
  if (toggle) {
    const projectId = Number(toggle.dataset.toggleProjectId);
    if (expandedProjects.has(projectId)) expandedProjects.delete(projectId);
    else expandedProjects.add(projectId);
    renderProjects();
    return;
  }

  const button = event.target.closest("[data-project-id]");
  if (!button) return;
  const project = projects.find((item) => item.id === Number(button.dataset.projectId));
  if (!project) return;

  const alreadySelected = selectedProject?.id === project.id;
  selectedProject = project;
  expandedProjects.add(project.id);
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
$("#attention-shortcut").addEventListener("click", openAttention);
$("#terminal-attention-shortcut").addEventListener("click", openAttention);
$("#new-shell-button").addEventListener("click", createShell);
$("#todo-add-form").addEventListener("submit", (event) => {
  event.preventDefault();
  const input = $("#todo-add-input");
  const title = input.value;
  input.value = "";
  addTodo(title);
});
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
  loadProjectData().catch((error) => showError(error.message));
}, 3000);
void refreshTimer;
