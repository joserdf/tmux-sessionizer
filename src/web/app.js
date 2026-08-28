"use strict";

const $ = (id) => document.getElementById(id);

const STATUS_LABEL = {
  running: "running",
  waiting_input: "idle",
  waiting_permission: "attention!",
  finished: "finished",
};

let statePoll = null;
let outputPoll = null;
let current = null; // { tmuxName, title }
let killArmed = false;

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

/// A destructive inline button with a two-tap confirm: the first tap arms it
/// ("✕" → "sure?"), a second tap within 3s runs `onConfirm(btn)`. `stopPropagation`
/// keeps taps from bubbling to an enclosing row/card.
function confirmButton(className, label, onConfirm) {
  const btn = el("button", className, label);
  let armed = false;
  let timer = null;
  btn.onclick = (e) => {
    e.stopPropagation();
    if (!armed) {
      armed = true;
      btn.classList.add("confirm");
      btn.textContent = "sure?";
      timer = setTimeout(() => {
        armed = false;
        btn.classList.remove("confirm");
        btn.textContent = label;
      }, 3000);
      return;
    }
    clearTimeout(timer);
    onConfirm(btn);
  };
  return btn;
}

function diffSpan(diff, className) {
  const span = el("span", className || "diff");
  if (!diff || (diff.added === 0 && diff.removed === 0)) return span;
  const plus = el("span", "diff-added", `+${diff.added}`);
  const minus = el("span", "diff-removed", ` -${diff.removed}`);
  span.append(plus, minus);
  return span;
}

// ---------- list view ----------

// Projects start collapsed; only explicitly expanded ones are remembered.
const expanded = new Set(JSON.parse(localStorage.getItem("cm-expanded") || "[]"));
let lastProjects = [];

function toggleCollapse(name) {
  if (expanded.has(name)) expanded.delete(name);
  else expanded.add(name);
  localStorage.setItem("cm-expanded", JSON.stringify([...expanded]));
  renderProjects(lastProjects);
}

async function refreshState() {
  let state;
  try {
    state = await api("/api/state");
  } catch (e) {
    $("loading") && ($("loading").textContent = "connection lost — retrying…");
    return;
  }
  $("host").textContent = `@${state.host || "?"}`;
  lastProjects = state.projects || [];
  renderProjects(lastProjects);
}

function renderProjects(projects) {
  const root = $("projects");
  root.textContent = "";

  if (projects.length === 0) {
    root.append(el("div", "empty", "no projects configured"));
    return;
  }

  for (const p of projects) {
    const proj = el("section", "project");
    const isCollapsed = !expanded.has(p.name);
    const tasks = (p.tasks || []).filter((t) => !t.archived);
    const adhoc = p.adhoc_sessions || [];

    const header = el("div", "project-header");
    const toggle = el("button", "project-toggle");
    toggle.append(el("span", "chevron", isCollapsed ? "▸" : "▾"));
    const name = el("span", "project-name", p.name);
    if (p.branch) name.append(el("span", "project-branch", `⎇ ${p.branch}`));
    toggle.append(name);
    toggle.onclick = () => toggleCollapse(p.name);
    const actions = el("div", "project-actions");
    const addBtn = el("button", "new-task-btn", "+ task");
    addBtn.onclick = () => openSheet({ mode: "task", project: p.name });
    const addAdhocBtn = el("button", "new-task-btn", "+ adhoc");
    addAdhocBtn.onclick = () => openSheet({ mode: "adhoc", project: p.name });
    actions.append(addBtn, addAdhocBtn);
    header.append(toggle, actions);
    proj.append(header);

    if (isCollapsed) {
      const sessions = [...tasks.flatMap((t) => t.sessions || []), ...adhoc];
      const summary = el("button", "project-summary");
      summary.append(el("span", `dot ${urgentStatus(sessions)}`));
      summary.append(
        el("span", "label", `${tasks.length} task${tasks.length === 1 ? "" : "s"} · ${sessions.length} session${sessions.length === 1 ? "" : "s"}`)
      );
      summary.onclick = () => toggleCollapse(p.name);
      proj.append(summary);
      root.append(proj);
      continue;
    }

    for (const t of tasks) proj.append(renderTask(p, t));

    for (const s of adhoc) {
      const card = el("div", "task");
      card.append(sessionRow(s, `adhoc · ${s.name}`));
      proj.append(card);
    }

    if (tasks.length === 0 && adhoc.length === 0) {
      proj.append(el("div", "empty", "no tasks"));
    }

    root.append(proj);
  }
}

/// Most attention-worthy status across sessions, for the collapsed summary dot.
function urgentStatus(sessions) {
  const order = ["waiting_permission", "waiting_input", "running", "finished"];
  for (const status of order) {
    if (sessions.some((s) => s.status === status)) return status;
  }
  return "";
}

function renderTask(project, task) {
  const card = el("div", "task");

  const header = el("div", "task-header");
  header.append(el("div", "task-name", task.name));

  const meta = el("div", "task-meta");
  const taskDiff = diffSpan(task.diff);
  if (taskDiff.childNodes.length) {
    taskDiff.classList.add("tappable");
    taskDiff.onclick = () =>
      openDiff(
        `project=${encodeURIComponent(project.name)}&branch=${encodeURIComponent(task.branch)}`,
        `${task.name} · vs ${task.base_branch}`
      );
  }
  meta.append(taskDiff);
  if (task.pr_url) {
    const a = el("a", "pr-link", "PR ↗");
    a.href = task.pr_url;
    a.target = "_blank";
    meta.append(a);
  }
  meta.append(
    confirmButton("row-del", "✕", async (btn) => {
      btn.disabled = true;
      try {
        await post("/api/tasks/delete", { project: project.name, task: task.name });
        toast(`task '${task.name}' deleted`);
        refreshState();
      } catch (e) {
        toast(e.message, true);
        btn.disabled = false;
      }
    })
  );
  header.append(meta);
  card.append(header);

  for (const s of task.sessions || []) {
    card.append(sessionRow(s, `${task.name} · ${s.name}`));
  }

  const add = el("button", "add-session", "+ session");
  add.onclick = async () => {
    add.disabled = true;
    add.textContent = "creating…";
    try {
      await post("/api/sessions", { project: project.name, task: task.name });
      toast("session created");
      refreshState();
    } catch (e) {
      toast(e.message, true);
      add.disabled = false;
      add.textContent = "+ session";
    }
  };
  card.append(add);

  return card;
}

function sessionRow(session, title) {
  const isMain = session.name === "main";
  const wrap = el("div", "session-row-wrap");
  const row = el("button", "session-row");
  const dot = el("span", `dot ${session.status || ""}`);
  const label = el("span", isMain ? "label main" : "label", isMain ? "◆ main" : `#${session.name}`);
  const status = el(
    "span",
    `status-label ${session.status || ""}`,
    STATUS_LABEL[session.status] || "…"
  );
  row.append(dot, label, diffSpan(session.diff, "diff"), status, el("span", "chev", "›"));
  row.onclick = () => openSession(session.tmux_name, title, session.status);

  if (isMain) {
    // The main session belongs to the task — delete the task to remove it.
    wrap.append(row);
    return wrap;
  }

  const del = confirmButton("row-del", "✕", async (btn) => {
    btn.disabled = true;
    try {
      await post(`/api/sessions/${encodeURIComponent(session.tmux_name)}/kill`);
      toast("session killed");
      refreshState();
    } catch (e) {
      toast(e.message, true);
      btn.disabled = false;
    }
  });

  wrap.append(row, del);
  return wrap;
}

// ---------- session view ----------

function openSession(tmuxName, title, status) {
  current = { tmuxName, title };
  killArmed = false;
  $("kill-btn").textContent = "kill";
  $("kill-btn").classList.remove("confirm");
  $("sv-title").textContent = title;
  setStatus(status);
  $("term").textContent = "";
  lastRaw = null;
  $("session-view").hidden = false;
  refreshOutput(true);
  clearInterval(outputPoll);
  outputPoll = setInterval(refreshOutput, 1500);
}

function closeSession() {
  current = null;
  clearInterval(outputPoll);
  outputPoll = null;
  $("session-view").hidden = true;
  refreshState();
}

function setStatus(status) {
  $("sv-dot").className = `dot ${status || ""}`;
  $("sv-status").textContent = STATUS_LABEL[status] || "…";
}

function termPinnedToBottom() {
  const t = $("term");
  return t.scrollHeight - t.scrollTop - t.clientHeight < 60;
}

// ---------- ANSI rendering ----------

const TERM_FG = "#aeb8cf";
const TERM_BG = "#0b0d12";

const BASE16 = [
  "#20242f", "#f26d78", "#7fd962", "#ffb454",
  "#73a3f2", "#c792ea", "#5ccfe6", "#cdd5e8",
  "#4a5370", "#ff8189", "#98e878", "#ffcc7b",
  "#8fb8ff", "#ddb3f5", "#7ee0f0", "#eef1f8",
];

function color256(n) {
  if (n < 16) return BASE16[n];
  if (n < 232) {
    const c = n - 16;
    const steps = [0, 95, 135, 175, 215, 255];
    return `rgb(${steps[Math.floor(c / 36)]},${steps[Math.floor(c / 6) % 6]},${steps[c % 6]})`;
  }
  const v = 8 + (n - 232) * 10;
  return `rgb(${v},${v},${v})`;
}

function colorOf(c) {
  if (c == null) return null;
  return typeof c === "number" ? color256(c) : c;
}

function applySgr(s, params) {
  const p = params.length ? params.split(/[;:]/).map((x) => Number(x) || 0) : [0];
  for (let i = 0; i < p.length; i++) {
    const n = p[i];
    if (n === 0) {
      s.fg = s.bg = null;
      s.bold = s.dim = s.italic = s.underline = s.strike = s.reverse = false;
    } else if (n === 1) s.bold = true;
    else if (n === 2) s.dim = true;
    else if (n === 3) s.italic = true;
    else if (n === 4) s.underline = true;
    else if (n === 7) s.reverse = true;
    else if (n === 9) s.strike = true;
    else if (n === 22) s.bold = s.dim = false;
    else if (n === 23) s.italic = false;
    else if (n === 24) s.underline = false;
    else if (n === 27) s.reverse = false;
    else if (n === 29) s.strike = false;
    else if (n >= 30 && n <= 37) s.fg = n - 30;
    else if (n === 39) s.fg = null;
    else if (n >= 40 && n <= 47) s.bg = n - 40;
    else if (n === 49) s.bg = null;
    else if (n >= 90 && n <= 97) s.fg = n - 82;
    else if (n >= 100 && n <= 107) s.bg = n - 92;
    else if (n === 38 || n === 48) {
      const set = (v) => (n === 38 ? (s.fg = v) : (s.bg = v));
      if (p[i + 1] === 5) {
        set(p[i + 2]);
        i += 2;
      } else if (p[i + 1] === 2) {
        set(`rgb(${p[i + 2]},${p[i + 3]},${p[i + 4]})`);
        i += 4;
      }
    }
  }
}

function styleFor(s) {
  let fg = s.fg;
  if (s.bold && typeof fg === "number" && fg < 8) fg += 8;
  let fgc = colorOf(fg);
  let bgc = colorOf(s.bg);
  if (s.reverse) {
    const nf = bgc || TERM_BG;
    bgc = fgc || TERM_FG;
    fgc = nf;
  }
  let st = "";
  if (fgc) st += `color:${fgc};`;
  if (bgc) st += `background:${bgc};`;
  if (s.bold) st += "font-weight:600;";
  if (s.dim) st += "opacity:.55;";
  if (s.italic) st += "font-style:italic;";
  if (s.underline) st += "text-decoration:underline;";
  else if (s.strike) st += "text-decoration:line-through;";
  return st;
}

const ANSI_RE =
  // SGR (captured) | other CSI | OSC | charset/keypad escapes
  /\x1b\[([0-9;:]*)m|\x1b\[[0-9;?]*[a-zA-Z]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[()][A-Z0-9]|\x1b[=>]/g;

function escapeHtml(text) {
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function ansiToHtml(raw) {
  raw = raw.replace(/\r/g, "");
  const s = {
    fg: null, bg: null,
    bold: false, dim: false, italic: false,
    underline: false, strike: false, reverse: false,
  };
  let html = "";
  let idx = 0;

  const flush = (text) => {
    if (!text) return;
    const esc = escapeHtml(text);
    const style = styleFor(s);
    html += style ? `<span style="${style}">${esc}</span>` : esc;
  };

  ANSI_RE.lastIndex = 0;
  let m;
  while ((m = ANSI_RE.exec(raw))) {
    flush(raw.slice(idx, m.index));
    idx = m.index + m[0].length;
    if (m[1] !== undefined) applySgr(s, m[1]);
  }
  flush(raw.slice(idx));
  return html;
}

/// Scale the terminal font so `cols` columns fit the screen width
/// (IBM Plex Mono advance width is 0.6em). Below the readable minimum we
/// keep the minimum and allow horizontal scrolling.
function fitTerm(cols) {
  const t = $("term");
  const avail = t.clientWidth - 20; // horizontal padding
  const size = Math.min(13, Math.max(7, avail / (cols * 0.6)));
  t.style.fontSize = `${size.toFixed(2)}px`;
}

let lastRaw = null;
let lastCols = 80;

async function refreshOutput(force) {
  if (!current) return;
  try {
    const data = await api(
      `/api/sessions/${encodeURIComponent(current.tmuxName)}/output?lines=1000`
    );
    const t = $("term");
    const pinned = force || termPinnedToBottom();
    const raw = (data.text || "").replace(/\s+$/, "");
    if (raw !== lastRaw) {
      lastRaw = raw;
      lastCols = data.width || 80;
      t.innerHTML = ansiToHtml(raw);
      fitTerm(lastCols);
      if (pinned) t.scrollTop = t.scrollHeight;
    }
  } catch (e) {
    // session likely gone
  }
}

async function refreshSessionStatus() {
  if (!current) return;
  try {
    const state = await api("/api/state");
    $("host").textContent = `@${state.host || "?"}`;
    for (const p of state.projects || []) {
      const all = [...(p.tasks || []).flatMap((t) => t.sessions || []), ...(p.adhoc_sessions || [])];
      const found = all.find((s) => s.tmux_name === current.tmuxName);
      if (found) {
        setStatus(found.status);
        return;
      }
    }
  } catch (e) {}
}

async function sendMessage() {
  if (!current) return;
  const msg = $("msg");
  const text = msg.value.trim();
  if (!text) return;
  $("send-btn").disabled = true;
  try {
    await post(`/api/sessions/${encodeURIComponent(current.tmuxName)}/send`, {
      text,
      submit: true,
    });
    msg.value = "";
    msg.style.height = "auto";
    setTimeout(() => refreshOutput(true), 400);
  } catch (e) {
    toast(e.message, true);
  } finally {
    $("send-btn").disabled = false;
  }
}

async function sendKey(key) {
  if (!current) return;
  try {
    await post(`/api/sessions/${encodeURIComponent(current.tmuxName)}/keys`, { key });
    setTimeout(() => refreshOutput(true), 300);
  } catch (e) {
    toast(e.message, true);
  }
}

async function killSession() {
  if (!current) return;
  const btn = $("kill-btn");
  if (!killArmed) {
    killArmed = true;
    btn.textContent = "sure?";
    btn.classList.add("confirm");
    setTimeout(() => {
      killArmed = false;
      btn.textContent = "kill";
      btn.classList.remove("confirm");
    }, 3000);
    return;
  }
  btn.disabled = true;
  try {
    await post(`/api/sessions/${encodeURIComponent(current.tmuxName)}/kill`);
    toast("session killed");
    closeSession();
  } catch (e) {
    toast(e.message, true);
  } finally {
    btn.disabled = false;
  }
}

// ---------- diff view ----------

let diffQuery = null;

function openDiff(query, title) {
  diffQuery = query;
  $("dv-title").textContent = title;
  $("dv-stats").textContent = "";
  $("diff-body").textContent = "loading…";
  $("diff-view").hidden = false;
  loadDiff();
}

function closeDiff() {
  diffQuery = null;
  $("diff-view").hidden = true;
}

async function loadDiff() {
  if (!diffQuery) return;
  try {
    const data = await api(`/api/diff?${diffQuery}`);
    renderDiff(data.text || "");
  } catch (e) {
    $("diff-body").textContent = "";
    $("diff-body").append(el("div", "empty", `no diff: ${e.message}`));
  }
}

function renderDiff(text) {
  const body = $("diff-body");
  body.textContent = "";

  const files = parseDiff(text);
  let added = 0;
  let removed = 0;
  for (const f of files) {
    added += f.added;
    removed += f.removed;
  }
  const stats = $("dv-stats");
  stats.textContent = "";
  stats.append(
    el("span", "diff-added", `+${added}`),
    el("span", "", " "),
    el("span", "diff-removed", `-${removed}`),
    el("span", "", ` · ${files.length} file${files.length === 1 ? "" : "s"}`)
  );

  if (files.length === 0) {
    body.append(el("div", "empty", "no changes"));
    return;
  }

  for (const f of files) {
    const card = el("div", "file-card");
    const header = el("button", "file-header");
    header.append(el("span", "file-path", f.path));
    const meta = el("span", "task-meta");
    meta.append(el("span", "diff-added", `+${f.added}`), el("span", "diff-removed", `-${f.removed}`));
    header.append(meta);
    const content = el("pre", "file-diff");
    let html = "";
    for (const line of f.lines) {
      const esc = escapeHtml(line) || " ";
      let cls = "dl";
      if (line.startsWith("+")) cls += " d-add";
      else if (line.startsWith("-")) cls += " d-del";
      else if (line.startsWith("@@")) cls += " d-hunk";
      html += `<span class="${cls}">${esc}</span>`;
    }
    content.innerHTML = html;
    header.onclick = () => {
      content.hidden = !content.hidden;
    };
    card.append(header, content);
    body.append(card);
  }
}

/// Split a unified diff into per-file chunks with counts; drops git metadata
/// lines before the first hunk of each file.
function parseDiff(text) {
  const files = [];
  let cur = null;
  let inHunk = false;
  for (const line of text.split("\n")) {
    if (line.startsWith("diff --git ")) {
      const path = (line.match(/ b\/(.*)$/) || [])[1] || line.slice(11);
      cur = { path, lines: [], added: 0, removed: 0 };
      files.push(cur);
      inHunk = false;
      continue;
    }
    if (!cur) continue;
    if (line.startsWith("@@")) inHunk = true;
    if (!inHunk) {
      if (line.startsWith("new file")) cur.lines.push(line);
      else if (line.startsWith("deleted file")) cur.lines.push(line);
      else if (line.startsWith("rename ")) cur.lines.push(line);
      continue;
    }
    cur.lines.push(line);
    if (line.startsWith("+") && !line.startsWith("+++")) cur.added++;
    else if (line.startsWith("-") && !line.startsWith("---")) cur.removed++;
  }
  return files;
}

// ---------- new-task sheet ----------

let sheetCtx = null;

function openSheet(ctx) {
  sheetCtx = ctx;
  const mode = ctx.mode;
  const title =
    mode === "project"
      ? "add project"
      : mode === "adhoc"
        ? `new adhoc session · ${ctx.project}`
        : `new task · ${ctx.project}`;
  $("sheet-title").textContent = title;
  $("sheet-path").hidden = mode !== "project";
  $("sheet-path").value = "";
  $("sheet-prompt").hidden = mode !== "task";
  $("sheet-prompt").value = "";
  $("sheet-name").value = "";
  $("sheet-name").placeholder =
    mode === "project"
      ? "project name (optional)"
      : mode === "adhoc"
        ? "adhoc session name"
        : "task name";
  $("sheet-create").textContent = mode === "project" ? "add" : "create";
  $("sheet-backdrop").hidden = false;
  (mode === "project" ? $("sheet-path") : $("sheet-name")).focus();
}

function closeSheet() {
  sheetCtx = null;
  $("sheet-backdrop").hidden = true;
}

function createFromSheet() {
  if (!sheetCtx) return;
  if (sheetCtx.mode === "project") return createProjectFromSheet();
  if (sheetCtx.mode === "adhoc") return createAdhocFromSheet();
  return createTaskFromSheet();
}

async function createAdhocFromSheet() {
  const name = $("sheet-name").value.trim();
  if (!name) {
    toast("session name required", true);
    return;
  }
  const btn = $("sheet-create");
  btn.disabled = true;
  btn.textContent = "creating…";
  try {
    await post("/api/adhoc", { project: sheetCtx.project, name });
    toast(`adhoc session '${name}' created`);
    closeSheet();
    refreshState();
  } catch (e) {
    toast(e.message, true);
  } finally {
    btn.disabled = false;
    btn.textContent = "create";
  }
}

async function createTaskFromSheet() {
  const name = $("sheet-name").value.trim();
  if (!name) {
    toast("task name required", true);
    return;
  }
  const btn = $("sheet-create");
  btn.disabled = true;
  btn.textContent = "creating…";
  try {
    await post("/api/tasks", {
      project: sheetCtx.project,
      name,
      prompt: $("sheet-prompt").value.trim() || null,
    });
    toast(`task '${name}' created`);
    closeSheet();
    refreshState();
  } catch (e) {
    toast(e.message, true);
  } finally {
    btn.disabled = false;
    btn.textContent = "create";
  }
}

async function createProjectFromSheet() {
  const path = $("sheet-path").value.trim();
  if (!path) {
    toast("project path required", true);
    return;
  }
  const btn = $("sheet-create");
  btn.disabled = true;
  btn.textContent = "adding…";
  try {
    const res = await post("/api/projects", {
      path,
      name: $("sheet-name").value.trim() || null,
    });
    const name = (res && res.name) || "project";
    // Expand the freshly added project so its (empty) task list is visible.
    expanded.add(name);
    localStorage.setItem("cm-expanded", JSON.stringify([...expanded]));
    toast(`project '${name}' added`);
    closeSheet();
    refreshState();
  } catch (e) {
    toast(e.message, true);
  } finally {
    btn.disabled = false;
    btn.textContent = "add";
  }
}

// ---------- wiring ----------

$("add-project-btn").onclick = () => openSheet({ mode: "project" });
$("back-btn").onclick = closeSession;
$("kill-btn").onclick = killSession;
$("sv-diff-btn").onclick = () => {
  if (current) {
    openDiff(`session=${encodeURIComponent(current.tmuxName)}`, `${current.title} · diff`);
  }
};
$("diff-back-btn").onclick = closeDiff;
$("diff-refresh-btn").onclick = loadDiff;
$("send-btn").onclick = sendMessage;
$("sheet-cancel").onclick = closeSheet;
$("sheet-create").onclick = createFromSheet;
$("sheet-backdrop").onclick = (e) => {
  if (e.target === $("sheet-backdrop")) closeSheet();
};

for (const btn of $("quick-keys").querySelectorAll("button")) {
  btn.onclick = () => sendKey(btn.dataset.key);
}

const msg = $("msg");
msg.addEventListener("input", () => {
  msg.style.height = "auto";
  msg.style.height = `${Math.min(msg.scrollHeight, 120)}px`;
});

refreshState();
statePoll = setInterval(() => {
  if (current) refreshSessionStatus();
  else refreshState();
}, 3000);

window.addEventListener("resize", () => {
  if (current) fitTerm(lastCols);
});

document.addEventListener("visibilitychange", () => {
  if (!document.hidden) {
    if (current) refreshOutput(true);
    else refreshState();
  }
});
