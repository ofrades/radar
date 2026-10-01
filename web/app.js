import { Terminal } from "./xterm.mjs";
import { FitAddon } from "./fit-addon.mjs";

// Prefer the libghostty-vt renderer when the build ships its WASM module;
// otherwise fall back to xterm.js. The check is async and the choice is read
// when a terminal is attached.
let GhosttyTerminal = null;
let ghosttyEngine = false;
(async () => {
  try {
    const module = await import("./ghostty-terminal.mjs");
    await module.loadGhostty();
    GhosttyTerminal = module.GhosttyTerminal;
    ghosttyEngine = true;
  } catch (error) {
    console.warn("libghostty-vt unavailable; using xterm.js:", error);
  }
})();

// ---------------------------------------------------------------------------
// Small DOM helpers
// ---------------------------------------------------------------------------

const $ = (selector) => document.querySelector(selector);

function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (value == null || value === false) continue;
    if (key === "class") node.className = value;
    else if (key === "text") node.textContent = value;
    else if (key === "html") node.innerHTML = value;
    else if (key === "dataset") Object.assign(node.dataset, value);
    else if (key.startsWith("on") && typeof value === "function") {
      node.addEventListener(key.slice(2).toLowerCase(), value);
    } else if (value === true) node.setAttribute(key, "");
    else node.setAttribute(key, value);
  }
  for (const child of children.flat()) {
    if (child == null || child === false) continue;
    node.append(child.nodeType ? child : document.createTextNode(String(child)));
  }
  return node;
}

function escapeHtml(text) {
  return String(text).replace(/[&<>"']/g, (char) => ({
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  }[char]));
}

/// Inline markdown, escaped first so only the tags below can appear.
function inlineMarkdown(text) {
  let html = escapeHtml(text);
  html = html.replace(/`([^`]+)`/g, "<code>$1</code>");
  html = html.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  html = html.replace(/(^|[^*])\*([^*]+)\*/g, "$1<em>$2</em>");
  html = html.replace(
    /\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g,
    '<a href="$2" target="_blank" rel="noopener noreferrer">$1</a>',
  );
  return html;
}

/// A tiny, safe Markdown renderer: headings, fenced code, lists, paragraphs,
/// bold/italic/inline-code/links. Enough for a card body and thread messages.
function renderMarkdown(text) {
  const root = el("div", { class: "markdown" });
  const lines = String(text ?? "").replace(/\r\n/g, "\n").split("\n");
  let index = 0;
  while (index < lines.length) {
    const line = lines[index];
    if (/^\s*```/.test(line)) {
      const language = line.replace(/^\s*```/, "").trim();
      const code = [];
      index += 1;
      while (index < lines.length && !/^\s*```/.test(lines[index])) {
        code.push(lines[index]);
        index += 1;
      }
      index += 1;
      root.append(el("pre", {}, el("code", {
        class: language ? `language-${language}` : "",
        text: code.join("\n"),
      })));
      continue;
    }
    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    if (heading) {
      root.append(el("h4", { class: "md-heading", html: inlineMarkdown(heading[2]) }));
      index += 1;
      continue;
    }
    if (/^\s*([-*+]|\d+\.)\s+/.test(line)) {
      const ordered = /^\s*\d+\./.test(line);
      const list = el(ordered ? "ol" : "ul");
      while (index < lines.length && /^\s*([-*+]|\d+\.)\s+/.test(lines[index])) {
        list.append(el("li", {
          html: inlineMarkdown(lines[index].replace(/^\s*([-*+]|\d+\.)\s+/, "")),
        }));
        index += 1;
      }
      root.append(list);
      continue;
    }
    if (line.trim() === "") {
      index += 1;
      continue;
    }
    const paragraph = [];
    while (
      index < lines.length &&
      lines[index].trim() !== "" &&
      !/^\s*```/.test(lines[index]) &&
      !/^#{1,6}\s/.test(lines[index]) &&
      !/^\s*([-*+]|\d+\.)\s+/.test(lines[index])
    ) {
      paragraph.push(lines[index]);
      index += 1;
    }
    root.append(el("p", { html: paragraph.map(inlineMarkdown).join("<br>") }));
  }
  return root;
}

function relativeTime(millis) {
  if (!millis) return "";
  const seconds = Math.max(0, Math.floor((Date.now() - millis) / 1000));
  if (seconds < 60) return "just now";
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
  return `${Math.floor(seconds / 86400)}d ago`;
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

const main = $("#main");
const appShell = $(".app-shell");
const serverState = $("#server-state");
const pageErrorHost = el("div", { class: "page-error hidden", id: "page-error" });

let projects = [];
const progress = new Map();
let loadedProjects = false;
let online = true;
let renderPending = false;

const pendingResponses = new Set();
const answerDrafts = new Map();
const commentDrafts = new Map();
const noticedAttention = new Set();
const initializedAttention = new Set();

let editingCard = null;

// Terminal state
let terminalOpen = false;
let lastNonSessionHash = "#/";
/// The one full-screen terminal, when a session route is open.
let singleTerminal = null;

// The Agents wall: a persistent set of live terminal tiles, reconciled in
// place so polling never reconnects a pane.
let agentsWall = null;
const dismissedAgents = new Set();

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

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
  pageErrorHost.textContent = message;
  pageErrorHost.classList.remove("hidden");
  if (!pageErrorHost.isConnected) main.append(pageErrorHost);
}

function clearError() {
  pageErrorHost.classList.add("hidden");
  pageErrorHost.textContent = "";
}

function showToast(message) {
  const toast = $("#toast");
  toast.textContent = message;
  toast.classList.remove("hidden");
  window.clearTimeout(showToast.timeout);
  showToast.timeout = window.setTimeout(() => toast.classList.add("hidden"), 2600);
}

function setOnline(value) {
  online = value;
  serverState.classList.toggle("online", value);
  serverState.setAttribute("aria-label", value ? "Progress up to date" : "Progress unavailable — retrying");
}

// ---------------------------------------------------------------------------
// Data access
// ---------------------------------------------------------------------------

function projectById(id) {
  return projects.find((project) => project.id === Number(id)) || null;
}

function projectProgress(id) {
  return progress.get(Number(id)) || null;
}

function boardOf(projectId) {
  return projectProgress(projectId)?.board || null;
}

function snapshotOf(projectId) {
  return projectProgress(projectId)?.snapshot || null;
}

function sessionsOf(projectId) {
  return projectProgress(projectId)?.sessions || [];
}

function lanesOf(projectId) {
  return boardOf(projectId)?.lanes || [];
}

function cardsOf(projectId) {
  return boardOf(projectId)?.cards || [];
}

function cardById(projectId, cardId) {
  return cardsOf(projectId).find((card) => card.id === cardId) || null;
}

function laneKind(projectId, laneId) {
  return lanesOf(projectId).find((lane) => lane.id === laneId)?.kind || "custom";
}

function laneRank(projectId, laneId) {
  switch (laneKind(projectId, laneId)) {
    case "in_progress": return 0;
    case "review": return 1;
    case "todo": return 2;
    case "done": return 9;
    default: return 3;
  }
}

/// Open cards (not in a done-kind lane), ordered In progress, Review, Todo.
function openCards(projectId) {
  return cardsOf(projectId)
    .filter((card) => !card.done)
    .slice()
    .sort((a, b) => {
      const rank = laneRank(projectId, a.lane_id) - laneRank(projectId, b.lane_id);
      return rank !== 0 ? rank : a.position - b.position;
    });
}

function doneCount(projectId) {
  return cardsOf(projectId).filter((card) => card.done).length;
}

function openTodoCount() {
  return projects.reduce((sum, project) => sum + openCards(project.id).length, 0);
}

function runningSessions(projectId) {
  return sessionsOf(projectId).filter((session) => session.state === "running");
}

/// Radar-launched agent sessions — the native Agents wall only ever holds the
/// `agent` slot, never a shell, editor or diff tool.
function agentSessions(projectId) {
  return runningSessions(projectId).filter((session) => session.slot === "agent");
}

function stoppedSessions(projectId) {
  return sessionsOf(projectId).filter((session) => session.state !== "running" && session.attachable !== false);
}

function unresolvedAttention() {
  const items = [];
  for (const project of projects) {
    for (const item of snapshotOf(project.id)?.attention || []) {
      if (item.resolved_at_millis == null) items.push({ project, item });
    }
  }
  items.sort((a, b) => b.item.created_at_millis - a.item.created_at_millis);
  return items;
}

function attentionForCard(projectId, cardId) {
  return (snapshotOf(projectId)?.attention || []).filter(
    (item) => item.card_id === cardId && item.resolved_at_millis == null,
  );
}

/// The latest agent note on a card — what was done, visible without opening a
/// terminal.
function latestNote(projectId, cardId) {
  const events = snapshotOf(projectId)?.events || [];
  for (let index = events.length - 1; index >= 0; index -= 1) {
    const event = events[index];
    if (event.card_id !== cardId || !event.session_id) continue;
    if (event.payload?.type === "message") {
      const text = (event.payload.data?.text || "").split("\n")[0].trim();
      if (text) return text.length > 96 ? `${text.slice(0, 95)}…` : text;
    }
  }
  return null;
}

/// The one attachable session bound to a claim, if it is unambiguous.
function claimSession(projectId, claim) {
  if (!claim) return null;
  const matches = sessionsOf(projectId).filter(
    (session) => session.claim === claim && session.attachable !== false,
  );
  return matches.length === 1 ? matches[0] : null;
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

function parseRoute() {
  const raw = location.hash.replace(/^#\/?/, "");
  const parts = raw.split("/").filter(Boolean).map((part) => decodeURIComponent(part));
  if (parts[0] === "agents") return { name: "agents" };
  if (parts[0] === "project" && parts[1]) {
    if (parts[2] === "card" && parts[3]) {
      return { name: "card", projectId: Number(parts[1]), cardId: parts.slice(3).join("/") };
    }
    if (parts[2] === "session" && parts[3]) {
      return { name: "session", projectId: Number(parts[1]), sessionId: parts.slice(3).join("/") };
    }
    return { name: "project", projectId: Number(parts[1]) };
  }
  return { name: "home" };
}

function navigate(hash) {
  if (location.hash === hash) render();
  else location.hash = hash;
}

window.addEventListener("hashchange", () => {
  if (parseRoute().name !== "session") lastNonSessionHash = location.hash || "#/";
  editingCard = null;
  render();
});

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

function editableFocused() {
  const active = document.activeElement;
  if (!active) return false;
  const editable = active.tagName === "INPUT" || active.tagName === "TEXTAREA" || active.isContentEditable;
  return editable && main.contains(active);
}

function scheduleRender() {
  if (terminalOpen) return;
  if (editableFocused()) {
    renderPending = true;
    return;
  }
  renderPending = false;
  render();
}

document.addEventListener("focusout", () => {
  if (!renderPending) return;
  window.setTimeout(() => {
    if (!editableFocused()) scheduleRender();
  }, 0);
});

function render() {
  const route = parseRoute();
  if (route.name === "session") {
    renderSessionRoute(route);
    return;
  }
  if (terminalOpen) exitTerminal();
  if (route.name !== "agents") teardownAgentsWall();
  main.classList.toggle("wall-open", route.name === "agents");
  if (route.name === "agents") {
    showAgentsWall();
    return;
  }
  main.replaceChildren();
  const error = $("#page-error");
  let view;
  switch (route.name) {
    case "project":
      view = renderProject(route.projectId);
      break;
    case "card":
      view = renderCard(route.projectId, route.cardId);
      break;
    default:
      view = renderHome();
  }
  main.append(view, pageErrorHost);
  if (error) error.classList.add("hidden");
}

/// A session route either enters the terminal or explains why it cannot.
function renderSessionRoute(route) {
  if (terminalOpen && terminalSession?.id === route.sessionId && terminalProjectId === route.projectId) {
    return;
  }
  if (!loadedProjects || !progress.has(route.projectId)) {
    if (terminalOpen) exitTerminal();
    main.replaceChildren(el("div", { class: "home-view" },
      hero("Session", "Loading…", () => navigate(lastNonSessionHash || "#/")),
    ), pageErrorHost);
    return;
  }
  const session = sessionsOf(route.projectId).find((item) => item.id === route.sessionId);
  if (!session || session.attachable === false) {
    if (terminalOpen) exitTerminal();
    main.replaceChildren(el("div", { class: "home-view" },
      hero("Session", session ? "That conversation has ended. Reopen it from the Radar desktop app." : "That session is no longer available.", () => navigate(lastNonSessionHash || "#/")),
    ), pageErrorHost);
    return;
  }
  enterTerminal(route.projectId, session);
}

function hero(title, summary, back) {
  return el("div", { class: "page-heading" },
    el("div", {},
      back ? el("button", { class: "back-button", type: "button", onclick: back }, el("span", {}, "‹"), " Back") : null,
      el("h1", { text: title }),
      el("p", { class: "project-summary", text: summary }),
    ),
  );
}

function sectionHeading(title, trailing) {
  return el("div", { class: "section-heading" },
    el("div", {}, el("h2", { text: title })),
    trailing ? el("span", { class: "project-summary", text: trailing }) : null,
  );
}

// ---- Home ----

function renderHome() {
  const wrap = el("div", { class: "dashboard home" });
  if (!loadedProjects) {
    wrap.append(hero("Home", "Loading your workspace…"));
    return wrap;
  }
  if (!projects.length) {
    wrap.append(hero("Home", "No projects yet"));
    wrap.append(el("div", { class: "empty-card" },
      el("strong", {}, "Add a project in Radar on your computer."),
      el("span", {}, "The web client is a remote view of the workspace Radar already knows."),
    ));
    return wrap;
  }

  const needs = unresolvedAttention();
  const running = projects.reduce((sum, project) => sum + agentSessions(project.id).length, 0);
  const projectLabel = projects.length === 1 ? "1 project" : `${projects.length} projects`;
  wrap.append(hero("Home", `${projectLabel} · ${needs.length} need you`));
  wrap.append(agentsDestination(running));

  if (needs.length) {
    wrap.append(sectionHeading("Needs you", `${needs.length} open`));
    for (const { project, item } of needs) wrap.append(attentionCard(project, item));
  }

  const toDos = openTodoCount();
  wrap.append(sectionHeading("Projects", toDos === 1 ? "1 to-do" : `${toDos} to-dos`));
  const grid = el("div", { class: "lane-grid" });
  for (const project of projects) grid.append(projectLane(project));
  wrap.append(grid);
  return wrap;
}

function agentsDestination(running) {
  const count = running === 0 ? "No agents" : running === 1 ? "1 running" : `${running} running`;
  return el("button", {
    class: "destination-card",
    type: "button",
    onclick: () => navigate("#/agents"),
  },
    el("span", { class: "destination-icon", "aria-hidden": "true" }, ">_"),
    el("span", { class: "destination-copy" },
      el("span", { class: "destination-title" }, "Agents"),
      el("span", { class: "destination-summary" }, `${count} · All projects, one workspace`),
      el("span", { class: "destination-hint" }, "Open a live session and work in it"),
    ),
    el("span", { class: "destination-arrow", "aria-hidden": "true" }, "›"),
  );
}

function projectLane(project) {
  const lane = el("article", { class: "lane" });
  const board = boardOf(project.id);
  const stale = projectProgress(project.id)?.stale;

  lane.append(el("header", { class: "lane-header" },
    el("button", {
      class: "lane-name",
      type: "button",
      title: project.path,
      onclick: () => navigate(`#/project/${project.id}`),
    }, project.name),
    el("span", { class: "lane-path", text: project.path }),
  ));

  const pills = el("div", { class: "lane-pills" });
  let anyPill = false;
  for (const boardLane of board?.lanes || []) {
    if (boardLane.kind === "done") continue;
    const count = cardsOf(project.id).filter((card) => card.lane_id === boardLane.id && !card.done).length;
    if (!count) continue;
    anyPill = true;
    pills.append(el("span", { class: `pill ${pillClass(boardLane.kind)}` }, `${boardLane.name} ${count}`));
  }
  if (!anyPill) pills.append(el("span", { class: "pill" }, board ? "No open to-dos" : "No board"));
  lane.append(pills);

  lane.append(todoAddForm(project.id));

  lane.append(el("p", { class: "lane-section" }, "To-dos"));
  const open = openCards(project.id);
  if (!board) {
    lane.append(el("p", { class: "quiet" }, stale ? "Board unavailable" : "No board"));
  } else if (!open.length) {
    lane.append(el("p", { class: "quiet" }, "No to-dos yet"));
  } else {
    const list = el("div", { class: "todo-list" });
    for (const card of open) list.append(todoRow(project.id, card));
    lane.append(list);
  }

  lane.append(el("footer", { class: "lane-foot" }, laneFooterText(project.id)));
  return lane;
}

function pillClass(kind) {
  if (kind === "in_progress") return "pill-active";
  if (kind === "review") return "pill-review";
  if (kind === "done") return "pill-done";
  return "";
}

function laneFooterText(projectId) {
  const running = agentSessions(projectId).length;
  const stopped = stoppedSessions(projectId).length;
  const live = running === 0 ? "no agents" : running === 1 ? "1 running" : `${running} running`;
  if (!stopped) return live;
  return `${live} · ${stopped} stopped`;
}

function todoAddForm(projectId) {
  const input = el("input", {
    class: "todo-add-input",
    type: "text",
    placeholder: "Add a to-do…",
    "aria-label": "Add a to-do",
  });
  const form = el("form", { class: "todo-add" }, input);
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const title = input.value.trim();
    if (!title) return;
    input.value = "";
    addTodo(projectId, title);
  });
  return form;
}

function todoRow(projectId, card) {
  const row = el("div", { class: "todo-row" });
  const tick = el("button", {
    class: `todo-tick${card.done ? " done" : ""}`,
    type: "button",
    title: card.done ? "Reopen this to-do" : "Close this to-do",
    "aria-label": card.done ? "Reopen to-do" : "Close to-do",
    onclick: (event) => {
      event.stopPropagation();
      toggleDone(projectId, card);
    },
  }, el("span", { class: "todo-box" }));
  row.append(tick);

  const open = el("button", {
    class: "todo",
    type: "button",
    title: `Open this card\n${card.id}`,
    onclick: () => navigate(`#/project/${projectId}/card/${encodeURIComponent(card.id)}`),
  }, el("span", { class: `todo-title${card.done ? " done" : ""}`, text: card.title }));
  const copy = el("div", { class: "todo-copy" }, open);
  const meta = el("div", { class: "todo-meta" });
  if (card.claim) {
    const session = claimSession(projectId, card.claim);
    if (session) {
      meta.append(el("button", {
        class: "todo-claim link",
        type: "button",
        title: "Open this agent's session",
        onclick: (event) => { event.stopPropagation(); openSession(projectId, session); },
      }, `@${card.claim}`));
    } else {
      meta.append(el("span", { class: "todo-claim", text: `@${card.claim}` }));
    }
  }
  const note = latestNote(projectId, card.id);
  if (note) meta.append(el("span", { class: "todo-note", text: note }));
  if (meta.childNodes.length) copy.append(meta);
  row.append(copy);
  return row;
}

// ---- Project ----

function renderProject(projectId) {
  const project = projectById(projectId);
  if (!project) return missingView("That project is no longer available.");
  const wrap = el("div", { class: "home-view project-view" });
  const board = boardOf(projectId);
  const done = doneCount(projectId);
  const open = openCards(projectId).length;
  wrap.append(hero(project.name, `${project.path} · ${open} open · ${done} done`, () => navigate("#/")));
  wrap.append(el("div", { class: "project-actions" },
    el("button", {
      class: "small-button",
      type: "button",
      onclick: () => createShell(projectId),
    }, "＋ New terminal"),
  ));
  wrap.append(todoAddForm(projectId));

  if (!board) {
    wrap.append(el("div", { class: "empty-card" },
      el("strong", {}, "No board for this project."),
      el("span", {}, projectProgress(projectId)?.stale ? "The board is temporarily unavailable." : "Enable the board in Radar on your computer."),
    ));
    return wrap;
  }

  const columns = el("div", { class: "project-columns" });
  for (const boardLane of board.lanes) {
    if (boardLane.kind === "done") continue;
    const cards = cardsOf(projectId)
      .filter((card) => card.lane_id === boardLane.id && !card.done)
      .sort((a, b) => a.position - b.position);
    const column = el("section", { class: "project-column" },
      el("h3", {}, el("span", { text: boardLane.name }), el("span", { class: "column-count", text: String(cards.length) })),
    );
    const list = el("div", { class: "column-cards" });
    for (const card of cards) list.append(todoRow(projectId, card));
    if (!cards.length) list.append(el("p", { class: "quiet" }, "Nothing here"));
    column.append(list);
    columns.append(column);
  }
  wrap.append(columns);
  return wrap;
}

// ---- Card ----

function renderCard(projectId, cardId) {
  const project = projectById(projectId);
  if (!project) return missingView("That project is no longer available.");
  const card = cardById(projectId, cardId);
  if (!card) {
    return el("div", { class: "home-view card-view" },
      hero("Card", "This card is no longer on the board", () => navigate(`#/project/${projectId}`)),
      el("p", { class: "quiet" }, projectProgress(projectId) ? "It may have been removed or moved." : "Loading the card…"),
    );
  }

  const wrap = el("div", { class: "home-view card-view" });
  wrap.append(hero(card.title, cardMeta(projectId, card), () => navigate(`#/project/${projectId}`)));

  wrap.append(el("div", { class: "card-id" },
    el("code", { text: card.id }),
    el("button", {
      class: "small-button",
      type: "button",
      onclick: async () => {
        try {
          await navigator.clipboard.writeText(card.id);
          showToast("Card id copied");
        } catch {
          showToast(card.id);
        }
      },
    }, "Copy id"),
  ));

  if (card.body.trim()) {
    const body = renderMarkdown(card.body);
    body.classList.add("card-body");
    wrap.append(body);
  }

  if (editingCard === card.id) {
    wrap.append(cardEditForm(projectId, card));
  } else {
    wrap.append(cardControls(projectId, card));
  }

  wrap.append(el("h2", { class: "lane-section" }, "Linked sessions"));
  const linked = card.claim
    ? sessionsOf(projectId).filter((session) => session.claim === card.claim && session.attachable !== false)
    : [];
  if (!linked.length) {
    wrap.append(el("p", { class: "quiet" }, card.claim ? `No live session for @${card.claim}` : "No session on this card yet."));
  } else {
    for (const session of linked) wrap.append(sessionCard(projectId, session));
  }

  wrap.append(el("h2", { class: "lane-section" }, "Conversation"));
  wrap.append(cardThread(projectId, card));

  wrap.append(cardReplyForm(projectId, card));
  return wrap;
}

function cardMeta(projectId, card) {
  const project = projectById(projectId);
  const lane = lanesOf(projectId).find((item) => item.id === card.lane_id);
  const parts = [];
  if (project) parts.push(project.name);
  parts.push(lane?.name || card.lane);
  if (card.claim) parts.push(`@${card.claim}`);
  if (card.done) parts.push("done");
  return parts.join(" · ");
}

function cardControls(projectId, card) {
  const controls = el("div", { class: "card-controls" });
  controls.append(el("button", {
    class: "small-button",
    type: "button",
    onclick: () => { editingCard = card.id; render(); },
  }, "Edit"));

  controls.append(el("button", {
    class: card.done ? "small-button" : "primary-button",
    type: "button",
    onclick: () => toggleDone(projectId, card),
  }, card.done ? "Reopen" : "Close to-do"));

  const lanes = lanesOf(projectId);
  const select = el("select", { class: "lane-select", "aria-label": "Move to another lane" });
  for (const lane of lanes) {
    select.append(el("option", {
      value: lane.name,
      selected: lane.id === card.lane_id,
    }, lane.name));
  }
  select.addEventListener("change", () => moveCard(projectId, card, select.value));
  controls.append(select);

  controls.append(el("button", {
    class: "small-button",
    type: "button",
    onclick: () => navigate(`#/project/${projectId}`),
  }, "Open project"));
  return controls;
}

function cardEditForm(projectId, card) {
  const title = el("input", { class: "todo-add-input", type: "text", value: card.title, "aria-label": "Card title" });
  const body = el("textarea", { class: "card-body-edit", rows: 8, "aria-label": "Card body" });
  body.value = card.body;
  const error = el("p", { class: "card-error hidden" });
  const form = el("form", { class: "card-edit" },
    title,
    body,
    error,
    el("div", { class: "card-controls" },
      el("button", { class: "primary-button", type: "submit" }, "Save"),
      el("button", { class: "small-button", type: "button", onclick: () => { editingCard = null; render(); } }, "Cancel"),
    ),
  );
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const nextTitle = title.value.trim();
    if (!nextTitle) {
      error.textContent = "A card needs a title.";
      error.classList.remove("hidden");
      return;
    }
    try {
      await mutateCard(projectId, card.id, {
        action: "update",
        title: nextTitle,
        body: body.value,
        revision: card.revision,
      });
      editingCard = null;
      showToast("Card saved");
      await refreshAll({ quiet: true });
      render();
    } catch (requestError) {
      error.textContent = `Could not save: ${requestError.message}`;
      error.classList.remove("hidden");
    }
  });
  return form;
}

function cardThread(projectId, card) {
  const thread = el("div", { class: "thread" });
  const events = (snapshotOf(projectId)?.events || [])
    .filter((event) => event.card_id === card.id)
    .slice()
    .sort((a, b) => a.sequence - b.sequence);
  const attention = attentionForCard(projectId, card.id);

  if (!events.length && !attention.length) {
    thread.append(el("p", { class: "quiet" }, "No activity on this card yet"));
    return thread;
  }

  for (const event of events) {
    const payload = event.payload || {};
    if (payload.type === "message") {
      const author = event.session_id ? "Agent" : "You";
      thread.append(threadMessage(author, payload.data?.text || "", event.at_millis));
    } else if (payload.type === "board_changed") {
      thread.append(el("p", { class: "thread-system", text: boardChangeText(payload.data || {}) }));
    } else if (payload.type === "attention_resolved") {
      const data = payload.data || {};
      thread.append(el("p", { class: "thread-system", text: `Answered: ${responseText(data.response)}` }));
    }
  }

  for (const item of attention) {
    thread.append(attentionCard(projectOfId(projectId), item));
  }
  return thread;
}

function projectOfId(projectId) {
  return projectById(projectId) || { id: projectId, name: `project ${projectId}`, path: "" };
}

function threadMessage(author, text, atMillis) {
  const row = el("div", { class: `thread-row ${author === "You" ? "thread-you" : "thread-agent"}` });
  row.append(el("div", { class: "thread-head" },
    el("span", { class: "thread-author", text: author }),
    el("span", { class: "thread-age", text: relativeTime(atMillis) }),
  ));
  row.append(renderMarkdown(text));
  return row;
}

function boardChangeText(data) {
  const action = data.action || "updated";
  const label = ({
    added: "Added to the board",
    board_card_added: "Added to the board",
    moved: "Moved",
    board_card_moved: "Moved",
    claimed: "Claimed",
    board_card_claimed: "Claimed",
    released: "Released",
    board_card_released: "Released",
    done: "Closed",
    board_card_done: "Closed",
    completed: "Closed",
    board_card_completed: "Closed",
    reopened: "Reopened",
    board_card_reopened: "Reopened",
    edited: "Edited",
    board_card_edited: "Edited",
  })[action] || action.replaceAll("_", " ");
  return data.column ? `${label} · ${data.column}` : label;
}

function responseText(response) {
  if (response == null) return "";
  if (typeof response === "string") return response;
  if (response.answer != null) return response.answer;
  if (response === "approve") return "approved";
  return String(response);
}

function cardReplyForm(projectId, card) {
  const input = el("input", {
    class: "reply-input",
    type: "text",
    placeholder: "Reply, or leave the agent a note…",
    "aria-label": "Reply to card",
    value: commentDrafts.get(card.id) || "",
  });
  input.addEventListener("input", () => commentDrafts.set(card.id, input.value));
  const form = el("form", { class: "reply-row" },
    input,
    el("button", { class: "primary-button", type: "submit" }, "Send"),
  );
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const text = input.value.trim();
    if (!text) return;
    input.value = "";
    commentDrafts.delete(card.id);
    try {
      await request(`/api/projects/${projectId}/cards/${encodeURIComponent(card.id)}/comments`, {
        method: "POST",
        body: JSON.stringify({ text }),
      });
      await refreshAll({ quiet: true });
      render();
    } catch (error) {
      input.value = text;
      commentDrafts.set(card.id, text);
      showToast(error.message);
    }
  });
  return form;
}

// ---- Agents ----

/// The sessions that belong on the Agents wall, mirroring the native policy
/// (`src/gui/agents.rs`): live sessions whose to-do is live work. A card in a
/// Todo or Done lane — or one that left the board — keeps the session off the
/// wall without stopping it; a session with no bound to-do is unjudged and
/// shows. Ended/catalog-only history never stands here.
function agentWall(projectId) {
  return agentSessions(projectId).filter((session) => {
    const cardId = sessionCardId(projectId, session);
    if (!cardId) return true;
    const card = cardById(projectId, cardId);
    if (!card) return false;
    return cardIsLiveWork(projectId, card);
  });
}

/// The card a session is working: the one whose claim names the session's
/// agent (the same exact match the board's claim links use).
function sessionCardId(projectId, session) {
  if (!session.claim) return null;
  return cardsOf(projectId).find((card) => card.claim === session.claim)?.id || null;
}

function cardIsLiveWork(projectId, card) {
  if (card.done) return false;
  const lane = lanesOf(projectId).find((item) => item.id === card.lane_id);
  if (lane) return lane.kind !== "todo" && lane.kind !== "done";
  const name = (card.lane || "").toLowerCase();
  return name !== "todo" && name !== "backlog" && name !== "done";
}

/// A session's activity sign, the same dot vocabulary the native panel header
/// wears: a request waiting is "needs you", a reported agent state is its own,
/// otherwise the process is simply running.
function sessionSign(projectId, session) {
  const snapshot = snapshotOf(projectId);
  const waiting = (snapshot?.attention || []).some(
    (item) => item.session_id === session.id && item.resolved_at_millis == null,
  );
  if (waiting) return { label: "needs you", cls: "sign-needs-you" };
  const event = (snapshot?.events || []).findLast(
    (item) => item.session_id === session.id && item.kind === "agent_state_changed",
  );
  const state = event?.payload?.data?.state;
  if (state === "waiting_for_input" || state === "waiting_for_approval") {
    return { label: "waiting", cls: "sign-waiting" };
  }
  if (state === "working") return { label: "working", cls: "sign-working" };
  return { label: "running", cls: "sign-running" };
}

/// The wall's membership across every project, in project order, newest
/// activity first — the native `live_sessions_by_project` order.
function agentsWallWanted() {
  const wanted = [];
  for (const project of projects) {
    const sessions = agentWall(project.id)
      .filter((session) => !dismissedAgents.has(session.id))
      .slice()
      .sort((a, b) => (b.last_activity_ms || 0) - (a.last_activity_ms || 0));
    for (const session of sessions) wanted.push({ project, session });
  }
  return wanted;
}

/// The Agents page is the workspace: real, interactive terminal panels tiled
/// across the viewport. It is reconciled in place — polling never rebuilds a
/// pane, so typing is never interrupted.
function showAgentsWall() {
  if (!agentsWall) {
    const meta = el("p", { class: "project-summary wall-meta" });
    const grid = el("div", { class: "agents-grid" });
    const root = el("div", { class: "home-view agents-view agents-wall" },
      el("div", { class: "page-heading wall-heading" },
        el("button", { class: "back-button", type: "button", onclick: () => navigate("#/") },
          el("span", {}, "‹"), " Back"),
        el("h1", { text: "Agents" }),
        meta,
      ),
      grid,
    );
    const observer = new ResizeObserver(() => layoutAgentWall());
    observer.observe(grid);
    agentsWall = { root, grid, meta, observer, tiles: new Map(), empty: null };
  }
  if (agentsWall.root.parentElement !== main) main.replaceChildren(agentsWall.root, pageErrorHost);
  syncAgentsWall();
}

function teardownAgentsWall() {
  if (!agentsWall) return;
  for (const tile of agentsWall.tiles.values()) tile.view.dispose();
  agentsWall.observer?.disconnect();
  agentsWall.root.remove();
  agentsWall = null;
}

function syncAgentsWall() {
  if (!agentsWall) return;
  const wanted = agentsWallWanted();
  const wantedIds = new Set(wanted.map(({ session }) => session.id));

  for (const [id, tile] of [...agentsWall.tiles]) {
    if (!wantedIds.has(id)) {
      tile.view.dispose();
      tile.el.remove();
      agentsWall.tiles.delete(id);
    }
  }
  for (const { project, session } of wanted) {
    if (!agentsWall.tiles.has(session.id)) {
      agentsWall.tiles.set(session.id, createAgentTile(project, session));
    }
  }
  for (const { project, session } of wanted) {
    const tile = agentsWall.tiles.get(session.id);
    updateAgentTile(tile, project, session);
  }
  // Reorder only when the order actually changed: re-inserting a node that is
  // already in place would blur the terminal the human is typing in.
  const order = wanted.map(({ session }) => agentsWall.tiles.get(session.id).el);
  const inOrder =
    agentsWall.grid.children.length === order.length &&
    order.every((node, index) => agentsWall.grid.children[index] === node);
  if (!inOrder) {
    for (const node of order) agentsWall.grid.append(node);
  }

  if (wanted.length === 0 && !agentsWall.empty) {
    agentsWall.empty = el("div", { class: "empty-card wall-empty" },
      el("strong", {}, "No agent panels yet."),
      el("span", {}, "Open a project and start a session from a to-do. Active work appears here; a to-do still in Todo or already done keeps its session off this wall, and its program keeps running."),
    );
    agentsWall.grid.append(agentsWall.empty);
  } else if (wanted.length > 0 && agentsWall.empty) {
    agentsWall.empty.remove();
    agentsWall.empty = null;
  }

  const count = wanted.length;
  agentsWall.meta.textContent = `${count === 1 ? "1 running" : `${count} running`} · All projects, one workspace`;
  layoutAgentWall();
}

/// One tile: the workspace panel's own header (activity sign, session name,
/// its to-do, a done check, a close that keeps the program running) over a
/// live terminal attached to the daemon session.
function createAgentTile(project, session) {
  const dot = el("span", { class: "activity-dot", "aria-hidden": "true" }, "●");
  const title = el("button", { class: "tile-title", type: "button" });
  const todo = el("button", { class: "tile-todo", type: "button" });
  const done = el("button", { class: "tile-action", type: "button", title: "Mark this to-do done" }, "✓");
  const close = el("button", { class: "tile-action", type: "button", title: "Close this panel — its program keeps running" }, "✕");
  const header = el("div", { class: "tile-header" }, dot, title, todo, done, close);
  const body = el("div", { class: "tile-body" });
  const tileEl = el("section", { class: "agents-tile" }, header, body);

  const view = new TerminalView(project.id, session, {
    interactive: true,
    onGone: () => {
      dismissedAgents.add(session.id);
      syncAgentsWall();
    },
  });
  const tile = { el: tileEl, body, view, dot, title, todo, done };
  title.addEventListener("click", () => openSession(project.id, session));
  close.addEventListener("click", () => {
    dismissedAgents.add(session.id);
    view.dispose();
    tileEl.remove();
    agentsWall?.tiles.delete(session.id);
    syncAgentsWall();
  });
  view.mount(body);
  return tile;
}

function updateAgentTile(tile, project, session) {
  const sign = sessionSign(project.id, session);
  tile.dot.className = `activity-dot ${sign.cls}`;
  tile.dot.title = sign.label;
  const name = session.title || session.label;
  tile.title.textContent = name;
  tile.title.title = `${name} — open full screen`;

  const cardId = sessionCardId(project.id, session);
  const card = cardId ? cardById(project.id, cardId) : null;
  if (card) {
    tile.todo.hidden = false;
    tile.todo.textContent = card.title;
    tile.todo.title = `Open this card\n${card.id}`;
    tile.todo.onclick = () => navigate(`#/project/${project.id}/card/${encodeURIComponent(card.id)}`);
    tile.done.hidden = false;
    tile.done.onclick = () => toggleDone(project.id, card);
  } else {
    tile.todo.hidden = true;
    tile.done.hidden = true;
  }
}

/// Pick the column count whose tiles come closest to a terminal's 8:5 shape,
/// the same score the native tiler uses.
function layoutAgentWall() {
  if (!agentsWall) return;
  const count = agentsWall.tiles.size;
  const grid = agentsWall.grid;
  if (count === 0) {
    grid.style.gridTemplateColumns = "1fr";
    return;
  }
  const width = grid.clientWidth || 1;
  const height = grid.clientHeight || 1;
  let best = 1;
  let bestScore = Infinity;
  for (let cols = 1; cols <= count; cols += 1) {
    const rows = Math.ceil(count / cols);
    const aspect = width / cols / (height / rows);
    const score = Math.abs(Math.log(aspect / (8 / 5)));
    if (score < bestScore) {
      bestScore = score;
      best = cols;
    }
  }
  grid.style.gridTemplateColumns = `repeat(${best}, minmax(0, 1fr))`;
}

/// A session row, used by a card's linked sessions.
function sessionCard(projectId, session) {
  const attachable = session.attachable !== false;
  const button = el("button", {
    class: "session-card",
    type: "button",
    disabled: !attachable,
    title: attachable ? "Open session" : "Ended conversation — reopen it from the Radar desktop app",
    "aria-label": `${attachable ? "Open session" : "Ended conversation"}: ${session.title || session.label}`,
  },
    el("span", { class: "session-icon", "aria-hidden": "true" },
      session.slot === "agent" ? "✳" : session.slot === "editor" ? "▤" : "⌘"),
    el("span", { class: "session-copy" },
      el("span", { class: "session-title", text: session.title || session.label }),
      el("span", { class: "session-subtitle", text: sessionSubtitle(session) }),
    ),
    el("span", { class: `session-state ${session.state}`, text: session.state }),
    el("span", { class: "session-arrow", text: attachable ? "›" : "" }),
  );
  if (attachable) button.addEventListener("click", () => openSession(projectId, session));
  return button;
}

function sessionSubtitle(session) {
  const age = session.last_activity_ms ? ` · ${relativeTime(session.last_activity_ms)}` : "";
  const detail = session.detail || (session.pid ? `PID ${session.pid}` : session.program || "Radar session");
  return `${session.label}${age} · ${detail}`;
}

// ---- Needs you (attention) ----

function attentionCard(project, item) {
  const card = el("article", { class: "attention-card" });
  card.dataset.attentionId = item.id;
  card.setAttribute("aria-busy", String(pendingResponses.has(item.id)));
  const top = el("div", { class: "attention-top" },
    el("span", { class: "attention-kind", text: `${item.kind} · ${project.name}` }),
    el("span", { class: "attention-age", text: relativeTime(item.created_at_millis) }),
  );
  card.append(top);
  card.append(renderMarkdown(item.reason));

  const target = el("div", { class: "attention-meta" });
  if (item.card_id) {
    target.append(el("button", {
      class: "link",
      type: "button",
      onclick: () => navigate(`#/project/${project.id}/card/${encodeURIComponent(item.card_id)}`),
    }, `Card ${shortId(item.card_id)}`));
  }
  if (item.session_id) {
    const session = sessionsOf(project.id).find((row) => row.id === item.session_id && row.attachable !== false);
    if (session) {
      target.append(el("button", {
        class: "link",
        type: "button",
        onclick: () => openSession(project.id, session),
      }, `Agent ${shortId(item.session_id)}`));
    } else {
      target.append(el("span", { text: `Agent ${shortId(item.session_id)}` }));
    }
  }
  if (target.childNodes.length) card.append(target);

  const actions = el("div", { class: "attention-actions" });
  const allowed = new Set(item.allowed_actions || []);
  if (allowed.has("answer")) {
    const answer = el("textarea", {
      class: "answer-input",
      rows: 1,
      placeholder: "Write a reply…",
      "aria-label": "Reply to agent",
    });
    answer.value = answerDrafts.get(item.id) || "";
    answer.addEventListener("input", () => answerDrafts.set(item.id, answer.value));
    const form = el("form", { class: "answer-box" },
      answer,
      el("button", { class: "small-button", type: "submit" }, "Reply"),
    );
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      resolveAttention(project.id, item, { action: "answer", answer: answer.value });
    });
    actions.append(form);
  }
  for (const action of ["approve", "deny", "dismiss"]) {
    if (!allowed.has(action)) continue;
    actions.append(el("button", {
      class: action === "approve" ? "primary-button compact" : "small-button",
      type: "button",
      onclick: () => resolveAttention(project.id, item, { action }),
    }, action[0].toUpperCase() + action.slice(1)));
  }
  if (item.seen_at_millis == null) {
    actions.append(el("button", {
      class: "small-button",
      type: "button",
      onclick: () => resolveAttention(project.id, item, { change: "seen" }),
    }, "Mark seen"));
  }
  if (item.acknowledged_at_millis == null) {
    actions.append(el("button", {
      class: "small-button",
      type: "button",
      onclick: () => resolveAttention(project.id, item, { change: "acknowledge" }),
    }, "Acknowledge"));
  }
  card.append(actions);

  if (pendingResponses.has(item.id)) {
    for (const control of card.querySelectorAll("button, textarea")) control.disabled = true;
    card.append(el("p", { class: "quiet" }, "Sending response…"));
  }
  return card;
}

function shortId(id) {
  if (!id) return "";
  return id.length > 14 ? `${id.slice(0, 8)}…` : id;
}

function missingView(message) {
  return el("div", { class: "home-view" },
    hero("Radar", message, () => navigate("#/")),
  );
}

// ---------------------------------------------------------------------------
// Mutations
// ---------------------------------------------------------------------------

async function mutateCard(projectId, cardId, body) {
  const board = await request(`/api/projects/${projectId}/cards/${encodeURIComponent(cardId)}`, {
    method: "POST",
    body: JSON.stringify(body),
  });
  const entry = progress.get(projectId);
  if (entry) entry.board = board;
  return board;
}

async function addTodo(projectId, title) {
  try {
    const board = await request(`/api/projects/${projectId}/board`, {
      method: "POST",
      body: JSON.stringify({ title }),
    });
    const entry = progress.get(projectId);
    if (entry) entry.board = board;
    render();
    focusTodoInput(projectId);
  } catch (error) {
    showToast(error.message);
  }
}

/// Start the user's login shell in a project and open it.
async function createShell(projectId) {
  try {
    const session = await request(`/api/projects/${projectId}/sessions`, {
      method: "POST",
      body: "{}",
    });
    await refreshAll({ quiet: true });
    openSession(projectId, session);
  } catch (error) {
    showToast(error.message);
  }
}

function focusTodoInput(projectId) {
  const route = parseRoute();
  if (route.name === "project" && route.projectId === projectId) {
    main.querySelector(".todo-add-input")?.focus({ preventScroll: true });
  }
}

async function toggleDone(projectId, card) {
  try {
    await mutateCard(projectId, card.id, {
      action: card.done ? "reopen" : "complete",
      revision: card.revision,
    });
    render();
  } catch (error) {
    showToast(error.message);
  }
}

async function moveCard(projectId, card, lane) {
  try {
    await mutateCard(projectId, card.id, { action: "move", lane, revision: card.revision });
    showToast(`Moved to ${lane}`);
    render();
  } catch (error) {
    showToast(error.message);
    render();
  }
}

async function resolveAttention(projectId, item, body) {
  if (pendingResponses.has(item.id)) return;
  pendingResponses.add(item.id);
  const focused = document.activeElement;
  if (focused && main.contains(focused)) focused.blur();
  render();
  try {
    await request(`/api/projects/${projectId}/attention/${encodeURIComponent(item.id)}`, {
      method: "POST",
      body: JSON.stringify({ revision: item.revision, ...body }),
    });
    answerDrafts.delete(item.id);
    showToast("Response sent");
  } catch (error) {
    showToast(error.message);
  } finally {
    pendingResponses.delete(item.id);
    await refreshAll({ quiet: true });
    render();
  }
}

// ---------------------------------------------------------------------------
// Data loading
// ---------------------------------------------------------------------------

async function loadProjects() {
  clearError();
  projects = await request("/api/projects");
  loadedProjects = true;
  setOnline(true);
  updateAttentionShortcut();
  render();
  await refreshAll({ quiet: true });
}

async function refreshAll({ quiet = false } = {}) {
  if (!projects.length) {
    render();
    return;
  }
  const results = await Promise.all(projects.map(async (project) => {
    try {
      const [sessions, snapshot, board] = await Promise.all([
        request(`/api/projects/${project.id}/sessions`),
        request(`/api/projects/${project.id}/activity`),
        request(`/api/projects/${project.id}/board`),
      ]);
      progress.set(project.id, { sessions, snapshot, board, stale: false });
      return true;
    } catch (error) {
      const existing = progress.get(project.id);
      if (existing) existing.stale = true;
      else progress.set(project.id, { sessions: [], snapshot: null, board: null, stale: true });
      if (!quiet) showError(`Progress unavailable: ${error.message}`);
      return false;
    }
  }));
  const allOnline = results.every(Boolean);
  setOnline(allOnline);
  if (allOnline) clearError();
  updateAttentionShortcut();
  scheduleRender();
}

function updateAttentionShortcut() {
  let count = 0;
  for (const project of projects) {
    const snapshot = snapshotOf(project.id);
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
    if (!button) continue;
    button.classList.toggle("hidden", count === 0);
    button.textContent = `${count} need you`;
    button.setAttribute("aria-label", `${count} unresolved requests. Open Home.`);
  }
}

// ---------------------------------------------------------------------------
// Terminal
// ---------------------------------------------------------------------------

const TERMINAL_FONT = '"JetBrainsMono Nerd Font", ui-monospace, SFMono-Regular, Menlo, Consolas, monospace';
const XTERM_THEME = {
  background: "#0f0f0e",
  foreground: "#e4d5aa",
  cursor: "#92705c",
  selectionBackground: "rgba(146,112,92,0.35)",
  black: "#0b0b0b",
  red: "#b79167",
  green: "#d6c783",
  yellow: "#fff3a2",
  blue: "#ad7260",
  magenta: "#e09e74",
  cyan: "#e0e58b",
  white: "#e4d5aa",
  brightBlack: "#696962",
  brightRed: "#d4a570",
  brightGreen: "#f1dd83",
  brightYellow: "#ffee87",
  brightBlue: "#cc836c",
  brightMagenta: "#ffaf79",
  brightCyan: "#f5fb8a",
  brightWhite: "#ebe0bf",
};

/// Strip the terminal's own query replies from typed input: the daemon owns
/// query replies, so a client that also parses must not echo them.
function terminalInputFilter(data) {
  return data
    .replace(/\x1b\[[0-9;?]*[Rcn]/g, "")
    .replace(/\x1b\][^\x07]*(?:\x07|\x1b\\)/gs, "")
    .replace(/\x1bP[\s\S]*?\x1b\\/g, "");
}

/// One attached terminal: the renderer (libghostty-vt's WASM engine when the
/// build ships it, xterm.js otherwise), the WebSocket to the daemon session,
/// reconnect backoff, and input/resize. Reused by the full-screen session view
/// and by every tile on the Agents wall, so the two can never drift.
class TerminalView {
  constructor(projectId, session, options = {}) {
    this.projectId = projectId;
    this.session = session;
    this.readOnly = options.readOnly ?? session.state !== "running";
    this.interactive = options.interactive !== false && !this.readOnly;
    this.onStatus = options.onStatus || (() => {});
    this.onGone = options.onGone || (() => {});
    this.generation = 0;
    this.socket = null;
    this.terminal = null;
    this.fitAddon = null;
    this.observer = null;
    this.reconnectTimer = null;
    this.reconnectAttempts = 0;
    this.applyingRemoteResize = false;
    this.disposed = false;
  }

  mount(container) {
    this.element = container;
    this.terminal = ghosttyEngine
      ? new GhosttyTerminal({ fontFamily: TERMINAL_FONT, fontSize: 13 })
      : new Terminal({
          cursorBlink: true,
          disableStdin: this.readOnly,
          convertEol: false,
          fontFamily: TERMINAL_FONT,
          fontSize: 13,
          scrollback: 5000,
          theme: XTERM_THEME,
        });
    this.fitAddon = new FitAddon();
    this.terminal.loadAddon(this.fitAddon);
    this.terminal.open(container);
    if (this.interactive) {
      this.terminal.onData((data) => this.sendInput(data));
      this.terminal.onBinary((data) =>
        this.sendBytes(Uint8Array.from(data, (char) => char.charCodeAt(0))));
      this.terminal.onResize(({ cols, rows }) => {
        if (!this.applyingRemoteResize) this.sendResize(cols, rows);
      });
    }
    this.observer = new ResizeObserver(() => requestAnimationFrame(() => this.fit()));
    this.observer.observe(container);
    this.fit();
    this.connect(false);
  }

  fit() {
    if (this.disposed) return;
    if (ghosttyEngine) this.terminal?.fit?.();
    else this.fitAddon?.fit();
  }

  focus() {
    this.terminal?.focus?.();
  }

  sendInput(data) {
    if (!this.interactive || !this.socket || this.socket.readyState !== WebSocket.OPEN) return;
    const filtered = terminalInputFilter(data);
    if (filtered) this.socket.send(new TextEncoder().encode(filtered));
  }

  sendBytes(bytes) {
    if (this.socket?.readyState === WebSocket.OPEN && bytes.length) this.socket.send(bytes);
  }

  sendResize(cols, rows) {
    if (!this.interactive || !this.socket || this.socket.readyState !== WebSocket.OPEN || !cols || !rows) return;
    this.socket.send(JSON.stringify({ type: "resize", cols, rows }));
  }

  connect(reset) {
    if (reset) this.terminal?.reset?.();
    const url = new URL(
      `api/projects/${this.projectId}/sessions/${encodeURIComponent(this.session.id)}/terminal`,
      document.baseURI,
    );
    url.protocol = location.protocol === "https:" ? "wss:" : "ws:";
    if (ghosttyEngine) url.searchParams.set("engine", "ghostty");
    const generation = ++this.generation;
    const socket = new WebSocket(url.toString());
    this.socket = socket;
    socket.binaryType = "arraybuffer";
    socket.addEventListener("open", () => {
      if (generation !== this.generation) return;
      this.setStatus(this.readOnly ? "Read-only" : "Connected", !this.readOnly);
      this.fit();
      if (this.interactive) this.sendResize(this.terminal?.cols, this.terminal?.rows);
    });
    socket.addEventListener("message", (message) => {
      if (generation !== this.generation) return;
      if (message.data instanceof ArrayBuffer) {
        const bytes = new Uint8Array(message.data);
        if (ghosttyEngine) {
          // Tagged frames: 1 = lossless snapshot, 0 = raw output.
          if (bytes[0] === 1) this.terminal?.loadSnapshot?.(bytes.subarray(1));
          else this.terminal?.write?.(bytes.subarray(1));
        } else {
          this.terminal?.write?.(bytes);
        }
        return;
      }
      try {
        const status = JSON.parse(message.data);
        if (status.type === "error") this.setStatus(status.message, false, true);
        else if (status.type === "resync_required") this.setStatus("Refreshing…", false, true);
        else if (status.type === "resize") this.applyRemoteResize(status.cols, status.rows);
        else if (status.type === "closed") this.markReadOnly("exited");
      } catch {
        // Non-JSON frames carry no status.
      }
    });
    socket.addEventListener("close", () => {
      if (generation !== this.generation) return;
      this.socket = null;
      if (!this.disposed) void this.retry();
    });
    socket.addEventListener("error", () => {
      if (generation === this.generation) this.setStatus("Connection interrupted…", false, true);
    });
  }

  applyRemoteResize(cols, rows) {
    if (!Number.isInteger(cols) || !Number.isInteger(rows) || cols < 2 || rows < 1) return;
    this.applyingRemoteResize = true;
    try {
      this.terminal?.resize?.(cols, rows);
    } finally {
      this.applyingRemoteResize = false;
    }
  }

  async retry() {
    if (this.readOnly) return;
    try {
      const sessions = await request(`/api/projects/${this.projectId}/sessions`);
      const current = sessions.find((item) => item.id === this.session.id);
      if (!current) {
        this.onGone();
        return;
      }
      if (current.state !== "running") {
        this.markReadOnly(current.state);
        return;
      }
    } catch {
      // A temporary HTTP outage falls through to the regular backoff.
    }
    if (this.disposed) return;
    const delay = Math.min(1000 * 2 ** this.reconnectAttempts, 8000);
    this.reconnectAttempts += 1;
    this.setStatus(`Reconnecting in ${Math.ceil(delay / 1000)}s…`, false, true);
    this.reconnectTimer = window.setTimeout(() => {
      this.reconnectTimer = null;
      if (!this.disposed) this.connect(true);
    }, delay);
  }

  markReadOnly(state) {
    this.readOnly = true;
    this.interactive = false;
    this.session = { ...this.session, state };
    this.setStatus("Read-only");
  }

  reconnectNow() {
    if (this.readOnly || this.disposed) return;
    window.clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    this.reconnectAttempts = 0;
    this.socket?.close();
    this.socket = null;
    this.connect(true);
  }

  setStatus(message, connected = false, error = false) {
    this.onStatus(message, connected, error);
  }

  dispose() {
    this.disposed = true;
    this.generation += 1;
    window.clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    this.observer?.disconnect();
    this.observer = null;
    this.socket?.close();
    this.socket = null;
    this.terminal?.dispose?.();
    this.terminal = null;
    this.fitAddon = null;
  }
}

function openSession(projectId, session) {
  navigate(`#/project/${projectId}/session/${encodeURIComponent(session.id)}`);
}

function enterTerminal(projectId, session) {
  singleTerminal?.dispose();
  terminalOpen = true;
  appShell.classList.add("session-open");
  main.classList.add("hidden");
  $("#terminal-view").classList.remove("hidden");
  $("#terminal-title").textContent = session.title || session.label;

  const status = $("#terminal-connection");
  singleTerminal = new TerminalView(projectId, session, {
    onStatus: (message, connected) => {
      status.textContent = message;
      status.classList.toggle("connected", connected);
    },
    onGone: () => closeTerminal(),
  });
  const readOnly = singleTerminal.readOnly;
  $("#reconnect-button").disabled = readOnly;
  document.querySelectorAll("[data-key]").forEach((button) => { button.disabled = readOnly; });
  status.textContent = readOnly ? "Read-only · use Back to choose another session" : "Connecting…";
  status.classList.remove("connected");

  singleTerminal.mount($("#terminal"));
  singleTerminal.focus();
  for (const button of document.querySelectorAll("[data-key]")) {
    button.onclick = () => {
      singleTerminal?.focus();
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
      if (key) singleTerminal?.sendInput(key);
    };
  }
  window.addEventListener("resize", fitSingleTerminal);
  window.visualViewport?.addEventListener("resize", fitSingleTerminal);
}

function fitSingleTerminal() {
  window.setTimeout(() => {
    singleTerminal?.fit();
    singleTerminal?.focus();
  }, 60);
}

function closeTerminal() {
  const target = lastNonSessionHash || "#/";
  if (location.hash !== target) {
    exitTerminal();
    location.hash = target;
  } else {
    exitTerminal();
    render();
  }
}

/// Tear the full-screen terminal down without touching the route.
function exitTerminal() {
  singleTerminal?.dispose();
  singleTerminal = null;
  terminalOpen = false;
  window.removeEventListener("resize", fitSingleTerminal);
  window.visualViewport?.removeEventListener("resize", fitSingleTerminal);
  $("#terminal-view").classList.add("hidden");
  appShell.classList.remove("session-open");
  main.classList.remove("hidden");
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

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

$("#attention-shortcut").addEventListener("click", () => navigate("#/"));
$("#terminal-attention-shortcut").addEventListener("click", () => {
  lastNonSessionHash = "#/";
  closeTerminal();
});
$("#refresh-button").addEventListener("click", () => refreshAll().catch((error) => showError(error.message)));
$("#back-button").addEventListener("click", closeTerminal);
$("#detach-button").addEventListener("click", closeTerminal);
$("#reconnect-button").addEventListener("click", () => singleTerminal?.reconnectNow());
window.addEventListener("beforeunload", () => {
  singleTerminal?.dispose();
  if (agentsWall) for (const tile of agentsWall.tiles.values()) tile.view.dispose();
});

loadProjects().catch((error) => {
  loadedProjects = true;
  setOnline(false);
  showError(error.message);
  render();
});

window.setInterval(() => {
  if (terminalOpen) return;
  refreshAll({ quiet: true }).catch(() => {});
}, 3000);
