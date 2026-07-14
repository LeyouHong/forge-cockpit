/* ---------- message elements ---------- */
function clearMessages() { messagesEl.innerHTML = ''; }
function addUser(text, images) {
  const row = document.createElement('div'); row.className = 'row';
  const b = document.createElement('div'); b.className = 'msg user';
  if (text) b.textContent = text;
  for (const im of (images || [])) {
    const img = document.createElement('img'); img.className = 'msg-img';
    img.src = 'data:' + im.mime + ';base64,' + im.base64; b.appendChild(img);
  }
  row.appendChild(b); messagesEl.appendChild(row); scrollDown();
}
function addAssistantBubble(markdown) {
  const row = document.createElement('div'); row.className = 'row';
  const b = document.createElement('div'); b.className = 'msg assistant';
  b.innerHTML = renderMarkdown(markdown || '');
  row.appendChild(b); messagesEl.appendChild(row);
}
function toolChip(label, name, output) {
  const wrap = document.createElement('div');
  const c = document.createElement('span'); c.className = 'tool';
  const k = document.createElement('span'); k.className = 'k'; k.textContent = label;
  c.append(k, document.createTextNode(name));
  wrap.appendChild(c);
  if (output != null && output !== '') {
    const pre = document.createElement('div'); pre.className = 'tool-out';
    pre.textContent = output; pre.style.display = 'none';
    c.onclick = () => { pre.style.display = pre.style.display === 'none' ? 'block' : 'none'; };
    c.title = 'Toggle output';
    wrap.appendChild(pre);
  }
  return wrap;
}
// Builds the streaming elements (reasoning / tools / body) inside `parent`
// and returns a context reused by handleEvent. `scrollEl` is what auto-scrolls.
function makeStreamCtx(parent, scrollEl) {
  // Reasoning is collapsed by default (it's long and rarely wanted); click to expand.
  const reasoning = document.createElement('details'); reasoning.className = 'reasoning-wrap'; reasoning.open = false; reasoning.style.display = 'none';
  const rsum = document.createElement('summary'); rsum.textContent = 'Reasoning';
  const reasoningBody = document.createElement('div'); reasoningBody.className = 'reasoning';
  reasoning.append(rsum, reasoningBody);
  // Tool calls are collapsed into a quiet "N tool calls" toggle instead of a long chip list.
  const toolsWrap = document.createElement('details'); toolsWrap.className = 'tools-wrap'; toolsWrap.style.display = 'none';
  const tsum = document.createElement('summary'); tsum.textContent = 'Tools';
  const tools = document.createElement('div'); tools.className = 'tools-body';
  toolsWrap.append(tsum, tools);
  const body = document.createElement('div'); body.className = 'msg assistant'; body.style.display = 'none';
  parent.append(reasoning, toolsWrap, body);
  const ctx = { reasoning, reasoningBody, toolsWrap, tools, toolCount: 0, body, raw: '' };
  ctx.addTool = (chip) => {
    ctx.tools.appendChild(chip);
    ctx.toolCount++;
    tsum.textContent = ctx.toolCount + ' tool call' + (ctx.toolCount > 1 ? 's' : '');
    toolsWrap.style.display = '';
  };
  ctx.scroll = () => { if (scrollEl) scrollEl.scrollTop = scrollEl.scrollHeight; };
  return ctx;
}
function addStreamingAssistant() {
  const row = document.createElement('div'); row.className = 'row';
  messagesEl.appendChild(row);
  return makeStreamCtx(row, messagesEl);
}

/* ---------- conversations ---------- */
function makeConvoRow(id, label, active) {
  const row = document.createElement('div');
  row.className = 'convo' + (active ? ' active' : '');
  const dot = document.createElement('span'); dot.className = 'dot';
  const name = document.createElement('span'); name.className = 'name';
  name.textContent = label; name.title = label + '  (double-click to rename)';
  name.ondblclick = (e) => { e.stopPropagation(); renameConversation(id, label); };
  const del = document.createElement('button'); del.className = 'del'; del.textContent = '×'; del.title = 'Delete';
  del.onclick = (e) => { e.stopPropagation(); deleteConversation(id, label); };
  row.append(dot, name, del);
  row.onclick = () => { selectConversation(id, label); document.body.classList.remove('sidebar-open'); };
  return row;
}

async function renameConversation(id, current) {
  const title = await confirmModal({ title: 'Rename conversation', body: 'New title:', confirmText: 'Rename', danger: false, input: { value: current === 'New chat' ? '' : current, placeholder: 'Conversation title' } });
  if (title == null || !title.trim()) return;
  const res = await api('/api/conversations/' + id + '/rename', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ title: title.trim() }) });
  if (res.ok && id === conversationId) $('conv-title').textContent = title.trim();
  loadConversations();
}

let convoCache = [];
async function loadConversations() {
  const res = await api('/api/conversations');
  convoCache = await res.json();
  renderConversations();
}
function renderConversations() {
  const q = ($('convo-search').value || '').trim().toLowerCase();
  convosEl.innerHTML = '';
  const ids = new Set(convoCache.map((c) => c.id));
  if (conversationId && !ids.has(conversationId) && !q) {
    convosEl.appendChild(makeConvoRow(conversationId, 'New chat', true));
  }
  let shown = 0;
  for (const c of convoCache) {
    const label = c.title || 'New chat';
    if (q && !label.toLowerCase().includes(q)) continue;
    convosEl.appendChild(makeConvoRow(c.id, label, c.id === conversationId));
    shown++;
  }
  if (q && !shown) convosEl.innerHTML = '<div class="conn-empty" style="padding:8px 12px">No matches.</div>';
}
$('convo-search').addEventListener('input', renderConversations);

async function deleteConversation(id, label) {
  const ok = await confirmModal({
    title: 'Delete conversation',
    body: 'Delete “' + label + '”? This can’t be undone.',
    confirmText: 'Delete',
  });
  if (!ok) return;
  const res = await api('/api/conversations/' + id, { method: 'DELETE' });
  if (!res.ok) { await confirmModal({ title: 'Delete failed', body: 'Could not delete this conversation.', confirmText: 'OK', hideCancel: true }); return; }
  if (id === conversationId) {
    conversationId = null;
    $('conv-title').textContent = 'forge-cockpit';
    messagesEl.innerHTML = '<div class="empty"><div class="big">⚒</div>Conversation deleted.</div>';
  }
  await loadConversations();
}

async function newConversation() {
  const res = await api('/api/conversations', { method: 'POST' });
  const c = await res.json();
  conversationId = c.id;
  $('conv-title').textContent = 'New chat';
  messagesEl.innerHTML = '<div class="empty"><div class="big">⚒</div>Say something to get started.</div>';
  await loadConversations();
  inputEl.focus();
}

async function selectConversation(id, label) {
  conversationId = id;
  messagesEl.innerHTML = '<div class="empty">Loading…</div>';
  const res = await api('/api/conversations/' + id + '/messages');
  if (!res.ok) { messagesEl.innerHTML = '<div class="empty err">Failed to load.</div>'; return; }
  const msgs = await res.json();
  $('conv-title').textContent = label || 'Conversation';
  clearMessages();
  renderHistory(msgs);
  refreshUsage(id);
  await loadConversations();
  // Resume an in-flight turn, if one is still running server-side.
  attachLive(id, {});
}

function renderHistory(msgs) {
  let rendered = 0;
  // Consecutive tool chips fold into one collapsed "N tool calls" group, so a
  // reloaded conversation reads as clean as a freshly streamed one.
  let group = null;
  const flushGroup = () => { group = null; };
  const addHistoryTool = (chip) => {
    if (!group) {
      const wrap = document.createElement('details'); wrap.className = 'tools-wrap';
      const s = document.createElement('summary'); s.textContent = 'Tools';
      const b = document.createElement('div'); b.className = 'tools-body';
      wrap.append(s, b);
      const row = document.createElement('div'); row.className = 'row'; row.appendChild(wrap);
      messagesEl.appendChild(row);
      group = { body: b, summary: s, count: 0 };
    }
    group.body.appendChild(chip);
    group.count++;
    group.summary.textContent = group.count + ' tool call' + (group.count > 1 ? 's' : '');
  };
  for (const m of msgs) {
    if (m.kind === 'text') {
      if (m.role === 'system') continue;
      if (m.role === 'user') { flushGroup(); addUser(m.content); }
      else {
        if (m.content && m.content.trim()) { flushGroup(); addAssistantBubble(m.content); }
        for (const tc of (m.tool_calls || [])) addHistoryTool(toolChip('⚙ ', tc, null));
      }
      rendered++;
    } else if (m.kind === 'tool') {
      addHistoryTool(toolChip('▸ ', m.name, m.output));
      rendered++;
    } else if (m.kind === 'image') {
      flushGroup();
      const row = document.createElement('div'); row.className = 'row';
      row.appendChild(toolChip('🖼 ', 'image', null)); messagesEl.appendChild(row);
      rendered++;
    }
  }
  if (rendered === 0) messagesEl.innerHTML = '<div class="empty">No messages yet — continue below.</div>';
  scrollDown();
}

/* ---------- agent / model pickers ---------- */
// The agent picker is intentionally hidden: this UI always uses the
// full-capability "forge" agent (code + shell + MCP/GitHub tools) so users
// don't have to understand the agent modes. Falls back to the first agent
// if "forge" isn't available.
async function ensureDefaultAgent() {
  const res = await api('/api/agents'); const list = await res.json();
  const pick = list.find((a) => a.id === 'forge') || list[0];
  if (pick && !pick.active) {
    await api('/api/agents', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ id: pick.id }) });
  }
}
const modelSupportsImage = {};
async function loadModels() {
  const res = await api('/api/models'); const list = await res.json();
  const sel = $('model'); sel.innerHTML = '';
  for (const m of list) {
    modelSupportsImage[m.id] = !!m.supports_image;
    const o = document.createElement('option');
    o.value = m.id; o.textContent = m.name || m.id; o.selected = m.active; sel.appendChild(o);
  }
}
$('model').onchange = async (e) => {
  await api('/api/models', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ id: e.target.value }) });
};

/* ---------- chat streaming (resumable turns) ---------- */
// Turns run server-side and survive page refreshes: POST /api/chat starts a
// background turn; GET /api/chat/{conv}/live replays + streams its events.
async function send() {
  const text = inputEl.value.trim();
  const images = pendingImages.slice();
  if ((!text && !images.length) || streaming) return;
  // "add a todo: …" goes straight to the TODO list — no agent turn needed.
  const todo = images.length ? null : tryTodoIntercept(text);
  if (todo) {
    inputEl.value = ''; autoGrow();
    const first = messagesEl.querySelector('.empty'); if (first) first.remove();
    addUser(text);
    const ok = await addTodo(todo);
    addAssistantBubble(ok ? '✓ Added to your TODO list: **' + todo.replace(/\*/g, '') + '**' : 'Could not save that TODO.');
    scrollDown();
    setTasksOpen(true);
    return;
  }
  ensureNotifyPermission();
  inputEl.value = ''; autoGrow(); clearAttachments();
  await runTurn(text, images);
}

async function runTurn(text, images) {
  if (!conversationId) await newConversation();
  const first = messagesEl.querySelector('.empty');
  if (first) first.remove();
  addUser(text, images);
  const res = await api('/api/chat', {
    method: 'POST', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ conversation_id: conversationId, message: text, images }),
  });
  if (!res.ok) {
    const out = addStreamingAssistant();
    out.body.style.display = ''; out.body.classList.add('err');
    const e = await res.json().catch(() => ({}));
    out.body.textContent = 'Error: ' + (e.error || ('HTTP ' + res.status));
    const retry = document.createElement('button');
    retry.className = 'btn btn-sm btn-ghost retry-btn'; retry.textContent = 'Retry';
    retry.onclick = () => { if (!streaming) runTurn(text, images); };
    out.body.appendChild(retry);
    return;
  }
  loadTasks();
  await attachLive(conversationId, { skipUser: true });
}

// Attaches to a conversation's live turn (if any). Renders replayed +
// live events; used both right after send() and to resume after a refresh.
async function attachLive(convId, opts) {
  const res = await api('/api/chat/' + convId + '/live');
  if (!res.ok) return false; // no active turn — nothing to resume
  streaming = true; setStreamingUI(true);
  let out = null; // created lazily so the user bubble lands first
  const seen = new Set();
  let skipUser = !!(opts && opts.skipUser);
  let sawComplete = false;
  try {
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      const frames = buffer.split('\n\n');
      buffer = frames.pop();
      for (const frame of frames) {
        const line = frame.split('\n').find((l) => l.startsWith('data:'));
        if (!line) continue;
        let ev; try { ev = JSON.parse(line.slice(5).trim()); } catch { continue; }
        if (ev.seq != null) { if (seen.has(ev.seq)) continue; seen.add(ev.seq); }
        if (conversationId !== convId) continue; // user switched away; keep draining
        if (ev.type === 'user') {
          if (skipUser) { skipUser = false; continue; }
          addUser(ev.text || ''); continue;
        }
        if (ev.type === 'usage') { renderUsage(ev); continue; }
        if (ev.type === 'complete') sawComplete = true;
        if (!out) out = addStreamingAssistant();
        handleEvent(ev, out);
      }
    }
  } catch (e) {
    if (out) { out.body.style.display = ''; out.body.classList.add('err'); out.body.textContent = 'Error: ' + e.message; }
  } finally {
    if (conversationId === convId) { streaming = false; setStreamingUI(false); }
    if (sawComplete) notifyDone('Reply ready');
    loadConversations();
    loadTasks();
    // The agent may have checked off a TODO by editing the settings file
    // during the turn — refresh the list so the panel reflects it.
    loadTodos();
  }
  return true;
}

function setStreamingUI(on) {
  const btn = $('send');
  btn.textContent = on ? '■' : '↑';
  btn.title = on ? 'Stop' : 'Send (Enter)';
  btn.classList.toggle('stop', on);
}
// Stop is an explicit endpoint now (disconnecting no longer kills the turn).
function stopStreaming() {
  if (conversationId) api('/api/chat/' + conversationId + '/stop', { method: 'POST' });
}

/* ---------- completion notifications ---------- */
let unseenDone = 0;
function updateTitleFlash() { document.title = unseenDone > 0 ? '(' + unseenDone + ') forge-cockpit' : 'forge-cockpit'; }
document.addEventListener('visibilitychange', () => { if (!document.hidden) { unseenDone = 0; updateTitleFlash(); } });
function ensureNotifyPermission() { if (window.Notification && Notification.permission === 'default') { try { Notification.requestPermission(); } catch (e) {} } }
// Only nudges when the tab is in the background (foreground already shows it).
function notifyDone(body) {
  if (!document.hidden) return;
  unseenDone++; updateTitleFlash();
  try { if (window.Notification && Notification.permission === 'granted') new Notification('forge-cockpit', { body, tag: 'forge-done' }); } catch (e) {}
}

/* ---------- usage display ---------- */
function fmtTokens(n) { return n >= 1000 ? (n / 1000).toFixed(1) + 'k' : String(n); }
function renderUsage(u) {
  const el = $('usage-info');
  if (!u || !u.total_tokens) { el.textContent = ''; return; }
  let s = fmtTokens(u.total_tokens) + ' tok';
  if (u.cost != null) s += ' · $' + Number(u.cost).toFixed(4);
  el.textContent = s;
  el.title = 'prompt ' + (u.prompt_tokens || 0) + ' · completion ' + (u.completion_tokens || 0);
}
async function refreshUsage(convId) {
  try { renderUsage(await (await api('/api/conversations/' + convId + '/usage')).json()); }
  catch (e) { renderUsage(null); }
}

function handleEvent(ev, out) {
  switch (ev.type) {
    case 'text':
      // Coalesce re-renders to one per frame (avoids O(n²) on long replies).
      out.body.style.display = ''; out.raw += ev.text;
      if (!out._pending) { out._pending = true; requestAnimationFrame(() => { out._pending = false; out.body.innerHTML = renderMarkdown(out.raw); (out.scroll || scrollDown)(); }); }
      break;
    case 'reasoning':
      out.reasoning.style.display = ''; out.reasoningBody.textContent += ev.text; break;
    case 'tool_input':
      out.addTool(toolChip('▸ ', ev.title, ev.subtitle)); break;
    case 'tool_call_start':
      // args expandable; skip the placeholder empty-object payload
      out.addTool(toolChip('⚙ ', ev.name, ev.arguments && ev.arguments !== '{}' ? ev.arguments : null)); break;
    case 'tool_call_end':
      out.addTool(toolChip('✓ ', ev.name, ev.output || null)); break;
    case 'tool_output':
      break; // superseded by tool_call_end
    case 'retry':
      out.addTool(toolChip('↻ ', ev.cause, null)); break;
    case 'interrupt':
      out.body.style.display = ''; out.body.classList.add('err');
      out.body.innerHTML += '<p class="err">[interrupted: ' + escapeHtml(ev.reason) + ']</p>'; break;
    case 'error':
      out.body.style.display = ''; out.body.classList.add('err');
      out.body.innerHTML += '<p class="err">[error: ' + escapeHtml(ev.message) + ']</p>'; break;
    case 'complete':
      // Ensure the final markdown is flushed even if a frame was pending.
      if (out.raw) out.body.innerHTML = renderMarkdown(out.raw);
      break;
  }
  (out.scroll || scrollDown)();
}

/* ---------- image attachments ---------- */
function renderAttachPreview() {
  const box = $('attach-preview'); box.innerHTML = '';
  pendingImages.forEach((im, i) => {
    const t = document.createElement('div'); t.className = 'thumb';
    const img = document.createElement('img'); img.src = 'data:' + im.mime + ';base64,' + im.base64;
    const rm = document.createElement('button'); rm.className = 'rm'; rm.textContent = '×';
    rm.onclick = () => { pendingImages.splice(i, 1); renderAttachPreview(); };
    t.append(img, rm); box.appendChild(t);
  });
  // Warn when the active model can't see images.
  const model = $('model').value;
  if (pendingImages.length && model && modelSupportsImage[model] === false) {
    const w = document.createElement('div'); w.className = 'gh-fail';
    w.style.alignSelf = 'center';
    w.textContent = '⚠ ' + model + ' 不支持图片输入 — 模型将看不到这张图。请切换支持视觉的模型。';
    box.appendChild(w);
  }
}
function clearAttachments() { pendingImages = []; renderAttachPreview(); }
function addImageFile(file) {
  if (!file || !file.type.startsWith('image/')) return;
  const reader = new FileReader();
  reader.onload = () => { pendingImages.push({ base64: reader.result.split(',')[1], mime: file.type, name: file.name || 'image' }); renderAttachPreview(); };
  reader.readAsDataURL(file);
}
$('attach').onclick = () => $('file-input').click();
$('file-input').onchange = (e) => { for (const f of e.target.files) addImageFile(f); e.target.value = ''; };
inputEl.addEventListener('paste', (e) => {
  for (const item of (e.clipboardData || {}).items || []) {
    if (item.type.startsWith('image/')) { addImageFile(item.getAsFile()); e.preventDefault(); }
  }
});

/* ---------- responsive sidebar ---------- */
$('hamburger').onclick = () => document.body.classList.toggle('sidebar-open');

/* ---------- export conversation ---------- */
$('export-conv').onclick = async () => {
  if (!conversationId) { await confirmModal({ title: 'No conversation', body: 'Open or start a conversation first.', confirmText: 'OK', hideCancel: true, danger: false }); return; }
  const res = await api('/api/conversations/' + conversationId + '/export');
  if (!res.ok) { await confirmModal({ title: 'Export failed', body: 'Could not export.', confirmText: 'OK', hideCancel: true, danger: false }); return; }
  const html = await res.text();
  const url = URL.createObjectURL(new Blob([html], { type: 'text/html' }));
  const a = document.createElement('a'); a.href = url; a.download = 'forge-' + conversationId.slice(0, 8) + '.html';
  a.click(); URL.revokeObjectURL(url);
};

/* ---------- slash command / skill palette ---------- */
let allCommands = [], allSkills = [], paletteItems = [], paletteSel = 0;
async function loadCommandsSkills() {
  try { allCommands = await (await api('/api/commands')).json(); } catch (e) { allCommands = []; }
  try { allSkills = await (await api('/api/skills')).json(); } catch (e) { allSkills = []; }
}
function updatePalette() {
  const pal = $('cmd-palette');
  const m = inputEl.value.match(/^\/(\S*)$/);
  if (!m) { pal.classList.remove('open'); return; }
  const q = m[1].toLowerCase();
  const cmds = allCommands.filter((c) => c.name.toLowerCase().includes(q)).slice(0, 8);
  const sks = allSkills.filter((s) => s.name.toLowerCase().includes(q)).slice(0, 6);
  paletteItems = []; paletteSel = 0; pal.innerHTML = '';
  const section = (title, arr, kind) => {
    if (!arr.length) return;
    const h = document.createElement('div'); h.className = 'cmd-sec'; h.textContent = title; pal.appendChild(h);
    for (const x of arr) {
      const idx = paletteItems.length; paletteItems.push({ kind, data: x });
      const it = document.createElement('div'); it.className = 'cmd-item'; it.dataset.i = idx;
      const k = document.createElement('span'); k.className = 'k'; k.textContent = (kind === 'cmd' ? '/' : '📚 ') + x.name;
      const d = document.createElement('span'); d.className = 'd'; d.textContent = x.description || '';
      it.append(k, d);
      it.onmousedown = (e) => { e.preventDefault(); palettePick(idx); };
      it.onmouseenter = () => { paletteSel = idx; paintPalette(); };
      pal.appendChild(it);
    }
  };
  section('Commands', cmds, 'cmd');
  section('Skills', sks, 'skill');
  if (!paletteItems.length) { pal.classList.remove('open'); return; }
  pal.classList.add('open'); paintPalette();
}
function paintPalette() {
  $('cmd-palette').querySelectorAll('.cmd-item').forEach((el) => el.classList.toggle('sel', +el.dataset.i === paletteSel));
}
function paletteMove(d) {
  if (!paletteItems.length) return;
  paletteSel = (paletteSel + d + paletteItems.length) % paletteItems.length; paintPalette();
  const el = $('cmd-palette').querySelector('.cmd-item.sel'); if (el) el.scrollIntoView({ block: 'nearest' });
}
function palettePick(idx) {
  const item = paletteItems[idx != null ? idx : paletteSel]; if (!item) return;
  if (item.kind === 'cmd') inputEl.value = item.data.prompt || ('/' + item.data.name + ' ');
  else inputEl.value = 'Use the ' + item.data.name + ' skill to ';
  $('cmd-palette').classList.remove('open'); inputEl.focus(); autoGrow();
}

/* ---------- composer ---------- */
function autoGrow() { inputEl.style.height = 'auto'; inputEl.style.height = Math.min(inputEl.scrollHeight, 200) + 'px'; }
inputEl.addEventListener('input', () => { autoGrow(); updatePalette(); });
$('send').onclick = () => (streaming ? stopStreaming() : send());
$('new').onclick = newConversation;
inputEl.addEventListener('keydown', (e) => {
  // Slash palette navigation takes priority when it's open.
  if ($('cmd-palette').classList.contains('open')) {
    if (e.key === 'ArrowDown') { e.preventDefault(); paletteMove(1); return; }
    if (e.key === 'ArrowUp') { e.preventDefault(); paletteMove(-1); return; }
    if (e.key === 'Escape') { $('cmd-palette').classList.remove('open'); return; }
    if ((e.key === 'Enter' || e.key === 'Tab') && !e.isComposing) { e.preventDefault(); palettePick(); return; }
  }
  // Ignore Enter while an IME is composing (e.g. selecting a pinyin
  // candidate) — `isComposing`/keyCode 229 mark an in-progress composition.
  if (e.key === 'Enter' && !e.shiftKey && !e.isComposing && e.keyCode !== 229) {
    e.preventDefault();
    send();
  }
});

// Delegated copy handler: assistant bubbles rebuild their innerHTML while
// streaming, so a per-button listener wouldn't survive — listen on the
// persistent container instead.
messagesEl.addEventListener('click', async (e) => {
  const btn = e.target.closest('.copy-btn');
  if (!btn) return;
  const code = btn.parentElement.querySelector('pre code');
  if (!code) return;
  const text = code.textContent;
  try {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      await navigator.clipboard.writeText(text);
    } else {
      const ta = document.createElement('textarea');
      ta.value = text; ta.style.position = 'fixed'; ta.style.opacity = '0';
      document.body.appendChild(ta); ta.select(); document.execCommand('copy'); ta.remove();
    }
    btn.textContent = 'Copied'; btn.classList.add('copied');
  } catch (err) {
    btn.textContent = 'Failed';
  }
  setTimeout(() => { btn.textContent = 'Copy'; btn.classList.remove('copied'); }, 1400);
});
