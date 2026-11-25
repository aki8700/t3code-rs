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
    if (kid == null || kid === false || kid === '') continue;
    el.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
  }
  return el;
}

const ls = {
  get(k, d) { try { const v = localStorage.getItem(k); return v == null ? d : v; } catch { return d; } },
  set(k, v) { try { localStorage.setItem(k, v); } catch { /* storage may be unavailable */ } },
  json(k, d) { try { return JSON.parse(this.get(k, '')) ?? d; } catch { return d; } },
};

const ICONS = {
  panel: '<rect x="3" y="4" width="18" height="16" rx="2.5"/><path d="M9.5 4v16"/>',
  compose: '<path d="M11 5H6a2 2 0 0 0-2 2v11a2 2 0 0 0 2 2h11a2 2 0 0 0 2-2v-5"/><path d="M17.5 3.5a2.1 2.1 0 0 1 3 3L12 15l-4 1 1-4z"/>',
  search: '<circle cx="11" cy="11" r="7"/><path d="M20 20l-3.8-3.8"/>',
  diff: '<path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><path d="M12 8v5M9.5 10.5h5M9.5 17h5"/>',
  chart: '<path d="M4 20V10M10 20V4M16 20v-7M22 20H2"/>',
  palette: '<path d="M12 3a9 9 0 1 0 0 18c1.2 0 2-.9 2-2 0-.6-.2-1-.6-1.4-.3-.4-.6-.8-.6-1.4 0-1.1.9-2 2-2h2.4c2.1 0 3.8-1.7 3.8-3.8C21 6 17 3 12 3z"/><circle cx="7.5" cy="11" r="1"/><circle cx="10.5" cy="7.5" r="1"/><circle cx="15" cy="7.8" r="1"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  folderPlus: '<path d="M3 7a2 2 0 0 1 2-2h4l2 2.5h8a2 2 0 0 1 2 2V17a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2zM12 11v5M9.5 13.5h5"/>',
  clip: '<path d="M21 11l-9 9a5 5 0 0 1-7-7l9-9a3.5 3.5 0 0 1 5 5l-9 9a2 2 0 0 1-3-3l8-8"/>',
  stop: '<rect class="fill" x="7" y="7" width="10" height="10" rx="2"/>',
  arrowUp: '<path d="M12 19V5M5.5 11.5L12 5l6.5 6.5"/>',
  down: '<path d="M12 5v14M6 13l6 6 6-6"/>',
  fork: '<circle cx="6" cy="5" r="2.2"/><circle cx="6" cy="19" r="2.2"/><circle cx="18" cy="7" r="2.2"/><path d="M6 7.2v9.6M18 9.2c0 4.5-6 3.800-11 7"/>',
  split: '<circle cx="12" cy="5" r="2.2"/><circle cx="5" cy="19" r="2.2"/><circle cx="19" cy="19" r="2.2"/><path d="M12 7.2V11M12 11c-4 0-7 2-7 5.8M12 11c4 0 7 2 7 5.8"/>',
  close: '<path d="M6 6l12 12M18 6L6 18"/>',
  refresh: '<path d="M21 12a9 9 0 1 1-2.700-6.400L21 8M21 3v5h-5"/>',
  expand: '<path d="M7 9l5-5 5 5M7 15l5 5 5-5"/>',
  pin: '<path d="M12 17v5M9 3h6l-1 7 3.500 4h-11L10 10z"/>',
  archive: '<path d="M3 4h18v4.500H3zM5 8.500V20h14V8.500M10 12.500h4"/>',
  copy: '<rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15V6a2 2 0 0 1 2-2h9"/>',
  check: '<path d="M5 12.500l4.500 4.500L19 7.500"/>',
  chevron: '<path d="M9 6l6 6-6 6"/>',
  chevDown: '<path d="M6 9l6 6 6-6"/>',
  terminal: '<path d="M5 7l5 5-5 5M12.500 17H19"/>',
  tool: '<path d="M14.700 6.300a4 4 0 0 0 5 5L11 20a2.100 2.100 0 0 1-3-3l8.700-8.700a4 4 0 0 0-5-5l2.600 2.600-.600 3-3 .600-2.600-2.600a4 4 0 0 0 5 5"/>',
  file: '<path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8zM14 3v5h5"/>',
  filePen: '<path d="M13 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h4M13 3v5h5M18.400 12.600a1.900 1.900 0 0 1 2.700 2.700L16 20.400l-3.500.800.800-3.500z"/>',
  globe: '<circle cx="12" cy="12" r="9"/><path d="M3 12h18M12 3c3 3.200 3 14.800 0 18M12 3c-3 3.200-3 14.800 0 18"/>',
  bot: '<rect x="4" y="8" width="16" height="12" rx="3"/><path d="M12 4v4M9 13v2M15 13v2"/>',
  handoff: '<path d="M4 8h14M14 4l4 4-4 4M20 16H6M10 12l-4 4 4 4"/>',
  brain: '<path d="M9 4a3 3 0 0 0-3 3 3.500 3.500 0 0 0-2 6 3.500 3.500 0 0 0 3 5.500A2.500 2.500 0 0 0 12 18V6a2.500 2.500 0 0 0-3-2zM15 4a3 3 0 0 1 3 3 3.500 3.500 0 0 1 2 6 3.500 3.500 0 0 1-3 5.500A2.500 2.500 0 0 1 12 18"/>',
  info: '<circle cx="12" cy="12" r="9"/><path d="M12 11v6M12 7.500v.01"/>',
  shield: '<path d="M12 3l8 3v6c0 4.500-3.300 8-8 9-4.700-1-8-4.500-8-9V6z"/><path d="M8.500 12l2.500 2.500 4.500-5"/>',
  alert: '<path d="M12 4l9.500 16h-19zM12 10v4M12 17v.01"/>',
};
const svg = (n) => `<svg class="ic" viewBox="0 0 24 24" aria-hidden="true">${ICONS[n] || ''}</svg>`;
const iconEl = (n, cls = '') => { const s = h('span', { class: `iw ${cls}` }); s.innerHTML = svg(n); return s; };
const hydrateIcons = (root = document) => $$('[data-icon]', root).forEach((el) => { if (!el.querySelector('svg')) el.insertAdjacentHTML('afterbegin', svg(el.dataset.icon)); });
const ibtn = (ic, title, onclick, cls = '') => {
  const b = h('button', { class: `ibtn ${cls}`, type: 'button', title, 'aria-label': title, onclick });
  b.innerHTML = svg(ic);
  return b;
};

const IS_MAC = /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
const kbd = (k) => (IS_MAC ? `⌘${k}` : `Ctrl+${k}`);

function fmtAgo(iso) {
  const t = Date.parse(iso); if (!t) return '';
  const now = new Date();
  const s = Math.max(0, (now - t) / 1000);
  if (s < 45) return 'たった今';
  if (s < 3600) return `${Math.max(1, Math.round(s / 60))}分前`;
  if (s < 86400) return `${Math.floor(s / 3600)}時間前`;
  const day = (d) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  const days = Math.round((day(now) - day(new Date(t))) / 86400000);
  if (days <= 1) return '昨日';
  if (days < 30) return `${days}日前`;
  return new Date(t).toLocaleDateString('ja-JP', { year: '2-digit', month: 'numeric', day: 'numeric' });
}
function fmtDur(ms) {
  if (ms == null || !isFinite(ms)) return '–';
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(1)}秒`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}分${Math.floor(s % 60)}秒`;
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
const oneLine = (s, n = 200) => clip(String(s ?? '').replace(/\s+/g, ' ').trim(), n);
const pretty = (v) => { try { return typeof v === 'string' ? v : JSON.stringify(v, null, 2); } catch { return String(v); } };
const isCoarse = () => matchMedia('(pointer: coarse)').matches;
const isNarrow = () => matchMedia('(max-width: 860px)').matches;
const hashNum = (s) => { let n = 0; for (const c of String(s)) n = (n * 31 + c.codePointAt(0)) >>> 0; return n; };

function toast(msg, kind = '', ms = 3800) {
  const box = $('#toasts');
  const el = h('div', { class: `toast ${kind}`, text: msg });
  box.append(el);
  // re-show so the popover sits above any open modal <dialog> in the top layer
  try { box.hidePopover(); box.showPopover(); } catch { /* popover unsupported */ }
  setTimeout(() => { el.remove(); if (!box.childElementCount) { try { box.hidePopover(); } catch { /* noop */ } } }, ms);
}

// ───────────────────────── desktop shell (wry / WKWebView) ─────────────────────────
const isDesktop = () => window.__T3_DESKTOP__ === true || document.documentElement.classList.contains('t3-desktop');
const ipc = (msg) => window.ipc?.postMessage(msg);

function wireDesktop() {
  if (window.__T3_DESKTOP__ === true) document.documentElement.classList.add('t3-desktop');
  const INTERACTIVE = 'button, input, textarea, select, a, label, summary, [role="button"], [contenteditable="true"], .no-drag';
  const onDragRegion = (e) => isDesktop() && e.button === 0 && e.target.closest?.('.drag-region') && !e.target.closest(INTERACTIVE);
  // The second press of a double click must not start a window drag, or dblclick never fires.
  document.addEventListener('mousedown', (e) => { if (onDragRegion(e) && e.detail < 2) ipc('drag'); });
  document.addEventListener('dblclick', (e) => { if (onDragRegion(e)) ipc('toggle-maximize'); });
  document.addEventListener('click', (e) => {
    if (!isDesktop()) return;
    const a = e.target.closest?.('a[target="_blank"]');
    if (!a || !/^https?:/i.test(a.href)) return;
    e.preventDefault();
    ipc(`open-external:${a.href}`);
  });
}

// ───────────────────────── labels & domain helpers ─────────────────────────
const PROVIDER_LABEL = { claude: 'Claude', codex: 'Codex', antigravity: 'Antigravity', 'open-code': 'OpenCode', opencode: 'OpenCode', mock: 'Mock' };
const PROVIDER_FALLBACK = ['claude', 'codex', 'antigravity', 'open-code'];
const provLabel = (p) => PROVIDER_LABEL[p] || p || '?';
const provClass = (p) => `p-${p === 'opencode' ? 'open-code' : p || 'mock'}`;
const provDot = (p, extra = '') => h('span', { class: `pdot ${provClass(p)} ${extra}`, title: provLabel(p) });
const TIERS = {
  0: { label: 'Tier 0 無料/ローカル' },
  1: { label: 'Tier 1 高速' },
  2: { label: 'Tier 2 監査' },
  3: { label: 'Tier 3 設計' },
};
const EFFORT_FALLBACK = ['low', 'medium', 'high', 'xhigh'];
const STATUS_LABEL = {
  idle: '待機', running: '実行中', 'waiting-for-children': '子の完了待ち', 'rate-limited': 'レート制限', completed: '完了', failed: '失敗', interrupted: '中断',
  queued: '待ち行列', cancelled: '取消',
};
const REL_LABEL = { delegate: '委譲', delegated: '委譲', fork: '分岐', forked: '分岐', handoff: '引継', child: '子' };
const MODES = ['approval-required', 'auto-accept-edits', 'full-access'];
const MODE_LABEL = { 'approval-required': '承認制', 'auto-accept-edits': '編集自動', 'full-access': 'フルアクセス' };
const MODE_HINT = {
  'approval-required': '変更やコマンド実行の前に承認を求めます',
  'auto-accept-edits': 'ファイル編集は自動承認し、それ以外は承認を求めます',
  'full-access': '承認なしですべて実行します。信頼できる作業でのみ使ってください',
};
const BUSY = new Set(['running', 'waiting-for-children', 'rate-limited']);
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
  collapsedProjects: new Set(ls.json('t3.pcollapsed', [])),
  expandedProjects: new Set(), // projects showing all threads
  behavior: ls.get('t3.behavior', '') === 'queue' ? 'queue' : 'steer',
  atts: [],               // composer attachments
  drafts: new Map(),      // threadId | 'new' -> { text, atts }
  // composer selection. runtime mode / plan / isolation apply only when a new thread is created.
  compose: { sel: null, mode: 'approval-required', plan: false, isolation: 'shared', baseRef: '' },
  heroSel: ls.json('t3.sel', null),
  optimistic: [],
  stick: true,
  diffOpen: ls.get('t3.diff', '0') === '1',
  diffStat: null,         // { threadId, add, del, files }
  ignoreWs: ls.get('t3.ws', '1') !== '0',
  dbStats: null,
  subscribed: new Set(),
  ws: null, wsOk: false,
  token: ls.get('t3.token', ''),
  theme: ls.get('t3.theme', 'system'),
};

const threadById = (id) => S.threads.find((t) => t.id === id) || (S.detail?.thread?.id === id ? S.detail.thread : null)
  || S.detail?.children?.find((t) => t.id === id) || null;
const curThread = () => S.detail?.thread || null;
const projectById = (id) => S.projects.find((p) => p.id === id) || null;
const projectTitle = (id) => projectById(id)?.title || '';

const providerInfo = (id) => S.providers.find((x) => x.provider === id) || null;
const findModelSpec = (sel) => providerInfo(sel?.provider)?.models?.find((m) => m.id === sel.model) || null;
const modelLabel = (sel) => findModelSpec(sel)?.label || sel?.model || 'モデル未選択';
const tierNum = (spec) => { const m = /tier(\d)/.exec(spec?.tier || ''); return m ? Number(m[1]) : null; };
function effortLevels(sel) {
  const spec = findModelSpec(sel);
  return spec ? spec.effort_levels || [] : EFFORT_FALLBACK;
}
function withEffort(sel, wanted) {
  const levels = effortLevels(sel);
  const out = { provider: sel.provider, model: sel.model };
  const spec = findModelSpec(sel);
  const e = [wanted, spec?.default_effort, 'high'].find((x) => x && levels.includes(x)) || levels.at(-1);
  if (e) out.effort = e;
  if (sel.agent) out.agent = sel.agent;
  return out;
}
const sameSel = (a, b) => !!a && !!b && a.provider === b.provider && a.model === b.model && (a.effort || '') === (b.effort || '') && (a.agent || '') === (b.agent || '');

function defaultSelection(projectId) {
  const known = (s) => s?.provider && s.model && (!S.providers.length || providerInfo(s.provider));
  const first = S.providers.find((p) => p.available && p.models?.length);
  const sel = [projectById(projectId)?.default_model, S.heroSel, S.meta?.default_model,
    first && { provider: first.provider, model: first.models[0].id }].find(known) || { provider: 'claude', model: '' };
  return withEffort(sel, sel.effort);
}

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
function attachmentBlobUrl(id) {
  if (blobCache.has(id)) return blobCache.get(id);
  const headers = S.token ? { Authorization: `Bearer ${S.token}` } : {};
  const p = fetch(`/api/attachments/${encodeURIComponent(id)}`, { headers, credentials: 'same-origin', referrerPolicy: 'no-referrer' })
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
    field('トークン', inp),
    h('div', { class: 'actions' }, h('button', { class: 'btn primary', type: 'submit', text: '保存して再読み込み' }))));
    setTimeout(() => inp.focus(), 0);
  }, { onclose: () => { tokenPromptShown = false; } });
}

// ───────────────────────── modal helper (native <dialog>) ─────────────────────────
function openModal(title, build, { cls = '', onclose } = {}) {
  closePopover();
  const dlg = h('dialog', { class: `modal ${cls}`, 'aria-label': title });
  const close = () => { try { dlg.close(); } catch { dlg.remove(); } };
  const body = h('div', { class: 'modal-body' });
  dlg.append(h('div', { class: 'modal-head' }, h('h2', { text: title }), ibtn('close', '閉じる', close)), body);
  dlg.addEventListener('close', () => { dlg.remove(); onclose?.(); });
  dlg.addEventListener('mousedown', (e) => { if (e.target === dlg) close(); });
  document.body.append(dlg);
  dlg.showModal();
  build(body, close, dlg);
  return { dlg, close, body };
}

// ───────────────────────── popover helper ─────────────────────────
let popover = null; // { el, anchor, place }
function closePopover() {
  if (!popover) return;
  const { el, anchor } = popover;
  popover = null;
  el.remove();
  anchor.setAttribute('aria-expanded', 'false');
}
/** Opens a floating panel above the anchor (bottom sheet on phones). Toggles when the same anchor is clicked again. */
function openPopover(anchor, content, { cls = '', align = 'end' } = {}) {
  const again = popover?.anchor === anchor;
  closePopover();
  if (again) return null;
  const el = h('div', { class: `popover ${cls}`, role: 'dialog' }, content);
  document.body.append(el);
  const place = () => {
    if (matchMedia('(max-width: 640px)').matches) { el.classList.add('sheet'); el.style.cssText = ''; return; }
    el.classList.remove('sheet');
    const r = anchor.getBoundingClientRect();
    const w = el.offsetWidth;
    const left = align === 'end' ? r.right - w : r.left;
    el.style.left = `${Math.max(8, Math.min(left, innerWidth - w - 8))}px`;
    el.style.bottom = `${Math.max(8, innerHeight - r.top + 8)}px`;
    el.style.maxHeight = `${Math.max(220, r.top - 20)}px`;
  };
  popover = { el, anchor, place };
  anchor.setAttribute('aria-expanded', 'true');
  place();
  return el;
}
document.addEventListener('pointerdown', (e) => {
  if (popover && !popover.el.contains(e.target) && !popover.anchor.contains(e.target)) closePopover();
}, true);
addEventListener('resize', () => popover?.place());

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
  const isDiff = lang === 'diff' || lang === 'patch';
  const body = isDiff ? code.split('\n').map((l) => {
    const c = l.startsWith('+') && !l.startsWith('+++') ? 'dl-add' : l.startsWith('-') && !l.startsWith('---') ? 'dl-del' : l.startsWith('@@') ? 'dl-hunk' : '';
    return `<span class="${c}">${esc(l) || ' '}</span>`;
  }).join('') : esc(code);
  return `<div class="codeblock"><div class="cb-head"><span>${esc(lang || 'text')}</span><button type="button" data-copy>${svg('copy')}<span>コピー</span></button></div><pre><code${isDiff ? ' class="diffc"' : ''}>${body}</code></pre></div>`;
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
      const start = ordered ? parseInt(m[2], 10) : 1;
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
      out.push(`<${tag}${start > 1 ? ` start="${start}"` : ''}>${items.map((it) => {
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
const mdEl = (src, cls = 'md') => { const el = h('div', { class: cls }); el.innerHTML = md(src); return el; };

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
const THEMES = [
  ['system', 'システムに合わせる'],
  ['dark', 'T3 Code (Dark)'],
  ['light', 'T3 Code (Light)'],
  ['ember', 'Ember'],
  ['ocean', 'Ocean'],
  ['grove', 'Grove'],
  ['iris', 'Iris'],
];
const darkQuery = matchMedia('(prefers-color-scheme: dark)');
const resolvedTheme = () => (S.theme === 'system' ? (darkQuery.matches ? 'dark' : 'light') : S.theme);
function applyTheme() {
  if (!THEMES.some(([id]) => id === S.theme)) S.theme = 'system';
  const root = document.documentElement;
  root.dataset.theme = resolvedTheme();
  $('meta[name="theme-color"]').content = getComputedStyle(root).getPropertyValue('--canvas').trim();
  $('#theme-name').textContent = S.theme === 'system' ? '自動' : THEMES.find(([id]) => id === S.theme)[1].replace(/^T3 Code \((.+)\)$/, '$1');
}
darkQuery.addEventListener('change', () => { if (S.theme === 'system') applyTheme(); });
function setTheme(id) { S.theme = id; ls.set('t3.theme', id); applyTheme(); }
function themeMenu(anchor) {
  const list = h('div', { class: 'menu', role: 'menu' }, THEMES.map(([id, label]) => {
    const swatch = h('span', { class: 'swatch', dataset: { theme: id === 'system' ? (darkQuery.matches ? 'dark' : 'light') : id } }, h('i'), h('i'), h('i'));
    return h('button', {
      type: 'button', role: 'menuitemradio', class: `menu-item${S.theme === id ? ' on' : ''}`, 'aria-checked': String(S.theme === id), dataset: { themeId: id },
      onclick: () => { setTheme(id); $$('.menu-item', list).forEach((b) => { const on = b.dataset.themeId === id; b.classList.toggle('on', on); b.setAttribute('aria-checked', String(on)); }); },
    }, swatch, h('span', { text: label }), iconEl('check', 'tick'));
  }));
  openPopover(anchor, list, { cls: 'menu-pop', align: 'start' });
}

// ───────────────────────── model / permission panels ─────────────────────────
function segmented(options, value, onChange, { label, disabled = false, cls = '' } = {}) {
  const box = h('div', { class: `seg ${cls}`, role: 'radiogroup', 'aria-label': label });
  for (const [v, text, title] of options) {
    box.append(h('button', {
      type: 'button', role: 'radio', title, disabled, dataset: { v }, class: v === value ? 'on' : '', 'aria-checked': String(v === value), text,
      onclick: () => {
        $$('button', box).forEach((b) => { const on = b.dataset.v === v; b.classList.toggle('on', on); b.setAttribute('aria-checked', String(on)); });
        onChange(v);
      },
    }));
  }
  return box;
}

/** Spotlight filter + provider pills + model list + effort bar. Used by the composer popover and the fork / delegate dialogs. */
function modelPanel({ value, onChange }) {
  let sel = withEffort(value || defaultSelection(S.projectId), value?.effort);
  let filter = 'all'; let cursor = 0; let rows = [];
  const ids = S.providers.length ? S.providers.map((p) => p.provider) : PROVIDER_FALLBACK;
  if (sel.provider && !ids.includes(sel.provider)) ids.push(sel.provider);
  const search = h('input', { class: 'mp-search', type: 'text', placeholder: 'モデルを絞り込み…', autocomplete: 'off', spellcheck: false, autocapitalize: 'off', 'aria-label': 'モデルを絞り込み' });
  const pills = h('div', { class: 'mp-pills', role: 'tablist' });
  const list = h('div', { class: 'mp-list', role: 'listbox', 'aria-label': 'モデル' });
  const effortBox = h('div', { class: 'mp-effort' });
  const el = h('div', { class: 'mp' }, h('div', { class: 'mp-searchrow' }, iconEl('search'), search), pills, list, effortBox);

  const commit = (next) => { sel = next; onChange?.({ ...sel }); drawList(false); drawEffort(); };
  function drawPills() {
    pills.replaceChildren(...[['all', 'すべて'], ...ids.map((id) => [id, provLabel(id)])].map(([id, text]) => h('button', {
      type: 'button', role: 'tab', class: filter === id ? 'on' : '', 'aria-selected': String(filter === id), dataset: { prov: id },
      onclick: () => { filter = id; cursor = 0; drawPills(); drawList(false); },
    }, id === 'all' ? null : provDot(id), text)));
  }
  function drawList(scroll = true) {
    const q = search.value.trim().toLowerCase();
    rows = [];
    for (const id of ids) {
      if (filter !== 'all' && filter !== id) continue;
      const info = providerInfo(id);
      const models = [...(info?.models || [])];
      if (sel.provider === id && sel.model && !models.some((m) => m.id === sel.model)) models.push({ id: sel.model, label: sel.model });
      for (const m of models) {
        if (q && !`${m.label} ${m.id} ${provLabel(id)}`.toLowerCase().includes(q)) continue;
        rows.push({ provider: id, spec: m, info });
      }
    }
    const raw = search.value.trim();
    if (raw && filter !== 'all' && !rows.some((r) => r.spec.id === raw)) rows.push({ provider: filter, spec: { id: raw, label: raw }, info: providerInfo(filter), custom: true });
    cursor = Math.min(cursor, Math.max(0, rows.length - 1));
    const keep = list.scrollTop;
    const nodes = [];
    let group = null;
    rows.forEach((r, i) => {
      if (filter === 'all' && r.provider !== group) {
        group = r.provider;
        nodes.push(h('div', { class: 'mp-group' }, provLabel(group), r.info && !r.info.available ? h('em', { text: '未導入' }) : null));
      }
      const on = sel.provider === r.provider && sel.model === r.spec.id;
      const tier = tierNum(r.spec);
      nodes.push(h('button', {
        type: 'button', role: 'option', 'aria-selected': String(on), dataset: { model: r.spec.id, provider: r.provider },
        class: `mp-row${on ? ' on' : ''}${i === cursor && q ? ' cur' : ''}${r.info && !r.info.available ? ' off' : ''}`,
        title: r.info?.error || r.spec.id,
        onclick: () => commit(withEffort({ provider: r.provider, model: r.spec.id }, sel.effort)),
      }, provDot(r.provider),
      h('span', { class: 'mp-name', text: r.custom ? `「${r.spec.id}」をモデルIDとして使う` : r.spec.label || r.spec.id }),
      r.spec.is_local ? h('span', { class: 'tag', text: 'local' }) : null,
      tier != null && TIERS[tier] ? h('span', { class: `tier t${tier}`, text: TIERS[tier].label }) : null,
      iconEl('check', 'tick')));
    });
    if (!rows.length) nodes.push(h('div', { class: 'mp-none', text: filter === 'all' ? '一致するモデルがありません（任意のIDを使うにはプロバイダを選択）' : 'モデルID を入力してください' }));
    list.replaceChildren(...nodes);
    list.scrollTop = keep;
    if (scroll) requestAnimationFrame(() => { const t = $('.mp-row.cur', list) || $('.mp-row.on', list); if (t) list.scrollTop = Math.max(0, t.offsetTop - list.offsetTop - list.clientHeight / 2); });
  }
  function drawEffort() {
    const levels = effortLevels(sel);
    effortBox.replaceChildren();
    if (!levels.length) return;
    effortBox.append(h('span', { class: 'mp-lab', text: 'Effort' }),
      segmented(levels.map((l) => [l, l]), sel.effort, (v) => { sel = { ...sel, effort: v }; onChange?.({ ...sel }); }, { label: '推論強度', cls: 'effort' }));
  }
  search.addEventListener('input', () => { cursor = 0; drawList(false); });
  search.addEventListener('keydown', (e) => {
    if (e.isComposing || e.keyCode === 229) return;
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault(); if (!rows.length) return;
      cursor = (cursor + (e.key === 'ArrowDown' ? 1 : -1) + rows.length) % rows.length;
      $$('.mp-row', list).forEach((b, i) => { b.classList.toggle('cur', i === cursor); if (i === cursor) b.scrollIntoView({ block: 'nearest' }); });
    } else if (e.key === 'Enter') {
      e.preventDefault();
      const r = rows[cursor];
      if (r) commit(withEffort({ provider: r.provider, model: r.spec.id }, sel.effort));
    }
  });
  drawPills(); drawList(); drawEffort();
  return { el, search, get: () => ({ ...sel }) };
}

/** 3-stage permission switch + Plan toggle. `locked` shows an existing thread's values (the API cannot change them). */
function permPanel({ mode, plan, locked = false, maxMode = 'full-access', planForced = false, onMode, onPlan, onNew }) {
  const max = MODES.indexOf(maxMode);
  const hint = h('p', { class: `perm-hint${mode === 'full-access' ? ' danger' : ''}`, text: MODE_HINT[mode] });
  const seg = segmented(MODES.map((m) => [m, MODE_LABEL[m], MODE_HINT[m]]), mode, (v) => {
    hint.textContent = MODE_HINT[v]; hint.classList.toggle('danger', v === 'full-access'); onMode?.(v);
  }, { label: '実行権限', disabled: locked, cls: 'perm' });
  if (!locked) $$('button', seg).forEach((b) => { b.disabled = MODES.indexOf(b.dataset.v) > max; });
  const chk = h('input', { type: 'checkbox', checked: plan || planForced, disabled: locked || planForced, onchange: (e) => onPlan?.(e.target.checked) });
  return h('div', { class: 'perm-panel' }, h('span', { class: 'mp-lab', text: '権限' }), seg, hint,
    h('label', { class: 'switch plan-switch' }, chk, h('i'), h('span', {}, h('b', { text: 'Plan モード' }), h('small', { text: planForced ? '親が Plan のため子も Plan になります' : '変更せずに調査と計画だけを行います' }))),
    locked ? h('p', { class: 'perm-lock' }, '権限と Plan はスレッド作成時に固定されます。',
      onNew ? h('button', { type: 'button', class: 'link', text: '新規スレッドで変更', onclick: onNew }) : null) : null);
}

function composerPerm() {
  const t = curThread();
  return permPanel({
    mode: t ? t.runtime_mode : S.compose.mode,
    plan: t ? t.interaction_mode === 'plan' : S.compose.plan,
    locked: !!t,
    onMode: (v) => { S.compose.mode = v; renderComposer(); },
    onPlan: (v) => { S.compose.plan = v; renderComposer(); },
    onNew: () => goHome(t?.project_id),
  });
}

function setComposeSel(sel) {
  S.compose.sel = sel;
  if (!curThread()) { S.heroSel = sel; ls.set('t3.sel', JSON.stringify(sel)); }
  renderComposer();
}
function openModelPopover(anchor) {
  const panel = modelPanel({ value: S.compose.sel, onChange: setComposeSel });
  const el = openPopover(anchor, h('div', { class: 'pop-stack' }, panel.el, composerPerm()), { cls: 'model-pop' });
  if (!el) return;
  popover.place();
  if (!isCoarse()) panel.search.focus({ preventScroll: true });
}

// ───────────────────────── sidebar ─────────────────────────
const MONO_HUES = [217, 265, 330, 12, 35, 150, 175, 195, 290, 95];
function monogram(project, cls = '') {
  const title = project?.title || '?';
  const letters = [...title.replace(/^[\d\-_. ]+/, '') || title];
  const el = h('span', { class: `mono-badge ${cls}`, 'aria-hidden': 'true', text: (letters[0] || '?').toUpperCase() });
  el.style.setProperty('--hue', MONO_HUES[hashNum(title) % MONO_HUES.length]);
  return el;
}

const threadCmp = (a, b) => (b.pinned - a.pinned) || (Date.parse(b.updated_at) - Date.parse(a.updated_at));
function buildTree(list) {
  const ids = new Set(list.map((t) => t.id));
  const byParent = new Map();
  for (const t of list) {
    const p = t.parent_thread_id && ids.has(t.parent_thread_id) ? t.parent_thread_id : null;
    if (!byParent.has(p)) byParent.set(p, []);
    byParent.get(p).push(t);
  }
  const mk = (t, seen) => {
    seen.add(t.id);
    return { t, kids: (byParent.get(t.id) || []).filter((k) => !seen.has(k.id)).sort(threadCmp).map((k) => mk(k, seen)) };
  };
  return (byParent.get(null) || []).sort(threadCmp).map((t) => mk(t, new Set()));
}
const treeHas = (node, id) => node.t.id === id || node.kids.some((k) => treeHas(k, id));

function threadRow(t, { shelf = false, kids = [] } = {}) {
  const pend = (S.pending.get(t.id) || []).length;
  const hasKids = !shelf && kids.length > 0;
  const collapsed = S.collapsed.has(t.id);
  const rel = t.parent_thread_id ? (REL_LABEL[t.relationship] || t.relationship || '子') : null;
  const status = STATUS_LABEL[t.status] || t.status;

  const act = (ic, title, fn, on) => ibtn(ic, title, (e) => { e.stopPropagation(); fn(); }, `xs${on ? ' on' : ''}`);
  const main = h('div', {
    class: 'trow-main', role: 'button', tabindex: 0,
    title: `${t.title}\n${provLabel(t.model_selection?.provider)} / ${t.model_selection?.model} · ${status}${rel ? ` · ${rel}` : ''}`,
    onclick: () => openThread(t.id),
    onkeydown: (e) => { if ((e.key === 'Enter' || e.key === ' ') && e.target === e.currentTarget) { e.preventDefault(); openThread(t.id); } },
  },
  hasKids ? h('button', {
    class: `caret${collapsed ? '' : ' open'}`, type: 'button', 'aria-label': collapsed ? '子スレッドを展開' : '子スレッドを折りたたむ', 'aria-expanded': String(!collapsed),
    onclick: (e) => { e.stopPropagation(); collapsed ? S.collapsed.delete(t.id) : S.collapsed.add(t.id); renderSidebar(); },
  }, iconEl('chevron')) : null,
  shelf ? monogram(projectById(t.project_id), 'sm') : null,
  provDot(t.model_selection?.provider, `${t.status === 'running' ? 'live' : ''} s-${t.status}`),
  h('span', { class: 'trow-title', text: t.title || '(無題)' }),
  t.pinned ? iconEl('pin', 'pinmark') : null,
  hasKids && collapsed ? h('span', { class: 'kidcount', text: String(kids.length) }) : null,
  pend ? h('span', { class: 'flag warn', text: '承認待ち' })
    : t.status === 'failed' ? h('span', { class: 'flag bad', text: '失敗' })
      : t.status === 'rate-limited' ? h('span', { class: 'flag warn', text: '制限中' })
        : t.status === 'waiting-for-children' ? h('span', { class: 'flag', text: '子待ち' }) : null,
  h('span', { class: 'trow-time', text: fmtAgo(t.updated_at) }),
  h('span', { class: 'trow-actions' },
    act('pin', t.pinned ? 'ピン留め解除' : 'ピン留め', () => patchThread(t.id, { pinned: !t.pinned }), t.pinned),
    act('archive', t.archived ? 'アーカイブ解除' : 'アーカイブ', () => patchThread(t.id, { archived: !t.archived }), t.archived)));

  return h('li', { class: `trow${t.id === S.threadId ? ' active' : ''}${t.archived ? ' archived' : ''}`, dataset: { id: t.id } },
    main, hasKids && !collapsed ? h('ul', { class: 'tkids' }, kids.map((k) => threadRow(k.t, { kids: k.kids }))) : null);
}

const THREADS_PER_PROJECT = 6;
function projectGroup(p) {
  const list = S.threads.filter((t) => t.project_id === p.id && (S.showArchived || !t.archived));
  const tree = buildTree(list);
  const collapsed = S.collapsedProjects.has(p.id);
  const all = S.expandedProjects.has(p.id);
  const shown = all ? tree : tree.filter((n, i) => i < THREADS_PER_PROJECT || treeHas(n, S.threadId));
  const current = p.id === S.projectId;
  return h('div', { class: `pgroup${current ? ' current' : ''}`, dataset: { project: p.id } },
    h('div', { class: 'pg-head' },
      h('button', {
        type: 'button', class: 'pg-toggle', 'aria-expanded': String(!collapsed), title: p.workspace_root,
        onclick: () => {
          collapsed ? S.collapsedProjects.delete(p.id) : S.collapsedProjects.add(p.id);
          ls.set('t3.pcollapsed', JSON.stringify([...S.collapsedProjects]));
          renderSidebar();
        },
      }, monogram(p), h('span', { class: 'pg-title', text: p.title }), iconEl('chevron', `pg-chev${collapsed ? '' : ' open'}`),
      h('span', { class: 'pg-count', text: list.length ? String(list.length) : '' })),
      ibtn('plus', `${p.title} で新規スレッド`, () => goHome(p.id), 'xs pg-add')),
    collapsed ? null : h('ul', { class: 'tlist' },
      shown.map((n) => threadRow(n.t, { kids: n.kids })),
      tree.length > shown.length || (all && tree.length > THREADS_PER_PROJECT) ? h('li', {}, h('button', {
        type: 'button', class: 'more', text: all ? '折りたたむ' : `さらに ${tree.length - shown.length} 件を表示`,
        onclick: () => { all ? S.expandedProjects.delete(p.id) : S.expandedProjects.add(p.id); renderSidebar(); },
      })) : null));
}

let sidebarSig = '';
function renderSidebar() {
  // skip DOM churn (and hover flicker) when nothing visible changed
  const sig = JSON.stringify([Math.floor(Date.now() / 60000), S.threadId, S.projectId, S.showArchived, [...S.collapsed], [...S.collapsedProjects], [...S.expandedProjects],
    [...S.pending].map(([k, v]) => [k, v.length]), S.projects.map((p) => [p.id, p.title]),
    S.threads.map((t) => [t.id, t.title, t.status, t.pinned, t.archived, t.updated_at, t.project_id, t.parent_thread_id, t.model_selection?.provider])]);
  const shelf = S.threads.filter((t) => !t.archived && (isBusy(t.status) && t.status !== 'waiting-for-children' || (S.pending.get(t.id) || []).length));
  document.title = `${[...S.pending.values()].some((a) => a.length) ? '⚠ ' : shelf.length ? `● ${shelf.length} ` : ''}${curThread() ? `${curThread().title} — ` : ''}t3code-rs`;
  if (sig === sidebarSig) return;
  sidebarSig = sig;
  shelf.sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at));
  $('#shelf').hidden = !shelf.length;
  $('#shelf-count').textContent = String(shelf.length);
  $('#shelf-list').replaceChildren(...shelf.map((t) => threadRow(t, { shelf: true })));
  $('#project-groups').replaceChildren(...(S.projects.length ? S.projects.map(projectGroup)
    : [h('div', { class: 'empty-note', text: 'プロジェクトを追加すると、ここにスレッドが並びます' })]));
}
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
  if (!S.threadId) renderHeroRecent();
}
const loadThreadsSoon = debounce(() => loadThreads().catch(() => {}), 300);

async function loadProjects() {
  S.projects = await api('/api/projects');
  if (!projectById(S.projectId)) S.projectId = S.projects[0]?.id || '';
  ls.set('t3.project', S.projectId);
  renderSidebar();
  if (!S.threadId) { renderHeader(); renderHero(); }
}

async function patchThread(id, body) {
  try {
    const t = await api(`/api/threads/${id}`, { method: 'PATCH', json: body });
    const i = S.threads.findIndex((x) => x.id === id);
    if (i >= 0 && t?.id) S.threads[i] = t;
    if (S.detail?.thread?.id === id && t?.id) S.detail.thread = t;
    renderSidebar(); renderHeader();
    await loadThreads();
  } catch (e) { toast(`更新に失敗: ${e.message}`, 'err'); }
}

// ───────────────────────── navigation ─────────────────────────
const draftKey = () => S.threadId || 'new';
function stashDraft() {
  const text = ta().value;
  if (text || S.atts.length) S.drafts.set(draftKey(), { text, atts: S.atts }); else S.drafts.delete(draftKey());
}
function restoreDraft() {
  const d = S.drafts.get(draftKey());
  ta().value = d?.text || '';
  S.atts = d?.atts || [];
  autosize(); renderAtts();
}
function setHash(hash) {
  try { history.replaceState(null, '', hash || location.pathname + location.search); } catch { location.hash = hash; }
}

/** Draft state for a new thread: hero + composer. The thread is created on first send. */
function goHome(projectId) {
  closePopover(); closeDrawer();
  stashDraft();
  S.threadId = null; S.detail = null; S.diffStat = null;
  if (projectId && projectById(projectId)) { S.projectId = projectId; ls.set('t3.project', projectId); }
  S.compose = { sel: defaultSelection(S.projectId), mode: 'approval-required', plan: false, isolation: 'shared', baseRef: '' };
  setHash('');
  restoreDraft();
  syncSubscriptions();
  renderAll();
  if (S.diffOpen) loadDiff();
  if (!isCoarse()) ta().focus();
}

async function openThread(id, { push = true } = {}) {
  if (!id) return;
  closePopover(); closeDrawer();
  if (push) setHash(`#/t/${id}`);
  if (id !== S.threadId) {
    stashDraft();
    S.threadId = id;
    S.detail = null; S.diffStat = null;
    restoreDraft();
    $('#main').dataset.view = 'thread';
    tl().replaceChildren(h('div', { class: 'loading', text: '読み込み中…' }));
    renderHeader(); renderApprovals();
  }
  S.stick = true;
  syncSubscriptions();
  await loadDetail(id, { reset: true });
}

async function loadDetail(id, { reset = false } = {}) {
  let d;
  try { d = await api(`/api/threads/${id}`); } catch (e) {
    if (id === S.threadId) {
      toast(`スレッドを読み込めません: ${e.message}`, 'err');
      if (e.status === 404) goHome();
      else if (!S.detail) tl().replaceChildren(h('div', { class: 'loading', text: '読み込みに失敗しました' }));
    }
    return;
  }
  if (id !== S.threadId) return;
  S.detail = d;
  pruneOptimistic(d);
  S.projectId = d.thread.project_id; ls.set('t3.project', S.projectId);
  const i = S.threads.findIndex((t) => t.id === id);
  if (i >= 0) S.threads[i] = d.thread; else S.threads.push(d.thread);
  if (reset) S.compose.sel = { ...d.thread.model_selection };
  renderAll();
  if (reset) {
    scrollBottom(true);
    if (S.diffOpen) loadDiff(); else refreshDiffStat();
  }
}
function pruneOptimistic(d) {
  const users = d.items.filter((x) => x.kind === 'user_message');
  S.optimistic = S.optimistic.filter((o) => {
    if (o.threadId !== d.thread.id) return true;
    return !users.some((u) => Date.parse(u.created_at) >= Number(o.key) - 10000
      && (o.text.startsWith('📎') || pstr(u.payload, 'text', 'prompt', 'content', 'message').trim() === o.text));
  });
}
const loadDetailSoon = debounce(() => { if (S.threadId) loadDetail(S.threadId); }, 250);

function renderAll() {
  $('#main').dataset.view = S.threadId ? 'thread' : 'hero';
  renderSidebar();
  renderHeader();
  renderTimeline();
  renderHero();
  renderApprovals();
  renderComposer();
  renderStatPill();
}

// ───────────────────────── topbar ─────────────────────────
const latestRun = () => { const r = S.detail?.runs || []; return r.length ? r[r.length - 1] : null; };

function renderHeader() {
  const t = curThread();
  const box = $('#tb-title');
  const cb = $('#children-bar');
  $$('.thread-only').forEach((b) => { b.hidden = !S.threadId; b.disabled = !t; });
  renderDiffBadge();
  if (box.querySelector('input.title-edit') && box.dataset.id === t?.id) return; // don't clobber an inline rename
  box.dataset.id = t?.id || '';
  const project = projectById(t ? t.project_id : S.projectId);
  const badge = project ? h('span', { class: 'proj-pill', title: project.workspace_root }, monogram(project, 'sm'), h('span', { text: project.title })) : null;
  if (!t) {
    box.replaceChildren(...[h('span', { class: 'tb-draft', text: S.threadId ? '' : '新規スレッド' }), S.threadId ? null : badge].filter(Boolean));
    cb.hidden = true; return;
  }
  const parent = t.parent_thread_id ? threadById(t.parent_thread_id) : null;
  box.replaceChildren(...[
    t.parent_thread_id ? h('button', { class: 'crumb', type: 'button', title: '親スレッドを開く', onclick: () => openThread(t.parent_thread_id) },
      h('span', { text: clip(parent?.title || t.parent_thread_id.slice(0, 8), 20) }), h('span', { class: 'sep', text: '/' })) : null,
    h('button', { type: 'button', id: 'thread-title', class: 'title-btn', title: 'クリックして名前を変更', text: t.title || '(無題)', onclick: renameInline }),
    badge,
    t.branch ? h('span', { class: 'branch-pill', title: t.worktree_path ? `worktree: ${t.worktree_path}` : 'ブランチ' }, iconEl('fork'), h('span', { text: t.branch }), t.worktree_path ? h('em', { text: 'worktree' }) : null) : null,
    ['idle', 'completed'].includes(t.status) ? null : h('span', { class: `status-pill s-${t.status}`, text: STATUS_LABEL[t.status] || t.status }),
  ].filter(Boolean));

  const kids = S.detail.children || [];
  cb.hidden = !kids.length;
  cb.replaceChildren(h('span', { class: 'muted', text: `子スレッド ${kids.length}` }),
    ...kids.map((k) => h('button', { class: 'child-chip', type: 'button', title: `${k.title} — ${STATUS_LABEL[k.status] || k.status}`, dataset: { child: k.id }, onclick: () => openThread(k.id) },
      provDot(k.model_selection?.provider, `${k.status === 'running' ? 'live' : ''} s-${k.status}`), h('span', { text: clip(k.title, 28) }))));
}

function renameInline() {
  const t = curThread(); const btn = $('#thread-title');
  if (!t || !btn) return;
  const inp = h('input', { class: 'title-edit', value: t.title, 'aria-label': 'スレッド名', maxLength: 200, spellcheck: false });
  let done = false;
  const finish = (save) => {
    if (done) return; done = true;
    const name = inp.value.trim();
    inp.remove(); renderHeader();
    if (save && name && name !== t.title) {
      const b = $('#thread-title'); if (b && S.threadId === t.id) b.textContent = name;
      patchThread(t.id, { title: name });
    }
  };
  inp.addEventListener('keydown', (e) => {
    if (e.isComposing || e.keyCode === 229) return;
    if (e.key === 'Enter') { e.preventDefault(); finish(true); } else if (e.key === 'Escape') { e.preventDefault(); finish(false); }
  });
  inp.addEventListener('blur', () => finish(true));
  btn.replaceWith(inp);
  inp.focus(); inp.select();
}

// ───────────────────────── hero (new thread) ─────────────────────────
function renderHero() {
  const hero = $('#hero'); const extra = $('#hero-extra');
  if (S.threadId) { hero.replaceChildren(); extra.replaceChildren(); return; }
  const project = projectById(S.projectId);
  hero.replaceChildren(...[
    h('h1', { text: '何を作りますか？' }),
    h('p', { class: 'hero-sub' }, project ? [h('b', { text: project.title }), h('span', { class: 'path', text: project.workspace_root })] : 'まずプロジェクト（作業ディレクトリ）を追加してください'),
    h('div', { class: 'proj-chips', role: 'radiogroup', 'aria-label': 'プロジェクト' },
      S.projects.map((p) => h('button', {
        type: 'button', role: 'radio', class: `proj-chip${p.id === S.projectId ? ' on' : ''}`, 'aria-checked': String(p.id === S.projectId), title: p.workspace_root, dataset: { project: p.id },
        onclick: () => {
          S.projectId = p.id; ls.set('t3.project', p.id);
          S.compose.sel = defaultSelection(p.id);
          renderSidebar(); renderHeader(); renderHero(); renderComposer(); ta().focus();
        },
      }, monogram(p, 'sm'), h('span', { text: p.title }))),
      h('button', { type: 'button', class: 'proj-chip add', onclick: projectDialog }, iconEl('plus'), h('span', { text: '追加' }))),
  ]);
  const base = h('input', { class: 'inp sm', placeholder: 'ベース ref（空欄で現在のブランチ）', value: S.compose.baseRef, hidden: S.compose.isolation !== 'worktree', spellcheck: false, autocapitalize: 'off', 'aria-label': 'ベース ref', oninput: (e) => { S.compose.baseRef = e.target.value; } });
  extra.replaceChildren(
    h('div', { class: 'iso-row' }, h('span', { class: 'muted', text: '作業ツリー' }),
      segmented([['shared', '共有', '現在の作業ディレクトリをそのまま使う'], ['worktree', 'git worktree', '別ブランチの作業ツリーを作る']], S.compose.isolation,
        (v) => { S.compose.isolation = v; base.hidden = v !== 'worktree'; }, { label: '作業ツリー', cls: 'mini' }), base),
    h('div', { id: 'hero-recent' }));
  renderHeroRecent();
}
function renderHeroRecent() {
  const box = $('#hero-recent');
  if (!box) return;
  const recent = S.threads.filter((t) => !t.archived).sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at)).slice(0, 4);
  box.replaceChildren(...[
    recent.length ? h('div', { class: 'recent' }, h('span', { class: 'recent-lab', text: '最近のスレッド' }), recent.map((t) => h('button', { type: 'button', class: 'recent-chip', title: t.title, onclick: () => openThread(t.id) },
      provDot(t.model_selection?.provider, `s-${t.status}`), h('span', { class: 't', text: t.title || '(無題)' }), h('small', { text: fmtAgo(t.updated_at) })))) : null,
    h('div', { class: 'hero-keys' }, [['K', '検索'], ['N', '新規'], ['B', 'サイドバー'], ['D', '差分']].map(([k, l]) => h('span', {}, h('kbd', { text: kbd(k) }), l))),
  ].filter(Boolean));
}

// ───────────────────────── timeline ─────────────────────────
const tl = () => $('#timeline');
const scroller = () => $('#tl-scroll');

function nearBottom() { const s = scroller(); return s.scrollHeight - s.scrollTop - s.clientHeight < 140; }
function scrollBottom(force) {
  const s = scroller();
  if (force || S.stick) { s.scrollTop = s.scrollHeight; S.stick = true; }
  $('#btn-jump').hidden = S.stick;
}

async function openAttachment(id, name, isImage) {
  // The desktop shell has no download handler; hand files to the system browser instead.
  if (!isImage && isDesktop()) { ipc(`open-external:${location.origin}/api/attachments/${encodeURIComponent(id)}`); return; }
  try {
    const url = await attachmentBlobUrl(id);
    if (isImage) {
      openModal(name, (body) => body.append(h('img', { class: 'lightbox', src: url, alt: name })), { cls: 'wide' });
    } else {
      const a = h('a', { href: url, download: name }); document.body.append(a); a.click(); a.remove();
    }
  } catch { toast('添付を開けません', 'err'); }
}
function attThumb(id) {
  const a = (S.detail?.attachments || []).find((x) => x.id === id);
  const name = a?.filename || '添付';
  if (a && !/^image\//.test(a.mime_type || '')) {
    return h('button', { class: 'att-file', type: 'button', title: `${name} をダウンロード`, onclick: () => openAttachment(id, name, false) },
      iconEl('file'), h('span', { text: name }), h('small', { text: fmtBytes(a.size_bytes) }));
  }
  const img = h('img', { alt: name });
  const btn = h('button', { class: 'att-thumb', type: 'button', title: name, onclick: () => openAttachment(id, name, true) }, img);
  attachmentBlobUrl(id).then((u) => { img.src = u; }).catch(() => { img.alt = `${name}（読み込み失敗）`; });
  return btn;
}

function fileEntries(p) {
  const raw = p?.files ?? p?.changes ?? p?.paths ?? (p?.path || p?.file_path ? [p] : []);
  const arr = Array.isArray(raw) ? raw : typeof raw === 'object' && raw ? Object.entries(raw).map(([path, v]) => ({ path, ...(typeof v === 'object' ? v : { kind: v }) })) : [];
  return arr.map((f) => (typeof f === 'string' ? { path: f } : f)).filter(Boolean)
    .map((f) => ({ ...f, path: f.path || f.file_path || f.file || f.name })).filter((f) => f.path);
}
function kindLetter(k) {
  k = String(k || 'M').toLowerCase();
  if (/^(a|add|new|creat|untracked|\?)/.test(k)) return 'A';
  if (/^(d|del|remov)/.test(k)) return 'D';
  if (/^(r|ren|mov)/.test(k)) return 'R';
  return 'M';
}

// ── compact task step cards ──
const STEP_KINDS = new Set(['tool_call', 'tool_result', 'command_execution', 'file_change', 'subagent', 'handoff']);
const TOOL_VERBS = [
  [/^(bash|shell|exec|exec_command|run_command|run_shell_command|command|local_shell)$/i, 'コマンド実行', 'terminal'],
  [/^(read|read_file|view|view_file|cat)$/i, 'ファイル読込', 'file'],
  [/^(edit|write|multiedit|multi_edit|notebookedit|write_file|edit_file|replace|patch|apply_patch|str_replace_based_edit_tool)$/i, 'ファイル編集', 'filePen'],
  [/^(grep|glob|search|ls|list|list_dir|list_directory|find|search_files|grep_search|toolsearch)$/i, '検索', 'search'],
  [/^(web_?search|web_?fetch|google_web_search|fetch|read_url)$/i, 'Web 参照', 'globe'],
  [/^(task|agent|subagent|delegate_task|mcp__.*delegate_task)$/i, 'サブエージェント', 'bot'],
];
const callIdOf = (p) => p?.tool_use_id || p?.callID || p?.call_id || p?.id || null;
const resultText = (p) => textOf(p?.output ?? p?.content ?? p?.result ?? p?.state?.output ?? p?.state?.error ?? p?.error ?? p?.text ?? p);
const isErrorResult = (p) => !!(p?.is_error || p?.error || p?.state?.status === 'error' || p?.status === 'failed');
const badge = (text, kind = '', title) => h('span', { class: `sbadge ${kind}`, text, title });

function pre(text, max = 40000) {
  const s = String(text ?? '');
  const el = h('div', { class: 'outbox' }, h('pre', { text: s.length > max ? `${s.slice(0, max)}\n… （表示は先頭 ${fmtNum(max)} 文字。コピーで全文）` : s }));
  el.append(ibtn('copy', 'コピー', () => copyText(s), 'xs ocopy'));
  return el;
}
const section = (label, node) => h('div', { class: 's-sec' }, h('div', { class: 's-lab', text: label }), node);

/** One single-line expandable row. The body is built lazily on first expand (outputs can be huge). */
function stepRow({ item, icon, verb, target, badges = [], sections = [], cls = '', trailing }) {
  const body = h('div', { class: 's-body' });
  const bytes = h('span', { class: 'sbytes' });
  const badgeBox = h('span', { class: 's-badges' }, badges, bytes);
  const el = h('details', { class: `step ${cls}`, dataset: { item: item.id } },
    h('summary', {}, iconEl(icon, 's-ic'), h('span', { class: 's-verb', text: verb }), h('span', { class: 's-target', text: target, title: target }), badgeBox, trailing, iconEl('chevron', 's-chev')),
    body);
  const queue = [...sections];
  const flush = () => { if (el.open) while (queue.length) body.append(queue.shift()()); };
  el.addEventListener('toggle', flush);
  let raw = 0; let stored = 0;
  el.addBytes = (it) => {
    raw += Number(it.raw_bytes) || 0;
    stored += Number(it.compressed_bytes ?? it.raw_bytes) || 0;
    if (raw < 1024) return;
    const squeezed = stored > 0 && stored < raw;
    bytes.className = `sbytes sbadge${squeezed ? ' zip' : ''}`;
    bytes.textContent = squeezed ? `${fmtBytes(raw)} → ${fmtBytes(stored)}` : fmtBytes(raw);
    bytes.title = squeezed ? `zstd 圧縮で ${((1 - stored / raw) * 100).toFixed(0)}% 削減` : 'ペイロードサイズ';
  };
  el.addSection = (fn) => { queue.push(fn); flush(); };
  el.addBadge = (b) => badgeBox.prepend(b);
  el.addBytes(item);
  return el;
}

function buildStep(item) {
  const p = item.payload;
  const sum = item.summary || '';
  switch (item.kind) {
    case 'tool_call': {
      const name = pstr(p, 'name', 'tool', 'tool_name', 'title') || (p?.type === 'web_search' ? 'web_search' : '') || sum || 'tool';
      const args = p?.input ?? p?.arguments ?? p?.args ?? p?.params ?? p?.state?.input ?? p;
      const found = typeof args === 'string' ? args
        : args?.command ?? args?.file_path ?? args?.filePath ?? args?.path ?? args?.query ?? args?.pattern ?? args?.url ?? args?.description ?? p?.query ?? p?.state?.title ?? '';
      const [, verb, icon] = TOOL_VERBS.find(([re]) => re.test(name)) || [null, 'ツール呼び出し', 'tool'];
      const specific = verb !== 'ツール呼び出し';
      const label = p?.server ? `${p.server} · ${name}` : name;
      const target = oneLine(textOf(found) || sum, 220);
      return stepRow({
        item, icon, verb, target: specific ? target || label : [label, target && target !== label ? target : ''].filter(Boolean).join('  '),
        cls: 'tool', sections: [() => section(`入力 · ${label}`, pre(pretty(args)))],
      });
    }
    case 'tool_result': {
      const out = resultText(p) || sum;
      const bad = isErrorResult(p);
      return stepRow({ item, icon: bad ? 'alert' : 'check', verb: 'ツール結果', target: oneLine(out, 220), badges: bad ? [badge('エラー', 'bad')] : [], cls: bad ? 'tool bad' : 'tool', sections: [() => section('出力', pre(out))] });
    }
    case 'command_execution': {
      const cmd = pstr(p, 'command', 'cmd', 'title') || (Array.isArray(p?.command) ? p.command.join(' ') : '') || sum || pstr(p, 'step_type');
      const out = [textOf(p?.output ?? p?.stdout ?? p?.aggregated_output ?? p?.state?.output), textOf(p?.stderr)].filter(Boolean).join('\n');
      const code = p?.exit_code ?? p?.exitCode ?? p?.code;
      const bad = code != null && Number(code) !== 0;
      return stepRow({
        item, icon: 'terminal', verb: 'コマンド実行', target: oneLine(cmd, 260), cls: bad ? 'cmd bad' : 'cmd',
        badges: code == null ? [] : [badge(`exit ${code}`, bad ? 'bad' : 'good')],
        sections: [() => section('コマンド', pre(cmd)), () => section('出力', out ? pre(out) : h('div', { class: 'muted', text: '出力はありません' }))],
      });
    }
    case 'file_change': {
      const files = fileEntries(p);
      const total = (...k) => files.reduce((a, f) => a + (Number(f[k[0]] ?? f[k[1]]) || 0), 0);
      const add = total('additions', 'added'); const del = total('deletions', 'removed');
      const target = files.length ? `${files[0].path}${files.length > 1 ? ` 他 ${files.length - 1} 件` : ''}` : oneLine(sum || pstr(p, 'step_type'), 220);
      return stepRow({
        item, icon: 'filePen', verb: 'ファイル変更', target, cls: 'fchg',
        badges: [add ? badge(`+${add}`, 'plus') : null, del ? badge(`−${del}`, 'minus') : null].filter(Boolean),
        sections: [() => (files.length ? h('div', { class: 'files' }, files.slice(0, 80).map((f) => {
          const L = kindLetter(f.kind ?? f.status ?? f.change_type);
          return h('button', { class: 'fchip', type: 'button', title: 'Git 差分を開く', onclick: () => setDiffOpen(true) },
            h('span', { class: `fk ${L}`, text: L }), h('span', { class: 'p', text: f.path }));
        })) : section('詳細', pre(pretty(p))))],
      });
    }
    case 'subagent': {
      const cid = pstr(p, 'child_thread_id', 'thread_id', 'id');
      const child = cid && threadById(cid);
      const st = child?.status || pstr(p, 'status') || 'idle';
      const title = child?.title || pstr(p, 'title', 'name', 'task') || sum || 'サブエージェント';
      const sel = child?.model_selection || (p?.provider ? { provider: p.provider, model: p.model } : null);
      const row = stepRow({
        item, icon: 'bot', verb: 'サブエージェント', target: title, cls: 'sub',
        badges: [sel ? h('span', { class: 'sbadge prov' }, provDot(sel.provider), provLabel(sel.provider)) : null, h('span', { class: `sbadge sub-status s-${st}`, text: STATUS_LABEL[st] || st })].filter(Boolean),
        trailing: child || /^[0-9a-f-]{36}$/i.test(cid) ? h('button', { class: 'btn xs', type: 'button', text: '開く', onclick: (e) => { e.preventDefault(); openThread(cid); } }) : null,
        sections: [() => section('詳細', pre(pretty(p)))],
      });
      row.dataset.child = cid;
      return row;
    }
    case 'handoff': {
      const text = pstr(p, 'summary', 'text', 'content') || sum || textOf(p);
      return stepRow({ item, icon: 'handoff', verb: 'コンテキスト引き継ぎ', target: oneLine(text, 160), cls: 'handoff', sections: [() => mdEl(text)] });
    }
    default:
      return null;
  }
}

const callRows = new Map(); // `${run}:${callId}` -> tool_call row, so results fold into their call

/** Appends an item to `container` (before `before`), grouping consecutive steps into one stack. */
function appendItem(item, container, before = null) {
  if (!STEP_KINDS.has(item.kind)) { container.insertBefore(renderMessage(item), before); return; }
  const cid = callIdOf(item.payload);
  const key = cid ? `${item.run_id}:${cid}` : null;
  if (item.kind === 'tool_result' && key && callRows.has(key)) {
    const row = callRows.get(key);
    callRows.delete(key);
    const out = resultText(item.payload) || item.summary || '';
    row.dataset.also = item.id;
    if (isErrorResult(item.payload)) { row.classList.add('bad'); row.addBadge(badge('エラー', 'bad')); }
    row.addBytes(item);
    row.addSection(() => section('出力', pre(out)));
    return;
  }
  let group = before ? before.previousElementSibling : container.lastElementChild;
  if (!group?.classList.contains('steps') || group.dataset.run !== item.run_id) {
    const g = h('div', { class: 'steps', dataset: { run: item.run_id } });
    g.append(h('button', {
      class: 'steps-more', type: 'button', hidden: true,
      onclick: () => { g.classList.toggle('expanded'); labelSteps(g); },
    }));
    container.insertBefore(g, before);
    group = g;
  }
  const row = buildStep(item);
  group.append(row);
  if (item.kind === 'tool_call' && key) callRows.set(key, row);
  labelSteps(group);
}
const STEPS_VISIBLE = 8;
function labelSteps(g) {
  const n = g.childElementCount - 1;
  const btn = g.firstElementChild;
  btn.hidden = n <= STEPS_VISIBLE;
  btn.textContent = g.classList.contains('expanded') ? '以前のステップを隠す' : `以前の ${n - STEPS_VISIBLE} ステップを表示`;
}

function renderMessage(item) {
  const p = item.payload;
  const kind = item.kind;
  const sum = item.summary || '';
  const id = { dataset: { item: item.id } };
  const time = () => h('time', { dateTime: item.created_at, title: new Date(item.created_at).toLocaleString('ja-JP'), text: new Date(item.created_at).toLocaleTimeString('ja-JP', { hour: '2-digit', minute: '2-digit' }) });
  switch (kind) {
    case 'user_message': {
      const text = pstr(p, 'text', 'prompt', 'content', 'message') || sum;
      const ids = Array.isArray(p?.attachment_ids) ? p.attachment_ids : [];
      return h('div', { class: 'msg user', ...id },
        ids.length ? h('div', { class: 'msg-atts' }, ids.map(attThumb)) : null,
        text ? h('div', { class: 'bubble', text }) : null,
        h('div', { class: 'msg-meta' }, time(), ibtn('copy', 'メッセージをコピー', () => copyText(text), 'xs')));
    }
    case 'assistant_message': {
      const text = pstr(p, 'text', 'content', 'message') || sum;
      return h('div', { class: 'msg assistant', ...id }, mdEl(text),
        h('div', { class: 'msg-meta' }, ibtn('copy', 'メッセージをコピー', () => copyText(text), 'xs'), time()));
    }
    case 'reasoning': {
      const text = pstr(p, 'text', 'content', 'summary') || sum || textOf(p);
      return h('details', { class: 'reasoning', ...id }, h('summary', {}, iconEl('brain'), h('span', { text: '思考プロセス' }), h('small', { text: `${fmtNum(text.length)}文字` }), iconEl('chevron', 's-chev')), h('pre', { text }));
    }
    case 'system_notice': {
      const text = pstr(p, 'message', 'text', 'content') || sum || textOf(p);
      const bad = /error|fail|エラー|失敗/i.test(text);
      return h('div', { class: `notice${bad ? ' err' : ''}`, ...id }, iconEl(bad ? 'alert' : 'info'), h('span', { text }));
    }
    default:
      return h('details', { class: 'reasoning', ...id }, h('summary', {}, iconEl('info'), h('span', { text: kind }), h('small', { text: oneLine(sum, 80) }), iconEl('chevron', 's-chev')), h('pre', { text: pretty(p) }));
  }
}

function runFooter(run) {
  if (!run || !TERMINAL_RUN.has(run.status)) return null;
  const ok = run.status === 'completed';
  return h('div', { class: `run-foot ${run.status}`, dataset: { run: run.id } },
    h('span', { class: 'rf-line' },
      provDot(run.provider),
      h('span', { text: `${run.model}${run.effort ? ` · ${run.effort}` : ''}` }),
      run.duration_ms != null ? h('span', { text: fmtDur(run.duration_ms) }) : null,
      run.input_tokens != null || run.output_tokens != null ? h('span', { title: '入力 / 出力トークン', text: `↑${fmtTok(run.input_tokens)} ↓${fmtTok(run.output_tokens)}` }) : null,
      ok ? null : h('span', { class: 'rf-status', text: STATUS_LABEL[run.status] || run.status })),
    run.status === 'failed' && run.error_message ? h('div', { class: 'rf-error', text: run.error_message }) : null);
}

function renderTimeline() {
  const root = tl();
  const d = S.detail;
  if (!d) { if (!S.threadId) root.replaceChildren(); callRows.clear(); return; }
  // keep what the reader had expanded (and their scroll position) across refreshes
  const same = root.dataset.thread === d.thread.id;
  const open = new Set(same ? $$('details[open][data-item]', root).map((e) => e.dataset.item) : []);
  const expanded = new Set(same ? $$('.steps.expanded', root).map((e) => e.dataset.run) : []);
  const top = scroller().scrollTop;
  root.dataset.thread = d.thread.id;
  callRows.clear();
  const frag = document.createDocumentFragment();
  // incoming handoff (this thread was forked from another)
  for (const ho of (d.handoffs || []).filter((x) => x.target_thread_id === d.thread.id)) {
    const src = threadById(ho.source_thread_id);
    frag.append(h('details', { class: 'handoff-in', dataset: { item: ho.id } }, h('summary', {},
      iconEl('handoff'),
      h('span', { class: 'k', text: '引き継ぎ元' }),
      h('span', { class: 'v', text: `${src?.title || ho.source_thread_id.slice(0, 8)}（${provLabel(ho.from_model?.provider)} ${ho.from_model?.model} → ${provLabel(ho.to_model?.provider)} ${ho.to_model?.model}）` }),
      h('button', { class: 'btn xs', type: 'button', text: '元を開く', onclick: (e) => { e.preventDefault(); openThread(ho.source_thread_id); } }),
      iconEl('chevron', 's-chev')),
    mdEl(ho.summary || '')));
  }
  const items = [...(d.items || [])].sort((a, b) => a.ordinal - b.ordinal);
  const runs = new Map((d.runs || []).map((r) => [r.id, r]));
  items.forEach((it, i) => {
    appendItem(it, frag);
    if (items[i + 1]?.run_id !== it.run_id) { const f = runFooter(runs.get(it.run_id)); if (f) frag.append(f); }
  });
  const lr = latestRun();
  if (lr && !items.some((it) => it.run_id === lr.id)) { const f = runFooter(lr); if (f) frag.append(f); }
  for (const o of S.optimistic) if (o.threadId === d.thread.id) frag.append(optimisticEl(o));
  frag.append(h('div', { id: 'live' }));
  if (!items.length && !isBusy(d.thread.status)) frag.prepend(h('div', { class: 'blank' }, h('p', { text: 'まだメッセージはありません。下の入力欄から始めてください。' })));
  $$('.steps', frag).forEach((g) => { if (expanded.has(g.dataset.run)) { g.classList.add('expanded'); labelSteps(g); } });
  $$('details[data-item]', frag).forEach((e) => { if (open.has(e.dataset.item)) e.open = true; });
  root.replaceChildren(frag);
  if (same && !S.stick) scroller().scrollTop = top;
  renderLive();
  scrollBottom();
}

function optimisticEl(o) { return h('div', { class: 'msg user pending', dataset: { opt: o.key } }, h('div', { class: 'bubble', text: o.text })); }

const renderLive = rafThrottle(() => {
  const host = $('#live');
  const t = curThread();
  if (!host || !t) return;
  const live = S.live.get(t.id);
  const stick = S.stick;
  let reas = $('.reasoning', host);
  let asst = $('.msg.assistant', host);
  let work = $('.working', host);
  if (live?.reasoning) {
    if (!reas) { reas = h('details', { class: 'reasoning live', open: true }, h('summary', {}, iconEl('brain'), h('span', { class: 'shimmer', text: '思考中…' }), h('small'), iconEl('chevron', 's-chev')), h('pre')); host.prepend(reas); }
    $('small', reas).textContent = `${fmtNum(live.reasoning.length)}文字`;
    const box = $('pre', reas); box.textContent = live.reasoning; box.scrollTop = box.scrollHeight;
  } else reas?.remove();
  if (live?.assistant) {
    if (!asst) { asst = h('div', { class: 'msg assistant live' }, h('div', { class: 'md' })); host.insertBefore(asst, work); }
    asst.firstChild.innerHTML = md(live.assistant);
  } else asst?.remove();
  if (isBusy(t.status)) {
    if (!work) { work = h('div', { class: 'working' }, h('span', { class: 'dots' }, h('i'), h('i'), h('i')), h('span', { class: 'w-label' }), h('span', { class: 'w-time' })); host.append(work); }
    $('.w-label', work).textContent = t.status === 'rate-limited' ? 'レート制限 — 自動再開を待っています' : t.status === 'waiting-for-children' ? '子スレッドの完了を待っています' : '作業中';
    tickTimer();
  } else work?.remove();
  if (stick) scrollBottom();
});
function tickTimer() {
  const el = $('#live .w-time');
  const run = latestRun();
  if (el) el.textContent = run && !TERMINAL_RUN.has(run.status) ? fmtDur(Date.now() - Date.parse(run.started_at)) : '';
}
setInterval(() => { if (isBusy(curThread()?.status)) tickTimer(); }, 1000);

function jumpToItem(itemId) {
  const el = $(`#timeline [data-item="${CSS.escape(itemId)}"], #timeline [data-also="${CSS.escape(itemId)}"]`);
  if (!el) return;
  const g = el.closest('.steps');
  if (g) { g.classList.add('expanded'); labelSteps(g); }
  if (el.tagName === 'DETAILS') el.open = true;
  S.stick = false;
  el.scrollIntoView({ block: 'center' });
  el.classList.add('flash');
  setTimeout(() => el.classList.remove('flash'), 1600);
}

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
    const ok = h('button', { class: 'btn primary', type: 'button', text: '承認 (Approve)', onclick: () => act(true) });
    const no = h('button', { class: 'btn danger-ghost', type: 'button', text: '拒否 (Reject)', onclick: () => act(false) });
    btns.push(ok, no);
    box.append(h('div', { class: 'approval', role: 'alert' },
      h('div', { class: 'ah' }, iconEl('alert'), h('span', { class: 't', text: req.title || '承認が必要です' }), h('span', { class: 'sbadge warn', text: req.kind })),
      cmd ? h('pre', { text: cmd }) : (p && Object.keys(p || {}).length ? h('pre', { text: pretty(p) }) : null),
      h('div', { class: 'ab' }, no, ok)));
  }
}
function dropPending(threadId, reqId) {
  const a = (S.pending.get(threadId) || []).filter((r) => r.id !== reqId);
  if (a.length) S.pending.set(threadId, a); else S.pending.delete(threadId);
  renderApprovals(); renderSidebarSoon();
}

// ───────────────────────── composer ─────────────────────────
const ta = () => $('#prompt');
let sending = false;

function renderComposer() {
  const t = curThread();
  const busy = t ? isBusy(t.status) : false;
  $$('.run-only').forEach((el) => { el.hidden = !busy; });
  $$('#behavior button').forEach((b) => { const on = b.dataset.b === S.behavior; b.classList.toggle('on', on); b.setAttribute('aria-checked', String(on)); });
  const mode = t ? t.runtime_mode : S.compose.mode;
  const plan = t ? t.interaction_mode === 'plan' : S.compose.plan;
  const perm = $('#btn-perm');
  perm.className = `chip-btn perm m-${mode}`;
  perm.title = `実行権限: ${MODE_LABEL[mode] || mode} — ${MODE_HINT[mode] || ''}`;
  perm.replaceChildren(iconEl(mode === 'full-access' ? 'alert' : 'shield'), h('span', { text: MODE_LABEL[mode] || mode }));
  $('#btn-plan-chip').hidden = !plan;
  const sel = S.compose.sel;
  const pill = $('#btn-model');
  pill.title = sel ? `${provLabel(sel.provider)} / ${sel.model}${sel.effort ? ` · effort ${sel.effort}` : ''}` : 'モデルを選択';
  pill.replaceChildren(...[
    provDot(sel?.provider),
    h('span', { class: 'mname', text: modelLabel(sel).replace(/^[\w-]+\//, '') }),
    sel?.effort ? h('span', { class: `effort e-${sel.effort}` }, h('i'), h('span', { text: sel.effort })) : null,
    iconEl('chevDown', 'mchev'),
  ].filter(Boolean));
  ta().placeholder = isNarrow() ? (busy ? '追加の指示…' : t ? '追加の指示を入力…' : '依頼内容を入力…')
    : busy ? (S.behavior === 'steer' ? '実行中のターンに指示を割り込ませる…' : '次のターンに予約する指示…')
      : t ? '追加の指示を入力（Shift+Enter で改行）' : 'やりたいことを入力（Enter で送信 / Shift+Enter で改行）';
  const send = $('#btn-send');
  const label = !busy ? '送信' : S.behavior === 'steer' ? '割り込み送信 (Steer)' : '予約送信 (Queue)';
  send.title = label; send.setAttribute('aria-label', label);
  send.disabled = sending || (!ta().value.trim() && !S.atts.length);
  $('#btn-stop').disabled = !t || !busy;
}
const syncSendEnabled = () => { $('#btn-send').disabled = sending || (!ta().value.trim() && !S.atts.length); };

function autosize() { const el = ta(); el.style.height = 'auto'; el.style.height = `${Math.min(el.scrollHeight, Math.max(120, window.innerHeight * 0.36))}px`; }

function removeAtt(a) {
  S.atts = S.atts.filter((x) => x !== a);
  for (const d of S.drafts.values()) d.atts = d.atts.filter((x) => x !== a);
  if (a.local) URL.revokeObjectURL(a.local);
  renderAtts();
}
function renderAtts() {
  const box = $('#att-chips');
  box.hidden = !S.atts.length;
  box.replaceChildren(...S.atts.map((a) => h('span', { class: `achip${a.uploading ? ' up' : ''}`, title: a.name },
    a.local ? h('img', { src: a.local, alt: '' }) : iconEl('file', 'aicon'),
    h('span', { class: 'meta' }, h('span', { class: 'n', text: a.name }), h('small', { text: a.uploading ? 'アップロード中…' : fmtBytes(a.size) })),
    ibtn('close', `${a.name} を削除`, () => removeAtt(a), 'xs'))));
  syncSendEnabled();
}

async function addFiles(files) {
  for (const f of files) {
    const a = { name: f.name || 'clipboard.png', size: f.size, mime: f.type, uploading: true, id: null, local: /^image\//.test(f.type) ? URL.createObjectURL(f) : null };
    S.atts = [...S.atts, a]; renderAtts();
    const fd = new FormData();
    fd.append('file', f, a.name);
    if (S.threadId) fd.append('thread_id', S.threadId);
    try {
      const res = await api('/api/attachments', { method: 'POST', form: fd });
      a.id = res.id; a.uploading = false;
    } catch (e) {
      removeAtt(a);
      toast(`アップロード失敗 (${a.name}): ${e.message}`, 'err');
    }
    renderAtts();
  }
}

async function send() {
  if (sending) return;
  const input = ta();
  const text = input.value.trim();
  if (!text && !S.atts.length) return;
  if (S.atts.some((a) => a.uploading)) { toast('添付のアップロード完了を待っています…'); return; }
  let t = curThread();
  if (S.threadId && !t) return; // still loading
  if (!t && !projectById(S.projectId)) { toast('先にプロジェクトを追加してください', 'err'); projectDialog(); return; }
  const sel = { ...S.compose.sel };
  if (!sel.model) { toast('モデルを選択してください', 'err'); openModelPopover($('#btn-model')); return; }
  const atts = S.atts.slice();
  sending = true; syncSendEnabled();
  try {
    if (!t) {
      try {
        t = await createThread({
          title: oneLine(text.replace(/^[\s#>*`-]+/, ''), 40) || '新規スレッド', model_selection: sel, runtime_mode: S.compose.mode,
          interaction_mode: S.compose.plan ? 'plan' : 'default', isolation: S.compose.isolation, base_ref: S.compose.baseRef.trim(),
        });
      } catch (e) { toast(`スレッド作成に失敗: ${e.message}`, 'err'); return; }
      input.value = ''; S.atts = []; S.drafts.delete('new');
      await openThread(t.id);
      S.compose.sel = sel;
    } else { input.value = ''; S.atts = []; }
    autosize(); renderAtts();
    const busy = isBusy(curThread()?.id === t.id ? curThread().status : t.status);
    const body = { prompt: text };
    if (busy) body.behavior = S.behavior;
    if (atts.length) body.attachment_ids = atts.map((a) => a.id);
    if (!sameSel(sel, t.model_selection)) body.model_selection = sel.provider === t.model_selection?.provider && t.model_selection.agent ? { ...sel, agent: t.model_selection.agent } : sel;
    const key = String(Date.now());
    const opt = { key, threadId: t.id, text: text || `📎 ${atts.length}件の添付` };
    S.optimistic.push(opt);
    if (S.threadId === t.id) { $('#timeline .blank')?.remove(); $('#live')?.before(optimisticEl(opt)); scrollBottom(true); }
    try {
      await api(`/api/threads/${t.id}/messages`, { method: 'POST', json: body });
      loadDetailSoon();
    } catch (e) {
      S.optimistic = S.optimistic.filter((o) => o.key !== key);
      $(`[data-opt="${key}"]`)?.remove();
      if (S.threadId === t.id) { input.value = text; S.atts = atts; autosize(); renderAtts(); } else S.drafts.set(t.id, { text, atts });
      toast(`送信に失敗: ${e.message}`, 'err');
    }
  } finally { sending = false; renderComposer(); }
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
  try { await navigator.clipboard.writeText(text); toast('コピーしました', 'ok', 1500); return true; } catch { /* fall through */ }
  const x = h('textarea', { value: text, style: 'position:fixed;opacity:0' });
  (document.querySelector('dialog[open]') || document.body).append(x); x.select();
  let ok = false;
  try { ok = document.execCommand('copy'); } catch { /* noop */ }
  x.remove();
  toast(ok ? 'コピーしました' : 'コピーできません', ok ? 'ok' : 'err', 1500);
  return ok;
}

// ───────────────────────── session stats pill ─────────────────────────
async function loadDbStats() {
  try { S.dbStats = await api('/api/stats/db'); } catch { S.dbStats = null; }
  renderStatPill();
}
const loadDbStatsSoon = debounce(loadDbStats, 800);

function renderStatPill() {
  const box = $('#stat-pill');
  const parts = [];
  if (!S.wsOk) parts.push(h('span', { class: 'sp off', text: '再接続中…' }));
  const runs = S.detail?.runs;
  if (runs) {
    const sum = (k) => runs.reduce((a, r) => a + (Number(r[k]) || 0), 0);
    parts.push(h('span', { class: 'sp', title: 'このスレッドの Run 数', text: `${fmtNum(runs.length)} runs` }),
      h('span', { class: 'sp', title: 'このスレッドの入力 / 出力トークン', text: `↑${fmtTok(sum('input_tokens'))} ↓${fmtTok(sum('output_tokens'))}` }));
  } else if (S.dbStats) {
    parts.push(h('span', { class: 'sp', text: `${fmtNum(S.dbStats.threads_count)} スレッド` }), h('span', { class: 'sp', text: `${fmtNum(S.dbStats.runs_count)} runs` }));
  }
  if (S.dbStats) {
    const raw = Number(S.dbStats.raw_payload_bytes) || 0; const stored = Number(S.dbStats.stored_payload_bytes) || 0;
    const saved = Math.max(0, raw - stored);
    parts.push(h('span', { class: 'sp zstd', title: `DB 全体の zstd 圧縮: ${fmtBytes(raw)} → ${fmtBytes(stored)}`, text: `zstd −${fmtBytes(saved)}${raw ? ` (${((saved / raw) * 100).toFixed(0)}%)` : ''}` }));
  }
  if (S.meta) {
    const clean = S.meta.telemetry_enabled === false && S.meta.privacy?.telemetry_enabled !== true;
    parts.push(h('span', { class: `sp shield ${clean ? 'good' : 'bad'}` }, iconEl(clean ? 'shield' : 'alert'), clean ? 'ゼロテレメトリ' : 'テレメトリ有効'));
  }
  if (!parts.length) parts.push(h('span', { class: 'sp', text: '統計' }));
  box.replaceChildren(...parts);
}

// ───────────────────────── dialogs ─────────────────────────
function field(label, control, hint) {
  return h('div', { class: 'field' }, h('div', { class: 'lab', text: label }), control, hint ? h('div', { class: 'hint', text: hint }) : null);
}

function projectDialog() {
  closeDrawer();
  openModal('プロジェクト', (body, close) => {
    const title = h('input', { class: 'inp', placeholder: '例: t3code-rs', required: true, 'aria-label': '名前' });
    const root = h('input', { class: 'inp mono', placeholder: '/Users/you/src/project', required: true, autocapitalize: 'off', spellcheck: false, 'aria-label': 'ワークスペースのパス' });
    const list = h('div', { class: 'plist' });
    const renderList = () => {
      list.replaceChildren(h('div', { class: 'sec-h', text: `登録済み (${S.projects.length})` }), ...S.projects.map((p) => h('div', { class: 'pl-row' },
        monogram(p),
        h('div', { class: 'pl-main' }, h('b', { text: p.title }), h('small', { text: p.workspace_root })),
        h('button', {
          class: 'btn xs danger-ghost', type: 'button', text: '削除',
          onclick: async (e) => {
            const b = e.currentTarget;
            // two-step confirm (window.confirm is unavailable in some webviews)
            if (!b.dataset.armed) { b.dataset.armed = '1'; b.textContent = '本当に削除'; setTimeout(() => { delete b.dataset.armed; b.textContent = '削除'; }, 3000); return; }
            try {
              await api(`/api/projects/${p.id}`, { method: 'DELETE' });
              await loadProjects(); await loadThreads(); renderList();
              if (curThread() && !projectById(curThread().project_id)) goHome();
            } catch (err) { toast(`削除に失敗: ${err.message}`, 'err'); }
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
          close(); goHome(p.id); toast('プロジェクトを追加しました', 'ok');
        } catch (err) { toast(`追加に失敗: ${err.message}`, 'err'); }
      },
    }, field('名前', title), field('ワークスペースのパス', root, 'サーバーから見える絶対パス（git リポジトリ推奨）。削除してもファイルは消えません。'),
    h('div', { class: 'actions' }, h('button', { class: 'btn primary', type: 'submit', text: '追加' }))), list);
    setTimeout(() => title.focus(), 0);
  });
}

function forkDialog() {
  const t = curThread(); if (!t) return;
  openModal('分岐・モデル切替', (body, close) => {
    const title = h('input', { class: 'inp', value: `${t.title} (分岐)`, 'aria-label': 'タイトル' });
    const picker = modelPanel({ value: S.compose.sel || t.model_selection });
    const memo = h('textarea', { class: 'inp', rows: 3, placeholder: '空欄ならサーバー側が会話履歴から引き継ぎ要約を作成します', 'aria-label': '引き継ぎメモ' });
    const go = h('button', { class: 'btn primary', type: 'submit', text: '分岐して開く' });
    body.append(h('form', {
      class: 'form',
      onsubmit: async (e) => {
        e.preventDefault(); if (e.submitter !== go) return;
        go.disabled = true;
        const req = { title: title.value.trim() || undefined, target_model: picker.get() };
        if (memo.value.trim()) req.handoff_summary = memo.value.trim();
        try {
          const r = await api(`/api/threads/${t.id}/fork`, { method: 'POST', json: req });
          const nt = r.thread || r;
          close(); toast('分岐しました', 'ok');
          await loadThreads(); await openThread(nt.id);
        } catch (err) { go.disabled = false; toast(`分岐に失敗: ${err.message}`, 'err'); }
      },
    }, h('p', { class: 'muted', text: `現在: ${provLabel(t.model_selection?.provider)} / ${t.model_selection?.model}。会話の文脈を引き継いだ新しいスレッドを、選んだプロバイダ・モデルで作ります。元のスレッドはそのまま残ります。` }),
    field('新スレッドのタイトル', title), field('分岐先のモデル', h('div', { class: 'mp-embed' }, picker.el)), field('引き継ぎメモ（任意）', memo),
    h('div', { class: 'actions' }, go)));
  }, { cls: 'form-modal' });
}

function delegateDialog() {
  const t = curThread(); if (!t) return;
  openModal('並列委譲（子スレッド）', (body, close) => {
    const draft = ta().value.trim();
    const title = h('input', { class: 'inp', placeholder: '例: テストを書く', 'aria-label': 'タイトル' });
    const task = h('textarea', { class: 'inp', rows: 4, required: true, placeholder: '子エージェントへの指示（背景・完了条件を含める）', value: draft, 'aria-label': 'タスク' });
    const picker = modelPanel({ value: S.compose.sel || t.model_selection });
    let isolation = 'shared'; let mode = t.runtime_mode || 'approval-required'; let plan = t.interaction_mode === 'plan';
    const base = h('input', { class: 'inp mono', placeholder: 'ベース ref（空欄で現在のブランチ）', hidden: true, spellcheck: false, 'aria-label': 'ベース ref' });
    const iso = segmented([['shared', '共有 (shared)'], ['worktree', 'git worktree（並列編集向け）']], isolation, (v) => { isolation = v; base.hidden = v !== 'worktree'; }, { label: '作業ツリー' });
    const go = h('button', { class: 'btn primary', type: 'submit', text: '委譲を開始' });
    body.append(h('form', {
      class: 'form',
      onsubmit: async (e) => {
        e.preventDefault(); if (e.submitter !== go) return;
        if (!task.value.trim()) { task.focus(); return; }
        go.disabled = true;
        const spec = {
          parent_thread_id: t.id, title: title.value.trim() || oneLine(task.value, 40), task: task.value.trim(),
          model_selection: picker.get(), isolation, base_ref: base.value.trim() || null, runtime_mode: mode, interaction_mode: plan ? 'plan' : 'default',
        };
        try {
          const r = await api(`/api/threads/${t.id}/delegate`, { method: 'POST', json: spec });
          close(); toast(`子スレッドを開始しました: ${r.thread?.title || ''}`, 'ok');
          if (draft && ta().value.trim() === draft) { ta().value = ''; autosize(); syncSendEnabled(); }
          await loadThreads(); loadDetailSoon();
        } catch (err) { go.disabled = false; toast(`委譲に失敗: ${err.message}`, 'err'); }
      },
    }, field('タイトル', title), field('タスク', task), field('子のモデル', h('div', { class: 'mp-embed' }, picker.el)),
    field('作業ツリー', h('div', { class: 'stack' }, iso, base), 'worktree は親と別ブランチで編集するため、複数の子を同時に走らせても衝突しません'),
    permPanel({ mode, plan, maxMode: t.runtime_mode, planForced: t.interaction_mode === 'plan', onMode: (v) => { mode = v; }, onPlan: (v) => { plan = v; } }),
    h('div', { class: 'actions' }, go)));
    setTimeout(() => task.focus(), 0);
  }, { cls: 'form-modal' });
}

function searchDialog() {
  const open = $('dialog.search');
  if (open) { open.close(); return; }
  closeDrawer();
  openModal('全文検索', (body, close) => {
    const inp = h('input', { class: 'srch-input', type: 'text', enterKeyHint: 'search', placeholder: '会話・コマンド出力を検索（日本語対応）', autocomplete: 'off', spellcheck: false, 'aria-label': '検索語' });
    const list = h('ul', { class: 'hits', role: 'listbox' });
    const note = h('div', { class: 'srch-note' });
    let hits = []; let sel = 0; let seq = 0;
    const mark = (snip) => esc(snip).replace(/&lt;(\/?)(mark|b)&gt;/gi, '<$1mark>');
    const draw = () => {
      list.replaceChildren(...hits.map((x, i) => {
        const th = threadById(x.thread_id);
        const li = h('li', { class: `hit${i === sel ? ' sel' : ''}`, role: 'option', onclick: () => go(x), onmousemove: () => { if (sel !== i) { sel = i; $$('.hit', list).forEach((e, k) => e.classList.toggle('sel', k === i)); } } },
          h('div', { class: 'ht' }, th ? provDot(th.model_selection?.provider) : null, h('span', { text: x.thread_title || '(無題)' }), th ? h('em', { text: projectTitle(th.project_id) }) : null, h('small', { text: fmtAgo(x.updated_at) })),
          x.snippet ? h('div', { class: 'sn' }) : null);
        if (x.snippet) li.querySelector('.sn').innerHTML = mark(x.snippet);
        return li;
      }));
    };
    const recent = () => {
      hits = S.threads.filter((t) => !t.archived).sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at)).slice(0, 8)
        .map((t) => ({ thread_id: t.id, thread_title: t.title, updated_at: t.updated_at }));
      sel = 0; draw(); note.textContent = hits.length ? '最近のスレッド' : 'スレッドはまだありません';
    };
    const go = async (x) => { close(); await openThread(x.thread_id); if (x.item_id) requestAnimationFrame(() => jumpToItem(x.item_id)); };
    const run = debounce(async () => {
      const q = inp.value.trim();
      const my = ++seq;
      if (!q) { recent(); return; }
      try {
        const r = await api(`/api/search?q=${encodeURIComponent(q)}&limit=30`);
        if (my !== seq) return;
        hits = Array.isArray(r) ? r : []; sel = 0; draw();
        note.textContent = hits.length ? `${hits.length}件` : '一致なし';
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
    body.append(h('div', { class: 'srch-row' }, iconEl('search'), inp, h('kbd', { text: 'esc' })), note, list);
    recent();
    setTimeout(() => inp.focus(), 0);
  }, { cls: 'search' });
}

const scard = (l, v) => h('div', { class: 'scard' }, h('div', { class: 'sl', text: l }), h('div', { class: 'sv', text: v }));

function statsDialog() {
  if ($('dialog.stats')) return;
  closeDrawer();
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
        if ((pv.notes || []).length) out.push(h('ul', { class: 'notes' }, pv.notes.map((n) => h('li', { text: n }))));
      } else out.push(h('div', { class: 'errtxt', text: `取得に失敗: ${meta.reason.message}` }));
      // ── providers
      if (S.providers.length) {
        out.push(h('div', { class: 'sec-h', text: 'プロバイダ' }), h('div', { class: 'provs' }, S.providers.map((p) => h('div', { class: `prov-card${p.available ? '' : ' off'}`, title: p.error || p.binary_path || '' },
          provDot(p.provider), h('b', { text: provLabel(p.provider) }), h('small', { text: p.available ? p.version || '利用可能' : p.error || '未導入' })))));
      }
      // ── storage
      out.push(h('div', { class: 'sec-h', text: 'ストレージ（SQLite + zstd 圧縮）' }));
      if (db.status === 'fulfilled') {
        const d = db.value;
        S.dbStats = d;
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
      out.push(h('div', { class: 'sec-h' }, h('span', { text: 'orch-stats（プロバイダ / モデル別）' }), h('button', { class: 'btn xs', type: 'button', text: '更新', onclick: render })));
      if (orch.status === 'fulfilled') {
        const rows = [...orch.value].sort((a, b) => b.total_runs - a.total_runs);
        if (!rows.length) out.push(h('div', { class: 'muted', text: 'まだ実行履歴がありません' }));
        else {
          const tot = rows.reduce((a, r) => ({ n: a.n + r.total_runs, ok: a.ok + r.completed_runs, f: a.f + r.failed_runs, i: a.i + r.total_input_tokens, o: a.o + r.total_output_tokens }), { n: 0, ok: 0, f: 0, i: 0, o: 0 });
          const rate = (r) => (r.total_runs > 0 ? r.completed_runs / r.total_runs : 0);
          const rateCell = (v, n) => { const i = h('i', { class: v >= .9 ? 'ok' : v >= .6 ? 'warn' : 'err' }); i.style.width = `${v * 100}%`; return h('td', { class: 'rate' }, h('div', { class: 'bar' }, i), n ? `${(v * 100).toFixed(0)}%` : '–'); };
          const tb = h('tbody');
          for (const r of rows) {
            tb.append(h('tr', {}, h('td', {}, provDot(r.provider), ` ${provLabel(r.provider)} / ${r.model}`), h('td', { text: `${fmtNum(r.total_runs)}` }), h('td', { text: `${fmtNum(r.completed_runs)} / ${fmtNum(r.failed_runs)}` }),
              rateCell(rate(r), r.total_runs), h('td', { text: fmtDur(r.avg_duration_ms) }), h('td', { text: fmtTok(r.total_input_tokens) }), h('td', { text: fmtTok(r.total_output_tokens) })));
          }
          tb.append(h('tr', { class: 'total' }, h('td', { text: '合計' }), h('td', { text: fmtNum(tot.n) }), h('td', { text: `${fmtNum(tot.ok)} / ${fmtNum(tot.f)}` }),
            rateCell(tot.n ? tot.ok / tot.n : 0, tot.n), h('td'), h('td', { text: fmtTok(tot.i) }), h('td', { text: fmtTok(tot.o) })));
          out.push(h('div', { class: 'twrap' }, h('table', { class: 'tbl' },
            h('thead', {}, h('tr', {}, ['モデル', '実行', '成功 / 失敗', '成功率', '平均所要', '入力tok', '出力tok'].map((x) => h('th', { text: x })))), tb)));
        }
      } else out.push(h('div', { class: 'errtxt', text: `取得に失敗: ${orch.reason.message}` }));
      body.replaceChildren(...out);
      renderStatPill();
    };
    render();
  }, { cls: 'wide stats' });
}

// ───────────────────────── diff panel ─────────────────────────
let diffSeq = 0;
let diffShownFor = null;
const diffOpenFiles = new Set();

function setDiffOpen(open) {
  S.diffOpen = open;
  ls.set('t3.diff', open ? '1' : '0');
  $('#diff-panel').hidden = !open;
  $('#app').dataset.diff = open ? 'open' : 'closed';
  $('#btn-diff').setAttribute('aria-pressed', String(open));
  if (open) loadDiff();
}

function renderDiffBadge() {
  const b = $('#diff-badge');
  const d = S.diffStat;
  const dirty = !!d && d.threadId === S.threadId && d.files > 0;
  b.hidden = !dirty;
  if (dirty) b.replaceChildren(h('span', { class: 'plus', text: `+${d.add}` }), h('span', { class: 'minus', text: `−${d.del}` }));
}
const fetchDiff = (id) => api(`/api/threads/${id}/diff?ignore_whitespace=${S.ignoreWs}`);
function noteDiffStat(threadId, d) {
  S.diffStat = { threadId, add: d.total_additions, del: d.total_deletions, files: d.clean ? 0 : d.files.length };
  renderDiffBadge();
}
async function refreshDiffStat() {
  const id = S.threadId; if (!id) return;
  try { const d = await fetchDiff(id); if (id === S.threadId) noteDiffStat(id, d); } catch { /* not a git repository: no badge */ }
}

async function loadDiff() {
  const t = curThread();
  const body = $('#diff-body'); const sum = $('#diff-sum'); const branch = $('#diff-branch');
  const my = ++diffSeq;
  if (!t) { diffShownFor = null; body.replaceChildren(h('div', { class: 'dp-empty', text: 'スレッドを選択すると、作業ツリーの差分が表示されます' })); sum.replaceChildren(); branch.textContent = ''; return; }
  if (diffShownFor !== t.id) { diffOpenFiles.clear(); sum.replaceChildren(); branch.textContent = ''; body.replaceChildren(h('div', { class: 'dp-empty', text: '読み込み中…' })); }
  try {
    const d = await fetchDiff(t.id);
    if (my !== diffSeq) return;
    diffShownFor = t.id;
    noteDiffStat(t.id, d);
    sum.replaceChildren(h('span', { class: 'sbadge plus', text: `+${d.total_additions}` }), h('span', { class: 'sbadge minus', text: `−${d.total_deletions}` }), h('span', { class: 'muted', text: `${d.files.length} ファイル` }));
    branch.textContent = d.branch ? `⎇ ${d.branch}` : '';
    if (d.clean || !d.files.length) { body.replaceChildren(h('div', { class: 'dp-empty' }, iconEl('check'), h('span', { text: '変更はありません（クリーン）' }))); return; }
    const top = body.scrollTop;
    body.replaceChildren(...d.files.map(diffFile));
    body.scrollTop = top;
  } catch (e) {
    if (my !== diffSeq) return;
    diffShownFor = null; S.diffStat = null; renderDiffBadge();
    sum.replaceChildren(); branch.textContent = '';
    body.replaceChildren(h('div', { class: 'dp-empty errtxt', text: `差分を取得できません: ${e.message}` }));
  }
}
const refreshDiffSoon = debounce(() => { if (S.diffOpen) loadDiff(); else refreshDiffStat(); }, 600);

function patchLines(patch) {
  const lines = String(patch || '').split('\n');
  if (lines.at(-1) === '') lines.pop();
  const frag = document.createDocumentFragment();
  let a = 0; let b = 0; let inHunk = false;
  const row = (cls, o, n, sign, text) => frag.append(h('div', { class: `dl ${cls}` },
    h('span', { class: 'ln', text: o }), h('span', { class: 'ln', text: n }), h('span', { class: 'sg', text: sign }), h('span', { class: 'tx', text })));
  for (const l of lines) {
    const hunk = /^@@+ -(\d+)(?:,\d+)? (?:-\d+(?:,\d+)? )*\+(\d+)/.exec(l);
    if (hunk) { a = +hunk[1]; b = +hunk[2]; inHunk = true; frag.append(h('div', { class: 'dl hunk', text: l })); continue; }
    if (l.startsWith('diff ')) inHunk = false;
    if (!inHunk) { if (/^(rename|copy|new file|deleted file|old mode|new mode|Binary)/.test(l)) frag.append(h('div', { class: 'dl meta', text: l })); continue; }
    if (l.startsWith('+')) row('add', '', b++, '+', l.slice(1));
    else if (l.startsWith('-')) row('del', a++, '', '−', l.slice(1));
    else if (l.startsWith('\\')) frag.append(h('div', { class: 'dl meta', text: l }));
    else row('', a++, b++, '', l.slice(1));
  }
  if (!frag.childNodes.length) frag.append(h('div', { class: 'dl meta', text: '（パッチ本文なし：バイナリ or 空白のみの変更）' }));
  return h('div', { class: 'patch' }, h('div', { class: 'patch-in' }, frag));
}

function diffFile(f) {
  const L = kindLetter(f.status);
  const slash = f.path.lastIndexOf('/');
  const d = h('details', { class: 'dfile', dataset: { path: f.path } }, // collapsed by default
    h('summary', {}, iconEl('chevron', 's-chev'), h('span', { class: `fk ${L}`, text: L }),
      h('span', { class: 'fp', title: f.path }, h('b', { text: f.path.slice(slash + 1) }), slash > 0 ? h('small', { text: f.path.slice(0, slash) }) : null),
      ibtn('copy', 'パスをコピー', (e) => { e.preventDefault(); e.stopPropagation(); copyText(f.path); }, 'xs dcopy'),
      h('span', { class: 'sbadge plus', text: `+${f.additions}` }), h('span', { class: 'sbadge minus', text: `−${f.deletions}` })));
  let drawn = false;
  const draw = () => { if (d.open && !drawn) { drawn = true; d.append(patchLines(f.patch)); } };
  d.addEventListener('toggle', () => { d.open ? diffOpenFiles.add(f.path) : diffOpenFiles.delete(f.path); draw(); });
  if (diffOpenFiles.has(f.path)) { d.open = true; draw(); }
  return d;
}

// ───────────────────────── WebSocket ─────────────────────────
let backoff = 500; let reconnectTimer = null; let pingTimer = null;

function setConn(kind, text) {
  const c = $('#conn');
  c.className = `conn ${kind}`;
  c.title = text;
  $('#conn-text').textContent = text;
}
function wsSend(obj) { if (S.ws && S.ws.readyState === WebSocket.OPEN) S.ws.send(JSON.stringify(obj)); }

function syncSubscriptions() {
  const want = new Set();
  if (S.threadId) want.add(S.threadId);
  for (const t of S.threads) if (isBusy(t.status)) want.add(t.id);
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
    renderStatPill();
    syncSubscriptions();
    clearInterval(pingTimer);
    pingTimer = setInterval(() => wsSend({ type: 'ping' }), 25000);
    // catch up on anything missed while disconnected
    loadThreadsSoon(); if (S.threadId) loadDetailSoon();
  };
  ws.onmessage = (ev) => { let m; try { m = JSON.parse(ev.data); } catch { return; } try { handleWs(m); } catch (e) { console.error('ws handler', e); } };
  ws.onclose = () => { S.wsOk = false; S.subscribed.clear(); clearInterval(pingTimer); setConn('retry', `再接続中… (${(backoff / 1000).toFixed(1)}s)`); renderStatPill(); scheduleReconnect(); };
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
        if (isBusy(prev) && !isBusy(m.status)) { loadDetailSoon(); refreshDiffSoon(); }
      }
      if (!isBusy(m.status) && S.pending.has(m.thread_id)) { S.pending.delete(m.thread_id); renderApprovals(); }
      break;
    }
    case 'run_started': {
      S.live.set(m.thread_id, { runId: m.run.id, assistant: '', reasoning: '' });
      setThreadStatus(m.thread_id, 'running');
      renderSidebarSoon();
      if (m.thread_id === cur && S.detail) {
        if (!S.detail.runs.some((r) => r.id === m.run.id)) S.detail.runs.push(m.run);
        renderHeader(); renderComposer(); renderStatPill();
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
        if (m.thread_id === cur) { loadDetailSoon(); refreshDiffSoon(); }
        loadThreadsSoon(); loadDbStatsSoon();
      } else if (m.thread_id === cur) { renderHeader(); renderLive(); }
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
      if (it.kind === 'user_message') {
        const k = S.optimistic.findIndex((x) => x.threadId === m.thread_id);
        if (k >= 0) { const [o] = S.optimistic.splice(k, 1); $(`[data-opt="${o.key}"]`)?.remove(); }
      }
      S.stick = nearBottom() || S.stick;
      const host = $('#live');
      if (host) {
        $('#timeline .blank')?.remove();
        // optimistic bubbles stay below the items that were sent before them
        appendItem(it, tl(), $('#timeline > .msg.user.pending') || host);
      }
      renderLive(); scrollBottom();
      if (it.kind === 'file_change') refreshDiffSoon();
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
        $$(`.step.sub[data-child="${CSS.escape(m.child_thread_id)}"] .sub-status`).forEach((el) => {
          el.className = `sbadge sub-status s-${m.status}`;
          el.textContent = STATUS_LABEL[m.status] || m.status;
        });
      }
      syncSubscriptions(); renderSidebarSoon();
      break;
    }
    case 'error':
      toast(`サーバーエラー: ${m.message}`, 'err', 6000);
      break;
    default: break; // pong
  }
}

// ───────────────────────── sidebar visibility ─────────────────────────
function openDrawer() { $('#app').dataset.drawer = 'open'; $('#scrim').hidden = false; }
function closeDrawer() { $('#app').dataset.drawer = 'closed'; $('#scrim').hidden = true; }
function toggleSidebar() {
  closePopover();
  const app = $('#app');
  if (isNarrow()) { app.dataset.drawer === 'open' ? closeDrawer() : openDrawer(); return; }
  app.dataset.sb = app.dataset.sb === 'collapsed' ? 'open' : 'collapsed';
  ls.set('t3.sb', app.dataset.sb);
}

// ───────────────────────── wiring ─────────────────────────
function wire() {
  wireDesktop();
  hydrateIcons();
  applyTheme();
  $$('[data-kbd]').forEach((el) => { el.title = `${el.dataset.title} (${kbd(el.dataset.kbd)})`; });
  $$('[data-kbd-label]').forEach((el) => { el.textContent = kbd(el.dataset.kbdLabel); });
  $('#app').dataset.sb = ls.get('t3.sb', 'open') === 'collapsed' ? 'collapsed' : 'open';
  $('#chk-ws').checked = S.ignoreWs;
  $('#btn-archived').setAttribute('aria-pressed', String(S.showArchived));

  $('#btn-theme').onclick = (e) => themeMenu(e.currentTarget);
  $('#btn-sb-open').onclick = toggleSidebar;
  $('#btn-sb-collapse').onclick = toggleSidebar;
  $('#scrim').onclick = closeDrawer;
  $('#btn-home').onclick = () => goHome();
  $('#btn-new-thread').onclick = () => goHome();
  $('#btn-add-project').onclick = projectDialog;
  $('#btn-search').onclick = searchDialog;
  $('#btn-sb-search').onclick = searchDialog;
  $('#btn-stats').onclick = statsDialog;
  $('#btn-top-stats').onclick = statsDialog;
  $('#stat-pill').onclick = statsDialog;
  $('#btn-archived').onclick = (e) => {
    S.showArchived = !S.showArchived; ls.set('t3.archived', S.showArchived ? '1' : '0');
    e.currentTarget.setAttribute('aria-pressed', String(S.showArchived));
    renderSidebar(); loadThreads().catch(() => {});
  };
  $('#btn-diff').onclick = () => setDiffOpen(!S.diffOpen);
  $('#btn-diff-close').onclick = () => setDiffOpen(false);
  $('#btn-diff-refresh').onclick = loadDiff;
  $('#btn-diff-expand').onclick = () => {
    const all = $$('#diff-body details.dfile');
    const anyClosed = all.some((d) => !d.open);
    all.forEach((d) => { d.open = anyClosed; });
  };
  $('#chk-ws').onchange = (e) => { S.ignoreWs = e.target.checked; ls.set('t3.ws', S.ignoreWs ? '1' : '0'); loadDiff(); };
  $('#btn-fork').onclick = forkDialog;
  $('#btn-delegate').onclick = delegateDialog;
  $('#btn-stop').onclick = interrupt;
  $('#btn-jump').onclick = () => scrollBottom(true);
  $('#btn-model').onclick = (e) => openModelPopover(e.currentTarget);
  const permPop = (e) => openPopover(e.currentTarget, composerPerm(), { cls: 'perm-pop', align: 'start' });
  $('#btn-perm').onclick = permPop;
  $('#btn-plan-chip').onclick = permPop;
  $$('#behavior button').forEach((b) => { b.onclick = () => { S.behavior = b.dataset.b; ls.set('t3.behavior', S.behavior); renderComposer(); }; });

  // keep pinned to the bottom when layout changes (approval banner, composer growth, late image loads)
  if (typeof ResizeObserver === 'function') {
    const ro = new ResizeObserver(() => { if (S.stick && S.threadId) scrollBottom(); });
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
  input.addEventListener('input', () => { autosize(); syncSendEnabled(); });
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
  $('#composer').addEventListener('mousedown', (e) => { if (e.target.id === 'composer' || e.target.classList.contains('cbar')) { e.preventDefault(); input.focus(); } });
  $('#btn-attach').onclick = () => $('#file-input').click();
  $('#file-input').onchange = (e) => { addFiles([...e.target.files]); e.target.value = ''; };

  // drag & drop anywhere on the main column
  const main = $('#main'); const hint = $('#drop-hint');
  const hasFiles = (e) => !!e.dataTransfer?.types?.includes('Files');
  let hideHint = null;
  addEventListener('dragover', (e) => {
    if (!hasFiles(e)) return;
    e.preventDefault(); // never let a stray drop navigate away from the app
    const inside = main.contains(e.target) && !$('dialog[open]');
    e.dataTransfer.dropEffect = inside ? 'copy' : 'none';
    hint.hidden = !inside;
    clearTimeout(hideHint); hideHint = setTimeout(() => { hint.hidden = true; }, 160);
  });
  addEventListener('drop', (e) => {
    if (!hasFiles(e)) return;
    e.preventDefault(); hint.hidden = true;
    if (main.contains(e.target) && e.dataTransfer.files.length) addFiles([...e.dataTransfer.files]);
  });

  // code block copy (delegated; works for every rendered Markdown)
  document.addEventListener('click', async (e) => {
    const b = e.target.closest?.('[data-copy]');
    if (!b) return;
    const code = b.closest('.codeblock')?.querySelector('code');
    if (!code) return;
    const text = code.classList.contains('diffc') ? [...code.children].map((s) => s.textContent).join('\n') : code.textContent;
    if (await copyText(text)) {
      const label = b.querySelector('span:last-child');
      label.textContent = 'コピー済み'; setTimeout(() => { label.textContent = 'コピー'; }, 1500);
    }
  });

  // global shortcuts (⌘ on macOS so that Ctrl+K/B/D keep their text-editing meaning there)
  document.addEventListener('keydown', (e) => {
    if (e.isComposing || e.keyCode === 229) return;
    if (e.key === 'Escape') {
      if (popover) { e.preventDefault(); const a = popover.anchor; closePopover(); a.focus(); } else if ($('#app').dataset.drawer === 'open') closeDrawer();
      return;
    }
    const mod = IS_MAC ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey;
    if (!mod || e.altKey || e.shiftKey) return;
    const k = e.key.toLowerCase();
    const modal = $('dialog[open]');
    if (k === 'k') { if (modal && !modal.classList.contains('search')) return; e.preventDefault(); searchDialog(); return; }
    if (!['b', 'd', 'n'].includes(k)) return;
    e.preventDefault();
    if (modal) return;
    if (k === 'b') toggleSidebar();
    else if (k === 'd') setDiffOpen(!S.diffOpen);
    else goHome();
  });
  addEventListener('hashchange', () => { const id = threadFromHash(); if (id && id !== S.threadId) openThread(id, { push: false }); else if (!id && S.threadId) goHome(); });
  matchMedia('(max-width: 860px)').addEventListener('change', () => { closeDrawer(); closePopover(); renderComposer(); });
}

const threadFromHash = () => (/^#\/t\/([0-9a-f-]{36})$/i.exec(location.hash) || [])[1] || null;

// ───────────────────────── boot ─────────────────────────
async function boot() {
  wire();
  S.compose.sel = defaultSelection(S.projectId);
  const hashId = threadFromHash();
  if (hashId) $('#main').dataset.view = 'thread';
  renderHeader(); renderHero(); renderComposer(); renderStatPill();
  setDiffOpen(S.diffOpen);
  connectWs();
  try {
    const [meta, providers] = await Promise.all([
      api('/api/meta').catch(() => null),
      api('/api/providers').catch(() => []),
    ]);
    S.meta = meta; S.providers = Array.isArray(providers) ? providers : [];
    if (S.wsOk && meta) setConn('ok', `接続済み · v${meta.version}`);
    if (meta?.follow_up_behavior && !ls.get('t3.behavior', '')) S.behavior = meta.follow_up_behavior === 'queue' ? 'queue' : 'steer';
    await loadProjects();
    S.compose.sel = defaultSelection(S.projectId);
    renderComposer(); renderStatPill(); loadDbStats();
    await loadThreads();
  } catch (e) {
    setConn('bad', `サーバーに接続できません: ${e.message}`);
    toast(`初期化に失敗: ${e.message}`, 'err', 6000);
  }
  if (hashId) await openThread(hashId, { push: false });
  else if (!isCoarse() && !$('dialog[open]')) ta().focus();
  $('#app').classList.add('ready');
  setInterval(() => { if (!document.hidden) loadThreads().catch(() => {}); }, 15000);
}

boot();
