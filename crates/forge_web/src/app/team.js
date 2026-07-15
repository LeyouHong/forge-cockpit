/* ---------- team (resident agent team) page ---------- */
const TEAM = { project: null, poll: null };
/* The team roster is DATA (.team.json via /api/team/config): members carry
   id/name/icon/stage/agent/role_prompt/depends_on, the canvas edits them. */
const TM_STAGES = {
  plan: { statuses: [], desc: 'plans before the pipeline' },
  implement: { statuses: ['open', 'in_progress'] },
  review: { statuses: ['review'] },
  qa: { statuses: ['qa'] },
};
const TM_DONE = { id: 'done', name: 'Done', icon: '🏁', statuses: ['done', 'rejected'] };
async function tmLoadConfig() {
  const d = await api('/api/team/config?project=' + encodeURIComponent(TEAM.project)).then((r) => r.json()).catch(() => null);
  TEAM.cfg = d && Array.isArray(d.members) ? d : { members: [], positions: {} };
}
function tmPos(id, i) {
  const p = (TEAM.cfg && TEAM.cfg.positions || {})[id];
  return p && typeof p.x === 'number' ? p : { x: 24 + i * 244, y: 90 };
}
async function tmSaveConfig() {
  const body = Object.assign({ project: TEAM.project }, TEAM.cfg);
  const r = await api('/api/team/config', { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
  if (!r.ok) {
    const d = await r.json().catch(() => ({}));
    $('tm-status').textContent = '✗ ' + (d.error || 'team save failed'); $('tm-status').style.color = '#e5484d';
    await tmLoadConfig(); TEAM._sig = null; loadTeam();
    return false;
  }
  TEAM._sig = null;
  return true;
}
function tmMakeDraggable(el, id) {
  const th = el.querySelector('.th'); if (!th) return;
  th.style.cursor = 'grab';
  th.addEventListener('mousedown', (e) => {
    if (e.target.closest('button,select,input')) return;
    e.preventDefault();
    const sx = e.clientX, sy = e.clientY, ox = el.offsetLeft, oy = el.offsetTop;
    let moved = false;
    const move = (ev) => {
      moved = true;
      el.style.left = Math.max(0, ox + ev.clientX - sx) + 'px';
      el.style.top = Math.max(0, oy + ev.clientY - sy) + 'px';
      tmEdges();
    };
    const up = () => {
      document.removeEventListener('mousemove', move); document.removeEventListener('mouseup', up);
      if (!moved) return;
      TEAM.cfg.positions = TEAM.cfg.positions || {};
      TEAM.cfg.positions[id] = { x: el.offsetLeft, y: el.offsetTop };
      tmSaveConfig();
    };
    document.addEventListener('mousemove', move); document.addEventListener('mouseup', up);
  });
}
function openTeam() { $('team-overlay').classList.add('open'); tmLoadProjects(); tmShowTab(TEAM.tab || 'ws'); }
$('team-open').onclick = openTeam;
$('team-close').onclick = () => { tmStopPoll(); tmDisposeTermDock(); $('team-overlay').classList.remove('open'); };
$('tm-refresh').onclick = () => loadTeam();
function tmStopPoll() { if (TEAM.poll) { clearInterval(TEAM.poll); TEAM.poll = null; } }
async function tmLoadProjects() {
  const d = await (await api('/api/pipeline/projects')).json().catch(() => ({ projects: [] }));
  const sel = $('tm-project'); sel.innerHTML = '';
  for (const p of (d.projects || [])) { const o = document.createElement('option'); o.value = p.name; o.textContent = p.name; sel.appendChild(o); }
  if ((d.projects || []).length) {
    if (!TEAM.project || !d.projects.find((p) => p.name === TEAM.project)) TEAM.project = d.projects[0].name;
    sel.value = TEAM.project; startTeamPoll();
  } else {
    TEAM.project = null; [...$('tm-canvas').querySelectorAll('.tm-card')].forEach((c) => c.remove());
    $('tm-edges').innerHTML = ''; $('tm-alerts').innerHTML = ''; $('tm-empty').textContent = 'no projects — click "+ Project" to add a folder.';
  }
}
$('tm-export').onclick = async () => {
  if (!TEAM.project) return;
  const d = await api('/api/team/yaml?project=' + encodeURIComponent(TEAM.project)).then((r) => r.json()).catch(() => null);
  if (!d || !d.yaml) { $('tm-status').textContent = '✗ export failed'; return; }
  const a = document.createElement('a');
  a.href = URL.createObjectURL(new Blob([d.yaml], { type: 'text/yaml' }));
  a.download = 'team-' + TEAM.project + '.yaml'; a.click(); URL.revokeObjectURL(a.href);
};
$('tm-import').onclick = () => {
  if (!TEAM.project) return;
  const f = document.createElement('input'); f.type = 'file'; f.accept = '.yaml,.yml,text/yaml';
  f.onchange = async () => {
    const file = f.files && f.files[0]; if (!file) return;
    if (!(await uiConfirm('Replace the "' + TEAM.project + '" team with ' + file.name + '?', { confirmText: 'Replace' }))) return;
    const yaml = await file.text();
    const r = await api('/api/team/yaml', plBody({ b: { project: TEAM.project, yaml } }));
    const d = await r.json().catch(() => ({}));
    if (!r.ok) { $('tm-status').textContent = '✗ ' + (d.error || 'import failed'); $('tm-status').style.color = '#e5484d'; return; }
    $('tm-status').textContent = '✓ imported ' + d.members + ' members'; $('tm-status').style.color = '#2e9e44';
    await tmLoadConfig(); TEAM._sig = null; loadTeam();
  };
  f.click();
};
$('tm-addproject').onclick = () => openPicker('', async (p) => {
  const name = await addProjectPath(p);
  if (name) { TEAM.project = name; tmLoadProjects(); }
});
$('tm-project').onchange = () => { TEAM.project = $('tm-project').value; startTeamPoll(); };
$('tm-run').onclick = () => { tmRun(false); };
function autoGrowTextarea(id, onEnter) {
  const g = $(id);
  const grow = () => { g.style.height = 'auto'; g.style.height = Math.min(g.scrollHeight, 260) + 'px'; };
  g.addEventListener('input', grow);
  g.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); onEnter(); }
  });
  setTimeout(grow, 0);
}
autoGrowTextarea('tm-goal', () => tmRun(false));
$('tm-daemon').onclick = () => { if (TEAM.running) tmStop(); else { tmRun(true); } };
async function tmRun(daemon) {
  if (!TEAM.project) { $('tm-status').textContent = 'add a project first'; return; }
  const goal = $('tm-goal').value.trim();
  const r = await api('/api/team/run', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ project: TEAM.project, goal, daemon }) });
  const d = await r.json();
  if (!r.ok) { $('tm-status').textContent = '✗ ' + (d.error || 'failed'); $('tm-status').style.color = '#e5484d'; return; }
  $('tm-status').textContent = (daemon ? '⚡ daemon started' : '▶ team started') + ' (pid ' + d.pid + ')'; $('tm-status').style.color = '#2e9e44';
  startTeamPoll();
}
async function tmStop() {
  if (!TEAM.project) return;
  // Full knock-off: stop the orchestrator AND tear down the resident terminals.
  const r = await api('/api/team/stop', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ project: TEAM.project, teardown: true }) });
  const d = await r.json().catch(() => ({}));
  const n = (d && d.torn_down && d.torn_down.length) || 0;
  $('tm-status').textContent = n ? ('stopped — tore down ' + n + ' terminal' + (n > 1 ? 's' : '')) : 'stopped';
  $('tm-status').style.color = 'var(--muted)'; loadTeam();
}
function startTeamPoll() { tmStopPoll(); TEAM._sig = null; tmLoadConfig().then(loadTeam); TEAM.poll = setInterval(() => { if (!$('team-overlay').classList.contains('open')) { tmStopPoll(); return; } loadTeam(); }, 3000); }
async function loadTeam() {
  if (!TEAM.project) return;
  const [d, st, act] = await Promise.all([
    api('/api/team?project=' + encodeURIComponent(TEAM.project)).then((r) => r.json()).catch(() => null),
    api('/api/team/status?project=' + encodeURIComponent(TEAM.project)).then((r) => r.json()).catch(() => ({ members: {} })),
    api('/api/team/activity?project=' + encodeURIComponent(TEAM.project)).then((r) => r.json()).catch(() => null),
  ]);
  TEAM.status = (st && st.members) || {};
  renderActivity(act);
  if (d) renderTeam(d);
  tmSyncAllTermBtn();
  tmRenderTermDock();
}
function renderActivity(act) {
  const box = $('tm-activity');
  const cur = act && act.current;
  if (!cur) { box.style.display = 'none'; return; }
  box.style.display = '';
  // Humanize the current orchestrator line into a phase label.
  let label = cur;
  const m1 = cur.match(/⚑\s*(\w[\w-]*)\s*planning/);
  const m2 = cur.match(/→\s*(\S+)\s*\[(\w+)\].*→\s*(\S+)/);
  if (/planning done/.test(cur)) label = '✅ planning done — work is being scheduled';
  else if (m1) label = '📋 planning: ' + m1[1] + ' is working…';
  else if (m2) label = '🔨 ' + m2[3] + ' is working ' + m2[1] + ' [' + m2[2] + ']';
  else if (/idle, waiting/.test(cur)) label = '⏸ idle — waiting for new requests (daemon)';
  else if (/STUCK/.test(cur)) label = '⚠ ' + cur;
  $('tm-act-cur').innerHTML = '<b>Activity:</b> ' + escapeHtml(label);
  $('tm-act-trace').textContent = ((act.trace || []).join('\n'));
}
$('tm-act-toggle').onclick = (e) => {
  e.preventDefault();
  const t = $('tm-act-trace');
  const open = t.style.display !== 'none';
  t.style.display = open ? 'none' : '';
  $('tm-act-toggle').textContent = open ? 'trace ▾' : 'trace ▴';
};
function renderTeam(d) {
  const reqs = d.requests || [], msgs = d.messages || [];
  TEAM.running = !!d.running;
  $('tm-daemon').textContent = d.running ? '■ Stop' : '⚡ Start Daemon';
  if (!d.running && ($('tm-status').textContent || '').indexOf('started') !== -1) {
    // One-shot Run auto-stops when the orchestrator exits; reflect it.
    $('tm-status').textContent = '✓ team finished — all work processed'; $('tm-status').style.color = 'var(--muted)';
    $('tm-activity').style.display = 'none';
  }
  $('tm-empty').textContent = reqs.length ? '' : (d.running
    ? '⏳ team is running — the planning chain (pm → architect → …) takes a few minutes before requests appear here.'
    : 'no work on the board yet — enter a goal + Run, or start it from the terminal.');
  // Only rebuild the canvas when the board data actually changed, so an open
  // agent dropdown isn't reset by the 3s poll.
  const sig = JSON.stringify([reqs, msgs, TEAM.cfg, TEAM.status]);
  if (sig === TEAM._sig) return;
  TEAM._sig = sig;
  const alerts = msgs.filter((m) => m.to === 'human' && m.category === 'ticket');
  const ab = $('tm-alerts'); ab.innerHTML = '';
  api('/api/team/approvals?project=' + encodeURIComponent(TEAM.project)).then((r) => r.json()).then((d) => {
    for (const key of d.pending || []) {
      const el = document.createElement('div'); el.className = 'tm-alert';
      el.innerHTML = '<b>⏸ approval</b> — ' + escapeHtml(key) + ' ';
      const b = document.createElement('button'); b.className = 'btn btn-sm'; b.textContent = '✓ Approve';
      b.onclick = async () => { await api('/api/team/approve', plBody({ b: { project: TEAM.project, key } })); TEAM._sig = null; loadTeam(); };
      el.appendChild(b); ab.appendChild(el);
    }
  }).catch(() => {});
  for (const a of alerts.slice(0, 4)) { const el = document.createElement('div'); el.className = 'tm-alert'; el.innerHTML = '<b>⚠ alert</b> — ' + escapeHtml((a.body || '').slice(0, 200)); ab.appendChild(el); }
  const canvas = $('tm-canvas'); [...canvas.querySelectorAll('.tm-card')].forEach((c) => c.remove());
  const members = (TEAM.cfg && TEAM.cfg.members) || [];
  members.forEach((m, i) => canvas.appendChild(tmCard(m, i, reqs, msgs)));
  canvas.appendChild(tmDoneCard(members.length, reqs));
  tmEdges();
}
function tmCard(m, i, reqs, msgs) {
  const meta = TM_STAGES[m.stage] || TM_STAGES.implement;
  const pos = tmPos(m.id, i);
  const mine = reqs.filter((r) => meta.statuses.includes(r.status));
  const st0 = (TEAM.status || {})[m.id];
  const working = (st0 && st0.status === 'working')
    || mine.find((r) => r.claimed_by && r.claimed_by.indexOf(m.id) === 0);
  const inbox = msgs.filter((x) => x.to === m.id + '-1' && !x.read && x.category === 'ticket').length;
  const el = document.createElement('div'); el.className = 'tm-card' + (working ? ' active' : ''); el.id = 'tm-' + m.id;
  el.style.left = pos.x + 'px'; el.style.top = pos.y + 'px';
  const th = document.createElement('div'); th.className = 'th';
  th.innerHTML = '<span class="ico">' + escapeHtml(m.icon || '🤖') + '</span><span class="nm">' + escapeHtml(m.name || m.id) + '</span>';
  const edit = document.createElement('button'); edit.className = 'icon-btn'; edit.textContent = '✎'; edit.title = 'edit agent';
  edit.style.cssText = 'margin-left:auto;font-size:11px;padding:0 4px';
  edit.onclick = (e) => { e.stopPropagation(); tmOpenMember(m.id); };
  th.appendChild(edit);
  const cnt = document.createElement('span'); cnt.className = 'cnt' + (mine.length ? '' : ' zero'); cnt.textContent = mine.length; th.appendChild(cnt);
  const ms = (TEAM.status || {})[m.id];
  if (ms && ms.paused) { el.style.opacity = '0.55'; }
  if (ms) {
    const dotColor = ms.status === 'working' ? '#2e9e44' : ms.paused ? '#d4a72c' : '#8a8f98';
    const dot = document.createElement('span'); dot.title = 'session: ' + ms.status + (ms.request ? ' · ' + ms.request : '') + (ms.paused ? ' · paused (finishing current work, taking nothing new)' : '');
    dot.style.cssText = 'width:8px;height:8px;border-radius:50%;background:' + dotColor + ';margin-left:4px' + (ms.status === 'working' ? ';box-shadow:0 0 0 3px rgba(46,158,68,.25)' : '');
    th.appendChild(dot);
    const pp = document.createElement('span'); pp.textContent = ms.paused ? '▶' : '⏸';
    pp.title = ms.paused ? 'resume — start taking new work again' : 'pause — finish current work, take nothing new (requests for this stage wait)';
    pp.style.cssText = 'cursor:pointer;font-size:11px;margin-left:2px';
    pp.onclick = async (e) => {
      e.stopPropagation();
      await api('/api/team/pause', plBody({ b: { project: TEAM.project, member: m.id, paused: !ms.paused } }));
      TEAM._sig = null; loadTeam();
    };
    th.appendChild(pp);
    if (ms.status === 'working') {
      const ii = document.createElement('span'); ii.textContent = '⎋';
      ii.title = 'interrupt — stop this agent\'s current turn now (it retries fresh; the session stays alive)';
      ii.style.cssText = 'cursor:pointer;font-size:11px;margin-left:2px';
      ii.onclick = async (e) => {
        e.stopPropagation();
        ii.textContent = '…';
        await api('/api/team/interrupt', plBody({ b: { project: TEAM.project, member: m.id } }));
        setTimeout(() => { ii.textContent = '⎋'; }, 1200);
      };
      th.appendChild(ii);
    }
    if (ms.has_log) {
      const lg = document.createElement('span'); lg.textContent = '📜'; lg.title = 'view resident session log'; lg.style.cssText = 'cursor:pointer;font-size:11px;margin-left:2px';
      lg.onclick = (e) => { e.stopPropagation(); tmShowSession(m); };
      th.appendChild(lg);
    }
    if (ms.terminal) {
      const tm = document.createElement('span'); tm.textContent = '⌨'; tm.title = 'open this member\'s live terminal (watch or take over)';
      tm.style.cssText = 'cursor:pointer;font-size:11px;margin-left:2px';
      tm.onclick = (e) => { e.stopPropagation(); tmOpenTerminal(ms.terminal, m.name || m.id); };
      th.appendChild(tm);
    }
  }
  const tb = document.createElement('div'); tb.className = 'tb';
  if (m.stage === 'plan') {
    const d = document.createElement('div'); d.className = 'rq';
    d.textContent = (st0 && st0.request === 'planning')
      ? '⏳ planning…'
      : ((m.role_prompt || '').trim() ? 'custom SOP · ' + TM_STAGES.plan.desc : TM_STAGES.plan.desc);
    tb.appendChild(d);
  } else if (!mine.length) {
    const n = document.createElement('div'); n.className = 'none'; n.textContent = '(idle)'; tb.appendChild(n);
  } else {
    for (const r of mine.slice(0, 3)) {
      const q = document.createElement('div'); q.className = 'rq'; q.style.cursor = 'pointer'; q.title = 'view request';
      q.innerHTML = (r.claimed_by && r.claimed_by.indexOf(m.id) === 0 ? '<span class="dot">● </span>' : '') + escapeHtml(r.title || r.id);
      q.onclick = () => tmShowRequest(r.id); tb.appendChild(q);
    }
  }
  el.appendChild(th); el.appendChild(tb);
  const allMine = msgs.filter((x) => x.to === m.id + '-1');
  const unreadMine = allMine.filter((x) => !x.read);
  if (allMine.length) {
    const tf = document.createElement('div'); tf.className = 'tf'; tf.style.cursor = 'pointer'; tf.title = 'view inbox';
    tf.innerHTML = '📨 <span class="box">' + (unreadMine.length ? unreadMine.length + ' unread' : allMine.length + ' msg') + (inbox ? ' · ' + inbox + ' ticket' + (inbox > 1 ? 's' : '') : '') + '</span>';
    tf.onclick = (e) => { e.stopPropagation(); tmShowInbox(m, allMine); };
    el.appendChild(tf);
  }
  const af = document.createElement('div'); af.className = 'tf';
  const lbl = document.createElement('span'); lbl.textContent = 'terminal'; af.appendChild(lbl);
  const cmd = document.createElement('span'); cmd.style.cssText = 'margin-left:auto;font-size:10px;opacity:.75;max-width:130px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap';
  cmd.textContent = '⌨ ' + ((m.terminal_cmd || '').trim().split(/\s+/)[0] || 'claude');
  cmd.title = 'this member is a resident tmux terminal running: ' + ((m.terminal_cmd || '').trim() || 'claude --dangerously-skip-permissions') + '\nedit via ✎';
  af.appendChild(cmd); el.appendChild(af);
  tmMakeDraggable(el, m.id);
  const port = document.createElement('div'); port.className = 'tm-port';
  port.title = 'drag to another card to connect (that card will depend on this one)';
  port.onmousedown = (e) => tmStartConnect(e, m.id, el);
  el.appendChild(port);
  return el;
}
/* drag-to-connect: from a card's ◉ port to another card = target depends_on source */
function tmStartConnect(e, sourceId, sourceEl) {
  e.preventDefault(); e.stopPropagation();
  const canvas = $('tm-canvas'); const svg = $('tm-edges');
  const temp = document.createElementNS('http://www.w3.org/2000/svg', 'path');
  temp.setAttribute('stroke-dasharray', '4 3'); svg.appendChild(temp);
  const local = (ev) => {
    const r = canvas.getBoundingClientRect();
    return { x: ev.clientX - r.left + canvas.scrollLeft, y: ev.clientY - r.top + canvas.scrollTop };
  };
  const sx = sourceEl.offsetLeft + sourceEl.offsetWidth, sy = sourceEl.offsetTop + sourceEl.offsetHeight / 2;
  const move = (ev) => {
    const pt = local(ev); const mx = (sx + pt.x) / 2;
    temp.setAttribute('d', 'M' + sx + ',' + sy + ' C' + mx + ',' + sy + ' ' + mx + ',' + pt.y + ' ' + pt.x + ',' + pt.y);
  };
  const up = async (ev) => {
    document.removeEventListener('mousemove', move); document.removeEventListener('mouseup', up);
    temp.remove();
    const hit = document.elementFromPoint(ev.clientX, ev.clientY);
    const card = hit && hit.closest ? hit.closest('.tm-card') : null;
    if (!card) return;
    const targetId = card.id.replace(/^tm-/, '');
    if (targetId === sourceId) return;
    if (targetId === 'done') { $('tm-status').textContent = 'Done is automatic — qa-stage members already flow into it'; return; }
    const target = (TEAM.cfg.members || []).find((x) => x.id === targetId);
    if (!target) return;
    target.depends_on = target.depends_on || [];
    if (target.depends_on.includes(sourceId)) return;
    target.depends_on.push(sourceId);
    if (!(await tmSaveConfig())) { target.depends_on = target.depends_on.filter((d) => d !== sourceId); return; }
    loadTeam();
  };
  document.addEventListener('mousemove', move); document.addEventListener('mouseup', up);
}
function tmDoneCard(i, reqs) {
  const pos = tmPos('done', i);
  const mine = reqs.filter((r) => TM_DONE.statuses.includes(r.status));
  const el = document.createElement('div'); el.className = 'tm-card'; el.id = 'tm-done';
  el.style.left = pos.x + 'px'; el.style.top = pos.y + 'px';
  const th = document.createElement('div'); th.className = 'th';
  th.innerHTML = '<span class="ico">🏁</span><span class="nm">Done</span><span class="cnt' + (mine.length ? '' : ' zero') + '">' + mine.length + '</span>';
  const tb = document.createElement('div'); tb.className = 'tb';
  if (!mine.length) { const n = document.createElement('div'); n.className = 'none'; n.textContent = '(none yet)'; tb.appendChild(n); }
  else for (const r of mine.slice(0, 3)) { const q = document.createElement('div'); q.className = 'rq'; q.style.cursor = 'pointer'; q.innerHTML = escapeHtml(r.title || r.id); q.onclick = () => tmShowRequest(r.id); tb.appendChild(q); }
  el.appendChild(th); el.appendChild(tb);
  tmMakeDraggable(el, 'done');
  return el;
}
/* member add/edit modal */
$('tm-mem-close').onclick = () => $('tm-memmodal').classList.remove('open');
$('tm-addagent').onclick = () => tmOpenMember(null);
/* Workspace / Code tabs */
function tmShowTab(t) {
  TEAM.tab = t;
  $('tm-canvas').style.display = t === 'ws' ? '' : 'none';
  $('tm-code').style.display = t === 'code' ? '' : 'none';
  $('tm-term').style.display = t === 'term' ? 'flex' : 'none';
  $('tm-tab-ws').className = 'btn btn-sm' + (t === 'ws' ? ' btn-primary' : '');
  $('tm-tab-code').className = 'btn btn-sm' + (t === 'code' ? ' btn-primary' : '');
  $('tm-tab-term').className = 'btn btn-sm' + (t === 'term' ? ' btn-primary' : '');
  if (t === 'code') tmCodeBrowse(TEAM.codePath || '');
  if (t === 'term') tmRenderTermDock();
}
$('tm-tab-ws').onclick = () => tmShowTab('ws');
$('tm-tab-code').onclick = () => tmShowTab('code');
$('tm-tab-term').onclick = () => tmShowTab('term');
async function tmCodeBrowse(path) {
  TEAM.codePath = path;
  const d = await api('/api/team/files?project=' + encodeURIComponent(TEAM.project) + '&path=' + encodeURIComponent(path)).then((r) => r.json()).catch(() => ({ dirs: [], files: [] }));
  const box = $('tm-code-list'); box.innerHTML = '';
  const item = (label, fn, bold, dirty) => { const el = document.createElement('div'); el.textContent = label; el.style.cssText = 'cursor:pointer;padding:2px 4px;border-radius:4px' + (bold ? ';font-weight:600' : '') + (dirty || label.endsWith(' ●') ? ';color:#d97706' : ''); el.onmouseenter = () => el.style.background = 'var(--bg-2,rgba(0,0,0,.05))'; el.onmouseleave = () => el.style.background = ''; el.onclick = fn; box.appendChild(el); };
  const crumb = document.createElement('div'); crumb.className = 'sl'; crumb.textContent = '/' + (path || ''); crumb.style.marginBottom = '4px'; box.appendChild(crumb);
  if (path) item('⬆ ..', () => tmCodeBrowse(path.split('/').slice(0, -1).join('/')), true);
  for (const dir of d.dirs || []) item('📁 ' + dir.name + (dir.changed ? ' ●' : ''), () => tmCodeBrowse(path ? path + '/' + dir.name : dir.name), true);
  for (const f of d.files || []) item('📄 ' + f.name + (f.changed ? ' ●' : ''), () => tmShowCode(path ? path + '/' + f.name : f.name, f.changed), false, f.changed);
}
function tmHlDiff(text) {
  return text.split('\n').map((l) => {
    const e = escapeHtml(l);
    if (l.startsWith('+') && !l.startsWith('+++')) return '<span style="color:#2e9e44">' + e + '</span>';
    if (l.startsWith('-') && !l.startsWith('---')) return '<span style="color:#e5484d">' + e + '</span>';
    if (l.startsWith('@@')) return '<span style="color:#8b8fd6">' + e + '</span>';
    if (l.startsWith('diff ') || l.startsWith('commit ')) return '<b>' + e + '</b>';
    return e;
  }).join('\n');
}
// The File/Diff buttons are rebuilt inside tm-code-view on every render, so
// they are handled by delegation rather than per-button listeners.
document.addEventListener('click', (ev) => {
  const b = ev.target.closest('[data-code-rel]');
  if (b) tmShowCode(b.getAttribute('data-code-rel'), true, b.getAttribute('data-code-mode'));
});
async function tmShowCode(rel, changed, mode) {
  mode = mode || 'file';
  const view = $('tm-code-view'); view.innerHTML = '<span class="sl">…</span>';
  let bar = '';
  if (changed) {
    // Inline handlers cannot carry the CSP nonce, so these dispatch through
    // the delegated listener below. It also drops the old habit of splicing
    // a file path into an HTML attribute.
    bar = '<div style="margin-bottom:6px"><button class="btn btn-sm' + (mode === 'file' ? ' btn-primary' : '') + '" data-code-rel="' + escapeHtml(rel) + '" data-code-mode="file">File</button> ' +
          '<button class="btn btn-sm' + (mode === 'diff' ? ' btn-primary' : '') + '" data-code-rel="' + escapeHtml(rel) + '" data-code-mode="diff">Diff</button></div>';
  }
  if (mode === 'diff') {
    const dd = await api('/api/team/diff?project=' + encodeURIComponent(TEAM.project) + '&files=' + encodeURIComponent(rel)).then((r) => r.json()).catch(() => ({ diff: '' }));
    view.innerHTML = bar + tmHlDiff(dd.diff || '(no diff)');
    return;
  }
  const ff = await api('/api/team/file?project=' + encodeURIComponent(TEAM.project) + '&path=' + encodeURIComponent(rel)).then((r) => r.json()).catch(() => null);
  if (!ff || ff.content == null) { view.innerHTML = bar + '(cannot read file)'; return; }
  const lang = (rel.split('.').pop() || '').toLowerCase();
  view.innerHTML = bar + highlightCode(ff.content, lang);
}
function tmExampleSop(name, stage) {
  const work = {
    plan: ['1. Explore the project (`read`, `fs_search`) so your plan references real files and conventions.',
           '2. `list_requests()` — never duplicate work already on the board.',
           '3. Produce your planning artifact (PRD / design notes) in the workspace, then break the work into',
           '   small, independently-shippable requests via `create_request` — each with 2-5 TESTABLE acceptance criteria.'].join('\n'),
    implement: ['1. `list_requests(status: "open")` — find work for your stage; `claim_request(id)` before starting.',
           '2. `get_request(id)` — read the description and acceptance criteria; check `get_inbox` for rework notes.',
           '3. Implement the change WITH tests, following the project conventions.',
           '4. `submit_engineer_work(id, files_changed, notes)` — honest notes on what you did and verified.'].join('\n'),
    review: ['1. `list_requests(status: "review")` — find work waiting for review. If none, stop.',
           '2. Read the diff of every changed file; verify each acceptance criterion is actually met.',
           '3. Check correctness, security, performance, and maintainability — verify findings before reporting.',
           '4. `submit_review(id, result: "approved" | "changes_requested", findings)` — each finding: file, severity, description.'].join('\n'),
    qa: ['1. `list_requests(status: "qa")` — find work waiting for QA. If none, stop.',
           '2. `get_request(id)` — read the acceptance criteria and the engineer notes.',
           '3. Verify EACH criterion concretely: write and run a real test (`shell`); never guess.',
           '4. `submit_qa(id, result: "passed" | "failed", notes)` — what you tested and the results.'].join('\n'),
  }[stage] || '';
  return '# Role: ' + name + '\n\n' +
    'You are the **' + name + '** on a forge-cockpit workspace team. You coordinate ONLY through\n' +
    'the workspace MCP tools.\n\n## SOP\n\n' + work + '\n\n## Rules\n\n' +
    '- Coordinate only through the MCP tools — never hand-edit `request.yml` / `response.yml`.\n' +
    '- Verify against the acceptance criteria, not vibes.\n' +
    '- If blocked or unsure, `send_message` the lead instead of guessing.\n';
}
async function tmOpenMember(id) {
  const members = (TEAM.cfg && TEAM.cfg.members) || [];
  const m = id ? members.find((x) => x.id === id) : null;
  $('tm-mem-title').textContent = m ? 'Edit — ' + (m.name || m.id) : 'Add Agent';
  $('tm-mem-del').style.display = m ? '' : 'none';
  $('tm-mem-msg').textContent = '';
  const b = $('tm-mem-body'); b.innerHTML = '';
  const row = (label, node) => { const r = document.createElement('div'); r.className = 'pl-insp-row'; const l = document.createElement('label'); l.textContent = label; r.appendChild(l); r.appendChild(node); b.appendChild(r); };
  const idIn = document.createElement('input'); idIn.value = m ? m.id : ''; idIn.placeholder = 'e.g. ui-designer (letters/digits/-/_)'; idIn.disabled = !!m;
  // fills the form fields from a member-shaped object (template / import)
  let applyMember = null;
  if (!m) {
    const wrap = document.createElement('div'); wrap.style.cssText = 'display:flex;gap:6px;align-items:center';
    const tplSel = document.createElement('select'); tplSel.style.flex = '1';
    const d0 = document.createElement('option'); d0.value = ''; d0.textContent = '— start from a saved template —'; tplSel.appendChild(d0);
    const tpls = await api('/api/team/templates').then((r) => r.json()).catch(() => ({ templates: [] }));
    for (const t of tpls.templates || []) { const o = document.createElement('option'); o.value = t.name; o.textContent = t.name; tplSel.appendChild(o); }
    tplSel.onchange = () => {
      const t = (tpls.templates || []).find((x) => x.name === tplSel.value);
      if (t && t.member && applyMember) applyMember(t.member);
    };
    const delT = document.createElement('button'); delT.className = 'btn btn-sm'; delT.textContent = '✕'; delT.title = 'delete selected template';
    delT.onclick = async () => {
      if (!tplSel.value || !(await uiConfirm('Delete template "' + tplSel.value + '"?', { confirmText: 'Delete', danger: true }))) return;
      await api('/api/team/templates/delete', plBody({ b: { name: tplSel.value } }));
      tmOpenMember(null);
    };
    const imp = document.createElement('button'); imp.className = 'btn btn-sm'; imp.textContent = '📂 Import';
    imp.title = 'load a member JSON file';
    imp.onclick = () => {
      const f = document.createElement('input'); f.type = 'file'; f.accept = '.json,application/json';
      f.onchange = () => {
        const file = f.files && f.files[0]; if (!file) return;
        file.text().then((txt) => {
          try { const obj = JSON.parse(txt); if (applyMember) applyMember(obj.member || obj); }
          catch (e) { $('tm-mem-msg').textContent = 'bad JSON: ' + e.message; }
        });
      };
      f.click();
    };
    wrap.appendChild(tplSel); wrap.appendChild(delT); wrap.appendChild(imp);
    row('template', wrap);
  }
  row('id', idIn);
  const nameIn = document.createElement('input'); nameIn.value = m ? m.name : ''; nameIn.placeholder = 'display name';
  row('name', nameIn);
  const icoIn = document.createElement('input'); icoIn.value = m ? (m.icon || '') : '🤖'; icoIn.style.maxWidth = '70px';
  row('icon', icoIn);
  const stSel = document.createElement('select');
  stSel.title = 'Which lifecycle stage this member works:\nplan = runs once per goal, before the pipeline (PRD/design/requests)\nimplement = works requests in open/in_progress\nreview = works requests in review\nqa = works requests in qa';
  for (const st of ['plan', 'implement', 'review', 'qa']) { const o = document.createElement('option'); o.value = st; o.textContent = st; if (m && m.stage === st) o.selected = true; stSel.appendChild(o); }
  row('stage', stSel);
  const apprCb = document.createElement('input'); apprCb.type = 'checkbox'; apprCb.checked = !!(m && m.requires_approval);
  apprCb.style.cssText = 'width:auto;margin:0';
  const apprWrap = document.createElement('div');
  apprWrap.style.cssText = 'display:flex;gap:8px;align-items:center;font-size:12px;color:var(--text);cursor:pointer';
  apprWrap.title = 'When checked, each piece of work for this member parks until you click Approve on the Team page (a ticket lands in the human inbox)';
  const apprTxt = document.createElement('span'); apprTxt.textContent = 'require human approval before each piece of work';
  apprTxt.onclick = () => { apprCb.checked = !apprCb.checked; };
  apprWrap.appendChild(apprCb); apprWrap.appendChild(apprTxt);
  row('approval', apprWrap);
  const termCmdIn = document.createElement('input');
  termCmdIn.value = m ? (m.terminal_cmd || '') : '';
  termCmdIn.placeholder = 'claude --dangerously-skip-permissions (default)';
  termCmdIn.title = 'The CLI this member\'s resident tmux terminal runs (on its own subscription login — no provider API key). Leave empty for Claude Code with permission prompts off; claude-family commands get session-resume flags appended automatically.';
  row('terminal cmd', termCmdIn);
  const depWrap = document.createElement('div'); depWrap.style.cssText = 'display:flex;flex-wrap:wrap;gap:8px';
  depWrap.title = 'Upstream members (the canvas edges). For plan members this sets the planning ORDER; for workers it feeds topology context and notifications. Tip: drag the port on a card to connect visually.';
  for (const other of members) {
    if (m && other.id === m.id) continue;
    const l = document.createElement('label'); l.style.cssText = 'display:flex;gap:4px;align-items:center;font-size:11px';
    const cb = document.createElement('input'); cb.type = 'checkbox'; cb.value = other.id;
    if (m && (m.depends_on || []).includes(other.id)) cb.checked = true;
    l.appendChild(cb); l.appendChild(document.createTextNode(other.id)); depWrap.appendChild(l);
  }
  row('depends on', depWrap);
  const sopTa = document.createElement('textarea'); sopTa.value = m ? (m.role_prompt || '') : '';
  sopTa.placeholder = 'custom SOP (markdown) — leave empty to use the built-in SOP for this id/stage';
  sopTa.title = 'The standard operating procedure injected as this member prompt. Empty = built-in SOP for this id or its stage. Use Insert example SOP to start.';
  sopTa.style.minHeight = '130px';
  const sopWrap = document.createElement('div');
  const exBtn = document.createElement('button'); exBtn.className = 'btn btn-sm'; exBtn.textContent = '✨ Insert example SOP';
  exBtn.style.marginTop = '5px';
  exBtn.title = 'generate a starter SOP for the selected stage';
  exBtn.onclick = async () => {
    if (sopTa.value.trim() && !(await uiConfirm('Replace the current SOP with an example?', { confirmText: 'Replace' }))) return;
    sopTa.value = tmExampleSop(nameIn.value.trim() || idIn.value.trim() || 'Agent', stSel.value);
  };
  sopWrap.appendChild(sopTa); sopWrap.appendChild(exBtn);
  row('role prompt (SOP)', sopWrap);
  applyMember = (t) => {
    if (!idIn.disabled && t.id) idIn.value = t.id;
    if (t.name) nameIn.value = t.name;
    if (t.icon) icoIn.value = t.icon;
    if (t.stage) stSel.value = t.stage;
    if (typeof t.role_prompt === 'string') sopTa.value = t.role_prompt;
    if (typeof t.terminal_cmd === 'string') termCmdIn.value = t.terminal_cmd;
  };
  if (m) {
    const wrap = document.createElement('div'); wrap.style.cssText = 'display:flex;gap:6px';
    const saveT = document.createElement('button'); saveT.className = 'btn btn-sm'; saveT.textContent = '💾 Save as template';
    saveT.onclick = async () => {
      const name = await uiPrompt('Template name:', { value: m.name || m.id }); if (!name) return;
      const member = { id: m.id, name: nameIn.value.trim() || m.id, icon: icoIn.value.trim(), stage: stSel.value, role_prompt: sopTa.value, terminal_cmd: termCmdIn.value.trim(), depends_on: [] };
      const r = await api('/api/team/templates', plBody({ b: { name, member } }));
      $('tm-mem-msg').textContent = r.ok ? '✓ template saved' : 'save failed';
    };
    const expT = document.createElement('button'); expT.className = 'btn btn-sm'; expT.textContent = '📤 Export JSON';
    expT.onclick = () => {
      const member = { id: m.id, name: nameIn.value.trim() || m.id, icon: icoIn.value.trim(), stage: stSel.value, role_prompt: sopTa.value, terminal_cmd: termCmdIn.value.trim(), depends_on: [] };
      const a = document.createElement('a');
      a.href = URL.createObjectURL(new Blob([JSON.stringify({ member }, null, 2)], { type: 'application/json' }));
      a.download = m.id + '.json'; a.click(); URL.revokeObjectURL(a.href);
    };
    wrap.appendChild(saveT); wrap.appendChild(expT);
    row('template', wrap);
  }
  $('tm-mem-save').onclick = async () => {
    const nid = (m ? m.id : idIn.value).trim();
    if (!nid) { $('tm-mem-msg').textContent = 'id required'; return; }
    const nm = { id: nid, name: nameIn.value.trim() || nid, icon: icoIn.value.trim(), stage: stSel.value, role_prompt: sopTa.value, requires_approval: apprCb.checked, terminal_cmd: termCmdIn.value.trim(), depends_on: [...depWrap.querySelectorAll('input:checked')].map((c) => c.value) };
    const list = (TEAM.cfg.members || []).slice();
    const at = list.findIndex((x) => x.id === nid);
    if (m) { list[at] = nm; } else { if (at !== -1) { $('tm-mem-msg').textContent = 'id already exists'; return; } list.push(nm); }
    const prev = TEAM.cfg.members; TEAM.cfg.members = list;
    if (await tmSaveConfig()) { $('tm-memmodal').classList.remove('open'); loadTeam(); }
    else { TEAM.cfg.members = prev; $('tm-mem-msg').textContent = 'rejected — see status bar'; }
  };
  $('tm-mem-del').onclick = async () => {
    if (!m || !(await uiConfirm('Remove agent "' + m.id + '" from the team?', { confirmText: 'Remove', danger: true }))) return;
    const prev = TEAM.cfg.members;
    TEAM.cfg.members = (TEAM.cfg.members || []).filter((x) => x.id !== m.id)
      .map((x) => Object.assign({}, x, { depends_on: (x.depends_on || []).filter((d) => d !== m.id) }));
    if (TEAM.cfg.positions) delete TEAM.cfg.positions[m.id];
    if (await tmSaveConfig()) { $('tm-memmodal').classList.remove('open'); loadTeam(); }
    else { TEAM.cfg.members = prev; $('tm-mem-msg').textContent = 'rejected — see status bar'; }
  };
  $('tm-memmodal').classList.add('open');
}
function tmShowInbox(m, msgs) {
  $('tm-req-title').textContent = '📨 ' + (m.name || m.id) + ' — inbox';
  const b = $('tm-req-body'); b.innerHTML = '';
  const sorted = msgs.slice().sort((a, x) => (a.created_at < x.created_at ? 1 : -1));
  for (const msg of sorted.slice(0, 30)) {
    const r = document.createElement('div'); r.className = 'pl-insp-row';
    const l = document.createElement('label');
    const ts = new Date(msg.created_at || msg.at || 0);
    l.textContent = (msg.category === 'ticket' ? '🎫 ' : '🔔 ') + (msg.from || '?') + ' · ' + (isNaN(ts) ? (msg.created_at || '') : ts.toLocaleString()) + (msg.read ? '' : ' · unread');
    const c = document.createElement('div'); c.style.cssText = 'font-size:12px;white-space:pre-wrap;line-height:1.5'; c.textContent = msg.body || '';
    r.appendChild(l); r.appendChild(c); b.appendChild(r);
  }
  if (!sorted.length) b.innerHTML = '<span class="sl">(empty)</span>';
  $('tm-reqmodal').classList.add('open');
}
async function tmShowSession(m) {
  $('tm-req-title').textContent = '📜 ' + (m.name || m.id) + ' — resident session';
  const b = $('tm-req-body'); b.innerHTML = '<span class="sl">…</span>';
  const fetchLog = async () => {
    const d = await api('/api/team/session-log?project=' + encodeURIComponent(TEAM.project) + '&member=' + encodeURIComponent(m.id)).then((r) => r.json()).catch(() => ({ log: '' }));
    return (d && d.log) || '(no session log yet)';
  };
  const head = document.createElement('div'); head.style.cssText = 'display:flex;gap:8px;align-items:center;margin-bottom:6px';
  head.innerHTML = '<span class="sl" style="flex:1">this member keeps one persistent conversation across tasks — resuming its own memory each run</span>';
  const reset = document.createElement('button'); reset.className = 'btn btn-sm'; reset.textContent = '↻ Reset session';
  reset.title = 'Forget this member\'s conversation — next run starts with fresh context';
  reset.onclick = async () => {
    if (!(await uiConfirm('Reset ' + m.id + '\'s session? It loses its accumulated memory.', { confirmText: 'Reset' }))) return;
    await api('/api/team/reset-session', plBody({ b: { project: TEAM.project, member: m.id } }));
    pre.textContent = '(session reset — cleared)';
  };
  head.appendChild(reset);
  const pre = document.createElement('pre'); pre.style.cssText = 'font-size:10.5px;white-space:pre-wrap;max-height:60vh;overflow:auto;background:var(--bg-2,rgba(0,0,0,.05));padding:8px;border-radius:6px;margin:0';
  pre.textContent = await fetchLog();
  b.innerHTML = ''; b.appendChild(head); b.appendChild(pre);
  $('tm-reqmodal').classList.add('open');
}
$('tm-req-close').onclick = () => $('tm-reqmodal').classList.remove('open');
// ── Watches: monitor files/git/commands, route changes to board or inbox ──
async function tmOpenWatches() {
  if (!TEAM.project) { alert('pick a project first'); return; }
  let watches = await api('/api/team/watches?project=' + encodeURIComponent(TEAM.project)).then((r) => r.json()).catch(() => []);
  if (!Array.isArray(watches)) watches = [];
  $('tm-req-title').textContent = '👁 Watches — ' + TEAM.project.split('/').pop();
  const b = $('tm-req-body'); b.innerHTML = '';
  const info = document.createElement('div'); info.className = 'sl'; info.style.marginBottom = '8px';
  info.textContent = 'Evaluated by the running orchestrator on its poll loop. First look sets a baseline; changes then create a board request (the team handles it) or ticket the human inbox.';
  b.appendChild(info);
  const list = document.createElement('div'); b.appendChild(list);
  const msg = document.createElement('span'); msg.className = 'sl';
  const rowFor = (w) => {
    const r = document.createElement('div');
    r.style.cssText = 'display:flex;flex-wrap:wrap;gap:6px;align-items:center;border:1px solid var(--border);border-radius:8px;padding:8px;margin-bottom:6px;font-size:12px';
    const en = document.createElement('input'); en.type = 'checkbox'; en.checked = w.enabled !== false; en.title = 'enabled'; en.style.cssText = 'width:auto;margin:0';
    const id = document.createElement('input'); id.value = w.id || ''; id.placeholder = 'id'; id.style.maxWidth = '110px';
    const kind = document.createElement('select');
    for (const k of ['files', 'git', 'command']) { const o = document.createElement('option'); o.value = k; o.textContent = k; if ((w.kind || 'files') === k) o.selected = true; kind.appendChild(o); }
    const target = document.createElement('input'); target.value = w.target || ''; target.style.flex = '1'; target.style.minWidth = '140px';
    const pattern = document.createElement('input'); pattern.value = w.pattern || ''; pattern.placeholder = '*.rs / substring'; pattern.style.maxWidth = '110px';
    const action = document.createElement('select'); action.title = 'request = auto-create a board request; alert = ticket the human inbox';
    for (const a of ['request', 'alert']) { const o = document.createElement('option'); o.value = a; o.textContent = a === 'request' ? '→ board request' : '→ human alert'; if ((w.action || 'request') === a) o.selected = true; action.appendChild(o); }
    const iv = document.createElement('input'); iv.type = 'number'; iv.min = '1'; iv.value = w.interval_secs || 30; iv.title = 'check interval (seconds)'; iv.style.maxWidth = '70px';
    const brief = document.createElement('input'); brief.value = w.brief || ''; brief.placeholder = 'brief: what should the team DO about a change?'; brief.style.cssText = 'flex-basis:100%;min-width:0';
    const del = document.createElement('button'); del.className = 'btn btn-sm'; del.textContent = '🗑'; del.onclick = () => r.remove();
    const syncPh = () => {
      target.placeholder = kind.value === 'files' ? 'path under project (e.g. src)' : kind.value === 'command' ? 'shell command (e.g. npm test 2>&1 | tail -5)' : '(tracks HEAD + working tree)';
      target.disabled = kind.value === 'git';
      pattern.style.display = kind.value === 'files' ? '' : 'none';
    };
    kind.onchange = syncPh; syncPh();
    r.append(en, id, kind, target, pattern, action, iv, del, brief);
    r._get = () => ({ id: id.value.trim(), kind: kind.value, target: target.value.trim(), pattern: pattern.value.trim(), action: action.value, brief: brief.value.trim(), interval_secs: Math.max(1, parseInt(iv.value, 10) || 30), enabled: en.checked });
    return r;
  };
  watches.forEach((w) => list.appendChild(rowFor(w)));
  const ctl = document.createElement('div'); ctl.style.cssText = 'display:flex;gap:8px;align-items:center;margin-top:8px';
  const add = document.createElement('button'); add.className = 'btn btn-sm'; add.textContent = '+ Add watch';
  add.onclick = () => list.appendChild(rowFor({ id: 'watch-' + (list.children.length + 1), kind: 'files', interval_secs: 30, enabled: true }));
  const save = document.createElement('button'); save.className = 'btn btn-primary btn-sm'; save.textContent = '💾 Save';
  save.onclick = async () => {
    const ws = [...list.children].map((r) => r._get());
    const resp = await api('/api/team/watches', { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ project: TEAM.project, watches: ws }) });
    if (resp.ok) { msg.textContent = '✓ saved'; setTimeout(() => { msg.textContent = ''; }, 1500); }
    else { msg.textContent = (await resp.json().catch(() => ({}))).error || 'save failed'; }
  };
  ctl.append(add, save, msg); b.appendChild(ctl);
  $('tm-reqmodal').classList.add('open');
}
$('tm-watches').onclick = tmOpenWatches;
// ── One-click: whole team ↔ resident terminals ──
// Mode note under the tabs: teams are terminal-only — every agent is a
// resident tmux terminal on the CLI's own subscription login.
function tmSyncAllTermBtn() {
  const ms = (TEAM.cfg && TEAM.cfg.members) || [];
  $('tm-mode-note').textContent = ms.length
    ? '⌨ all agents run in resident terminals — Claude Code on your subscription, no provider API key'
    : '';
}
// ── Live member terminal (xterm.js over WebSocket → tmux attach) ──
let xtermReady = null;
function loadXterm() {
  if (xtermReady) return xtermReady;
  xtermReady = new Promise((resolve, reject) => {
    const css = document.createElement('link'); css.rel = 'stylesheet'; css.href = '/vendor/xterm.css';
    document.head.appendChild(css);
    const s1 = document.createElement('script'); s1.src = '/vendor/xterm.js';
    s1.onload = () => {
      const s2 = document.createElement('script'); s2.src = '/vendor/xterm-addon-fit.js';
      s2.onload = resolve; s2.onerror = reject;
      document.head.appendChild(s2);
    };
    s1.onerror = reject;
    document.head.appendChild(s1);
  });
  return xtermReady;
}
// Wire an xterm + `/ws/terminal` bridge into `body` for one tmux session.
// Returns a handle so callers (the modal + the dock) manage lifecycle. The
// token rides in the subprotocol list, not the URL: a WebSocket URL shows up in
// devtools/proxy logs, and this one grants a shell. The server checks it and
// selects 'forge-terminal', never echoing it back.
function tmAttachTerm(body, sessionName) {
  const term = new window.Terminal({ fontSize: 13, cursorBlink: true, theme: { background: '#0d1117' } });
  const fit = new window.FitAddon.FitAddon();
  term.loadAddon(fit); term.open(body); try { fit.fit(); } catch {}
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  const ws = new WebSocket(
    proto + '://' + location.host + '/ws/terminal?session=' + encodeURIComponent(sessionName),
    ['forge-terminal', TOKEN]
  );
  ws.binaryType = 'arraybuffer';
  const sendResize = () => { if (ws.readyState === 1) ws.send(JSON.stringify({ t: 'r', c: term.cols, r: term.rows })); };
  ws.onopen = sendResize;
  ws.onmessage = (ev) => {
    if (typeof ev.data === 'string') {
      // \x01-prefixed text frame = human-readable error from the server
      term.write('\r\n\x1b[31m' + ev.data.replace(/^\x01/, '') + '\x1b[0m\r\n');
      return;
    }
    term.write(new Uint8Array(ev.data));
  };
  ws.onclose = () => term.write('\r\n\x1b[90m[disconnected]\x1b[0m\r\n');
  term.onData((d) => { if (ws.readyState === 1) ws.send(JSON.stringify({ t: 'i', d })); });
  const ro = new ResizeObserver(() => { try { fit.fit(); sendResize(); } catch {} });
  ro.observe(body);
  return {
    fit: () => { try { fit.fit(); sendResize(); } catch {} },
    focus: () => { try { term.focus(); } catch {} },
    dispose: () => { ro.disconnect(); try { ws.close(); } catch {} try { term.dispose(); } catch {} },
  };
}
async function tmOpenTerminal(sessionName, title) {
  try { await loadXterm(); } catch { alert('failed to load terminal assets'); return; }
  const ov = document.createElement('div');
  ov.style.cssText = 'position:fixed;inset:0;z-index:300;background:rgba(0,0,0,.55);display:flex;align-items:center;justify-content:center';
  const box = document.createElement('div');
  box.style.cssText = 'width:min(1000px,94vw);height:min(640px,86vh);background:#0d1117;border-radius:10px;display:flex;flex-direction:column;overflow:hidden;box-shadow:0 12px 40px rgba(0,0,0,.5)';
  const hd = document.createElement('div');
  hd.style.cssText = 'display:flex;gap:10px;align-items:center;padding:8px 12px;background:#161b22;color:#e6edf3;font-size:12px';
  hd.innerHTML = '<span>⌨ ' + escapeHtml(title) + ' — live member terminal</span><span style="opacity:.55">keystrokes go straight to the agent\'s session; closing detaches (the member keeps running)</span>';
  const cp = document.createElement('button'); cp.className = 'btn btn-sm'; cp.textContent = '📋 attach cmd'; cp.style.marginLeft = 'auto';
  cp.title = 'copy: tmux attach -t ' + sessionName;
  cp.onclick = () => { navigator.clipboard && navigator.clipboard.writeText('tmux attach -t ' + sessionName); cp.textContent = '✓'; setTimeout(() => { cp.textContent = '📋 attach cmd'; }, 1200); };
  const cl = document.createElement('button'); cl.className = 'btn btn-sm'; cl.textContent = '✕';
  hd.appendChild(cp); hd.appendChild(cl);
  const body = document.createElement('div'); body.style.cssText = 'flex:1;min-height:0;padding:4px';
  box.appendChild(hd); box.appendChild(body); ov.appendChild(box); document.body.appendChild(ov);
  const h = tmAttachTerm(body, sessionName); h.focus();
  const close = () => { h.dispose(); ov.remove(); };
  cl.onclick = close;
  ov.onclick = (e) => { if (e.target === ov) close(); };
}

// ── Live terminal dock: one sub-tab per agent's resident session ─────────────
// Reuses the same `/ws/terminal` bridge as the modal, but keeps each agent's
// terminal on a persistent tab so you can watch the whole team at once. A tab's
// xterm/socket is created lazily on first view and kept alive until the session
// disappears (member torn down) or the team overlay closes.
function tmLiveSessions() {
  const st = TEAM.status || {};
  const members = (TEAM.cfg && TEAM.cfg.members) || [];
  const out = [];
  for (const m of members) {
    const ms = st[m.id];
    if (ms && ms.terminal) out.push({ id: m.id, name: m.name || m.id, icon: m.icon || '', session: ms.terminal, working: ms.status === 'working' });
  }
  return out;
}
function tmRenderTermDock() {
  const live = tmLiveSessions();
  const badge = $('tm-term-badge'); if (badge) badge.textContent = live.length ? '(' + live.length + ')' : '';
  const empty = $('tm-term-empty'); if (empty) empty.style.display = live.length ? 'none' : '';
  const tabs = $('tm-term-tabs'), panes = $('tm-term-panes'); if (!tabs || !panes) return;
  const dock = TEAM.termDock || (TEAM.termDock = { panes: {}, active: null });
  const liveSet = new Set(live.map((l) => l.session));
  // Drop tabs/panes for sessions that are gone (member torn down / reaped).
  for (const s of Object.keys(dock.panes)) {
    if (!liveSet.has(s)) {
      const p = dock.panes[s]; if (p.handle) p.handle.dispose(); p.tab.remove(); p.pane.remove();
      delete dock.panes[s]; if (dock.active === s) dock.active = null;
    }
  }
  for (const l of live) {
    const existing = dock.panes[l.session];
    if (existing) { const dot = existing.tab.querySelector('.tm-term-dot'); if (dot) dot.style.background = l.working ? '#2e9e44' : 'var(--muted)'; continue; }
    const tab = document.createElement('button'); tab.className = 'btn btn-sm';
    tab.innerHTML = '<span class="tm-term-dot" style="display:inline-block;width:7px;height:7px;border-radius:50%;margin-right:5px;background:' + (l.working ? '#2e9e44' : 'var(--muted)') + '"></span>' + escapeHtml((l.icon ? l.icon + ' ' : '') + l.name);
    tab.title = 'tmux attach -t ' + l.session;
    tab.onclick = () => tmTermSelect(l.session);
    tabs.appendChild(tab);
    const pane = document.createElement('div'); pane.style.cssText = 'position:absolute;inset:0;display:none;padding:4px';
    panes.appendChild(pane);
    dock.panes[l.session] = { tab, pane, handle: null };
  }
  if (TEAM.tab === 'term' && (!dock.active || !dock.panes[dock.active]) && live[0]) {
    tmTermSelect(live[0].session);
  }
}
async function tmTermSelect(session) {
  const dock = TEAM.termDock; if (!dock || !dock.panes[session]) return;
  dock.active = session;
  try { await loadXterm(); } catch { return; }
  for (const s of Object.keys(dock.panes)) {
    const p = dock.panes[s]; const on = s === session;
    p.pane.style.display = on ? '' : 'none';
    p.tab.className = 'btn btn-sm' + (on ? ' btn-primary' : '');
    if (on) {
      if (!p.handle) p.handle = tmAttachTerm(p.pane, s);
      requestAnimationFrame(() => { p.handle.fit(); p.handle.focus(); });
    }
  }
}
function tmDisposeTermDock() {
  const dock = TEAM.termDock; if (!dock) return;
  for (const s of Object.keys(dock.panes)) { const p = dock.panes[s]; if (p.handle) p.handle.dispose(); p.tab.remove(); p.pane.remove(); }
  TEAM.termDock = { panes: {}, active: null };
}
async function tmShowRequest(id) {
  const d = await (await api('/api/team/request?project=' + encodeURIComponent(TEAM.project) + '&id=' + encodeURIComponent(id))).json().catch(() => null);
  if (!d || !d.request) return;
  const req = d.request, res = d.response;
  $('tm-req-title').textContent = req.title || id;
  const b = $('tm-req-body'); b.innerHTML = '';
  const sec = (label, html) => { const s = document.createElement('div'); s.className = 'pl-insp-row'; const l = document.createElement('label'); l.textContent = label; s.appendChild(l); const c = document.createElement('div'); c.style.cssText = 'font-size:12px;white-space:pre-wrap;line-height:1.5'; c.innerHTML = html; s.appendChild(c); b.appendChild(s); };
  sec('status', '<b>' + escapeHtml(req.status || '') + '</b>' + (req.claimed_by ? ' · @' + escapeHtml(req.claimed_by) : ''));
  if (req.description) sec('description', escapeHtml(req.description));
  if ((req.acceptance_criteria || []).length) sec('acceptance criteria', '<ul style="margin:0;padding-left:18px">' + req.acceptance_criteria.map((c) => '<li>' + escapeHtml(c) + '</li>').join('') + '</ul>');
  if (res) {
    if (res.engineer) {
      sec('🔨 engineer', escapeHtml(res.engineer.notes || '') + ((res.engineer.files_changed || []).length ? '<div class="sl" style="margin-top:3px">files: ' + res.engineer.files_changed.map(escapeHtml).join(', ') + '</div>' : ''));
      if ((res.engineer.files_changed || []).length) {
        const dr = document.createElement('div'); dr.className = 'pl-insp-row';
        const btn = document.createElement('button'); btn.className = 'btn btn-sm'; btn.textContent = '📄 View code changes';
        btn.onclick = async () => {
          btn.textContent = '…';
          const dd = await api('/api/team/diff?project=' + encodeURIComponent(TEAM.project) + '&files=' + encodeURIComponent(res.engineer.files_changed.join(','))).then((r) => r.json()).catch(() => ({ diff: '' }));
          const pre = document.createElement('pre');
          pre.style.cssText = 'font-size:10.5px;white-space:pre-wrap;max-height:340px;overflow:auto;background:var(--bg-2,rgba(0,0,0,.05));padding:8px;border-radius:6px';
          pre.textContent = dd.diff || '(no diff found — the change may be in older commits)';
          btn.replaceWith(pre);
        };
        dr.appendChild(btn); b.appendChild(dr);
      }
    }
    if (res.review) sec('🔍 review — ' + escapeHtml(res.review.result || ''), (res.review.findings || []).length ? '<ul style="margin:0;padding-left:18px">' + res.review.findings.map((f) => '<li>[' + escapeHtml(f.severity || '') + '] ' + escapeHtml(f.file || '') + ' — ' + escapeHtml(f.description || '') + '</li>').join('') + '</ul>' : '(no findings)');
    if (res.qa) sec('✅ qa — ' + escapeHtml(res.qa.result || ''), escapeHtml(res.qa.notes || ''));
  }
  $('tm-reqmodal').classList.add('open');
}
function tmEdges() {
  const svg = $('tm-edges'); svg.innerHTML = '';
  const dims = (k) => { const el = document.getElementById('tm-' + k); if (!el) return null; return { x: el.offsetLeft, y: el.offsetTop, w: el.offsetWidth, h: el.offsetHeight }; };
  const path = (d, cls) => { const p = document.createElementNS('http://www.w3.org/2000/svg', 'path'); p.setAttribute('d', d); if (cls) p.setAttribute('class', cls); svg.appendChild(p); };
  const flow = (fromId, toId, cls) => {
    const a = dims(fromId), b = dims(toId); if (!a || !b) return;
    const ax = a.x + a.w, ay = a.y + a.h / 2, bx = b.x, by = b.y + b.h / 2, mx = (ax + bx) / 2;
    path('M' + ax + ',' + ay + ' C' + mx + ',' + ay + ' ' + mx + ',' + by + ' ' + bx + ',' + by, cls);
  };
  const members = (TEAM.cfg && TEAM.cfg.members) || [];
  // solid edges = depends_on (dep → member), plus every qa member → done.
  // Each depends_on edge gets an invisible wide twin that deletes on click.
  const clickable = (fromId, toId) => {
    const a = dims(fromId), b = dims(toId); if (!a || !b) return;
    const ax = a.x + a.w, ay = a.y + a.h / 2, bx = b.x, by = b.y + b.h / 2, mx = (ax + bx) / 2;
    const h = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    h.setAttribute('d', 'M' + ax + ',' + ay + ' C' + mx + ',' + ay + ' ' + mx + ',' + by + ' ' + bx + ',' + by);
    h.setAttribute('class', 'hit'); h.setAttribute('stroke', 'transparent'); h.setAttribute('stroke-width', '12'); h.setAttribute('fill', 'none');
    const t = document.createElementNS('http://www.w3.org/2000/svg', 'title');
    t.textContent = fromId + ' → ' + toId + ' (click to remove)'; h.appendChild(t);
    h.onclick = async () => {
      if (!(await uiConfirm('Remove connection ' + fromId + ' → ' + toId + '?', { confirmText: 'Remove' }))) return;
      const m = (TEAM.cfg.members || []).find((x) => x.id === toId); if (!m) return;
      m.depends_on = (m.depends_on || []).filter((d) => d !== fromId);
      if (await tmSaveConfig()) loadTeam();
    };
    svg.appendChild(h);
  };
  for (const m of members) for (const dep of (m.depends_on || [])) { flow(dep, m.id); clickable(dep, m.id); }
  for (const m of members) if (m.stage === 'qa') flow(m.id, 'done');
  // rework back-edges (dashed): review/qa members hand work back to implementers
  const impl = members.filter((m) => m.stage === 'implement');
  if (impl.length) {
    const eng = dims(impl[0].id);
    for (const m of members) {
      if (m.stage !== 'review' && m.stage !== 'qa') continue;
      const f = dims(m.id); if (!f || !eng) continue;
      const ax = f.x + f.w / 2, ay = f.y + f.h, bx = eng.x + eng.w / 2, by = eng.y + eng.h, dy = Math.max(ay, by) + 42;
      path('M' + ax + ',' + ay + ' C' + ax + ',' + dy + ' ' + bx + ',' + dy + ' ' + bx + ',' + by, 'rework');
    }
  }
}

// Renders one framed stats card per connected platform, each with a small
// row of KPI tiles fetched from its board endpoint.
async function openKanban() {
  $('kanban-overlay').classList.add('open');
  const board = $('kanban-board'); board.innerHTML = '<div class="kb-loading">Loading…</div>';
  const avail = await (await api('/api/board/platforms')).json().catch(() => ({}));
  const connected = KB_PLATFORMS.filter((p) => avail[p.key]);
  if (!connected.length) { board.innerHTML = '<div class="kb-empty">No boards yet — connect a platform on the 🧩 Integrations page.</div>'; return; }
  board.innerHTML = '';
  for (const p of connected) {
    const card = document.createElement('div'); card.className = 'stat-card'; card.id = 'sc-' + p.key;
    card.innerHTML = '<div class="sc-head">' + p.logo + '<span class="sc-name">' + p.name + '</span><span class="sc-go">↗</span></div><div class="sc-sub"></div><div class="sc-tiles"><span class="sl">Loading…</span></div>';
    board.appendChild(card);
    loadStats(p);
  }
}
// GitHub card: turn the "owner/repo" subtitle into a picker so the user can
// pin which repo the GitHub + Actions boards report on. Reloads both cards.
function makeRepoPicker(card, current) {
  const sub = card.querySelector('.sc-sub'); sub.textContent = '';
  const sel = document.createElement('select'); sel.className = 'repo-select';
  const add = (val, label, selected) => { const o = document.createElement('option'); o.value = val; o.textContent = label; if (selected) o.selected = true; sel.appendChild(o); };
  add(current, current, true);
  add('__auto__', '⟳ auto-detect from git', false);
  sub.appendChild(sel);
  sel.onmousedown = (e) => e.stopPropagation(); // don't trigger the card's open-link
  sel.onchange = async () => {
    const v = sel.value === '__auto__' ? '' : sel.value;
    sel.disabled = true;
    await api('/api/github/repo', { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ repo: v }) });
    loadStats({ key: 'github' });
    const gha = KB_PLATFORMS.find((x) => x.key === 'gha'); if (gha && $('sc-' + gha.key)) loadStats(gha);
  };
  // Eagerly load the repo list so the first open already shows every option.
  api('/api/github/repos').then((r) => r.json()).then((d) => {
    for (const r of (d.repos || [])) { if (r !== current) add(r, r, false); }
  }).catch(() => {});
}
// Jira card: pick which project the board scopes to (or all projects).
function makeJiraPicker(card, current) {
  const sub = card.querySelector('.sc-sub'); sub.textContent = '';
  const sel = document.createElement('select'); sel.className = 'repo-select';
  const add = (val, label, selected) => { const o = document.createElement('option'); o.value = val; o.textContent = label; if (selected) o.selected = true; sel.appendChild(o); };
  add('__all__', 'All projects', !current);
  if (current) add(current, current, true);
  sub.appendChild(sel);
  const suf = document.createElement('span'); suf.textContent = ' · last 30d'; sub.appendChild(suf);
  sel.onmousedown = (e) => e.stopPropagation();
  sel.onchange = async () => {
    const v = sel.value === '__all__' ? '' : sel.value;
    sel.disabled = true;
    await api('/api/jira/project', { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ project: v }) });
    loadStats({ key: 'jira' });
  };
  api('/api/jira/projects').then((r) => r.json()).then((d) => {
    for (const pr of (d.projects || [])) { if (pr.key !== current) add(pr.key, pr.key + ' — ' + pr.name, false); }
  }).catch(() => {});
}
async function loadStats(p) {
  const card = $('sc-' + p.key); if (!card) return;
  const tiles = card.querySelector('.sc-tiles');
  let d;
  try { d = await (await api('/api/board/' + p.key)).json(); } catch (e) { tiles.innerHTML = '<span class="sl">failed to load</span>'; return; }
  if (d.error) { tiles.innerHTML = '<span class="sl">' + escapeHtml(String(d.error).slice(0, 120)) + '</span>'; return; }
  card.querySelector('.sc-sub').textContent = d.subtitle || '';
  if (d.url) { const h = card.querySelector('.sc-head'); h.setAttribute('role', 'link'); h.onclick = () => window.open(d.url, '_blank', 'noopener'); }
  if (p.key === 'github' && d.repo) makeRepoPicker(card, d.repo);
  if (p.key === 'jira') makeJiraPicker(card, d.project || '');
  tiles.innerHTML = '';
  for (const st of (d.stats || [])) {
    const t = document.createElement(st.url ? 'a' : 'div'); t.className = 'stat-tile';
    if (st.url) { t.href = st.url; t.target = '_blank'; t.rel = 'noopener'; }
    t.innerHTML = '<div class="sv">' + escapeHtml(String(st.value != null ? st.value : 0)) + escapeHtml(st.suffix || '') + '</div><div class="sl">' + escapeHtml(st.label || '') + '</div>';
    tiles.appendChild(t);
  }
}

loadConversations();
// Lock in the full-capability agent, then load its models.
ensureDefaultAgent().then(loadModels);
loadCommandsSkills();
inputEl.focus();

// Deep-link a panel via the URL hash (#pipeline / #team / #schedules /
// #usage) — shareable links, and lets headless capture render a specific
// overlay directly.
function openPanelFromHash() {
  const map = { pipeline: 'pipeline-open', team: 'team-open', schedules: 'sched-open', usage: 'usage-open' };
  const id = map[(location.hash || '').replace('#', '')];
  if (id && $(id)) $(id).click();
}
window.addEventListener('hashchange', openPanelFromHash);
if (location.hash) setTimeout(openPanelFromHash, 300);
