/* ---------- usage analytics ---------- */
const USAGE = { days: 30 };
$('usage-open').onclick = () => { $('usage-overlay').classList.add('open'); usageLoad(); };
$('usage-close').onclick = () => $('usage-overlay').classList.remove('open');
$('usage-refresh').onclick = () => usageLoad();
function fmtNum(n) { n = +n || 0; if (n >= 1e9) return (n / 1e9).toFixed(1) + 'B'; if (n >= 1e6) return (n / 1e6).toFixed(1) + 'M'; if (n >= 1e3) return (n / 1e3).toFixed(1) + 'K'; return '' + n; }
function fmtUsd(n) { return '$' + (+n || 0).toFixed(2); }
async function usageLoad() {
  const rb = $('usage-range'); rb.innerHTML = '';
  for (const [d, lbl] of [[7, '7d'], [30, '30d'], [90, '90d'], [0, 'All']]) {
    const b = document.createElement('button'); b.className = 'btn btn-sm' + (USAGE.days === d ? ' btn-primary' : ''); b.textContent = lbl;
    b.onclick = () => { USAGE.days = d; usageLoad(); }; rb.appendChild(b);
  }
  const body = $('usage-body'); body.innerHTML = '<span class="sl">scanning sessions…</span>';
  const d = await api('/api/usage?days=' + USAGE.days).then((r) => r.json()).catch(() => null);
  if (!d) { body.innerHTML = '<span class="err">failed to load usage</span>'; return; }
  const t = d.total || {};
  const totalTok = (t.input || 0) + (t.output || 0) + (t.cache_read || 0) + (t.cache_write || 0);
  const cacheHit = (t.input + t.cache_read) ? (t.cache_read / (t.input + t.cache_read) * 100) : 0;
  const dailyAvg = d.days_with_activity ? t.cost / d.days_with_activity : 0;
  const card = (label, big, sub, tip) => '<div title="' + (tip || '') + '" style="flex:1;min-width:150px;border:1px solid var(--border);border-radius:10px;padding:12px 14px">' +
    '<div class="sl">' + label + '</div><div style="font-size:22px;font-weight:700;margin:2px 0">' + big + '</div><div class="sl">' + (sub || '') + '</div></div>';
  let html = '<div style="border:1px solid var(--border);border-radius:8px;padding:8px 12px;margin-bottom:12px;font-size:12px;background:var(--bg-2,rgba(94,106,210,.06))">' +
    'ℹ️ Token spend by your <b>forge agents</b> (chat / team / pipeline), priced at your provider\'s per-token rate (e.g. DeepSeek). Cache reads are near-free and shown separately.</div>' +
    '<div style="display:flex;gap:10px;flex-wrap:wrap;margin-bottom:14px">' +
    card('Total cost', fmtUsd(t.cost), (t.messages || 0) + ' messages', 'Input + output + cache-write tokens at your provider rate (cache reads shown separately).') +
    card('+ cache reads', fmtUsd(t.cache_cost), fmtNum(t.cache_read) + ' cached tokens', 'Cache-read tokens are billed at a large discount — kept out of the headline.') +
    card('Tokens', fmtNum(totalTok), fmtNum(t.input) + ' in · ' + fmtNum(t.output) + ' out') +
    card('Daily avg', fmtUsd(dailyAvg), d.days_with_activity + ' active days · ' + cacheHit.toFixed(0) + '% cache hit') +
    '</div>';

  // cost trend (inline SVG)
  const trend = d.trend || [];
  if (trend.length) {
    const max = Math.max(...trend.map((x) => x.cost), 0.0001);
    const W = 1040, Hh = 120, pad = 4;
    const pts = trend.map((x, i) => {
      const px = trend.length > 1 ? pad + i * (W - 2 * pad) / (trend.length - 1) : W / 2;
      const py = Hh - pad - (x.cost / max) * (Hh - 2 * pad);
      return px.toFixed(1) + ',' + py.toFixed(1);
    }).join(' ');
    html += '<div style="border:1px solid var(--border);border-radius:10px;padding:12px 14px;margin-bottom:14px">' +
      '<div class="sl" style="margin-bottom:6px">Cost per day — peak ' + fmtUsd(max) + '</div>' +
      '<svg viewBox="0 0 ' + W + ' ' + Hh + '" style="width:100%;height:120px"><polyline points="' + pts + '" fill="none" stroke="var(--accent,#5e6ad2)" stroke-width="2"/></svg>' +
      '<div class="sl">' + (trend[0] || {}).day + ' → ' + (trend[trend.length - 1] || {}).day + '</div></div>';
  }

  // by model + by project side by side
  const bar = (rows, key, unit) => {
    if (!rows.length) return '<span class="sl">(none)</span>';
    const max = Math.max(...rows.map((r) => r.cost), 0.0001);
    return rows.map((r) => '<div style="margin-bottom:6px"><div style="display:flex;justify-content:space-between;font-size:12px"><span>' + escapeHtml(r[key] || '?') + '</span><span>' + fmtUsd(r.cost) + '</span></div>' +
      '<div style="height:6px;background:var(--bg-2,rgba(0,0,0,.06));border-radius:3px;overflow:hidden"><div style="height:100%;width:' + (r.cost / max * 100).toFixed(1) + '%;background:var(--accent,#5e6ad2)"></div></div></div>').join('');
  };
  html += '<div style="display:flex;gap:14px;flex-wrap:wrap">' +
    '<div style="flex:1;min-width:300px;border:1px solid var(--border);border-radius:10px;padding:12px 14px"><div class="sl" style="margin-bottom:8px">By model</div>' + bar(d.by_model || [], 'model') + '</div>' +
    '<div style="flex:1;min-width:300px;border:1px solid var(--border);border-radius:10px;padding:12px 14px"><div class="sl" style="margin-bottom:8px">By project (top 20)</div>' + bar(d.by_project || [], 'project') + '</div>' +
    '</div>';
  if (!totalTok) html = '<span class="sl">no forge agent activity in this range — run a chat, team, or pipeline first.</span>';
  body.innerHTML = html;
}
