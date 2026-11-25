// Usage & Limits data helpers for t3code-rs web UI — pure functions and constants.
// Extracted from app.js (Tier 4 Wave A). No app state (S), DOM, fetch, or storage access.
// The only environment read is matchMedia() inside axisMarks(), evaluated at call time
// to pick x-axis label density for the current viewport width.
import { clip } from './util.js';

export const U_PROVIDERS = {
  claude: { label: 'Claude Code', color: 'var(--p-claude)' },
  codex: { label: 'Codex', color: 'var(--text)' }, // high-contrast foreground; follows the surface it sits on
  antigravity: { label: 'Antigravity', color: 'var(--p-antigravity)' },
  opencode: { label: 'OpenCode', color: 'var(--p-opencode)' },
};

export const U_PERIODS = [['1', '過去24時間'], ['7', '7日間'], ['30', '30日間'], ['90', '90日間']];

export const U_METRICS = [['cost', 'コスト'], ['tokens', 'トークン'], ['limits', '上限']];

export const U_KINDS = [
  { key: 'input_tokens', label: '入力', color: 'var(--text)' },
  { key: 'output_tokens', label: '出力', color: 'var(--accent)' },
  { key: 'cache_read_tokens', label: 'キャッシュ読込', color: 'color-mix(in srgb, var(--text) 32%, transparent)' },
  { key: 'cache_write_tokens', label: 'キャッシュ書込', color: 'color-mix(in srgb, var(--text) 58%, transparent)' },
];

export const U_SUMS = ['cost_usd', 'total_tokens', ...U_KINDS.map((k) => k.key), 'sessions', 'requests'];

export const uNum = (v) => { const n = Number(v); return Number.isFinite(n) && n > 0 ? n : 0; };

export function uId(p) {
  const s = String(p || 'unknown').toLowerCase();
  return s === 'open-code' ? 'opencode' : s === 'claude-code' || s === 'claudeagent' ? 'claude' : s;
}

export function uStats(x) {
  const o = Object.fromEntries(U_SUMS.map((k) => [k, uNum(x?.[k])]));
  o.reasoning_tokens = uNum(x?.reasoning_tokens);
  if (!o.total_tokens) o.total_tokens = U_KINDS.reduce((a, k) => a + o[k.key], 0);
  return o;
}

export const uSum = (rows) => Object.fromEntries(U_SUMS.map((k) => [k, rows.reduce((a, r) => a + r[k], 0)]));

export function uProvider(id, serverLabel, serverColor) {
  const known = Object.hasOwn(U_PROVIDERS, id) ? U_PROVIDERS[id] : null;
  // colours end up in style properties, so only known values or plain hex are accepted
  return known ? { id, ...known } : { id, label: String(serverLabel || id), color: /^#[0-9a-f]{3,8}$/i.test(serverColor || '') ? serverColor : '#8b8b8b' };
}

/** Makes the /api/usage payload safe to render: arrays always exist, numbers are finite, provider ids are canonical. */
export function normalizeUsage(raw) {
  if (!raw || typeof raw !== 'object' || !(raw.totals || raw.providers || raw.buckets)) throw new Error('応答の形式が不正です');
  const rows = (a) => (Array.isArray(a) ? a.filter((x) => x && typeof x === 'object') : []);
  const shares = (x) => ({ cost_share: uNum(x.cost_share), token_share: uNum(x.token_share) });
  const providers = rows(raw.providers).map((p) => ({ ...uStats(p), ...shares(p), ...uProvider(uId(p.provider), p.label, p.color) }));
  const models = rows(raw.models).map((m) => ({ ...uStats(m), ...shares(m), ...uProvider(uId(m.provider)), model: String(m.model || '(不明なモデル)') }))
    .filter((m) => !(m.model === '<synthetic>' && !m.total_tokens)); // placeholder rows of locally generated replies
  const buckets = rows(raw.buckets).map((b) => {
    const by = new Map();
    for (const [k, v] of Object.entries(b.by_provider || {})) {
      const id = uId(k); const prev = by.get(id) || { cost_usd: 0, total_tokens: 0 };
      by.set(id, { cost_usd: prev.cost_usd + uNum(v?.cost_usd), total_tokens: prev.total_tokens + uNum(v?.total_tokens) });
    }
    return { key: String(b.key ?? ''), label: String(b.label ?? b.key ?? ''), by };
  }).sort((a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0));
  // a provider that only shows up in buckets still needs a legend row and a colour
  for (const b of buckets) {
    for (const id of b.by.keys()) {
      if (providers.some((p) => p.id === id)) continue;
      const sum = (k) => buckets.reduce((a, x) => a + (x.by.get(id)?.[k] || 0), 0);
      providers.push({ ...uStats({ cost_usd: sum('cost_usd'), total_tokens: sum('total_tokens') }), cost_share: 0, token_share: 0, ...uProvider(id) });
    }
  }
  const w = raw.window || {};
  const totals = raw.totals ? uStats(raw.totals) : { ...uSum(providers), reasoning_tokens: 0 };
  return {
    window: { since: String(w.since || ''), until: String(w.until || ''), hourly: w.resolution ? w.resolution === 'hour' : Number(w.days) === 1 },
    totals, providers, models, buckets, scannedAt: Date.parse(raw.scanned_at) || 0,
  };
}

export const LIMIT_LOW = 15; // remaining %: amber in the Limits view

export const LIMIT_CRITICAL = 10; // remaining %: warned above the composer

export const uText = (v, max) => (typeof v === 'string' ? clip(v.trim(), max) : '');

export const uTime = (v) => (typeof v === 'string' && Date.parse(v)) || 0;

export const uPercent = (v) => {
  const n = typeof v === 'number' || (typeof v === 'string' && v.trim()) ? Number(v) : NaN;
  return Number.isFinite(n) ? Math.min(100, Math.max(0, n)) : null;
};

export function uWindow(w) {
  let used = uPercent(w.used_percent);
  let remaining = uPercent(w.remaining_percent);
  if (used == null && remaining == null) return null;
  remaining ??= 100 - used;
  used ??= 100 - remaining;
  const kind = uText(w.kind, 24);
  const mins = Number(w.window_duration_mins);
  return {
    id: uText(w.id, 60), kind, used, remaining,
    label: uText(w.label, 60) || (kind === 'session' ? 'セッション' : kind === 'weekly' ? '週間' : '上限'),
    durationMins: Number.isFinite(mins) && mins > 0 ? mins : 0,
    resetsAt: uTime(w.resets_at),
  };
}

/** Banked reset credits; null unless at least one can be spent. */
export function uCredits(c) {
  const count = Math.floor(uNum(c?.available_count));
  return count > 0 ? { count, nextExpiresAt: uTime(c.next_expires_at) } : null;
}

/** Makes the /api/usage/limits payload safe to render. */
export function normalizeLimits(raw) {
  if (!raw || typeof raw !== 'object' || !Array.isArray(raw.providers)) throw new Error('応答の形式が不正です');
  const objects = (a) => (Array.isArray(a) ? a.filter((x) => x && typeof x === 'object') : []);
  const providers = objects(raw.providers).map((p) => ({
    ...uProvider(uId(p.provider), uText(p.label, 40)),
    rawProvider: uText(p.provider, 40), credits: uCredits(p.reset_credits),
    plan: uText(p.plan, 60), status: uText(p.status, 24), source: uText(p.source, 40), message: uText(p.message, 400),
    windows: objects(p.windows).map(uWindow).filter(Boolean),
  }));
  return { providers, checkedAt: uTime(raw.checked_at) };
}

/** A window whose reset time has passed no longer limits anything, whatever its last reading said. */
export const limitIsReset = (w, now) => w.resetsAt > 0 && w.resetsAt <= now;

export const limitIsOut = (w, now) => w.remaining <= 0 && !limitIsReset(w, now);

export const fmtLeft = (v) => (v > 0 && v < 1 ? '<1%' : `${Math.round(v)}%`);

/** "6日2時間" / "2時間2分" / "45分"; `gap` spaces out the form without days ("1 時間 54 分"). */
export function fmtDurationJa(ms, gap = '') {
  const mins = Math.floor(ms / 60000);
  if (mins < 1) return '1分未満';
  const d = Math.floor(mins / 1440); const hours = Math.floor((mins % 1440) / 60); const m = mins % 60;
  if (d) return `${d}日${hours ? `${hours}時間` : ''}`;
  return [...(hours ? [hours, '時間'] : []), ...(m ? [m, '分'] : [])].join(gap);
}

export function fmtResetIn(resetsAt, now) {
  if (!resetsAt) return '';
  const ms = resetsAt - now;
  if (ms <= 0) return 'リセット済';
  return ms < 60000 ? 'まもなくリセット' : `あと ${fmtDurationJa(ms)}`;
}

/** "今日 17:04" / "明日 23:22" / "12/01 17:28" on the viewer's calendar. */
export function fmtResetAt(ts, now) {
  const t = new Date(ts);
  const day = (d) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  const ahead = Math.round((day(t) - day(new Date(now))) / 86400000);
  const date = ahead === 0 ? '今日' : ahead === 1 ? '明日' : `${String(t.getMonth() + 1).padStart(2, '0')}/${String(t.getDate()).padStart(2, '0')}`;
  return `${date} ${hhmm(t)}`;
}

export const LIMIT_SOURCES = {
  'claude-cli': 'CLI', 'codex-app-server': 'CLI', 'codex-session-log': 'ローカルログ', 'antigravity-api': 'Antigravity API', runtime: '実行時検知',
};

export const limitSource = (s) => (Object.hasOwn(LIMIT_SOURCES, s) ? LIMIT_SOURCES[s] : '');

/** What one window says right now. Pace and refill only exist while its reset is still ahead. */
export function limitState(w, now) {
  const expired = limitIsReset(w, now);
  const upcoming = w.resetsAt > now;
  const length = w.durationMins * 60000;
  const elapsed = upcoming && length ? Math.min(1, Math.max(0, (length - (w.resetsAt - now)) / length)) : null;
  let pace = null;
  if (elapsed != null) {
    const ahead = w.used - elapsed * 100;
    pace = ahead > 5 ? { cls: 'ahead', icon: 'trendUp', title: 'ペース超過: 時間経過より速く消費中' }
      : ahead < -5 ? { cls: 'under', icon: 'trendDown', title: '余裕あり: 時間経過より緩やかに消費中' }
        : { cls: 'even', icon: 'gauge', title: '適正ペース' };
  }
  return {
    expired, upcoming, pace,
    timeLeft: elapsed == null ? null : Math.round((1 - elapsed) * 100),
    refill: upcoming && w.remaining < 100 ? 100 - w.remaining : 0,
    level: expired ? '' : w.remaining <= 0 ? ' out' : w.remaining <= LIMIT_LOW ? ' low' : '',
  };
}

export const WEEKDAYS = '日月火水木金土';

export const hhmm = (t) => `${String(t.getHours()).padStart(2, '0')}:${String(t.getMinutes()).padStart(2, '0')}`;

export const hourLabel = (t) => `${String(t.getHours()).padStart(2, '0')}:00`;

/** Bucket keys are "YYYY-MM-DD" or an hourly timestamp; zoned timestamps are shown in local time. */
export function bucketDate(key) {
  const m = /^(\d{4})-(\d\d)-(\d\d)(?:[T ](\d\d)(?::(\d\d))?)?/.exec(key);
  if (!m) return null;
  const zoned = /[T ]\d\d.*(Z|[+-]\d\d:?\d\d)$/.test(key);
  const t = zoned ? new Date(key) : new Date(+m[1], +m[2] - 1, +m[3], +(m[4] || 0), +(m[5] || 0));
  return Number.isNaN(t.getTime()) ? null : { t, timed: m[4] != null };
}

// labels are built from the key so that hours follow the viewer's clock; the server label is the fallback
export function bucketTick(b, hourly) {
  const p = bucketDate(b.key);
  if (!p || (hourly && !p.timed)) return b.label;
  return hourly ? hourLabel(p.t) : `${p.t.getMonth() + 1}/${p.t.getDate()}`;
}

export function bucketTitle(b, hourly, short = false) {
  const p = bucketDate(b.key);
  if (!p) return b.label;
  const { t } = p;
  const day = short ? `${t.getMonth() + 1}/${t.getDate()}` : `${t.getMonth() + 1}月${t.getDate()}日`;
  return hourly ? `${day} ${p.timed ? hourLabel(t) : b.label}` : `${day} (${WEEKDAYS[t.getDay()]})`;
}

export function rangeText(w) {
  const f = (s) => {
    if (!w.hourly) return s.replace(/^(\d{4}-\d\d-\d\d)T.*$/, '$1'); // daily windows are whole days
    const p = bucketDate(s);
    return p?.timed ? `${p.t.getMonth() + 1}/${p.t.getDate()} ${hhmm(p.t)}` : s;
  };
  return w.since || w.until ? `${f(w.since)} – ${f(w.until)}` : '';
}

/** 1 / 2 / 5 steps, about four intervals, up to a round number at or above the peak. */
export function niceScale(peak) {
  if (!(peak > 0)) return { max: 1, step: 0.25, ticks: [0, 0.25, 0.5, 0.75, 1], labeled: false };
  const raw = peak / 4;
  const mag = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 5, 10].find((x) => x * mag >= raw * (1 - 1e-9)) * mag;
  const count = Math.max(1, Math.ceil(peak / step - 1e-9));
  return { max: step * count, step, ticks: Array.from({ length: count + 1 }, (_, i) => step * i), labeled: true };
}

/** Cubic Bézier path with monotone (Fritsch–Carlson) tangents: smooth, and never overshoots below zero. */
export function monotonePath(pts) {
  const n = pts.length;
  const f = (v) => Math.round(v * 100) / 100;
  if (n < 2) return n ? `M${f(pts[0][0])} ${f(pts[0][1])}` : '';
  const m = pts.slice(1).map((p, i) => (p[1] - pts[i][1]) / (p[0] - pts[i][0]));
  const t = pts.map((_, i) => (i === 0 ? m[0] : i === n - 1 ? m[n - 2] : m[i - 1] * m[i] <= 0 ? 0 : (m[i - 1] + m[i]) / 2));
  m.forEach((s, i) => {
    if (s === 0) { t[i] = 0; t[i + 1] = 0; return; }
    const a = t[i] / s; const b = t[i + 1] / s; const q = a * a + b * b;
    if (q > 9) { const k = 3 / Math.sqrt(q); t[i] = k * a * s; t[i + 1] = k * b * s; }
  });
  let d = `M${f(pts[0][0])} ${f(pts[0][1])}`;
  for (let i = 0; i < n - 1; i++) {
    const [x0, y0] = pts[i]; const [x1, y1] = pts[i + 1]; const dx = (x1 - x0) / 3;
    d += `C${f(x0 + dx)} ${f(y0 + t[i] * dx)} ${f(x1 - dx)} ${f(y1 - t[i + 1] * dx)} ${f(x1)} ${f(y1)}`;
  }
  return d;
}

/** Indices that get an x-axis label: whole hours for the 24h view, otherwise evenly spaced days ending today. */
export function axisMarks(columns, hourly) {
  const n = columns.length;
  const narrow = matchMedia('(max-width: 640px)').matches;
  const all = columns.map((_, i) => i);
  if (hourly) {
    const every = narrow ? 6 : 4;
    const marks = all.filter((i) => bucketDate(columns[i].bucket.key)?.t.getHours() % every === 0);
    if (marks.length) return marks;
  }
  const every = Math.max(1, Math.ceil(n / (narrow ? 4 : 7)));
  return all.filter((i) => (n - 1 - i) % every === 0);
}
