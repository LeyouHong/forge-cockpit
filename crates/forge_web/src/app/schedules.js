/* ---------- schedules (timed automation) ---------- */
const SCH = { list: [], poll: null };
$('sched-open').onclick = () => { $('sched-overlay').classList.add('open'); schedLoad(); SCH.poll = setInterval(() => { if (!$('sched-overlay').classList.contains('open')) { clearInterval(SCH.poll); return; } schedLoad(); }, 5000); };
$('sched-close').onclick = () => { clearInterval(SCH.poll); $('sched-overlay').classList.remove('open'); };
$('sched-refresh').onclick = () => schedLoad(true);
$('sched-m-close').onclick = () => $('sched-modal').classList.remove('open');
$('sched-new').onclick = () => schedOpenModal(null);

async function schedLoad(force) {
  const d = await api('/api/schedules').then((r) => r.json()).catch(() => ({ schedules: [] }));
  SCH.list = d.schedules || [];
  // Don't rebuild (and collapse expanded histories) when nothing changed.
  const sig = JSON.stringify(SCH.list);
  if (!force && sig === SCH._sig) return;
  SCH._sig = sig;
  SCH.expanded = SCH.expanded || new Set();
  const box = $('sched-list'); box.innerHTML = '';
  $('sched-empty').textContent = SCH.list.length ? '' : 'no schedules yet — "+ New schedule" fires a pipeline or a prompt on a timer.';
  for (const s of SCH.list) box.appendChild(schedRow(s));
}
function schedTrigDesc(s) {
  if (s.trigger === 'every') return 'every ' + s.every_minutes + ' min';
  if (s.trigger === 'cron') return 'cron ' + s.cron;
  if (s.trigger === 'once') return 'once at ' + s.at;
  return 'manual only';
}
function schedRow(s) {
  const colors = { idle: '#2e9e44', running: '#f59e0b', last_failed: '#e5484d', paused: '#8a8f98' };
  const el = document.createElement('div'); el.className = 'pl-run';
  const rh = document.createElement('div'); rh.className = 'rh';
  rh.innerHTML = '<span style="color:' + colors[s.state] + '">●</span> <b>' + escapeHtml(s.name) + '</b>'
    + ' <span class="sl">' + (s.body_kind === 'pipeline' ? '🧬 ' + escapeHtml(s.pipeline) : '💬 prompt') + ' · ' + escapeHtml(schedTrigDesc(s)) + (s.action_kind && s.action_kind !== 'none' ? ' · -> ' + escapeHtml(s.action_kind) : '') + '</span>'
    + '<span class="sl" style="margin-left:auto">' + (s.next_run_at && s.enabled ? 'next ' + new Date(s.next_run_at).toLocaleString() : '') + '</span>';
  const bar = document.createElement('div'); bar.style.cssText = 'display:flex;gap:6px;margin-top:5px;align-items:center';
  const btn = (label, fn, title) => { const b = document.createElement('button'); b.className = 'btn btn-sm'; b.textContent = label; if (title) b.title = title; b.onclick = fn; bar.appendChild(b); };
  btn('▶ Run now', async () => { await api('/api/schedules/fire', plBody({ b: { id: s.id } })); schedLoad(true); });
  btn(s.enabled ? '⏸ Pause' : '▶ Resume', async () => { const u = Object.assign({}, s, { enabled: !s.enabled }); await api('/api/schedules/update', plBody({ b: u })); schedLoad(true); });
  btn('✎ Edit', () => schedOpenModal(s));
  btn('✕ Delete', async () => { if (!(await uiConfirm('Delete schedule "' + s.name + '"?', { confirmText: 'Delete', danger: true }))) return; await api('/api/schedules/delete', plBody({ b: { id: s.id } })); schedLoad(true); });
  const hist = document.createElement('a'); hist.href = '#'; hist.className = 'sl'; hist.textContent = 'runs ▾'; hist.style.marginLeft = 'auto';
  const runsBox = document.createElement('div'); runsBox.style.cssText = 'display:none;margin-top:6px;border-top:1px solid var(--border);padding-top:6px';
  const loadRunHistory = async () => {
    const d = await api('/api/schedule-runs?id=' + encodeURIComponent(s.id)).then((r) => r.json()).catch(() => ({ runs: [] }));
    runsBox.innerHTML = '';
    if (!(d.runs || []).length) runsBox.innerHTML = '<span class="sl">(no runs yet)</span>';
    for (const r of d.runs || []) {
      const row = document.createElement('div'); row.style.cssText = 'font-size:11px;margin-bottom:4px';
      const acs = r.action_status ? ' · action: ' + escapeHtml(r.action_status) : '';
      row.innerHTML = '<b style="color:' + (r.status === 'done' ? '#2e9e44' : r.status === 'failed' ? '#e5484d' : '#f59e0b') + '">' + r.status + '</b> · '
        + new Date(r.started_at).toLocaleString() + ' · ' + escapeHtml(r.fired_by) + acs;
      if (r.output_tail) { const pre = document.createElement('pre'); pre.style.cssText = 'font-size:10px;white-space:pre-wrap;max-height:220px;overflow:auto;background:var(--bg-2,rgba(0,0,0,.04));padding:5px;border-radius:6px;margin:3px 0 0'; pre.textContent = r.output_tail; row.appendChild(pre); }
      runsBox.appendChild(row);
    }
    runsBox.style.display = '';
    hist.textContent = 'runs ▴';
  };
  hist.onclick = async (e) => {
    e.preventDefault();
    SCH.expanded = SCH.expanded || new Set();
    if (runsBox.style.display === 'none') { SCH.expanded.add(s.id); await loadRunHistory(); }
    else { SCH.expanded.delete(s.id); runsBox.style.display = 'none'; hist.textContent = 'runs ▾'; }
  };
  // A rebuild (data changed) re-opens histories the user had expanded.
  if (SCH.expanded && SCH.expanded.has(s.id)) loadRunHistory();
  bar.appendChild(hist);
  el.appendChild(rh); el.appendChild(bar); el.appendChild(runsBox);
  return el;
}

async function schedOpenModal(s) {
  $('sched-m-title').textContent = s ? 'Edit — ' + s.name : 'New schedule';
  $('sched-m-msg').textContent = '';
  const b = $('sched-m-body'); b.innerHTML = '';
  const row = (label, node) => { const r = document.createElement('div'); r.className = 'pl-insp-row'; const l = document.createElement('label'); l.textContent = label; r.appendChild(l); r.appendChild(node); b.appendChild(r); };
  const nameIn = document.createElement('input'); nameIn.value = s ? s.name : ''; nameIn.placeholder = 'e.g. nightly PR review';
  row('name', nameIn);

  const kindSel = document.createElement('select');
  for (const k of ['pipeline', 'prompt']) { const o = document.createElement('option'); o.value = k; o.textContent = k === 'pipeline' ? '🧬 pipeline' : '💬 prompt (one-shot forge task)'; if (s && s.body_kind === k) o.selected = true; kindSel.appendChild(o); }
  row('body', kindSel);

  const pipeWrap = document.createElement('div');
  const pipeSel = document.createElement('select');
  const files = await api('/api/pipeline/files').then((r) => r.json()).catch(() => ({ files: [] }));
  for (const f of files.files || []) { const o = document.createElement('option'); o.value = f; o.textContent = f; if (s && s.pipeline === f) o.selected = true; pipeSel.appendChild(o); }
  pipeWrap.appendChild(pipeSel);
  const inputsBox = document.createElement('div'); inputsBox.style.marginTop = '6px'; pipeWrap.appendChild(inputsBox);
  const renderInputs = async () => {
    inputsBox.innerHTML = '';
    if (!pipeSel.value) return;
    const g = await api('/api/pipeline/graph?name=' + encodeURIComponent(pipeSel.value)).then((r) => r.json()).catch(() => null);
    const keys = Object.keys((g && g.workflow && g.workflow.input) || {});
    for (const k of keys) {
      const l = document.createElement('div'); l.className = 'sl'; l.textContent = k; inputsBox.appendChild(l);
      const i = document.createElement('input'); i.dataset.k = k; i.className = 'sched-inp';
      i.value = (s && s.inputs && s.inputs[k]) || '';
      inputsBox.appendChild(i);
    }
  };
  pipeSel.onchange = renderInputs;
  row('pipeline', pipeWrap);
  await renderInputs();

  const promptTa = document.createElement('textarea'); promptTa.value = s ? (s.prompt || '') : '';
  promptTa.placeholder = 'free-text instructions run as a one-shot forge task on each fire';
  row('prompt', promptTa);
  const agentIn = document.createElement('input'); agentIn.value = s ? (s.agent || '') : ''; agentIn.placeholder = 'optional forge agent id (prompt body)';
  row('agent', agentIn);
  const dirIn = document.createElement('input'); dirIn.value = s ? (s.dir || '') : ''; dirIn.placeholder = '~/path/to/project (working dir; empty = home)';
  row('directory', dirIn);

  const trigSel = document.createElement('select');
  for (const t of ['every', 'cron', 'once', 'manual']) { const o = document.createElement('option'); o.value = t; o.textContent = { every: 'every N minutes', cron: 'cron expression', once: 'once at a time', manual: 'manual only' }[t]; if (s && s.trigger === t) o.selected = true; trigSel.appendChild(o); }
  row('trigger', trigSel);
  const everyIn = document.createElement('input'); everyIn.type = 'number'; everyIn.min = '1'; everyIn.value = s && s.every_minutes ? s.every_minutes : 60;
  row('every (minutes)', everyIn);
  const cronIn = document.createElement('input'); cronIn.value = s ? (s.cron || '') : ''; cronIn.placeholder = '0 9 * * 1-5  (9am Mon–Fri)';
  row('cron', cronIn);
  const atIn = document.createElement('input'); atIn.type = 'datetime-local'; if (s && s.at) atIn.value = s.at.slice(0, 16);
  row('once at', atIn);


  const syncVis = () => {
    pipeWrap.parentElement.style.display = kindSel.value === 'pipeline' ? '' : 'none';
    promptTa.parentElement.style.display = kindSel.value === 'prompt' ? '' : 'none';
    agentIn.parentElement.style.display = kindSel.value === 'prompt' ? '' : 'none';
    everyIn.parentElement.style.display = trigSel.value === 'every' ? '' : 'none';
    cronIn.parentElement.style.display = trigSel.value === 'cron' ? '' : 'none';
    atIn.parentElement.style.display = trigSel.value === 'once' ? '' : 'none';
  };
  kindSel.onchange = syncVis; trigSel.onchange = syncVis; syncVis();

  $('sched-m-save').onclick = async () => {
    const inputs = {}; for (const el of inputsBox.querySelectorAll('.sched-inp')) inputs[el.dataset.k] = el.value;
    const body = {
      id: s ? s.id : '', name: nameIn.value.trim(), enabled: s ? s.enabled : true,
      body_kind: kindSel.value, pipeline: pipeSel.value || '', inputs,
      dir: dirIn.value.trim(), prompt: promptTa.value, agent: agentIn.value.trim(),
      trigger: trigSel.value, every_minutes: parseInt(everyIn.value, 10) || 0,
      cron: cronIn.value.trim(), at: atIn.value,
      action_kind: (s && s.action_kind) || 'none',
      action_config: (s && s.action_config) || {},
    };
    const r = await api(s ? '/api/schedules/update' : '/api/schedules', plBody({ b: body, m: s ? 'POST' : 'POST' }));
    const d = await r.json().catch(() => ({}));
    if (!r.ok) { $('sched-m-msg').textContent = d.error || 'save failed'; return; }
    $('sched-modal').classList.remove('open');
    schedLoad();
  };
  $('sched-modal').classList.add('open');
}
