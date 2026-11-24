// t3code-rs web UI — vanilla ES module. No build step, no external requests.
// All dynamic text goes through textContent / esc(); the Markdown renderer escapes before it builds HTML.

// ───────────────────────── utilities ─────────────────────────
const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];
const ESC = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' };
const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ESC[c]);

function h(tag, attrs, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v == null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k === 'text') el.textContent = v;
    else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
    else if (k === 'dataset') { for (const [a, b] of Object.entries(v)) if (b != null) el.dataset[a] = b; }
    else if (k in el && k !== 'list' && k !== 'form') { try { el[k] = v; } catch { el.setAttribute(k, v); } }
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const kid of kids.flat()) {
    if (kid == null || kid === false) continue;
    el.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
  }
  return el;
}

const ls = {
  get(k, d) { try { const v = localStorage.getItem(k); return v == null ? d : v; } catch { return d; } },
  set(k, v) { try { localStorage.setItem(k, v); } catch { /* storage may be unavailable */ } },
};

const ICONS = {
  menu: '<path d="M3 6h18M3 12h18M3 18h18"/>',
  search: '<circle cx="11" cy="11" r="7"/><path d="M21 21l-4.3-4.3"/>',
  diff: '<path d="M12 4v8M8 8h8M8 18h8"/>',
  clip: '<path d="M21 11l-9 9a5 5 0 0 1-7-7l9-9a3.5 3.5 0 0 1 5 5l-9 9a2 2 0 0 1-3-3l8-8"/>',
  stop: '<rect class="fill" x="6" y="6" width="12" height="12" rx="2"/>',
  send: '<path d="M22 2L11 13M22 2l-7 20-4-9-9-4 20-7z"/>',
  fork: '<circle cx="6" cy="5" r="2"/><circle cx="6" cy="19" r="2"/><circle cx="18" cy="9" r="2"/><path d="M6 7v10M18 11c0 4-6 3-12 6"/>',
  split: '<path d="M12 3v6M12 9L5 20M12 9l7 11"/>',
  close: '<path d="M6 6l12 12M18 6L6 18"/>',
  refresh: '<path d="M21 12a9 9 0 1 1-3-6.7L21 8M21 3v5h-5"/>',
  expand: '<path d="M7 9l5-5 5 5M7 15l5 5 5-5"/>',
  pin: '<path d="M12 17v5M9 3h6l-1 7 3 3H7l3-3-1-7z"/>',
  archive: '<path d="M3 4h18v4H3zM5 8v12h14V8M10 12h4"/>',
  copy: '<rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15V6a2 2 0 0 1 2-2h9"/>',
  open: '<path d="M14 4h6v6M20 4l-9 9M18 14v5H5V6h5"/>',
};
const icon = (n) => `<svg class="ic" viewBox="0 0 24 24" aria-hidden="true">${ICONS[n] || ''}</svg>`;
const hydrateIcons = (root = document) => $$('[data-icon]', root).forEach((el) => { if (!el.querySelector('svg')) el.insertAdjacentHTML('afterbegin', icon(el.dataset.icon)); });

function fmtAgo(iso) {
  const t = Date.parse(iso); if (!t) return '';
  const s = Math.max(0, (Date.now() - t) / 1000);
  if (s < 45) return 'たった今';
  if (s < 3600) return `${Math.round(s / 60)}分前`;
  if (s < 86400) return `${Math.round(s / 3600)}時間前`;
  if (s < 86400 * 30) return `${Math.round(s / 86400)}日前`;
  return new Date(t).toLocaleDateString('ja-JP');
}
function fmtDur(ms) {
  if (ms == null || !isFinite(ms)) return '–';
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(1)}秒`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}分${Math.round(s % 60)}秒`;
  return `${Math.floor(m / 60)}時間${m % 60}分`;
}
function fmtBytes(n) {
  n = Number(n) || 0;
  if (n < 1024) return `${n} B`;
  if (n < 1024 ** 2) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(2)} MB`;
  return `${(n / 1024 ** 3).toFixed(2)} GB`;
}
const fmtNum = (n) => Number(n || 0).toLocaleString('ja-JP');
function fmtTok(n) {
  n = Number(n) || 0;
  if (n >= 1e6) return `${(n / 1e6).toFixed(2)}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)}k`;
  return String(n);
}
const debounce = (fn, ms) => { let t; return (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); }; };
const rafThrottle = (fn) => { let q = false; return () => { if (q) return; q = true; requestAnimationFrame(() => { q = false; fn(); }); }; };
const clip = (s, n) => { s = String(s ?? ''); return s.length > n ? s.slice(0, n - 1) + '…' : s; };
const pretty = (v) => { try { return typeof v === 'string' ? v : JSON.stringify(v, null, 2); } catch { return String(v); } };
const isCoarse = () => matchMedia('(pointer: coarse)').matches;

function toast(msg, kind = '', ms = 3800) {
  const el = h('div', { class: `toast ${kind}`, text: msg });
  $('#toasts').append(el);
  setTimeout(() => el.remove(), ms);
}

// ───────────────────────── labels & domain helpers ─────────────────────────
const PROVIDER_LABEL = { claude: 'Claude', codex: 'Codex', antigravity: 'Antigravity', 'open-code': 'OpenCode', opencode: 'OpenCode', mock: 'Mock' };
const PROVIDER_FALLBACK = ['claude', 'codex', 'antigravity', 'open-code', 'mock'];
const provLabel = (p) => PROVIDER_LABEL[p] || p || '?';
const TIERS = {
  0: { short: 'T0', label: 'Tier 0 無料/ローカル' },
  1: { short: 'T1', label: 'Tier 1 高速' },
  2: { short: 'T2', label: 'Tier 2 監査' },
  3: { short: 'T3', label: 'Tier 3 設計' },
};
const STATUS_LABEL = {
  idle: '待機', running: '実行中', 'waiting-for-children': '子の完了待ち', 'rate-limited': 'レート制限', completed: '完了', failed: '失敗', interrupted: '中断',
  queued: '待ち行列', cancelled: '取消',
};
const REL_LABEL = { delegate: '委譲', delegated: '委譲', fork: '分岐', forked: '分岐', handoff: '引継', child: '子' };
const MODE_LABEL = { 'approval-required': '承認必須', 'auto-accept-edits': '編集を自動承認', 'full-access': 'フルアクセス' };
const BUSY = new Set(['running', 'waiting-for-children', 'rate-limited']);
const ACTIVE_SHELF = new Set(['running', 'rate-limited', 'waiting-for-children']);
const TERMINAL_RUN = new Set(['completed', 'failed', 'cancelled']);
const isBusy = (st) => BUSY.has(st);

function authLabel(m) {
  m = String(m || '').toLowerCase();
  if (m.includes('cloudflare')) return 'Cloudflare Access';
  if (m.includes('tailscale')) return 'Tailscale';
  if (m.includes('bearer')) return 'Bearer トークン';
  if (m.includes('local')) return 'LocalOnly（ループバック限定）';
  return m || '不明';
}

// ───────────────────────── state ─────────────────────────
const S = {
  meta: null,
  providers: [],
  projects: [],
  projectId: ls.get('t3.project', ''),
  threads: [],
  showArchived: ls.get('t3.archived', '0') === '1',
  threadId: null,
  detail: null,           // { thread, runs, items, children, handoffs, attachments }
  live: new Map(),        // threadId -> { runId, assistant, reasoning }
  pending: new Map(),     // threadId -> PendingRequest[]
  collapsed: new Set(),   // thread ids collapsed in tree
  behavior: ls.get('t3.behavior', 'steer') === 'queue' ? 'queue' : 'steer',
  atts: [],               // composer attachments
  stick: true,
  diffOpen: ls.get('t3.diff', '0') === '1',
  ignoreWs: ls.get('t3.ws', '1') !== '0',
  subscribed: new Set(),
  ws: null, wsOk: false,
  token: ls.get('t3.token', ''),
  theme: ls.get('t3.theme', 'auto'),
};

const threadById = (id) => S.threads.find((t) => t.id === id) || (S.detail?.thread?.id === id ? S.detail.thread : null)
  || S.detail?.children?.find((t) => t.id === id) || null;
const curThread = () => S.detail?.thread || null;

function findModelSpec(sel) {
  if (!sel) return null;
  const p = S.providers.find((x) => x.provider === sel.provider);
  return p?.models?.find((m) => m.id === sel.model) || null;
}
function tierNum(sel) {
  const spec = findModelSpec(sel);
  const m = /tier(\d)/.exec(spec?.tier || '');
  return m ? Number(m[1]) : null;
}
const tierBadge = (sel, full = false) => {
  const n = tierNum(sel);
  if (n == null || !TIERS[n]) return null;
  return h('span', { class: `badge t${n}`, title: TIERS[n].label, text: full ? TIERS[n].label : TIERS[n].short });
};

// ───────────────────────── API ─────────────────────────
class ApiError extends Error { constructor(status, msg) { super(msg); this.status = status; } }
let tokenPromptShown = false;

async function api(path, { method = 'GET', json, form, signal } = {}) {
  const headers = {};
  if (S.token) headers.Authorization = `Bearer ${S.token}`;
  let body;
  if (json !== undefined) { headers['Content-Type'] = 'application/json'; body = JSON.stringify(json); } else if (form) body = form;
  const res = await fetch(path, { method, headers, body, credentials: 'same-origin', signal, referrerPolicy: 'no-referrer' });
  if (!res.ok) {
    let msg = res.statusText || `HTTP ${res.status}`;
    try {
      const t = await res.text();
      try { const j = JSON.parse(t); msg = j.error?.message || j.error || j.message || t; } catch { msg = t || msg; }
    } catch { /* ignore */ }
    if (res.status === 401) promptToken();
    throw new ApiError(res.status, typeof msg === 'string' ? msg : pretty(msg));
  }
  if (res.status === 204) return null;
  const ct = res.headers.get('content-type') || '';
  return ct.includes('json') ? res.json() : res.text();
}

const blobCache = new Map();
async function attachmentBlobUrl(id) {
  if (blobCache.has(id)) return blobCache.get(id);
  const headers = S.token ? { Authorization: `Bearer ${S.token}` } : {};
  const p = fetch(`/api/attachments/${id}`, { headers, credentials: 'same-origin' })
    .then((r) => { if (!r.ok) throw new Error(r.statusText); return r.blob(); })
    .then((b) => URL.createObjectURL(b));
  blobCache.set(id, p);
  p.catch(() => blobCache.delete(id));
  return p;
}

function promptToken() {
  if (tokenPromptShown) return;
  tokenPromptShown = true;
  openModal('認証が必要です', (body, close) => {
    const inp = h('input', { class: 'inp', type: 'password', placeholder: 'Bearer トークン', autocomplete: 'off' });
    body.append(h('form', {
      class: 'form',
      onsubmit: (e) => { e.preventDefault(); S.token = inp.value.trim(); ls.set('t3.token', S.token); close(); location.reload(); },
    },
    h('p', { class: 'muted', text: 'サーバーが Bearer 認証で動作しています。トークンはこのブラウザの localStorage にのみ保存され、サーバー以外へは送信されません。（Cloudflare Access / Tailscale 経由なら通常は不要です）' }),
    h('div', { class: 'field' }, h('label', { text: 'トークン' }), inp),
    h('div', { class: 'actions' }, h('button', { class: 'btn primary', type: 'submit', text: '保存して再読み込み' }))));
    setTimeout(() => inp.focus(), 0);
  }, { onclose: () => { tokenPromptShown = false; } });
}

// ───────────────────────── modal helper (native <dialog>) ─────────────────────────
function openModal(title, build, { cls = '', onclose } = {}) {
  const dlg = h('dialog', { class: `modal ${cls}` });
  const close = () => { try { dlg.close(); } catch { dlg.remove(); } };
  const body = h('div', { class: 'modal-body' });
  const closeBtn = h('button', { class: 'ibtn', type: 'button', title: '閉じる', 'aria-label': '閉じる', onclick: close });
  closeBtn.innerHTML = icon('close');
  dlg.append(h('div', { class: 'modal-head' }, h('h2', { text: title }), closeBtn), body);
  dlg.addEventListener('close', () => { dlg.remove(); onclose?.(); });
  dlg.addEventListener('click', (e) => { if (e.target === dlg) close(); });
  document.body.append(dlg);
  dlg.showModal();
  build(body, close, dlg);
  return { dlg, close, body };
}

// ───────────────────────── Markdown (safe: escape first, never raw HTML) ─────────────────────────
const SAFE_URL = /^(https?:\/\/|\/|#|mailto:)/i;

function mdInline(src) {
  const stash = [];
  const put = (html) => { stash.push(html); return `\u0000${stash.length - 1}\u0001`; };
  let s = String(src).replace(/[\u0000\u0001]/g, '');
  s = s.replace(/(`+)([\s\S]*?[^`])\1(?!`)/g, (_, __, code) => put(`<code>${esc(code.trim())}</code>`));
  s = esc(s);
  const emph = (t) => t
    .replace(/\*\*([^*\n]+?)\*\*/g, '<strong>$1</strong>')
    .replace(/__([^_\n]+?)__/g, '<strong>$1</strong>')
    .replace(/(^|[^*\w])\*([^*\s][^*\n]*?)\*(?!\*)/g, '$1<em>$2</em>')
    .replace(/(^|[^\w_])_([^_\s][^_\n]*?)_(?![\w_])/g, '$1<em>$2</em>')
    .replace(/~~([^~\n]+?)~~/g, '<del>$1</del>');
  // images are never fetched (privacy): show as plain link
  s = s.replace(/!?\[([^\]\n]+)\]\(([^)\s]+)(?:\s+&quot;[^)]*&quot;)?\)/g, (m, t, u) => (SAFE_URL.test(u)
    ? put(`<a href="${u}" target="_blank" rel="noopener noreferrer">${emph(t)}</a>`) : m));
  s = s.replace(/(^|[\s(（])(https?:\/\/[^\s<>()（）]+[^\s<>()（）.,;:!?、。])/g, (m, pre, u) => `${pre}${put(`<a href="${u}" target="_blank" rel="noopener noreferrer">${u}</a>`)}`);
  s = emph(s);
  for (let i = 0; i < 4 && /\u0000\d+\u0001/.test(s); i++) s = s.replace(/\u0000(\d+)\u0001/g, (_, n) => stash[+n]);
  return s;
}

function mdCodeBlock(code, lang) {
  lang = (lang || '').replace(/[^\w+#.-]/g, '').slice(0, 24);
  let body;
  if (lang === 'diff' || lang === 'patch') {
    body = code.split('\n').map((l) => {
      const c = l.startsWith('+') && !l.startsWith('+++') ? 'dl-add' : l.startsWith('-') && !l.startsWith('---') ? 'dl-del' : l.startsWith('@@') ? 'dl-hunk' : '';
      return `<span class="${c}">${esc(l) || ' '}</span>`;
    }).join('');
  } else body = esc(code);
  return `<div class="codeblock"><div class="cb-head"><span>${esc(lang || 'text')}</span><button type="button" data-copy>コピー</button></div><pre><code>${body}</code></pre></div>`;
}

const RE_FENCE = /^\s*(`{3,}|~{3,})\s*([^\s`]*)/;
const RE_HR = /^\s*([-*_])(\s*\1){2,}\s*$/;
const RE_LI = /^(\s*)([-*+]|\d+[.)])\s+(.*)$/;
const RE_TSEP = /^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$/;
const indentOf = (l) => l.match(/^\s*/)[0].length;

function mdBlocks(lines) {
  const out = [];
  let i = 0;
  const startsBlock = (l, next) => RE_FENCE.test(l) || /^#{1,6}\s/.test(l) || RE_HR.test(l) || /^\s*>/.test(l) || RE_LI.test(l)
    || (l.includes('|') && next != null && RE_TSEP.test(next) && next.includes('-'));
  while (i < lines.length) {
    const line = lines[i];
    let m = RE_FENCE.exec(line);
    if (m) {
      const f = m[1];
      const close = new RegExp(`^\\s*${f[0] === '`' ? '`' : '~'}{${f.length},}\\s*$`);
      const buf = []; i++;
      while (i < lines.length && !close.test(lines[i])) buf.push(lines[i++]);
      i++;
      out.push(mdCodeBlock(buf.join('\n'), m[2]));
      continue;
    }
    if (!line.trim()) { i++; continue; }
    if ((m = /^(#{1,6})\s+(.*?)\s*#*\s*$/.exec(line))) { out.push(`<h${m[1].length}>${mdInline(m[2])}</h${m[1].length}>`); i++; continue; }
    if (RE_HR.test(line)) { out.push('<hr>'); i++; continue; }
    if (/^\s*>/.test(line)) {
      const buf = [];
      while (i < lines.length && /^\s*>/.test(lines[i])) buf.push(lines[i++].replace(/^\s*>\s?/, ''));
      out.push(`<blockquote>${mdBlocks(buf).join('')}</blockquote>`);
      continue;
    }
    if (line.includes('|') && i + 1 < lines.length && RE_TSEP.test(lines[i + 1]) && lines[i + 1].includes('-')) {
      const cells = (l) => l.trim().replace(/^\|/, '').replace(/\|$/, '').split('|').map((c) => c.trim());
      const head = cells(line);
      const al = cells(lines[i + 1]).map((c) => (/^:-+:$/.test(c) ? 'center' : /-+:$/.test(c) ? 'right' : ''));
      i += 2;
      const rows = [];
      while (i < lines.length && lines[i].trim() && lines[i].includes('|')) rows.push(cells(lines[i++]));
      const td = (tag, c, k) => `<${tag}${al[k] ? ` style="text-align:${al[k]}"` : ''}>${mdInline(c)}</${tag}>`;
      out.push(`<div class="table-wrap"><table><thead><tr>${head.map((c, k) => td('th', c, k)).join('')}</tr></thead><tbody>${
        rows.map((r) => `<tr>${head.map((_, k) => td('td', r[k] ?? '', k)).join('')}</tr>`).join('')}</tbody></table></div>`);
      continue;
    }
    if ((m = RE_LI.exec(line))) {
      const base = m[1].length;
      const ordered = /\d/.test(m[2]);
      const items = [];
      while (i < lines.length) {
        const lm = RE_LI.exec(lines[i]);
        if (!lm || lm[1].length !== base) break;
        const it = { text: lm[3], sub: [] };
        items.push(it); i++;
        while (i < lines.length) {
          const l = lines[i];
          if (!l.trim()) {
            const nx = lines.slice(i + 1).find((x) => x.trim());
            if (nx != null && indentOf(nx) > base) { it.sub.push(''); i++; continue; }
            break;
          }
          if (indentOf(l) > base) { it.sub.push(l); i++; continue; }
          break;
        }
      }
      const tag = ordered ? 'ol' : 'ul';
      out.push(`<${tag}>${items.map((it) => {
        let t = it.text; let box = '';
        const tm = /^\[( |x|X)\]\s+/.exec(t);
        if (tm) { box = tm[1] === ' ' ? '☐ ' : '☑ '; t = t.slice(tm[0].length); }
        let subHtml = '';
        if (it.sub.length) {
          const min = Math.min(...it.sub.filter((x) => x.trim()).map(indentOf));
          subHtml = mdBlocks(it.sub.map((x) => x.slice(min))).join('');
        }
        return `<li>${box}${mdInline(t)}${subHtml}</li>`;
      }).join('')}</${tag}>`);
      continue;
    }
    const buf = [line]; i++;
    while (i < lines.length && lines[i].trim() && !startsBlock(lines[i], lines[i + 1])) buf.push(lines[i++]);
    out.push(`<p>${buf.map(mdInline).join('<br>')}</p>`);
  }
  return out;
}
const md = (src) => mdBlocks(String(src ?? '').replace(/\r\n?/g, '\n').split('\n')).join('');

// ───────────────────────── payload helpers (defensive: payload shapes are provider-specific) ─────────────────────────
function pstr(p, ...keys) {
  if (p == null) return '';
  if (typeof p === 'string') return p;
  for (const k of keys) { const v = p[k]; if (typeof v === 'string' && v) return v; }
  return '';
}
function textOf(v) {
  if (v == null) return '';
  if (typeof v === 'string') return v;
  if (Array.isArray(v)) return v.map(textOf).filter(Boolean).join('\n');
  if (typeof v === 'object') {
    if (typeof v.text === 'string') return v.text;
    if (v.content != null) return textOf(v.content);
    return pretty(v);
  }
  return String(v);
}

// ───────────────────────── theme ─────────────────────────
const THEMES = ['auto', 'light', 'dark'];
const THEME_LABEL = { auto: '◐ 自動', light: '☀ ライト', dark: '☾ ダーク' };
function applyTheme() {
  const root = document.documentElement;
  if (S.theme === 'auto') root.removeAttribute('data-theme'); else root.setAttribute('data-theme', S.theme);
  $('#btn-theme').textContent = THEME_LABEL[S.theme];
}

// ───────────────────────── model picker ─────────────────────────
function modelPicker(root, { value, onChange, compact = false } = {}) {
  const prov = h('select', { class: 'sel prov', 'aria-label': 'プロバイダ' });
  const model = h('select', { class: 'sel model', 'aria-label': 'モデル' });
  const modelText = h('input', { class: 'inp model', type: 'text', placeholder: 'モデルID', 'aria-label': 'モデルID', hidden: true, style: 'width:160px' });
  const effort = h('select', { class: 'sel effort', 'aria-label': '推論強度 (effort)' });
  root.replaceChildren(prov, model, modelText, effort);

  const provIds = S.providers.length ? S.providers.map((p) => p.provider) : PROVIDER_FALLBACK;
  for (const id of provIds) {
    const info = S.providers.find((p) => p.provider === id);
    prov.append(h('option', { value: id, text: provLabel(id) + (info && !info.available ? '（未導入）' : '') }));
  }
  const specOf = () => S.providers.find((p) => p.provider === prov.value)?.models?.find((m) => m.id === model.value) || null;
  const fire = () => onChange?.(get());

  function fillModels(pref) {
    const info = S.providers.find((p) => p.provider === prov.value);
    const models = info?.models || [];
    model.replaceChildren();
    if (!models.length) {
      model.hidden = true; modelText.hidden = false; modelText.value = pref || '';
    } else {
      model.hidden = false; modelText.hidden = true;
      for (const m of models) {
        const n = /tier(\d)/.exec(m.tier || '');
        const tag = n ? `[T${n[1]}] ` : '';
        const ex = `${m.is_free ? ' 無料' : ''}${m.is_local ? ' ローカル' : ''}`;
        model.append(h('option', { value: m.id, text: `${tag}${m.label || m.id}${ex}` }));
      }
      if (pref && models.some((m) => m.id === pref)) model.value = pref;
      else if (pref) { model.append(h('option', { value: pref, text: pref })); model.value = pref; }
    }
  }
  function fillEffort(pref) {
    const spec = specOf();
    const levels = spec?.effort_levels?.length ? spec.effort_levels : ['low', 'medium', 'high', 'xhigh'];
    effort.replaceChildren(...levels.map((l) => h('option', { value: l, text: l })));
    const want = pref || spec?.default_effort;
    if (want && levels.includes(want)) effort.value = want;
    else if (levels.includes('high')) effort.value = 'high';
  }
  function get() {
    const m = model.hidden ? modelText.value.trim() : model.value;
    const sel = { provider: prov.value, model: m };
    if (effort.value) sel.effort = effort.value;
    return sel;
  }
  function set(sel) {
    if (!sel) return;
    if (![...prov.options].some((o) => o.value === sel.provider)) prov.append(h('option', { value: sel.provider, text: provLabel(sel.provider) }));
    prov.value = sel.provider;
    fillModels(sel.model);
    fillEffort(sel.effort);
  }
  prov.addEventListener('change', () => { fillModels(); fillEffort(); fire(); });
  model.addEventListener('change', () => { fillEffort(); fire(); });
  modelText.addEventListener('change', fire);
  effort.addEventListener('change', fire);
  set(value || S.meta?.default_model || { provider: provIds[0], model: '' });
  return { get, set };
}

// ───────────────────────── sidebar ─────────────────────────
function projectTitle(id) { return S.projects.find((p) => p.id === id)?.title || ''; }

function renderProjects() {
  const sel = $('#project-select');
  sel.replaceChildren(...S.projects.map((p) => h('option', { value: p.id, text: p.title })));
  if (!S.projects.length) sel.append(h('option', { value: '', text: '（プロジェクトなし）' }));
  if (S.projectId && S.projects.some((p) => p.id === S.projectId)) sel.value = S.projectId;
  else { S.projectId = S.projects[0]?.id || ''; sel.value = S.projectId; }
  ls.set('t3.project', S.projectId);
}

function threadRow(t, { flat = false, kids = [], depthTree = true } = {}) {
  const pend = (S.pending.get(t.id) || []).length;
  const hasKids = !flat && depthTree && kids.length > 0;
  const collapsed = S.collapsed.has(t.id);
  const rel = t.parent_thread_id ? (REL_LABEL[t.relationship] || t.relationship || '子') : null;

  const li = h('li', { class: `trow${t.id === S.threadId ? ' active' : ''}${t.pinned ? ' pinned' : ''}${t.archived ? ' archived' : ''}`, dataset: { id: t.id } });
  const caret = h('button', {
    class: `caret${hasKids ? '' : ' none'}`, type: 'button', 'aria-label': collapsed ? '展開' : '折りたたむ', text: collapsed ? '▶' : '▼',
    onclick: (e) => { e.stopPropagation(); collapsed ? S.collapsed.delete(t.id) : S.collapsed.add(t.id); renderTree(); },
  });
  const parent = flat && t.parent_thread_id ? threadById(t.parent_thread_id) : null;
  const sub = h('div', { class: 'trow-sub' },
    tierBadge(t.model_selection),
    h('span', { class: 'badge prov', text: provLabel(t.model_selection?.provider) }),
    rel ? h('span', { class: 'badge rel', text: rel }) : null,
    flat && t.project_id !== S.projectId ? h('span', { class: 'badge', text: clip(projectTitle(t.project_id), 12) }) : null,
    parent ? h('span', { text: `↳ ${clip(parent.title, 14)}` }) : null,
    h('span', { text: STATUS_LABEL[t.status] || t.status }),
    !flat ? h('span', { text: fmtAgo(t.updated_at) }) : null,
    hasKids && collapsed ? h('span', { class: 'badge', text: `子 ${kids.length}` }) : null);

  const act = (ic, title, fn) => {
    const b = h('button', { class: 'ibtn sm', type: 'button', title, 'aria-label': title, onclick: (e) => { e.stopPropagation(); fn(); } });
    b.innerHTML = icon(ic);
    return b;
  };
  const main = h('div', { class: 'trow-main', onclick: () => openThread(t.id) },
    caret,
    h('span', { class: `st st-${t.status}`, title: STATUS_LABEL[t.status] || t.status }),
    h('div', { class: 'trow-body' }, h('span', { class: 'trow-title', text: t.title || '(無題)' }), sub),
    pend ? h('span', { class: 'pend-mark', title: '承認待ち', text: '⚠' }) : null,
    h('div', { class: 'trow-actions' },
      act('pin', t.pinned ? 'ピン留め解除' : 'ピン留め', () => patchThread(t.id, { pinned: !t.pinned })),
      act('archive', t.archived ? 'アーカイブ解除' : 'アーカイブ', () => patchThread(t.id, { archived: !t.archived }))));
  li.append(main);
  if (hasKids && !collapsed) {
    li.append(h('ul', { class: 'tlist' }, ...kids.map((k) => threadRow(k.t, { kids: k.kids }))));
  }
  return li;
}

function buildTree(list) {
  const ids = new Set(list.map((t) => t.id));
  const byParent = new Map();
  for (const t of list) {
    const p = t.parent_thread_id && ids.has(t.parent_thread_id) ? t.parent_thread_id : null;
    if (!byParent.has(p)) byParent.set(p, []);
    byParent.get(p).push(t);
  }
  const cmp = (a, b) => (b.pinned - a.pinned) || (Date.parse(b.updated_at) - Date.parse(a.updated_at));
  const mk = (t, seen) => {
    seen.add(t.id);
    return { t, kids: (byParent.get(t.id) || []).filter((k) => !seen.has(k.id)).sort(cmp).map((k) => mk(k, seen)) };
  };
  return (byParent.get(null) || []).sort(cmp).map((t) => mk(t, new Set()));
}

function renderTree() {
  const list = S.threads.filter((t) => t.project_id === S.projectId && (S.showArchived || !t.archived));
  const ul = $('#thread-list');
  ul.replaceChildren();
  if (!list.length) ul.append(h('li', { class: 'empty-note', text: S.projectId ? 'スレッドはまだありません' : 'プロジェクトを追加してください' }));
  for (const n of buildTree(list)) ul.append(threadRow(n.t, { kids: n.kids }));
}

function renderShelf() {
  const shelf = S.threads.filter((t) => !t.archived && (ACTIVE_SHELF.has(t.status) || (S.pending.get(t.id) || []).length));
  shelf.sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at));
  const ul = $('#shelf-list');
  ul.replaceChildren();
  if (!shelf.length) ul.append(h('li', { class: 'empty-note', text: '稼働中のスレッドはありません' }));
  for (const t of shelf) ul.append(threadRow(t, { flat: true }));
  const c = $('#shelf-count');
  c.textContent = String(shelf.length);
  c.classList.toggle('live', shelf.length > 0);
  const pend = [...S.pending.values()].some((a) => a.length);
  document.title = `${pend ? '⚠ ' : shelf.length ? `● ${shelf.length} ` : ''}t3code-rs`;
}

const renderSidebar = () => { renderShelf(); renderTree(); };
const renderSidebarSoon = rafThrottle(renderSidebar);

async function loadThreads() {
  let list;
  const q = 'include_archived=true&limit=500';
  try {
    list = await api(`/api/threads?${q}`);
  } catch (e) {
    if (e.status !== 400 && e.status !== 422) throw e;
    list = (await Promise.all(S.projects.map((p) => api(`/api/threads?project_id=${p.id}&${q}`)))).flat();
  }
  S.threads = Array.isArray(list) ? list : [];
  syncSubscriptions();
  renderSidebar();
}
const loadThreadsSoon = debounce(() => loadThreads().catch(() => {}), 300);

async function loadProjects() {
  S.projects = await api('/api/projects');
  renderProjects();
}

async function patchThread(id, body) {
  try {
    const t = await api(`/api/threads/${id}`, { method: 'PATCH', json: body });
    const i = S.threads.findIndex((x) => x.id === id);
    if (i >= 0 && t?.id) S.threads[i] = t;
    if (S.detail?.thread?.id === id && t?.id) S.detail.thread = t;
    await loadThreads();
    renderHeader();
  } catch (e) { toast(`更新に失敗: ${e.message}`, 'err'); }
}

// ───────────────────────── thread open / detail ─────────────────────────
async function openThread(id, { push = true } = {}) {
  if (!id) return;
  closeSidebar();
  if (push) { try { history.replaceState(null, '', `#/t/${id}`); } catch { location.hash = `#/t/${id}`; } }
  S.threadId = id;
  S.stick = true;
  S.atts = []; renderAtts();
  syncSubscriptions();
  await loadDetail(id, { reset: true });
}

async function loadDetail(id, { reset = false } = {}) {
  let d;
  try { d = await api(`/api/threads/${id}`); } catch (e) {
    if (id === S.threadId) { toast(`スレッドを読み込めません: ${e.message}`, 'err'); if (e.status === 404) { S.threadId = null; S.detail = null; renderAll(); } }
    return;
  }
  if (id !== S.threadId) return;
  S.detail = d;
  pruneOptimistic(d);
  if (d.thread.project_id !== S.projectId) { S.projectId = d.thread.project_id; renderProjects(); }
  const i = S.threads.findIndex((t) => t.id === id);
  if (i >= 0) S.threads[i] = d.thread; else S.threads.push(d.thread);
  renderAll();
  if (reset) {
    composerPicker?.set(d.thread.model_selection);
    scrollBottom(true);
  }
  if (S.diffOpen) loadDiff();
}
function pruneOptimistic(d) {
  if (!S._optimistic?.length) return;
  const users = d.items.filter((x) => x.kind === 'user_message');
  S._optimistic = S._optimistic.filter((o) => {
    if (o.threadId !== d.thread.id) return true;
    return !users.some((u) => Date.parse(u.created_at) >= Number(o.key) - 10000
      && (o.text.startsWith('📎') || pstr(u.payload, 'text', 'prompt', 'content', 'message').trim() === o.text));
  });
}
const loadDetailSoon = debounce(() => { if (S.threadId) loadDetail(S.threadId); }, 250);

function renderAll() {
  renderSidebar();
  renderHeader();
  renderTimeline();
  renderApprovals();
  renderComposer();
}

function renderHeader() {
  const t = curThread();
  const tb = $('#thread-title');
  const meta = $('#thread-meta');
  meta.replaceChildren();
  const cb = $('#children-bar');
  if (!t) {
    tb.textContent = 'スレッドを選択してください'; tb.disabled = true;
    cb.hidden = true; return;
  }
  tb.disabled = false;
  tb.textContent = t.title || '(無題)';
  const parent = t.parent_thread_id ? threadById(t.parent_thread_id) : null;
  if (t.parent_thread_id) {
    meta.append(h('button', { class: 'link', type: 'button', text: `↑ 親: ${clip(parent?.title || t.parent_thread_id.slice(0, 8), 24)}`, onclick: () => openThread(t.parent_thread_id) }));
  }
  const running = t.status === 'running';
  meta.append(...[
    h('span', { class: `st st-${t.status}` }),
    h('span', { id: 'status-text', text: STATUS_LABEL[t.status] || t.status }),
    h('span', { class: 'muted', id: 'run-timer' }),
    tierBadge(t.model_selection, true),
    h('span', { text: `${provLabel(t.model_selection?.provider)} · ${t.model_selection?.model}${t.model_selection?.effort ? ` · ${t.model_selection.effort}` : ''}` }),
    t.runtime_mode ? h('span', { class: 'badge', text: MODE_LABEL[t.runtime_mode] || t.runtime_mode }) : null,
    t.interaction_mode === 'plan' ? h('span', { class: 'badge warn', text: '計画モード' }) : null,
    t.branch ? h('span', { class: 'badge', title: t.worktree_path || '', text: `⎇ ${t.branch}` }) : null,
    t.worktree_path ? h('span', { class: 'badge', text: 'worktree' }) : null].filter(Boolean));
  if (running) tickTimer();

  const kids = S.detail.children || [];
  cb.hidden = !kids.length;
  cb.replaceChildren(h('span', { class: 'muted', text: `子スレッド ${kids.length}` }),
    ...kids.map((k) => h('button', { class: 'chip', type: 'button', dataset: { child: k.id }, onclick: () => openThread(k.id) },
      h('span', { class: `st st-${k.status}` }), tierBadge(k.model_selection), h('span', { text: clip(k.title, 28) }))));
}

function tickTimer() {
  const el = $('#run-timer');
  const run = latestRun();
  if (!el) return;
  if (run && run.status === 'running') el.textContent = `· ${fmtDur(Date.now() - Date.parse(run.started_at))}`;
  else el.textContent = '';
}
setInterval(() => { if (curThread()?.status === 'running') tickTimer(); }, 1000);
const latestRun = () => { const r = S.detail?.runs || []; return r.length ? r[r.length - 1] : null; };

// ───────────────────────── timeline ─────────────────────────
const tl = () => $('#timeline');
const scroller = () => $('#tl-scroll');

function nearBottom() { const s = scroller(); return s.scrollHeight - s.scrollTop - s.clientHeight < 140; }
function scrollBottom(force) {
  const s = scroller();
  if (force || S.stick) { s.scrollTop = s.scrollHeight; S.stick = true; }
  $('#btn-jump').hidden = S.stick;
}

function runOf(id) { return (S.detail?.runs || []).find((r) => r.id === id); }

function runDivider(run) {
  if (!run) return null;
  const parts = [
    h('span', { text: `Run #${run.ordinal}` }),
    h('span', { text: `${provLabel(run.provider)} · ${run.model}${run.effort ? ` · ${run.effort}` : ''}` }),
    h('span', { class: `st st-${run.status}` }),
    h('span', { text: STATUS_LABEL[run.status] || run.status }),
  ];
  if (run.duration_ms != null) parts.push(h('span', { text: fmtDur(run.duration_ms) }));
  if (run.input_tokens != null || run.output_tokens != null) parts.push(h('span', { text: `↑${fmtTok(run.input_tokens)} ↓${fmtTok(run.output_tokens)} tok` }));
  if (run.error_message) parts.push(h('span', { class: 'badge bad', title: run.error_message, text: clip(run.error_message, 60) }));
  return h('div', { class: 'run-div', dataset: { run: run.id } }, h('span', { class: 'mid' }, parts));
}

function attThumb(id) {
  const a = (S.detail?.attachments || []).find((x) => x.id === id);
  const name = a?.filename || '添付';
  if (a && !/^image\//.test(a.mime_type || '')) {
    return h('a', { class: 'att-file', href: '#', text: `📄 ${name}`, onclick: async (e) => { e.preventDefault(); try { window.open(await attachmentBlobUrl(id), '_blank', 'noopener'); } catch { toast('添付を開けません', 'err'); } } });
  }
  const img = h('img', { class: 'att-thumb', alt: name, title: name, loading: 'lazy' });
  attachmentBlobUrl(id).then((u) => { img.src = u; img.onclick = () => window.open(u, '_blank', 'noopener'); }).catch(() => { img.alt = `${name}（読み込み失敗）`; });
  return img;
}

function fileEntries(p) {
  const raw = p?.files ?? p?.changes ?? p?.paths ?? (p?.path ? [p] : []);
  const arr = Array.isArray(raw) ? raw : typeof raw === 'object' && raw ? Object.entries(raw).map(([path, v]) => ({ path, ...(typeof v === 'object' ? v : { kind: v }) })) : [];
  return arr.map((f) => (typeof f === 'string' ? { path: f } : f)).filter((f) => f && (f.path || f.file || f.name));
}
function kindLetter(k) {
  k = String(k || 'M').toLowerCase();
  if (/^(a|add|new|creat|untracked)/.test(k)) return 'A';
  if (/^(d|del|remov)/.test(k)) return 'D';
  if (/^(r|ren|mov)/.test(k)) return 'R';
  return 'M';
}

function renderItem(item) {
  const p = item.payload;
  const kind = item.kind;
  const sum = item.summary || '';
  const id = { dataset: { item: item.id } };
  switch (kind) {
    case 'user_message': {
      const text = pstr(p, 'text', 'prompt', 'content', 'message') || sum;
      const ids = Array.isArray(p?.attachment_ids) ? p.attachment_ids : [];
      const el = h('div', { class: 'msg user', ...id }, text);
      if (ids.length) el.append(h('div', { class: 'msg-atts' }, ids.map(attThumb)));
      return el;
    }
    case 'assistant_message': {
      const text = pstr(p, 'text', 'content', 'message') || sum;
      const el = h('div', { class: 'msg assistant', ...id }, h('div', { class: 'md' }));
      el.firstChild.innerHTML = md(text);
      const cp = h('button', { class: 'ibtn sm copy-msg', type: 'button', title: 'メッセージをコピー', 'aria-label': 'コピー', onclick: () => copyText(text) });
      cp.innerHTML = icon('copy');
      el.append(cp);
      return el;
    }
    case 'reasoning': {
      const text = pstr(p, 'text', 'content', 'summary') || sum || textOf(p);
      return h('details', { class: 'reasoning', ...id }, h('summary', { text: `思考 (${text.length.toLocaleString('ja-JP')}文字)` }), h('pre', { text }));
    }
    case 'tool_call': {
      const name = pstr(p, 'name', 'tool', 'tool_name', 'title') || sum || 'tool';
      const args = p?.input ?? p?.arguments ?? p?.args ?? p?.params ?? p;
      const short = clip(textOf(typeof args === 'string' ? args : (args?.command ?? args?.path ?? args?.file_path ?? args?.query ?? args?.pattern ?? sum ?? '')) || pretty(args), 140).replace(/\s+/g, ' ');
      return h('details', { class: 'card tool', ...id }, h('summary', {}, h('span', { class: 'k', text: `🛠 ${name}` }), h('span', { class: 'v', text: short })),
        h('div', { class: 'cbody' }, h('pre', { text: pretty(args) })));
    }
    case 'tool_result': {
      const out = textOf(p?.output ?? p?.content ?? p?.result ?? p?.text ?? p) || sum;
      const bad = p?.is_error || p?.error;
      const d = h('details', { class: 'card res', ...id }, h('summary', {}, h('span', { class: 'k', text: bad ? '✗ 結果' : '↳ 結果' }), h('span', { class: 'v', text: clip(out.replace(/\s+/g, ' '), 140) })),
        h('div', { class: 'cbody' }, h('pre', { text: clip(out, 20000) })));
      return d;
    }
    case 'command_execution': {
      const cmd = pstr(p, 'command', 'cmd', 'title') || sum;
      const out = [textOf(p?.output ?? p?.stdout ?? p?.aggregated_output), textOf(p?.stderr)].filter(Boolean).join('\n');
      const code = p?.exit_code ?? p?.exitCode ?? p?.code;
      const badge = code == null ? null : h('span', { class: `badge ${Number(code) === 0 ? 'good' : 'bad'}`, text: `exit ${code}` });
      if (!out) return h('div', { class: 'card cmd', ...id }, h('div', { class: 'card-h' }, h('span', { class: 'k', text: '$' }), h('span', { class: 'v', text: cmd }), badge));
      return h('details', { class: 'card cmd', ...id }, h('summary', {}, h('span', { class: 'k', text: '$' }), h('span', { class: 'v', text: cmd }), badge),
        h('div', { class: 'cbody' }, h('pre', { text: clip(out, 40000) })));
    }
    case 'file_change': {
      const files = fileEntries(p);
      const wrap = h('div', { class: 'card fchg', ...id }, h('div', { class: 'card-h' }, h('span', { class: 'k', text: '✎ ファイル変更' }), h('span', { class: 'v', text: sum || `${files.length}件` })));
      if (files.length) {
        wrap.append(h('div', { class: 'files' }, files.slice(0, 60).map((f) => {
          const L = kindLetter(f.kind ?? f.status ?? f.change_type);
          const add = f.additions ?? f.added; const del = f.deletions ?? f.removed;
          return h('button', { class: 'fchip', type: 'button', title: 'Git 差分を開く', onclick: () => setDiffOpen(true) },
            h('span', { class: `fk ${L}`, text: L }), h('span', { class: 'p', text: f.path || f.file || f.name }),
            add != null ? h('span', { class: 'plus', text: `+${add}` }) : null, del != null ? h('span', { class: 'minus', text: `−${del}` }) : null);
        })));
      } else if (p && Object.keys(p).length) wrap.append(h('div', { class: 'cbody' }, h('pre', { text: pretty(p) })));
      return wrap;
    }
    case 'subagent': {
      const cid = pstr(p, 'child_thread_id', 'thread_id', 'id');
      const child = cid && threadById(cid);
      const st = child?.status || pstr(p, 'status') || 'idle';
      const title = child?.title || pstr(p, 'title', 'name', 'task') || sum || 'サブエージェント';
      const sel = child?.model_selection || (p?.provider ? { provider: p.provider, model: p.model } : null);
      return h('div', { class: 'card sub', dataset: { item: item.id, child: cid } }, h('div', { class: 'card-h' },
        h('span', { class: `st st-${st}` }), h('span', { class: 'k', text: '子エージェント' }), tierBadge(sel),
        sel ? h('span', { class: 'badge prov', text: provLabel(sel.provider) }) : null,
        h('span', { class: 'v', text: title }), h('span', { class: 'sub-status muted', text: STATUS_LABEL[st] || st }),
        cid ? h('button', { class: 'btn sm', type: 'button', text: '開く', onclick: () => openThread(cid) }) : null));
    }
    case 'handoff': {
      const text = pstr(p, 'summary', 'text', 'content') || sum || textOf(p);
      const el = h('details', { class: 'card handoff', ...id }, h('summary', {}, h('span', { class: 'k', text: '⇄ コンテキスト引き継ぎ' }), h('span', { class: 'v', text: clip(text.replace(/\s+/g, ' '), 100) })), h('div', { class: 'cbody' }, h('div', { class: 'md' })));
      el.querySelector('.md').innerHTML = md(text);
      return el;
    }
    case 'system_notice': {
      const text = pstr(p, 'message', 'text', 'content') || sum || textOf(p);
      return h('div', { class: `notice${/error|fail|エラー|失敗/i.test(text) ? ' err' : ''}`, ...id, text });
    }
    default:
      return h('details', { class: 'card', ...id }, h('summary', {}, h('span', { class: 'k', text: kind }), h('span', { class: 'v', text: sum })), h('div', { class: 'cbody' }, h('pre', { text: pretty(p) })));
  }
}

let renderedRun = null;
function appendItem(item, container = tl()) {
  if (item.run_id !== renderedRun) {
    renderedRun = item.run_id;
    const d = runDivider(runOf(item.run_id));
    if (d) container.append(d);
  }
  container.append(renderItem(item));
}

function renderTimeline() {
  const root = tl();
  const d = S.detail;
  const empty = $('#empty');
  root.replaceChildren();
  renderedRun = null;
  if (!d) {
    empty.replaceChildren(
      h('h2', { text: 't3code-rs へようこそ' }),
      h('p', { text: 'スレッドを選ぶか、下の入力欄にメッセージを書いて送信すると新しいスレッドを作成します。' }),
      h('p', { class: 'muted', text: 'Ctrl/Cmd+K で全文検索（日本語対応）。テレメトリ・認証情報の読み取りは一切ありません。' }),
      h('div', { class: 'row' },
        h('button', { class: 'btn primary', type: 'button', text: '＋ 新規スレッド', onclick: () => newThreadDialog() }),
        h('button', { class: 'btn', type: 'button', text: '全文検索', onclick: () => searchDialog() })));
    return;
  }
  empty.replaceChildren();
  // incoming handoff (this thread was forked from another)
  for (const ho of (d.handoffs || []).filter((x) => x.target_thread_id === d.thread.id)) {
    const src = threadById(ho.source_thread_id);
    const box = h('details', { class: 'card handoff' }, h('summary', {},
      h('span', { class: 'k', text: '⇄ 引き継ぎ元' }),
      h('span', { class: 'v', text: `${src?.title || ho.source_thread_id.slice(0, 8)} （${provLabel(ho.from_model?.provider)} ${ho.from_model?.model} → ${provLabel(ho.to_model?.provider)} ${ho.to_model?.model}）` }),
      h('button', { class: 'btn sm', type: 'button', text: '元を開く', onclick: (e) => { e.preventDefault(); openThread(ho.source_thread_id); } })),
    h('div', { class: 'cbody' }, h('div', { class: 'md' })));
    box.querySelector('.md').innerHTML = md(ho.summary || '');
    root.append(box);
  }
  const frag = document.createDocumentFragment();
  const items = [...(d.items || [])].sort((a, b) => a.ordinal - b.ordinal);
  for (const it of items) appendItem(it, frag);
  root.append(frag);
  // runs without items yet (e.g. just started)
  const lr = latestRun();
  if (lr && lr.id !== renderedRun && (lr.status === 'running' || lr.status === 'queued')) { const dv = runDivider(lr); if (dv) root.append(dv); renderedRun = lr.id; }
  // optimistic bubbles & live blocks
  for (const o of S._optimistic || []) if (o.threadId === d.thread.id) root.append(optimisticEl(o));
  root.append(h('div', { id: 'live' }));
  renderLive();
  scrollBottom();
}

function optimisticEl(o) { return h('div', { class: 'msg user pending', dataset: { opt: o.key }, text: o.text }); }

const renderLive = rafThrottle(() => {
  const host = $('#live');
  const t = curThread();
  if (!host || !t) return;
  const live = S.live.get(t.id);
  const stick = S.stick;
  let reas = host.querySelector('.reasoning');
  let asst = host.querySelector('.msg.assistant');
  let typing = host.querySelector('.typing');
  const hasR = live?.reasoning, hasA = live?.assistant;
  if (hasR) {
    if (!reas) { reas = h('details', { class: 'reasoning live', open: true }, h('summary', {}), h('pre')); host.prepend(reas); }
    reas.querySelector('summary').textContent = `思考中… (${live.reasoning.length.toLocaleString('ja-JP')}文字)`;
    const pre = reas.querySelector('pre'); pre.textContent = live.reasoning; pre.scrollTop = pre.scrollHeight;
  } else reas?.remove();
  if (hasA) {
    if (!asst) { asst = h('div', { class: 'msg assistant live' }, h('div', { class: 'md' })); host.append(asst); }
    asst.firstChild.innerHTML = md(live.assistant);
  } else asst?.remove();
  const showTyping = t.status === 'running' && !hasA && !hasR;
  if (showTyping && !typing) host.append(h('div', { class: 'typing' }, h('span', { class: 'st st-running' }), '考え中…'));
  if (!showTyping) typing?.remove();
  if (stick) scrollBottom();
}) ;

// ───────────────────────── approvals ─────────────────────────
function renderApprovals() {
  const box = $('#approval');
  box.replaceChildren();
  const t = curThread();
  if (!t) return;
  for (const req of S.pending.get(t.id) || []) {
    const p = req.payload;
    const cmd = pstr(p, 'command', 'cmd', 'path', 'file_path', 'description');
    const btns = [];
    const act = async (approved) => {
      btns.forEach((b) => { b.disabled = true; });
      try {
        await api(`/api/threads/${t.id}/respond`, { method: 'POST', json: { request_id: req.id, approved, payload: {} } });
        dropPending(t.id, req.id);
        toast(approved ? '承認しました' : '拒否しました', approved ? 'ok' : '');
      } catch (e) { btns.forEach((b) => { b.disabled = false; }); toast(`応答に失敗: ${e.message}`, 'err'); }
    };
    const ok = h('button', { class: 'btn ok', type: 'button', text: '承認 (Approve)', onclick: () => act(true) });
    const no = h('button', { class: 'btn danger', type: 'button', text: '拒否 (Reject)', onclick: () => act(false) });
    btns.push(ok, no);
    box.append(h('div', { class: 'approval', role: 'alert' },
      h('div', { class: 'ah' }, h('span', { text: '⚠' }), h('span', { class: 't', text: req.title || '承認が必要です' }), h('span', { class: 'badge warn', text: req.kind })),
      cmd ? h('pre', { text: cmd }) : (p && Object.keys(p || {}).length ? h('pre', { text: pretty(p) }) : null),
      h('div', { class: 'ab' }, ok, no)));
  }
}
function dropPending(threadId, reqId) {
  const a = (S.pending.get(threadId) || []).filter((r) => r.id !== reqId);
  if (a.length) S.pending.set(threadId, a); else S.pending.delete(threadId);
  renderApprovals(); renderSidebarSoon();
}

// ───────────────────────── composer ─────────────────────────
let composerPicker = null;
const ta = () => $('#prompt');

function renderComposer() {
  const t = curThread();
  const busy = t ? isBusy(t.status) : false;
  $$('.run-only').forEach((el) => { el.hidden = !busy; });
  $$('#behavior button').forEach((b) => { const on = b.dataset.b === S.behavior; b.classList.toggle('on', on); b.setAttribute('aria-checked', String(on)); });
  $('#send-lbl').textContent = !busy ? '送信' : S.behavior === 'steer' ? '割り込み送信' : '予約送信';
  $('#btn-fork').disabled = !t;
  $('#btn-delegate').disabled = !t;
  $('#btn-stop').disabled = !t || !busy;
}

function autosize() { const el = ta(); el.style.height = 'auto'; el.style.height = `${Math.min(el.scrollHeight, window.innerHeight * 0.4)}px`; }

function renderAtts() {
  const box = $('#att-chips');
  box.hidden = !S.atts.length;
  box.replaceChildren(...S.atts.map((a) => h('span', { class: `achip${a.uploading ? ' up' : ''}`, title: a.name },
    a.local ? h('img', { src: a.local, alt: '' }) : h('span', { text: '📄' }),
    h('span', { class: 'n', text: a.uploading ? `${a.name} …` : a.name }),
    h('small', { text: fmtBytes(a.size) }),
    h('button', { type: 'button', 'aria-label': '削除', text: '×', onclick: () => { S.atts = S.atts.filter((x) => x !== a); if (a.local) URL.revokeObjectURL(a.local); renderAtts(); } }))));
}

async function addFiles(files) {
  for (const f of files) {
    const a = { name: f.name || 'clipboard.png', size: f.size, mime: f.type, uploading: true, id: null, local: /^image\//.test(f.type) ? URL.createObjectURL(f) : null };
    S.atts.push(a); renderAtts();
    const fd = new FormData();
    fd.append('file', f, a.name);
    if (S.threadId) fd.append('thread_id', S.threadId);
    try {
      const res = await api('/api/attachments', { method: 'POST', form: fd });
      a.id = res.id; a.uploading = false;
    } catch (e) {
      S.atts = S.atts.filter((x) => x !== a);
      toast(`アップロード失敗 (${a.name}): ${e.message}`, 'err');
    }
    renderAtts();
  }
}

let sending = false;
async function send() {
  if (sending) return;
  const input = ta();
  const text = input.value.trim();
  if (!text && !S.atts.length) return;
  if (S.atts.some((a) => a.uploading)) { toast('添付のアップロード完了を待っています…'); return; }
  if (!S.projectId) { toast('先にプロジェクトを追加してください', 'err'); projectDialog(); return; }
  sending = true;
  const atts = S.atts.slice();
  const sel = composerPicker.get();
  try {
    let t = curThread();
    if (!t) {
      if (!sel.model) { toast('モデルを選択してください', 'err'); return; }
      t = await createThread({ title: clip(text.replace(/\s+/g, ' '), 40) || '新規スレッド', model_selection: sel });
      await openThread(t.id);
    }
    const busy = isBusy(t.status);
    const body = { prompt: text };
    if (busy) body.behavior = S.behavior;
    if (atts.length) body.attachment_ids = atts.map((a) => a.id);
    if (JSON.stringify(sel) !== JSON.stringify(t.model_selection)) body.model_selection = sel;
    input.value = ''; autosize();
    S.atts = []; renderAtts();
    const key = String(Date.now());
    (S._optimistic ||= []).push({ key, threadId: t.id, text: text || `📎 ${atts.length}件の添付` });
    S.stick = true;
    const live = $('#live');
    live?.before(optimisticEl(S._optimistic.at(-1))); scrollBottom(true);
    try {
      await api(`/api/threads/${t.id}/messages`, { method: 'POST', json: body });
      loadDetailSoon();
    } catch (e) {
      S._optimistic = S._optimistic.filter((o) => o.key !== key);
      $(`[data-opt="${key}"]`)?.remove();
      input.value = text; autosize(); S.atts = atts; renderAtts();
      toast(`送信に失敗: ${e.message}`, 'err');
    }
  } catch (e) {
    toast(`スレッド作成に失敗: ${e.message}`, 'err');
  } finally { sending = false; }
}

async function createThread(o) {
  const body = {
    project_id: S.projectId, title: o.title || '新規スレッド', model_selection: o.model_selection,
    runtime_mode: o.runtime_mode || 'approval-required', interaction_mode: o.interaction_mode || 'default',
    isolation: o.isolation || 'shared', base_ref: o.base_ref || null,
  };
  const r = await api('/api/threads', { method: 'POST', json: body });
  const t = r.thread || r;
  S.threads.unshift(t);
  renderSidebarSoon();
  return t;
}

async function interrupt() {
  const t = curThread(); if (!t) return;
  try { await api(`/api/threads/${t.id}/interrupt`, { method: 'POST' }); toast('停止を要求しました'); } catch (e) { toast(`停止に失敗: ${e.message}`, 'err'); }
}

async function copyText(text) {
  try { await navigator.clipboard.writeText(text); toast('コピーしました', 'ok', 1500); return; } catch { /* fall through */ }
  const x = h('textarea', { value: text, style: 'position:fixed;opacity:0' });
  document.body.append(x); x.select();
  try { document.execCommand('copy'); toast('コピーしました', 'ok', 1500); } catch { toast('コピーできません', 'err'); }
  x.remove();
}

// ───────────────────────── dialogs ─────────────────────────
function field(label, control, hint) {
  return h('div', { class: 'field' }, h('label', { text: label }), control, hint ? h('div', { class: 'hint', text: hint }) : null);
}
function radios(name, opts, value) {
  const box = h('div', { class: 'radios' });
  for (const [v, l] of opts) box.append(h('label', {}, h('input', { type: 'radio', name, value: v, checked: v === value }), l));
  box.value = () => box.querySelector('input:checked')?.value;
  return box;
}

function projectDialog() {
  openModal('プロジェクト', (body, close) => {
    const title = h('input', { class: 'inp', placeholder: '例: t3code-rs', required: true });
    const root = h('input', { class: 'inp', placeholder: '/Users/you/src/project', required: true, autocapitalize: 'off', spellcheck: false });
    const list = h('div', { class: 'form' });
    const renderList = () => {
      list.replaceChildren(h('div', { class: 'lab muted', text: `登録済み (${S.projects.length})` }), ...S.projects.map((p) => h('div', { class: 'row2', style: 'align-items:center;flex-wrap:nowrap' },
        h('div', { style: 'min-width:0;flex:1' }, h('div', { text: p.title, style: 'font-weight:600' }), h('div', { class: 'muted', style: 'font-size:12px;word-break:break-all', text: p.workspace_root })),
        h('button', {
          class: 'btn sm danger', type: 'button', text: '削除',
          onclick: async () => {
            if (!confirm(`プロジェクト「${p.title}」を削除しますか？（ファイルは削除されません）`)) return;
            try { await api(`/api/projects/${p.id}`, { method: 'DELETE' }); await loadProjects(); await loadThreads(); renderList(); } catch (e) { toast(`削除に失敗: ${e.message}`, 'err'); }
          },
        }))));
    };
    renderList();
    body.append(h('form', {
      class: 'form',
      onsubmit: async (e) => {
        e.preventDefault();
        try {
          const p = await api('/api/projects', { method: 'POST', json: { title: title.value.trim(), workspace_root: root.value.trim() } });
          await loadProjects();
          S.projectId = p.id; renderProjects(); renderTree(); close(); toast('プロジェクトを追加しました', 'ok');
        } catch (err) { toast(`追加に失敗: ${err.message}`, 'err'); }
      },
    }, h('div', { class: 'lab', style: 'font-weight:700', text: '＋ プロジェクト追加' }), field('名前', title), field('ワークスペースのパス', root, 'サーバーから見える絶対パス（git リポジトリ推奨）'),
    h('div', { class: 'actions' }, h('button', { class: 'btn primary', type: 'submit', text: '追加' }))),
    h('hr', { style: 'border:0;border-top:1px solid var(--border);margin:16px 0' }), list);
    setTimeout(() => title.focus(), 0);
  });
}

function newThreadDialog() {
  if (!S.projectId) { projectDialog(); return; }
  openModal('新規スレッド', (body, close) => {
    const title = h('input', { class: 'inp', placeholder: '空欄なら最初のメッセージから自動設定' });
    const pickRoot = h('div', { class: 'picker' });
    const picker = modelPicker(pickRoot, { value: composerPicker?.get() });
    const mode = h('select', { class: 'sel' }, ...Object.entries(MODE_LABEL).map(([v, l]) => h('option', { value: v, text: l })));
    mode.value = 'approval-required';
    const inter = h('select', { class: 'sel' }, h('option', { value: 'default', text: '通常' }), h('option', { value: 'plan', text: '計画 (Plan)' }));
    const iso = radios('iso', [['shared', '共有 (shared)'], ['worktree', 'git worktree']], 'shared');
    const base = h('input', { class: 'inp', placeholder: '例: main（空欄で現在のブランチ）', hidden: true });
    iso.addEventListener('change', () => { base.hidden = iso.value() !== 'worktree'; });
    body.append(h('form', {
      class: 'form',
      onsubmit: async (e) => {
        e.preventDefault();
        const sel = picker.get();
        if (!sel.model) { toast('モデルを選択してください', 'err'); return; }
        try {
          const t = await createThread({ title: title.value.trim(), model_selection: sel, runtime_mode: mode.value, interaction_mode: inter.value, isolation: iso.value(), base_ref: base.value.trim() });
          close(); await openThread(t.id); ta().focus();
        } catch (err) { toast(`作成に失敗: ${err.message}`, 'err'); }
      },
    }, field('タイトル（任意）', title), field('プロバイダ / モデル / effort', pickRoot),
    h('div', { class: 'row2' }, field('実行権限', mode), field('モード', inter)),
    field('作業ツリー', h('div', {}, iso, base)),
    h('div', { class: 'actions' }, h('button', { class: 'btn primary', type: 'submit', text: '作成' }))));
    setTimeout(() => title.focus(), 0);
  });
}

function forkDialog() {
  const t = curThread(); if (!t) return;
  openModal('分岐・モデル切替', (body, close) => {
    const title = h('input', { class: 'inp', value: `${t.title} (分岐)` });
    const pickRoot = h('div', { class: 'picker' });
    const picker = modelPicker(pickRoot, { value: t.model_selection });
    const memo = h('textarea', { class: 'inp', rows: 4, placeholder: '空欄ならサーバー側が会話履歴から引き継ぎ要約を作成します' });
    const go = h('button', { class: 'btn primary', type: 'submit', text: '分岐して開く' });
    body.append(h('form', {
      class: 'form',
      onsubmit: async (e) => {
        e.preventDefault(); go.disabled = true;
        const req = { title: title.value.trim() || undefined, target_model: picker.get() };
        if (memo.value.trim()) req.handoff_summary = memo.value.trim();
        try {
          const r = await api(`/api/threads/${t.id}/fork`, { method: 'POST', json: req });
          const nt = r.thread || r;
          close(); toast('分岐しました', 'ok');
          await loadThreads(); await openThread(nt.id);
        } catch (err) { go.disabled = false; toast(`分岐に失敗: ${err.message}`, 'err'); }
      },
    }, h('p', { class: 'muted', style: 'margin:0', text: `現在: ${provLabel(t.model_selection?.provider)} / ${t.model_selection?.model}。会話の文脈を引き継いだ新しいスレッドを、選んだプロバイダ・モデルで作ります。元のスレッドはそのまま残ります。` }),
    field('新スレッドのタイトル', title), field('分岐先 プロバイダ / モデル / effort', pickRoot), field('引き継ぎメモ（任意）', memo),
    h('div', { class: 'actions' }, go)));
  });
}

function delegateDialog() {
  const t = curThread(); if (!t) return;
  openModal('並列委譲 (子スレッド)', (body, close) => {
    const draft = ta().value.trim();
    const title = h('input', { class: 'inp', placeholder: '例: テストを書く' });
    const task = h('textarea', { class: 'inp', rows: 5, required: true, placeholder: '子エージェントへの指示（背景・完了条件を含める）', value: draft });
    const pickRoot = h('div', { class: 'picker' });
    const picker = modelPicker(pickRoot, { value: t.model_selection });
    const iso = radios('diso', [['shared', '共有 (shared)'], ['worktree', 'git worktree（並列編集向け）']], 'shared');
    const base = h('input', { class: 'inp', placeholder: '例: main（空欄で現在のブランチ）', hidden: true });
    iso.addEventListener('change', () => { base.hidden = iso.value() !== 'worktree'; });
    const mode = h('select', { class: 'sel' }, ...Object.entries(MODE_LABEL).map(([v, l]) => h('option', { value: v, text: l })));
    mode.value = t.runtime_mode || 'approval-required';
    const inter = h('select', { class: 'sel' }, h('option', { value: 'default', text: '通常' }), h('option', { value: 'plan', text: '計画 (Plan)' }));
    const go = h('button', { class: 'btn primary', type: 'submit', text: '委譲を開始' });
    body.append(h('form', {
      class: 'form',
      onsubmit: async (e) => {
        e.preventDefault(); go.disabled = true;
        const spec = {
          parent_thread_id: t.id, title: title.value.trim() || clip(task.value.replace(/\s+/g, ' '), 40), task: task.value.trim(),
          model_selection: picker.get(), isolation: iso.value(), base_ref: base.value.trim() || null, runtime_mode: mode.value, interaction_mode: inter.value,
        };
        try {
          const r = await api(`/api/threads/${t.id}/delegate`, { method: 'POST', json: spec });
          close(); toast(`子スレッドを開始しました: ${r.thread?.title || ''}`, 'ok');
          if (draft && ta().value.trim() === draft) { ta().value = ''; autosize(); }
          await loadThreads(); loadDetailSoon();
        } catch (err) { go.disabled = false; toast(`委譲に失敗: ${err.message}`, 'err'); }
      },
    }, field('タイトル', title), field('タスク', task), field('子の プロバイダ / モデル / effort', pickRoot),
    field('作業ツリー', h('div', {}, iso, base), 'worktree は親と別ブランチで編集するため、複数の子を同時に走らせても衝突しません'),
    h('div', { class: 'row2' }, field('実行権限', mode), field('モード', inter)),
    h('div', { class: 'actions' }, go)));
    setTimeout(() => task.focus(), 0);
  });
}

function searchDialog() {
  const open = $('dialog.search');
  if (open) { open.close(); return; }
  openModal('全文検索', (body, close) => {
    const inp = h('input', { class: 'inp srch-input', type: 'text', enterKeyHint: 'search', placeholder: '会話・コマンド出力を検索（日本語 3 文字以上）', autocomplete: 'off', spellcheck: false });
    const list = h('ul', { class: 'hits' });
    const note = h('div', { class: 'muted', style: 'font-size:12px;margin-top:8px' });
    let hits = []; let sel = 0; let seq = 0;
    const mark = (snip) => esc(snip).replace(/&lt;(\/?)(mark|b)&gt;/gi, '<$1mark>');
    const draw = () => {
      list.replaceChildren(...hits.map((x, i) => {
        const li = h('li', { class: `hit${i === sel ? ' sel' : ''}`, onclick: () => go(x), onmousemove: () => { if (sel !== i) { sel = i; $$('.hit', list).forEach((e, k) => e.classList.toggle('sel', k === i)); } } },
          h('div', { class: 'ht' }, h('span', { text: x.thread_title || '(無題)' }), h('small', { text: fmtAgo(x.updated_at) })), h('div', { class: 'sn' }));
        li.querySelector('.sn').innerHTML = mark(x.snippet || '');
        return li;
      }));
    };
    const go = (x) => { close(); S.projectId = threadById(x.thread_id)?.project_id || S.projectId; openThread(x.thread_id); };
    const run = debounce(async () => {
      const q = inp.value.trim();
      const my = ++seq;
      if (!q) { hits = []; draw(); note.textContent = ''; return; }
      try {
        const r = await api(`/api/search?q=${encodeURIComponent(q)}&limit=30`);
        if (my !== seq) return;
        hits = Array.isArray(r) ? r : []; sel = 0; draw();
        note.textContent = hits.length ? `${hits.length}件` : (q.length < 3 ? '3文字以上で検索できます（trigram 索引）' : '一致なし');
      } catch (e) { note.textContent = `検索に失敗: ${e.message}`; }
    }, 120);
    inp.addEventListener('input', run);
    inp.addEventListener('keydown', (e) => {
      if (e.isComposing || e.keyCode === 229) return;
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        e.preventDefault(); if (!hits.length) return;
        sel = (sel + (e.key === 'ArrowDown' ? 1 : -1) + hits.length) % hits.length;
        $$('.hit', list).forEach((el, i) => { el.classList.toggle('sel', i === sel); if (i === sel) el.scrollIntoView({ block: 'nearest' }); });
      } else if (e.key === 'Enter' && hits[sel]) { e.preventDefault(); go(hits[sel]); }
    });
    body.append(inp, note, list);
    setTimeout(() => inp.focus(), 0);
  }, { cls: 'search' });
}

function statsDialog() {
  openModal('統計・プライバシー・ストレージ', (body) => {
    const render = async () => {
      body.replaceChildren(h('div', { class: 'loading', text: '読み込み中…' }));
      const [meta, orch, db] = await Promise.allSettled([api('/api/meta'), api('/api/stats/orch'), api('/api/stats/db')]);
      const out = [];
      // ── privacy
      out.push(h('div', { class: 'sec-h', text: 'プライバシー / 認証' }));
      if (meta.status === 'fulfilled') {
        const m = meta.value; const pv = m.privacy || {};
        const noTelemetry = m.telemetry_enabled === false && pv.telemetry_enabled !== true;
        const noCreds = pv.reads_provider_credentials !== true;
        S.meta = m;
        const authMode = pv.auth_mode || m.auth_identity?.method;
        out.push(h('div', { class: 'privacy-badges' },
          h('span', { class: `pbadge ${noTelemetry ? 'good' : 'bad'}`, text: noTelemetry ? '✓ テレメトリ完全無効' : '✗ テレメトリが有効です' }),
          h('span', { class: `pbadge ${noCreds ? 'good' : 'bad'}`, text: noCreds ? '✓ 認証ファイル不読' : '✗ 認証情報を読み取ります' }),
          h('span', { class: 'pbadge info', text: `認証: ${authLabel(authMode)}` }),
          pv.passed != null ? h('span', { class: `pbadge ${pv.passed ? 'good' : 'bad'}`, text: pv.passed ? '✓ プライバシー監査 PASS' : '✗ プライバシー監査 FAIL' }) : null));
        const rows = [['バージョン', m.version], ['認証ユーザー', m.auth_identity ? [m.auth_identity.email, m.auth_identity.subject].filter(Boolean).join(' / ') : '（なし）'],
          ['バインド', pv.bind_host], ['既定モデル', m.default_model ? `${provLabel(m.default_model.provider)} / ${m.default_model.model}` : null],
          ['追加指示の既定', m.follow_up_behavior === 'queue' ? 'Queue (予約)' : m.follow_up_behavior ? 'Steer (割り込み)' : null],
          ['許可された外部通信', (pv.allowed_outbound_urls || []).length ? pv.allowed_outbound_urls.join(', ') : 'なし（完全ローカル）']].filter((r) => r[1]);
        const dl = h('dl', { class: 'kv' });
        rows.forEach(([k, v]) => dl.append(h('dt', { text: k }), h('dd', { text: String(v) })));
        out.push(dl);
        if ((pv.notes || []).length) out.push(h('ul', { class: 'muted', style: 'font-size:12.5px;margin:8px 0 0;padding-left:18px' }, pv.notes.map((n) => h('li', { text: n }))));
      } else out.push(h('div', { class: 'errtxt', text: `取得に失敗: ${meta.reason.message}` }));
      // ── storage
      out.push(h('div', { class: 'sec-h', text: 'ストレージ（SQLite + zstd 圧縮）' }));
      if (db.status === 'fulfilled') {
        const d = db.value;
        const raw = Number(d.raw_payload_bytes) || 0; const st = Number(d.stored_payload_bytes) || 0;
        const ratio = st > 0 ? raw / st : 0;
        const saved = Math.max(0, raw - st);
        const pct = raw > 0 ? (saved / raw) * 100 : 0;
        const bar = (v, cls) => { const i = h('i', { class: cls }); i.style.width = `${raw > 0 ? Math.max(1, (v / raw) * 100) : 0}%`; return h('div', { class: 'bar' }, i); };
        out.push(h('div', { class: 'sgrid' },
          scard('DB ファイル', fmtBytes(d.db_size_bytes)), scard('圧縮率', ratio ? `×${ratio.toFixed(2)}` : '–'), scard('節約量', `${fmtBytes(saved)} (${pct.toFixed(0)}%)`),
          scard('スレッド / Run', `${fmtNum(d.threads_count)} / ${fmtNum(d.runs_count)}`), scard('ターン項目', fmtNum(d.turn_items_count)),
          scard('重複排除ブロブ / 添付', `${fmtNum(d.dedup_blobs_count)} / ${fmtNum(d.attachments_count)}`)));
        out.push(h('div', { class: 'dbrow' }, h('span', { text: '元サイズ' }), bar(raw, ''), h('span', { text: fmtBytes(raw) })));
        out.push(h('div', { class: 'dbrow' }, h('span', { text: '保存サイズ' }), bar(st, 'ok'), h('span', { text: fmtBytes(st) })));
      } else out.push(h('div', { class: 'errtxt', text: `取得に失敗: ${db.reason.message}` }));
      // ── orch
      out.push(h('div', { class: 'sec-h' }, 'orch-stats（プロバイダ / モデル別）', h('button', { class: 'btn sm', type: 'button', text: '更新', onclick: render })));
      if (orch.status === 'fulfilled') {
        const rows = [...orch.value].sort((a, b) => b.total_runs - a.total_runs);
        if (!rows.length) out.push(h('div', { class: 'muted', text: 'まだ実行履歴がありません' }));
        else {
          const tot = rows.reduce((a, r) => ({ n: a.n + r.total_runs, ok: a.ok + r.completed_runs, f: a.f + r.failed_runs, i: a.i + r.total_input_tokens, o: a.o + r.total_output_tokens }), { n: 0, ok: 0, f: 0, i: 0, o: 0 });
          const rate = (r) => (r.total_runs > 0 ? r.completed_runs / r.total_runs : 0);
          const rateCell = (v, n) => { const i = h('i', { class: v >= .9 ? 'ok' : v >= .6 ? 'warn' : 'err' }); i.style.width = `${v * 100}%`; return h('td', { class: 'rate' }, h('div', { class: 'bar' }, i), n ? `${(v * 100).toFixed(0)}%` : '–'); };
          const tb = h('tbody');
          for (const r of rows) {
            tb.append(h('tr', {}, h('td', { text: `${provLabel(r.provider)} / ${r.model}` }), h('td', { text: `${fmtNum(r.total_runs)}` }), h('td', { text: `${fmtNum(r.completed_runs)} / ${fmtNum(r.failed_runs)}` }),
              rateCell(rate(r), r.total_runs), h('td', { text: fmtDur(r.avg_duration_ms) }), h('td', { text: fmtTok(r.total_input_tokens) }), h('td', { text: fmtTok(r.total_output_tokens) })));
          }
          tb.append(h('tr', { style: 'font-weight:700' }, h('td', { text: '合計' }), h('td', { text: fmtNum(tot.n) }), h('td', { text: `${fmtNum(tot.ok)} / ${fmtNum(tot.f)}` }),
            rateCell(tot.n ? tot.ok / tot.n : 0, tot.n), h('td', { text: '' }), h('td', { text: fmtTok(tot.i) }), h('td', { text: fmtTok(tot.o) })));
          out.push(h('div', { class: 'twrap' }, h('table', { class: 'tbl' },
            h('thead', {}, h('tr', {}, ['モデル', '実行', '成功 / 失敗', '成功率', '平均所要', '入力tok', '出力tok'].map((x) => h('th', { text: x })))), tb)));
        }
      } else out.push(h('div', { class: 'errtxt', text: `取得に失敗: ${orch.reason.message}` }));
      body.replaceChildren(...out);
    };
    render();
  }, { cls: 'wide' });
}
const scard = (l, v) => h('div', { class: 'scard' }, h('div', { class: 'sl', text: l }), h('div', { class: 'sv', text: v }));

// ───────────────────────── diff panel ─────────────────────────
let diffSeq = 0;
function setDiffOpen(open) {
  S.diffOpen = open;
  ls.set('t3.diff', open ? '1' : '0');
  $('#diff-panel').hidden = !open;
  $('#app').dataset.diff = open ? 'open' : 'closed';
  $('#btn-diff').setAttribute('aria-pressed', String(open));
  if (open) loadDiff();
}

async function loadDiff() {
  const t = curThread();
  const body = $('#diff-body'); const sum = $('#diff-sum');
  if (!t) { body.replaceChildren(h('div', { class: 'dp-empty', text: 'スレッドを選択してください' })); sum.textContent = ''; return; }
  const my = ++diffSeq;
  body.replaceChildren(h('div', { class: 'dp-empty', text: '読み込み中…' }));
  try {
    const d = await api(`/api/threads/${t.id}/diff?ignore_whitespace=${S.ignoreWs}`);
    if (my !== diffSeq) return;
    sum.replaceChildren(h('span', { class: 'plus', text: `+${d.total_additions}` }), ' ', h('span', { class: 'minus', text: `−${d.total_deletions}` }), ` · ${d.files.length}ファイル${d.branch ? ` · ⎇ ${d.branch}` : ''}`);
    if (d.clean || !d.files.length) { body.replaceChildren(h('div', { class: 'dp-empty', text: '変更はありません（クリーン）' })); return; }
    body.replaceChildren(...d.files.map(diffFile));
  } catch (e) {
    if (my !== diffSeq) return;
    sum.textContent = '';
    body.replaceChildren(h('div', { class: 'dp-empty errtxt', text: `差分を取得できません: ${e.message}` }));
  }
}
const loadDiffSoon = debounce(() => { if (S.diffOpen) loadDiff(); }, 600);

function diffFile(f) {
  const L = kindLetter(f.status);
  const d = h('details', { class: 'dfile' }, // collapsed by default (diffFilesCollapsed: true)
    h('summary', {}, h('span', { class: `fk ${L}`, text: L }), h('span', { class: 'fp', title: f.path, text: f.path }),
      h('span', { class: 'plus', text: `+${f.additions}` }), h('span', { class: 'minus', text: `−${f.deletions}` })));
  let drawn = false;
  d.addEventListener('toggle', () => {
    if (!d.open || drawn) return;
    drawn = true;
    const pre = h('div', { class: 'patch' });
    const lines = String(f.patch || '').split('\n');
    if (lines.at(-1) === '') lines.pop();
    const frag = document.createDocumentFragment();
    for (const l of lines) {
      const c = l.startsWith('+++') || l.startsWith('---') || l.startsWith('diff ') || l.startsWith('index ') ? 'meta' : l.startsWith('+') ? 'add' : l.startsWith('-') ? 'del' : l.startsWith('@@') ? 'hunk' : '';
      frag.append(h('span', { class: `dl ${c}`, text: l || ' ' }));
    }
    if (!lines.length) frag.append(h('span', { class: 'dl meta', text: '（パッチ本文なし：バイナリ or 空白のみの変更）' }));
    pre.append(frag);
    d.append(pre);
  });
  return d;
}

// ───────────────────────── WebSocket ─────────────────────────
let backoff = 500; let reconnectTimer = null; let pingTimer = null;

function setConn(kind, text) {
  const c = $('#conn');
  c.className = `conn ${kind}`;
  $('#conn-text').textContent = text;
}
function wsSend(obj) { if (S.ws && S.ws.readyState === WebSocket.OPEN) S.ws.send(JSON.stringify(obj)); }

function syncSubscriptions() {
  const want = new Set();
  if (S.threadId) want.add(S.threadId);
  for (const t of S.threads) if (ACTIVE_SHELF.has(t.status)) want.add(t.id);
  if (!S.wsOk) return;
  for (const id of want) if (!S.subscribed.has(id)) { wsSend({ type: 'subscribe', thread_id: id }); S.subscribed.add(id); }
  for (const id of [...S.subscribed]) if (!want.has(id)) { wsSend({ type: 'unsubscribe', thread_id: id }); S.subscribed.delete(id); }
}

function connectWs() {
  clearTimeout(reconnectTimer);
  if (S.ws && (S.ws.readyState === WebSocket.OPEN || S.ws.readyState === WebSocket.CONNECTING)) return;
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  let ws;
  try { ws = new WebSocket(`${proto}://${location.host}/ws${S.token ? `?token=${encodeURIComponent(S.token)}` : ''}`); } catch { scheduleReconnect(); return; }
  S.ws = ws;
  setConn('retry', '接続中…');
  ws.onopen = () => {
    S.wsOk = true; backoff = 500; S.subscribed.clear();
    setConn('ok', S.meta ? `接続済み · v${S.meta.version}` : '接続済み');
    syncSubscriptions();
    clearInterval(pingTimer);
    pingTimer = setInterval(() => wsSend({ type: 'ping' }), 25000);
    // catch up on anything missed while disconnected
    loadThreadsSoon(); if (S.threadId) loadDetailSoon();
  };
  ws.onmessage = (ev) => { let m; try { m = JSON.parse(ev.data); } catch { return; } try { handleWs(m); } catch (e) { console.error('ws handler', e); } };
  ws.onclose = () => { S.wsOk = false; S.subscribed.clear(); clearInterval(pingTimer); setConn('retry', `再接続中… (${(backoff / 1000).toFixed(1)}s)`); scheduleReconnect(); };
  ws.onerror = () => { try { ws.close(); } catch { /* noop */ } };
}
function scheduleReconnect() {
  clearTimeout(reconnectTimer);
  reconnectTimer = setTimeout(connectWs, backoff + Math.random() * 250);
  backoff = Math.min(backoff * 1.7, 10000);
}
document.addEventListener('visibilitychange', () => { if (!document.hidden && !S.wsOk) { backoff = 500; connectWs(); } });
addEventListener('online', () => { backoff = 500; connectWs(); });

function setThreadStatus(id, status) {
  const t = S.threads.find((x) => x.id === id);
  if (t) t.status = status;
  if (S.detail?.thread?.id === id) S.detail.thread.status = status;
  const c = S.detail?.children?.find((x) => x.id === id);
  if (c) c.status = status;
}

function handleWs(m) {
  const cur = S.threadId;
  switch (m.type) {
    case 'hello':
      if (S.meta) S.meta.version = m.server_version;
      setConn('ok', `接続済み · v${m.server_version} · ${authLabel(m.auth_method)}`);
      break;
    case 'thread_status_changed': {
      const prev = threadById(m.thread_id)?.status;
      setThreadStatus(m.thread_id, m.status);
      syncSubscriptions(); renderSidebarSoon();
      if (m.thread_id === cur) {
        renderHeader(); renderComposer(); renderLive();
        if (isBusy(prev) && !isBusy(m.status)) { loadDetailSoon(); loadDiffSoon(); }
      }
      if (!isBusy(m.status) && S.pending.has(m.thread_id) && m.status !== 'waiting-for-children') { S.pending.delete(m.thread_id); renderApprovals(); }
      break;
    }
    case 'run_started': {
      S.live.set(m.thread_id, { runId: m.run.id, assistant: '', reasoning: '' });
      if (m.thread_id === cur && S.detail) {
        if (!S.detail.runs.some((r) => r.id === m.run.id)) S.detail.runs.push(m.run);
        setThreadStatus(m.thread_id, 'running');
        renderHeader(); renderComposer();
        const root = tl(); const live = $('#live');
        if (root && live && m.run.id !== renderedRun) { const dv = runDivider(m.run); if (dv) live.before(dv); renderedRun = m.run.id; }
        renderLive(); scrollBottom();
      }
      break;
    }
    case 'run_status_changed': {
      const run = (S.detail?.runs || []).find((r) => r.id === m.run_id);
      if (run) run.status = m.status;
      if (TERMINAL_RUN.has(m.status)) {
        S.live.delete(m.thread_id);
        if (m.status === 'failed' && m.error) toast(`実行失敗: ${m.error}`, 'err', 6000);
        if (m.thread_id === cur) { loadDetailSoon(); loadDiffSoon(); }
        loadThreadsSoon();
      } else if (m.thread_id === cur) renderHeader();
      break;
    }
    case 'delta': {
      let live = S.live.get(m.thread_id);
      if (!live) { live = { runId: m.run_id, assistant: '', reasoning: '' }; S.live.set(m.thread_id, live); }
      const key = m.channel === 'reasoning' ? 'reasoning' : 'assistant';
      live[key] = (live[key] + m.text).slice(-200000);
      if (m.thread_id === cur) { S.stick = nearBottom() || S.stick; renderLive(); }
      break;
    }
    case 'turn_item_added': {
      const it = m.item;
      const live = S.live.get(m.thread_id);
      if (live) {
        if (it.kind === 'assistant_message') live.assistant = '';
        if (it.kind === 'reasoning') live.reasoning = '';
      }
      if (m.thread_id !== cur || !S.detail) break;
      if (S.detail.items.some((x) => x.id === it.id)) break;
      S.detail.items.push(it);
      if (it.kind === 'user_message' && S._optimistic?.length) {
        const k = S._optimistic.findIndex((x) => x.threadId === m.thread_id);
        if (k >= 0) { const [o] = S._optimistic.splice(k, 1); $(`[data-opt="${o.key}"]`)?.remove(); }
      }
      S.stick = nearBottom() || S.stick;
      const host = $('#live');
      const frag = document.createDocumentFragment();
      appendItem(it, frag);
      host ? host.before(frag) : tl().append(frag);
      renderLive(); scrollBottom();
      if (it.kind === 'file_change') loadDiffSoon();
      if (it.kind === 'subagent') loadDetailSoon();
      break;
    }
    case 'pending_request': {
      const r = m.request;
      const a = S.pending.get(r.thread_id) || [];
      if (!a.some((x) => x.id === r.id)) a.push(r);
      S.pending.set(r.thread_id, a);
      if (r.thread_id === cur) { renderApprovals(); scrollBottom(); } else toast(`⚠ 承認待ち: ${r.title || r.kind}`, '', 5000);
      renderSidebarSoon();
      break;
    }
    case 'subagent_updated': {
      const known = threadById(m.child_thread_id);
      if (!known) loadThreadsSoon(); else setThreadStatus(m.child_thread_id, m.status);
      if (m.parent_thread_id === cur) {
        if (!known || !S.detail?.children?.some((c) => c.id === m.child_thread_id)) loadDetailSoon();
        renderHeader();
        $$(`.card.sub[data-child="${m.child_thread_id}"]`).forEach((el) => {
          const dot = $('.st', el); if (dot) dot.className = `st st-${m.status}`;
          const st = $('.sub-status', el); if (st) st.textContent = STATUS_LABEL[m.status] || m.status;
        });
      }
      syncSubscriptions(); renderSidebarSoon();
      break;
    }
    case 'error':
      toast(`サーバーエラー: ${m.message}`, 'err', 6000);
      break;
    default: break;
  }
}

// ───────────────────────── sidebar drawer ─────────────────────────
function openSidebar() { $('#app').dataset.sidebar = 'open'; $('#scrim').hidden = false; }
function closeSidebar() { $('#app').dataset.sidebar = 'closed'; $('#scrim').hidden = true; }

// ───────────────────────── wiring ─────────────────────────
function wire() {
  hydrateIcons();
  applyTheme();
  $('#chk-archived').checked = S.showArchived;
  $('#chk-ws').checked = S.ignoreWs;

  $('#btn-theme').onclick = () => { S.theme = THEMES[(THEMES.indexOf(S.theme) + 1) % THEMES.length]; ls.set('t3.theme', S.theme); applyTheme(); };
  $('#btn-menu').onclick = () => (($('#app').dataset.sidebar === 'open') ? closeSidebar() : openSidebar());
  $('#scrim').onclick = closeSidebar;
  $('#btn-new-thread').onclick = newThreadDialog;
  $('#btn-add-project').onclick = projectDialog;
  $('#btn-search').onclick = searchDialog;
  $('#btn-stats').onclick = statsDialog;
  $('#btn-diff').onclick = () => setDiffOpen(!S.diffOpen);
  $('#btn-diff-close').onclick = () => setDiffOpen(false);
  $('#btn-diff-refresh').onclick = loadDiff;
  $('#btn-diff-expand').onclick = () => {
    const all = $$('#diff-body details.dfile');
    const anyClosed = all.some((d) => !d.open);
    all.forEach((d) => { d.open = anyClosed; });
  };
  $('#chk-ws').onchange = (e) => { S.ignoreWs = e.target.checked; ls.set('t3.ws', S.ignoreWs ? '1' : '0'); loadDiff(); };
  $('#chk-archived').onchange = (e) => { S.showArchived = e.target.checked; ls.set('t3.archived', S.showArchived ? '1' : '0'); loadThreads().catch(() => {}); };
  $('#project-select').onchange = (e) => { S.projectId = e.target.value; ls.set('t3.project', S.projectId); renderTree(); };
  $('#btn-fork').onclick = forkDialog;
  $('#btn-delegate').onclick = delegateDialog;
  $('#btn-stop').onclick = interrupt;
  $('#btn-jump').onclick = () => scrollBottom(true);
  $('#thread-title').onclick = () => {
    const t = curThread(); if (!t) return;
    const name = prompt('スレッド名を変更', t.title);
    if (name && name.trim() && name.trim() !== t.title) patchThread(t.id, { title: name.trim() });
  };
  $$('#behavior button').forEach((b) => { b.onclick = () => { S.behavior = b.dataset.b; ls.set('t3.behavior', S.behavior); renderComposer(); }; });

  // keep pinned to the bottom when layout changes (approval banner, composer growth, late image loads)
  if (typeof ResizeObserver === 'function') {
    const ro = new ResizeObserver(() => { if (S.stick) scrollBottom(); });
    ro.observe(tl()); ro.observe($('#tl-wrap'));
  }
  let lastTop = 0;
  scroller().addEventListener('scroll', () => {
    const top = scroller().scrollTop;
    const up = top < lastTop - 1; lastTop = top;
    if (nearBottom()) S.stick = true; else if (up) S.stick = false; // only a user scroll-up releases the pin
    $('#btn-jump').hidden = S.stick;
  }, { passive: true });

  // composer
  const input = ta();
  input.addEventListener('input', autosize);
  input.addEventListener('keydown', (e) => {
    if (e.key !== 'Enter' || e.isComposing || e.keyCode === 229) return; // never send while IME is composing
    if (e.shiftKey || e.altKey) return;
    if (isCoarse() && !(e.metaKey || e.ctrlKey)) return; // phones: Enter = newline, use the send button
    e.preventDefault(); send();
  });
  input.addEventListener('paste', (e) => {
    const files = [...(e.clipboardData?.files || [])];
    if (files.length) { e.preventDefault(); addFiles(files); }
  });
  $('#composer').addEventListener('submit', (e) => { e.preventDefault(); send(); });
  $('#btn-attach').onclick = () => $('#file-input').click();
  $('#file-input').onchange = (e) => { addFiles([...e.target.files]); e.target.value = ''; };
  const comp = $('#composer');
  ['dragenter', 'dragover'].forEach((n) => comp.addEventListener(n, (e) => { if (e.dataTransfer?.types?.includes('Files')) { e.preventDefault(); comp.classList.add('drag'); } }));
  ['dragleave', 'drop'].forEach((n) => comp.addEventListener(n, () => comp.classList.remove('drag')));
  comp.addEventListener('drop', (e) => { if (e.dataTransfer?.files?.length) { e.preventDefault(); addFiles([...e.dataTransfer.files]); } });

  // code block copy (delegated; works for every rendered Markdown)
  document.addEventListener('click', (e) => {
    const b = e.target.closest?.('[data-copy]');
    if (!b) return;
    const code = b.closest('.codeblock')?.querySelector('code');
    if (code) copyText(code.textContent).then?.(() => { b.textContent = '✓ コピー済み'; setTimeout(() => { b.textContent = 'コピー'; }, 1500); });
  });

  // global shortcuts
  document.addEventListener('keydown', (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') { e.preventDefault(); searchDialog(); }
  });
  addEventListener('hashchange', () => { const id = threadFromHash(); if (id && id !== S.threadId) openThread(id, { push: false }); });
  addEventListener('resize', () => { if (innerWidth > 860) closeSidebar(); });
}

const threadFromHash = () => (/^#\/t\/([0-9a-f-]{36})$/i.exec(location.hash) || [])[1] || null;

// ───────────────────────── boot ─────────────────────────
async function boot() {
  wire();
  setDiffOpen(S.diffOpen);
  composerPicker = modelPicker($('#model-picker'), { value: null });
  renderTimeline(); renderHeader(); renderComposer();
  connectWs();
  try {
    const [meta, providers] = await Promise.all([
      api('/api/meta').catch(() => null),
      api('/api/providers').catch(() => []),
    ]);
    S.meta = meta; S.providers = Array.isArray(providers) ? providers : [];
    if (S.wsOk && meta) setConn('ok', `接続済み · v${meta.version}`);
    composerPicker = modelPicker($('#model-picker'), { value: meta?.default_model || null });
    if (meta?.follow_up_behavior && !ls.get('t3.behavior', '')) { S.behavior = meta.follow_up_behavior === 'queue' ? 'queue' : 'steer'; renderComposer(); }
    await loadProjects();
    await loadThreads();
  } catch (e) {
    setConn('bad', `サーバーに接続できません: ${e.message}`);
    toast(`初期化に失敗: ${e.message}`, 'err', 6000);
  }
  const hashId = threadFromHash();
  if (hashId) await openThread(hashId, { push: false });
  setInterval(() => { if (!document.hidden) loadThreads().catch(() => {}); }, 15000);
}

boot();
