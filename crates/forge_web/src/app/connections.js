/* ---------- connections drawer (providers + MCP) ---------- */
function openSettings() { $('drawer-overlay').classList.add('open'); loadProviders(); }
function closeSettings() { $('drawer-overlay').classList.remove('open'); }
$('settings').onclick = openSettings;
$('drawer-close').onclick = closeSettings;
$('drawer-overlay').addEventListener('mousedown', (e) => { if (e.target === $('drawer-overlay')) closeSettings(); });

function connRow({ name, sub, badge, actions }) {
  const row = document.createElement('div'); row.className = 'conn';
  const info = document.createElement('div'); info.className = 'info';
  const nm = document.createElement('div'); nm.className = 'nm'; nm.textContent = name;
  info.appendChild(nm);
  if (sub) { const s = document.createElement('div'); s.className = 'sub'; s.textContent = sub; s.title = sub; info.appendChild(s); }
  row.appendChild(info);
  if (badge) { const b = document.createElement('span'); b.className = 'badge ' + badge.cls; b.textContent = badge.text; row.appendChild(b); }
  const act = document.createElement('div'); act.className = 'actions';
  for (const a of actions) {
    const btn = document.createElement('button'); btn.className = 'btn btn-sm ' + (a.cls || 'btn-ghost');
    btn.textContent = a.label; btn.onclick = a.onClick; act.appendChild(btn);
  }
  row.appendChild(act);
  return row;
}

/* ----- MCP integrations ----- */
const GH_LOGO = '<svg class="gh-logo" viewBox="0 0 16 16" fill="currentColor" aria-hidden="true"><path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0016 8c0-4.42-3.58-8-8-8z"/></svg>';
const JIRA_LOGO = '<svg class="gh-logo" viewBox="0 0 32 32" aria-hidden="true"><path fill="#2684FF" d="M16 1 30.5 15.5a1.3 1.3 0 0 1 0 1.9L16 31.9l-6.4-6.4L16 19.1l6.4-6.4z"/><path fill="#2684FF" opacity=".8" d="M16 12.7 9.6 6.3 3.2 12.7a1.3 1.3 0 0 0 0 1.9L9.6 21 16 14.6z"/></svg>';
const SENTRY_LOGO = '<svg class="gh-logo" viewBox="0 0 32 32" aria-hidden="true"><rect width="32" height="32" rx="8" fill="#362D59"/><path fill="#fff" d="M16 8.5c-.5 0-1 .27-1.27.73l-1.8 3.1.98.57a8.9 8.9 0 0 1 4.32 6.94h-1.9a7 7 0 0 0-3.36-5.32l-1.8 3.12.86.5a3.5 3.5 0 0 1 1.64 2.5H9.1a.28.28 0 0 1-.24-.42l.98-1.7-.98-.57-1 1.7A1.15 1.15 0 0 0 8.86 21h4.7a5.2 5.2 0 0 0-2.28-4.32l.83-1.43a6.8 6.8 0 0 1 2.94 5.75h5.1a10.6 10.6 0 0 0-5.2-9.14l.83-1.43c.05-.09.18-.09.23 0l5.75 9.96a.28.28 0 0 1-.24.42h-1.55c.02.38.03.76.03 1.15h1.52c.88 0 1.43-.96 1-1.72l-5.76-9.96A1.46 1.46 0 0 0 16 8.5z"/></svg>';
const GCAL_LOGO = '<svg class="gh-logo" viewBox="0 0 32 32" aria-hidden="true"><rect x="6" y="6" width="20" height="20" rx="3" fill="#fff" stroke="#4285F4" stroke-width="2"/><rect x="6" y="6" width="20" height="6" rx="3" fill="#4285F4"/><text x="16" y="24" font-size="10" font-weight="700" fill="#4285F4" text-anchor="middle" font-family="-apple-system,Arial,sans-serif">31</text></svg>';
const GHA_LOGO = '<svg class="gh-logo" viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="11" fill="#2088FF"/><path fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" d="M7 12.5l3 3 7-7"/></svg>';
const SLACK_LOGO = '<svg class="gh-logo" viewBox="0 0 122.8 122.8" aria-hidden="true"><path d="M25.8 77.6c0 7.1-5.8 12.9-12.9 12.9S0 84.7 0 77.6s5.8-12.9 12.9-12.9h12.9v12.9z" fill="#e01e5a"/><path d="M32.3 77.6c0-7.1 5.8-12.9 12.9-12.9s12.9 5.8 12.9 12.9v32.3c0 7.1-5.8 12.9-12.9 12.9s-12.9-5.8-12.9-12.9V77.6z" fill="#e01e5a"/><path d="M45.2 25.8c-7.1 0-12.9-5.8-12.9-12.9S38.1 0 45.2 0s12.9 5.8 12.9 12.9v12.9H45.2z" fill="#36c5f0"/><path d="M45.2 32.3c7.1 0 12.9 5.8 12.9 12.9s-5.8 12.9-12.9 12.9H12.9C5.8 58.1 0 52.3 0 45.2s5.8-12.9 12.9-12.9h32.3z" fill="#36c5f0"/><path d="M97 45.2c0-7.1 5.8-12.9 12.9-12.9s12.9 5.8 12.9 12.9-5.8 12.9-12.9 12.9H97V45.2z" fill="#2eb67d"/><path d="M90.5 45.2c0 7.1-5.8 12.9-12.9 12.9s-12.9-5.8-12.9-12.9V12.9C64.7 5.8 70.5 0 77.6 0s12.9 5.8 12.9 12.9v32.3z" fill="#2eb67d"/><path d="M77.6 97c7.1 0 12.9 5.8 12.9 12.9s-5.8 12.9-12.9 12.9-12.9-5.8-12.9-12.9V97h12.9z" fill="#ecb22e"/><path d="M77.6 90.5c-7.1 0-12.9-5.8-12.9-12.9s5.8-12.9 12.9-12.9h32.3c7.1 0 12.9 5.8 12.9 12.9s-5.8 12.9-12.9 12.9H77.6z" fill="#ecb22e"/></svg>';
const GMAIL_LOGO = '<svg class="gh-logo" viewBox="0 0 48 48" aria-hidden="true"><path fill="#4caf50" d="M45 16.2l-5 2.75-5 4.75L35 40h7c1.657 0 3-1.343 3-3V16.2z"/><path fill="#1e88e5" d="M3 16.2l3.614 1.71L13 23.7V40H6c-1.657 0-3-1.343-3-3V16.2z"/><polygon fill="#e53935" points="35,11.2 24,19.45 13,11.2 12,17 13,23.7 24,31.95 35,23.7 36,17"/><path fill="#c62828" d="M3 12.298V16.2l10 7.5V11.2L9.876 8.859C9.132 8.301 8.228 8 7.298 8C4.924 8 3 9.924 3 12.298z"/><path fill="#fbc02d" d="M45 12.298V16.2l-10 7.5V11.2l3.124-2.341C38.868 8.301 39.772 8 40.702 8C43.076 8 45 9.924 45 12.298z"/></svg>';

// Built-in integrations. These MCP endpoints don't support OAuth dynamic
// client registration, so we connect with a token (Bearer header).
const PRESETS = [
  {
    key: 'github', name: 'GitHub', logo: GH_LOGO,
    url: 'https://api.githubcopilot.com/mcp/', urlEditable: false,
    tokenPh: 'Paste a GitHub Personal Access Token',
    note: 'Create a token at <a href="https://github.com/settings/personal-access-tokens" target="_blank" rel="noopener">github.com/settings → Personal access tokens</a> with the repo permissions you want forge-cockpit to have.',
  },
  {
    key: 'jira', name: 'Jira', logo: JIRA_LOGO, type: 'stdio',
    fields: [
      { id: 'site', placeholder: 'Site URL — https://your-site.atlassian.net', required: true },
      { id: 'email', placeholder: 'Atlassian account email', required: true },
      { id: 'token', placeholder: 'API token', password: true, required: true },
    ],
    build: (v) => ({ command: 'uvx', args: ['mcp-atlassian'], env: { JIRA_URL: v.site, JIRA_USERNAME: v.email, JIRA_API_TOKEN: v.token } }),
    note: 'Runs a local <b>mcp-atlassian</b> bridge via <code>uvx</code>. Uses your site URL, email and <a href="https://id.atlassian.com/manage-profile/security/api-tokens" target="_blank" rel="noopener">API token</a>.',
  },
  {
    key: 'sentry', name: 'Sentry', logo: SENTRY_LOGO, type: 'stdio',
    fields: [
      { id: 'token', placeholder: 'Sentry user auth token', password: true, required: true },
      { id: 'host', placeholder: 'Host — leave blank for sentry.io (self-hosted only)', required: false },
    ],
    build: (v) => ({ command: 'npx', args: ['-y', '@sentry/mcp-server@latest'].concat(v.host ? ['--host=' + v.host] : []), env: { SENTRY_ACCESS_TOKEN: v.token } }),
    note: 'Runs the official <b>@sentry/mcp-server</b> via <code>npx</code> (needs Node). Create a <a href="https://sentry.io/settings/account/api/auth-tokens/" target="_blank" rel="noopener">user auth token</a> with org/project/event scopes.',
  },
  {
    key: 'slack', name: 'Slack', logo: SLACK_LOGO, type: 'stdio',
    fields: [
      { id: 'token', placeholder: 'Bot User OAuth Token (xoxb-…)', password: true, required: true },
      { id: 'post', type: 'toggle', label: 'Enable posting (let the agent send messages)' },
    ],
    build: (v) => {
      const t = (v.token || '').trim();
      const env = t.startsWith('xoxp') ? { SLACK_MCP_XOXP_TOKEN: t } : { SLACK_MCP_XOXB_TOKEN: t };
      if (v.post) env.SLACK_MCP_ADD_MESSAGE_TOOL = 'true';
      return { command: 'npx', args: ['-y', 'slack-mcp-server@latest', '--transport', 'stdio'], env };
    },
    note: 'Runs <b>slack-mcp-server</b> via <code>npx</code> (needs Node). Paste your app\'s <b>Bot User OAuth Token</b> (<code>xoxb-…</code>) from <b>OAuth &amp; Permissions</b>, with scopes <code>chat:write</code>, <code>channels:history</code>, <code>channels:read</code>, <code>groups:history</code>, <code>users:read</code>, and invite the bot to the channels it should read. Toggle <b>posting</b> on to let it send.',
  },
  {
    key: 'gmail', name: 'Gmail', logo: GMAIL_LOGO, type: 'stdio',
    fields: [
      { id: 'email', placeholder: 'Gmail address (you@gmail.com)', required: true },
      { id: 'pass', placeholder: 'App password (16 chars)', password: true, required: true },
    ],
    build: (v) => ({
      command: 'npx',
      args: ['-y', 'mcp-mail-server'],
      env: {
        IMAP_HOST: 'imap.gmail.com', IMAP_PORT: '993', IMAP_SECURE: 'true',
        SMTP_HOST: 'smtp.gmail.com', SMTP_PORT: '465', SMTP_SECURE: 'true',
        EMAIL_USER: (v.email || '').trim(),
        EMAIL_PASS: (v.pass || '').replace(/\s+/g, ''),
      },
    }),
    note: 'Runs <b>mcp-mail-server</b> via <code>npx</code> (needs Node) over Gmail IMAP/SMTP — reads and sends. Needs <b>2-Step Verification</b> on, then create a 16-char <b>App Password</b> at <a href="https://myaccount.google.com/apppasswords" target="_blank" rel="noopener">myaccount.google.com/apppasswords</a> and paste it above (spaces are fine). Stored locally in <code>~/forge/.mcp.json</code>.',
  },
];
const PRESET_KEYS = new Set(PRESETS.map((p) => p.key));

async function loadMcp() {
  const servers = await (await api('/api/mcp')).json();
  const box = $('integrations'); box.innerHTML = '';
  for (const p of PRESETS) box.appendChild(renderIntegrationCard(p, servers.find((s) => s.name === p.key)));
  box.appendChild(await renderGcalCard());

  // Any non-preset custom servers, shown read-only with a Remove button.
  const others = servers.filter((s) => !PRESET_KEYS.has(s.name));
  const list = $('mcp-list'); list.innerHTML = '';
  for (const s of others) {
    list.appendChild(connRow({
      name: s.name, sub: s.detail ? (s.url + ' · ' + s.detail) : (s.url || s.kind),
      badge: mcpBadge(s.status),
      actions: [{ label: 'Remove', onClick: () => mcpDelete(s.name) }],
    }));
  }
}

function mcpBadge(status) {
  if (status === 'connected') return { text: 'connected', cls: 'on' };
  if (status === 'failed') return { text: 'error', cls: 'off' };
  return { text: 'not connected', cls: 'off' };
}

function renderIntegrationCard(preset, srv) {
  const connected = srv && srv.status === 'connected';
  const failed = srv && srv.status === 'failed';
  const card = document.createElement('div'); card.className = 'gh-card';

  const head = document.createElement('div'); head.className = 'gh-head';
  head.innerHTML = preset.logo;
  const t = document.createElement('div'); t.style.flex = '1';
  const title = document.createElement('div'); title.className = 'gh-title'; title.textContent = preset.name;
  const sub = document.createElement('div'); sub.className = 'gh-sub';
  sub.textContent = connected ? (srv.detail || 'connected') : failed ? 'connection failed' : 'not connected';
  t.append(title, sub); head.appendChild(t);
  const badge = document.createElement('span'); const b = mcpBadge(srv ? srv.status : 'none');
  badge.className = 'badge ' + b.cls; badge.textContent = b.text; head.appendChild(badge);
  card.appendChild(head);

  const body = document.createElement('div'); body.className = 'gh-body';
  if (connected) {
    const btn = document.createElement('button'); btn.className = 'btn btn-sm btn-ghost'; btn.textContent = 'Disconnect';
    btn.onclick = () => integrationDisconnect(preset);
    body.appendChild(btn);
  } else if (preset.type === 'stdio') {
    renderStdioForm(preset, body);
    if (failed) { const f = document.createElement('div'); f.className = 'gh-fail'; f.textContent = String(srv.detail || '').slice(0, 240); body.appendChild(f); }
    const note = document.createElement('div'); note.className = 'gh-note'; note.innerHTML = preset.note;
    body.appendChild(note);
  } else {
    const row = document.createElement('div'); row.className = 'gh-connect';
    const input = document.createElement('input'); input.type = 'password'; input.placeholder = preset.tokenPh;
    const btn = document.createElement('button'); btn.className = 'btn btn-sm btn-primary'; btn.textContent = 'Connect';
    const go = () => integrationConnectHttp(preset, preset.url, input.value.trim(), btn);
    btn.onclick = go;
    input.addEventListener('keydown', (e) => { if (e.key === 'Enter' && !e.isComposing) go(); });
    row.append(input, btn); body.appendChild(row);
    if (failed) { const f = document.createElement('div'); f.className = 'gh-fail'; f.textContent = String(srv.detail || '').slice(0, 240); body.appendChild(f); }
    const note = document.createElement('div'); note.className = 'gh-note'; note.innerHTML = preset.note;
    body.appendChild(note);
  }
  card.appendChild(body);
  return card;
}

// Google Calendar is not an MCP server: it connects via a read-only private
// iCal URL, stored server-side, and only powers the Boards card.
async function renderGcalCard() {
  let cur = null;
  try { cur = (await (await api('/api/gcal')).json()).url; } catch (e) {}
  const connected = !!cur;
  const card = document.createElement('div'); card.className = 'gh-card';
  const head = document.createElement('div'); head.className = 'gh-head';
  head.innerHTML = GCAL_LOGO;
  const t = document.createElement('div'); t.style.flex = '1';
  const title = document.createElement('div'); title.className = 'gh-title'; title.textContent = 'Google Calendar';
  const sub = document.createElement('div'); sub.className = 'gh-sub'; sub.textContent = connected ? 'connected (read-only)' : 'not connected';
  t.append(title, sub); head.appendChild(t);
  const badge = document.createElement('span'); badge.className = 'badge ' + (connected ? 'on' : 'off'); badge.textContent = connected ? 'connected' : 'not connected';
  head.appendChild(badge); card.appendChild(head);

  const body = document.createElement('div'); body.className = 'gh-body';
  if (connected) {
    const btn = document.createElement('button'); btn.className = 'btn btn-sm btn-ghost'; btn.textContent = 'Disconnect';
    btn.onclick = async () => { btn.disabled = true; await api('/api/gcal', { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ url: '' }) }); loadMcp(); };
    body.appendChild(btn);
  } else {
    const row = document.createElement('div'); row.className = 'gh-connect';
    const input = document.createElement('input'); input.placeholder = 'Secret iCal URL (…/basic.ics)';
    const btn = document.createElement('button'); btn.className = 'btn btn-sm btn-primary'; btn.textContent = 'Connect';
    const go = async () => {
      const url = input.value.trim(); if (!url) return;
      btn.disabled = true; btn.textContent = 'Connecting…';
      await api('/api/gcal', { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ url }) });
      // Validate by fetching the board once; surface a clear error if it fails.
      const chk = await api('/api/board/gcal');
      if (!chk.ok) {
        const d = await chk.json().catch(() => ({}));
        await api('/api/gcal', { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ url: '' }) });
        await confirmModal({ title: 'Could not read calendar', body: (d.error || 'That URL did not return an iCal feed.') + '\n\nIn Google Calendar: Settings → your calendar → "Secret address in iCal format".', confirmText: 'OK', hideCancel: true, danger: false });
      }
      loadMcp();
    };
    btn.onclick = go;
    input.addEventListener('keydown', (e) => { if (e.key === 'Enter' && !e.isComposing) go(); });
    row.append(input, btn); body.appendChild(row);
    const note = document.createElement('div'); note.className = 'gh-note';
    note.innerHTML = 'In Google Calendar: <b>Settings → [your calendar] → Integrate calendar → “Secret address in iCal format”</b>. Read-only; shows upcoming events on the 📋 board.';
    body.appendChild(note);
  }
  card.appendChild(body);
  return card;
}

// Local (stdio) bridges — Jira, Sentry, … — configured from preset.fields.
function renderStdioForm(preset, body) {
  const form = document.createElement('div'); form.className = 'gh-form';
  const inputs = {};
  for (const f of preset.fields) {
    if (f.type === 'toggle') {
      const row = document.createElement('label'); row.className = 'sw-row';
      const cb = document.createElement('input'); cb.type = 'checkbox'; cb.className = 'sw'; cb.checked = !!f.default;
      const span = document.createElement('span'); span.textContent = f.label || f.id;
      row.append(cb, span); form.appendChild(row);
      inputs[f.id] = cb;
      continue;
    }
    const el = document.createElement('input');
    if (f.password) el.type = 'password';
    el.placeholder = f.placeholder || f.id;
    inputs[f.id] = el; form.appendChild(el);
  }
  const btn = document.createElement('button'); btn.className = 'btn btn-sm btn-primary'; btn.textContent = 'Connect'; btn.style.alignSelf = 'flex-start';
  form.appendChild(btn); body.appendChild(form);
  const go = () => {
    const v = {};
    for (const f of preset.fields) {
      const el = inputs[f.id];
      v[f.id] = el.type === 'checkbox' ? (el.checked ? 'true' : '') : el.value.trim();
    }
    connectStdio(preset, v, btn);
  };
  btn.onclick = go;
  for (const f of preset.fields) {
    if (inputs[f.id].type === 'checkbox') continue;
    inputs[f.id].addEventListener('keydown', (e) => { if (e.key === 'Enter' && !e.isComposing) go(); });
  }
}

async function connectStdio(preset, v, btn) {
  const missing = preset.fields.some((f) => f.required && !v[f.id]);
  if (missing) { await confirmModal({ title: 'Missing fields', body: 'Please fill in all required fields.', confirmText: 'OK', hideCancel: true, danger: false }); return; }
  if (btn) { btn.disabled = true; btn.textContent = 'Connecting…'; }
  const cfg = preset.build(v);
  const res = await api('/api/mcp', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(Object.assign({ name: preset.key }, cfg)) });
  await finishConnect(preset, res, btn);
}

async function integrationConnectHttp(preset, url, token, btn) {
  if (!token) { await confirmModal({ title: 'Token required', body: 'Paste a token to connect ' + preset.name + '.', confirmText: 'OK', hideCancel: true, danger: false }); return; }
  if (btn) { btn.disabled = true; btn.textContent = 'Connecting…'; }
  const res = await api('/api/mcp', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ name: preset.key, url, token }) });
  await finishConnect(preset, res, btn);
}

async function finishConnect(preset, res, btn) {
  if (!res.ok) { await confirmModal({ title: 'Failed', body: 'Could not save the connection.', confirmText: 'OK', hideCancel: true, danger: false }); if (btn) { btn.disabled = false; btn.textContent = 'Connect'; } return; }
  await loadMcp();
  const check = await (await api('/api/mcp')).json();
  const srv = check.find((s) => s.name === preset.key);
  if (srv && srv.status === 'connected') await confirmModal({ title: preset.name + ' connected', body: 'forge-cockpit can now use ' + preset.name + ' tools (' + (srv.detail || 'ready') + ').', confirmText: 'Done', hideCancel: true, danger: false });
  else await confirmModal({ title: 'Saved, but not connected', body: preset.name + ' did not connect: ' + String(srv && srv.detail || 'unknown error').slice(0, 260) + '\n\nCheck the values and that the runtime (uvx/docker) is installed.', confirmText: 'OK', hideCancel: true, danger: false });
}
async function integrationDisconnect(preset) {
  const ok = await confirmModal({ title: 'Disconnect ' + preset.name, body: 'Remove the stored ' + preset.name + ' token and connection?', confirmText: 'Disconnect' });
  if (!ok) return;
  await api('/api/mcp/' + encodeURIComponent(preset.key), { method: 'DELETE' });
  loadMcp();
}

$('mcp-add').onclick = async () => {
  const name = $('mcp-name').value.trim(), url = $('mcp-url').value.trim(), token = $('mcp-token').value.trim();
  if (!name || !url) { await confirmModal({ title: 'Missing fields', body: 'Both a name and a URL are required.', confirmText: 'OK', hideCancel: true, danger: false }); return; }
  const body = { name, url }; if (token) body.token = token;
  const res = await api('/api/mcp', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
  if (!res.ok) { await confirmModal({ title: 'Failed', body: 'Could not add server.', confirmText: 'OK', hideCancel: true, danger: false }); return; }
  $('mcp-name').value = ''; $('mcp-url').value = ''; $('mcp-token').value = ''; loadMcp();
};
async function mcpDelete(name) {
  const ok = await confirmModal({ title: 'Remove MCP server', body: 'Remove “' + name + '” from the config?', confirmText: 'Remove' });
  if (!ok) return;
  await api('/api/mcp/' + encodeURIComponent(name), { method: 'DELETE' });
  loadMcp();
}

/* ----- Providers ----- */
async function loadProviders() {
  const list = $('prov-list'); list.innerHTML = '<div class="conn-empty">Loading…</div>';
  const res = await api('/api/providers'); const provs = await res.json();
  list.innerHTML = '';
  // Configured first, then the rest alphabetically (backend already sorts).
  provs.sort((a, b) => (b.configured - a.configured));
  const signedIn = provs.filter((p) => p.configured).length;
  $('prov-summary').textContent = 'AI providers' + (signedIn ? ' · ' + signedIn + ' signed in' : '');
  for (const p of provs) {
    const actions = [];
    if (p.configured) {
      actions.push({ label: 'Sign out', onClick: () => providerLogout(p.id) });
    } else {
      if (p.methods.includes('device')) actions.push({ label: 'Sign in', cls: 'btn-primary', onClick: () => providerDevice(p.id) });
      if (p.methods.includes('api_key')) actions.push({ label: 'API key', cls: p.methods.includes('device') ? 'btn-ghost' : 'btn-primary', onClick: () => providerApiKey(p.id) });
      if (!p.methods.some((m) => m === 'device' || m === 'api_key')) actions.push({ label: 'CLI only', cls: 'btn-ghost', onClick: () => confirmModal({ title: p.id, body: 'This provider uses “' + p.methods.join(', ') + '”. Sign in with: forge provider login', confirmText: 'OK', hideCancel: true, danger: false }) });
    }
    $('prov-list').appendChild(connRow({
      name: p.id, sub: p.methods.join(' · '),
      badge: { text: p.configured ? 'signed in' : 'not signed in', cls: p.configured ? 'on' : 'off' },
      actions,
    }));
  }
}
async function providerApiKey(id) {
  const key = await confirmModal({ title: 'Sign in to ' + id, body: 'Paste your API key.', confirmText: 'Save', input: { placeholder: 'sk-…', type: 'password' } });
  if (!key) return;
  const res = await api('/api/providers/apikey', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ id, api_key: key }) });
  if (!res.ok) { const e = await res.json().catch(() => ({})); await confirmModal({ title: 'Failed', body: e.error ? String(e.error).slice(0, 300) : 'Could not save key.', confirmText: 'OK', hideCancel: true, danger: false }); }
  loadProviders(); loadModels();
}
async function providerDevice(id) {
  const res = await api('/api/providers/device', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ id }) });
  if (!res.ok) { await confirmModal({ title: 'Failed', body: 'Could not start device sign-in.', confirmText: 'OK', hideCancel: true, danger: false }); return; }
  const reader = res.body.getReader(); const dec = new TextDecoder(); let buf = '';
  let dialog = null;
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += dec.decode(value, { stream: true });
    const frames = buf.split('\n\n'); buf = frames.pop();
    for (const f of frames) {
      const line = f.split('\n').find((l) => l.startsWith('data:'));
      if (!line) continue;
      const ev = JSON.parse(line.slice(5).trim());
      if (ev.type === 'code') {
        if (ev.verification_uri_complete || ev.verification_uri) window.open(ev.verification_uri_complete || ev.verification_uri, '_blank');
        dialog = confirmModal({ title: 'Enter this code', body: 'Open ' + ev.verification_uri + ' and enter code:  ' + ev.user_code + '\n\nWaiting for you to authorize…', confirmText: 'Working…', hideCancel: true, danger: false });
      } else if (ev.type === 'done') {
        $('modal-overlay').classList.remove('open');
        await confirmModal({ title: 'Connected', body: 'Sign-in succeeded.', confirmText: 'OK', hideCancel: true, danger: false });
      } else if (ev.type === 'error') {
        $('modal-overlay').classList.remove('open');
        await confirmModal({ title: 'Sign-in failed', body: String(ev.message || '').slice(0, 300), confirmText: 'OK', hideCancel: true, danger: false });
      }
    }
  }
  loadProviders(); loadModels();
}
async function providerLogout(id) {
  const ok = await confirmModal({ title: 'Sign out of ' + id, body: 'Remove stored credentials for this provider?', confirmText: 'Sign out' });
  if (!ok) return;
  await api('/api/providers/' + encodeURIComponent(id), { method: 'DELETE' });
  loadProviders(); loadModels();
}
