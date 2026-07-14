const $ = (id) => document.getElementById(id);
const messagesEl = $('messages'), convosEl = $('convos'), inputEl = $('input');
let conversationId = null;
let streaming = false;
let pendingImages = [];

// Every API call carries the per-run bearer token. Using a custom header
// (rather than a cookie) also blocks cross-origin CSRF, since the browser
// won't let other pages set it without a CORS grant we never issue.
function api(url, opts = {}) {
  opts.headers = Object.assign({ Authorization: 'Bearer ' + TOKEN }, opts.headers || {});
  return fetch(url, opts);
}
function scrollDown() { messagesEl.scrollTop = messagesEl.scrollHeight; }

/* ---------- theme toggle ---------- */
(function initTheme() {
  const saved = localStorage.getItem('forge-theme');
  if (saved) document.documentElement.setAttribute('data-theme', saved);
})();
$('theme').onclick = () => {
  const cur = document.documentElement.getAttribute('data-theme');
  const dark = cur ? cur === 'dark' : matchMedia('(prefers-color-scheme: dark)').matches;
  const next = dark ? 'light' : 'dark';
  document.documentElement.setAttribute('data-theme', next);
  localStorage.setItem('forge-theme', next);
};

/* ---------- confirm modal (replaces native confirm/alert) ---------- */
// App-styled replacements for the browser's native confirm/prompt/alert.
const uiConfirm = (body, { title = 'Confirm', confirmText = 'OK', danger = false } = {}) =>
  confirmModal({ title, body, confirmText, danger });
const uiPrompt = (body, { title = 'Input', value = '', placeholder = '', confirmText = 'OK' } = {}) =>
  confirmModal({ title, body, confirmText, danger: false, input: { value, placeholder } });
const uiAlert = (body, { title = 'Notice' } = {}) =>
  confirmModal({ title, body, confirmText: 'OK', hideCancel: true, danger: false });
function confirmModal({ title, body, confirmText = 'Delete', hideCancel = false, danger = true, input = null }) {
  return new Promise((resolve) => {
    const overlay = $('modal-overlay'), ok = $('modal-ok'), cancel = $('modal-cancel'), field = $('modal-input');
    $('modal-title').textContent = title;
    $('modal-body').textContent = body;
    ok.textContent = confirmText;
    // Destructive → danger button; input/acknowledgement → neutral primary.
    ok.className = 'btn ' + ((danger && !input && !hideCancel) ? 'btn-danger' : 'btn-primary');
    cancel.style.display = hideCancel ? 'none' : '';
    if (input) {
      field.style.display = ''; field.value = input.value || '';
      field.placeholder = input.placeholder || ''; field.type = input.type || 'text';
    } else { field.style.display = 'none'; }
    overlay.classList.add('open');
    setTimeout(() => (input ? field : ok).focus(), 0);

    // Resolve value: boolean for confirms; the string (or null) for prompts.
    const result = (okPressed) => input ? (okPressed ? field.value : null) : okPressed;
    const done = (okPressed) => {
      overlay.classList.remove('open');
      ok.removeEventListener('click', onOk);
      cancel.removeEventListener('click', onCancel);
      document.removeEventListener('keydown', onKey);
      overlay.removeEventListener('mousedown', onBackdrop);
      resolve(result(okPressed));
    };
    const onOk = () => done(true);
    const onCancel = () => done(false);
    const onKey = (e) => {
      if (e.key === 'Escape') done(false);
      else if (e.key === 'Enter' && !e.isComposing && e.keyCode !== 229 && (!input || e.target === field)) done(true);
    };
    const onBackdrop = (e) => { if (e.target === overlay) done(false); };
    ok.addEventListener('click', onOk);
    cancel.addEventListener('click', onCancel);
    document.addEventListener('keydown', onKey);
    overlay.addEventListener('mousedown', onBackdrop);
  });
}

/* ---------- tiny markdown renderer (XSS-safe: escapes before formatting) ---------- */
function escapeHtml(s) {
  return s.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}

// Compact, language-agnostic syntax highlighter. Tokenizes the raw code and
// escapes every gap/token, so it stays XSS-safe. Good enough for Rust, JS,
// Python, Go, C-likes, etc. — not a full parser.
const HL_KEYWORDS = new Set(('fn let mut pub struct enum impl trait const static type where as dyn move use mod crate self super match if else for while loop break continue return in of async await unsafe extern box ref ' +
  'function var class extends typeof instanceof void export import default yield this new delete throw switch case do try catch finally ' +
  'def pass lambda global nonlocal elif except finally raise with del not and or is print assert ' +
  'func package interface defer go chan range select ' +
  'public private protected final abstract implements throws extends ' +
  'true false null nil undefined None True False').split(/\s+/));
const HASH_COMMENT_LANGS = new Set(['py', 'python', 'sh', 'bash', 'shell', 'zsh', 'rb', 'ruby', 'yaml', 'yml', 'toml', 'ini', 'r', 'perl', 'pl', 'makefile', 'dockerfile']);

function highlightCode(code, lang) {
  const hash = HASH_COMMENT_LANGS.has((lang || '').toLowerCase());
  const comment = hash ? '\\/\\/[^\\n]*|\\/\\*[\\s\\S]*?\\*\\/|#[^\\n]*' : '\\/\\/[^\\n]*|\\/\\*[\\s\\S]*?\\*\\/';
  const RE = new RegExp(
    '(' + comment + ')' +                                    // 1 comment
    '|("(?:\\\\.|[^"\\\\])*"|\'(?:\\\\.|[^\'\\\\])*\'|`(?:\\\\.|[^`\\\\])*`)' + // 2 string
    '|(\\b0x[0-9a-fA-F]+\\b|\\b\\d[\\d_]*\\.?\\d*(?:[eE][+-]?\\d+)?\\b)' +       // 3 number
    '|([A-Za-z_$][A-Za-z0-9_$]*)' +                          // 4 word (keyword/type/plain)
    '|([a-z_$][A-Za-z0-9_$]*)(?=\\s*\\()',                   // (unused; fn handled below)
    'g');
  let out = '', last = 0, m;
  while ((m = RE.exec(code))) {
    out += escapeHtml(code.slice(last, m.index));
    const full = m[0];
    let cls;
    if (m[1]) cls = 'c-com';
    else if (m[2]) cls = 'c-str';
    else if (m[3]) cls = 'c-num';
    else if (m[4]) {
      if (HL_KEYWORDS.has(full)) cls = 'c-kw';
      else if (/^[A-Z]/.test(full)) cls = 'c-typ';
      else if (code.slice(m.index + full.length).match(/^\s*\(/)) cls = 'c-fn';
      else cls = null;
    }
    out += cls ? '<span class="' + cls + '">' + escapeHtml(full) + '</span>' : escapeHtml(full);
    last = m.index + full.length;
    if (full.length === 0) RE.lastIndex++;
  }
  out += escapeHtml(code.slice(last));
  return out;
}

function renderMarkdown(src) {
  const blocks = [];
  // Fenced code blocks first, swapped for collision-proof placeholders.
  src = src.replace(/```([^\n]*)\n?([\s\S]*?)```/g, (m, lang, code) => {
    blocks.push(
      '<div class="code-block"><button class="copy-btn" type="button">Copy</button>' +
      '<pre class="code"><code>' + highlightCode(code.replace(/\n$/, ''), lang.trim()) + '</code></pre></div>'
    );
    return '%%FBLK' + (blocks.length - 1) + '%%';
  });
  src = escapeHtml(src);
  src = src.replace(/`([^`]+)`/g, (m, c) => '<code class="inline">' + c + '</code>');
  src = src.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
  src = src.replace(/(^|[^*])\*([^*\n]+)\*/g, '$1<em>$2</em>');
  src = src.replace(/\[([^\]]+)\]\((https?:\/\/[^)]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');

  const lines = src.split('\n');
  let html = '', list = null;
  const closeList = () => { if (list) { html += '</' + list + '>'; list = null; } };
  const isRow = (l) => /\|/.test(l) && /\S/.test(l);
  const isSep = (l) => /^\s*\|?[\s:|-]*-[\s:|-]*\|?\s*$/.test(l) && /\|/.test(l);
  const cells = (l) => l.replace(/^\s*\|/, '').replace(/\|\s*$/, '').split('|').map((c) => c.trim());
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]; let m;
    // GitHub-style table: header row + separator row + body rows.
    if (isRow(line) && i + 1 < lines.length && isSep(lines[i + 1])) {
      closeList();
      const head = cells(line);
      let j = i + 2; const rows = [];
      while (j < lines.length && isRow(lines[j]) && !isSep(lines[j])) { rows.push(cells(lines[j])); j++; }
      let t = '<div class="table-wrap"><table><thead><tr>';
      for (const h of head) t += '<th>' + h + '</th>';
      t += '</tr></thead><tbody>';
      for (const r of rows) {
        t += '<tr>';
        for (let k = 0; k < head.length; k++) t += '<td>' + (r[k] || '') + '</td>';
        t += '</tr>';
      }
      t += '</tbody></table></div>';
      html += t; i = j - 1; continue;
    }
    if ((m = line.trim().match(/^%%FBLK(\d+)%%$/))) { closeList(); html += blocks[m[1]]; continue; }
    if ((m = line.match(/^(#{1,3})\s+(.*)$/))) { closeList(); html += '<h' + m[1].length + '>' + m[2] + '</h' + m[1].length + '>'; continue; }
    if ((m = line.match(/^\s*>\s?(.*)$/))) { closeList(); html += '<blockquote>' + m[1] + '</blockquote>'; continue; }
    if ((m = line.match(/^\s*[-*]\s+(.*)$/))) { if (list !== 'ul') { closeList(); list = 'ul'; html += '<ul>'; } html += '<li>' + m[1] + '</li>'; continue; }
    if ((m = line.match(/^\s*\d+\.\s+(.*)$/))) { if (list !== 'ol') { closeList(); list = 'ol'; html += '<ol>'; } html += '<li>' + m[1] + '</li>'; continue; }
    if (line.trim() === '') { closeList(); continue; }
    closeList();
    html += '<p>' + line + '</p>';
  }
  closeList();
  html = html.replace(/%%FBLK(\d+)%%/g, (m, i) => blocks[i]);
  return html;
}
