"use strict";

const $ = (id) => document.getElementById(id);

const COLUMN_LABELS = {
  backlog: "backlog",
  ready: "ready",
  running: "running",
  needs_you: "needs you",
  review: "review",
  done: "done",
};

let boards = [];
let selectedName = null;
let paused = false;
let dragging = null; // { id, from }
let refreshPending = false;

// ---------- helpers ----------

async function api(path, opts) {
  const res = await fetch(path, opts);
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(text || `${res.status} ${res.statusText}`);
  }
  const ct = res.headers.get("content-type") || "";
  return ct.includes("json") ? res.json() : null;
}

function post(path, body) {
  return api(path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body || {}),
  });
}

let toastTimer = null;
function toast(msg, isError) {
  const el = $("toast");
  el.textContent = msg;
  el.className = isError ? "error" : "";
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (el.hidden = true), 3500);
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = text;
  return node;
}

function currentBoard() {
  if (!selectedName && boards.length) selectedName = boards[0].name;
  return boards.find((b) => b.name === selectedName) || boards[0] || null;
}

// ---------- data ----------

async function refresh() {
  let data;
  try {
    data = await api("/api/v2/boards");
  } catch (e) {
    $("loading") && ($("loading").textContent = "connection lost — retrying…");
    return;
  }
  boards = data || [];
  const board = currentBoard();
  if (board) paused = !!board.paused;
  render();
}

/// Debounced re-fetch (SSE events can arrive in bursts).
function scheduleRefresh() {
  if (refreshPending) return;
  refreshPending = true;
  setTimeout(() => {
    refreshPending = false;
    refresh();
  }, 1000);
}

// ---------- rendering ----------

function render() {
  const board = currentBoard();
  const select = $("board-select");
  select.textContent = "";
  for (const b of boards) {
    const opt = el("option", null, b.name);
    opt.value = b.name;
    if (b.name === selectedName) opt.selected = true;
    select.append(opt);
  }
  $("board-project").textContent = board ? board.project : "";
  $("pause-chip").hidden = !paused;
  $("pause-btn").textContent = paused ? "resume" : "pause";

  const root = $("board");
  root.textContent = "";

  if (!board) {
    root.append(el("div", "empty", "no boards configured"));
    return;
  }

  for (const col of board.columns || []) {
    root.append(columnEl(col));
  }
}

function columnEl(col) {
  const colName = col.column;
  const node = el("div", `column col-${colName}`);
  node.dataset.column = colName;

  const header = el("div", "column-header");
  header.append(el("span", "column-name", COLUMN_LABELS[colName] || colName));
  header.append(el("span", "column-count", String((col.cards || []).length)));
  node.append(header);

  const cards = el("div", "column-cards");
  for (const card of col.cards || []) cards.append(cardEl(card));
  if (!(col.cards || []).length) cards.append(el("div", "empty-lane", "—"));
  node.append(cards);

  node.addEventListener("dragover", (e) => {
    e.preventDefault();
    node.classList.add("over");
  });
  node.addEventListener("dragleave", () => node.classList.remove("over"));
  node.addEventListener("drop", (e) => {
    e.preventDefault();
    node.classList.remove("over");
    if (!dragging) return;
    const { id, from } = dragging;
    dragging = null;
    if (from === colName) return;
    moveCard(id, from, colName);
  });
  return node;
}

function cardEl(card) {
  const node = el("div", "card");
  node.draggable = true;
  node.dataset.id = card.id;

  const top = el("div", "card-top");
  top.append(el("span", "card-key", card.key || card.id));
  if (card.priority) top.append(el("span", "card-priority", `P${card.priority}`));
  node.append(top);

  if (card.title) node.append(el("div", "card-title", card.title));

  if (card.labels && card.labels.length) {
    const labels = el("div", "card-labels");
    for (const l of card.labels) labels.append(el("span", "tag", l));
    node.append(labels);
  }

  if (card.session) {
    node.append(el("span", "card-session", `⌂ ${card.session}`));
  }

  node.addEventListener("dragstart", () => {
    dragging = { id: card.id, from: card.column };
    node.classList.add("dragging");
  });
  node.addEventListener("dragend", () => {
    node.classList.remove("dragging");
    for (const c of document.querySelectorAll(".column.over")) c.classList.remove("over");
    dragging = null;
  });
  return node;
}

async function moveCard(id, from, column) {
  const board = currentBoard();
  if (!board) return;
  try {
    await post(`/api/v2/boards/${encodeURIComponent(board.name)}/cards/move`, {
      id,
      column,
    });
    toast(`${COLUMN_LABELS[from] || from} → ${COLUMN_LABELS[column] || column}`);
    refresh();
  } catch (e) {
    toast(e.message, true);
    refresh(); // revert to the server's view of the board
  }
}

// ---------- actions ----------

function summarize(summary) {
  const parts = [];
  for (const b of summary.boards || []) {
    if ((b.dispatched || []).length) {
      parts.push(`${b.name}: ${b.dispatched.length} dispatched`);
    }
  }
  if (!parts.length) return "nothing to dispatch";
  return parts.join(" · ");
}

async function runDispatch(dryRun) {
  const btn = dryRun ? $("dry-run-btn") : $("dispatch-btn");
  btn.disabled = true;
  btn.textContent = dryRun ? "checking…" : "dispatching…";
  try {
    const summary = await post("/api/v2/boards/dispatch", { dry_run: dryRun });
    const msg = dryRun
      ? `dry run → ${summarize(summary || {})}`
      : `dispatched ${(summary && summary.dispatched) || 0}`;
    toast(msg);
    refresh();
  } catch (e) {
    toast(e.message, true);
  } finally {
    btn.disabled = false;
    btn.textContent = dryRun ? "dry run" : "dispatch now";
  }
}

async function togglePause() {
  const btn = $("pause-btn");
  btn.disabled = true;
  try {
    await post("/api/v2/boards/pause", { paused: !paused });
    refresh();
  } catch (e) {
    toast(e.message, true);
  } finally {
    btn.disabled = false;
  }
}

// ---------- wiring ----------

$("board-select").onchange = (e) => {
  selectedName = e.target.value;
  render();
};
$("dispatch-btn").onclick = () => runDispatch(false);
$("dry-run-btn").onclick = () => runDispatch(true);
$("pause-btn").onclick = togglePause;

let eventSource = null;
function openStateStream() {
  if (eventSource || typeof EventSource === "undefined") return;
  try {
    eventSource = new EventSource("/events");
  } catch (e) {
    return; // fall back to the poll
  }
  // Any worker state change can imply a board change (dispatch, status moves,
  // session creation). Re-fetch on any event, debounced.
  eventSource.addEventListener("state", scheduleRefresh);
}

refresh();
openStateStream();
// A slow poll is a safety net for browsers without EventSource or a wedged stream.
setInterval(refresh, 5000);
