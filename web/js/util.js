// t3code-rs shared utilities, icons, formatters, and domain helpers (zero dependencies).

export const $ = (s, r = document) => r.querySelector(s);
export const $$ = (s, r = document) => [...r.querySelectorAll(s)];
const ESC = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' };
export const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ESC[c]);

export function h(tag, attrs, ...kids) {
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

export const ls = {
  get(k, d) { try { const v = localStorage.getItem(k); return v == null ? d : v; } catch { return d; } },
  set(k, v) { try { localStorage.setItem(k, v); } catch { /* storage may be unavailable */ } },
  remove(k) { try { localStorage.removeItem(k); } catch { /* storage may be unavailable */ } },
  json(k, d) { try { return JSON.parse(this.get(k, '')) ?? d; } catch { return d; } },
};

export const ICONS = {
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
  swarm: '<circle cx="12" cy="5.500" r="2.500"/><circle cx="5.500" cy="17.500" r="2.500"/><circle cx="18.500" cy="17.500" r="2.500"/><path d="M10.800 7.700l-4.100 7.600M13.200 7.700l4.100 7.600M8 17.500h8"/>',
  open: '<path d="M14 4h6v6M20 4l-9 9M18 14v4a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4"/>',
  ticket: '<path d="M3 9a3 3 0 0 1 0 6v3a1 1 0 0 0 1 1h16a1 1 0 0 0 1-1v-3a3 3 0 0 1 0-6V6a1 1 0 0 0-1-1H4a1 1 0 0 0-1 1zM14 5v2.500M14 10.750v2.500M14 16.500V19"/>',
  trendUp: '<path d="M16 7h6v6M22 7l-8.500 8.500-5-5L2 17"/>',
  trendDown: '<path d="M16 17h6v-6M22 17l-8.500-8.500-5 5L2 7"/>',
  gauge: '<path d="M12 14l4-4M3.340 19a10 10 0 1 1 17.320 0"/>',
};

export const svg = (n) => `<svg class="ic" viewBox="0 0 24 24" aria-hidden="true">${ICONS[n] || ''}</svg>`;
export const svgNode = (tag, attrs) => {
  const el = document.createElementNS('http://www.w3.org/2000/svg', tag);
  for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, v);
  return el;
};
export const iconEl = (n, cls = '') => { const s = h('span', { class: `iw ${cls}` }); s.innerHTML = svg(n); return s; };
export const hydrateIcons = (root = document) => $$('[data-icon]', root).forEach((el) => { if (!el.querySelector('svg')) el.insertAdjacentHTML('afterbegin', svg(el.dataset.icon)); });
export const ibtn = (ic, title, onclick, cls = '') => {
  const b = h('button', { class: `ibtn ${cls}`, type: 'button', title, 'aria-label': title, onclick });
  b.innerHTML = svg(ic);
  return b;
};

export const IS_MAC = /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
export const kbd = (k) => (IS_MAC ? `⌘${k}` : `Ctrl+${k}`);

export function fmtAgo(iso) {
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
export function fmtDur(ms) {
  if (ms == null || !isFinite(ms)) return '–';
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(1)}秒`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}分${Math.floor(s % 60)}秒`;
  return `${Math.floor(m / 60)}時間${m % 60}分`;
}
export function fmtBytes(n) {
  n = Number(n) || 0;
  if (n < 1024) return `${n} B`;
  if (n < 1024 ** 2) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(2)} MB`;
  return `${(n / 1024 ** 3).toFixed(2)} GB`;
}
export const fmtNum = (n) => Number(n || 0).toLocaleString('ja-JP');
export function fmtTok(n) {
  n = Number(n) || 0;
  if (n >= 1e6) return `${(n / 1e6).toFixed(2)}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)}k`;
  return String(n);
}
const usdFmt = new Intl.NumberFormat('en-US', { style: 'currency', currency: 'USD', minimumFractionDigits: 2, maximumFractionDigits: 2 });
export const fmtUsd = (n) => (n > 0 && n < 0.005 ? '<$0.01' : usdFmt.format(n));
export function fmtCompact(n) {
  if (Math.round(n) < 1000) return String(Math.round(n));
  const units = ['K', 'M', 'B', 'T'];
  let v = n / 1000; let i = 0;
  while (v >= 999.5 && i < units.length - 1) { v /= 1000; i += 1; }
  return `${Number(v.toFixed(v >= 100 ? 0 : v >= 10 ? 1 : 2))}${units[i]}`;
}
export function fmtShare(x) { const p = x * 100; return p > 0 && p < 0.1 ? '<0.1%' : `${p.toFixed(1)}%`; }
export function fmtPctCompact(pct) {
  if (!(pct > 0)) return '0%';
  if (pct < 1) return '<1%';
  return `${Math.round(pct)}%`;
}

export const debounce = (fn, ms) => { let t; return (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); }; };
export const rafThrottle = (fn) => { let q = false; return () => { if (q) return; q = true; requestAnimationFrame(() => { q = false; fn(); }); }; };
export const clip = (s, n) => { s = String(s ?? ''); return s.length > n ? s.slice(0, n - 1) + '…' : s; };
export const oneLine = (s, n = 200) => clip(String(s ?? '').replace(/\s+/g, ' ').trim(), n);
export const pretty = (v) => { try { return typeof v === 'string' ? v : JSON.stringify(v, null, 2); } catch { return String(v); } };
export const isCoarse = () => matchMedia('(pointer: coarse)').matches;
export const isNarrow = () => matchMedia('(max-width: 860px)').matches;
export const hashNum = (s) => { let n = 0; for (const c of String(s)) n = (n * 31 + c.codePointAt(0)) >>> 0; return n; };
export const compactPath = (p) => { const parts = String(p).split('/').filter(Boolean); return parts.length > 2 ? `…/${parts.slice(-2).join('/')}` : String(p); };
export function kindLetter(k) {
  k = String(k || 'M').toLowerCase();
  if (/^(a|add|new|creat|untracked|\?)/.test(k)) return 'A';
  if (/^(d|del|remov)/.test(k)) return 'D';
  if (/^(r|ren|mov)/.test(k)) return 'R';
  return 'M';
}

export function cleanTitle(src) {
  const raw = String(src ?? '');
  let s = raw.replace(/<(command-name|command-message|command-args|system-reminder|local-command-caveat)\b[^<>]*>[\s\S]*?(<\/\1>|$)/gi, ' ');
  if (/<user_action\b/i.test(s)) s = s.replace(/<\/?[a-z][\w-]*[^<>]*(>|$)/gi, ' ');
  s = s.replace(/<\/?[a-z]+(?:[-_][a-z]+)+(?:\s[^<>]*)?(>|$)/gi, ' ')
    .replace(/\s+/g, ' ').trim()
    .replace(/^(?:#{1,6}(?:\s+|$)|>\s*)+/, '').trim();
  if (s) return s;
  const cmd = /<command-name>\s*([^<]*)/i.exec(raw)?.[1].trim();
  return cmd || (/<user_action\b/i.test(raw) ? 'ユーザー操作' : '(無題)');
}

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
export function sha1(bytes) {
  const len = ((bytes.length + 8 >> 6) + 1) * 64;
  const buf = new Uint8Array(len); buf.set(bytes); buf[bytes.length] = 0x80;
  const view = new DataView(buf.buffer);
  view.setUint32(len - 4, (bytes.length * 8) >>> 0);
  const H = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0];
  const w = new Uint32Array(80);
  const rol = (x, n) => (x << n) | (x >>> (32 - n));
  for (let off = 0; off < len; off += 64) {
    for (let i = 0; i < 80; i++) w[i] = i < 16 ? view.getUint32(off + i * 4) : rol(w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16], 1);
    let [a, b, c, d, e] = H;
    for (let i = 0; i < 80; i++) {
      const [fn, k] = i < 20 ? [(b & c) | (~b & d), 0x5a827999] : i < 40 ? [b ^ c ^ d, 0x6ed9eba1] : i < 60 ? [(b & c) | (b & d) | (c & d), 0x8f1bbcdc] : [b ^ c ^ d, 0xca62c1d6];
      const t = (rol(a, 5) + fn + e + k + w[i]) | 0;
      e = d; d = c; c = rol(b, 30); b = a; a = t;
    }
    [a, b, c, d, e].forEach((v, i) => { H[i] = (H[i] + v) | 0; });
  }
  return H.flatMap((v) => [v >>> 24, (v >>> 16) & 255, (v >>> 8) & 255, v & 255]);
}
export function resolveThreadId(raw) {
  const s = String(raw || '');
  if (UUID_RE.test(s)) return s.toLowerCase();
  if (!s.startsWith('thread:')) return '';
  const ns = [0x6b, 0xa7, 0xb8, 0x11, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8];
  const b = sha1(new Uint8Array([...ns, ...new TextEncoder().encode(s)])).slice(0, 16);
  b[6] = (b[6] & 0x0f) | 0x50; b[8] = (b[8] & 0x3f) | 0x80;
  const hex = b.map((x) => x.toString(16).padStart(2, '0')).join('');
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

export function pstr(p, ...keys) {
  if (p == null) return '';
  if (typeof p === 'string') return p;
  for (const k of keys) { const v = p[k]; if (typeof v === 'string' && v) return v; }
  return '';
}
export function textOf(v) {
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

export function toast(msg, kind = '', ms = 3800) {
  const box = $('#toasts');
  const el = h('div', { class: `toast ${kind}`, text: msg });
  box.append(el);
  try { box.hidePopover(); box.showPopover(); } catch { /* popover unsupported */ }
  setTimeout(() => { el.remove(); if (!box.childElementCount) { try { box.hidePopover(); } catch { /* noop */ } } }, ms);
}

export async function copyText(text, done = 'コピーしました') {
  try { await navigator.clipboard.writeText(text); toast(done, 'ok', 1500); return true; } catch { /* fall through */ }
  const x = h('textarea', { value: text, style: 'position:fixed;opacity:0' });
  (document.querySelector('dialog[open]') || document.body).append(x); x.select();
  let ok = false;
  try { ok = document.execCommand('copy'); } catch { /* noop */ }
  x.remove();
  toast(ok ? done : 'コピーできません', ok ? 'ok' : 'err', 1500);
  return ok;
}

export const isDesktop = () => window.__T3_DESKTOP__ === true || document.documentElement.classList.contains('t3-desktop');
export const ipc = (msg) => window.ipc?.postMessage(msg);
export function wireDesktop() {
  if (window.__T3_DESKTOP__ === true) document.documentElement.classList.add('t3-desktop');
  const INTERACTIVE = 'button, input, textarea, select, a, label, summary, [role="button"], [contenteditable="true"], .no-drag';
  const onDragRegion = (e) => isDesktop() && e.button === 0 && e.target.closest?.('.drag-region') && !e.target.closest(INTERACTIVE);
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

export const PROVIDER_LABEL = { claude: 'Claude', codex: 'Codex', antigravity: 'Antigravity', 'open-code': 'OpenCode', opencode: 'OpenCode', mock: 'Mock' };
export const PROVIDER_FALLBACK = ['claude', 'codex', 'antigravity', 'open-code'];
export const provLabel = (p) => PROVIDER_LABEL[p] || p || '?';
export const provClass = (p) => `p-${p === 'opencode' ? 'open-code' : p || 'mock'}`;
export const provDot = (p, extra = '') => h('span', { class: `pdot ${provClass(p)} ${extra}`, title: provLabel(p) });
export const TIERS = {
  0: { label: 'Tier 0 無料/ローカル' },
  1: { label: 'Tier 1 高速' },
  2: { label: 'Tier 2 監査' },
  3: { label: 'Tier 3 設計' },
};
export const EFFORT_FALLBACK = ['low', 'medium', 'high', 'xhigh'];
export const STATUS_LABEL = {
  idle: '待機', running: '実行中', 'waiting-for-children': '子の完了待ち', 'rate-limited': 'レート制限', completed: '完了', failed: '失敗', interrupted: '中断',
  queued: '待ち行列', cancelled: '取消',
};
export const REL_LABEL = { delegate: '委譲', delegated: '委譲', fork: '分岐', forked: '分岐', handoff: '引継', child: '子' };
export const MODES = ['approval-required', 'auto-accept-edits', 'full-access'];
export const MODE_LABEL = { 'approval-required': '承認制', 'auto-accept-edits': '編集自動', 'full-access': 'フルアクセス' };
export const MODE_HINT = {
  'approval-required': '変更やコマンド実行の前に承認を求めます',
  'auto-accept-edits': 'ファイル編集は自動承認し、それ以外は承認を求めます',
  'full-access': '承認なしですべて実行します。信頼できる作業でのみ使ってください',
};
export const BUSY = new Set(['running', 'waiting-for-children', 'rate-limited']);
export const TERMINAL_RUN = new Set(['completed', 'failed', 'cancelled']);
export const isBusy = (st) => BUSY.has(st);

export function authLabel(m) {
  m = String(m || '').toLowerCase();
  if (m.includes('cloudflare')) return 'Cloudflare Access';
  if (m.includes('tailscale')) return 'Tailscale';
  if (m.includes('bearer')) return 'Bearer トークン';
  if (m.includes('local')) return 'LocalOnly（ループバック限定）';
  return m || '不明';
}

export const THEMES = [
  ['system', 'システムに合わせる'],
  ['dark', 'T3 Code (Dark)'],
  ['light', 'T3 Code (Light)'],
  ['ember', 'Ember'],
  ['ocean', 'Ocean'],
  ['grove', 'Grove'],
  ['iris', 'Iris'],
];
