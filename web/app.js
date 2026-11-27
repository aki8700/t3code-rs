// t3code-rs web UI — vanilla ES modules (no build step, zero external requests).
import {
  $, $$, esc, h, ls, svg, svgNode, iconEl, hydrateIcons, ibtn,
  IS_MAC, kbd, fmtAgo, fmtDur, fmtBytes, fmtNum, fmtTok, fmtCompact, fmtShare, fmtUsd, fmtPctCompact,
  debounce, rafThrottle, clip, oneLine, pretty, isCoarse, isNarrow, hashNum, compactPath, kindLetter,
  cleanTitle, resolveThreadId, pstr, textOf, toast, copyText, isDesktop, ipc, wireDesktop,
  PROVIDER_FALLBACK, provLabel, provClass, provDot, TIERS, EFFORT_FALLBACK, STATUS_LABEL, REL_LABEL,
  MODES, MODE_LABEL, MODE_HINT, TERMINAL_RUN, isBusy, authLabel, THEMES,
} from './js/util.js';
import { md, mdEl } from './js/markdown.js';
import {
  U_PERIODS, U_METRICS, U_KINDS, uId, uSum, normalizeUsage, uText, normalizeLimits,
  LIMIT_CRITICAL, limitIsReset, limitIsOut, fmtLeft, fmtDurationJa, fmtResetIn, fmtResetAt,
  limitSource, limitState, bucketTick, bucketTitle, rangeText, niceScale, monotonePath, axisMarks,
} from './js/usage-data.js';
import {
  fileEntries, changedFiles, patchLines, latestTurnChangedPaths, filterDiffFilesByPaths,
} from './js/patch.js';
import { wireRailResizer } from './js/rail.js';
import {
  CTX_SOURCE_LABEL, CTX_KIND_META, fallbackContextWindow, contextRingSvg, setContextRing, ctxSection,
} from './js/context-ui.js';
import {
  allCommands, GROUP_LABEL, parseShellInput, parseSlashInput, findCommand,
  getPaletteSuggestions, loadShellHistory, pushShellHistory,
} from './js/commands.js';

// ───────────────────────── state ─────────────────────────
const S = {
  meta: null,
  pauseAll: false,
  providers: [],
  projects: [],
  projectId: ls.get('t3.project', ''),
  threads: [],
  showArchived: ls.get('t3.archived', '0') === '1',
  threadId: null,
  detail: null,           // { thread, runs, items, children, handoffs, attachments }
  live: new Map(),        // threadId -> { runId, assistant, reasoning }
  pending: new Map(),     // threadId -> PendingRequest[]
  showKids: ls.get('t3.kids', '0') === '1', // unfold delegated / forked children by default
  kidsOpen: new Map(),    // thread id -> explicit fold state chosen by the user
  collapsedProjects: new Set(ls.json('t3.pcollapsed', [])),
  expandedProjects: new Set(), // projects showing all threads
  showEmpty: ls.get('t3.pempty', '0') === '1', // unfold the "no threads" project group
  expandedMsgs: new Set(), // long user messages the reader unfolded
  loadingOlder: null, // thread id whose older page is being fetched
  behavior: ls.get('t3.behavior', '') === 'queue' ? 'queue' : 'steer',
  atts: [],               // composer attachments
  drafts: new Map(),      // threadId | 'new' -> { text, atts }
  // composer selection. isolation applies only when a new thread is created.
  compose: { sel: null, mode: 'approval-required', plan: false, isolation: 'shared', baseRef: '' },
  heroSel: ls.json('t3.sel', null),
  optimistic: [],
  stick: true,
  rail: (() => {
    const saved = ls.get('t3.rail', null);
    if (saved === 'none' || saved === '') return null;
    if (['context', 'diff', 'swarm'].includes(saved)) return saved;
    return innerWidth > 1180 ? 'context' : null;
  })(), // which panel owns the right rail
  swarms: new Map(),      // threadId -> SwarmBoardSnapshot
  liveUsage: new Map(),   // threadId -> { runId, context_tokens, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens }
  diffStat: null,         // { threadId, add, del, files }
  diffData: null,         // { threadId, data }
  diffError: null,        // { threadId, message }
  parentDiff: null,       // { parentId, childId, branch, data } while showing a parent↔child diff
  diffScope: ls.get('t3.diffScope', 'all') === 'latest' ? 'latest' : 'all',
  ignoreWs: ls.get('t3.ws', '1') !== '0',
  dbStats: null,
  usage: { days: 7, metric: 'cost', tab: 'model', hidden: new Set(), cache: new Map(), errors: new Map(), pending: new Map(), seq: new Map(), refreshing: new Set(), onchange: null },
  limits: { data: null, error: '', pending: null, refreshing: false, redeeming: false, seq: 0, fetchedAt: 0, onchange: null },
  subscribed: new Set(),
  ws: null, wsOk: false,
  token: ls.get('t3.token', ''),
  theme: ls.get('t3.theme', 'system'),
  shellHistory: loadShellHistory(ls),
  workspaceMeta: new Map(), // projectId -> { commands, files } (GET workspace-meta, cached)
};

const threadById = (id) => S.threads.find((t) => t.id === id) || (S.detail?.thread?.id === id ? S.detail.thread : null)
  || S.detail?.children?.find((t) => t.id === id) || null;
const curThread = () => S.detail?.thread || null;
/** 'rate-limited' はサイドバー・タイムライン・モニターすべてで「制限中」と表示する。 */
const statusLabel = (st) => (st === 'rate-limited' ? '制限中' : (STATUS_LABEL[st] || st));
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

const blobCache = new Map(); // attachment id -> Promise<object URL>, oldest first
const BLOB_CACHE_MAX = 120;
function releaseBlob(id) {
  const p = blobCache.get(id);
  if (!p) return;
  blobCache.delete(id);
  p.then((u) => URL.revokeObjectURL(u)).catch(() => {});
}
const clearBlobCache = () => [...blobCache.keys()].forEach(releaseBlob);
function attachmentBlobUrl(id) {
  if (blobCache.has(id)) return blobCache.get(id);
  const headers = S.token ? { Authorization: `Bearer ${S.token}` } : {};
  const p = fetch(`/api/attachments/${encodeURIComponent(id)}`, { headers, credentials: 'same-origin', referrerPolicy: 'no-referrer' })
    .then((r) => { if (!r.ok) throw new Error(r.statusText); return r.blob(); })
    .then((b) => URL.createObjectURL(b));
  blobCache.set(id, p);
  while (blobCache.size > BLOB_CACHE_MAX) releaseBlob(blobCache.keys().next().value);
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
function openModal(title, build, { cls = '', onclose, head } = {}) {
  closePopover();
  const dlg = h('dialog', { class: `modal ${cls}`, 'aria-label': title });
  const close = () => { try { dlg.close(); } catch { dlg.remove(); } };
  const body = h('div', { class: 'modal-body' });
  dlg.append(head ? head(close) : h('div', { class: 'modal-head' }, h('h2', { text: title }), ibtn('close', '閉じる', close)), body);
  dlg.addEventListener('close', () => { if (popover && dlg.contains(popover.el)) closePopover(); dlg.remove(); onclose?.(); });
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
/**
 * Opens a floating panel above the anchor (`below`: true = under it, 'auto' = whichever side has room; bottom sheet on phones).
 * Toggles when the same anchor is clicked again.
 */
function openPopover(anchor, content, { cls = '', align = 'end', below = false } = {}) {
  const again = popover?.anchor === anchor;
  closePopover();
  if (again) return null;
  const el = h('div', { class: `popover ${cls}`, role: 'dialog' }, content);
  // a modal <dialog> sits in the top layer: anything mounted outside it is hidden behind it and inert
  (anchor.closest('dialog') || document.body).append(el);
  const place = () => {
    if (matchMedia('(max-width: 640px)').matches) { el.classList.add('sheet'); el.style.cssText = ''; return; }
    el.classList.remove('sheet');
    el.style.cssText = '';
    const r = anchor.getBoundingClientRect();
    const w = el.offsetWidth;
    const left = align === 'end' ? r.right - w : align === 'center' ? r.left + (r.width - w) / 2 : r.left;
    el.style.left = `${Math.max(8, Math.min(left, innerWidth - w - 8))}px`;
    const under = below === 'auto' ? el.offsetHeight <= innerHeight - r.bottom - 16 || innerHeight - r.bottom >= r.top : below;
    if (under) {
      el.style.top = `${r.bottom + 6}px`;
      el.style.maxHeight = `${Math.max(160, Math.min(460, innerHeight - r.bottom - 16))}px`;
    } else {
      el.style.bottom = `${Math.max(8, innerHeight - r.top + 8)}px`;
      el.style.maxHeight = `${Math.max(220, r.top - 20)}px`;
    }
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

// ───────────────────────── theme ─────────────────────────
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

/**
 * 3-stage permission switch + Plan toggle. onMode / onPlan may resolve to false to reject the change.
 * confirmFull asks inline before switching to full access (used on existing threads).
 */
function permPanel({ mode, plan, maxMode = 'full-access', planForced = false, confirmFull = false, note, onMode, onPlan }) {
  const max = MODES.indexOf(maxMode);
  let current = mode;
  const hint = h('p', { class: `perm-hint${mode === 'full-access' ? ' danger' : ''}`, text: MODE_HINT[mode] });
  const confirmBox = h('div', { class: 'perm-confirm', role: 'alert', hidden: true });
  const show = (v) => {
    $$('button', seg).forEach((b) => { const on = b.dataset.v === v; b.classList.toggle('on', on); b.setAttribute('aria-checked', String(on)); });
    hint.textContent = MODE_HINT[v]; hint.classList.toggle('danger', v === 'full-access');
  };
  const apply = async (v) => {
    const prev = current; current = v; show(v);
    if (await onMode?.(v) === false) { current = prev; show(prev); }
  };
  const seg = segmented(MODES.map((m) => [m, MODE_LABEL[m], MODE_HINT[m]]), mode, (v) => {
    confirmBox.hidden = true;
    if (confirmFull && v === 'full-access' && current !== v) {
      show(current);
      confirmBox.replaceChildren(h('span', { text: 'このスレッドをフルアクセスにしますか？ 承認なしですべて実行されます。' }),
        h('div', { class: 'actions' },
          h('button', { type: 'button', class: 'btn xs', text: 'キャンセル', onclick: () => { confirmBox.hidden = true; } }),
          h('button', { type: 'button', class: 'btn xs warn', text: 'フルアクセスにする', onclick: () => { confirmBox.hidden = true; apply('full-access'); } })));
      confirmBox.hidden = false;
      popover?.place();
      return;
    }
    apply(v);
  }, { label: '実行権限', cls: 'perm' });
  $$('button', seg).forEach((b) => { b.disabled = MODES.indexOf(b.dataset.v) > max; });
  const chk = h('input', { type: 'checkbox', checked: plan || planForced, disabled: planForced, onchange: async (e) => { const box = e.target; if (await onPlan?.(box.checked) === false) box.checked = !box.checked; } });
  return h('div', { class: 'perm-panel' }, h('span', { class: 'mp-lab', text: '権限' }), seg, hint, confirmBox,
    h('label', { class: 'switch plan-switch' }, chk, h('i'), h('span', {}, h('b', { text: 'Plan モード' }), h('small', { text: planForced ? '親が Plan のため子も Plan になります' : '変更せずに調査と計画だけを行います' }))),
    note ? h('p', { class: 'perm-note', text: note }) : null);
}

/** Changes permission / plan of an existing thread; resolves false when the server did not apply it. */
async function patchPerm(id, change) {
  const t = curThread()?.id === id ? curThread() : threadById(id);
  if (!t) return false;
  const body = { runtime_mode: t.runtime_mode, interaction_mode: t.interaction_mode, ...change };
  try {
    const r = await api(`/api/threads/${id}`, { method: 'PATCH', json: body });
    if (r?.runtime_mode !== body.runtime_mode || r?.interaction_mode !== body.interaction_mode) {
      toast('サーバーが権限の変更を反映しませんでした（サーバーの更新が必要です）', 'err', 6000);
      return false;
    }
    const i = S.threads.findIndex((x) => x.id === id);
    if (i >= 0) S.threads[i] = r;
    if (S.detail?.thread?.id === id) S.detail.thread = r;
    renderComposer();
    if (shownRail() === 'context') renderContextPanel();
    toast('権限設定を変更しました', 'ok', 2000);
    return true;
  } catch (e) { toast(`権限の変更に失敗: ${e.message}`, 'err'); return false; }
}

function composerPerm() {
  const t = curThread();
  if (!t) {
    return permPanel({
      mode: S.compose.mode, plan: S.compose.plan,
      onMode: (v) => { S.compose.mode = v; renderComposer(); },
      onPlan: (v) => { S.compose.plan = v; renderComposer(); },
    });
  }
  return permPanel({
    mode: t.runtime_mode, plan: t.interaction_mode === 'plan', confirmFull: true, note: '変更は次の実行から適用されます',
    onMode: (v) => patchPerm(t.id, { runtime_mode: v }),
    onPlan: (v) => patchPerm(t.id, { interaction_mode: v ? 'plan' : 'default' }),
  });
}

function setComposeSel(sel) {
  S.compose.sel = sel;
  if (!curThread()) { S.heroSel = sel; ls.set('t3.sel', JSON.stringify(sel)); }
  renderComposer();
  if (shownRail() === 'context') renderContextPanel();
}
function openModelPopover(anchor, initialQuery = '') {
  const panel = modelPanel({ value: S.compose.sel, onChange: setComposeSel });
  const el = openPopover(anchor, h('div', { class: 'pop-stack' }, panel.el, composerPerm()), { cls: 'model-pop' });
  if (!el) return;
  if (initialQuery) {
    panel.search.value = initialQuery;
    panel.search.dispatchEvent(new Event('input'));
  }
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
  const collapsed = !(S.kidsOpen.get(t.id) ?? (S.showKids || kids.some((k) => treeHas(k, S.threadId))));
  const rel = t.parent_thread_id ? (REL_LABEL[t.relationship] || t.relationship || '子') : null;
  const status = statusLabel(t.status);

  const title = cleanTitle(t.title);
  const act = (name, ic, label, fn, on) => { const b = ibtn(ic, label, (e) => { e.stopPropagation(); fn(); }, `xs${on ? ' on' : ''}`); b.dataset.act = name; return b; };
  const main = h('div', {
    class: 'trow-main', role: 'button', tabindex: 0,
    title: `${title}\n${provLabel(t.model_selection?.provider)} / ${t.model_selection?.model} · ${status}${rel ? ` · ${rel}` : ''}`,
    onclick: () => openThread(t.id),
    onkeydown: (e) => { if ((e.key === 'Enter' || e.key === ' ') && e.target === e.currentTarget) { e.preventDefault(); openThread(t.id); } },
  },
  hasKids ? h('button', {
    class: `caret${collapsed ? '' : ' open'}`, type: 'button', 'aria-label': collapsed ? '子スレッドを展開' : '子スレッドを折りたたむ', 'aria-expanded': String(!collapsed),
    onclick: (e) => { e.stopPropagation(); S.kidsOpen.set(t.id, collapsed); renderSidebar(); },
  }, iconEl('chevron')) : null,
  shelf ? monogram(projectById(t.project_id), 'sm') : null,
  provDot(t.model_selection?.provider, `${t.status === 'running' ? 'live' : ''} s-${t.status}`),
  h('span', { class: 'trow-title', text: title }),
  t.pinned ? iconEl('pin', 'pinmark') : null,
  hasKids && collapsed ? h('span', { class: 'kidcount', title: `子スレッド ${kids.length} 件`, text: String(kids.length) }) : null,
  pend ? h('span', { class: 'flag warn', text: '承認待ち' })
    : t.status === 'failed' ? h('span', { class: 'flag bad', text: '失敗' })
      : t.status === 'rate-limited' ? h('span', { class: 'flag warn', text: '制限中' })
        : t.status === 'waiting-for-children' ? h('span', { class: 'flag', text: '子待ち' }) : null,
  h('span', { class: 'trow-time', text: fmtAgo(t.updated_at) }),
  h('span', { class: 'trow-actions' },
    act('pin', 'pin', t.pinned ? 'ピン留め解除' : 'ピン留め', () => patchThread(t.id, { pinned: !t.pinned }), t.pinned),
    act('archive', 'archive', t.archived ? 'アーカイブ解除' : 'アーカイブ', () => patchThread(t.id, { archived: !t.archived }), t.archived)));

  return h('li', { class: `trow${t.id === S.threadId ? ' active' : ''}${t.archived ? ' archived' : ''}`, dataset: { id: t.id } },
    main, hasKids && !collapsed ? h('ul', { class: 'tkids' }, kids.map((k) => threadRow(k.t, { kids: k.kids }))) : null);
}

const THREADS_PER_PROJECT = 10;
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
        type: 'button', class: 'more', dataset: { more: p.id }, text: all ? '折りたたむ' : `さらに ${tree.length - shown.length} 件を表示`,
        onclick: () => { all ? S.expandedProjects.delete(p.id) : S.expandedProjects.add(p.id); renderSidebar(); },
      })) : null));
}

/** Projects with threads, most recently active first; projects without threads separately. */
function orderedProjects() {
  const latest = new Map();
  for (const t of S.threads) {
    if (t.archived && !S.showArchived) continue;
    const at = Date.parse(t.updated_at) || 0;
    if (at >= (latest.get(t.project_id) ?? 0)) latest.set(t.project_id, at);
  }
  return {
    active: S.projects.filter((p) => latest.has(p.id)).sort((a, b) => latest.get(b.id) - latest.get(a.id)),
    empty: S.projects.filter((p) => !latest.has(p.id)),
  };
}
const hasThreads = (projectId) => S.threads.some((t) => t.project_id === projectId && !t.archived);
/** New threads default to the current project, unless it is empty: then to the most recently active one. */
function preferActiveProject() {
  const top = orderedProjects().active[0];
  if (top && !hasThreads(S.projectId)) { S.projectId = top.id; ls.set('t3.project', top.id); }
}

function projectList() {
  if (!S.projects.length) return [h('div', { class: 'empty-note', text: 'プロジェクトを追加すると、ここにスレッドが並びます' })];
  const { active, empty } = orderedProjects();
  if (!active.length) return empty.map(projectGroup);
  const open = S.showEmpty;
  return [
    ...active.map(projectGroup),
    empty.length ? h('button', {
      type: 'button', class: 'empty-toggle', 'aria-expanded': String(open),
      onclick: () => { S.showEmpty = !open; ls.set('t3.pempty', S.showEmpty ? '1' : '0'); renderSidebar(); },
    }, iconEl('chevron', `pg-chev${open ? ' open' : ''}`), h('span', { text: `スレッドなし (${empty.length})` })) : null,
    ...(open ? empty.map(projectGroup) : []),
  ].filter(Boolean);
}

/** Remembers which sidebar control had focus so that a re-render does not drop keyboard users to <body>. */
function sidebarFocus() {
  const a = document.activeElement;
  if (!a || !$('#sidebar .sb-scroll').contains(a)) return null;
  const row = a.closest('.trow');
  if (row) {
    const box = a.closest('#shelf') ? '#shelf-list' : '#project-groups';
    const main = `${box} .trow[data-id="${CSS.escape(row.dataset.id)}"] > .trow-main`;
    return a.classList.contains('trow-main') ? main : a.classList.contains('caret') ? `${main} > .caret` : `${main} [data-act="${a.dataset.act}"]`;
  }
  if (a.classList.contains('more')) return `.more[data-more="${CSS.escape(a.dataset.more)}"]`;
  if (a.classList.contains('empty-toggle')) return '.empty-toggle';
  const g = a.closest('.pgroup');
  return g ? `.pgroup[data-project="${CSS.escape(g.dataset.project)}"] ${a.classList.contains('pg-add') ? '.pg-add' : '.pg-toggle'}` : null;
}

/** Unfolds whatever hides the open thread and scrolls its row into view. */
function revealActiveRow() {
  const t = curThread();
  if (!t || (isNarrow() && $('#app').dataset.drawer !== 'open')) return;
  S.collapsedProjects.delete(t.project_id);
  for (let p = threadById(t.parent_thread_id), n = 0; p && n < 20; p = threadById(p.parent_thread_id), n++) S.kidsOpen.delete(p.id);
  renderSidebar();
  const activeRow = $('#project-groups .trow.active > .trow-main') || $('#shelf-list .trow.active > .trow-main');
  activeRow?.scrollIntoView({ block: 'nearest' });
}

let sidebarSig = '';
function renderSidebar() {
  // skip DOM churn (and hover flicker) when nothing visible changed
  const sig = JSON.stringify([Math.floor(Date.now() / 60000), S.threadId, S.projectId, S.showArchived, S.showKids, S.showEmpty, [...S.kidsOpen], [...S.collapsedProjects], [...S.expandedProjects],
    [...S.pending].map(([k, v]) => [k, v.length]), S.projects.map((p) => [p.id, p.title]),
    S.threads.map((t) => [t.id, t.title, t.status, t.pinned, t.archived, t.updated_at, t.project_id, t.parent_thread_id, t.model_selection?.provider])]);
  const shelf = S.threads.filter((t) => !t.archived && (isBusy(t.status) && t.status !== 'waiting-for-children' || (S.pending.get(t.id) || []).length));
  document.title = `${[...S.pending.values()].some((a) => a.length) ? '⚠ ' : shelf.length ? `● ${shelf.length} ` : ''}${curThread() ? `${cleanTitle(curThread().title)} — ` : ''}t3code-rs`;
  if (sig === sidebarSig) return;
  sidebarSig = sig;
  const focus = sidebarFocus();
  shelf.sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at));
  $('#shelf').hidden = !shelf.length;
  $('#shelf-count').textContent = String(shelf.length);
  $('#shelf-list').replaceChildren(...shelf.map((t) => threadRow(t, { shelf: true })));
  $('#project-groups').replaceChildren(...projectList());
  if (focus) $(`#sidebar ${focus}`)?.focus({ preventScroll: true });
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
  loadWorkspaceMeta(S.projectId);
}

/** Cached workspace metadata (local skills + file list) for `/` and `@file` completion. Failures stay silent and yield an empty set. */
async function loadWorkspaceMeta(projectId) {
  if (!projectId) return { commands: [], files: [] };
  const hit = S.workspaceMeta.get(projectId);
  if (hit) return hit;
  try {
    const meta = await api(`/api/projects/${projectId}/workspace-meta`);
    const out = { commands: Array.isArray(meta?.commands) ? meta.commands : [], files: Array.isArray(meta?.files) ? meta.files : [] };
    S.workspaceMeta.set(projectId, out);
    return out;
  } catch { /* silent: completion simply offers nothing extra */
    const empty = { commands: [], files: [] };
    S.workspaceMeta.set(projectId, empty);
    return empty;
  }
}
const workspaceMetaFor = (projectId) => S.workspaceMeta.get(projectId) || { commands: [], files: [] };

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
  closePopover(); closeDrawer(); hideCommandPalette();
  stashDraft();
  if (S.threadId) clearBlobCache();
  S.threadId = null; S.detail = null; S.diffStat = null; S.diffData = null; S.diffError = null;
  diffOpenFiles.clear();
  if (projectId && projectById(projectId)) { S.projectId = projectId; ls.set('t3.project', projectId); } else preferActiveProject();
  loadWorkspaceMeta(S.projectId);
  S.compose = { sel: defaultSelection(S.projectId), mode: 'approval-required', plan: false, isolation: 'shared', baseRef: '' };
  setHash('');
  restoreDraft();
  syncSubscriptions();
  renderAll();
  if (S.rail === 'diff') loadDiff();
  if (!isCoarse()) ta().focus();
}

async function openThread(id, { push = true } = {}) {
  if (!id) return;
  closePopover(); closeDrawer(); hideCommandPalette();
  if (push) setHash(`#/t/${id}`);
  if (id !== S.threadId) {
    stashDraft();
    S.threadId = id;
    S.detail = null; S.diffStat = null; S.diffData = null; S.diffError = null; S.parentDiff = null;
    diffOpenFiles.clear();
    clearBlobCache();
    restoreDraft();
    $('#main').dataset.view = 'thread';
    tl().replaceChildren(h('div', { class: 'loading', text: '読み込み中…' }));
    renderHeader(); renderApprovals(); renderSwarm(); renderContextPanel();
  }
  S.stick = true;
  syncSubscriptions();
  loadSwarm(id);
  await loadDetail(id, { reset: true });
  loadWorkspaceMeta(S.projectId);
  if (id === S.threadId) revealActiveRow();
}

const PAGE_SIZE = 150;
/** Accepts both the paginated { items, has_more, total_items } reply and a legacy full { items } one. */
function normalizeDetail(d) {
  d.items = [...(d.items || [])].sort((a, b) => a.ordinal - b.ordinal);
  d.has_more = d.has_more === true;
  d.total_items = Number(d.total_items) || d.items.length;
  d.attachments ||= [];
  return d;
}
const mergeById = (a, b) => [...new Map([...a, ...b].map((x) => [x.id, x])).values()];
/** Keeps already loaded older pages across a refresh, provided the new window still touches them. */
function keepOlderPages(d, prev) {
  if (!d.has_more || prev?.thread?.id !== d.thread.id || !prev.items.length || !d.items.length) return d;
  const fresh = new Set(d.items.map((x) => x.id));
  if (!prev.items.some((x) => fresh.has(x.id))) return d;
  const first = d.items[0].ordinal;
  const older = prev.items.filter((x) => x.ordinal < first && !fresh.has(x.id));
  if (!older.length) return d;
  d.items = [...older, ...d.items];
  d.has_more = prev.has_more;
  d.attachments = mergeById(prev.attachments, d.attachments);
  return d;
}

async function loadDetail(id, { reset = false } = {}) {
  let d;
  try { d = normalizeDetail(await api(`/api/threads/${id}?limit=${PAGE_SIZE}`)); } catch (e) {
    if (id === S.threadId) {
      toast(`スレッドを読み込めません: ${e.message}`, 'err');
      if (e.status === 404) goHome();
      else if (!S.detail) tl().replaceChildren(h('div', { class: 'loading', text: '読み込みに失敗しました' }));
    }
    return;
  }
  if (id !== S.threadId) return;
  S.detail = keepOlderPages(d, S.detail);
  // Restore an in-flight turn's streamed text after a reload. A longer
  // client-side live text is never overwritten by a shorter server one.
  if (d.live?.run_id && isBusy(d.thread.status)) {
    const cur = S.live.get(id);
    const next = { runId: d.live.run_id, assistant: d.live.assistant || '', reasoning: d.live.reasoning || '' };
    if (cur?.runId === next.runId) {
      if ((cur.assistant || '').length > next.assistant.length) next.assistant = cur.assistant;
      if ((cur.reasoning || '').length > next.reasoning.length) next.reasoning = cur.reasoning;
    }
    S.live.set(id, next);
  }
  pruneOptimistic(d);
  S.projectId = d.thread.project_id; ls.set('t3.project', S.projectId);
  const i = S.threads.findIndex((t) => t.id === id);
  if (i >= 0) S.threads[i] = d.thread; else S.threads.push(d.thread);
  if (reset) S.compose.sel = { ...d.thread.model_selection };
  renderAll();
  if (reset) {
    scrollBottom(true);
    if (S.rail === 'diff') loadDiff(); else refreshDiffStat();
  }
}

/** Prepends the previous page and keeps the reader's place. Resolves true when something was added. */
async function loadOlder(limit = PAGE_SIZE) {
  const d = S.detail;
  if (!d?.has_more || S.loadingOlder === d.thread.id || !d.items.length) return false;
  const id = d.thread.id;
  S.loadingOlder = id;
  const btn = $('#load-older');
  if (btn) { btn.disabled = true; btn.textContent = '読み込み中…'; }
  try {
    const r = await api(`/api/threads/${id}?before_ordinal=${d.items[0].ordinal}&limit=${limit}`);
    if (S.detail?.thread?.id !== id) return false;
    const cur = S.detail;
    const first = cur.items[0].ordinal;
    const older = (r.items || []).filter((x) => x.ordinal < first).sort((a, b) => a.ordinal - b.ordinal);
    cur.items = [...older, ...cur.items];
    cur.has_more = r.has_more === true && older.length > 0;
    cur.attachments = mergeById(cur.attachments, r.attachments || []);
    S.loadingOlder = null;
    renderKeepingAnchor();
    return older.length > 0;
  } catch (e) {
    toast(`以前の履歴を読み込めません: ${e.message}`, 'err');
    S.loadingOlder = null;
    if (S.detail?.thread?.id === id) renderKeepingAnchor();
    return false;
  } finally { if (S.loadingOlder === id) S.loadingOlder = null; }
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
  renderQueueStrip();
  renderStatPill();
  renderSwarm();
  renderContextMeters();
  renderContextPanel();
}

// ───────────────────────── topbar ─────────────────────────
const latestRun = () => { const r = S.detail?.runs || []; return r.length ? r[r.length - 1] : null; };

function renderPauseAll() {
  const btn = $('#btn-pause-all');
  if (!btn) return;
  btn.textContent = S.pauseAll ? '再開' : '一時停止';
  btn.title = S.pauseAll ? '自動実行の保留を解除します' : '実行中のターンを終えてから、自動実行を保留します';
  btn.setAttribute('aria-pressed', String(S.pauseAll));
}

async function refreshPauseAll() {
  try {
    const state = await api('/api/pause-all');
    if (typeof state.paused === 'boolean' && state.paused !== S.pauseAll) {
      S.pauseAll = state.paused;
      renderPauseAll();
    }
  } catch { /* The rest of the workspace remains usable if this request fails. */ }
}

function renderHeader() {
  renderPauseAll();
  const t = curThread();
  const box = $('#tb-title');
  const kidsBtn = $('#btn-children');
  const kids = (t && S.detail?.children) || [];
  kidsBtn.hidden = !kids.length;
  $('.n', kidsBtn).textContent = String(kids.length);
  kidsBtn.setAttribute('aria-label', `子スレッド ${kids.length} 件`);
  $$('.thread-only').forEach((b) => { b.hidden = !S.threadId; b.disabled = !t; });
  renderDiffBadge();
  if (box.querySelector('input.title-edit') && box.dataset.id === t?.id) return; // don't clobber an inline rename
  box.dataset.id = t?.id || '';
  const project = projectById(t ? t.project_id : S.projectId);
  const badge = project ? h('span', { class: 'proj-pill', title: project.workspace_root }, monogram(project, 'sm'), h('span', { text: project.title })) : null;
  if (!t) {
    box.replaceChildren(...[h('span', { class: 'tb-draft', text: S.threadId ? '' : '新規スレッド' }), S.threadId ? null : badge].filter(Boolean));
    return;
  }
  const parent = t.parent_thread_id ? threadById(t.parent_thread_id) : null;
  box.replaceChildren(...[
    t.parent_thread_id ? h('button', { class: 'crumb', type: 'button', title: '親スレッドを開く', onclick: () => openThread(t.parent_thread_id) },
      h('span', { text: clip(parent ? cleanTitle(parent.title) : t.parent_thread_id.slice(0, 8), 20) }), h('span', { class: 'sep', text: '/' })) : null,
    h('button', { type: 'button', id: 'thread-title', class: 'title-btn', title: 'クリックして名前を変更', text: cleanTitle(t.title), onclick: renameInline }),
    badge,
    t.branch ? h('span', { class: 'branch-pill', title: t.worktree_path ? `worktree: ${t.worktree_path}` : 'ブランチ' }, iconEl('fork'), h('span', { text: t.branch }), t.worktree_path ? h('em', { text: 'worktree' }) : null) : null,
    ['idle', 'completed'].includes(t.status) ? null : h('span', { class: `status-pill s-${t.status}`, text: statusLabel(t.status) }),
  ].filter(Boolean));
}

function childrenMenu(anchor) {
  const kids = S.detail?.children || [];
  if (!kids.length) return;
  const parent = curThread();
  const rows = [...kids].sort(threadCmp).map((k) => {
    const openBtn = h('button', {
      type: 'button', role: 'menuitem', class: 'menu-item kid-item', title: cleanTitle(k.title), dataset: { child: k.id },
      onclick: () => openThread(k.id),
    }, provDot(k.model_selection?.provider, `${k.status === 'running' ? 'live' : ''} s-${k.status}`), h('span', { class: 't', text: cleanTitle(k.title) }), h('small', { text: statusLabel(k.status) }));
    const acts = isWorktreeChild(k, parent) ? h('span', { class: 'kid-acts' },
      h('button', {
        type: 'button', class: 'btn xs', text: '親との差分', title: '親スレッドとの差分を表示',
        onclick: (e) => { e.stopPropagation(); showParentDiff(parent.id, k.id); },
      }),
      mergeConfirmControl(parent.id, k, 'btn xs')) : null;
    return h('div', { class: 'kid-row' }, openBtn, acts);
  });
  openPopover(anchor, h('div', { class: 'menu kids-list', role: 'menu', 'aria-label': '子スレッド' }, rows), { cls: 'kids-pop', below: true });
}

/** 専用 worktree 子スレッドか: 親と worktree/ブランチが異なる子。親が未読込でも worktree_path があれば対象とする。 */
function isWorktreeChild(child, parent) {
  if (!child?.parent_thread_id || !child.worktree_path) return false;
  if (parent && child.worktree_path === parent.worktree_path) return false;
  if (parent && child.branch && parent.branch && child.branch === parent.branch) return false;
  return true;
}

/** 親スレッドとの差分を取得し、#diff-panel にバナー付きで表示する。 */
async function showParentDiff(parentId, childId) {
  const child = threadById(childId) || (S.detail?.children || []).find((k) => k.id === childId) || null;
  closePopover();
  diffSeq++; // 進行中の通常差分取得を無効化する
  S.parentDiff = { parentId, childId, branch: child?.branch || '', data: null };
  setRail('diff'); // loadDiff は parentDiff 表示中は何もしない
  const body = $('#diff-body');
  body?.replaceChildren(h('div', { class: 'dp-empty', text: '親との差分を読み込み中…' }));
  try {
    const d = await api(`/api/threads/${parentId}/children/${childId}/diff?ignore_whitespace=${S.ignoreWs}`);
    if (!S.parentDiff || S.parentDiff.childId !== childId) return; // 別スレッドへ移動済み
    diffSeq++; // setRail 経由の通常差分取得が残っていれば無効化する
    S.parentDiff.data = d;
    if (!S.parentDiff.branch) S.parentDiff.branch = d.branch || '';
    diffShownFor = childId;
    renderDiffView(d);
  } catch (e) {
    S.parentDiff = null;
    toast(`親との差分を取得できません: ${e.message}`, 'err');
    loadDiff();
  }
}

/** インライン 2 段階確認の「親へマージ」コントロール（window.confirm は使わない）。 */
function mergeConfirmControl(parentId, child, btnClass = 'btn xs') {
  const wrap = h('span', { class: 'merge-ctl' });
  const showIdle = () => {
    wrap.replaceChildren(h('button', {
      type: 'button', class: btnClass, text: '親へマージ', title: 'この子スレッドを親スレッドへマージする',
      onclick: (e) => { e.stopPropagation(); showArmed(); },
    }));
  };
  const showArmed = () => {
    const go = h('button', {
      type: 'button', class: `${btnClass} primary`, text: 'マージ実行',
      onclick: (e) => { e.stopPropagation(); doMerge(parentId, child, go, cancel); },
    });
    const cancel = h('button', {
      type: 'button', class: btnClass, text: 'キャンセル',
      onclick: (e) => { e.stopPropagation(); showIdle(); },
    });
    wrap.replaceChildren(h('span', { class: 'merge-q', text: '本当にマージ？' }), go, cancel);
    setTimeout(() => { if (wrap.isConnected && wrap.contains(go) && !go.disabled) showIdle(); }, 10000);
  };
  showIdle();
  return wrap;
}

async function doMerge(parentId, child, goBtn, cancelBtn) {
  goBtn.disabled = true;
  if (cancelBtn) cancelBtn.disabled = true;
  try {
    const r = await api(`/api/threads/${parentId}/children/${child.id}/merge`, { method: 'POST', json: {} });
    if (r.merged) {
      toast('親へマージしました', 'ok');
      closePopover();
      await loadThreads();
      loadDetailSoon();
      refreshDiffSoon();
    } else {
      const files = r.conflict_files || [];
      toast(files.length ? `マージ競合: ${files.slice(0, 3).join(', ')}${files.length > 3 ? ` 他 ${files.length - 3} 件` : ''}` : 'マージできませんでした（競合）', 'err', 6000);
      goBtn.disabled = false;
      if (cancelBtn) cancelBtn.disabled = false;
    }
  } catch (e) {
    toast(`マージに失敗: ${e.message}`, 'err');
    goBtn.disabled = false;
    if (cancelBtn) cancelBtn.disabled = false;
  }
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
      Object.values(orderedProjects()).flat().map((p) => h('button', {
        type: 'button', role: 'radio', class: `proj-chip${p.id === S.projectId ? ' on' : ''}`, 'aria-checked': String(p.id === S.projectId), title: p.workspace_root, dataset: { project: p.id },
        onclick: () => {
          S.projectId = p.id; ls.set('t3.project', p.id);
          loadWorkspaceMeta(p.id);
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
    h('button', { type: 'button', class: 'swarm-cta', id: 'btn-swarm-cta', title: '入力欄の先頭に /swarm を挿入', onclick: insertSwarmCommand },
      iconEl('swarm'), h('code', { text: '/swarm' }), h('span', { text: 'で複数エージェントの協調を始める' })),
    h('div', { id: 'hero-recent' }));
  renderHeroRecent();
}
function renderHeroRecent() {
  const box = $('#hero-recent');
  if (!box) return;
  const recent = S.threads.filter((t) => !t.archived).sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at)).slice(0, 4);
  box.replaceChildren(...[
    recent.length ? h('div', { class: 'recent' }, h('span', { class: 'recent-lab', text: '最近のスレッド' }), recent.map((t) => h('button', { type: 'button', class: 'recent-chip', title: cleanTitle(t.title), onclick: () => openThread(t.id) },
      provDot(t.model_selection?.provider, `s-${t.status}`), h('span', { class: 't', text: cleanTitle(t.title) }), h('small', { text: fmtAgo(t.updated_at) })))) : null,
    h('div', { class: 'hero-keys' },
      [['/', 'コマンド'], ['!', 'シェル実行']].map(([k, l]) => h('span', {}, h('kbd', { text: k }), l)),
      [['K', '検索'], ['N', '新規'], ['B', 'サイドバー'], ['I', 'コンテキスト'], ['D', '差分'], ['U', '使用量']].map(([k, l]) => h('span', {}, h('kbd', { text: kbd(k) }), l))),
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

// ── compact task step cards ──
const STEP_KINDS = new Set(['tool_call', 'tool_result', 'command_execution', 'file_change', 'subagent', 'handoff']);
const SEARCH_NOTICES = new Set(['file_search', 'web_search']);
const noticeType = (it) => (it.kind === 'system_notice' ? it.payload?.type || '' : '');
const isStep = (it) => STEP_KINDS.has(it.kind) || SEARCH_NOTICES.has(noticeType(it));
const isErrorNotice = (p) => p?.status === 'failed' || p?.level === 'error' || p?.is_error === true;
const TOOL_VERBS = [
  [/^(bash|shell|exec|exec_command|run_command|run_shell_command|command|local_shell)$/i, 'コマンド実行', 'terminal'],
  [/^(read|read_file|view|view_file|cat)$/i, 'ファイル読込', 'file'],
  [/^(edit|write|multiedit|multi_edit|notebookedit|write_file|edit_file|replace|patch|apply_patch|str_replace_based_edit_tool)$/i, 'ファイル編集', 'filePen'],
  [/^(grep|glob|search|ls|list|list_dir|list_directory|find|search_files|grep_search|toolsearch)$/i, '検索', 'search'],
  [/^(web_?search|web_?fetch|google_web_search|fetch|read_url)$/i, 'Web 参照', 'globe'],
  [/^(task|agent|subagent|delegate_task|mcp__.*delegate_task)$/i, 'サブエージェント', 'bot'],
];
const DRIVER_PROVIDER = { claudeAgent: 'claude', codex: 'codex', antigravity: 'antigravity', opencode: 'open-code' };
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
/** Output block that starts as a preview when the text is huge. */
function outputBlock(text) {
  const s = String(text ?? '');
  const lines = s.split('\n');
  if (s.length <= 3000 && lines.length <= 40) return pre(s);
  const preview = lines.slice(0, 40).join('\n').slice(0, 3000);
  const box = h('div', { class: 'out-trunc' }, pre(`${preview}\n…`));
  box.append(h('button', { type: 'button', class: 'btn xs out-more', text: `すべて表示（${fmtNum(s.length)} 文字・${fmtNum(lines.length)} 行）`, onclick: () => box.replaceChildren(pre(s, 200000)) }));
  return box;
}
const section = (label, node) => h('div', { class: 's-sec' }, h('div', { class: 's-lab', text: label }), node);

/** One single-line expandable row. The body is built lazily on first expand (outputs can be huge). */
function stepRow({ item, icon, verb, target, targetTitle, badges = [], sections = [], cls = '', trailing, open = false }) {
  const body = h('div', { class: 's-body' });
  const bytes = h('span', { class: 'sbytes' });
  const badgeBox = h('span', { class: 's-badges' }, badges, bytes);
  const el = h('details', { class: `step ${cls}`, dataset: { item: item.id }, open },
    h('summary', {}, iconEl(icon, 's-ic'), h('span', { class: 's-verb', text: verb }), h('span', { class: 's-target', text: target, title: targetTitle || target }), badgeBox, trailing, iconEl('chevron', 's-chev')),
    body);
  const queue = [...sections];
  const flush = () => { if (el.open) while (queue.length) body.append(queue.shift()()); };
  el.addEventListener('toggle', flush);
  if (open) flush();
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

// ── file changes ──
const lineBadges = (add, del) => [add ? badge(`+${add}`, 'plus') : null, del ? badge(`−${del}`, 'minus') : null].filter(Boolean);
function fileBlock(f, open) {
  if (!f.patch) {
    return h('button', { class: 'fchip', type: 'button', title: `${f.path}\nGit 差分を開く`, onclick: () => setRail('diff') },
      h('span', { class: `fk ${f.letter}`, text: f.letter }), h('span', { class: 'p', text: compactPath(f.path) }), lineBadges(f.add, f.del));
  }
  const d = h('details', { class: 'finline', open },
    h('summary', {}, iconEl('chevron', 's-chev'), h('span', { class: `fk ${f.letter}`, text: f.letter }), h('span', { class: 'p', title: f.path, text: compactPath(f.path) }),
      lineBadges(f.add, f.del), ibtn('copy', 'パスをコピー', (e) => { e.preventDefault(); e.stopPropagation(); copyText(f.path, 'パスをコピーしました'); }, 'xs')));
  let drawn = false;
  const draw = () => { if (d.open && !drawn) { drawn = true; d.append(patchLines(f.patch, 400, f.path)); } };
  d.addEventListener('toggle', draw);
  draw();
  return d;
}

function buildStep(item) {
  const p = item.payload;
  const sum = item.summary || '';
  switch (SEARCH_NOTICES.has(noticeType(item)) ? 'search' : item.kind) {
    case 'search': {
      const patterns = [...new Set([p.pattern, ...(Array.isArray(p.patterns) ? p.patterns : []), p.query].filter((x) => typeof x === 'string' && x))];
      const web = p.type === 'web_search';
      const target = patterns.join(' · ') || p.title || sum || '検索';
      return stepRow({
        item, icon: web ? 'globe' : 'search', verb: web ? 'Web 検索' : 'ファイル検索', target: oneLine(target, 220),
        badges: isErrorNotice(p) ? [badge('エラー', 'bad')] : [], cls: isErrorNotice(p) ? 'search bad' : 'search',
        sections: [() => section('検索語', pre(patterns.join('\n') || target))],
      });
    }
    case 'tool_call': {
      const name = pstr(p, 'name', 'tool', 'tool_name', 'title') || (p?.type === 'web_search' ? 'web_search' : '') || sum || 'tool';
      const args = p?.input ?? p?.arguments ?? p?.args ?? p?.params ?? p?.state?.input ?? p;
      const found = typeof args === 'string' ? args
        : args?.command ?? args?.file_path ?? args?.filePath ?? args?.path ?? args?.query ?? args?.pattern ?? args?.url ?? args?.description ?? p?.query ?? p?.state?.title ?? '';
      const [, verb, icon] = TOOL_VERBS.find(([re]) => re.test(name)) || [null, 'ツール呼び出し', 'tool'];
      const specific = verb !== 'ツール呼び出し';
      const label = p?.server ? `${p.server} · ${name}` : name;
      const target = oneLine(textOf(found) || sum, 220);
      const out = textOf(p?.output ?? p?.state?.output);
      const bad = isErrorResult(p);
      const row = stepRow({
        item, icon, verb, target: specific ? target || label : [label, target && target !== label ? target : ''].filter(Boolean).join('  '),
        cls: bad ? 'tool bad' : 'tool', badges: bad ? [badge('エラー', 'bad')] : [],
        sections: [() => section(`入力 · ${label}`, pre(pretty(args))), ...(out ? [() => section('出力', outputBlock(out))] : [])],
      });
      row.output = out;
      return row;
    }
    case 'tool_result': {
      const out = resultText(p) || sum;
      const bad = isErrorResult(p);
      return stepRow({ item, icon: bad ? 'alert' : 'check', verb: 'ツール結果', target: oneLine(out, 220), badges: bad ? [badge('エラー', 'bad')] : [], cls: bad ? 'tool bad' : 'tool', sections: [() => section('出力', outputBlock(out))] });
    }
    case 'command_execution': {
      const cmd = pstr(p, 'command', 'cmd', 'title') || (Array.isArray(p?.command) ? p.command.join(' ') : '') || sum || pstr(p, 'step_type');
      const userShell = p?.source === 'user_shell';
      const verifyHook = p?.source === 'verify_hook';
      const out = userShell
        ? pstr(p, 'output') || [textOf(p?.stdout), textOf(p?.stderr)].filter(Boolean).join('\n')
        : [textOf(p?.output ?? p?.stdout ?? p?.aggregated_output ?? p?.state?.output), textOf(p?.stderr)].filter(Boolean).join('\n');
      const code = p?.exit_code ?? p?.exitCode ?? p?.code;
      const timedOut = p?.timed_out === true;
      const bad = timedOut || (code != null && Number(code) !== 0);
      const dur = Number(p?.duration_ms);
      const badges = [
        timedOut ? badge('timeout', 'bad') : code == null ? null : badge(`exit ${code}`, bad ? 'bad' : 'good'),
        (userShell || verifyHook) && Number.isFinite(dur) ? badge(fmtDur(dur)) : null,
      ].filter(Boolean);
      return stepRow({
        item, icon: 'terminal', verb: verifyHook ? '自動検証' : userShell ? 'シェル実行' : 'コマンド実行', target: oneLine(cmd, 260),
        cls: `cmd${userShell ? ' user-shell' : ''}${verifyHook ? ' verify-hook' : ''}${bad ? ' bad' : ''}`,
        badges, open: userShell || (verifyHook && bad),
        sections: [
          () => section(p?.cwd ? `コマンド (${compactPath(p.cwd)})` : 'コマンド', pre(cmd)),
          () => section('出力', out ? outputBlock(out) : h('div', { class: 'muted', text: '出力はありません' })),
        ],
      });
    }
    case 'file_change': {
      const files = changedFiles(p);
      const add = files.reduce((a, f) => a + f.add, 0); const del = files.reduce((a, f) => a + f.del, 0);
      const target = files.length ? `${compactPath(files[0].path)}${files.length > 1 ? ` 他 ${files.length - 1} 件` : ''}` : oneLine(sum || pstr(p, 'step_type') || 'ファイル', 220);
      return stepRow({
        item, icon: 'filePen', verb: 'ファイル変更', target, targetTitle: files.map((f) => f.path).join('\n'), cls: 'fchg',
        badges: lineBadges(add, del),
        sections: [() => (files.length ? h('div', { class: 'files' }, files.slice(0, 80).map((f) => fileBlock(f, files.length <= 3)))
          : h('div', { class: 'muted', text: '変更内容の詳細はありません' }))],
      });
    }
    case 'subagent': {
      const cid = resolveThreadId(pstr(p, 'child_thread_id', 'childThreadId') || pstr(p, 'thread_id'));
      const child = cid ? threadById(cid) : null;
      const st = child?.status || pstr(p, 'status') || 'idle';
      const title = cleanTitle(child?.title || pstr(p, 'title', 'name', 'task') || sum || 'サブエージェント');
      const provider = DRIVER_PROVIDER[p?.driver] || p?.provider;
      const sel = child?.model_selection || (provider ? { provider, model: p.model } : null);
      const prompt = pstr(p, 'prompt', 'task');
      const result = typeof p?.result === 'string' ? p.result : pstr(p?.result, 'text', 'summary', 'content', 'message') || (Array.isArray(p?.result) ? textOf(p.result) : '');
      const progress = pstr(p, 'progress');
      const row = stepRow({
        item, icon: 'bot', verb: 'サブエージェント', target: title, cls: 'sub',
        badges: [sel ? h('span', { class: 'sbadge prov' }, provDot(sel.provider), provLabel(sel.provider)) : null, h('span', { class: `sbadge sub-status s-${st}`, text: statusLabel(st) })].filter(Boolean),
        trailing: cid ? h('button', { class: 'btn xs open-child', type: 'button', text: 'スレッドを開く →', onclick: (e) => { e.preventDefault(); openThread(cid); } }) : null,
        sections: [
          ...(prompt ? [() => h('details', { class: 'sub-prompt' }, h('summary', {}, iconEl('chevron', 's-chev'), h('span', { text: '指示内容（プロンプト）' }), h('small', { text: `${fmtNum(prompt.length)}文字` })), pre(prompt))] : []),
          ...(progress && !result ? [() => section('進捗', h('div', { class: 'muted', text: progress }))] : []),
          () => section('結果', result ? mdEl(result, 'md sub-result') : h('div', { class: 'muted', text: 'まだ結果はありません' })),
        ],
      });
      row.dataset.child = cid;
      return row;
    }
    case 'handoff': {
      const text = pstr(p, 'summary', 'text', 'content') || sum || 'コンテキストを引き継ぎました';
      return stepRow({ item, icon: 'handoff', verb: 'コンテキスト引き継ぎ', target: oneLine(text, 160), cls: 'handoff', sections: [() => mdEl(text)] });
    }
    default:
      return null;
  }
}

const callRows = new Map(); // `${run}:${callId}` -> tool_call row, so results fold into their call

/** Appends an item to `container` (before `before`), grouping consecutive steps into one stack. */
function appendItem(item, container, before = null) {
  if (!isStep(item)) { const el = renderMessage(item); if (el) container.insertBefore(el, before); return; }
  const cid = callIdOf(item.payload);
  const key = cid ? `${item.run_id}:${cid}` : null;
  if (item.kind === 'tool_result' && key && callRows.has(key)) {
    const row = callRows.get(key);
    callRows.delete(key);
    const out = resultText(item.payload) || item.summary || '';
    row.dataset.also = item.id;
    if (isErrorResult(item.payload)) { row.classList.add('bad'); row.addBadge(badge('エラー', 'bad')); }
    row.addBytes(item);
    if (out !== row.output) row.addSection(() => section('出力', outputBlock(out)));
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
  btn.hidden = n <= STEPS_VISIBLE || !!g.closest('.work-fold');
  btn.textContent = g.classList.contains('expanded') ? '以前のステップを隠す' : `以前の ${n - STEPS_VISIBLE} ステップを表示`;
}

// ── "worked for X" fold of a finished run ──
const foldIndex = new Map(); // item id -> work fold that contains it
let keepOpen = new Set();    // item ids whose <details> the reader had open before a re-render
const FOLD_COUNTS = [
  ['コマンド', (it) => it.kind === 'command_execution'],
  ['ツール', (it) => it.kind === 'tool_call'],
  ['編集', (it) => it.kind === 'file_change'],
  ['検索', (it) => SEARCH_NOTICES.has(noticeType(it))],
  ['サブエージェント', (it) => it.kind === 'subagent'],
  ['思考', (it) => it.kind === 'reasoning'],
  ['途中経過', (it) => it.kind === 'assistant_message'],
  ['通知', (it) => noticeType(it) === 'notification'],
];
function foldLabel(items, durationMs) {
  const counts = FOLD_COUNTS.map(([label, test]) => [label, items.filter(test).length]).filter(([, n]) => n > 0);
  const parts = counts.map(([label, n]) => `${label} ${fmtNum(n)}`);
  if (durationMs > 0) parts.unshift(`${fmtDur(durationMs).replace(/\.\d秒$/, '秒')}作業`);
  if (!counts.length) parts.push(`作業ステップ ${fmtNum(items.length)}`);
  return parts.join(' · ');
}
/** Collapsed summary row; the step DOM is only built when the reader opens it. */
function workFold(items, durationMs) {
  const body = h('div', { class: 'wf-body' });
  const bad = items.filter((it) => (it.kind === 'command_execution' && Number(it.payload?.exitCode ?? it.payload?.exit_code ?? 0) !== 0) || (it.kind === 'subagent' && it.payload?.status === 'failed')).length;
  const el = h('details', { class: 'work-fold', dataset: { last: items.at(-1).id } },
    h('summary', {}, iconEl('chevron', 's-chev'), h('span', { class: 'wf-sum', text: foldLabel(items, durationMs) }), bad ? h('span', { class: 'wf-bad', title: '失敗したステップ', text: `失敗 ${bad}` }) : null),
    body);
  let built = false;
  el.build = () => {
    if (built) return;
    built = true;
    for (const it of items) appendItem(it, body);
    $$('.steps', body).forEach((g) => g.classList.add('expanded'));
    $$('details[data-item]', body).forEach((d) => { if (keepOpen.has(d.dataset.item)) d.open = true; });
  };
  el.addEventListener('toggle', () => { if (el.open) el.build(); });
  for (const it of items) foldIndex.set(it.id, el);
  return el;
}

const isRunDone = (run, fallback) => (run ? !['queued', 'running', 'waiting-for-children', 'rate-limited'].includes(run.status) : fallback);

/** Renders one finished stretch: user message(s) → work fold → final answer → trailing notes. */
function appendFinished(items, group, durationMs) {
  const chunks = [{ users: [], rest: [] }];
  for (const it of items) {
    let c = chunks.at(-1);
    if (it.kind === 'user_message') {
      if (c.rest.length) { c = { users: [], rest: [] }; chunks.push(c); }
      c.users.push(it);
    } else c.rest.push(it);
  }
  for (const { users, rest } of chunks) {
    users.forEach((it) => appendItem(it, group));
    const final = rest.findLast((it) => it.kind === 'assistant_message');
    const tail = []; const inner = [];
    for (const it of rest) {
      if (it === final) continue;
      const type = noticeType(it);
      if (type === 'checkpoint') { if (fileEntries(it.payload).length) tail.push(it); } else if (it.kind === 'system_notice' && !SEARCH_NOTICES.has(type) && isErrorNotice(it.payload)) tail.push(it); else inner.push(it);
    }
    const worked = inner.some((it) => it.kind !== 'assistant_message' && (it.kind !== 'system_notice' || isStep(it)));
    if (!worked) { rest.forEach((it) => appendItem(it, group)); continue; }
    const fold = workFold(inner, chunks.length === 1 ? durationMs : null);
    if (inner.some((it) => keepOpen.has(it.id))) { fold.build(); fold.open = true; }
    group.append(fold);
    if (final) appendItem(final, group);
    tail.forEach((it) => appendItem(it, group));
  }
}

// ── messages ──
/** Rough visual line count, so that long prompts fold without forcing a layout. */
function isLongText(text) {
  const perLine = isNarrow() ? 40 : 78;
  let lines = 0;
  for (const l of String(text).split('\n')) {
    lines += Math.max(1, Math.ceil((l.length + (l.match(/[^\u0000-ÿ]/g)?.length || 0)) / perLine));
    if (lines > 14) return true;
  }
  return false;
}
function userBubble(text, key) {
  const bubble = h('div', { class: 'bubble', text });
  if (!isLongText(text)) return [bubble];
  const btn = h('button', { type: 'button', class: 'bubble-toggle' });
  const set = (open) => {
    bubble.classList.toggle('clamp', !open);
    btn.textContent = open ? '折りたたむ' : '全文を表示';
    btn.setAttribute('aria-expanded', String(open));
  };
  btn.onclick = () => {
    const open = bubble.classList.contains('clamp');
    open ? S.expandedMsgs.add(key) : S.expandedMsgs.delete(key);
    S.stick = false;
    set(open);
    if (!open) bubble.scrollIntoView({ block: 'nearest' });
  };
  set(S.expandedMsgs.has(key));
  return [bubble, btn];
}

/** system_notice → one compact line. Payloads are never dumped as JSON; empty checkpoints are hidden. */
function renderNotice(item) {
  const p = item.payload && typeof item.payload === 'object' ? item.payload : { message: String(item.payload ?? '') };
  const id = { item: item.id };
  if (p.type === 'checkpoint') {
    const files = fileEntries(p);
    if (!files.length) return null;
    return h('button', { type: 'button', class: 'ckpt', dataset: id, title: `${files.map((f) => f.path).join('\n')}\nGit 差分を開く`, onclick: () => setRail('diff') },
      iconEl('filePen'), h('span', { text: `${fmtNum(files.length)} ファイル変更` }));
  }
  const bad = isErrorNotice(p);
  let text; let cid = '';
  if (p.type === 'notification') {
    text = p.summary || p.title || 'システム通知';
    cid = resolveThreadId(p.source?.childThreadId || p.child_thread_id);
  } else {
    text = p.title || p.message || p.type || 'システム通知';
    const detail = p.type === 'error' ? p.failure?.message : '';
    if (detail && detail !== text) text = `${text}: ${detail}`;
  }
  text = String(typeof text === 'string' ? text : 'システム通知');
  return h('div', { class: `notice${bad ? ' err' : ''}`, dataset: id }, iconEl(bad ? 'alert' : 'info'),
    h('span', { class: 'notice-text', title: clip(text, 600), text: oneLine(text, 240) }),
    cid ? h('button', { type: 'button', class: 'link notice-open', text: 'スレッドを開く →', onclick: () => openThread(cid) }) : null);
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
        text ? userBubble(text, item.id) : null,
        h('div', { class: 'msg-meta' }, time(), ibtn('copy', 'メッセージをコピー', () => copyText(text), 'xs')));
    }
    case 'assistant_message': {
      const text = pstr(p, 'text', 'content', 'message') || sum;
      return h('div', { class: 'msg assistant', ...id }, mdEl(text),
        h('div', { class: 'msg-meta' }, ibtn('copy', 'メッセージをコピー', () => copyText(text), 'xs'), time()));
    }
    case 'reasoning': {
      const text = pstr(p, 'text', 'content', 'summary') || sum || textOf(p);
      const d = h('details', { class: 'reasoning', ...id }, h('summary', {}, iconEl('brain'), h('span', { text: '思考プロセス' }), h('small', { text: `${fmtNum(text.length)}文字` }), iconEl('chevron', 's-chev')));
      d.addEventListener('toggle', () => { if (d.open && !d.querySelector('pre')) d.append(h('pre', { text })); });
      return d;
    }
    case 'system_notice':
      return renderNotice(item);
    default:
      return h('details', { class: 'reasoning', ...id }, h('summary', {}, iconEl('info'), h('span', { text: kind }), h('small', { text: oneLine(sum, 80) }), iconEl('chevron', 's-chev')), h('pre', { text: pretty(p) }));
  }
}

function runFooter(run) {
  if (!run || !TERMINAL_RUN.has(run.status)) return null;
  const ok = run.status === 'completed';
  const runCost = Number(S.detail?.context?.run_costs?.[run.id]) || 0;
  const canRollback = S.detail?.checkpoints?.includes(run.id) && !isBusy(curThread()?.status);
  return h('div', { class: `run-foot ${run.status}`, dataset: { run: run.id } },
    h('span', { class: 'rf-line' },
      provDot(run.provider),
      h('span', { text: `${run.model}${run.effort ? ` · ${run.effort}` : ''}` }),
      run.duration_ms != null ? h('span', { text: fmtDur(run.duration_ms) }) : null,
      run.input_tokens || run.output_tokens ? h('span', { title: '入力 / 出力トークン', text: `↑${fmtTok(run.input_tokens)} ↓${fmtTok(run.output_tokens)}` }) : null,
      runCost > 0 ? h('span', { title: 'このターンの推定コスト', text: fmtUsd(runCost) }) : null,
      ok ? null : h('span', { class: 'rf-status', text: statusLabel(run.status) }),
      canRollback ? h('button', {
        class: 'btn xs rf-rollback', type: 'button', text: '元に戻す', title: 'このターン開始前の作業ツリー状態に戻す',
        onclick: async (e) => {
          const b = e.currentTarget;
          // two-step confirm (window.confirm is unavailable in some webviews)
          if (!b.dataset.armed) { b.dataset.armed = '1'; b.textContent = '本当に戻す'; b.classList.add('warn'); setTimeout(() => { if (!b.isConnected) return; delete b.dataset.armed; b.textContent = '元に戻す'; b.classList.remove('warn'); }, 3000); return; }
          b.disabled = true;
          try {
            await api(`/api/threads/${S.threadId}/runs/${run.id}/rollback`, { method: 'POST' });
            toast('ターン開始前の状態に戻しました', 'ok');
            loadDetailSoon(); refreshDiffSoon();
          } catch (err) { toast(`元に戻せません: ${err.message}`, 'err'); b.disabled = false; }
        },
      }) : null),
    run.status === 'failed' && run.error_message ? h('div', { class: 'rf-error', text: run.error_message }) : null);
}

const runGroup = (runId) => h('div', { class: 'run-group', dataset: { run: runId || '' } });

// Incremental re-render bookkeeping: the item ids of the last renderTimeline
// pass, so a plain refresh can append only the new tail instead of rebuilding
// the DOM (keeps .run-group nodes, open folds, text selection and scroll).
let timelineSig = null; // { threadId, firstId, ids, lastRunId, allFolded, count, totalItems, hasMore, handoffs }

function timelineSignature(d) {
  const items = d.items;
  const runs = new Map((d.runs || []).map((r) => [r.id, r]));
  let allFolded = true;
  for (let i = 0; i < items.length;) {
    const runId = items[i].run_id;
    let j = i + 1;
    while (j < items.length && items[j].run_id === runId) j++;
    if (!isRunDone(runs.get(runId), j < items.length || !isBusy(d.thread.status))) allFolded = false;
    i = j;
  }
  return {
    threadId: d.thread.id,
    firstId: items[0]?.id || null,
    ids: items.map((it) => it.id),
    lastRunId: items.at(-1)?.run_id || null,
    allFolded,
    count: items.length,
    totalItems: d.total_items,
    hasMore: d.has_more,
    handoffs: (d.handoffs || []).length,
  };
}

/** Appends only the new tail when the refresh is a plain append; false means "do the full render". */
function tryIncrementalTimeline(root, d) {
  const sig = timelineSig;
  const items = d.items;
  if (!sig || sig.threadId !== d.thread.id || root.dataset.thread !== d.thread.id) return false;
  if (!sig.count) return false; // the previous render was the blank state (or a footer-only thread): rebuild it fully
  if (items.length < sig.count || !sig.allFolded) return false;
  if (sig.count && items[0]?.id !== sig.firstId) return false;
  for (let i = 0; i < sig.count; i++) if (items[i].id !== sig.ids[i]) return false; // previously rendered items must be an exact prefix
  if (d.has_more !== sig.hasMore || d.total_items !== sig.totalItems) return false;
  if ((d.handoffs || []).length !== sig.handoffs) return false;
  if (S.optimistic.some((o) => o.threadId === d.thread.id)) return false; // optimistic rows sit between groups and #live
  const live = $('#live', root);
  if (!live) return false;
  const groups = $$('.run-group', root);
  if (sig.count && (!groups.length || groups.at(-1).dataset.run !== sig.lastRunId)) return false;
  const fresh = items.slice(sig.count);
  if (fresh.length && sig.count && fresh[0].run_id === sig.lastRunId) return false; // new items continue the last run: needs a full re-render
  const strayFoot = root.querySelector(':scope > .run-foot');
  if (strayFoot && fresh.some((it) => it.run_id === strayFoot.dataset.run)) return false; // a run that already has a standalone footer gained items
  const runs = new Map((d.runs || []).map((r) => [r.id, r]));
  const anchor = strayFoot || live;
  for (let i = 0; i < fresh.length;) {
    const runId = fresh[i].run_id;
    let j = i + 1;
    while (j < fresh.length && fresh[j].run_id === runId) j++;
    const seg = fresh.slice(i, j);
    const run = runs.get(runId);
    const group = runGroup(runId);
    if (isRunDone(run, sig.count + j < items.length || !isBusy(d.thread.status))) appendFinished(seg, group, run?.duration_ms);
    else seg.forEach((it) => appendItem(it, group));
    const foot = runFooter(run);
    if (foot) group.append(foot);
    if (group.childElementCount) anchor.before(group);
    i = j;
  }
  // a run with no items yet only gets its footer once it is terminal
  const lr = latestRun();
  if (lr && !items.some((it) => it.run_id === lr.id)) {
    const foot = runFooter(lr);
    const old = root.querySelector(':scope > .run-foot[data-run="' + CSS.escape(lr.id) + '"]');
    if (foot && !old) live.before(foot);
    else if (foot && old && old.className !== foot.className) old.replaceWith(foot);
  }
  root.dataset.thread = d.thread.id;
  root.dataset.threadId = d.thread.id;
  timelineSig = timelineSignature(d);
  renderLive();
  scrollBottom();
  return true;
}

function renderTimeline() {
  const root = tl();
  const d = S.detail;
  if (!d) { if (!S.threadId) root.replaceChildren(); callRows.clear(); foldIndex.clear(); timelineSig = null; return; }
  if (tryIncrementalTimeline(root, d)) return;
  // keep what the reader had expanded (and their scroll position) across refreshes
  const same = root.dataset.thread === d.thread.id;
  keepOpen = new Set(same ? $$('details[open][data-item]', root).map((e) => e.dataset.item) : []);
  if (same) $$('details.work-fold[open]', root).forEach((e) => keepOpen.add(e.dataset.last));
  const expanded = new Set(same ? $$('.steps.expanded', root).map((e) => e.dataset.run) : []);
  const top = scroller().scrollTop;
  root.dataset.thread = d.thread.id;
  root.dataset.threadId = d.thread.id;
  callRows.clear(); foldIndex.clear();
  const frag = document.createDocumentFragment();
  if (d.has_more) {
    const left = d.total_items - d.items.length;
    frag.append(h('button', { type: 'button', id: 'load-older', class: 'load-older', disabled: S.loadingOlder === d.thread.id, text: S.loadingOlder === d.thread.id ? '読み込み中…' : `以前の履歴を読み込む${left > 0 ? ` (残り ${fmtNum(left)} 件)` : ''}`, onclick: () => loadOlder() }));
  }
  // incoming handoff (this thread was forked from another)
  for (const ho of d.has_more ? [] : (d.handoffs || []).filter((x) => x.target_thread_id === d.thread.id)) {
    const src = threadById(ho.source_thread_id);
    frag.append(h('details', { class: 'handoff-in', dataset: { item: ho.id } }, h('summary', {},
      iconEl('handoff'),
      h('span', { class: 'k', text: '引き継ぎ元' }),
      h('span', { class: 'v', text: `${src ? cleanTitle(src.title) : ho.source_thread_id.slice(0, 8)}（${provLabel(ho.from_model?.provider)} ${ho.from_model?.model} → ${provLabel(ho.to_model?.provider)} ${ho.to_model?.model}）` }),
      h('button', { class: 'btn xs', type: 'button', text: '元を開く', onclick: (e) => { e.preventDefault(); openThread(ho.source_thread_id); } }),
      iconEl('chevron', 's-chev')),
    mdEl(ho.summary || '')));
  }
  const items = d.items;
  const runs = new Map((d.runs || []).map((r) => [r.id, r]));
  for (let i = 0; i < items.length;) {
    const runId = items[i].run_id;
    let j = i + 1;
    while (j < items.length && items[j].run_id === runId) j++;
    const seg = items.slice(i, j);
    const run = runs.get(runId);
    const group = runGroup(runId);
    if (isRunDone(run, j < items.length || !isBusy(d.thread.status))) appendFinished(seg, group, run?.duration_ms);
    else seg.forEach((it) => appendItem(it, group));
    const foot = runFooter(run);
    if (foot) group.append(foot);
    if (group.childElementCount) frag.append(group);
    i = j;
  }
  const lr = latestRun();
  if (lr && !items.some((it) => it.run_id === lr.id)) { const f = runFooter(lr); if (f) frag.append(f); }
  for (const o of S.optimistic) if (o.threadId === d.thread.id) frag.append(optimisticEl(o));
  frag.append(h('div', { id: 'live' }));
  if (!items.length && !isBusy(d.thread.status)) frag.prepend(h('div', { class: 'blank' }, h('p', { text: 'まだメッセージはありません。下の入力欄から始めてください。' })));
  $$('.steps', frag).forEach((g) => { if (!g.closest('.work-fold') && expanded.has(g.dataset.run)) { g.classList.add('expanded'); labelSteps(g); } });
  $$('details[data-item]', frag).forEach((e) => { if (keepOpen.has(e.dataset.item)) e.open = true; });
  root.replaceChildren(frag);
  timelineSig = timelineSignature(d);
  if (same && !S.stick) scroller().scrollTop = top;
  renderLive();
  scrollBottom();
}

/** Re-renders after older items were prepended, holding the first visible message where it was. */
function renderKeepingAnchor() {
  const sc = scroller();
  const edge = sc.getBoundingClientRect().top;
  const anchor = $$('#timeline .msg[data-item]').find((e) => e.getBoundingClientRect().bottom > edge);
  const was = anchor?.getBoundingClientRect().top;
  S.stick = false;
  renderTimeline();
  if (!anchor) return;
  const fix = () => {
    const now = $(`#timeline .msg[data-item="${CSS.escape(anchor.dataset.item)}"]`);
    if (now) sc.scrollTop += now.getBoundingClientRect().top - was;
  };
  fix(); requestAnimationFrame(() => { fix(); requestAnimationFrame(fix); });
}

function optimisticEl(o) { return h('div', { class: 'msg user pending', dataset: { opt: o.key } }, userBubble(o.text, `opt:${o.key}`)); }

// Live Markdown repaint throttle: last md() render time + pending trailing timer.
let liveMdLast = 0;
let liveMdTimer = 0;

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
    const text = live.assistant;
    if (asst.dataset.len !== String(text.length)) {
      // Long replies re-parse Markdown on every frame; throttle those repaints.
      const now = performance.now();
      if (text.length > 4000 && now - liveMdLast < 120) {
        if (!liveMdTimer) liveMdTimer = setTimeout(() => { liveMdTimer = 0; renderLive(); }, 120 - (now - liveMdLast));
      } else {
        liveMdLast = now;
        asst.firstChild.innerHTML = md(text);
        asst.dataset.len = String(text.length);
      }
    }
  } else asst?.remove();
  if (isBusy(t.status)) {
    if (!work) { work = h('div', { class: 'working' }, h('span', { class: 'dots' }, h('i'), h('i'), h('i')), h('span', { class: 'w-label' }), h('span', { class: 'w-time' })); host.append(work); }
    $('.w-label', work).textContent = t.status === 'rate-limited' ? '制限中 — 自動再開を待っています' : t.status === 'waiting-for-children' ? '子スレッドの完了を待っています' : '作業中';
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

async function jumpToItem(itemId) {
  const threadId = S.threadId;
  // the hit may be on an older page
  while (S.threadId === threadId && S.detail?.has_more && !S.detail.items.some((x) => x.id === itemId)) {
    if (!await loadOlder(1000)) break;
  }
  if (S.threadId !== threadId) return;
  const fold = foldIndex.get(itemId);
  if (fold) { fold.build(); fold.open = true; }
  const el = $(`#timeline [data-item="${CSS.escape(itemId)}"], #timeline [data-also="${CSS.escape(itemId)}"]`);
  if (!el) return;
  const g = el.closest('.steps');
  if (g) { g.classList.add('expanded'); labelSteps(g); }
  if (el.tagName === 'DETAILS') el.open = true;
  S.stick = false;
  el.scrollIntoView({ block: 'center' });
  requestAnimationFrame(() => el.scrollIntoView({ block: 'center' })); // sizes settle once skipped content is rendered
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
  ta().placeholder = isNarrow() ? (busy ? '追加の指示…' : t ? '追加の指示（/ コマンド · ! シェル）' : '依頼内容（/ コマンド · ! シェル）')
    : busy ? (S.behavior === 'steer' ? '実行中のターンに指示を割り込ませる（/ コマンド · ! シェル）…' : '次のターンに予約する指示（/ コマンド · ! シェル）…')
      : t ? '追加の指示を入力（/ コマンド · ! シェル実行 · Shift+Enter で改行）' : 'やりたいことを入力（/ コマンド · ! シェル実行 · Enter で送信）';
  const send = $('#btn-send');
  const shellOn = isShellModeInput(ta().value);
  const label = shellOn ? 'シェルコマンドを実行' : !busy ? '送信' : S.behavior === 'steer' ? '割り込み送信 (Steer)' : '予約送信 (Queue)';
  send.title = label; send.setAttribute('aria-label', label);
  send.disabled = sending || (!ta().value.trim() && !S.atts.length);
  $('#btn-stop').disabled = !t || !busy;
  syncComposerHints();
  syncLimitAlert();
  renderContextMeters();
  renderQueueStrip();
}
const syncSendEnabled = () => {
  $('#btn-send').disabled = sending || (!ta().value.trim() && !S.atts.length);
  syncComposerHints();
  updateCommandPalette();
};

/** Queued prompts for the current thread, with per-item cancel. Lives next to #swarm-hint. */
function renderQueueStrip() {
  const hint = $('#swarm-hint');
  if (!hint) return;
  let strip = $('#queue-strip');
  if (!strip) { strip = h('div', { id: 'queue-strip', class: 'queue-strip', role: 'status' }); hint.after(strip); }
  const queue = S.detail?.queue || [];
  if (!queue.length) { strip.replaceChildren(); return; }
  strip.replaceChildren(
    h('span', { class: 'q-label', text: `予約中 (${queue.length})` }),
    ...queue.map((q) => h('span', { class: 'q-item' },
      h('span', { class: 'q-text', text: oneLine(q.prompt, 70) }),
      h('button', {
        class: 'q-cancel', type: 'button', text: '×', title: 'この予約をキャンセル', 'aria-label': '予約をキャンセル',
        onclick: async (e) => {
          const b = e.currentTarget; b.disabled = true;
          try {
            await api(`/api/threads/${S.threadId}/queue/${q.id}`, { method: 'DELETE' });
            loadDetailSoon();
            toast('予約をキャンセルしました');
          } catch (err) { toast(`予約をキャンセルできません: ${err.message}`, 'err'); b.disabled = false; }
        },
      }))),
  );
}

const SWARM_COMMAND = /^\s*\/swarm(\s|$)/i;
const isShellModeInput = (val) => !!parseShellInput(val) || /^\s*\/(sh|shell|bash|run)(\s|$)/i.test(val || '');
/** Pill above the textarea for /swarm or ! shell command mode. Touches the DOM only on a change. */
function syncComposerHints() {
  const box = $('#swarm-hint');
  const val = ta().value;
  const shellOn = isShellModeInput(val);
  const swarmOn = !shellOn && SWARM_COMMAND.test(val);
  $('#composer').classList.toggle('shell-mode', shellOn);
  const sendBtn = $('#btn-send');
  if (shellOn) {
    sendBtn.title = 'シェルコマンドを実行';
    sendBtn.setAttribute('aria-label', 'シェルコマンドを実行');
  }
  const t = curThread();
  const proj = projectById(t?.project_id || S.projectId);
  const where = t?.worktree_path ? compactPath(t.worktree_path) : proj?.title || '作業ディレクトリ';
  const key = shellOn ? `shell:${where}` : swarmOn ? 'swarm' : '';
  if ((box.dataset.key || '') === key) return;
  box.dataset.key = key;
  if (shellOn) {
    box.replaceChildren(h('span', { class: 'shell-pill' }, iconEl('terminal'), h('span', { text: `シェル実行 · ${where} · Enter で実行` })));
  } else if (swarmOn) {
    box.replaceChildren(h('span', { class: 'swarm-pill' }, iconEl('swarm'), h('span', { text: 'Swarm モード · マルチエージェント協調ボードを起動' })));
  } else {
    box.replaceChildren();
  }
}
function insertSwarmCommand() {
  const input = ta();
  if (!SWARM_COMMAND.test(input.value)) input.value = `/swarm ${input.value}`;
  else if (input.value.trim() === '/swarm') input.value = '/swarm ';
  cmdDismissed = false;
  autosize(); syncSendEnabled();
  input.focus();
  input.setSelectionRange(input.value.length, input.value.length);
}

// ───────────────────────── slash & shell command palette ─────────────────────────
let cmdMenu = null; // { mode, items, index }
let cmdDismissed = false;

function hideCommandPalette() {
  cmdMenu = null;
  const el = $('#cmd-palette');
  if (el) { el.hidden = true; el.replaceChildren(); }
  ta()?.setAttribute('aria-expanded', 'false');
}

function commandPaletteContext() {
  const models = [];
  for (const p of S.providers) {
    const providerLabel = provLabel(p.provider);
    for (const m of p.models || []) models.push({ provider: p.provider, providerLabel, id: m.id, label: m.label || m.id });
  }
  const meta = workspaceMetaFor(curThread()?.project_id || S.projectId);
  return {
    provider: S.compose.sel?.provider || curThread()?.model_selection?.provider,
    hasThread: !!curThread(),
    isBusy: isBusy(curThread()?.status),
    effortLevels: effortLevels(S.compose.sel),
    models,
    themes: THEMES,
    shellHistory: S.shellHistory,
    workspaceCommands: meta.commands,
    workspaceFiles: meta.files,
  };
}

function updateCommandPalette() {
  if (cmdDismissed || sending) { hideCommandPalette(); return; }
  const res = getPaletteSuggestions(ta().value, commandPaletteContext());
  if (!res || !res.items.length) { hideCommandPalette(); return; }
  // Shell history starts unselected (-1) so Enter always runs what the user typed unless they navigate with arrows.
  const defIdx = res.mode === 'shell' ? -1 : 0;
  const prevIdx = cmdMenu && cmdMenu.mode === res.mode
    ? (cmdMenu.index < 0 ? defIdx : Math.min(cmdMenu.index, res.items.length - 1))
    : defIdx;
  cmdMenu = { ...res, index: prevIdx };
  renderCommandPalette();
}

function renderCommandPalette() {
  const box = $('#cmd-palette');
  if (!box || !cmdMenu || !cmdMenu.items.length) { hideCommandPalette(); return; }
  const headerText = cmdMenu.mode === 'shell' ? '最近のシェルコマンド（↑↓ 選択 · Tab 補完 · Enter 実行）'
    : cmdMenu.mode === 'file' ? 'ファイル参照（↑↓ 選択 · Tab 補完 · Enter 補完）'
    : cmdMenu.mode === 'arg' ? `/${cmdMenu.command.name} の引数候補（↑↓ 選択 · Tab 補完 · Enter 実行）`
      : 'スラッシュコマンド（↑↓ 選択 · Tab 補完 · Enter 実行 · Esc 閉じる）';
  const list = h('div', { class: 'cmd-list' }, cmdMenu.items.map((item, i) => {
    const sel = i === cmdMenu.index;
    return h('button', {
      type: 'button',
      role: 'option',
      class: `cmd-opt${sel ? ' sel' : ''}`,
      'aria-selected': String(sel),
      onmousedown: (e) => { e.preventDefault(); },
      onmousemove: () => { if (cmdMenu && cmdMenu.index !== i) moveCommandSelection(i); },
      onclick: () => applyCommandChoice(item, { execute: item.execute }),
    },
    item.provider ? provDot(item.provider) : iconEl(item.icon || 'terminal', 'cmd-ic'),
    h('span', { class: 'cmd-name' }, h('b', { text: item.label }), item.args ? h('em', { text: item.args }) : null),
    h('span', { class: 'cmd-desc', text: item.desc }),
    item.fuzzy ? h('span', { class: 'cmd-tag warn', text: '候補' })
      : item.group ? h('span', { class: 'cmd-tag', text: item.provider ? provLabel(item.provider) : item.group }) : null);
  }));
  box.replaceChildren(h('div', { class: 'cmd-head', text: headerText }), list);
  box.hidden = false;
  ta().setAttribute('aria-expanded', 'true');
  $('.cmd-opt.sel', box)?.scrollIntoView({ block: 'nearest' });
}

function moveCommandSelection(nextIdx) {
  if (!cmdMenu || !cmdMenu.items.length) return;
  const n = cmdMenu.items.length;
  cmdMenu.index = ((nextIdx % n) + n) % n;
  const opts = $$('#cmd-palette .cmd-opt');
  opts.forEach((el, i) => {
    const on = i === cmdMenu.index;
    el.classList.toggle('sel', on);
    el.setAttribute('aria-selected', String(on));
    if (on) el.scrollIntoView({ block: 'nearest' });
  });
}

function applyCommandChoice(item, { execute = false } = {}) {
  if (!item) return;
  const input = ta();
  if (item.kind === 'file') {
    // Replace only the trailing `@<query>` token; keep the rest of the input and keep typing (never sends).
    const m = /@([^\s@]{0,80})$/.exec(input.value);
    input.value = m ? input.value.slice(0, input.value.length - m[0].length) + item.insert : `${input.value}${item.insert}`;
    cmdDismissed = false;
    hideCommandPalette();
    autosize();
    syncSendEnabled();
    input.focus();
    input.setSelectionRange(input.value.length, input.value.length);
    return;
  }
  if (execute) {
    input.value = item.insert.trim();
    hideCommandPalette();
    autosize();
    send();
    return;
  }
  input.value = item.insert;
  cmdDismissed = false;
  autosize();
  syncSendEnabled();
  input.focus();
  input.setSelectionRange(input.value.length, input.value.length);
}

async function runShellCommand(command) {
  const cmd = String(command || '').trim();
  if (!cmd) { toast('実行するコマンドを入力してください（例: !git status）', 'warn'); return; }
  const t = curThread();
  const project = projectById(t?.project_id || S.projectId);
  if (!t && !project) { toast('先にプロジェクトを追加してください', 'err'); projectDialog(); return; }
  S.shellHistory = pushShellHistory(ls, cmd);
  const input = ta();
  input.value = '';
  hideCommandPalette();
  autosize();
  syncSendEnabled();
  // コンポーザはロックしない: 実行中も別メッセージ送信や ! 連続実行ができる。
  const label = oneLine(cmd, 56);
  const streamPath = t ? `/api/threads/${t.id}/exec/stream` : `/api/projects/${project.id}/exec/stream`;
  let res;
  try {
    res = await api(streamPath, { method: 'POST', json: { command: cmd } });
  } catch (e) {
    if (e.status === 404 || e.status === 405) { await runShellCommandSync(t, project, cmd, label); return; }
    input.value = `!${cmd}`;
    autosize();
    toast(`コマンド実行に失敗: ${e.message}`, 'err', 6000);
    return;
  }
  const execId = res?.exec_id;
  if (!execId) { await runShellCommandSync(t, project, cmd, label); return; }
  shellStartCard({ execId, threadId: t?.id || null, command: res.command || cmd, cwd: res.cwd || '' });
  if (!isCoarse() && !$('dialog[open]')) ta().focus();
}

/** 旧サーバー向けの同期 /exec フォールバック（/exec/stream が 404/405 の場合のみ）。 */
async function runShellCommandSync(t, project, cmd, label) {
  try {
    if (t) {
      const res = await api(`/api/threads/${t.id}/exec`, { method: 'POST', json: { command: cmd } });
      loadDetailSoon();
      refreshDiffSoon();
      if (res.timed_out) toast(`タイムアウトしました: $ ${label}`, 'warn', 5000);
      else if (res.exit_code !== 0) toast(`exit ${res.exit_code}: $ ${label}`, 'warn', 4000);
      else toast(`完了 (${fmtDur(res.duration_ms)}): $ ${label}`, 'ok', 2200);
    } else {
      const res = await api(`/api/projects/${project.id}/exec`, { method: 'POST', json: { command: cmd } });
      shellResultDialog(res);
    }
  } catch (e) {
    const input = ta();
    input.value = `!${cmd}`;
    autosize();
    toast(`コマンド実行に失敗: ${e.message}`, 'err', 6000);
  }
}

// ── ライブシェル出力カード（Tier 4 Item 3） ──
const liveShells = new Map(); // exec_id -> { execId, threadId, command, el, out, status, stopBtn, chars }
const SHELL_LIVE_MAX_CHARS = 200000;

function shellStartCard({ execId, threadId, command, cwd }) {
  const out = h('pre', { class: 'shell-live-out', 'aria-live': 'polite' });
  const status = h('span', { class: 'shell-live-status' }, h('span', { class: 'pulse', 'aria-hidden': 'true' }), h('span', { text: '実行中' }));
  const stopBtn = h('button', {
    type: 'button', class: 'btn xs danger-ghost', text: '停止', title: 'このシェル実行を停止する',
    onclick: async () => {
      stopBtn.disabled = true;
      try { await api(`/api/exec/${encodeURIComponent(execId)}/cancel`, { method: 'POST' }); }
      catch (e) { toast(`停止に失敗: ${e.message}`, 'err'); stopBtn.disabled = false; }
    },
  });
  const card = h('div', { class: 'shell-live', dataset: { exec: execId } },
    h('div', { class: 'shell-live-head' }, iconEl('terminal'), h('code', { class: 'shell-live-cmd', text: `$ ${command}` }), h('span', { class: 'grow' }), status, stopBtn),
    cwd ? h('div', { class: 'muted mono shell-live-cwd', text: cwd }) : null,
    out);
  const live = $('#live');
  if (threadId && live?.isConnected) live.before(card);
  else $('#approval')?.after(card);
  liveShells.set(execId, { execId, threadId, command, el: card, out, status, stopBtn, chars: 0 });
  scrollBottom();
}

function shellAppendChunk(execId, stream, chunk) {
  const s = liveShells.get(execId);
  if (!s || !s.el.isConnected) return;
  const text = String(chunk ?? '');
  if (!text) return;
  s.out.append(h('span', { class: stream === 'stderr' ? 'sh-err' : 'sh-out', text }));
  s.chars += text.length;
  while (s.chars > SHELL_LIVE_MAX_CHARS && s.out.firstChild) {
    s.chars -= (s.out.firstChild.textContent || '').length;
    s.out.firstChild.remove();
  }
  if (S.stick) scrollBottom();
}

function shellFinish(m) {
  const s = liveShells.get(m.exec_id);
  if (!s) return;
  liveShells.delete(m.exec_id);
  const label = oneLine(s.command, 56);
  // スレッド実行はサーバーが CommandExecution を永続化して turn_item_added を配信する:
  // ライブカードを片付けて二重表示を防ぐ（item_id が無いエラー時のみカードに最終結果を残す）。
  if (m.thread_id != null && m.item_id) {
    s.el.remove();
    if (m.thread_id === S.threadId) loadDetailSoon();
    return;
  }
  const bad = m.timed_out || m.cancelled || (m.exit_code != null && m.exit_code !== 0);
  if (m.thread_id != null && !m.item_id && bad) {
    toast(m.timed_out ? `タイムアウトしました: $ ${label}` : m.cancelled ? `キャンセルしました: $ ${label}` : `exit ${m.exit_code}: $ ${label}`, 'warn', 5000);
  }
  s.el.classList.add('done');
  s.status.replaceChildren(
    m.timed_out ? badge('timeout', 'bad') : badge(`exit ${m.exit_code ?? '?'}`, bad ? 'bad' : 'good'),
    badge(fmtDur(m.duration_ms)),
    m.truncated ? badge('一部省略', 'warn') : null,
    m.cancelled ? badge('キャンセル済み', 'warn') : null,
  );
  s.stopBtn.replaceWith(h('button', {
    type: 'button', class: 'btn xs', text: '閉じる',
    onclick: () => s.el.remove(),
  }));
  if (S.stick) scrollBottom();
}

function shellResultDialog(res) {
  const out = [res.stdout, res.stderr].filter(Boolean).join(res.stdout && !res.stdout.endsWith('\n') ? '\n' : '');
  const bad = res.timed_out || (res.exit_code != null && res.exit_code !== 0);
  openModal('シェル実行結果', (body, close) => {
    const badges = [
      res.timed_out ? badge('timeout', 'bad') : badge(`exit ${res.exit_code ?? '?'}`, bad ? 'bad' : 'good'),
      badge(fmtDur(res.duration_ms)),
      res.truncated ? badge('一部省略', 'warn') : null,
    ].filter(Boolean);
    body.append(
      h('div', { class: 'shell-res' },
        h('div', { class: 'shell-res-meta' },
          h('code', { class: 'shell-res-cmd', text: `$ ${res.command}` }),
          h('div', { class: 'shell-res-badges' }, badges)),
        h('div', { class: 'muted mono shell-res-cwd', text: res.cwd }),
        out ? outputBlock(out) : h('div', { class: 'muted', text: '出力はありません' }),
        h('div', { class: 'actions' },
          h('button', { type: 'button', class: 'btn sm', text: '出力をコピー', onclick: () => copyText(out || `$ ${res.command}`, 'コピーしました') }),
          h('button', {
            type: 'button', class: 'btn sm primary', text: 'この出力を AI に渡す',
            onclick: () => {
              close();
              const snippet = `コマンド実行結果:\n\`\`\`sh\n$ ${res.command}\n${(out || '(出力なし)').slice(0, 6000)}\n\`\`\`\n\n`;
              ta().value = snippet;
              autosize(); syncSendEnabled(); ta().focus();
            },
          }))));
  }, { cls: 'wide' });
}

function commandHelpDialog() {
  const provider = S.compose.sel?.provider || curThread()?.model_selection?.provider;
  const cmds = allCommands(provider);
  const groups = new Map();
  for (const c of cmds) {
    const label = c.provider ? `${provLabel(c.provider)} コマンド` : (GROUP_LABEL[c.group] || c.group);
    if (!groups.has(label)) groups.set(label, []);
    groups.get(label).push(c);
  }
  openModal('コマンド一覧（入力欄）', (body, close) => {
    const runOrInsert = (c) => {
      close();
      applyCommandChoice({
        insert: c.needsArg || c.args ? `/${c.name} ` : `/${c.name}`,
        execute: !c.needsArg && !c.args && (!c.requiresThread || !!curThread()),
      }, { execute: !c.needsArg && !c.args && (!c.requiresThread || !!curThread()) });
    };
    body.append(
      h('div', { class: 'help-banner' },
        iconEl('terminal'),
        h('div', {},
          h('b', { text: '直接シェル実行: !<コマンド>' }),
          h('p', { class: 'muted', text: '先頭に ! を付けて入力すると（例: !git status / !cargo test）、現在の作業ディレクトリで直接実行し、結果をタイムラインに記録します。' }))),
      ...[...groups.entries()].map(([title, list]) => h('section', { class: 'help-sec' },
        h('h3', { class: 'sec-h', text: title }),
        h('div', { class: 'help-cmds' }, list.map((c) => h('button', {
          type: 'button', class: 'help-row',
          onclick: () => runOrInsert(c),
        },
        h('code', { class: 'help-cmd', text: `/${c.name}${c.args ? ` ${c.args}` : ''}` }),
        h('span', { class: 'help-desc', text: c.desc })))))));
  }, { cls: 'wide' });
}

/**
 * Executes built-in local slash commands.
 * Returns `{ handled: true }` if consumed locally, or `{ handled: false, overrideText? }` to send to the provider CLI.
 */
async function executeSlashCommand(parsed) {
  const provider = S.compose.sel?.provider || curThread()?.model_selection?.provider;
  const cmd = findCommand(parsed.name, provider);
  if (!cmd || cmd.sendToProvider) return { handled: false };

  const t = curThread();
  const arg = parsed.arg;
  const clearInput = () => { ta().value = ''; hideCommandPalette(); autosize(); syncSendEnabled(); };

  if (cmd.requiresThread && !t) {
    clearInput();
    toast(`/${cmd.name} はスレッドを開いた状態で実行してください`, 'warn');
    return { handled: true };
  }

  switch (cmd.name) {
    case 'help':
      clearInput();
      commandHelpDialog();
      return { handled: true };
    case 'new':
      clearInput();
      goHome(S.projectId);
      toast('新規スレッドを開きました', 'ok', 1800);
      return { handled: true };
    case 'sh':
      if (!arg) {
        ta().value = '/sh ';
        autosize(); syncSendEnabled(); ta().focus();
        toast('実行するシェルコマンドを入力してください（例: /sh git status）', 'info');
        return { handled: true };
      }
      await runShellCommand(arg);
      return { handled: true };
    case 'compact': {
      // Claude Code and Antigravity support native in-place /compact on the active CLI session.
      const p = t.model_selection?.provider;
      if (p === 'antigravity' || p === 'claude') {
        return { handled: false, overrideText: arg ? `/compact ${arg}` : '/compact' };
      }
      clearInput();
      sending = true; syncSendEnabled();
      try {
        if (p === 'codex') {
          // `codex exec resume` would send "/compact" to the model as plain text, so the server
          // compacts the session through the app-server instead. The thread stays the same.
          // thread/compact/start takes no instruction, so a custom [方針] is not used.
          await api(`/api/threads/${t.id}/compact`, { method: 'POST' });
          toast('会話を圧縮しました', 'ok');
          await loadDetail(t.id);
          return { handled: true };
        }
        // OpenCode / Mock: compact by forking with an automatic handoff summary.
        const req = {
          title: `${cleanTitle(t.title)} (圧縮)`,
          target_model: S.compose.sel || t.model_selection,
        };
        if (arg) req.handoff_summary = arg;
        const r = await api(`/api/threads/${t.id}/fork`, { method: 'POST', json: req });
        const nt = r.thread || r;
        toast('会話を要約して新しいスレッドに圧縮しました', 'ok');
        await loadThreads();
        await openThread(nt.id);
      } catch (e) {
        toast(`圧縮に失敗: ${e.message}`, 'err');
      } finally {
        sending = false; renderComposer();
      }
      return { handled: true };
    }
    case 'model': {
      clearInput();
      if (!arg) { openModelPopover($('#btn-model')); return { handled: true }; }
      const q = arg.toLowerCase();
      const all = [];
      for (const p of S.providers) {
        for (const m of p.models || []) all.push({ provider: p.provider, id: m.id, label: m.label || m.id });
      }
      const exact = all.find((m) => `${m.provider}/${m.id}`.toLowerCase() === q || m.id.toLowerCase() === q || m.label.toLowerCase() === q);
      const matches = exact ? [exact] : all.filter((m) => `${m.provider}/${m.id} ${m.label}`.toLowerCase().includes(q));
      if (matches.length === 1) {
        const m = matches[0];
        setComposeSel(withEffort({ provider: m.provider, model: m.id }, S.compose.sel?.effort));
        toast(`モデルを ${m.label} (${provLabel(m.provider)}) に変更しました`, 'ok', 2200);
      } else {
        openModelPopover($('#btn-model'), arg);
      }
      return { handled: true };
    }
    case 'effort': {
      clearInput();
      const levels = effortLevels(S.compose.sel);
      if (!levels.length) { toast('現在のモデルでは Effort を変更できません', 'warn'); return { handled: true }; }
      const v = arg.toLowerCase();
      if (!v || !levels.includes(v)) {
        toast(`Effort を指定してください: ${levels.join(' / ')}（現在: ${S.compose.sel?.effort || 'default'}）`, 'info', 4000);
        return { handled: true };
      }
      setComposeSel({ ...S.compose.sel, effort: v });
      toast(`Effort を ${v} に変更しました`, 'ok', 2000);
      return { handled: true };
    }
    case 'plan': {
      clearInput();
      const cur = t ? t.interaction_mode === 'plan' : S.compose.plan;
      const v = arg.toLowerCase();
      const next = v === 'on' || v === 'true' || v === '1' ? true : v === 'off' || v === 'false' || v === '0' ? false : !cur;
      if (t) await patchPerm(t.id, { interaction_mode: next ? 'plan' : 'default' });
      else { S.compose.plan = next; renderComposer(); toast(`Plan モードを${next ? '有効' : '無効'}にしました`, 'ok', 2000); }
      return { handled: true };
    }
    case 'mode': {
      clearInput();
      if (!arg) { openPopover($('#btn-perm'), composerPerm(), { cls: 'perm-pop', align: 'start' }); return { handled: true }; }
      const map = {
        approval: 'approval-required', 'approval-required': 'approval-required', default: 'approval-required',
        auto: 'auto-accept-edits', edit: 'auto-accept-edits', edits: 'auto-accept-edits', 'auto-accept-edits': 'auto-accept-edits',
        full: 'full-access', 'full-access': 'full-access',
      };
      const next = map[arg.toLowerCase()];
      if (!next) { toast('権限モード: approval-required / auto-accept-edits / full-access', 'warn', 4000); return { handled: true }; }
      if (t) await patchPerm(t.id, { runtime_mode: next });
      else { S.compose.mode = next; renderComposer(); toast(`実行権限を「${MODE_LABEL[next]}」に変更しました`, 'ok', 2000); }
      return { handled: true };
    }
    case 'settings':
      clearInput();
      settingsDialog();
      return { handled: true };
    case 'diff': {
      clearInput();
      const v = arg.toLowerCase();
      if (v === 'latest' || v === 'all') {
        S.diffScope = v;
        ls.set('t3.diffScope', v);
        setRail('diff');
        loadDiff();
      } else {
        toggleDiff();
      }
      return { handled: true };
    }
    case 'context':
      clearInput();
      toggleContext();
      return { handled: true };
    case 'stop':
      clearInput();
      if (isBusy(t.status)) await interrupt();
      else toast('実行中のターンはありません', 'info', 2000);
      return { handled: true };
    case 'fork':
      clearInput();
      if (arg) {
        try {
          const r = await api(`/api/threads/${t.id}/fork`, { method: 'POST', json: { title: arg, target_model: S.compose.sel || t.model_selection } });
          const nt = r.thread || r;
          toast('分岐しました', 'ok');
          await loadThreads();
          await openThread(nt.id);
        } catch (e) { toast(`分岐に失敗: ${e.message}`, 'err'); }
      } else {
        forkDialog();
      }
      return { handled: true };
    case 'delegate':
      clearInput();
      delegateDialog(arg);
      return { handled: true };
    case 'title':
      if (!arg) {
        ta().value = '/title ';
        autosize(); syncSendEnabled(); ta().focus();
        toast('新しいタイトルを入力してください', 'info');
        return { handled: true };
      }
      clearInput();
      await patchThread(t.id, { title: arg });
      toast('タイトルを変更しました', 'ok', 2000);
      return { handled: true };
    case 'pin':
      clearInput();
      await patchThread(t.id, { pinned: !t.pinned });
      toast(t.pinned ? 'ピン留めを解除しました' : 'ピン留めしました', 'ok', 1800);
      return { handled: true };
    case 'archive':
      clearInput();
      await patchThread(t.id, { archived: !t.archived });
      toast(t.archived ? 'アーカイブを解除しました' : 'アーカイブしました', 'ok', 1800);
      return { handled: true };
    case 'export':
      clearInput();
      downloadThreadMarkdown(t);
      return { handled: true };
    case 'search':
      clearInput();
      searchDialog(arg);
      return { handled: true };
    case 'usage':
      clearInput();
      usageDialog();
      return { handled: true };
    case 'monitor':
      clearInput();
      monitorDialog();
      return { handled: true };
    case 'limits':
      clearInput();
      usageDialog({ metric: 'limits' });
      return { handled: true };
    case 'theme': {
      clearInput();
      if (!arg) { themeMenu($('#btn-theme')); return { handled: true }; }
      const q = arg.toLowerCase();
      const found = THEMES.find(([id, label]) => id.toLowerCase() === q || label.toLowerCase().includes(q));
      if (!found) { toast(`テーマ: ${THEMES.map(([id]) => id).join(' / ')}`, 'warn', 4000); return { handled: true }; }
      setTheme(found[0]);
      toast(`テーマを「${found[1]}」に変更しました`, 'ok', 2000);
      return { handled: true };
    }
    default:
      return { handled: false };
  }
}

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
  let text = input.value.trim();
  if (!text && !S.atts.length) return;
  hideCommandPalette();

  // Direct shell command: `!<command>`
  const shell = parseShellInput(text);
  if (shell) {
    await runShellCommand(shell.command);
    return;
  }

  // Local slash commands: `/compact`, `/new`, `/diff`, `/model`, `/sh`, `/help`, etc.
  const slash = parseSlashInput(text);
  if (slash) {
    const res = await executeSlashCommand(slash);
    if (res.handled) return;
    if (res.overrideText) text = res.overrideText;
  }

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
      atts.forEach((a) => { if (a.local) URL.revokeObjectURL(a.local); });
      loadDetailSoon();
      if (SWARM_COMMAND.test(text)) loadSwarm(t.id);
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

// ───────────────────────── マルチスレッド稼働モニター（Tier 4 Item 6） ─────────────────────────
let monitorOpen = false;
let monitorSeq = 0;
let monitorBody = null;
let monitorClose = null;
/** ダイアログが開いている間だけ WS イベントで ~300ms デバウンス再取得する。 */
const refreshMonitorSoon = debounce(() => { if (monitorOpen) loadMonitor(); }, 300);

function monitorDialog() {
  openModal('稼働モニター', (body, close) => {
    monitorOpen = true;
    monitorBody = body;
    monitorClose = close;
    body.replaceChildren(h('div', { class: 'dp-empty', text: '読み込み中…' }));
    loadMonitor();
  }, {
    cls: 'wide',
    onclose: () => { monitorOpen = false; monitorSeq++; monitorBody = null; monitorClose = null; },
  });
}

async function loadMonitor() {
  const body = monitorBody;
  if (!body || !monitorOpen) return;
  const my = ++monitorSeq;
  try {
    const d = await api('/api/monitor');
    if (my !== monitorSeq || !monitorOpen) return;
    renderMonitor(body, d);
  } catch (e) {
    if (my !== monitorSeq || !monitorOpen) return;
    body.replaceChildren(h('div', { class: 'dp-empty errtxt', text: `モニターを取得できません: ${e.message}` }),
      h('button', { type: 'button', class: 'btn xs', text: '再試行', onclick: () => loadMonitor() }));
  }
}

function renderMonitor(body, d) {
  const pending = Array.isArray(d.pending_requests) ? d.pending_requests : [];
  const runs = Array.isArray(d.active_runs) ? d.active_runs : [];
  const queued = Array.isArray(d.queued_thread_ids) ? d.queued_thread_ids : [];
  const shells = Number(d.active_shells) || 0;

  const pendingRows = pending.map((r) => {
    const t = threadById(r.thread_id);
    const p = r.payload;
    const summary = r.title || pstr(p, 'command', 'cmd', 'path', 'file_path', 'description') || r.kind || '承認リクエスト';
    const btns = [];
    const act = async (approved) => {
      btns.forEach((b) => { b.disabled = true; });
      try {
        await api(`/api/threads/${r.thread_id}/respond`, { method: 'POST', json: { request_id: r.id, approved, payload: {} } });
        dropPending(r.thread_id, r.id);
        toast(approved ? '承認しました' : '拒否しました', approved ? 'ok' : '');
        loadMonitor();
      } catch (e) { btns.forEach((b) => { b.disabled = false; }); toast(`応答に失敗: ${e.message}`, 'err'); }
    };
    const ok = h('button', { type: 'button', class: 'btn xs primary', text: '許可', onclick: () => act(true) });
    const no = h('button', { type: 'button', class: 'btn xs danger-ghost', text: '拒否', onclick: () => act(false) });
    btns.push(ok, no);
    return h('div', { class: 'mon-row' },
      h('div', { class: 'mon-main' },
        h('span', { class: 'mon-title', text: t ? cleanTitle(t.title) : (r.thread_id || '').slice(0, 8) }),
        h('span', { class: 'mon-sub', text: summary }),
        h('span', { class: 'sbadge warn', text: r.kind || '承認待ち' })),
      h('div', { class: 'mon-acts' }, no, ok));
  });

  const runRows = runs.map((r) => {
    const t = threadById(r.thread_id);
    const ms = r.model_selection || {};
    const model = `${provLabel(ms.provider)} / ${ms.model || ''}${ms.effort ? ` · ${ms.effort}` : ''}`;
    const pend = Number(r.pending_count) || 0;
    const stopBtn = h('button', {
      type: 'button', class: 'btn xs danger-ghost', text: '停止',
      onclick: async () => {
        stopBtn.disabled = true;
        try { await api(`/api/threads/${r.thread_id}/interrupt`, { method: 'POST' }); toast('停止を要求しました'); loadMonitor(); }
        catch (e) { toast(`停止に失敗: ${e.message}`, 'err'); stopBtn.disabled = false; }
      },
    });
    return h('div', { class: 'mon-row' },
      h('div', { class: 'mon-main' },
        h('span', { class: 'mon-title', text: t ? cleanTitle(t.title) : (r.title || (r.thread_id || '').slice(0, 8)) }),
        h('span', { class: 'mon-sub', text: model }),
        r.status === 'rate-limited' ? h('span', { class: 'flag warn', text: '制限中' })
          : r.status ? h('span', { class: 'sbadge', text: statusLabel(r.status) }) : null,
        pend ? h('span', { class: 'flag warn', text: `承認待ち ${pend} 件` }) : null),
      h('div', { class: 'mon-acts' },
        h('button', {
          type: 'button', class: 'btn xs', text: '開く',
          onclick: () => { monitorClose?.(); openThread(r.thread_id); },
        }),
        stopBtn));
  });

  const queuedRows = queued.map((id) => {
    const t = threadById(id);
    return h('div', { class: 'mon-row' },
      h('div', { class: 'mon-main' }, h('span', { class: 'mon-title', text: t ? cleanTitle(t.title) : id.slice(0, 8) })),
      h('div', { class: 'mon-acts' }, h('button', {
        type: 'button', class: 'btn xs', text: '開く',
        onclick: () => { monitorClose?.(); openThread(id); },
      })));
  });

  const section = (title, countBadge, rows, emptyText) => ctxSection(title,
    countBadge != null ? h('span', { class: 'sbadge', text: String(countBadge) }) : null,
    rows.length
      ? h('div', { class: 'ctx-card mon-card' }, rows)
      : h('div', { class: 'ctx-card ctx-muted-card', text: emptyText }));
  body.replaceChildren(
    h('div', { class: 'mon-head' },
      h('span', { class: 'sbadge', text: `実行中シェル ${shells} 件` }),
      h('span', { class: 'grow' }),
      h('button', { type: 'button', class: 'btn xs', text: '更新', onclick: () => loadMonitor() })),
    section('承認待ちリクエスト', pending.length || null, pendingRows, '承認待ちのリクエストはありません'),
    section('実行中スレッド', runs.length || null, runRows, '実行中のスレッドはありません'),
    section('キュー待ちスレッド', queued.length || null, queuedRows, 'キュー待ちのスレッドはありません'),
  );
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
    parts.push(h('span', { class: 'sp', title: 'このスレッドの Run 数', dataset: { utab: 'model' }, text: `${fmtNum(runs.length)} runs` }));
    const up = sum('input_tokens'); const down = sum('output_tokens');
    if (up || down) parts.push(h('span', { class: 'sp', title: 'このスレッドの入力 / 出力トークン', dataset: { utab: 'model' }, text: `↑${fmtTok(up)} ↓${fmtTok(down)}` }));
    const estCost = Number(S.detail?.context?.estimated_cost_usd) || 0;
    if (estCost > 0) parts.push(h('span', { class: 'sp', title: 'このスレッドの推定コスト', dataset: { utab: 'model' }, text: fmtUsd(estCost) }));
  } else if (S.dbStats) {
    parts.push(h('span', { class: 'sp', dataset: { utab: 'system' }, text: `${fmtNum(S.dbStats.threads_count)} スレッド` }), h('span', { class: 'sp', dataset: { utab: 'model' }, text: `${fmtNum(S.dbStats.runs_count)} runs` }));
  }
  if (S.meta) {
    const clean = S.meta.telemetry_enabled === false && S.meta.privacy?.telemetry_enabled !== true;
    parts.push(h('span', { class: `sp shield ${clean ? 'good' : 'bad'}`, dataset: { utab: 'system' } }, iconEl(clean ? 'shield' : 'alert'), clean ? 'ゼロテレメトリ' : 'テレメトリ有効'));
  }
  if (!parts.length) parts.push(h('span', { class: 'sp', text: '使用量' }));
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
          class: 'btn xs', type: 'button', text: '編集',
          onclick: () => projectEditDialog(p, renderList),
        }),
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
          S.showEmpty = true; // a new project has no threads yet, so show where it went
          await loadProjects();
          close(); goHome(p.id); toast('プロジェクトを追加しました', 'ok');
        } catch (err) { toast(`追加に失敗: ${err.message}`, 'err'); }
      },
    }, field('名前', title), field('ワークスペースのパス', root, 'サーバーから見える絶対パス（git リポジトリ推奨）。削除してもファイルは消えません。'),
    h('div', { class: 'actions' }, h('button', { class: 'btn primary', type: 'submit', text: '追加' }))), list);
    setTimeout(() => title.focus(), 0);
  });
}

/** Edit a project's title, default model, resident instructions and post-edit verify command (PATCH /api/projects/:id). */
function projectEditDialog(p, refreshList) {
  openModal('プロジェクト設定', (body, close) => {
    const title = h('input', { class: 'inp', value: p.title || '', 'aria-label': '名前' });
    let modelChanged = false;
    let clearModel = false;
    const picker = modelPanel({ value: p.default_model || defaultSelection(p.id), onChange: () => { modelChanged = true; } });
    const pickerWrap = h('div', { class: 'mp-embed' }, picker.el);
    const clearChk = h('input', {
      type: 'checkbox',
      onchange: (e) => { clearModel = e.target.checked; pickerWrap.classList.toggle('dim', clearModel); },
    });
    const instructions = h('textarea', { class: 'inp', rows: 3, placeholder: '初回ターンに自動付与するプロジェクト固有のルールや前提…', value: p.instructions || '', 'aria-label': '常駐指示' });
    const verify = h('input', { class: 'inp mono', placeholder: '例: cargo check / npm test（空欄で無効）', value: p.verify_command || '', autocapitalize: 'off', spellcheck: false, 'aria-label': '自動検証コマンド' });
    const go = h('button', { class: 'btn primary', type: 'submit', text: '保存' });
    body.append(h('form', {
      class: 'form',
      onsubmit: async (e) => {
        e.preventDefault(); if (e.submitter !== go) return;
        go.disabled = true;
        // Empty instructions / verify_command are sent as '' so the server clears them to null.
        const patch = { title: title.value.trim() || p.title, instructions: instructions.value.trim(), verify_command: verify.value.trim() };
        if (clearModel) patch.default_model = null;
        else if (modelChanged) patch.default_model = picker.get();
        try {
          await api(`/api/projects/${p.id}`, { method: 'PATCH', json: patch });
          await loadProjects();
          refreshList?.();
          close();
          toast('プロジェクト設定を保存しました', 'ok');
          if (p.id === S.projectId) { renderHero(); renderComposer(); }
        } catch (err) { go.disabled = false; toast(`保存に失敗: ${err.message}`, 'err'); }
      },
    },
    field('名前', title),
    field('既定モデル', h('div', { class: 'stack' },
      pickerWrap,
      h('label', { class: 'switch' }, clearChk, h('i'), h('span', { text: '既定モデルをクリア' })))),
    field('常駐指示', instructions, '初回ターンに自動で付与されます。空欄で無効。'),
    field('編集後の自動検証コマンド', verify, '編集ターン完了後に自動実行します。空欄で無効。'),
    h('div', { class: 'actions' }, go)));
    setTimeout(() => title.focus(), 0);
  }, { cls: 'form-modal' });
}

/** Runtime settings (GET/PATCH /api/settings). Opened from the sidebar and `/settings`. */
async function settingsDialog() {
  let data;
  try {
    data = await api('/api/settings');
  } catch (e) {
    toast(`設定を読み込めません: ${e.message}`, 'err');
    return;
  }
  const orig = data?.settings || data || {};
  const enabled = Array.isArray(data?.enabled_providers) && data.enabled_providers.length
    ? data.enabled_providers
    : (S.providers.length ? S.providers.map((p) => p.provider) : PROVIDER_FALLBACK);
  // Normalize once so the "changed?" comparison below is not fooled by null effort/agent.
  const normFallback = (m) => ({ provider: m.provider, model: m.model, ...(m.effort ? { effort: m.effort } : {}), ...(m.agent ? { agent: m.agent } : {}) });
  const origFallbacks = Array.isArray(orig.fallback_models) ? orig.fallback_models.map(normFallback) : [];
  const normNum = (v, fb) => { const n = Math.round(Number(v)); return Number.isFinite(n) ? Math.min(10, Math.max(0, n)) : fb; };
  const origBehavior = orig.follow_up_behavior === 'queue' ? 'queue' : 'steer';
  const origFixes = normNum(orig.verify_max_auto_fixes, 2);
  const origResume = orig.auto_resume_limited_threads !== false;
  const origRetries = Math.min(2, normNum(orig.rate_limit_max_retries, 2));
  const origMcp = orig.antigravity_mcp_enabled === true;
  const origProbe = orig.antigravity_quota_probe === true;

  let behavior = origBehavior;
  let modelChanged = false;
  let fallbacks = origFallbacks.map((m) => ({ ...m }));
  const picker = modelPanel({ value: orig.default_model || defaultSelection(S.projectId), onChange: () => { modelChanged = true; } });
  const behaviorSeg = segmented([
    ['steer', '割り込み', '実行中のターンに割り込んで指示を送る'],
    ['queue', 'キュー', '現在のターン終了後に送る'],
  ], behavior, (v) => { behavior = v; }, { label: '実行中メッセージの既定動作' });
  const fixesInp = h('input', { class: 'inp num', type: 'number', min: 0, max: 10, step: 1, inputmode: 'numeric', value: String(origFixes), 'aria-label': '検証失敗時の自動修正上限' });
  const retriesInp = h('input', { class: 'inp num', type: 'number', min: 0, max: 2, step: 1, inputmode: 'numeric', value: String(origRetries), 'aria-label': '容量エラー時の最大リトライ回数' });
  const resumeChk = h('input', { type: 'checkbox', checked: origResume, 'aria-label': 'レート制限時の自動再開' });
  const mcpChk = h('input', { type: 'checkbox', checked: origMcp, 'aria-label': 'Antigravity ネイティブ MCP 注入' });
  const probeChk = h('input', { type: 'checkbox', checked: origProbe, 'aria-label': 'Antigravity クォータ直接照会' });

  // Ordered fallback models: provider select + model id input to add, ↑ / ↓ / 削除 per row.
  const fbList = h('div', { class: 'fb-list' });
  const renderFallbacks = () => {
    fbList.replaceChildren(
      ...fallbacks.map((m, i) => h('div', { class: 'fb-row' },
        provDot(m.provider),
        h('span', { class: 'fb-model', text: `${provLabel(m.provider)} / ${m.model}` }),
        h('button', { type: 'button', class: 'btn xs', text: '↑', title: '上に移動', 'aria-label': `${m.model} を上に移動`, disabled: i === 0, onclick: () => { [fallbacks[i - 1], fallbacks[i]] = [fallbacks[i], fallbacks[i - 1]]; renderFallbacks(); } }),
        h('button', { type: 'button', class: 'btn xs', text: '↓', title: '下に移動', 'aria-label': `${m.model} を下に移動`, disabled: i === fallbacks.length - 1, onclick: () => { [fallbacks[i], fallbacks[i + 1]] = [fallbacks[i + 1], fallbacks[i]]; renderFallbacks(); } }),
        h('button', { type: 'button', class: 'btn xs danger-ghost', text: '削除', 'aria-label': `${m.model} を削除`, onclick: () => { fallbacks.splice(i, 1); renderFallbacks(); } }))),
      fallbacks.length ? null : h('p', { class: 'muted', text: 'フォールバックモデルはありません' }));
  };
  renderFallbacks();
  const fbProv = h('select', { class: 'inp sm', 'aria-label': 'フォールバックのプロバイダ' }, enabled.map((id) => h('option', { value: id, text: provLabel(id) })));
  const fbModel = h('input', { class: 'inp sm mono', placeholder: 'モデル ID（例: gpt-6-sol）', autocapitalize: 'off', spellcheck: false, 'aria-label': 'フォールバックのモデル' });
  const fbAdd = h('button', {
    type: 'button', class: 'btn sm', text: '追加',
    onclick: () => {
      const model = fbModel.value.trim();
      if (!model) { fbModel.focus(); return; }
      fallbacks.push({ provider: fbProv.value, model });
      fbModel.value = '';
      renderFallbacks();
    },
  });

  openModal('ランタイム設定', (body, close) => {
    const go = h('button', { class: 'btn primary', type: 'submit', text: '保存' });
    body.append(h('form', {
      class: 'form',
      onsubmit: async (e) => {
        e.preventDefault(); if (e.submitter !== go) return;
        go.disabled = true;
        // Partial update: only fields that changed from the loaded values are sent.
        const patch = {};
        if (modelChanged) patch.default_model = picker.get();
        if (behavior !== origBehavior) patch.follow_up_behavior = behavior;
        const fixes = normNum(fixesInp.value, origFixes);
        if (fixes !== origFixes) patch.verify_max_auto_fixes = fixes;
        if (resumeChk.checked !== origResume) patch.auto_resume_limited_threads = resumeChk.checked;
        const retries = Math.min(2, normNum(retriesInp.value, origRetries));
        if (retries !== origRetries) patch.rate_limit_max_retries = retries;
        if (JSON.stringify(fallbacks) !== JSON.stringify(origFallbacks)) patch.fallback_models = fallbacks;
        if (mcpChk.checked !== origMcp) patch.antigravity_mcp_enabled = mcpChk.checked;
        if (probeChk.checked !== origProbe) patch.antigravity_quota_probe = probeChk.checked;
        if (!Object.keys(patch).length) { close(); toast('変更はありません', '', 1800); return; }
        try {
          await api('/api/settings', { method: 'PATCH', json: patch });
          if (patch.default_model !== undefined || patch.follow_up_behavior !== undefined) {
            S.meta = {
              ...(S.meta || {}),
              ...(patch.default_model !== undefined ? { default_model: patch.default_model } : {}),
              ...(patch.follow_up_behavior !== undefined ? { follow_up_behavior: patch.follow_up_behavior } : {}),
            };
          }
          if (patch.follow_up_behavior !== undefined && !ls.get('t3.behavior', '')) S.behavior = patch.follow_up_behavior;
          close();
          toast('設定を保存しました（即時反映）', 'ok');
          renderComposer();
        } catch (err) { go.disabled = false; toast(`保存に失敗: ${err.message}`, 'err'); }
      },
    },
    field('既定モデル', h('div', { class: 'mp-embed' }, picker.el)),
    field('実行中メッセージの既定動作', behaviorSeg, '実行中に新しいメッセージを送ったときの動作。割り込みは現在のターンを止めて指示を送り、キューはターン終了後に送ります。'),
    field('検証失敗時の自動修正上限', fixesInp, '検証が失敗したときに自動で修正を試みる回数の上限（0〜10）。'),
    field('レート制限時の自動再開', h('label', { class: 'switch' }, resumeChk, h('i'), h('span', { text: 'レート制限で停止したスレッドを自動で再開する' }))),
    field('容量エラー時の最大リトライ回数', retriesInp, '一時的な容量制限だけを同じプロバイダで最大 2 回再試行します。アカウントのクォータ上限は再試行しません。'),
    field('クォータ枯渇時のフォールバックモデル', h('div', { class: 'stack' }, fbList, h('div', { class: 'fb-add' }, fbProv, fbModel, fbAdd)), 'プロバイダのクォータが枯渇したときに上から順に試します。'),
    field('Antigravity MCP', h('label', { class: 'switch' }, mcpChk, h('i'), h('span', { text: 'ネイティブ MCP 注入を有効にする' })), '有効にする前に `t3d setup-agy-mcp` で MCP 連携を設定してください。'),
    field('Antigravity クォータ照会', h('label', { class: 'switch' }, probeChk, h('i'), h('span', { text: 'Antigravity のクォータを直接照会する' }))),
    h('div', { class: 'actions' }, go)));
    setTimeout(() => picker.search.focus(), 0);
  }, { cls: 'form-modal' });
}

function forkDialog() {
  const t = curThread(); if (!t) return;
  openModal('分岐・モデル切替', (body, close) => {
    const title = h('input', { class: 'inp', value: `${cleanTitle(t.title)} (分岐)`, 'aria-label': 'タイトル' });
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

function delegateDialog(initialTask = '') {
  const t = curThread(); if (!t) return;
  const taskText = typeof initialTask === 'string' ? initialTask : '';
  openModal('並列委譲（子スレッド）', (body, close) => {
    const draft = taskText || ta().value.trim();
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
          close(); toast(`子スレッドを開始しました: ${r.thread ? cleanTitle(r.thread.title) : ''}`, 'ok');
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

/** Search snippet as plain prose: server highlight tags and Markdown syntax removed. */
const plainSnippet = (s) => String(s || '')
  .replace(/<\/?(mark|b)>/gi, '')
  .replace(/!?\[([^\]\n]*)\]\([^)\n]*\)?/g, '$1')
  .replace(/```[\w-]*|`|\*\*|__|~~/g, '')
  .replace(/(^|\n)\s*(#{1,6}\s+|>\s*|[-*+]\s+)/g, '$1')
  .replace(/\|?\s*:?-{3,}:?\s*\|?|\|/g, ' ')
  .replace(/\s+/g, ' ').trim();
const searchTerms = (q) => [...new Set(q.split(/\s+/).map((t) => t.replace(/^["'*]+|["'*]+$/g, '')).filter(Boolean))].sort((a, b) => b.length - a.length);
/** Text nodes with <mark> around every query term (works for trigram and 1–2 character LIKE searches alike). */
function highlight(text, terms) {
  if (!terms.length) return [text];
  const re = new RegExp(`(${terms.map((t) => t.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')).join('|')})`, 'giu');
  return text.split(re).map((part, i) => (i % 2 ? h('mark', { text: part }) : part)).filter((x) => x !== '');
}
/** Starts a long snippet shortly before its first match so the highlight stays inside the two visible lines. */
function aroundMatch(text, terms) {
  const low = text.toLowerCase();
  const at = Math.min(...terms.map((t) => low.indexOf(t.toLowerCase())).filter((i) => i >= 0));
  return Number.isFinite(at) && at > 50 ? `…${text.slice(at - 30)}` : text;
}
/** One group per thread with at most two distinct snippets; snippets that only repeat the title are dropped. */
function groupHits(rows, terms) {
  const norm = (s) => s.toLowerCase().replace(/…|\.\.\./g, '').replace(/\s+/g, '');
  const groups = new Map();
  for (const r of rows) {
    let g = groups.get(r.thread_id);
    if (!g) {
      g = { thread_id: r.thread_id, title: cleanTitle(r.thread_title), updated_at: r.updated_at, snippets: [], seen: [] };
      groups.set(r.thread_id, g);
    }
    if (g.snippets.length >= 2) continue;
    const text = plainSnippet(r.snippet);
    const key = norm(cleanTitle(text));
    if (!key || norm(g.title).startsWith(key) || norm(plainSnippet(r.thread_title)).startsWith(norm(text))) continue;
    if (g.seen.some((k) => k.includes(key) || key.includes(k))) continue;
    g.seen.push(key);
    g.snippets.push({ text: aroundMatch(text, terms), item_id: r.item_id });
  }
  return [...groups.values()];
}

function searchDialog(initialQuery = '') {
  const open = $('dialog.search');
  if (open) { open.close(); return; }
  const query = typeof initialQuery === 'string' ? initialQuery : '';
  closeDrawer();
  openModal('全文検索', (body, close) => {
    const inp = h('input', { class: 'srch-input', type: 'text', enterKeyHint: 'search', placeholder: '会話・コマンド出力を検索（日本語対応）', autocomplete: 'off', spellcheck: false, 'aria-label': '検索語', value: query });
    const list = h('ul', { class: 'hits', role: 'listbox' });
    const note = h('div', { class: 'srch-note', role: 'status' });
    let options = []; let sel = 0; let seq = 0;
    const select = (i, scroll) => {
      sel = i;
      options.forEach((o, k) => { o.el.classList.toggle('sel', k === i); o.el.setAttribute('aria-selected', String(k === i)); });
      if (scroll) options[i]?.el.scrollIntoView({ block: 'nearest' });
    };
    const go = async (x) => { close(); await openThread(x.thread_id); if (x.item_id) await jumpToItem(x.item_id); };
    const draw = (groups, terms) => {
      options = [];
      const option = (el, target) => {
        const i = options.length;
        el.setAttribute('role', 'option');
        el.addEventListener('click', () => go(target));
        el.addEventListener('mousemove', () => { if (sel !== i) select(i); });
        options.push({ el, target });
        return el;
      };
      list.replaceChildren(...groups.map((g) => {
        const th = threadById(g.thread_id);
        const head = h('div', { class: 'ht' }, th ? provDot(th.model_selection?.provider) : null, h('span', { class: 'ht-title' }, highlight(g.title, terms)), th ? h('em', { text: projectTitle(th.project_id) }) : null, h('small', { text: fmtAgo(g.updated_at) }));
        const li = h('li', { class: 'hit-group', role: 'presentation', dataset: { thread: g.thread_id } });
        if (!g.snippets.length) li.append(option(h('div', { class: 'hit' }, head), { thread_id: g.thread_id }));
        else {
          head.addEventListener('click', () => go({ thread_id: g.thread_id, item_id: g.snippets[0].item_id }));
          li.append(head, ...g.snippets.map((s) => option(h('div', { class: 'hit sub' }, h('div', { class: 'sn' }, highlight(s.text, terms))), { thread_id: g.thread_id, item_id: s.item_id })));
        }
        return li;
      }));
      if (options.length) select(0);
    };
    const recent = () => {
      draw(S.threads.filter((t) => !t.archived).sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at)).slice(0, 8)
        .map((t) => ({ thread_id: t.id, title: cleanTitle(t.title), updated_at: t.updated_at, snippets: [] })), []);
      note.textContent = options.length ? '最近のスレッド' : 'スレッドはまだありません';
    };
    const run = debounce(async () => {
      const q = inp.value.trim();
      const my = ++seq;
      if (!q) { recent(); return; }
      try {
        const r = await api(`/api/search?q=${encodeURIComponent(q)}&limit=60`);
        if (my !== seq) return;
        const terms = searchTerms(q);
        const groups = groupHits(Array.isArray(r) ? r : [], terms);
        draw(groups, terms);
        note.textContent = groups.length ? `${groups.length}スレッド` : '一致なし';
      } catch (e) { note.textContent = `検索に失敗: ${e.message}`; }
    }, 120);
    inp.addEventListener('input', run);
    inp.addEventListener('keydown', (e) => {
      if (e.isComposing || e.keyCode === 229) return;
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        e.preventDefault(); if (!options.length) return;
        select((sel + (e.key === 'ArrowDown' ? 1 : -1) + options.length) % options.length, true);
      } else if (e.key === 'Enter' && options[sel]) { e.preventDefault(); go(options[sel].target); }
    });
    body.append(h('div', { class: 'srch-row' }, iconEl('search'), inp, h('kbd', { text: 'esc' })), note, list);
    if (query.trim()) run(); else recent();
    setTimeout(() => inp.focus(), 0);
  }, { cls: 'search' });
}

// ───────────────────────── usage: data (T3 Code UsagePage) ─────────────────────────


function loadUsage(days, { refresh = false } = {}) {
  const u = S.usage;
  if (!refresh && u.pending.has(days)) return u.pending.get(days);
  const seq = (u.seq.get(days) || 0) + 1;
  u.seq.set(days, seq);
  if (refresh) u.refreshing.add(days);
  const latest = () => u.seq.get(days) === seq; // an older response must never overwrite a newer one
  const p = api(`/api/usage?days=${days}&tz_offset_mins=${-new Date().getTimezoneOffset()}${refresh ? '&refresh=1' : ''}`)
    .then(normalizeUsage)
    .then((d) => { if (latest()) { u.cache.set(days, d); u.errors.delete(days); } })
    .catch((e) => { if (latest()) u.errors.set(days, e.message || String(e)); })
    .finally(() => {
      if (!latest()) return;
      u.pending.delete(days); u.refreshing.delete(days);
      renderUsagePill(); u.onchange?.(days);
    });
  u.pending.set(days, p);
  u.onchange?.(days);
  return p;
}
const loadUsageSoon = debounce(() => { loadUsage(7); if (S.usage.onchange && S.usage.days !== 7) loadUsage(S.usage.days); }, 5000);

function renderUsagePill() {
  const d = S.usage.cache.get(7);
  const pill = $('#usage-pill');
  const btn = $('#btn-usage');
  const hint = `使用量を開く (${kbd('U')})`;
  const limits = limitsSummary(Date.now());
  btn.classList.toggle('limit-out', limits.exhausted);
  const tooltip = (head) => [head, limits.text, hint].filter(Boolean).join('\n');
  if (!d) {
    const err = S.usage.errors.get(7);
    pill.textContent = err ? '–' : '…';
    btn.title = tooltip(err ? `使用量を取得できません: ${err}` : '');
    return;
  }
  const t = d.totals;
  pill.textContent = `${t.cost_usd > 0 || !t.total_tokens ? fmtUsd(t.cost_usd) : `${fmtCompact(t.total_tokens)} tok`} · 7日`;
  btn.title = tooltip(`過去7日間: ${fmtUsd(t.cost_usd)}（API 推定）· ${fmtCompact(t.total_tokens)} トークン · ${fmtNum(t.sessions)} セッション`);
}

// ───────────────────────── usage: limits (T3 Code UsageLimits) ─────────────────────────


function loadUsageLimits({ refresh = false } = {}) {
  const l = S.limits;
  if (l.pending && (!refresh || l.refreshing)) return l.pending;
  const seq = ++l.seq;
  const latest = () => l.seq === seq; // an older response must never overwrite a newer one
  l.refreshing = refresh;
  l.pending = api(`/api/usage/limits${refresh ? '?refresh=1' : ''}`)
    .then(normalizeLimits)
    .then((d) => { if (latest()) { l.data = d; l.error = ''; l.fetchedAt = Date.now(); } })
    .catch((e) => { if (latest()) l.error = e.message || String(e); })
    .finally(() => {
      if (!latest()) return;
      l.pending = null; l.refreshing = false;
      renderLimitSurfaces();
    });
  l.onchange?.();
  return l.pending;
}
const loadUsageLimitsSoon = debounce(() => loadUsageLimits(), 5000);
/** Everything that shows limits; also run on a timer so countdowns and passed resets stay current. */
function renderLimitSurfaces() { renderUsagePill(); syncLimitAlert(); S.limits.onchange?.(); if (shownRail() === 'context') renderContextPanel(); }


/** The sidebar tooltip line (tightest live window per provider) and whether anything is used up. */
function limitsSummary(now) {
  const parts = []; let exhausted = false;
  for (const p of S.limits.data?.providers || []) {
    const live = p.windows.filter((w) => !limitIsReset(w, now));
    if (!live.length) continue;
    const least = Math.min(...live.map((w) => w.remaining));
    exhausted ||= least <= 0;
    parts.push(`${p.label} ${fmtLeft(least)}`);
  }
  return { text: parts.length ? `上限の残り: ${parts.join(' · ')}` : '', exhausted };
}

/** Pill above the textarea while the selected provider is out of quota or nearly so. A live region: touched only on change. */
function syncLimitAlert() {
  const box = $('#limit-alert');
  const id = uId(S.compose.sel?.provider || curThread()?.model_selection?.provider);
  const p = S.limits.data?.providers.find((x) => x.id === id);
  const now = Date.now();
  const live = p?.windows.filter((w) => !limitIsReset(w, now)) || [];
  // among used-up windows the one that blocks longest matters; otherwise the tightest one
  const out = live.filter((w) => w.remaining <= 0).sort((a, b) => b.resetsAt - a.resetsAt)[0];
  const w = out || live.filter((x) => x.remaining <= LIMIT_CRITICAL).sort((a, b) => a.remaining - b.remaining)[0];
  let text = '';
  if (w) {
    // a limit observed at run time has no quota name, only the fact that it was hit
    const pooled = p.windows.filter((x) => x.kind === w.kind).length > 1; // e.g. one 5-hour window per model pool
    const name = pooled ? `${w.label} ` : w.kind === 'session' ? 'セッション' : w.label;
    const what = w.id === 'runtime' ? w.label : `${name}上限${out ? '到達' : 'が残りわずか'}`;
    const left = fmtResetIn(w.resetsAt, now);
    const when = left.startsWith('あと') ? `${left}でリセット` : left;
    text = `${p.label} ${what}（残り ${fmtLeft(w.remaining)}${when ? ` · ${when}` : ''}）`;
  }
  const inThread = !!curThread();
  const key = text && `${out ? 'out' : 'low'}:${inThread ? 'thread' : 'home'}:${text}`;
  if ((box.dataset.key || '') === key) return;
  box.dataset.key = key;
  const focused = box.contains(document.activeElement);
  box.replaceChildren(...(text ? [h('button', {
    type: 'button', class: `limit-pill${out ? ' out' : ''}`, title: '使用量の「上限」を開く', onclick: () => usageDialog({ metric: 'limits' }),
  }, iconEl('alert'), h('span', { class: 'limit-pill-text', text }), h('span', { class: 'limit-pill-more', text: '— 詳細' })),
  h('button', {
    type: 'button', class: 'limit-switch', title: inThread ? '別モデルへ分岐する' : 'モデルを切り替える',
    onclick: () => { if (inThread) forkDialog(); else openModelPopover($('#btn-model')); },
  }, iconEl('split'), h('span', { text: inThread ? '別モデルへ分岐' : 'モデル切替' }))] : []));
  if (focused) $('button', box)?.focus();
}


/** The thick bar: remaining fill, hatched refill region, even-pace tick, and the countdown / reset-credit pill. */
function limitBar(p, w, st, now) {
  const { upcoming, expired, timeLeft } = st;
  const fill = h('span', { class: 'u-lwin-fill' });
  fill.style.width = w.remaining > 0 ? `max(2px, ${w.remaining}%)` : '0';
  let used = null;
  if (st.refill) { used = h('span', { class: 'u-lwin-used' }); used.style.width = `${st.refill}%`; }
  const pill = upcoming || p.credits ? h('span', { class: 'u-lwin-pill', 'aria-hidden': 'true' },
    upcoming ? [iconEl('refresh'), h('span', { class: 'u-lwin-in', text: fmtDurationJa(w.resetsAt - now, ' ') })] : null,
    upcoming && p.credits ? h('span', { text: '·' }) : null,
    p.credits ? [iconEl('ticket'), h('span', { class: 'u-lwin-credits', text: String(p.credits.count) })] : null) : null;
  const label = [
    `${p.label} ${w.label}: 残り ${fmtLeft(w.remaining)}`,
    timeLeft == null ? null : `均等なペースなら残り ${timeLeft}%`,
    upcoming ? `${fmtDurationJa(w.resetsAt - now)}後にリセット` : expired ? 'リセット済' : 'リセット時刻不明',
    p.credits ? `リセットクレジット ${p.credits.count} 件` : null,
    '詳細を表示',
  ].filter(Boolean).join(' · ');
  const bar = h('button', {
    type: 'button', class: `u-lwin-bar${timeLeft == null ? '' : ' paced'}`, 'aria-label': label, 'aria-haspopup': 'dialog', 'aria-expanded': 'false',
    dataset: { win: `${p.id}:${w.id || w.label}` }, onclick: (e) => openLimitPopover(e.currentTarget, p, w),
  },
  h('span', { class: 'u-lwin-track', 'aria-hidden': 'true' }, h('span', { class: 'u-lwin-layers' }, fill, used)),
  timeLeft == null ? null : h('span', { class: 'u-lwin-tick', 'aria-hidden': 'true' }),
  pill);
  if (timeLeft != null) bar.style.setProperty('--tick', `clamp(3px, ${timeLeft}%, calc(100% - 3px))`);
  return bar;
}

/** One quota window as its own card: label and big remaining share on the left, the bar on the right. */
function limitCard(p, w, now) {
  const st = limitState(w, now);
  let note = null;
  if (st.pace) {
    note = iconEl(st.pace.icon, `u-lwin-pace ${st.pace.cls}`);
    for (const [k, v] of Object.entries({ role: 'img', title: st.pace.title, 'aria-label': st.pace.title })) note.setAttribute(k, v);
  } else if (st.expired) note = h('span', { class: 'u-lwin-note', text: 'リセット済' });
  return h('div', { class: 'u-lwin-card', dataset: { window: w.id || w.label } },
    h('div', { class: 'u-lwin-meta' },
      h('span', { class: 'u-lwin-label', text: w.label }),
      h('div', { class: 'u-lwin-stat' }, h('b', { class: `u-lwin-pct${st.level}`, text: fmtLeft(w.remaining) }), h('span', { class: 'u-lwin-unit', text: '残り' }), note)),
    limitBar(p, w, st, now));
}

/** A provider section: header, then one card per window. */
function limitPool(p, now) {
  const source = limitSource(p.source);
  const pool = h('section', { class: 'u-lpool', 'aria-label': `${p.label} の上限`, dataset: { provider: p.id } },
    h('header', { class: 'u-lpool-head' }, uDot(p.color), h('h3', { text: p.label }),
      p.plan ? h('span', { class: 'u-limit-plan', text: p.plan }) : null,
      source ? h('span', { class: 'u-limit-source', title: `取得元: ${p.source}`, text: source }) : null,
      p.windows.some((w) => limitIsOut(w, now)) ? h('span', { class: 'u-limit-status', text: '上限到達' }) : null),
    p.windows.map((w) => limitCard(p, w, now)),
    p.message ? h('p', { class: 'u-limit-msg', text: p.message }) : null);
  pool.style.setProperty('--pc', p.color);
  return pool;
}

/** Detail panel of one window (UsageLimitsPooled popover), with the reset-credit action when the provider holds any. */
function limitDetail(p, w, now) {
  const st = limitState(w, now);
  const rows = (list) => h('dl', { class: 'u-lpop-rows' }, list.filter(Boolean).flatMap(([k, v]) => [h('dt', { text: k }), h('dd', { text: v })]));
  const reset = !w.resetsAt ? null : st.upcoming
    ? `${fmtResetAt(w.resetsAt, now)} · ${fmtDurationJa(w.resetsAt - now)}後` : `${fmtResetAt(w.resetsAt, now)}（リセット済）`;
  const c = p.credits;
  return [
    h('header', { class: 'u-lpop-head' }, uDot(p.color), h('div', {}, h('b', { text: p.label }), h('small', { text: w.label }))),
    p.plan ? rows([['プラン', p.plan]]) : null,
    rows([
      ['残り', fmtLeft(w.remaining)],
      reset && ['リセット', reset],
      st.timeLeft != null && ['均等なペース', `現時点で残り ${st.timeLeft}%`],
      st.refill && ['回復量', `プールの +${Math.round(st.refill)}%`],
    ]),
    c ? h('footer', { class: 'u-lpop-foot' },
      h('span', { text: `${c.count} 件を保有${c.nextExpiresAt > now ? ` · ${fmtDurationJa(c.nextExpiresAt - now)}後に期限切れ` : ''}` }),
      h('button', {
        type: 'button', class: 'btn sm', disabled: S.limits.redeeming, text: S.limits.redeeming ? '適用中…' : 'リセットを使用',
        onclick: () => {
          const anchor = popover?.anchor;
          closePopover();
          anchor?.focus(); // the confirm dialog hands focus back here when it closes
          confirmResetCredit(p);
        },
      })) : null,
  ].filter(Boolean);
}

function openLimitPopover(anchor, p, w, { focus = true } = {}) {
  const el = openPopover(anchor, limitDetail(p, w, Date.now()), { cls: 'u-lpop', align: 'center', below: 'auto' });
  if (!el) return;
  el.setAttribute('aria-label', `${p.label} ${w.label} の詳細`);
  el.tabIndex = -1;
  if (focus) el.focus({ preventScroll: true });
}

function confirmResetCredit(p) {
  openModal('リセットクレジットを使用しますか？', (body, close) => {
    const cancel = h('button', { type: 'button', class: 'btn', text: 'キャンセル', onclick: close });
    const ok = h('button', {
      type: 'button', class: 'btn primary', text: 'リセットを使用',
      onclick: () => {
        if (S.limits.redeeming) return;
        ok.disabled = true; cancel.disabled = true; ok.textContent = '適用中…';
        redeemResetCredit(p, close);
      },
    });
    body.append(h('div', { class: 'form' },
      h('p', { text: `${p.label} のリセットクレジットを 1 件消費して、使用上限をリセットします。（保有: ${p.credits.count} 件）` }),
      h('div', { class: 'actions' }, cancel, ok)));
    cancel.focus();
  }, { cls: 'limit-confirm' });
}

const RESET_OUTCOMES = {
  reset: ['ok', 'リセットを適用しました。'],
  nothingToReset: ['info', 'リセットする上限がありません。'],
  noCredit: ['warn', '使用できるリセットクレジットがありません。'],
  alreadyRedeemed: ['info', 'このリセットは適用済みです。'],
};

async function redeemResetCredit(p, close) {
  const l = S.limits;
  l.redeeming = true;
  let res; let failure;
  try { res = await api('/api/usage/limits/reset-credit', { method: 'POST', json: { provider: p.rawProvider || p.id } }); } catch (e) { failure = e; }
  // a limits request that is still in flight may predate the redemption: drop its answer
  l.seq += 1; l.pending = null; l.refreshing = false; l.redeeming = false;
  close();
  let data = null;
  try { if (res?.limits) data = normalizeLimits(res.limits); } catch { /* unusable payload: fetch it again below */ }
  if (data) { l.data = data; l.error = ''; l.fetchedAt = Date.now(); renderLimitSurfaces(); } else loadUsageLimits({ refresh: true });
  if (failure) { toast(`リセットを適用できませんでした: ${failure.message || failure}`, 'err', 6000); return; }
  const [kind, fallback] = Object.hasOwn(RESET_OUTCOMES, res?.outcome) ? RESET_OUTCOMES[res.outcome] : ['warn', 'リセットの結果を確認できませんでした。'];
  toast(uText(res?.message, 400) || fallback, kind, 6000);
}

function limitsView(d) {
  const now = Date.now();
  const pools = d.providers.filter((p) => p.windows.length);
  const notes = d.providers.filter((p) => !p.windows.length);
  if (!d.providers.length) return uEmpty('上限を確認できるプロバイダがありません');
  return h('div', { class: 'u-limits' },
    pools.map((p) => limitPool(p, now)),
    notes.length ? h('section', { class: 'u-limit-notes', 'aria-label': '上限を表示できないプロバイダ' },
      h('h3', { text: '上限を表示できないプロバイダ' }),
      h('ul', {}, notes.map((p) => h('li', { dataset: { provider: p.id, status: p.status } },
        h('div', { class: 'u-limit-note-head' }, uDot(p.color), h('b', { text: p.label }),
          h('span', { class: 'u-limit-source', text: p.status === 'unsupported' ? '非対応' : p.status === 'unavailable' ? '取得不可' : '情報なし' })),
        p.message ? h('p', { text: p.message }) : null,
        p.status === 'unavailable' ? h('p', { text: '更新ボタンで再試行できます。' }) : null)))) : null);
}

const limitsSkeleton = () => h('div', { class: 'u-limits u-skel', 'aria-busy': 'true', 'aria-label': '上限を取得中' },
  h('i', { class: 'card' }), h('i', { class: 'card' }), h('p', { class: 'muted', text: '上限を取得中…' }));

// ───────────────────────── usage: view ─────────────────────────
const uMetricKey = () => (S.usage.metric === 'cost' ? 'cost_usd' : 'total_tokens');
const uShareKey = () => (S.usage.metric === 'cost' ? 'cost_share' : 'token_share');
const uFormat = (v) => (S.usage.metric === 'cost' ? fmtUsd(v) : fmtCompact(v));

/** Applies the provider filter and the active metric to one payload. */
function usageView(d) {
  const key = uMetricKey();
  const rank = (a, b) => b[key] - a[key] || b.total_tokens - a.total_tokens;
  const ranked = [...d.providers].sort(rank);
  const visible = ranked.filter((p) => !S.usage.hidden.has(p.id));
  const filtered = visible.length !== ranked.length;
  const totals = filtered ? { ...uSum(visible), reasoning_tokens: 0 } : d.totals;
  // the server's shares are authoritative until the filter changes the denominator
  const share = (row) => (!filtered && row[uShareKey()]) || (totals[key] > 0 ? Math.min(1, row[key] / totals[key]) : 0);
  const models = d.models.filter((m) => !S.usage.hidden.has(m.id)).sort(rank);
  const columns = d.buckets.map((b) => {
    const parts = visible.map((p) => ({ p, cost_usd: b.by.get(p.id)?.cost_usd || 0, total_tokens: b.by.get(p.id)?.total_tokens || 0 }));
    return { bucket: b, parts, cost_usd: parts.reduce((a, x) => a + x.cost_usd, 0), total_tokens: parts.reduce((a, x) => a + x.total_tokens, 0) };
  });
  return { key, ranked, visible, filtered, totals, share, models, columns, hourly: d.window.hourly };
}

const uTint = (el, color) => { el.style.background = color; return el; };
const uDot = (color) => uTint(h('i', { class: 'u-dot', 'aria-hidden': 'true' }), color);
function uBar(fraction, color, cls = '') {
  const fill = uTint(h('i'), color);
  fill.style.width = fraction > 0 ? `max(4px, ${Math.min(100, fraction * 100)}%)` : '0';
  return h('div', { class: `u-bar ${cls}`, 'aria-hidden': 'true' }, fill);
}
const uEmpty = (text = 'この期間の利用はありません') => h('div', { class: 'u-empty', text });


function tickText(v, step) {
  if (S.usage.metric === 'tokens') return v === 0 ? '0' : fmtCompact(v);
  if (v === 0) return '$0';
  let digits = 0;
  while (digits < 4 && Math.abs(step * 10 ** digits - Math.round(step * 10 ** digits)) > 1e-6) digits += 1;
  if (digits === 1) digits = 2;
  return `$${v.toLocaleString('en-US', { minimumFractionDigits: digits, maximumFractionDigits: digits })}`;
}


/** UsageProviderChart: overlapping monotone areas, one per provider, each from the zero baseline. */
function usageChart(view) {
  const { columns, visible, key, hourly } = view;
  const n = columns.length;
  const scale = niceScale(Math.max(0, ...columns.flatMap((c) => c.parts.map((x) => x[key]))));
  const px = (i) => (n > 1 ? (i / (n - 1)) * 100 : 50); // percent
  const py = (v) => (1 - v / scale.max) * 100;
  const at = (el, v) => { el.style.top = `${py(v)}%`; return el; };
  const yaxis = h('div', { class: 'u-yaxis', 'aria-hidden': 'true' },
    scale.labeled ? scale.ticks.map((v) => at(h('span', { text: tickText(v, scale.step) }), v)) : null);
  const graph = svgNode('svg', { class: 'u-svg', viewBox: '0 0 1000 1000', preserveAspectRatio: 'none', 'aria-hidden': 'true' });
  const series = visible.map((p, k) => ({ p, values: columns.map((c) => c.parts[k][key]) }))
    .filter((s) => s.values.some((v) => v > 0))
    .sort((a, b) => b.values.reduce((x, y) => x + y, 0) - a.values.reduce((x, y) => x + y, 0)); // small series paint on top
  for (const s of series) {
    const pts = s.values.map((v, i) => [px(i) * 10, py(v) * 10]);
    if (pts.length === 1) pts.splice(0, 1, [0, pts[0][1]], [1000, pts[0][1]]);
    const line = monotonePath(pts);
    const area = svgNode('path', { class: 'u-area', d: `${line}L${pts.at(-1)[0]} 1000L${pts[0][0]} 1000Z`, 'fill-opacity': '0.12', stroke: 'none' });
    const stroke = svgNode('path', { class: 'u-line', d: line, fill: 'none', 'stroke-width': '2', 'vector-effect': 'non-scaling-stroke', 'stroke-linejoin': 'round', 'stroke-linecap': 'round' });
    area.style.fill = s.p.color; stroke.style.stroke = s.p.color;
    area.dataset.provider = s.p.id; stroke.dataset.provider = s.p.id;
    graph.append(area, stroke);
  }
  const cross = h('div', { class: 'u-cross', hidden: true, 'aria-hidden': 'true' });
  const dots = series.map((s) => { const d = h('i', { class: 'u-hdot', hidden: true, 'aria-hidden': 'true', dataset: { provider: s.p.id } }); d.style.color = s.p.color; return d; });
  const tip = h('div', { class: 'u-tip', role: 'status', hidden: true });
  const plot = h('div', {
    class: 'u-plot', tabindex: 0, role: 'group',
    'aria-label': `${hourly ? '時間別' : '日別'}の${S.usage.metric === 'cost' ? 'コスト' : 'トークン'}グラフ。左右キーで期間を移動`,
  }, scale.ticks.map((v) => at(h('div', { class: `u-grid${v === 0 ? ' base' : ''}`, 'aria-hidden': 'true' }), v)), graph, cross, dots,
  n && scale.labeled ? null : uEmpty(), tip);

  let active = -1;
  const hide = () => { active = -1; tip.hidden = true; cross.hidden = true; dots.forEach((d) => { d.hidden = true; }); };
  const show = (i, x, y) => {
    if (i !== active) {
      active = i;
      const c = columns[i];
      cross.style.left = `${px(i)}%`;
      dots.forEach((d, k) => { d.style.left = `${px(i)}%`; d.style.top = `${py(series[k].values[i])}%`; });
      const line = (cls, head, row) => h('div', { class: cls }, head, h('span', { class: 'u-tip-v' },
        h('b', { text: fmtUsd(row.cost_usd) }), h('small', { text: `${fmtNum(Math.round(row.total_tokens))} tok` })));
      const used = c.parts.filter((p) => p.cost_usd > 0 || p.total_tokens > 0);
      tip.replaceChildren(h('div', { class: 'u-tip-h', text: bucketTitle(c.bucket, hourly) }),
        ...(used.length ? used.map((p) => line('u-tip-row', h('span', { class: 'u-tip-l' }, uDot(p.p.color), p.p.label), p)) : [h('div', { class: 'u-tip-l', text: '利用なし' })]),
        line('u-tip-row total', h('span', { class: 'u-tip-l', text: '合計' }), c));
    }
    cross.hidden = false; tip.hidden = false;
    dots.forEach((d) => { d.hidden = false; });
    const w = tip.offsetWidth; const th = tip.offsetHeight;
    const left = x + 14 + w <= plot.clientWidth ? x + 14 : x - 14 - w;
    const top = y + 12 + th <= plot.clientHeight ? y + 12 : y - 12 - th;
    tip.style.left = `${Math.max(0, Math.min(left, plot.clientWidth - w))}px`;
    tip.style.top = `${Math.max(0, Math.min(top, plot.clientHeight - th))}px`;
  };
  const point = (e) => {
    const r = plot.getBoundingClientRect();
    if (!n || !r.width) return;
    const x = e.clientX - r.left;
    show(Math.max(0, Math.min(n - 1, Math.round((x / r.width) * (n - 1)))), x, e.clientY - r.top);
  };
  plot.addEventListener('pointermove', point);
  plot.addEventListener('pointerdown', point);
  plot.addEventListener('pointerleave', (e) => { if (e.pointerType === 'mouse') hide(); }); // touch keeps the tooltip until it taps elsewhere
  plot.addEventListener('blur', hide);
  plot.addEventListener('keydown', (e) => {
    if (!n || (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight')) return;
    e.preventDefault();
    const i = active < 0 ? n - 1 : Math.max(0, Math.min(n - 1, active + (e.key === 'ArrowLeft' ? -1 : 1)));
    show(i, (px(i) / 100) * plot.clientWidth, 8);
  });

  const xaxis = h('div', { class: 'u-xaxis', 'aria-hidden': 'true' }, axisMarks(columns, hourly).map((i) => {
    const x = px(i);
    const el = h('span', { class: x < 4 ? 'start' : x > 96 ? 'end' : '', text: bucketTick(columns[i].bucket, hourly) });
    el.style.left = `${x}%`;
    return el;
  }));
  return { el: h('div', { class: 'u-chart-box' }, h('div', { class: 'u-chart' }, yaxis, plot), xaxis), hide, plot };
}

/** Left hero column: headline metric and the ranked provider list, which doubles as the filter. */
function usageSummary(view, rerender) {
  const cost = S.usage.metric === 'cost';
  const t = view.totals;
  const providers = view.ranked.map((p) => {
    const off = S.usage.hidden.has(p.id);
    const share = off ? 0 : view.share(p);
    return h('button', {
      type: 'button', class: `u-prov${off ? ' off' : ''}`, dataset: { provider: p.id }, 'aria-pressed': String(!off),
      title: off ? `${p.label} を表示する` : `${p.label} を非表示にする`,
      onclick: () => { off ? S.usage.hidden.delete(p.id) : S.usage.hidden.add(p.id); rerender(); },
    },
    h('span', { class: 'u-prov-top' }, uDot(p.color), h('span', { class: 'u-prov-name', text: p.label }), h('span', { class: 'u-prov-val', text: off ? '–' : uFormat(p[view.key]) })),
    uBar(share, p.color),
    h('span', { class: 'u-prov-sub' }, h('span', { text: off ? '非表示' : `${fmtNum(p.sessions)} セッション` }), h('span', { text: off ? '' : fmtShare(share) })));
  });
  return h('div', { class: 'u-summary' },
    h('div', { class: 'u-headline' },
      h('p', { class: 'u-big', text: uFormat(t[view.key]) }),
      h('p', { class: 'u-sub', text: cost ? `${fmtNum(t.sessions)} セッション · API 推定` : `${fmtNum(t.sessions)} セッション · ${fmtNum(t.requests)} リクエスト` }),
      view.filtered ? h('button', { type: 'button', class: 'link u-reset', text: '絞り込み中 — すべて表示', onclick: () => { S.usage.hidden.clear(); rerender(); } }) : null),
    providers.length ? providers : uEmpty('プロバイダのデータがありません'));
}

function usageTotals(view) {
  const t = view.totals;
  const stat = (label, value, title) => h('div', { title }, h('dt', { text: label }), h('dd', { text: value }));
  const mix = U_KINDS.map((k) => ({ ...k, value: t[k.key] })).filter((k) => k.value > 0);
  const mixSum = mix.reduce((a, k) => a + k.value, 0);
  const prompt = t.input_tokens + t.cache_read_tokens + t.cache_write_tokens;
  const exact = (v) => `${fmtNum(v)} tok`;
  return [
    h('section', { class: 'u-section' },
      h('div', { class: 'u-thead' }, h('h3', { text: '合計' }),
        prompt > 0 ? h('span', { class: 'u-hit', title: `キャッシュ読込 ÷ (入力 + キャッシュ読込 + キャッシュ書込)\n${fmtNum(t.cache_read_tokens)} / ${fmtNum(prompt)} tok` }, 'キャッシュヒット率 ', h('b', { text: fmtShare(t.cache_read_tokens / prompt) })) : null),
      h('dl', { class: 'u-totals' },
        stat('処理トークン', fmtCompact(t.total_tokens), exact(t.total_tokens)),
        stat('入力', fmtCompact(t.input_tokens), exact(t.input_tokens)),
        stat('出力', fmtCompact(t.output_tokens), t.reasoning_tokens ? `${exact(t.output_tokens)}（うち推論 ${fmtNum(t.reasoning_tokens)}）` : exact(t.output_tokens)),
        stat('キャッシュ読込', fmtCompact(t.cache_read_tokens), exact(t.cache_read_tokens)),
        stat('キャッシュ書込', fmtCompact(t.cache_write_tokens), exact(t.cache_write_tokens)))),
    S.usage.metric === 'tokens' && mixSum > 0 ? h('section', { class: 'u-section u-mix' }, h('h3', { text: '種類別トークン' }),
      h('div', { class: 'u-mixbar', 'aria-hidden': 'true' }, mix.map((k) => {
        const seg = uTint(h('i', { title: `${k.label} · ${fmtCompact(k.value)} · ${fmtShare(k.value / mixSum)}` }), k.color);
        seg.style.flexGrow = String(k.value / mixSum);
        return seg;
      })),
      h('ul', {}, mix.map((k) => h('li', {}, uDot(k.color), h('span', { class: 'muted', text: k.label }), h('span', { text: fmtCompact(k.value) }))))) : null,
  ];
}

function usageModels(view) {
  if (!view.models.length) return uEmpty();
  const peak = Math.max(0, ...view.models.map((m) => m[view.key]));
  return h('ol', { class: 'u-models' }, view.models.map((m, i) => h('li', { class: 'u-model' },
    h('span', { class: 'u-rank', text: String(i + 1) }),
    h('span', { class: 'u-model-name', title: `${m.label} / ${m.model} · ${fmtNum(m.sessions)} セッション · ${fmtNum(m.requests)} リクエスト` }, uDot(m.color), h('code', { text: m.model })),
    h('span', { class: 'u-model-val', text: uFormat(m[view.key]) }),
    h('span', { class: 'u-model-share', text: fmtShare(view.share(m)) }),
    uBar(peak > 0 ? m[view.key] / peak : 0, m.color),
    h('span', { class: 'u-chips' }, U_KINDS.map((k) => h('span', { class: 'u-chip', title: `${k.label}: ${fmtNum(m[k.key])} tok` }, k.label, h('b', { text: fmtCompact(m[k.key]) })))))));
}

function usageTimeline(view) {
  const rows = view.columns.filter((c) => c.total_tokens > 0 || c.cost_usd > 0).reverse();
  if (!rows.length) return uEmpty();
  const peak = Math.max(0, ...rows.map((c) => c[view.key]));
  return h('table', { class: 'u-times' },
    h('thead', {}, h('tr', {}, h('th', { text: view.hourly ? '時間' : '日付' }), h('th', { class: 'u-t-bar', text: 'プロバイダ内訳' }), h('th', { text: 'トークン' }), h('th', { text: '推定コスト' }))),
    h('tbody', {}, rows.map((c) => {
      const bar = h('div', { class: 'u-hbar' }, c.parts.filter((x) => x[view.key] > 0).map((x) => {
        const seg = uTint(h('i', { title: `${x.p.label}: ${fmtUsd(x.cost_usd)} · ${fmtNum(x.total_tokens)} tok` }), x.p.color);
        seg.style.flexGrow = String(x[view.key] / c[view.key]);
        return seg;
      }));
      bar.style.width = c[view.key] > 0 ? `max(4px, ${(c[view.key] / peak) * 100}%)` : '0';
      return h('tr', {}, h('th', { scope: 'row', text: bucketTitle(c.bucket, view.hourly, true) }), h('td', { class: 'u-t-bar' }, bar),
        h('td', { class: 'muted', title: `${fmtNum(c.total_tokens)} tok`, text: fmtCompact(c.total_tokens) }), h('td', { text: fmtUsd(c.cost_usd) }));
    })));
}

const usageSkeleton = () => h('div', { class: 'u-hero u-skel', 'aria-busy': 'true', 'aria-label': '使用量を読み込み中' },
  h('div', { class: 'u-summary' }, ['big', 'sub', 'row', 'row', 'row', 'row'].map((c) => h('i', { class: c }))),
  h('div', {}, h('i', { class: 'chart' }), h('p', { class: 'muted', text: '使用量を集計しています…' })));

/** Opens the usage modal. A string selects a breakdown tab; { metric: 'limits' } opens the Limits view. */
function usageDialog(arg) {
  const { tab, metric } = typeof arg === 'string' ? { tab: arg } : arg || {};
  const u = S.usage;
  const l = S.limits;
  if (tab) u.tab = tab;
  if ($('dialog.usage[open]')) return;
  closeDrawer();
  if (metric) u.metric = metric;
  else if (tab && u.metric === 'limits') u.metric = 'cost'; // breakdown tabs do not exist in the Limits view
  const range = h('span', { class: 'u-range' });
  const top = h('div', { class: 'u-top' });
  const tabs = h('div', { class: 'u-tabs' });
  const panel = h('div', { class: 'u-panel' });
  const foot = h('p', { class: 'u-foot' });
  let system = null; // the DB / privacy panel loads on first use
  let chart = null;
  let drawn = []; // what the top half currently shows
  let breakdown = null;
  let period = null;
  const inLimits = () => u.metric === 'limits';

  const refresh = ibtn('refresh', '使用量を再スキャン', () => {
    if (inLimits()) { loadUsageLimits({ refresh: true }); return; }
    if (u.refreshing.has(u.days)) return;
    loadUsage(u.days, { refresh: true });
    if (u.tab === 'system') system?.load();
  }, 'sm u-refresh');

  const renderBreakdown = () => {
    const d = u.cache.get(u.days);
    const hourly = d ? d.window.hourly : u.days === 1;
    const focused = tabs.contains(document.activeElement);
    tabs.replaceChildren(segmented([['model', 'モデル別'], ['time', hourly ? '時間別' : '日別'], ['system', 'DB・プライバシー']], u.tab,
      (v) => { u.tab = v; renderBreakdown(); }, { label: '内訳の表示', cls: 'mini' }));
    if (focused) $('button.on', tabs)?.focus();
    if (u.tab === 'system') {
      system ||= systemPanel();
      if (panel.firstChild !== system.el) panel.replaceChildren(system.el);
    } else if (d) panel.replaceChildren(u.tab === 'model' ? usageModels(usageView(d)) : usageTimeline(usageView(d)));
    else panel.replaceChildren(uEmpty(u.errors.has(u.days) ? '使用量を取得できないため表示できません' : '読み込み中…'));
  };

  const setBusy = (busy, label) => {
    refresh.classList.toggle('busy', busy);
    refresh.setAttribute('aria-busy', String(busy));
    refresh.title = label; refresh.setAttribute('aria-label', label);
  };
  const errorBanner = (text, retry) => h('div', { class: 'u-error', role: 'alert' }, h('span', { text }),
    h('button', { type: 'button', class: 'btn xs', text: '再試行', onclick: retry }));

  const renderLimits = () => {
    const d = l.data;
    const busy = !!l.pending;
    range.textContent = '';
    setBusy(busy, '上限を再取得');
    const checked = d ? new Date(d.checkedAt || l.fetchedAt) : null;
    const direct = d?.providers.some((p) => p.source === 'antigravity-api');
    foot.textContent = `Claude / Codex の上限は各 CLI の公式インターフェース経由で取得しています（認証ファイルの直接読取なし）。${direct ? 'Antigravity は Antigravity API へ直接照会しています。' : ''}テレメトリ送信はありません${checked ? ` · 最終確認 ${checked.toLocaleTimeString('ja-JP', { hour12: false })}` : ''}`;
    // countdowns have minute resolution, so an unchanged minute needs no rebuild
    const state = [d, l.error, u.metric, !d && busy, l.redeeming, Math.floor(Date.now() / 60000)];
    if (state.every((v, i) => v === drawn[i])) return;
    drawn = state;
    chart = null;
    const focused = top.contains(document.activeElement) ? document.activeElement.dataset.win : null;
    const open = popover && top.contains(popover.anchor) ? { win: popover.anchor.dataset.win, focus: popover.el.contains(document.activeElement) } : null;
    if (open) closePopover();
    const banner = l.error && (d || !busy)
      ? errorBanner(`上限の${d ? '更新' : '取得'}に失敗しました: ${l.error}${d ? '（前回の結果を表示中）' : ''}`, () => loadUsageLimits({ refresh: true })) : null;
    top.replaceChildren(...[banner, d ? limitsView(d) : banner ? null : limitsSkeleton()].filter(Boolean));
    const barOf = (win) => top.querySelector(`.u-lwin-bar[data-win="${CSS.escape(win)}"]`);
    if (focused) barOf(focused)?.focus();
    // an open detail panel follows its window into the rebuilt view, with the new numbers
    const anchor = open && barOf(open.win);
    if (anchor) {
      const p = d.providers.find((x) => open.win.startsWith(`${x.id}:`) && x.windows.length);
      const w = p?.windows.find((x) => open.win === `${p.id}:${x.id || x.label}`);
      if (w) openLimitPopover(anchor, p, w, { focus: open.focus });
    }
  };

  const renderTop = () => {
    const d = u.cache.get(u.days);
    const error = u.errors.get(u.days);
    range.textContent = d ? rangeText(d.window) : '';
    setBusy(u.pending.has(u.days), '使用量を再スキャン');
    foot.textContent = d ? `コストは API 料金からの推定です${d.scannedAt ? ` · 最終スキャン ${new Date(d.scannedAt).toLocaleString('ja-JP')}` : ''}` : '';
    const banner = error ? errorBanner(`使用量の${d ? '更新' : '取得'}に失敗しました: ${error}${d ? '（前回の結果を表示中）' : ''}`, () => loadUsage(u.days)) : null;
    // starting a re-fetch changes nothing here; rebuilding would drop the tooltip under the pointer
    const state = [d, error, u.metric, [...u.hidden].join()];
    if (state.every((v, i) => v === drawn[i])) return;
    drawn = state;
    chart = null;
    if (!d) { top.replaceChildren(banner || usageSkeleton()); return; }
    const view = usageView(d);
    chart = usageChart(view);
    const focused = top.contains(document.activeElement) ? document.activeElement.dataset.provider : null;
    const title = `${view.hourly ? '時間別' : '日別'}${u.metric === 'cost' ? 'コスト' : '処理トークン'}`;
    top.replaceChildren(...[banner,
      h('section', { class: 'u-hero' }, usageSummary(view, () => { renderTop(); renderBreakdown(); }),
        h('div', { class: 'u-main' }, h('h3', { text: title }), chart.el)),
      ...usageTotals(view)].filter(Boolean));
    if (focused) top.querySelector(`.u-prov[data-provider="${CSS.escape(focused)}"]`)?.focus();
  };
  const render = () => {
    const limits = inLimits();
    period.hidden = limits;
    breakdown.hidden = limits;
    if (!limits && popover && top.contains(popover.anchor)) closePopover();
    if (limits) renderLimits(); else { renderTop(); renderBreakdown(); }
  };
  const onchange = (days) => { if (days === u.days && !inLimits()) render(); };
  const onlimits = () => { if (inLimits()) render(); };
  const selectMetric = (v) => {
    u.metric = v;
    render();
    if (v === 'limits') loadUsageLimits(); else if (!u.cache.has(u.days)) loadUsage(u.days);
  };

  const head = (close) => h('div', { class: 'u-head' },
    h('div', { class: 'u-title' }, h('h2', { text: '使用量' }), range),
    h('div', { class: 'u-controls' },
      period = segmented(U_PERIODS, String(u.days), (v) => { u.days = Number(v); render(); loadUsage(u.days); }, { label: '期間', cls: 'mini u-period' }),
      segmented(U_METRICS, u.metric, selectMetric, { label: '指標', cls: 'mini u-metric' })),
    h('div', { class: 'u-actions' }, refresh, ibtn('close', '閉じる (Esc)', close, 'sm u-close')));

  openModal('使用量', (body, _close, dlg) => {
    breakdown = h('section', { class: 'u-section u-breakdown' }, h('div', { class: 'u-bhead' }, h('h3', { text: '内訳' }), tabs), panel);
    body.append(top, breakdown, foot);
    dlg.addEventListener('pointerdown', (e) => { if (chart && !chart.plot.contains(e.target)) chart.hide(); });
    body.addEventListener('scroll', () => popover?.place(), { passive: true });
    dlg.tabIndex = -1; dlg.focus();
    u.onchange = onchange;
    l.onchange = onlimits;
    render();
    if (inLimits()) loadUsageLimits(); else loadUsage(u.days);
    if (tab === 'system') system.ready.then(() => tabs.parentElement.scrollIntoView({ block: 'start' })); // deep link from the stat pill
  }, { cls: 'usage', head, onclose: () => { if (u.onchange === onchange) u.onchange = null; if (l.onchange === onlimits) l.onchange = null; } });
}

// ───────────────────────── usage: DB / privacy tab ─────────────────────────
const scard = (l, v) => h('div', { class: 'scard' }, h('div', { class: 'sl', text: l }), h('div', { class: 'sv', text: v }));

function systemPanel() {
  const el = h('div', { class: 'u-system' }, h('div', { class: 'loading', text: '読み込み中…' }));
  let seq = 0;
  const load = async () => {
    const my = ++seq;
    const [meta, orch, db] = await Promise.allSettled([api('/api/meta'), api('/api/stats/orch'), api('/api/stats/db')]);
    if (my !== seq) return;
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
    out.push(h('div', { class: 'sec-h' }, h('span', { text: 'orch-stats（プロバイダ / モデル別）' }), h('button', { class: 'btn xs', type: 'button', text: '更新', onclick: load })));
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
    el.replaceChildren(...out);
    renderStatPill();
  };
  return { el, load, ready: load() };
}

// ───────────────────────── context panel (T3 Code right sidebar) ─────────────────────────
function threadContextInfo() {
  const t = curThread();
  if (!t) return null;
  const sel = S.compose.sel || t.model_selection;
  const spec = findModelSpec(sel);
  const ctx = S.detail?.context || null;
  const live = S.liveUsage.get(t.id) || null;
  const sameModel = sel && t.model_selection && sel.provider === t.model_selection.provider && sel.model === t.model_selection.model;
  // The server's window wins for the thread's own model: it carries the size the provider
  // itself reported (Codex `model_context_window`), which can differ from the catalog.
  const windowSize = (sameModel ? Number(ctx?.context_window) : 0)
    || Number(spec?.context_window)
    || fallbackContextWindow(sel?.provider, sel?.model);

  const runs = S.detail?.runs || [];
  const totalInput = ctx ? Number(ctx.total_input_tokens) || 0 : runs.reduce((a, r) => a + (Number(r.input_tokens) || 0), 0);
  const totalOutput = ctx ? Number(ctx.total_output_tokens) || 0 : runs.reduce((a, r) => a + (Number(r.output_tokens) || 0), 0);
  const totalDuration = ctx ? Number(ctx.total_duration_ms) || 0 : runs.reduce((a, r) => a + (Number(r.duration_ms) || 0), 0);
  const totalRawBytes = ctx ? Number(ctx.total_raw_bytes) || 0 : (S.detail?.items || []).reduce((a, it) => a + (Number(it.raw_bytes) || 0), 0);

  let source = ctx?.source || 'empty';
  let used = Number(ctx?.used_tokens) || 0;
  let input = Number(ctx?.input_tokens) || 0;
  let output = Number(ctx?.output_tokens) || 0;
  let cacheRead = Number(ctx?.cache_read_tokens) || 0;
  let cacheWrite = Number(ctx?.cache_write_tokens) || 0;
  if (isBusy(t.status) && live && live.context_tokens > 0) {
    source = 'live';
    used = Number(live.context_tokens) || 0;
    input = Number(live.input_tokens) || 0;
    output = Number(live.output_tokens) || 0;
    cacheRead = Number(live.cache_read_tokens) || 0;
    cacheWrite = Number(live.cache_write_tokens) || 0;
  }

  const pct = windowSize > 0 ? Math.min(100, (used / windowSize) * 100) : 0;
  const level = pct >= 90 ? 'crit' : pct >= 75 ? 'warn' : used > 0 ? 'ok' : 'idle';
  const remaining = Math.max(0, windowSize - used);
  const promptTok = input + cacheRead + cacheWrite;
  const cacheHit = promptTok > 0 && cacheRead > 0 ? cacheRead / promptTok : null;
  const breakdown = Array.isArray(ctx?.items_breakdown) ? ctx.items_breakdown : [];
  return {
    thread: t, sel, windowSize, used, pct, level, remaining, source,
    input, output, cacheRead, cacheWrite, cacheHit,
    totalInput, totalOutput, totalDuration, totalRawBytes, breakdown,
    runsCount: runs.length, compaction: ctx?.compaction || null,
  };
}

function renderContextMeters() {
  const info = threadContextInfo();
  const meter = $('#btn-ctx-meter');
  const tbRing = $('#tb-ctx-ring');
  const tbLabel = $('#tb-ctx-label');
  const tbBtn = $('#btn-context');
  if (!info) {
    if (meter) meter.hidden = true;
    setContextRing(tbRing, 0, 'idle', 14, 2);
    if (tbLabel) tbLabel.textContent = 'コンテキスト';
    if (tbBtn) { tbBtn.classList.remove('l-warn', 'l-crit'); tbBtn.title = `コンテキスト・スレッド詳細 (${kbd('I')})`; }
    return;
  }
  const pctText = fmtPctCompact(info.pct);
  const srcTag = info.source === 'estimated' ? '（推定）' : '';
  const tip = `コンテキストウィンドウ: ${pctText}${srcTag}\n${fmtCompact(info.used)} / ${fmtCompact(info.windowSize)} tok（残り ${fmtCompact(info.remaining)}）\nクリックで右側の詳細パネルを開閉 (${kbd('I')})`;
  if (meter) {
    meter.hidden = false;
    meter.className = `ctx-meter thread-only l-${info.level}`;
    meter.title = tip;
    meter.setAttribute('aria-label', `コンテキスト使用率 ${pctText}`);
    meter.setAttribute('aria-pressed', String(shownRail() === 'context'));
    setContextRing(meter, info.pct, info.level, 16, 2.2);
    let pctEl = meter.querySelector('.ctx-meter-pct');
    if (!pctEl) { pctEl = h('span', { class: 'ctx-meter-pct' }); meter.append(pctEl); }
    pctEl.textContent = pctText;
  }
  setContextRing(tbRing, info.pct, info.level, 14, 2);
  if (tbLabel) tbLabel.textContent = info.used > 0 ? pctText : 'コンテキスト';
  if (tbBtn) {
    tbBtn.classList.toggle('l-warn', info.level === 'warn');
    tbBtn.classList.toggle('l-crit', info.level === 'crit');
    tbBtn.title = tip;
  }
}

function renderContextLimits(providerId) {
  const d = S.limits.data;
  const now = Date.now();
  const id = uId(providerId);
  const matching = d?.providers.filter((p) => p.id === id && p.windows.length) || [];
  const pools = matching.length ? matching : (d?.providers.filter((p) => p.windows.length) || []);
  const headAction = h('div', { class: 'ctx-sec-acts' },
    ibtn('refresh', '上限を再取得', () => loadUsageLimits({ refresh: true }), 'xs'),
    h('button', { type: 'button', class: 'link ctx-more-link', text: 'すべて →', onclick: () => usageDialog({ metric: 'limits' }) }));
  if (!d) {
    return ctxSection('使用上限', headAction,
      h('div', { class: 'ctx-card ctx-muted-card', text: S.limits.pending ? '上限を取得中…' : '上限データがありません' }));
  }
  if (!pools.length) {
    return ctxSection('使用上限', headAction,
      h('div', { class: 'ctx-card ctx-muted-card', text: '表示できるクォータ枠がありません' }));
  }
  const rows = [];
  for (const p of pools) {
    if (!matching.length) {
      rows.push(h('div', { class: 'ctx-lprov' }, uDot(p.color), h('span', { text: p.label }), p.plan ? h('small', { text: p.plan }) : null));
    }
    for (const w of p.windows) {
      const st = limitState(w, now);
      const reset = st.upcoming ? fmtResetIn(w.resetsAt, now) : st.expired ? 'リセット済' : '';
      const fill = h('i');
      fill.style.width = w.remaining > 0 ? `max(2px, ${w.remaining}%)` : '0';
      fill.style.background = st.level === ' out' ? 'var(--error)' : st.level === ' low' ? 'var(--warning)' : p.color;
      rows.push(h('button', {
        type: 'button', class: 'ctx-lrow', title: `${p.label} ${w.label}: 残り ${fmtLeft(w.remaining)}${reset ? ` · ${reset}` : ''}`,
        onclick: () => usageDialog({ metric: 'limits' }),
      },
      h('div', { class: 'ctx-lrow-top' },
        h('span', { class: 'ctx-lrow-name', text: w.label }),
        h('span', { class: `ctx-lrow-val${st.level}`, text: `残り ${fmtLeft(w.remaining)}` })),
      h('div', { class: 'ctx-mini-bar' }, fill),
      reset ? h('div', { class: 'ctx-lrow-sub' }, h('span', { text: reset }), st.pace ? h('span', { text: st.pace.cls === 'ahead' ? '消費速め' : st.pace.cls === 'under' ? '余裕あり' : '適正ペース' }) : null) : null));
    }
  }
  const providerHead = matching[0] ? h('div', { class: 'ctx-lprov top' }, uDot(matching[0].color), h('span', { text: matching[0].label }), matching[0].plan ? h('span', { class: 'ctx-tag', text: matching[0].plan }) : null) : null;
  return ctxSection('使用上限', headAction, h('div', { class: 'ctx-card ctx-limits-card' }, providerHead, ...rows));
}

function renderContextChanges(t) {
  if (nonGitProjects.has(t.project_id)) {
    return ctxSection('変更ファイル', null,
      h('div', { class: 'ctx-card ctx-muted-card', text: 'Git リポジトリではありません' }));
  }
  const diff = S.diffData && S.diffData.threadId === t.id ? S.diffData.data : null;
  const stat = S.diffStat && S.diffStat.threadId === t.id ? S.diffStat : null;
  const err = S.diffError && S.diffError.threadId === t.id ? S.diffError : null;
  const openDiffAction = h('button', { type: 'button', class: 'link ctx-more-link', text: '差分を開く →', onclick: () => setRail('diff') });
  if (!diff && !stat) {
    if (err) {
      return ctxSection('変更ファイル', openDiffAction,
        h('div', { class: 'ctx-card ctx-muted-card' },
          h('div', { class: 'errtxt', text: `差分を取得できません: ${err.message}` }),
          h('button', { type: 'button', class: 'btn xs', text: '再試行', onclick: () => { S.diffError = null; renderContextPanel(); refreshDiffStat(); } })));
    }
    return ctxSection('変更ファイル', openDiffAction,
      h('div', { class: 'ctx-card ctx-muted-card', text: '作業ツリーを確認中…' }));
  }
  const files = diff?.files || [];
  const add = diff ? diff.total_additions : stat?.add || 0;
  const del = diff ? diff.total_deletions : stat?.del || 0;
  if (!files.length) {
    return ctxSection('変更ファイル', openDiffAction,
      h('div', { class: 'ctx-card ctx-clean' }, iconEl('check'), h('span', { text: '変更はありません（クリーン）' })));
  }
  const shown = files.slice(0, 10);
  return ctxSection('変更ファイル',
    h('div', { class: 'ctx-sec-acts' },
      h('span', { class: 'ctx-diff-nums' }, h('span', { class: 'plus', text: `+${add}` }), h('span', { class: 'minus', text: `−${del}` })),
      openDiffAction),
    h('div', { class: 'ctx-card ctx-files-card' },
      shown.map((f) => {
        const L = kindLetter(f.status);
        const slash = f.path.lastIndexOf('/');
        return h('button', {
          type: 'button', class: 'ctx-frow', title: `${f.path}\nクリックして差分を表示`,
          onclick: () => { diffOpenFiles.add(f.path); setRail('diff'); },
        },
        h('span', { class: `fk ${L}`, text: L }),
        h('span', { class: 'ctx-fpath' }, h('b', { text: f.path.slice(slash + 1) }), slash > 0 ? h('small', { text: f.path.slice(0, slash) }) : null),
        h('span', { class: 'ctx-fstat' }, f.additions ? h('span', { class: 'plus', text: `+${f.additions}` }) : null, f.deletions ? h('span', { class: 'minus', text: `−${f.deletions}` }) : null));
      }),
      files.length > shown.length ? h('button', { type: 'button', class: 'ctx-more-btn', text: `他 ${files.length - shown.length} 件を差分で表示 →`, onclick: () => setRail('diff') }) : null));
}

/** Thread title -> safe `.md` download name (falls back to the thread id prefix). */
function safeMdFilename(title, id) {
  const s = cleanTitle(title || '')
    .replace(/[\\/:*?"<>|\u0000-\u001f]/g, '').trim()
    .replace(/\s+/g, '-').replace(/-+/g, '-').slice(0, 80).replace(/^[.-]+|[.-]+$/g, '');
  return `${s || `thread-${String(id).slice(0, 8)}`}.md`;
}

/** GET /api/threads/:id/export (server redacts credentials) and download it as Markdown. */
async function downloadThreadMarkdown(t) {
  if (!t) return;
  try {
    const md = await api(`/api/threads/${t.id}/export`);
    const url = URL.createObjectURL(new Blob([md], { type: 'text/markdown; charset=utf-8' }));
    const a = h('a', { href: url, download: safeMdFilename(t.title, t.id) });
    document.body.append(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 5000);
    toast('Markdown をダウンロードしました', 'ok', 2000);
  } catch (e) {
    toast(`Markdown 出力に失敗: ${e.message}`, 'err');
  }
}

/** GET /api/threads/:id/export and copy the Markdown to the clipboard. */
async function copyThreadMarkdown(t) {
  if (!t) return;
  try {
    const md = await api(`/api/threads/${t.id}/export`);
    await copyText(md, 'Markdown をコピーしました');
  } catch (e) {
    toast(`Markdown コピーに失敗: ${e.message}`, 'err');
  }
}

function renderContextPanel() {
  const body = $('#context-body');
  if (!body) return;
  const info = threadContextInfo();
  if (!info) {
    const proj = projectById(S.projectId);
    const activeCount = S.threads.filter((x) => !x.archived && (!proj || x.project_id === proj.id)).length;
    const sel = S.compose.sel || defaultSelection(S.projectId);
    const win = Number(findModelSpec(sel)?.context_window) || fallbackContextWindow(sel?.provider, sel?.model);
    const parts = [
      ctxSection('新規スレッド', null, h('div', { class: 'ctx-card ctx-home-card' },
        h('div', { class: 'ctx-home-row' }, provDot(sel?.provider), h('b', { text: modelLabel(sel).replace(/^[\w-]+\//, '') }), h('span', { class: 'ctx-tag', text: `上限 ${fmtCompact(win)}` })),
        proj ? h('div', { class: 'ctx-kv-list' },
          h('div', { class: 'ctx-kv' }, h('span', { text: 'プロジェクト' }), h('b', { text: proj.title })),
          h('div', { class: 'ctx-kv' }, h('span', { text: 'スレッド数' }), h('b', { text: `${fmtNum(activeCount)} 件` })),
          h('div', { class: 'ctx-kv' }, h('span', { text: 'パス' }), h('button', { type: 'button', class: 'ctx-copy-val mono', title: 'クリックでコピー', text: compactPath(proj.workspace_root), onclick: () => copyText(proj.workspace_root, 'パスをコピーしました') })))
          : h('p', { class: 'muted', text: 'プロジェクトを選択または追加して開始してください' }))),
      renderContextLimits(sel?.provider),
    ];
    if (S.providers.length) {
      parts.push(ctxSection('プロバイダ', null, h('div', { class: 'ctx-card ctx-provs-card' },
        S.providers.map((p) => h('div', { class: `ctx-prov-row${p.available ? '' : ' off'}`, title: p.error || p.binary_path || '' },
          provDot(p.provider), h('span', { class: 'ctx-prov-n', text: provLabel(p.provider) }),
          h('small', { text: p.available ? (p.version || '利用可能') : '未導入' }))))));
    }
    body.replaceChildren(...parts);
    return;
  }

  const { thread: t, sel, windowSize, used, pct, level, remaining, source, input, output, cacheRead, cacheWrite, cacheHit, totalInput, totalOutput, totalDuration, totalRawBytes, breakdown, runsCount, compaction } = info;
  const estCost = Number(S.detail?.context?.estimated_cost_usd) || 0;
  const project = projectById(t.project_id);
  const parts = [];

  // ── 1. Context Window Card (hero)
  const segs = [];
  if (used > 0 && windowSize > 0) {
    const hasTokenSplit = source !== 'estimated' && (input || output || cacheRead || cacheWrite);
    if (hasTokenSplit) {
      const total = Math.max(used, input + output + cacheRead + cacheWrite);
      const scale = Math.min(100, (used / windowSize) * 100) / total;
      for (const [val, label, color] of [
        [cacheRead, 'キャッシュ読込', 'color-mix(in srgb, var(--accent) 55%, var(--text-muted))'],
        [cacheWrite, 'キャッシュ書込', 'color-mix(in srgb, var(--accent) 78%, var(--text))'],
        [input, '新規入力', 'var(--accent)'],
        [output, '出力', 'var(--success)'],
      ]) {
        if (val > 0) segs.push({ pct: val * scale, title: `${label}: ${fmtCompact(val)} tok`, color });
      }
    } else {
      const col = level === 'crit' ? 'var(--error)' : level === 'warn' ? 'var(--warning)' : 'var(--accent)';
      segs.push({ pct: Math.min(100, pct), title: `推定コンテキスト: ${fmtCompact(used)} tok`, color: col });
    }
  }
  const barTrack = h('div', { class: 'ctx-bar', 'aria-hidden': 'true' }, segs.map((s) => {
    const el = h('i', { title: s.title });
    el.style.width = `${Math.max(0.8, s.pct)}%`;
    el.style.background = s.color;
    return el;
  }));

  const statsGrid = h('div', { class: 'ctx-stats-grid' },
    h('div', { class: 'ctx-stat', title: `直近ステップ入力: ${fmtNum(input)} tok` },
      h('span', { text: source === 'estimated' ? '推定トークン' : '新規入力' }),
      h('b', { text: fmtCompact(input) })),
    h('div', { class: 'ctx-stat', title: `直近ステップ出力: ${fmtNum(output)} tok` },
      h('span', { text: '出力' }),
      h('b', { text: fmtCompact(output) })),
    cacheRead > 0 || cacheWrite > 0
      ? h('div', { class: 'ctx-stat', title: `キャッシュ読込: ${fmtNum(cacheRead)} / 書込: ${fmtNum(cacheWrite)}` },
        h('span', { text: 'キャッシュ' }),
        h('b', { text: cacheHit != null ? `${fmtCompact(cacheRead)} (${Math.round(cacheHit * 100)}%)` : fmtCompact(cacheRead + cacheWrite) }))
      : h('div', { class: 'ctx-stat', title: `スレッド累計入力 ${fmtNum(totalInput)} / 出力 ${fmtNum(totalOutput)}` },
        h('span', { text: 'スレッド累計' }),
        h('b', { text: totalInput || totalOutput ? `↑${fmtCompact(totalInput)} ↓${fmtCompact(totalOutput)}` : '–' })),
    h('div', { class: 'ctx-stat', title: `実行回数 ${runsCount} 回 / 総所要時間 ${fmtDur(totalDuration)}` },
      h('span', { text: 'ターン / 所要' }),
      h('b', { text: `${fmtNum(runsCount)} · ${totalDuration ? fmtDur(totalDuration) : '–'}` })),
    estCost > 0
      ? h('div', { class: 'ctx-stat', title: `スレッド累計の推定コスト: ${fmtUsd(estCost)}` },
        h('span', { text: '推定コスト' }),
        h('b', { text: fmtUsd(estCost) }))
      : null);

  const warnBanner = level === 'crit'
    ? h('div', { class: 'ctx-alert crit' }, iconEl('alert'), h('span', { text: 'コンテキスト上限間近です。分岐 (Fork) して引き継ぐと安定します。' }))
    : level === 'warn'
      ? h('div', { class: 'ctx-alert warn' }, iconEl('alert'), h('span', { text: 'コンテキスト使用率が 75% を超えています。' }))
      : null;

  parts.push(h('section', { class: 'ctx-card ctx-hero-card' },
    h('div', { class: 'ctx-hero-top' },
      h('div', { class: 'ctx-big-ring' },
        contextRingSvg(pct, level, 58, 4.5),
        h('span', { class: `ctx-big-pct l-${level}`, text: fmtPctCompact(pct) })),
      h('div', { class: 'ctx-hero-meta' },
        h('div', { class: 'ctx-hero-label' },
          h('span', { text: 'コンテキストウィンドウ' }),
          h('span', { class: `ctx-src s-${source}`, title: source === 'estimated' ? 'プロバイダからのトークン報告がないため保存データ量から概算しています' : 'プロバイダ報告に基づく値', text: CTX_SOURCE_LABEL[source] || source })),
        h('div', { class: 'ctx-hero-nums', title: `${fmtNum(used)} / ${fmtNum(windowSize)} tok` },
          h('b', { text: fmtCompact(used) }), h('span', { text: ` / ${fmtCompact(windowSize)}` })),
        h('div', { class: 'ctx-hero-rem', text: `残り ${fmtCompact(remaining)} tok` }))),
    barTrack,
    warnBanner,
    statsGrid,
    h('div', { class: 'ctx-quick-acts' },
      h('button', { type: 'button', class: 'btn xs', onclick: forkDialog }, iconEl('fork'), h('span', { text: '文脈を分岐' })),
      h('button', { type: 'button', class: 'btn xs', onclick: () => usageDialog('model') }, iconEl('chart'), h('span', { text: '使用量詳細' }))),
    h('div', { class: 'ctx-quick-acts' },
      h('button', { type: 'button', class: 'btn xs', onclick: () => downloadThreadMarkdown(t) }, iconEl('file'), h('span', { text: 'Markdown 出力' })),
      h('button', { type: 'button', class: 'btn xs', onclick: () => copyThreadMarkdown(t) }, iconEl('copy'), h('span', { text: 'MD コピー' })))));

  // ── 1b. Compaction recommendation (Tier 4 Item 4)
  {
    const util = Number(compaction?.utilization_pct);
    const worth = compaction?.worth_compacting === true;
    if (compaction && (compaction.recommended === true || (util >= 80 && worth))) {
      const be = Number(compaction.break_even_turns);
      parts.push(ctxSection('コンパクション推奨', null,
        h('div', { class: 'ctx-card ctx-compact-card' },
          h('div', { class: 'ctx-alert warn' }, iconEl('alert'), h('span', { text: 'コンテキスト使用率が高くなっています。要約して新しいスレッドへ分岐すると、以降のターンが軽くなります。' })),
          h('div', { class: 'ctx-compact-meta' },
            h('span', { text: Number.isFinite(util) ? `使用率 ${Math.round(util)}%` : '使用率 —' }),
            Number.isFinite(be) ? h('span', { text: `約 ${Math.round(be)} ターンで回収` }) : null),
          h('div', { class: 'ctx-quick-acts' },
            h('button', { type: 'button', class: 'btn xs', onclick: () => forkDialog() }, iconEl('fork'), h('span', { text: '要約して新スレッドへ分岐' }))))));
    }
  }

  // ── 1c. Worktree child: parent diff & merge (Tier 4 Item 2)
  {
    const parent = t.parent_thread_id ? threadById(t.parent_thread_id) : null;
    if (isWorktreeChild(t, parent)) {
      parts.push(ctxSection('親スレッドとの差分・マージ', null,
        h('div', { class: 'ctx-card' },
          h('div', { class: 'ctx-wt-row' },
            h('span', { class: 'muted', text: `親: ${parent ? cleanTitle(parent.title) : t.parent_thread_id.slice(0, 8)} · ⎇ ${t.branch || ''}` })),
          h('div', { class: 'ctx-quick-acts' },
            h('button', { type: 'button', class: 'btn xs', text: '親との差分', onclick: () => showParentDiff(t.parent_thread_id, t.id) }),
            mergeConfirmControl(t.parent_thread_id, t, 'btn xs')))));
    }
  }

  // ── 2. Context Breakdown (items composition)
  if (breakdown.length && totalRawBytes > 0) {
    const byKind = new Map(breakdown.map((b) => [b.kind, b]));
    const rows = CTX_KIND_META
      .map(([kind, label, color]) => {
        const b = byKind.get(kind);
        return b && b.count > 0 ? { kind, label, color, count: Number(b.count), bytes: Number(b.raw_bytes) } : null;
      })
      .filter(Boolean)
      .sort((a, b) => b.bytes - a.bytes);
    if (rows.length) {
      const stackBar = h('div', { class: 'ctx-stackbar', 'aria-hidden': 'true' }, rows.filter((r) => r.bytes > 0).map((r) => {
        const seg = h('i', { title: `${r.label}: ${fmtBytes(r.bytes)} (${fmtShare(r.bytes / totalRawBytes)})` });
        seg.style.flexGrow = String(r.bytes / totalRawBytes);
        seg.style.background = r.color;
        return seg;
      }));
      parts.push(ctxSection('コンテキスト内訳',
        h('span', { class: 'ctx-sec-meta', text: `${fmtNum(S.detail?.total_items || 0)} 項目 · ${fmtBytes(totalRawBytes)}` }),
        h('div', { class: 'ctx-card ctx-break-card' },
          stackBar,
          h('ul', { class: 'ctx-break-list' }, rows.map((r) => h('li', {},
            uDot(r.color),
            h('span', { class: 'ctx-bl-name', text: r.label }),
            h('span', { class: 'ctx-bl-cnt', text: `${fmtNum(r.count)}` }),
            h('span', { class: 'ctx-bl-sz', text: fmtBytes(r.bytes) }),
            h('span', { class: 'ctx-bl-pct', text: fmtShare(r.bytes / totalRawBytes) })))))));
    }
  }

  // ── 3. Provider Quota Limits
  parts.push(renderContextLimits(sel?.provider));

  // ── 4. Git Changes Summary
  parts.push(renderContextChanges(t));

  // ── 5. Thread Details / Environment
  const kv = [];
  kv.push(['モデル', h('button', {
    type: 'button', class: 'ctx-val-btn', title: 'モデルを変更',
    onclick: () => openModelPopover($('#btn-model')),
  }, provDot(sel?.provider), h('span', { text: `${modelLabel(sel).replace(/^[\w-]+\//, '')}${sel?.effort ? ` · ${sel.effort}` : ''}` }))]);
  kv.push(['実行権限', h('span', { class: 'ctx-val-inline' },
    h('span', { class: t.runtime_mode === 'full-access' ? 'ctx-warn-txt' : '', text: MODE_LABEL[t.runtime_mode] || t.runtime_mode }),
    t.interaction_mode === 'plan' ? h('span', { class: 'ctx-tag accent', text: 'Plan' }) : null)]);
  if (project) {
    kv.push(['プロジェクト', h('span', { text: project.title })]);
    kv.push(['ワークスペース', h('button', { type: 'button', class: 'ctx-copy-val mono', title: `${project.workspace_root}（クリックでコピー）`, text: compactPath(project.workspace_root), onclick: () => copyText(project.workspace_root, 'パスをコピーしました') })]);
  }
  if (t.branch) kv.push(['ブランチ', h('span', { class: 'mono', text: `⎇ ${t.branch}` })]);
  if (t.worktree_path) kv.push(['Worktree', h('button', { type: 'button', class: 'ctx-copy-val mono', title: `${t.worktree_path}（クリックでコピー）`, text: compactPath(t.worktree_path), onclick: () => copyText(t.worktree_path, 'Worktree パスをコピーしました') })]);
  if (t.provider_session_id) {
    kv.push(['セッション ID', h('button', { type: 'button', class: 'ctx-copy-val mono', title: `${t.provider_session_id}（クリックでコピー）`, text: clip(t.provider_session_id, 22), onclick: () => copyText(t.provider_session_id, 'セッション ID をコピーしました') })]);
  }
  kv.push(['更新', h('span', { title: new Date(t.updated_at).toLocaleString('ja-JP'), text: fmtAgo(t.updated_at) })]);
  parts.push(ctxSection('スレッド詳細',
    h('span', { class: `status-pill s-${t.status}`, text: statusLabel(t.status) }),
    h('div', { class: 'ctx-card ctx-kv-list' }, kv.map(([k, v]) => h('div', { class: 'ctx-kv' }, h('span', { text: k }), v)))));

  // ── 6. Related threads (parent / children / attachments)
  const parent = t.parent_thread_id ? threadById(t.parent_thread_id) : null;
  const kids = S.detail?.children || [];
  if (t.parent_thread_id || kids.length) {
    const rel = [];
    if (t.parent_thread_id) {
      rel.push(h('button', { type: 'button', class: 'ctx-rel-row', onclick: () => openThread(t.parent_thread_id) },
        iconEl('handoff'), h('span', { class: 'ctx-rel-tag', text: '親' }),
        h('span', { class: 'ctx-rel-title', text: parent ? cleanTitle(parent.title) : t.parent_thread_id.slice(0, 8) })));
    }
    for (const k of kids) {
      rel.push(h('button', { type: 'button', class: 'ctx-rel-row', onclick: () => openThread(k.id) },
        provDot(k.model_selection?.provider, k.status === 'running' ? 'live' : ''),
        h('span', { class: 'ctx-rel-title', text: cleanTitle(k.title) }),
        h('small', { text: statusLabel(k.status) })));
    }
    parts.push(ctxSection(`関連スレッド (${rel.length})`,
      h('button', { type: 'button', class: 'link ctx-more-link', text: '+ 委譲', onclick: delegateDialog }),
      h('div', { class: 'ctx-card ctx-rel-card' }, rel)));
  }

  const atts = S.detail?.attachments || [];
  if (atts.length) {
    parts.push(ctxSection(`添付ファイル (${atts.length})`, null,
      h('div', { class: 'ctx-card ctx-rel-card' }, atts.map((a) => h('button', {
        type: 'button', class: 'ctx-rel-row',
        onclick: () => openAttachment(a.id, a.filename, /^image\//.test(a.mime_type || '')),
      }, iconEl('clip'), h('span', { class: 'ctx-rel-title', text: a.filename }), h('small', { text: fmtBytes(a.size_bytes) }))))));
  }

  const scrollTop = body.scrollTop;
  body.replaceChildren(...parts);
  body.scrollTop = scrollTop;
}
const renderContextPanelSoon = rafThrottle(() => { if (shownRail() === 'context') renderContextPanel(); });

// ───────────────────────── diff panel ─────────────────────────
let diffSeq = 0;
let diffShownFor = null;
const diffOpenFiles = new Set();

/** The panel actually on screen: the Swarm board only shows for a thread that has one. */
const shownRail = () => (S.rail === 'swarm' && !curBoard() ? null : S.rail);
function renderRail() {
  const shown = shownRail();
  const hasSwarm = !!curBoard();
  $('#context-panel').hidden = shown !== 'context';
  $('#diff-panel').hidden = shown !== 'diff';
  $('#swarm-panel').hidden = shown !== 'swarm';
  const resizer = $('#rail-resizer');
  if (resizer) resizer.hidden = !shown;
  $('#app').dataset.rail = shown || 'none';
  $('#btn-context').setAttribute('aria-pressed', String(shown === 'context'));
  $('#btn-ctx-meter')?.setAttribute('aria-pressed', String(shown === 'context'));
  $('#btn-diff').setAttribute('aria-pressed', String(shown === 'diff'));
  $('#btn-swarm').setAttribute('aria-pressed', String(shown === 'swarm'));
  $$('.rail-tab').forEach((tab) => {
    const target = tab.dataset.railTab;
    if (target === 'swarm') tab.hidden = !hasSwarm;
    const on = shown === target;
    tab.classList.toggle('on', on);
    tab.setAttribute('aria-selected', String(on));
  });
}
function setRail(name) {
  S.rail = name;
  ls.set('t3.rail', name || 'none');
  renderRail();
  if (name === 'diff') loadDiff();
  else if (name === 'context') {
    renderContextPanel();
    if (S.threadId && (!S.diffData || S.diffData.threadId !== S.threadId)) refreshDiffStat();
  }
}
const toggleDiff = () => setRail(shownRail() === 'diff' ? null : 'diff');
const toggleContext = () => setRail(shownRail() === 'context' ? null : 'context');

function renderDiffBadge() {
  const b = $('#diff-badge');
  const d = S.diffStat;
  const dirty = !!d && d.threadId === S.threadId && d.files > 0;
  b.hidden = !dirty;
  if (dirty) b.replaceChildren(h('span', { class: 'plus', text: `+${d.add}` }), h('span', { class: 'minus', text: `−${d.del}` }));
  const tabBadge = $('#ctx-tab-diff-badge');
  if (tabBadge) {
    tabBadge.hidden = !dirty;
    if (dirty) tabBadge.textContent = String(d.files);
  }
}
const nonGitProjects = new Set();
const fetchDiff = (id) => api(`/api/threads/${id}/diff?ignore_whitespace=${S.ignoreWs}`);
function noteDiffStat(threadId, d) {
  S.diffStat = { threadId, add: d.total_additions, del: d.total_deletions, files: d.clean ? 0 : d.files.length };
  S.diffData = { threadId, data: d };
  S.diffError = null;
  renderDiffBadge();
  if (shownRail() === 'context' && threadId === S.threadId) renderContextPanel();
}
async function refreshDiffStat() {
  const id = S.threadId; if (!id) return;
  const project = curThread()?.id === id ? curThread().project_id : null;
  if (project && nonGitProjects.has(project)) return;
  try { const d = await fetchDiff(id); if (id === S.threadId) noteDiffStat(id, d); } catch (e) {
    if (project && /not a git repository/i.test(e.message)) {
      nonGitProjects.add(project); // no badge, and no repeat requests
    } else if (id === S.threadId) {
      S.diffError = { threadId: id, message: e.message || String(e) };
    }
    if (shownRail() === 'context' && id === S.threadId) renderContextPanel();
  }
}

function syncDiffScopeButtons() {
  $$('#diff-scope button').forEach((b) => {
    const on = b.dataset.scope === S.diffScope;
    b.classList.toggle('on', on);
    b.setAttribute('aria-checked', String(on));
  });
}

function renderDiffView(d) {
  const t = curThread();
  const body = $('#diff-body'); const sum = $('#diff-sum'); const branch = $('#diff-branch');
  syncDiffScopeButtons();
  branch.textContent = d.branch ? `⎇ ${d.branch}` : '';
  const pd = S.parentDiff && S.parentDiff.data === d ? S.parentDiff : null;
  const banner = pd ? h('div', { class: 'dp-parent-banner' }, iconEl('fork'),
    h('span', { class: 'dp-parent-label', text: `親スレッドとの差分（${pd.branch || d.branch || ''}）` }),
    h('span', { class: 'grow' }),
    h('button', { type: 'button', class: 'btn xs', text: '通常の差分に戻る', onclick: () => { S.parentDiff = null; loadDiff(); } })) : null;
  const latestOnly = S.diffScope === 'latest';
  let files = d.files || [];
  if (latestOnly && t && !pd) {
    const turnPaths = latestTurnChangedPaths(S.detail, projectById(t.project_id)?.workspace_root || '');
    files = filterDiffFilesByPaths(files, turnPaths);
  }
  const add = files.reduce((a, f) => a + (Number(f.additions) || 0), 0);
  const del = files.reduce((a, f) => a + (Number(f.deletions) || 0), 0);
  const risk = d.risk && !d.clean && files.length ? d.risk : null;
  const riskMeta = risk?.tier === 'no-undo'
    ? { cls: 'bad', label: 'No Undo · 人間承認必須', bannerCls: 'no-undo', text: '取り消し困難な変更（マイグレーション・機密設定・デプロイ等）を含みます。エージェントによる自動マージはブロックされます。人間が確認のうえマージしてください。' }
    : risk?.tier === 'cleanup'
      ? { cls: 'warn', label: 'Cleanup · 要レビュー', bannerCls: 'cleanup', text: '認証・スキーマ・依存設定または広範な変更を含みます。テスト緑＋別視点レビューを推奨します。' }
      : risk?.tier === 'one-click'
        ? { cls: 'good', label: 'One-Click', bannerCls: null, text: '' }
        : null;
  const riskTitle = risk
    ? [...(risk.reasons || []), ...(risk.flagged_paths?.length ? [`対象: ${risk.flagged_paths.join(', ')}`] : [])].join('\n')
    : '';
  sum.replaceChildren(
    ...[
      h('span', { class: 'sbadge plus', text: `+${add}` }),
      h('span', { class: 'sbadge minus', text: `−${del}` }),
      h('span', { class: 'muted', text: `${files.length} ファイル` }),
      riskMeta ? h('span', { class: `sbadge ${riskMeta.cls}`, text: riskMeta.label, title: riskTitle || riskMeta.label }) : null,
    ].filter(Boolean),
  );
  const riskBanner = riskMeta?.bannerCls ? h('div', { class: `dp-risk-banner ${riskMeta.bannerCls}` },
    h('div', { class: 'dp-risk-head', text: riskMeta.label }),
    h('div', { class: 'dp-risk-text', text: riskMeta.text }),
    risk.flagged_paths?.length
      ? h('div', { class: 'dp-risk-paths mono', text: risk.flagged_paths.join(', ') })
      : null,
  ) : null;
  if (d.clean || !d.files.length) {
    body.replaceChildren(...[banner, h('div', { class: 'dp-empty' }, iconEl('check'), h('span', { text: '変更はありません（クリーン）' }))].filter(Boolean));
    return;
  }
  if (!files.length && latestOnly) {
    body.replaceChildren(...[banner, h('div', { class: 'dp-empty' },
      iconEl('file'),
      h('span', { text: '最新ターンで変更されたファイルはありません' }),
      h('button', { type: 'button', class: 'btn xs', text: `全 ${d.files.length} ファイルを表示`, onclick: () => { S.diffScope = 'all'; ls.set('t3.diffScope', 'all'); renderDiffView(d); } }))].filter(Boolean));
    return;
  }
  const top = body.scrollTop;
  body.replaceChildren(...[banner, riskBanner, ...files.map(diffFile)].filter(Boolean));
  if (!pd) body.append(diffCommitForm(t));
  body.scrollTop = top;
}

async function loadDiff() {
  const t = curThread();
  const body = $('#diff-body'); const sum = $('#diff-sum'); const branch = $('#diff-branch');
  const my = ++diffSeq;
  syncDiffScopeButtons();
  if (!t) { diffShownFor = null; body.replaceChildren(h('div', { class: 'dp-empty', text: 'スレッドを選択すると、作業ツリーの差分が表示されます' })); sum.replaceChildren(); branch.textContent = ''; return; }
  if (S.parentDiff) return; // 親との差分表示中は通常差分で上書きしない
  if (diffShownFor !== t.id) { sum.replaceChildren(); branch.textContent = ''; body.replaceChildren(h('div', { class: 'dp-empty', text: '読み込み中…' })); }
  try {
    const d = await fetchDiff(t.id);
    if (my !== diffSeq) return;
    diffShownFor = t.id;
    noteDiffStat(t.id, d);
    renderDiffView(d);
  } catch (e) {
    if (my !== diffSeq) return;
    diffShownFor = null; S.diffStat = null; renderDiffBadge();
    sum.replaceChildren(); branch.textContent = '';
    body.replaceChildren(/not a git repository/i.test(e.message)
      ? h('div', { class: 'dp-empty' }, iconEl('fork'), h('span', { text: 'このプロジェクトは Git リポジトリではありません' }))
      : h('div', { class: 'dp-empty errtxt', text: `差分を取得できません: ${e.message}` }));
  }
}
const refreshDiffSoon = debounce(() => { if (S.rail === 'diff') loadDiff(); else refreshDiffStat(); }, 600);

function diffFile(f) {
  const t = curThread();
  const L = kindLetter(f.status);
  const slash = f.path.lastIndexOf('/');
  const d = h('details', { class: 'dfile', dataset: { path: f.path } }, // collapsed by default
    h('summary', {}, iconEl('chevron', 's-chev'), h('span', { class: `fk ${L}`, text: L }),
      h('span', { class: 'fp', title: f.path }, h('b', { text: f.path.slice(slash + 1) }), slash > 0 ? h('small', { text: f.path.slice(0, slash) }) : null),
      ibtn('copy', 'パスをコピー', (e) => { e.preventDefault(); e.stopPropagation(); copyText(f.path); }, 'xs dcopy'),
      h('button', {
        class: 'btn xs dgit', type: 'button', text: 'Stage', title: 'ステージする',
        onclick: async (e) => {
          e.preventDefault(); e.stopPropagation();
          if (!t) return;
          const b = e.currentTarget; b.disabled = true;
          try {
            await api(`/api/threads/${t.id}/git/stage`, { method: 'POST', json: { path: f.path } });
            toast('ステージしました', 'ok');
            loadDiff();
          } catch (err) { toast(`ステージに失敗: ${err.message}`, 'err'); b.disabled = false; }
        },
      }),
      h('button', {
        class: 'btn xs dgit danger-ghost', type: 'button', text: '破棄', title: 'このファイルの変更を破棄する',
        onclick: async (e) => {
          e.preventDefault(); e.stopPropagation();
          if (!t) return;
          const b = e.currentTarget;
          // two-step confirm (window.confirm is unavailable in some webviews)
          if (!b.dataset.armed) { b.dataset.armed = '1'; b.textContent = '本当に破棄'; setTimeout(() => { if (!b.isConnected) return; delete b.dataset.armed; b.textContent = '破棄'; }, 3000); return; }
          b.disabled = true;
          try {
            await api(`/api/threads/${t.id}/git/discard`, { method: 'POST', json: { path: f.path } });
            toast('変更を破棄しました', 'ok');
            loadDiff();
          } catch (err) { toast(`破棄に失敗: ${err.message}`, 'err'); b.disabled = false; }
        },
      }),
      h('span', { class: 'sbadge plus', text: `+${f.additions}` }), h('span', { class: 'sbadge minus', text: `−${f.deletions}` })));
  let drawn = false;
  const draw = () => { if (d.open && !drawn) { drawn = true; d.append(patchLines(f.patch, 3000, f.path)); } };
  d.addEventListener('toggle', () => { d.open ? diffOpenFiles.add(f.path) : diffOpenFiles.delete(f.path); draw(); });
  if (diffOpenFiles.has(f.path)) { d.open = true; draw(); }
  return d;
}

/** One-line commit form shown at the end of the diff body when files changed. */
function diffCommitForm(t) {
  const inp = h('input', { class: 'inp sm dp-commit-input', type: 'text', placeholder: 'コミットメッセージ', 'aria-label': 'コミットメッセージ', autocomplete: 'off' });
  const btn = h('button', { class: 'btn sm primary', type: 'submit', text: 'コミット' });
  return h('form', {
    class: 'dp-commit',
    onsubmit: async (e) => {
      e.preventDefault();
      const message = inp.value.trim();
      if (!message) { inp.focus(); return; }
      btn.disabled = true;
      try {
        const r = await api(`/api/threads/${t.id}/git/commit`, { method: 'POST', json: { message } });
        inp.value = '';
        toast(`コミットしました (${(r?.sha || '').slice(0, 7)})`, 'ok');
        loadDiff();
      } catch (err) { toast(`コミットに失敗: ${err.message}`, 'err'); }
      btn.disabled = false;
    },
  }, inp, btn);
}

// ───────────────────────── swarm board ─────────────────────────
const SWARM_STATE = { working: '稼働中', waiting: '待機', killed: '終了' };
const curBoard = () => (S.threadId && S.swarms.get(S.threadId)) || null;
const swarmOverlay = () => matchMedia('(max-width: 1180px)').matches;
const bareName = (n) => String(n ?? '').replace(/^@/, '');
const swarmWorkers = (b) => b.members.filter((m) => m.role !== 'coordinator');

const swarmPushes = new Map();  // threadId -> count of boards pushed by WS / POST (outdates GETs in flight)
const swarmGets = new Map();    // threadId -> sequence of the latest GET
const swarmKilling = new Set(); // `${board_id}:${name}`
const swarmDrafts = new Map();  // board_id -> unsent quick post
let swarmPosting = false;
let swarmShownBoard = null;     // board_id the panel currently renders
let swarmRosterSig = '';
let swarmStick = true;          // the feed follows new posts until the reader scrolls up

function normalizeBoard(b) {
  if (!b || typeof b !== 'object' || !b.board_id) return null;
  const objects = (x) => (Array.isArray(x) ? x.filter((v) => v && typeof v === 'object') : []);
  const messages = objects(b.messages).sort((a, c) => a.seq - c.seq);
  return { ...b, members: objects(b.members), messages, head_seq: Number(b.head_seq) || Number(messages.at(-1)?.seq) || 0 };
}
/** Stores a snapshot for one thread. A snapshot older than the cached one (by head_seq) is ignored. */
function storeSwarm(threadId, board) {
  const prev = S.swarms.get(threadId);
  if (board && prev?.board_id === board.board_id && board.head_seq < prev.head_seq) return;
  if (board) S.swarms.set(threadId, board); else S.swarms.delete(threadId);
  if (threadId === S.threadId) renderSwarm();
}
/** A pushed snapshot belongs to its thread and to every other cached view of the same board. */
function pushSwarm(threadId, raw) {
  const board = normalizeBoard(raw);
  const ids = new Set([threadId]);
  if (board) for (const [id, b] of S.swarms) if (b.board_id === board.board_id) ids.add(id);
  for (const id of ids) {
    swarmPushes.set(id, (swarmPushes.get(id) || 0) + 1);
    storeSwarm(id, board);
  }
}
async function loadSwarm(threadId, { manual = false } = {}) {
  const seq = (swarmGets.get(threadId) || 0) + 1;
  swarmGets.set(threadId, seq);
  const pushes = swarmPushes.get(threadId) || 0;
  try {
    const r = await api(`/api/threads/${threadId}/swarm`);
    if (swarmGets.get(threadId) !== seq || (swarmPushes.get(threadId) || 0) !== pushes) return;
    storeSwarm(threadId, normalizeBoard(r));
  } catch (e) {
    if (manual) toast(`Swarm ボードを取得できません: ${e.message}`, 'err'); // background fetches stay quiet
  }
}
/** POST returning a snapshot. If something newer was pushed meanwhile, re-reads instead of going backwards. */
async function postSwarm(path, json) {
  const threadId = S.threadId;
  const coordinator = curBoard().coordinator_thread_id;
  const pushes = swarmPushes.get(threadId) || 0;
  const next = normalizeBoard(await api(`/api/threads/${coordinator}/swarm/${path}`, { method: 'POST', json }));
  const raced = (swarmPushes.get(threadId) || 0) !== pushes && next?.head_seq <= (S.swarms.get(threadId)?.head_seq ?? -1);
  if (!next || raced) loadSwarm(threadId); else pushSwarm(threadId, next);
}

function renderSwarmPill(b) {
  const btn = $('#btn-swarm');
  btn.hidden = !b;
  if (!b) return;
  const workers = swarmWorkers(b);
  const working = workers.filter((m) => m.state === 'working').length;
  const alive = workers.filter((m) => m.state !== 'killed').length;
  $('.swarm-count', btn).textContent = String(alive);
  $('.pulse', btn).hidden = !working;
  const label = `Swarm ボード: ワーカー ${alive}（稼働 ${working}） (${IS_MAC ? '⌥S' : 'Alt+S'})`;
  btn.title = label; btn.setAttribute('aria-label', label);
}

function swarmCard(b, m) {
  const name = bareName(m.name);
  const state = Object.hasOwn(SWARM_STATE, m.state) ? m.state : 'waiting';
  const here = m.thread_id === S.threadId;
  const open = ibtn('open', here ? `@${name} のスレッドを表示中` : `@${name} のスレッドを開く`, () => {
    if (swarmOverlay()) setRail(null); // the overlay would hide the thread that was asked for
    openThread(m.thread_id);
  }, 'sm sw-open');
  open.disabled = !m.thread_id || here;
  open.dataset.fk = `open:${name}`;
  const stop = ibtn('stop', state === 'killed' ? `@${name} は終了しています` : `@${name} を停止`, () => confirmKill(name), 'sm sw-stop');
  stop.disabled = state === 'killed' || swarmKilling.has(`${b.board_id}:${name}`);
  stop.dataset.fk = `stop:${name}`;
  return h('li', { class: `sw-card s-${state}`, dataset: { worker: name } },
    h('div', { class: 'sw-row' },
      provDot(m.provider, state === 'working' ? 'live' : ''),
      h('span', { class: 'sw-name', title: `@${name}`, text: `@${name}` }),
      h('span', { class: `sw-state s-${state}` }, state === 'working' ? h('span', { class: 'pulse', 'aria-hidden': 'true' }) : null, SWARM_STATE[state]),
      h('span', { class: 'grow' }), open, stop),
    h('div', { class: 'sw-meta' },
      h('span', { class: 'sbadge', title: `${provLabel(m.provider)} / ${m.model || ''}`, text: m.model || provLabel(m.provider) }),
      m.effort ? h('span', { class: 'sbadge', title: 'effort', text: m.effort }) : null,
      m.role && m.role !== 'worker' ? h('span', { class: 'sbadge zip', text: m.role }) : null));
}

// in-app dialog: window.confirm is unavailable in some webviews
function confirmKill(name) {
  const board = curBoard(); if (!board) return;
  openModal('ワーカーを停止', (body, close) => {
    const cancel = h('button', { type: 'button', class: 'btn', text: 'キャンセル', onclick: close });
    body.append(h('div', { class: 'form' },
      h('p', { text: `@${name} を停止しますか？ 実行中の作業は中断されます。` }),
      h('div', { class: 'actions' }, cancel,
        h('button', { type: 'button', class: 'btn danger-ghost', id: 'btn-swarm-kill-ok', text: '停止する', onclick: () => { close(); killWorker(board.board_id, name); } }))));
    setTimeout(() => cancel.focus(), 0);
  }, { cls: 'form-modal' });
}
async function killWorker(boardId, name) {
  if (curBoard()?.board_id !== boardId) return;
  const key = `${boardId}:${name}`;
  const redraw = () => { swarmRosterSig = ''; renderSwarm(); };
  swarmKilling.add(key); redraw();
  try {
    await postSwarm(`workers/${encodeURIComponent(name)}/kill`);
    toast(`@${name} の停止を要求しました`);
  } catch (e) {
    toast(`停止に失敗 (@${name}): ${e.message}`, 'err');
  } finally { swarmKilling.delete(key); redraw(); }
}

/** Post body as text nodes, with @mentions and [done] wrapped in chips. Never parses HTML. */
function swarmText(text) {
  const parts = String(text ?? '').split(/((?<![A-Za-z0-9_.])@[A-Za-z0-9_][A-Za-z0-9_-]*|\[done\])/i);
  return h('div', { class: 'sw-text' }, parts.map((part, i) => (i % 2
    ? h('span', { class: part.startsWith('@') ? 'sw-mention' : 'sw-done', text: part })
    : part)));
}
function swarmMessage(b, msg) {
  const sender = bareName(msg.sender);
  const member = b.members.find((m) => bareName(m.name) === sender);
  const kind = sender === 'coordinator' || sender === 'user' ? sender : 'worker';
  const at = new Date(msg.created_at);
  return h('article', { class: 'sw-msg', dataset: { seq: String(msg.seq) } },
    h('div', { class: 'sw-msg-head' },
      h('span', { class: 'sw-seq', text: `#${msg.seq}` }),
      h('span', { class: `sw-sender ${kind}` }, member ? provDot(member.provider) : null, h('span', { text: kind === 'worker' ? `@${sender}` : sender })),
      h('time', { dateTime: String(msg.created_at ?? ''), title: Number.isNaN(at.getTime()) ? '' : at.toLocaleString('ja-JP'), text: fmtAgo(msg.created_at) })),
    swarmText(msg.text));
}
/** Keyed by seq (posts are immutable), so a live update only appends and the log announces just the new ones. */
function renderSwarmFeed(b) {
  const feed = $('#swarm-feed');
  if (b.messages.length) $('.sw-empty', feed)?.remove();
  const have = new Map($$('.sw-msg', feed).map((el) => [el.dataset.seq, el]));
  let at = $('.sw-msg', feed);
  for (const msg of b.messages) {
    const old = have.get(String(msg.seq));
    have.delete(String(msg.seq));
    if (old && old === at) at = at.nextElementSibling;
    else feed.insertBefore(old || swarmMessage(b, msg), at);
  }
  have.forEach((el) => el.remove());
  if (!b.messages.length && !$('.sw-empty', feed)) feed.append(h('div', { class: 'sw-empty', text: 'まだ投稿がありません' }));
  if (swarmStick) feed.scrollTop = feed.scrollHeight;
}
function tickSwarmTimes() {
  if ($('#swarm-panel').hidden) return;
  $$('#swarm-feed time').forEach((el) => { el.textContent = fmtAgo(el.dateTime); });
}

const syncSwarmSend = () => { $('#btn-swarm-send').disabled = swarmPosting || !$('#swarm-input').value.trim(); };

function renderSwarm() {
  const b = curBoard();
  const hadFocus = $('#swarm-panel').contains(document.activeElement);
  renderSwarmPill(b);
  renderRail();
  syncSubscriptions();
  if (!b) { if (hadFocus) ta().focus(); return; }

  const input = $('#swarm-input');
  if (swarmShownBoard !== b.board_id) {
    swarmShownBoard = b.board_id; swarmRosterSig = ''; swarmStick = true;
    $('#swarm-feed').replaceChildren();
    input.value = swarmDrafts.get(b.board_id) || '';
  }
  const workers = swarmWorkers(b);
  const count = (s) => workers.filter((m) => m.state === s).length;
  const title = $('#swarm-title');
  title.textContent = b.title || 'Swarm ボード'; title.title = title.textContent;
  const sum = `稼働 ${count('working')} / 待機 ${count('waiting')}${count('killed') ? ` / 終了 ${count('killed')}` : ''}`;
  if ($('#swarm-sum').textContent !== sum) $('#swarm-sum').textContent = sum;
  $('#swarm-head-seq').textContent = b.head_seq ? `#${b.head_seq}` : '';

  // rebuilt only when a card would look different; keyboard focus stays on the same button
  const sig = JSON.stringify([S.threadId, workers.map((m) => [m.name, m.thread_id, m.provider, m.model, m.effort, m.state, m.role])]);
  if (sig !== swarmRosterSig) {
    swarmRosterSig = sig;
    const list = $('#swarm-roster');
    const focused = list.contains(document.activeElement) ? document.activeElement.dataset.fk : null;
    list.replaceChildren(...(workers.length ? workers.map((m) => swarmCard(b, m)) : [h('li', { class: 'sw-empty', text: 'ワーカーはまだいません' })]));
    if (focused) ($(`[data-fk="${CSS.escape(focused)}"]:not(:disabled)`, list) || $('#btn-swarm-close')).focus();
  }
  renderSwarmFeed(b);
  syncSwarmSend();
}

function toggleSwarm() {
  if (!curBoard()) { toast('このスレッドには Swarm ボードがありません。/swarm で起動できます'); return; }
  if (shownRail() === 'swarm') { closeSwarm(); return; }
  closePopover(); closeDrawer();
  setRail('swarm');
  if (swarmOverlay()) $('#btn-swarm-close').focus();
}
function closeSwarm() {
  const hadFocus = $('#swarm-panel').contains(document.activeElement);
  setRail(null);
  if (hadFocus) $('#btn-swarm').focus();
}

function insertMention(name) {
  const input = $('#swarm-input');
  const before = input.value.slice(0, input.selectionStart);
  const after = input.value.slice(input.selectionEnd);
  const token = `${before && !/\s$/.test(before) ? ' ' : ''}@${name}${/^\s/.test(after) ? '' : ' '}`;
  input.value = before + token + after;
  input.focus();
  input.setSelectionRange(before.length + token.length, before.length + token.length);
  input.dispatchEvent(new Event('input'));
}
async function sendSwarmPost() {
  const input = $('#swarm-input');
  const text = input.value.trim();
  const board = curBoard();
  if (!text || !board || swarmPosting) return;
  swarmPosting = true; swarmStick = true; syncSwarmSend();
  try {
    await postSwarm('messages', { text });
    if (swarmDrafts.get(board.board_id)?.trim() === text) swarmDrafts.delete(board.board_id);
    if (swarmShownBoard === board.board_id && input.value.trim() === text) input.value = '';
  } catch (e) {
    toast(`投稿に失敗: ${e.message}`, 'err'); // the text stays in the input
  } finally { swarmPosting = false; syncSwarmSend(); }
}

function wireSwarm() {
  $('#btn-swarm').onclick = toggleSwarm;
  $('#btn-swarm-close').onclick = closeSwarm;
  $('#btn-swarm-refresh').onclick = () => { if (S.threadId) loadSwarm(S.threadId, { manual: true }); };
  $$('.sw-chip').forEach((b) => { b.onclick = () => insertMention(b.dataset.mention); });
  const input = $('#swarm-input');
  input.addEventListener('input', () => { if (swarmShownBoard) swarmDrafts.set(swarmShownBoard, input.value); syncSwarmSend(); });
  input.addEventListener('keydown', (e) => { if (e.key === 'Enter' && (e.isComposing || e.keyCode === 229)) e.preventDefault(); });
  $('#swarm-post').addEventListener('submit', (e) => { e.preventDefault(); sendSwarmPost(); });
  const feed = $('#swarm-feed');
  feed.addEventListener('scroll', () => { if (feed.clientHeight) swarmStick = feed.scrollHeight - feed.scrollTop - feed.clientHeight < 60; }, { passive: true });
  if (typeof ResizeObserver === 'function') new ResizeObserver(() => { if (swarmStick) feed.scrollTop = feed.scrollHeight; }).observe(feed);
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
  const coordinator = curBoard()?.coordinator_thread_id;
  if (coordinator) want.add(coordinator);
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
    loadThreadsSoon(); if (S.threadId) { loadDetailSoon(); loadSwarm(S.threadId); }
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

// Fires only while the tab is hidden or unfocused: Desktop IPC, else Web Notification.
function notifyUser(title, body, threadId) {
  if (!document.hidden && document.hasFocus()) return;
  if (isDesktop()) { ipc('notify:' + JSON.stringify({ title, body, thread_id: threadId || '' })); return; }
  if (typeof Notification === 'function' && Notification.permission === 'granted') {
    const n = new Notification(title, { body });
    n.onclick = () => { window.focus(); if (threadId) openThread(threadId); };
  }
}

// Run outcomes raise OS notifications for top-level threads and forks; swarm workers and delegated tasks stay in-app.
async function notifyRunOutcome(title, threadId, followup = false) {
  if (followup) return;
  // a just-spawned child may not be in the thread list yet
  const t = threadById(threadId) || (await api(`/api/threads/${threadId}`).catch(() => null))?.thread;
  if (!t?.parent_thread_id || t.relationship === 'fork') notifyUser(title, cleanTitle(t?.title) || threadId, threadId);
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
      if (m.status === 'rate-limited') { loadUsageLimitsSoon(); notifyRunOutcome('制限中', m.thread_id); }
      if (m.thread_id === cur) {
        renderHeader(); renderComposer(); renderLive();
        if (isBusy(prev) && !isBusy(m.status)) { loadDetailSoon(); refreshDiffSoon(); }
      }
      if (!isBusy(m.status) && S.pending.has(m.thread_id)) { S.pending.delete(m.thread_id); renderApprovals(); }
      refreshMonitorSoon();
      break;
    }
    case 'run_started': {
      S.live.set(m.thread_id, { runId: m.run.id, assistant: '', reasoning: '' });
      S.liveUsage.delete(m.thread_id);
      setThreadStatus(m.thread_id, 'running');
      renderSidebarSoon();
      if (m.thread_id === cur && S.detail) {
        if (!S.detail.runs.some((r) => r.id === m.run.id)) S.detail.runs.push(m.run);
        renderHeader(); renderComposer(); renderStatPill();
        renderLive(); scrollBottom();
      }
      refreshMonitorSoon();
      break;
    }
    case 'run_status_changed': {
      const run = (S.detail?.runs || []).find((r) => r.id === m.run_id);
      if (run) run.status = m.status;
      if (TERMINAL_RUN.has(m.status)) {
        const live = S.live.get(m.thread_id);
        if (!live || !live.runId || live.runId === m.run_id) S.live.delete(m.thread_id);
        const usage = S.liveUsage.get(m.thread_id);
        if (!usage || !usage.runId || usage.runId === m.run_id) S.liveUsage.delete(m.thread_id);
        if (m.status === 'completed' || m.status === 'failed') notifyRunOutcome(m.status === 'completed' ? '実行完了' : '実行失敗', m.thread_id, m.status === 'completed' && m.followup);
        if (m.status === 'failed' && m.error) toast(`実行失敗: ${m.error}`, 'err', 6000);
        if (m.thread_id === cur) { loadDetailSoon(); refreshDiffSoon(); loadSwarm(cur); }
        loadThreadsSoon(); loadDbStatsSoon(); loadUsageSoon(); loadUsageLimitsSoon();
      } else if (m.thread_id === cur) { renderHeader(); renderLive(); }
      refreshMonitorSoon();
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
      S.detail.total_items += 1;
      if (it.kind === 'user_message') {
        const k = S.optimistic.findIndex((x) => x.threadId === m.thread_id);
        if (k >= 0) { const [o] = S.optimistic.splice(k, 1); $(`[data-opt="${o.key}"]`)?.remove(); }
      }
      S.stick = nearBottom() || S.stick;
      const host = $('#live');
      if (host) {
        $('#timeline .blank')?.remove();
        // optimistic bubbles stay below the items that were sent before them
        const anchor = $('#timeline > .msg.user.pending') || host;
        let group = anchor.previousElementSibling;
        if (!group?.classList.contains('run-group') || group.dataset.run !== (it.run_id || '')) { group = runGroup(it.run_id); anchor.before(group); }
        appendItem(it, group, $(':scope > .run-foot', group));
      }
      renderLive(); scrollBottom();
      if (it.kind === 'file_change') refreshDiffSoon();
      if (it.kind === 'subagent') loadDetailSoon();
      renderContextPanelSoon();
      break;
    }
    case 'run_usage_updated': {
      S.liveUsage.set(m.thread_id, {
        runId: m.run_id,
        context_tokens: Number(m.context_tokens) || 0,
        input_tokens: Number(m.input_tokens) || 0,
        output_tokens: Number(m.output_tokens) || 0,
        cache_read_tokens: Number(m.cache_read_tokens) || 0,
        cache_write_tokens: Number(m.cache_write_tokens) || 0,
      });
      if (m.thread_id === cur) {
        renderContextMeters();
        renderContextPanelSoon();
      }
      break;
    }
    case 'pending_request': {
      const r = m.request;
      const a = S.pending.get(r.thread_id) || [];
      if (!a.some((x) => x.id === r.id)) a.push(r);
      S.pending.set(r.thread_id, a);
      if (r.thread_id === cur) { renderApprovals(); scrollBottom(); } else toast(`⚠ 承認待ち: ${r.title || r.kind}`, '', 5000);
      notifyUser('承認待ち', r.title || r.kind, r.thread_id);
      renderSidebarSoon();
      refreshMonitorSoon();
      break;
    }
    case 'pending_resolved':
      dropPending(m.thread_id, m.request_id);
      refreshMonitorSoon();
      break;
    case 'shell_output':
      shellAppendChunk(m.exec_id, m.stream, m.chunk);
      break;
    case 'shell_finished':
      shellFinish(m);
      refreshMonitorSoon();
      break;
    case 'subagent_updated': {
      const known = threadById(m.child_thread_id);
      if (!known) loadThreadsSoon(); else setThreadStatus(m.child_thread_id, m.status);
      if (m.parent_thread_id === cur) {
        if (!known || !S.detail?.children?.some((c) => c.id === m.child_thread_id)) loadDetailSoon();
        renderHeader();
        $$(`.step.sub[data-child="${CSS.escape(m.child_thread_id)}"] .sub-status`).forEach((el) => {
          el.className = `sbadge sub-status s-${m.status}`;
          el.textContent = statusLabel(m.status);
        });
      }
      syncSubscriptions(); renderSidebarSoon();
      break;
    }
    case 'swarm_updated':
      pushSwarm(m.thread_id, m.board);
      break;
    case 'resync':
      loadThreadsSoon();
      if (S.threadId) { loadDetailSoon(); loadSwarm(S.threadId); }
      break;
    case 'error':
      toast(`サーバーエラー: ${m.message}`, 'err', 6000);
      break;
    default: break; // pong
  }
}

// ───────────────────────── sidebar visibility ─────────────────────────
function openDrawer() { $('#app').dataset.drawer = 'open'; $('#scrim').hidden = false; revealActiveRow(); }
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
  $('#btn-pause-all').onclick = async (e) => {
    const btn = e.currentTarget;
    btn.disabled = true;
    try {
      const state = await api('/api/pause-all', { method: 'PUT', json: { paused: !S.pauseAll } });
      S.pauseAll = state.paused;
      renderPauseAll();
      toast(S.pauseAll ? '自動実行を一時停止しました。' : '自動実行を再開しました。');
    } catch (error) {
      toast(`Pause all の更新に失敗しました: ${error.message}`, 'err');
    } finally { btn.disabled = false; }
  };
  $('#btn-add-project').onclick = projectDialog;
  $('#btn-search').onclick = searchDialog;
  $('#btn-sb-search').onclick = searchDialog;
  $('#btn-usage').onclick = () => usageDialog();
  $('#btn-settings').onclick = () => settingsDialog();
  $('#btn-top-usage').onclick = () => usageDialog();
  $('#btn-monitor').onclick = () => monitorDialog();
  $('#stat-pill').onclick = (e) => usageDialog(e.target.closest('[data-utab]')?.dataset.utab || 'model');
  $('#btn-kids').setAttribute('aria-pressed', String(S.showKids));
  $('#btn-kids').onclick = (e) => {
    S.showKids = !S.showKids; ls.set('t3.kids', S.showKids ? '1' : '0'); S.kidsOpen.clear();
    e.currentTarget.setAttribute('aria-pressed', String(S.showKids));
    renderSidebar();
  };
  renderUsagePill();
  $('#btn-archived').onclick = (e) => {
    S.showArchived = !S.showArchived; ls.set('t3.archived', S.showArchived ? '1' : '0');
    e.currentTarget.setAttribute('aria-pressed', String(S.showArchived));
    renderSidebar(); loadThreads().catch(() => {});
  };
  $('#btn-diff').onclick = toggleDiff;
  $('#btn-diff-close').onclick = () => setRail(null);
  $('#btn-context').onclick = toggleContext;
  $('#btn-ctx-meter').onclick = toggleContext;
  $('#btn-ctx-close').onclick = () => setRail(null);
  $('#btn-ctx-refresh').onclick = () => {
    if (S.threadId) { loadDetail(S.threadId); refreshDiffStat(); }
    loadUsageLimits({ refresh: true });
  };
  $$('.rail-tab').forEach((tab) => {
    tab.onclick = () => {
      const target = tab.dataset.railTab;
      if (target === 'swarm' && !curBoard()) return;
      setRail(target);
    };
  });
  $$('.rail-tabs').forEach((bar) => {
    bar.addEventListener('keydown', (e) => {
      if (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight') return;
      const tabs = $$('.rail-tab', bar).filter((t) => !t.hidden);
      const i = tabs.indexOf(e.target);
      if (i < 0 || tabs.length < 2) return;
      e.preventDefault();
      const next = tabs[(i + (e.key === 'ArrowRight' ? 1 : -1) + tabs.length) % tabs.length];
      setRail(next.dataset.railTab);
      const panelId = next.getAttribute('aria-controls');
      $(`#${panelId} .rail-tab[data-rail-tab="${next.dataset.railTab}"]`)?.focus();
    });
  });
  wireSwarm();
  $('#btn-diff-refresh').onclick = loadDiff;
  $('#btn-diff-expand').onclick = () => {
    const all = $$('#diff-body details.dfile');
    const anyClosed = all.some((d) => !d.open);
    all.forEach((d) => { d.open = anyClosed; });
  };
  $('#chk-ws').onchange = (e) => { S.ignoreWs = e.target.checked; ls.set('t3.ws', S.ignoreWs ? '1' : '0'); loadDiff(); };
  $$('#diff-scope button').forEach((b) => {
    b.onclick = () => {
      S.diffScope = b.dataset.scope === 'latest' ? 'latest' : 'all';
      ls.set('t3.diffScope', S.diffScope);
      if (S.diffData && S.diffData.threadId === S.threadId) renderDiffView(S.diffData.data);
      else loadDiff();
    };
  });
  syncDiffScopeButtons();
  wireRailResizer();
  $('#btn-fork').onclick = forkDialog;
  $('#btn-delegate').onclick = delegateDialog;
  $('#btn-children').onclick = (e) => childrenMenu(e.currentTarget);
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
  $('#swarm-hint').after(h('div', { id: 'limit-alert', class: 'limit-alert', role: 'status' }));
  const input = ta();
  input.addEventListener('input', () => { cmdDismissed = false; autosize(); syncSendEnabled(); });
  input.addEventListener('keydown', (e) => {
    if (e.isComposing || e.keyCode === 229) return; // never act while IME is composing
    if (cmdMenu && cmdMenu.items.length) {
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        e.preventDefault();
        const next = cmdMenu.index < 0
          ? (e.key === 'ArrowDown' ? 0 : cmdMenu.items.length - 1)
          : cmdMenu.index + (e.key === 'ArrowDown' ? 1 : -1);
        moveCommandSelection(next);
        return;
      }
      if (e.key === 'Tab') {
        e.preventDefault();
        const idx = cmdMenu.index < 0 ? 0 : cmdMenu.index;
        applyCommandChoice(cmdMenu.items[idx], { execute: false });
        return;
      }
      if (e.key === 'Escape') {
        e.preventDefault();
        cmdDismissed = true;
        hideCommandPalette();
        return;
      }
      if (e.key === 'Enter' && !e.shiftKey && !e.altKey && cmdMenu.index >= 0) {
        if (isCoarse() && !(e.metaKey || e.ctrlKey)) return;
        e.preventDefault();
        const choice = cmdMenu.items[cmdMenu.index];
        applyCommandChoice(choice, { execute: choice.execute });
        return;
      }
    }
    if (e.key !== 'Enter') return;
    if (e.shiftKey || e.altKey) return;
    if (isCoarse() && !(e.metaKey || e.ctrlKey)) return; // phones: Enter = newline, use the send button
    e.preventDefault(); send();
  });
  document.addEventListener('pointerdown', (e) => {
    if (cmdMenu && !$('#composer').contains(e.target)) hideCommandPalette();
  }, true);
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

  // sidebar: arrow keys walk the visible rows
  $('#sidebar').addEventListener('keydown', (e) => {
    if ((e.key !== 'ArrowDown' && e.key !== 'ArrowUp') || e.altKey || e.metaKey || e.ctrlKey) return;
    const rows = $$('.sb-scroll .trow-main, .sb-scroll .pg-toggle, .sb-scroll .more, .sb-scroll .empty-toggle').filter((el) => el.offsetParent);
    if (!rows.length) return;
    const from = e.target.closest('.trow-main') || e.target.closest('.pg-head')?.querySelector('.pg-toggle') || e.target;
    const i = rows.indexOf(from);
    const next = rows[i < 0 ? (e.key === 'ArrowDown' ? 0 : rows.length - 1) : Math.max(0, Math.min(rows.length - 1, i + (e.key === 'ArrowDown' ? 1 : -1)))];
    e.preventDefault();
    next.focus();
    next.scrollIntoView({ block: 'nearest' });
  });

  // Markdown chips and code block copy (delegated; work for every rendered Markdown)
  document.addEventListener('click', async (e) => {
    const chip = e.target.closest?.('.md .thread-chip, .md .path-chip');
    if (chip) {
      if (chip.dataset.thread) { $('dialog[open]')?.close(); openThread(chip.dataset.thread); } else copyText(chip.dataset.path, 'パスをコピーしました');
      return;
    }
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
    const typing = !!e.target.closest?.('input, textarea, select, [contenteditable="true"]');
    if (e.key === 'Escape') {
      if (e.defaultPrevented) return;
      if (popover) { e.preventDefault(); const a = popover.anchor; closePopover(); a.focus(); } else if ($('#app').dataset.drawer === 'open') closeDrawer();
      else if (!$('dialog[open]') && shownRail() === 'swarm' && (swarmOverlay() || $('#swarm-panel').contains(e.target))) { e.preventDefault(); closeSwarm(); }
      else if (!$('dialog[open]') && shownRail() === 'context' && (swarmOverlay() || $('#context-panel').contains(e.target))) {
        e.preventDefault();
        const had = $('#context-panel').contains(document.activeElement);
        setRail(null);
        if (had) $('#btn-context')?.focus();
      } else if (!$('dialog[open]') && shownRail() === 'diff' && (swarmOverlay() || $('#diff-panel').contains(e.target))) {
        e.preventDefault();
        const had = $('#diff-panel').contains(document.activeElement);
        setRail(null);
        if (had) $('#btn-diff')?.focus();
      } else if (!typing && !$('dialog[open]')) { e.preventDefault(); ta().focus(); }
      return;
    }
    if (e.key === '/' && !e.metaKey && !e.ctrlKey && !e.altKey && !typing && !popover && !$('dialog[open]')) {
      e.preventDefault();
      const inp = ta();
      inp.focus();
      if (!inp.value) {
        inp.value = '/';
        cmdDismissed = false;
        autosize(); syncSendEnabled();
        inp.setSelectionRange(1, 1);
      }
      return;
    }
    // Alt+S (⌥S types "ß" on macOS, so match the physical key)
    if (e.code === 'KeyS' && e.altKey && !e.ctrlKey && !e.metaKey && !e.shiftKey) { e.preventDefault(); if (!$('dialog[open]')) toggleSwarm(); return; }
    const mod = IS_MAC ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey;
    if (!mod || e.altKey || e.shiftKey) return;
    const k = e.key.toLowerCase();
    const modal = $$('dialog[open]').at(-1); // the topmost one
    if (k === 'k') { if (modal && !modal.classList.contains('search')) return; e.preventDefault(); searchDialog(); return; }
    if (k === 'u') {
      if (!IS_MAC && typing) return; // ⌘U has no text-editing meaning on macOS, so it also works from the composer
      e.preventDefault(); // Ctrl+U would otherwise open "view source"
      if (modal?.classList.contains('usage')) modal.close(); else if (!modal) usageDialog();
      return;
    }
    if (!['b', 'd', 'i', 'n'].includes(k)) return;
    e.preventDefault();
    if (modal) return;
    if (k === 'b') toggleSidebar();
    else if (k === 'd') toggleDiff();
    else if (k === 'i') toggleContext();
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
  setRail(S.rail);
  connectWs();
  try {
    const [meta, providers, pauseState] = await Promise.all([
      api('/api/meta').catch(() => null),
      api('/api/providers').catch(() => []),
      api('/api/pause-all').catch(() => null),
    ]);
    S.meta = meta; S.providers = Array.isArray(providers) ? providers : [];
    S.pauseAll = pauseState?.paused === true;
    renderPauseAll();
    if (S.wsOk && meta) setConn('ok', `接続済み · v${meta.version}`);
    if (meta?.follow_up_behavior && !ls.get('t3.behavior', '')) S.behavior = meta.follow_up_behavior === 'queue' ? 'queue' : 'steer';
    await loadProjects();
    S.compose.sel = defaultSelection(S.projectId);
    renderComposer(); renderStatPill(); renderContextPanel(); loadDbStats(); loadUsage(7);
    await loadThreads();
    if (!S.threadId && !hashId) {
      preferActiveProject();
      S.compose.sel = defaultSelection(S.projectId);
      renderSidebar(); renderHeader(); renderHero(); renderComposer(); renderContextPanel();
    }
  } catch (e) {
    setConn('bad', `サーバーに接続できません: ${e.message}`);
    toast(`初期化に失敗: ${e.message}`, 'err', 6000);
  }
  if (hashId) await openThread(hashId, { push: false });
  else if (!isCoarse() && !$('dialog[open]')) ta().focus();
  $('#app').classList.add('ready');
  setTimeout(loadUsageLimits, 600); // background only: the CLIs behind it can be slow
  setInterval(() => { if (!document.hidden) { loadThreads().catch(() => {}); refreshPauseAll(); } }, 15000);
  setInterval(() => { if (!document.hidden) loadUsage(7); }, 5 * 60000);
  setInterval(tickSwarmTimes, 30000);
  setInterval(() => { if (!document.hidden) renderLimitSurfaces(); }, 30000);
}

boot();
