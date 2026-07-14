/* ---------- pipeline page ---------- */
const PL = { project: null, file: null };
const PL_COLOR = { done: '#2e9e44', running: '#3b82f6', failed: '#e5484d', skipped: '#9ca3af', pending: '#c7c7c7' };
function openPipeline() { $('pipeline-overlay').classList.add('open'); loadFiles(); loadRuns(); }
$('pipeline-open').onclick = openPipeline;
$('pipeline-close').onclick = () => { stopRunPoll(); $('pipeline-overlay').classList.remove('open'); };
$('pl-refresh').onclick = () => { loadFiles(); loadRuns(); };
function plSetEditor(content, label) {
  $('pl-editing').textContent = label; $('pl-editor').value = content; closeNodeModal();
  if (label === 'no pipeline open') { PL.file = null; PL.g = null; PL.sel = null; }
  [...$('pl-viewport').querySelectorAll('.pl-node')].forEach((n) => n.remove()); $('pl-edges').innerHTML = ''; $('pl-foreach').innerHTML = '';
  $('pl-status').textContent = ''; $('pl-status').className = 'pl-status';
}
function plStatus(msg, err) { const s = $('pl-status'); s.textContent = msg; s.className = 'pl-status ' + (err ? 'err' : 'ok'); }
function plShowValid(d) { if (d.valid) plStatus('✓ valid workflow', false); else plStatus('✗ ' + (d.error || 'invalid'), true); }
const plBody = (o) => ({ method: o.m || 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(o.b) });

// Add a Team project via a path (returns the created project name, or false).
async function addProjectPath(path) {
  if (!path || !path.trim()) return false;
  const r = await api('/api/pipeline/projects', plBody({ b: { path: path.trim() } })); const d = await r.json();
  return r.ok ? d.name : false;
}

/* server-side folder picker (server is local → this is your filesystem).
   PICK.onUse(path) is invoked with the chosen folder. */
const PICK = { path: null, parent: null, onUse: null };
$('pl-picker-close').onclick = () => $('pl-picker').classList.remove('open');
$('pl-picker-up').onclick = () => { if (PICK.parent) browseTo(PICK.parent); };
$('pl-picker-path').onkeydown = (e) => { if (e.key === 'Enter') { e.preventDefault(); browseTo($('pl-picker-path').value.trim()); } };
$('pl-picker-use').onclick = () => { if (PICK.path && PICK.onUse) PICK.onUse(PICK.path); $('pl-picker').classList.remove('open'); };
function openPicker(start, onUse) { PICK.onUse = onUse || null; $('pl-picker').classList.add('open'); browseTo(start); }
async function browseTo(path) {
  const url = '/api/pipeline/browse' + (path ? '?path=' + encodeURIComponent(path) : '');
  const d = await (await api(url)).json().catch(() => null);
  if (!d) return;
  PICK.path = d.path; PICK.parent = d.parent;
  $('pl-picker-path').value = d.path; $('pl-picker-cur').textContent = d.path; $('pl-picker-cur').title = d.path;
  const list = $('pl-picker-list'); list.innerHTML = '';
  if (!(d.dirs || []).length) { list.innerHTML = '<div class="sl" style="padding:8px">no sub-folders here</div>'; }
  for (const dir of (d.dirs || [])) {
    const row = document.createElement('div'); row.className = 'pl-picker-row';
    const ic = document.createElement('span'); ic.className = 'ic'; ic.textContent = '📁';
    const nm = document.createElement('span'); nm.textContent = dir.name;
    row.appendChild(ic); row.appendChild(nm);
    if (dir.git) { const g = document.createElement('span'); g.className = 'git'; g.textContent = 'git'; row.appendChild(g); }
    row.onclick = () => browseTo(d.path.replace(/\/+$/, '') + '/' + dir.name);
    list.appendChild(row);
  }
}
async function loadFiles() {
  const box = $('pl-files'); box.innerHTML = '<span class="sl">…</span>';
  const d = await (await api('/api/pipeline/files')).json().catch(() => ({ files: [] }));
  box.innerHTML = '';
  if (!(d.files || []).length) { box.innerHTML = '<span class="sl">no pipelines yet — + New</span>'; return; }
  for (const f of d.files) {
    const row = document.createElement('div'); row.className = 'pl-file' + (f === PL.file ? ' active' : '');
    const nm = document.createElement('span'); nm.className = 'nm'; nm.textContent = f; nm.onclick = () => openFile(f);
    const rm = document.createElement('span'); rm.className = 'rm'; rm.textContent = '✕'; rm.title = 'delete';
    rm.onclick = async (e) => { e.stopPropagation(); if (!(await uiConfirm('Delete ' + f + '?', { confirmText: 'Delete', danger: true }))) return; await api('/api/pipeline/file/delete', plBody({ b: { name: f } })); if (PL.file === f) { PL.file = null; plSetEditor('', 'no pipeline open'); } loadFiles(); };
    row.appendChild(nm); row.appendChild(rm); box.appendChild(row);
  }
}
/* ---- graph model + visual node editor ----
   PL.g = { name, vars, input,
            nodes: { id: {mode, prompt, project, retries, depends_on:[], outputs:[{name,extract}]} },
            layout: { id: {x,y} } } */
PL.tab = PL.tab || 'visual';
async function openFile(name) {
  PL.file = name; PL.sel = null; closeNodeModal(); stopRunPoll(); $('pl-editing').textContent = name;
  const d = await (await api('/api/pipeline/graph?name=' + encodeURIComponent(name))).json();
  PL.g = graphFromWorkflow(d.workflow || {}, d.layout || {});
  const f = await (await api('/api/pipeline/file?name=' + encodeURIComponent(name))).json().catch(() => ({}));
  $('pl-editor').value = f.content || '';
  PL.runOutputs = {};
  plShowValid(d); setTab(PL.tab); renderCanvas(); renderForEach(); loadFiles(); loadRuns();
}
const PL_NODE_KEYS = ['mode', 'prompt', 'project', 'retries', 'depends_on', 'outputs'];
const PL_WF_KEYS = ['name', 'vars', 'input', 'nodes'];
function graphFromWorkflow(wf, layout) {
  const nodes = {}; const raw = wf.nodes || {};
  for (const id of Object.keys(raw)) {
    const n = raw[id] || {};
    // Preserve node fields the visual editor doesn't model (verify, branch, …).
    const extra = {}; for (const k of Object.keys(n)) if (!PL_NODE_KEYS.includes(k)) extra[k] = n[k];
    nodes[id] = {
      mode: n.mode || 'claude', prompt: n.prompt || '', project: n.project || '', retries: n.retries || 0,
      depends_on: Array.isArray(n.depends_on) ? n.depends_on.slice() : [],
      outputs: Array.isArray(n.outputs) ? n.outputs.map((o) => ({ name: o.name || 'out', extract: o.extract || 'result' })) : [],
      _extra: extra,
    };
  }
  // Preserve top-level keys we don't model (description, git_push…); pull
  // for_each out into an editable model.
  const wfExtra = {}; for (const k of Object.keys(wf)) if (!PL_WF_KEYS.includes(k)) wfExtra[k] = wf[k];
  let forEach = null; const fe = wfExtra.for_each;
  if (fe) {
    forEach = {
      source: Array.isArray(fe.source) ? fe.source.join(', ') : (fe.source || ''),
      as: fe.as || 'item', on_failure: fe.on_failure || 'continue',
      before: Array.isArray(fe.before) ? fe.before.slice() : (Array.isArray(fe.setup) ? fe.setup.slice() : []),
    };
    delete wfExtra.for_each;
  }
  const g = { name: wf.name || (PL.file || '').replace(/\.(ya?ml)$/, ''), vars: wf.vars || {}, input: wf.input || {}, nodes, layout: Object.assign({}, layout), extra: wfExtra, forEach };
  ensureLayout(g); return g;
}
function ensureLayout(g) {
  const ids = Object.keys(g.nodes); const depth = {}; for (const id of ids) depth[id] = 0;
  let changed = true; for (let k = 0; k < ids.length && changed; k++) { changed = false; for (const id of ids) for (const d of g.nodes[id].depends_on) if (g.nodes[d] && depth[d] + 1 > depth[id]) { depth[id] = depth[d] + 1; changed = true; } }
  // Top-to-bottom: y grows with dependency depth; siblings at the same
  // depth spread horizontally. Matches the reference's vertical flow.
  const perRow = {};
  for (const id of ids) { if (g.layout[id] && typeof g.layout[id].x === 'number') continue; const r = depth[id]; perRow[r] = perRow[r] || 0; g.layout[id] = { x: 60 + perRow[r] * 240, y: 24 + r * 150 }; perRow[r]++; }
}
function newNodeId(g) { let i = 1; while (g.nodes['node' + i]) i++; return 'node' + i; }
function buildWorkflow() {
  const g = PL.g; const nodes = {};
  for (const id of Object.keys(g.nodes)) {
    const n = g.nodes[id]; const o = Object.assign({}, n._extra || {}); o.prompt = n.prompt;
    if (n.mode && n.mode !== 'claude') o.mode = n.mode;
    if (n.project) o.project = n.project;
    if (n.retries) o.retries = n.retries;
    if (n.depends_on && n.depends_on.length) o.depends_on = n.depends_on;
    if (n.outputs && n.outputs.length) o.outputs = n.outputs;
    nodes[id] = o;
  }
  // Start from preserved top-level extras (description…), then set ours.
  const wf = Object.assign({}, g.extra || {}, { name: g.name || 'workflow', input: g.input || {}, nodes });
  if (g.vars && Object.keys(g.vars).length) wf.vars = g.vars;
  if (g.forEach) {
    const fe = { source: g.forEach.source, as: g.forEach.as || 'item' };
    if (g.forEach.on_failure && g.forEach.on_failure !== 'continue') fe.on_failure = g.forEach.on_failure;
    if (g.forEach.before && g.forEach.before.length) fe.before = g.forEach.before;
    wf.for_each = fe;
  }
  return wf;
}

$('pl-new').onclick = async () => {
  let name = await uiPrompt('New pipeline file name:', { value: 'my-flow.yaml' }); if (!name) return;
  if (!/\.(ya?ml)$/.test(name)) name += '.yaml';
  const base = name.replace(/\.(ya?ml)$/, '');
  const r = await api('/api/pipeline/file', plBody({ m: 'PUT', b: { name, content: 'name: ' + base + '\ninput: {}\nnodes: {}\n' } }));
  if (!r.ok) { const d = await r.json(); plStatus(d.error || 'create failed', true); return; }
  await openFile(name); loadFiles();
};

/* tabs: Visual ↔ YAML (switching saves the current side, then loads the other from the file) */
function setTab(t) {
  PL.tab = t;
  $('pl-tab-visual').classList.toggle('active', t === 'visual');
  $('pl-tab-yaml').classList.toggle('active', t === 'yaml');
  $('pl-visual').style.display = t === 'visual' ? '' : 'none';
  $('pl-editor').style.display = t === 'yaml' ? '' : 'none';
}
$('pl-tab-visual').onclick = async () => { if (PL.tab === 'visual' || !PL.file) { setTab('visual'); return; } await plSaveYaml(); await openFile(PL.file); setTab('visual'); };
$('pl-tab-yaml').onclick = async () => {
  if (PL.tab === 'yaml' || !PL.file) { setTab('yaml'); return; }
  await plSaveGraph();
  const f = await (await api('/api/pipeline/file?name=' + encodeURIComponent(PL.file))).json().catch(() => ({}));
  $('pl-editor').value = f.content || ''; setTab('yaml');
};

async function plSaveGraph() {
  if (!PL.file || !PL.g) return { ok: false };
  const r = await api('/api/pipeline/graph', plBody({ m: 'PUT', b: { name: PL.file, workflow: buildWorkflow(), layout: PL.g.layout } }));
  return { r, d: await r.json() };
}
async function plSaveYaml() {
  const r = await api('/api/pipeline/file', plBody({ m: 'PUT', b: { name: PL.file, content: $('pl-editor').value } }));
  return { r, d: await r.json() };
}
async function plSave() { return PL.tab === 'visual' ? plSaveGraph() : plSaveYaml(); }

$('pl-validate').onclick = async () => {
  if (!PL.file) return;
  const d = PL.tab === 'visual'
    ? await (await api('/api/pipeline/validate-graph', plBody({ b: { workflow: buildWorkflow() } }))).json()
    : await (await api('/api/pipeline/validate', plBody({ b: { content: $('pl-editor').value } }))).json();
  plShowValid(d);
};
$('pl-save').onclick = async () => {
  if (!PL.file) { plStatus('open or create a pipeline first', true); return; }
  const { r, d } = await plSave();
  if (!r || !r.ok) { plStatus((d && d.error) || 'save failed', true); return; }
  plStatus(d.valid ? '✓ saved · valid' : '⚠ saved · ✗ ' + (d.error || 'invalid'), !d.valid);
};
/* Run dialog: pipelines are global, so Run asks for a target directory
   (where the agent/shell nodes execute) + values for the workflow's
   declared input fields, then POSTs { name, dir, inputs }. */
const RUN = { dir: '' };
$('pl-run').onclick = async () => {
  if (!PL.file) { plStatus('open a pipeline first', true); return; }
  const { d: sv } = await plSave();
  if (sv && sv.valid === false) { plStatus('✗ not valid — fix before running:\n' + (sv.error || ''), true); return; }
  openRunDialog();
};
function openRunDialog() {
  const input = (PL.g && PL.g.input) || {};
  const keys = Object.keys(input);
  RUN.dir = RUN.dir || '~';
  let html = '<div class="pl-insp-row"><label>target directory <span class="sl">(where nodes run)</span></label>'
    + '<div style="display:flex;gap:6px;align-items:center">'
    + '<input id="pl-run-dir" style="flex:1" value="' + escapeHtml(RUN.dir) + '" placeholder="~/path/to/project">'
    + '<button class="btn btn-sm" id="pl-run-browse">Browse…</button></div></div>';
  if (keys.length) {
    for (const k of keys) {
      const def = input[k] == null ? '' : String(input[k]);
      html += '<div class="pl-insp-row"><label>' + escapeHtml(k) + '</label>'
        + '<input class="pl-run-in" data-k="' + escapeHtml(k) + '" value="' + escapeHtml(def) + '"></div>';
    }
  } else {
    html += '<div class="sl">This pipeline declares no <code>input:</code> fields.</div>';
  }
  html += '<div class="pl-insp-row"><label style="display:flex;gap:8px;align-items:center;text-transform:none;font-size:12px;letter-spacing:0;cursor:pointer">'
    + '<input type="checkbox" id="pl-run-mcp" style="width:auto;margin:0"' + (RUN.useMcp ? ' checked' : '') + '>'
    + 'use my connected MCP tools (Gmail / Slack / …)</label>'
    + '<div class="sl">off = isolated workspace tools only (default). On lets agent nodes reach your integrations — e.g. send an email.</div></div>';
  $('pl-run-body').innerHTML = html;
  $('pl-run-msg').textContent = '';
  $('pl-run-browse').onclick = () => openPicker(RUN.dir && RUN.dir !== '~' ? RUN.dir : '', (p) => { RUN.dir = p; $('pl-run-dir').value = p; });
  $('pl-rundlg').classList.add('open');
}
$('pl-run-close').onclick = () => $('pl-rundlg').classList.remove('open');
$('pl-run-go').onclick = async () => {
  const dir = $('pl-run-dir').value.trim();
  if (!dir) { $('pl-run-msg').textContent = 'pick a target directory'; return; }
  RUN.dir = dir;
  const inputs = {}; for (const el of document.querySelectorAll('.pl-run-in')) inputs[el.dataset.k] = el.value;
  RUN.useMcp = !!($('pl-run-mcp') && $('pl-run-mcp').checked);
  const r = await api('/api/pipeline/run', plBody({ b: { name: PL.file, dir, inputs, use_mcp: RUN.useMcp } }));
  const d = await r.json();
  if (!r.ok) { $('pl-run-msg').textContent = d.error || 'run failed to start'; return; }
  $('pl-rundlg').classList.remove('open');
  plStatus('▶ run started — nodes light up as they run', false);
  startRunPoll();
};
/* live run-status coloring on the canvas */
function clearRunStatus() {
  if (!PL.g) return;
  for (const id of Object.keys(PL.g.nodes)) { const el = document.getElementById('pln-' + id); if (el) el.classList.remove('run-pending', 'run-running', 'run-done', 'run-failed', 'run-skipped'); }
}
function paintRunStatus(map) {
  if (!PL.g) return;
  for (const id of Object.keys(PL.g.nodes)) {
    const el = document.getElementById('pln-' + id); if (!el) continue;
    el.classList.remove('run-pending', 'run-running', 'run-done', 'run-failed', 'run-skipped');
    if (map[id]) el.classList.add('run-' + map[id]);
  }
}
async function pollRunOnce() {
  if (!PL.g) return null;
  const d = await (await api('/api/pipeline/runs')).json().catch(() => null);
  if (!d) return null;
  const mine = (d.runs || []).filter((r) => r.workflow === PL.g.name).sort((a, b) => (a.created_at < b.created_at ? 1 : -1))[0];
  if (!mine) return null;
  const map = {}; for (const nd of (mine.nodes || [])) map[nd.id] = nd.status;
  paintRunStatus(map);
  return mine.status;
}
function stopRunPoll() { if (PL.runPoll) { clearInterval(PL.runPoll); PL.runPoll = null; } }
function startRunPoll() {
  stopRunPoll(); clearRunStatus();
  let n = 0;
  PL.runPoll = setInterval(async () => {
    loadRuns();
    const st = await pollRunOnce();
    if (st === 'done' || st === 'failed' || ++n > 90 || !$('pipeline-overlay').classList.contains('open')) stopRunPoll();
  }, 2000);
}

/* palette */
$('pl-add-shell').onclick = () => addNode('shell');
$('pl-add-agent').onclick = () => addNode('claude');
$('pl-auto').onclick = () => { if (!PL.g) return; PL.g.layout = {}; ensureLayout(PL.g); renderCanvas(); };
function addNode(mode) {
  if (!PL.g) { plStatus('open or create a pipeline first', true); return; }
  const id = newNodeId(PL.g);
  PL.g.nodes[id] = { mode, prompt: mode === 'shell' ? 'echo hello' : 'Describe the task…', project: '', retries: 0, depends_on: [], outputs: [] };
  const g = toGraph($('pl-canvas').getBoundingClientRect().left + 60, $('pl-canvas').getBoundingClientRect().top + 60);
  PL.g.layout[id] = { x: g.x, y: g.y };
  openNodeModal(id);
}
async function loadRuns() {
  const box = $('pl-runs');
  const d = await (await api('/api/pipeline/runs')).json().catch(() => ({ runs: [] }));
  // capture the newest run for the open workflow → per-node outputs (shown in the edit modal)
  if (PL.g) {
    const mine = (d.runs || []).filter((r) => r.workflow === PL.g.name).sort((a, b) => (a.created_at < b.created_at ? 1 : -1))[0];
    PL.runOutputs = {}; PL.runId = mine ? mine.id : null;
    if (mine) for (const n of (mine.nodes || [])) PL.runOutputs[n.id] = { outputs: n.outputs || {}, status: n.status, error: n.error };
    refreshNodeOutputs();
  }
  box.innerHTML = '';
  if (!(d.runs || []).length) { box.innerHTML = '<span class="sl">no runs yet</span>'; return; }
  for (const r of d.runs.slice(0, 8)) {
    const row = document.createElement('div'); row.className = 'pl-run';
    const rh = document.createElement('div'); rh.className = 'rh';
    const wf = document.createElement('span'); wf.textContent = r.workflow;
    const st = document.createElement('span'); st.className = 'sl'; st.textContent = r.status; st.style.color = PL_COLOR[r.status] || '#888';
    rh.appendChild(wf); rh.appendChild(st);
    const pills = document.createElement('div'); pills.className = 'pl-pills';
    for (const nd of (r.nodes || [])) { const p = document.createElement('span'); p.className = 'pl-pill'; p.textContent = nd.id; p.title = nd.status; p.style.background = PL_COLOR[nd.status] || '#888'; pills.appendChild(p); }
    row.appendChild(rh); row.appendChild(pills); box.appendChild(row);
  }
}

/* ---- canvas pan / zoom (infinite-canvas feel) ---- */
PL.view = PL.view || { x: 0, y: 0, z: 1 };
function applyView() {
  const v = PL.view;
  $('pl-viewport').style.transform = 'translate(' + v.x + 'px,' + v.y + 'px) scale(' + v.z + ')';
  $('pl-canvas').style.backgroundPosition = v.x + 'px ' + v.y + 'px';
  $('pl-canvas').style.backgroundSize = (18 * v.z) + 'px ' + (18 * v.z) + 'px';
}
function toGraph(cx, cy) { const r = $('pl-canvas').getBoundingClientRect(); return { x: (cx - r.left - PL.view.x) / PL.view.z, y: (cy - r.top - PL.view.y) / PL.view.z }; }
function zoomAt(mx, my, factor) {
  const old = PL.view.z, nz = Math.min(2, Math.max(0.3, old * factor));
  PL.view.x = mx - (mx - PL.view.x) * (nz / old); PL.view.y = my - (my - PL.view.y) * (nz / old); PL.view.z = nz; applyView();
}
(function initCanvasNav() {
  const c = $('pl-canvas'); if (!c || c.dataset.nav) return; c.dataset.nav = '1';
  c.addEventListener('mousedown', (e) => {
    if (e.target.closest('.pl-node') || e.target.closest('.pl-zoom')) return;
    PL.sel = null; renderInspector(); [...$('pl-viewport').querySelectorAll('.pl-node.sel')].forEach((n) => n.classList.remove('sel'));
    const pan = { sx: e.clientX, sy: e.clientY, ox: PL.view.x, oy: PL.view.y }; c.classList.add('panning');
    const move = (ev) => { PL.view.x = pan.ox + (ev.clientX - pan.sx); PL.view.y = pan.oy + (ev.clientY - pan.sy); applyView(); };
    const up = () => { c.classList.remove('panning'); document.removeEventListener('mousemove', move); document.removeEventListener('mouseup', up); };
    document.addEventListener('mousemove', move); document.addEventListener('mouseup', up);
  });
  c.addEventListener('wheel', (e) => { e.preventDefault(); const r = c.getBoundingClientRect(); zoomAt(e.clientX - r.left, e.clientY - r.top, e.deltaY < 0 ? 1.12 : 0.89); }, { passive: false });
  $('pl-zin').onclick = () => { const r = c.getBoundingClientRect(); zoomAt(r.width / 2, r.height / 2, 1.15); };
  $('pl-zout').onclick = () => { const r = c.getBoundingClientRect(); zoomAt(r.width / 2, r.height / 2, 0.87); };
  $('pl-zfit').onclick = () => { PL.view = { x: 0, y: 0, z: 1 }; applyView(); };
})();

/* ---- canvas rendering ---- */
function nodePorts(id) {
  const el = document.getElementById('pln-' + id); if (!el || !PL.g.layout[id]) return null;
  const x = PL.g.layout[id].x, y = PL.g.layout[id].y, w = el.offsetWidth || 210, h = el.offsetHeight || 70;
  return { inX: x + w / 2, inY: y, outX: x + w / 2, outY: y + h };
}
function renderCanvas() {
  const vp = $('pl-viewport'); if (!vp || !PL.g) return;
  [...vp.querySelectorAll('.pl-node')].forEach((n) => n.remove());
  for (const id of Object.keys(PL.g.nodes)) vp.appendChild(nodeEl(id));
  drawEdges(); applyView();
}
/* for_each (batch) config bar */
function renderForEach() {
  const box = $('pl-foreach'); if (!box) return; box.innerHTML = '';
  if (!PL.g) return;
  const toggle = document.createElement('label'); toggle.className = 'pl-fe-toggle';
  const cb = document.createElement('input'); cb.type = 'checkbox'; cb.checked = !!PL.g.forEach;
  cb.onchange = () => { PL.g.forEach = cb.checked ? { source: '', as: 'item', on_failure: 'continue', before: [] } : null; renderForEach(); renderCanvas(); };
  toggle.appendChild(cb); toggle.appendChild(document.createTextNode(' for_each — run the DAG once per item'));
  box.appendChild(toggle);
  const fe = PL.g.forEach; if (!fe) return;
  const field = (label, el) => { const w = document.createElement('span'); w.className = 'pl-fe-field'; const l = document.createElement('label'); l.textContent = label; w.appendChild(l); w.appendChild(el); box.appendChild(w); };
  const src = document.createElement('input'); src.value = fe.source; src.placeholder = '{{input.ids}} or a,b,c'; src.style.minWidth = '20ch'; src.oninput = () => (fe.source = src.value); field('source', src);
  const asIn = document.createElement('input'); asIn.value = fe.as; asIn.style.minWidth = '7ch'; asIn.oninput = () => { fe.as = asIn.value || 'item'; }; field('as', asIn);
  const onf = document.createElement('select'); for (const o of ['continue', 'stop']) { const op = document.createElement('option'); op.value = o; op.textContent = o; if (fe.on_failure === o) op.selected = true; onf.appendChild(op); } onf.onchange = () => (fe.on_failure = onf.value); field('on_failure', onf);
  const setup = document.createElement('input'); setup.value = (fe.before || []).join(', '); setup.placeholder = 'setup node ids (run once)'; setup.oninput = () => { fe.before = setup.value.split(',').map((s) => s.trim()).filter(Boolean); renderCanvas(); }; field('setup', setup);
}
function nodeEl(id) {
  const n = PL.g.nodes[id], pos = PL.g.layout[id];
  const el = document.createElement('div'); el.className = 'pl-node' + (PL.sel === id ? ' sel' : ''); el.id = 'pln-' + id;
  el.style.left = pos.x + 'px'; el.style.top = pos.y + 'px';
  const pin = document.createElement('div'); pin.className = 'pl-port in'; pin.dataset.id = id;
  const head = document.createElement('div'); head.className = 'nh';
  const stat = document.createElement('span'); stat.className = 'nstat';
  head.appendChild(stat);
  const badge = document.createElement('span'); badge.className = 'badge ' + n.mode; badge.textContent = n.mode === 'shell' ? 'shell' : 'agent';
  const nid = document.createElement('span'); nid.className = 'nid'; nid.textContent = id;
  const acts = document.createElement('span'); acts.className = 'acts';
  const eb = document.createElement('span'); eb.className = 'e'; eb.textContent = 'edit'; eb.onclick = (e) => { e.stopPropagation(); openNodeModal(id); };
  const xb = document.createElement('span'); xb.className = 'x'; xb.textContent = '✕'; xb.onclick = (e) => { e.stopPropagation(); deleteNode(id); };
  acts.appendChild(eb); acts.appendChild(xb);
  head.appendChild(badge);
  if (PL.g.forEach && (PL.g.forEach.before || []).includes(id)) { const sb = document.createElement('span'); sb.className = 'badge setup'; sb.textContent = 'setup'; head.appendChild(sb); }
  head.appendChild(nid); head.appendChild(acts);
  const body = document.createElement('div'); body.className = 'nbody';
  if (n.project) { const pj = document.createElement('div'); pj.className = 'nproj'; pj.textContent = n.project; body.appendChild(pj); }
  const pr = document.createElement('div'); pr.className = 'nprompt'; pr.textContent = ((n.prompt || '').split('\n')[0] || '').slice(0, 60) || 'no prompt'; body.appendChild(pr);
  if (n.outputs && n.outputs.length) { const ou = document.createElement('div'); ou.className = 'nouts'; ou.textContent = 'outputs: ' + n.outputs.map((o) => o.name).join(', '); body.appendChild(ou); }
  const pout = document.createElement('div'); pout.className = 'pl-port out'; pout.dataset.id = id;
  el.appendChild(pin); el.appendChild(head); el.appendChild(body); el.appendChild(pout);
  updateNodeOutputLine(el, id);
  const onHead = (e) => !e.target.closest('.acts');
  head.onmousedown = (e) => { if (onHead(e)) startDrag(e, id); };
  head.ondblclick = (e) => { if (onHead(e)) { e.stopPropagation(); openNodeModal(id); } };
  head.onclick = (e) => { if (onHead(e)) { e.stopPropagation(); PL.sel = id; [...$('pl-viewport').querySelectorAll('.pl-node.sel')].forEach((x) => x.classList.remove('sel')); document.getElementById('pln-' + id).classList.add('sel'); } };
  pout.onmousedown = (e) => startLink(e, id);
  return el;
}
function deleteNode(id) {
  delete PL.g.nodes[id]; delete PL.g.layout[id];
  for (const k of Object.keys(PL.g.nodes)) PL.g.nodes[k].depends_on = PL.g.nodes[k].depends_on.filter((x) => x !== id);
  if (PL.sel === id) PL.sel = null; renderCanvas(); renderInspector();
}
// Show a one-line preview of a node's last-run output on its card.
function updateNodeOutputLine(el, id) {
  let rv = el.querySelector('.nout-val');
  const ro = PL.runOutputs && PL.runOutputs[id];
  const val = ro && ro.outputs ? (Object.values(ro.outputs)[0] || '') : '';
  const line = val ? '→ ' + (((val.split('\n').find((l) => l.trim())) || val).slice(0, 60)) : '';
  if (!line) { if (rv) rv.remove(); return; }
  if (!rv) { rv = document.createElement('div'); rv.className = 'nout-val'; (el.querySelector('.nbody') || el).appendChild(rv); }
  rv.textContent = line;
}
function refreshNodeOutputs() { if (!PL.g) return; for (const id of Object.keys(PL.g.nodes)) { const el = document.getElementById('pln-' + id); if (el) updateNodeOutputLine(el, id); } }
function drawEdges() {
  const svg = $('pl-edges'); svg.innerHTML = '';
  for (const id of Object.keys(PL.g.nodes)) for (const dep of PL.g.nodes[id].depends_on) {
    const a = nodePorts(dep), b = nodePorts(id); if (!a || !b) continue;
    svg.appendChild(edgePath(a.outX, a.outY, b.inX, b.inY, dep, id));
  }
}
const SVGNS = 'http://www.w3.org/2000/svg';
function edgePath(x1, y1, x2, y2, dep, id) {
  const dy = Math.max(30, Math.abs(y2 - y1) / 2);
  const d = 'M' + x1 + ',' + y1 + ' C' + x1 + ',' + (y1 + dy) + ' ' + x2 + ',' + (y2 - dy) + ' ' + x2 + ',' + y2;
  const g = document.createElementNS(SVGNS, 'g');
  const p = document.createElementNS(SVGNS, 'path'); p.setAttribute('d', d);
  const hit = document.createElementNS(SVGNS, 'path'); hit.setAttribute('d', d); hit.setAttribute('class', 'hit');
  hit.onclick = () => { PL.g.nodes[id].depends_on = PL.g.nodes[id].depends_on.filter((x) => x !== dep); renderCanvas(); renderInspector(); };
  g.appendChild(p); g.appendChild(hit); return g;
}
/* ---- node drag ---- */
let plDrag = null;
function startDrag(e, id) { e.preventDefault(); const p = PL.g.layout[id]; plDrag = { id, sx: e.clientX, sy: e.clientY, ox: p.x, oy: p.y }; document.onmousemove = onDragMove; document.onmouseup = endDrag; }
function onDragMove(e) {
  if (!plDrag) return;
  const nx = plDrag.ox + (e.clientX - plDrag.sx) / PL.view.z, ny = plDrag.oy + (e.clientY - plDrag.sy) / PL.view.z;
  PL.g.layout[plDrag.id] = { x: nx, y: ny };
  const el = document.getElementById('pln-' + plDrag.id); if (el) { el.style.left = nx + 'px'; el.style.top = ny + 'px'; }
  drawEdges();
}
function endDrag() { plDrag = null; document.onmousemove = null; document.onmouseup = null; }
/* ---- port linking (drag out→in to add a dependency) ---- */
let plLink = null;
function startLink(e, id) { e.preventDefault(); e.stopPropagation(); plLink = { from: id }; document.onmousemove = onLinkMove; document.onmouseup = endLink; }
function onLinkMove(e) {
  if (!plLink) return;
  document.querySelectorAll('.pl-port.in.tgt').forEach((p) => p.classList.remove('tgt'));
  const over = document.elementFromPoint(e.clientX, e.clientY);
  if (over && over.classList.contains('pl-port') && over.classList.contains('in')) over.classList.add('tgt');
  const from = nodePorts(plLink.from); if (!from) return;
  const m = toGraph(e.clientX, e.clientY);
  drawEdges();
  const p = document.createElementNS(SVGNS, 'path'); p.setAttribute('d', 'M' + from.outX + ',' + from.outY + ' L' + m.x + ',' + m.y); p.setAttribute('stroke-dasharray', '5,4'); $('pl-edges').appendChild(p);
}
function endLink(e) {
  const over = document.elementFromPoint(e.clientX, e.clientY);
  if (plLink && over && over.classList.contains('pl-port') && over.classList.contains('in')) {
    const to = over.dataset.id;
    if (to && to !== plLink.from && PL.g.nodes[to] && !PL.g.nodes[to].depends_on.includes(plLink.from)) PL.g.nodes[to].depends_on.push(plLink.from);
  }
  plLink = null; document.querySelectorAll('.pl-port.in.tgt').forEach((p) => p.classList.remove('tgt'));
  document.onmousemove = null; document.onmouseup = null; renderCanvas(); renderInspector();
}
/* ---- inspector ---- */
/* node edit modal */
function nodeModalOpen() { return $('pl-nodemodal').classList.contains('open'); }
function openNodeModal(id) { PL.sel = id; renderCanvas(); $('pl-nodemodal').classList.add('open'); buildNodeForm($('pl-nm-body')); }
function closeNodeModal() { stopNodeLogPoll(); $('pl-nodemodal').classList.remove('open'); }
function stopNodeLogPoll() { if (PL.logPoll) { clearInterval(PL.logPoll); PL.logPoll = null; } }
// Simple ANSI strip (inline regex). Keep in sync with the server-side
// equivalent at crates/forge_workspace/src/pipeline.rs strip_ansi().
function stripAnsiJs(s) { return (s || '').replace(/\[[0-9;?]*[a-zA-Z]/g, ''); }
// Existing call sites use renderInspector() to "refresh the editor"; now that
// means re-rendering the modal body when it's open.
function renderInspector() { if (nodeModalOpen() && PL.g && PL.sel && PL.g.nodes[PL.sel]) buildNodeForm($('pl-nm-body')); else if (nodeModalOpen() && !(PL.g && PL.g.nodes[PL.sel])) closeNodeModal(); }
$('pl-nm-close').onclick = closeNodeModal;
$('pl-nm-done').onclick = closeNodeModal;
$('pl-nm-del').onclick = () => { if (PL.sel) deleteNode(PL.sel); closeNodeModal(); };
function buildNodeForm(box) {
  box.innerHTML = ''; const id = PL.sel, n = PL.g && PL.g.nodes[id]; if (!n) return;
  const row = (label, el) => { const r = document.createElement('div'); r.className = 'pl-insp-row'; const l = document.createElement('label'); l.textContent = label; r.appendChild(l); r.appendChild(el); box.appendChild(r); };
  const idIn = document.createElement('input'); idIn.value = id;
  idIn.onchange = () => { const nid = idIn.value.trim(); if (!nid || nid === id || PL.g.nodes[nid]) { idIn.value = id; return; } renameNode(id, nid); };
  row('node id', idIn);
  const modeSel = document.createElement('select');
  for (const m of ['claude', 'shell']) { const o = document.createElement('option'); o.value = m; o.textContent = m === 'claude' ? 'agent (forge -p)' : 'shell'; if (n.mode === m) o.selected = true; modeSel.appendChild(o); }
  modeSel.onchange = () => { n.mode = modeSel.value; renderCanvas(); buildNodeForm(box); };
  row('mode', modeSel);
  const pjIn = document.createElement('input'); pjIn.value = n.project || ''; pjIn.placeholder = 'e.g. {{input.project}} or a path (optional)';
  pjIn.oninput = () => { n.project = pjIn.value; renderCanvas(); };
  row('project / cwd', pjIn);
  const pr = document.createElement('textarea'); pr.value = n.prompt || '';
  pr.oninput = () => { n.prompt = pr.value; const b = document.querySelector('#pln-' + id + ' .nprompt'); if (b) b.textContent = ((pr.value.split('\n')[0] || '').slice(0, 60)) || 'no prompt'; };
  row(n.mode === 'shell' ? 'command' : 'prompt', pr);
  const outWrap = document.createElement('div');
  const renderOuts = () => {
    outWrap.innerHTML = '';
    (n.outputs || []).forEach((o, i) => {
      const r = document.createElement('div'); r.className = 'pl-out-row';
      const nm = document.createElement('input'); nm.value = o.name; nm.placeholder = 'name'; nm.oninput = () => { o.name = nm.value; renderCanvas(); };
      const ex = document.createElement('select'); for (const x of ['result', 'stdout', 'git_diff']) { const op = document.createElement('option'); op.value = x; op.textContent = x; if (o.extract === x) op.selected = true; ex.appendChild(op); } ex.onchange = () => (o.extract = ex.value);
      const rm = document.createElement('button'); rm.className = 'btn btn-ghost btn-sm'; rm.textContent = '✕'; rm.onclick = () => { n.outputs.splice(i, 1); renderOuts(); renderCanvas(); };
      r.appendChild(nm); r.appendChild(ex); r.appendChild(rm); outWrap.appendChild(r);
    });
    const add = document.createElement('button'); add.className = 'btn btn-sm'; add.textContent = '+ output'; add.onclick = () => { n.outputs = n.outputs || []; n.outputs.push({ name: 'out', extract: n.mode === 'shell' ? 'stdout' : 'result' }); renderOuts(); renderCanvas(); };
    outWrap.appendChild(add);
  };
  renderOuts(); row('outputs', outWrap);
  if (n.depends_on && n.depends_on.length) {
    const dw = document.createElement('div'); dw.style.cssText = 'display:flex;flex-wrap:wrap;gap:4px';
    n.depends_on.forEach((dep) => { const t = document.createElement('span'); t.className = 'pl-pill'; t.style.cssText = 'background:#8a8f98;cursor:pointer'; t.title = 'remove dependency'; t.textContent = dep + ' ✕'; t.onclick = () => { n.depends_on = n.depends_on.filter((x) => x !== dep); renderCanvas(); buildNodeForm(box); }; dw.appendChild(t); });
    row('depends on (edit on canvas)', dw);
  }
  // last-run output (read-only) — what this node produced on the most recent run
  const ro = PL.runOutputs && PL.runOutputs[id];
  if (ro && (ro.status || ro.error || Object.keys(ro.outputs || {}).length)) {
    const r = document.createElement('div'); r.className = 'pl-insp-row';
    const l = document.createElement('label'); l.textContent = 'last run — ' + (ro.status || ''); r.appendChild(l);
    if (ro.error) { const e = document.createElement('div'); e.style.cssText = 'color:#e5484d;font-size:11px;white-space:pre-wrap'; e.textContent = ro.error; r.appendChild(e); }
    for (const [k, v] of Object.entries(ro.outputs || {})) {
      const oh = document.createElement('div'); oh.style.cssText = 'font-size:10px;color:#2e9e44;margin-top:4px;font-weight:600'; oh.textContent = k;
      const ta = document.createElement('textarea'); ta.readOnly = true; ta.value = v; ta.style.minHeight = '54px';
      r.appendChild(oh); r.appendChild(ta);
    }
    box.appendChild(r);
  }
  // live run log (streamed stdout) — polls while the node is running
  stopNodeLogPoll();
  if (PL.runId) {
    const r = document.createElement('div'); r.className = 'pl-insp-row';
    const l = document.createElement('label'); l.textContent = 'run log'; r.appendChild(l);
    const ta = document.createElement('textarea'); ta.readOnly = true; ta.style.minHeight = '90px'; ta.placeholder = '(no log for this node yet)'; r.appendChild(ta);
    box.appendChild(r);
    const fetchLog = async () => {
      const d = await (await api('/api/pipeline/node-log?run=' + encodeURIComponent(PL.runId) + '&node=' + encodeURIComponent(id))).json().catch(() => null);
      if (!d) return;
      const atBottom = ta.scrollTop + ta.clientHeight >= ta.scrollHeight - 8;
      ta.value = stripAnsiJs(d.log || '').slice(-8000);
      if (atBottom) ta.scrollTop = ta.scrollHeight;
    };
    fetchLog();
    if ((PL.runOutputs[id] || {}).status === 'running') {
      PL.logPoll = setInterval(async () => {
        if (!nodeModalOpen()) { stopNodeLogPoll(); return; }
        await fetchLog();
        const cur = (PL.runOutputs[id] || {}).status;
        if (cur && cur !== 'running') stopNodeLogPoll();
      }, 1500);
    }
  }
}
function renameNode(oldId, nid) {
  const g = PL.g; g.nodes[nid] = g.nodes[oldId]; delete g.nodes[oldId];
  g.layout[nid] = g.layout[oldId]; delete g.layout[oldId];
  for (const k of Object.keys(g.nodes)) g.nodes[k].depends_on = g.nodes[k].depends_on.map((x) => (x === oldId ? nid : x));
  PL.sel = nid; renderCanvas(); if (nodeModalOpen()) buildNodeForm($('pl-nm-body'));
}
