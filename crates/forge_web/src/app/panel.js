/* ---------- activity + todo panel (right rail) ---------- */
const tasksPanel = $('tasks-panel');
function setTasksOpen(open) {
  tasksPanel.classList.toggle('hidden', !open);
  localStorage.setItem('forge-tasks-open', open ? '1' : '0');
  if (open) { loadTasks(); loadPipelines(); loadTodos(); }
}
$('tasks-toggle').onclick = () => setTasksOpen(tasksPanel.classList.contains('hidden'));
$('tasks-close').onclick = () => setTasksOpen(false);

// Collapsible sections, persisted across reloads.
for (const [sec, key] of [['sec-activity', 'forge-sec-activity'], ['sec-todo', 'forge-sec-todo']]) {
  const el = $(sec);
  if (localStorage.getItem(key) === '1') el.classList.add('closed');
  el.querySelector('.p-head').onclick = (e) => {
    if (e.target.closest('button')) return; // the +Add button lives in the header
    el.classList.toggle('closed');
    localStorage.setItem(key, el.classList.contains('closed') ? '1' : '0');
  };
}

function fmtDur(s) {
  s = Math.max(0, Math.round(s));
  if (s < 60) return s + 's';
  const m = Math.floor(s / 60);
  return m < 60 ? m + 'm ' + (s % 60) + 's' : Math.floor(m / 60) + 'h ' + (m % 60) + 'm';
}
function fmtClock(ms) {
  const d = new Date(ms);
  return d.getHours().toString().padStart(2, '0') + ':' + d.getMinutes().toString().padStart(2, '0');
}
function runItem({ name, desc, status, statusText, title, onClick }) {
  const item = document.createElement('div'); item.className = 'run-item';
  const top = document.createElement('div'); top.className = 'r-top';
  const nm = document.createElement('span'); nm.className = 'r-name'; nm.textContent = name;
  const st = document.createElement('span'); st.className = 'r-status ' + status; st.textContent = statusText;
  top.append(nm, st); item.appendChild(top);
  if (desc && desc !== name) {
    const d = document.createElement('div'); d.className = 'r-desc'; d.textContent = desc;
    item.appendChild(d);
  }
  if (title) item.title = title;
  if (onClick) item.onclick = onClick;
  return item;
}
const noneEl = () => { const n = document.createElement('div'); n.className = 'act-none'; n.textContent = 'none'; return n; };
function convoTitle(id) { const c = convoCache.find((c) => c.id === id); return (c && c.title) || 'chat'; }

async function loadTasks() {
  if (tasksPanel.classList.contains('hidden') || document.hidden) return;
  let d;
  try { d = await (await api('/api/tasks')).json(); } catch (e) { return; }
  const run = $('running-list'); run.innerHTML = '';
  $('running-count').textContent = d.running.length ? '(' + d.running.length + ')' : '';
  if (!d.running.length) run.appendChild(noneEl());
  for (const t of d.running) {
    run.appendChild(runItem({
      name: convoTitle(t.conversation_id), desc: t.prompt,
      status: 'running', statusText: 'running',
      title: 'started ' + fmtClock(t.started_at_ms) + ' · ' + fmtDur(t.elapsed_secs),
      onClick: () => selectConversation(t.conversation_id, convoTitle(t.conversation_id)),
    }));
  }
  const rec = $('recent-list'); rec.innerHTML = '';
  if (!d.recent.length) rec.appendChild(noneEl());
  for (const t of d.recent.slice(0, 8)) {
    rec.appendChild(runItem({
      name: convoTitle(t.conversation_id), desc: t.prompt,
      status: t.status,
      statusText: { completed: 'done', stopped: 'stopped', error: 'error' }[t.status] || t.status,
      title: fmtClock(t.started_at_ms) + ' · ' + fmtDur(t.duration_secs),
      onClick: () => selectConversation(t.conversation_id, convoTitle(t.conversation_id)),
    }));
  }
}

// Running pipelines = queued / in-progress GitHub Actions runs on this repo
// (reuses the GitHub connection). Polled slowly — it hits the GitHub API.
async function loadPipelines() {
  if (tasksPanel.classList.contains('hidden') || document.hidden) return;
  let d;
  try { d = await (await api('/api/pipelines')).json(); } catch (e) { return; }
  const box = $('pipeline-list'); box.innerHTML = '';
  if (!d.pipelines.length) { box.appendChild(noneEl()); return; }
  for (const p of d.pipelines) {
    box.appendChild(runItem({
      name: p.name, desc: p.branch,
      status: 'running', statusText: p.status === 'queued' ? 'queued' : 'running',
      onClick: p.url ? () => window.open(p.url, '_blank', 'noopener') : null,
    }));
  }
}

/* ----- TODOs ----- */
async function loadTodos() {
  let d;
  try { d = await (await api('/api/todos')).json(); } catch (e) { return; }
  const open = d.todos.filter((t) => !t.done);
  $('todo-count').textContent = open.length;
  const list = $('todo-list'); list.innerHTML = '';
  if (!d.todos.length) {
    const e = document.createElement('div'); e.className = 'todo-empty';
    e.textContent = 'No open TODOs. Add one above, or tell chat “add a todo: …”.';
    list.appendChild(e); return;
  }
  // Open items first; completed ones stay visible, struck through, and can
  // be reopened by unchecking. × deletes for real.
  for (const t of [...open, ...d.todos.filter((x) => x.done)]) {
    const item = document.createElement('div'); item.className = 'todo-item' + (t.done ? ' done' : '');
    const cb = document.createElement('input'); cb.type = 'checkbox'; cb.checked = !!t.done;
    cb.title = t.done ? 'Reopen' : 'Mark done';
    cb.onchange = async () => {
      await api('/api/todos/' + t.id, { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ done: cb.checked }) });
      loadTodos();
    };
    const tx = document.createElement('span'); tx.className = 'tx'; tx.textContent = t.text;
    const rm = document.createElement('button'); rm.className = 'rm'; rm.textContent = '×'; rm.title = 'Delete';
    rm.onclick = async () => { await api('/api/todos/' + t.id, { method: 'DELETE' }); loadTodos(); };
    item.append(cb, tx, rm);
    list.appendChild(item);
  }
}
async function addTodo(text) {
  text = (text || '').trim(); if (!text) return false;
  const res = await api('/api/todos', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ text }) });
  if (res.ok) loadTodos();
  return res.ok;
}
// The header button flips to "Cancel" while the input row is open, so
// closing it again is obvious (Escape works too).
function setAddOpen(open) {
  const row = $('todo-add-row'), btn = $('todo-add-btn');
  row.classList.toggle('open', open);
  btn.textContent = open ? 'Cancel' : '+ Add';
  btn.classList.toggle('btn-primary', !open);
  btn.classList.toggle('btn-ghost', open);
  if (open) $('todo-input').focus();
}
$('todo-add-btn').onclick = () => {
  $('sec-todo').classList.remove('closed');
  setAddOpen(!$('todo-add-row').classList.contains('open'));
};
const submitTodoInput = async () => { if (await addTodo($('todo-input').value)) $('todo-input').value = ''; };
$('todo-save').onclick = submitTodoInput;
$('todo-input').addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && !e.isComposing && e.keyCode !== 229) submitTodoInput();
  if (e.key === 'Escape') setAddOpen(false);
});
// Chat shortcut: "add a todo: …" / "todo: …" / "待办：…" goes straight to
// the list without waking the agent.
function tryTodoIntercept(text) {
  const m = text.match(/^\s*(?:add\s+(?:a\s+)?todo|todo|(?:添加|加|新增)?\s*待办)\s*[:：]\s*(.+)$/i);
  if (!m) return null;
  return m[1].trim();
}

// Light polling keeps elapsed times and cross-tab runs fresh; skipped while hidden.
setInterval(loadTasks, 3000);
setInterval(loadPipelines, 30000);
setTasksOpen(localStorage.getItem('forge-tasks-open') === '1' ||
  (localStorage.getItem('forge-tasks-open') == null && window.innerWidth > 1100));
