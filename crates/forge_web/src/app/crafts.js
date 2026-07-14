/* ---------- crafts (project mini-apps) ---------- */
const CRAFT = { project: null, cur: null, poll: null };
$('craft-open').onclick = () => { $('craft-overlay').classList.add('open'); craftLoadProjects(); };
$('craft-close').onclick = () => { if (CRAFT.poll) clearInterval(CRAFT.poll); $('craft-overlay').classList.remove('open'); };
$('craft-refresh').onclick = () => craftLoadList();
async function craftLoadProjects() {
  const d = await api('/api/pipeline/projects').then((r) => r.json()).catch(() => ({ projects: [] }));
  const sel = $('craft-project'); sel.innerHTML = '';
  for (const p of d.projects || []) { const o = document.createElement('option'); o.value = p.name; o.textContent = p.name; sel.appendChild(o); }
  if ((d.projects || []).length) { CRAFT.project = CRAFT.project || d.projects[0].name; sel.value = CRAFT.project; craftLoadList(); }
  else { $('craft-list').innerHTML = '<span class="sl">no projects — add one on the Team page</span>'; }
}
$('craft-project').onchange = () => { CRAFT.project = $('craft-project').value; CRAFT.cur = null; craftLoadList(); };
async function craftLoadList() {
  if (!CRAFT.project) return;
  const d = await api('/api/crafts?project=' + encodeURIComponent(CRAFT.project)).then((r) => r.json()).catch(() => ({ crafts: [], building: [] }));
  const box = $('craft-list'); box.innerHTML = '';
  const building = new Set(d.building || []);
  if (!(d.crafts || []).length && !building.size) box.innerHTML = '<span class="sl">no crafts yet — "+ Craft"</span>';
  for (const b of building) if (!(d.crafts || []).includes(b)) {
    const el = document.createElement('div'); el.className = 'sl'; el.textContent = '⏳ ' + b + ' (building…)'; box.appendChild(el);
  }
  for (const name of d.crafts || []) {
    const el = document.createElement('div'); el.style.cssText = 'cursor:pointer;padding:5px 7px;border-radius:6px;font-size:13px' + (name === CRAFT.cur ? ';background:var(--bg-2,rgba(94,106,210,.12))' : '');
    el.textContent = (building.has(name) ? '⏳ ' : '🎨 ') + name;
    el.onclick = () => craftOpen(name);
    box.appendChild(el);
  }
  // poll while anything is building
  if (building.size) { if (!CRAFT.poll) CRAFT.poll = setInterval(craftLoadList, 3000); }
  else if (CRAFT.poll) { clearInterval(CRAFT.poll); CRAFT.poll = null; if (CRAFT.cur) craftOpen(CRAFT.cur); }
}
async function craftOpen(name) {
  CRAFT.cur = name; craftLoadList();
  $('craft-empty').style.display = 'none';
  $('craft-toolbar').style.display = 'flex';
  $('craft-cur-name').textContent = '🎨 ' + name;
  // Load the craft as its own document rather than srcdoc: an srcdoc frame
  // inherits this page's CSP, under which the craft's inline script would
  // be refused. /craft/view serves it with its own policy (inline script
  // allowed, no network). The cache-buster makes a refined craft reload.
  $('craft-frame').src = '/craft/view?project=' + encodeURIComponent(CRAFT.project)
    + '&name=' + encodeURIComponent(name) + '&v=' + Date.now();
}
$('craft-new').onclick = async () => {
  if (!CRAFT.project) return;
  const name = await uiPrompt('Craft name (short, e.g. "endpoints"):'); if (!name) return;
  const desc = await uiPrompt('Describe what this mini-app should do:'); if (!desc) return;
  const r = await api('/api/craft/generate', plBody({ b: { project: CRAFT.project, name: name.trim(), prompt: desc } }));
  if (!r.ok) { const e = await r.json().catch(() => ({})); await uiAlert(e.error || 'failed'); return; }
  CRAFT.cur = name.trim(); craftLoadList();
};
$('craft-refine').onclick = async () => {
  if (!CRAFT.cur) return;
  const desc = await uiPrompt('What should change?'); if (!desc) return;
  await api('/api/craft/generate', plBody({ b: { project: CRAFT.project, name: CRAFT.cur, prompt: desc, refine: true } }));
  craftLoadList();
};
$('craft-del').onclick = async () => {
  if (!CRAFT.cur || !(await uiConfirm('Delete craft "' + CRAFT.cur + '"?', { confirmText: 'Delete', danger: true }))) return;
  await api('/api/craft/delete', plBody({ b: { project: CRAFT.project, name: CRAFT.cur } }));
  CRAFT.cur = null; $('craft-toolbar').style.display = 'none'; $('craft-frame').src = 'about:blank'; $('craft-empty').style.display = ''; craftLoadList();
};
