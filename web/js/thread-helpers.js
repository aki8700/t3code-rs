// thread-helpers.js — pure, DOM-free helpers for the t3code-rs web UI.
//
// Browser + Node compatible ES module: no DOM, no fetch, no timers.
// Extracted from web/app.js (Wave 2 / G-6); app.js only wires these in.
// All functions are pure: same inputs, same outputs, no hidden state.
// Runs with: node --test web/js/thread-helpers.test.mjs
import { cleanTitle } from './util.js';
import { limitIsReset, fmtLeft } from './usage-data.js';

// ───────────────────────── threads: sort / group ─────────────────────────

/** Sort key for sidebar threads: pinned first, then most recently updated. */
export const threadCmp = (a, b) => (b.pinned - a.pinned) || (Date.parse(b.updated_at) - Date.parse(a.updated_at));

/** Groups a flat thread list into a parent/child tree ({ t, kids } nodes). */
export function buildTree(list) {
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

/** True when the tree node itself, or any descendant, has the given id. */
export const treeHas = (node, id) => node.t.id === id || node.kids.some((k) => treeHas(k, id));

/** Threads of one project, honouring the archived filter. */
export function threadsForProject(threads, projectId, showArchived) {
  return threads.filter((t) => t.project_id === projectId && (showArchived || !t.archived));
}

/** Projects with threads, most recently active first; projects without threads separately. */
export function orderProjects(projects, threads, showArchived) {
  const latest = new Map();
  for (const t of threads) {
    if (t.archived && !showArchived) continue;
    const at = Date.parse(t.updated_at) || 0;
    if (at >= (latest.get(t.project_id) ?? 0)) latest.set(t.project_id, at);
  }
  return {
    active: projects.filter((p) => latest.has(p.id)).sort((a, b) => latest.get(b.id) - latest.get(a.id)),
    empty: projects.filter((p) => !latest.has(p.id)),
  };
}

/**
 * Which top-level tree nodes a collapsed project group shows: the first
 * `limit` nodes, plus any node hiding the active thread. Expanded groups
 * show everything.
 */
export function visibleProjectTree(tree, activeThreadId, expanded, limit = 10) {
  return expanded ? tree : tree.filter((n, i) => i < limit || treeHas(n, activeThreadId));
}

/** 専用 worktree 子スレッドか: 親と worktree/ブランチが異なる子。親が未読込でも worktree_path があれば対象とする。 */
export function isWorktreeChild(child, parent) {
  if (!child?.parent_thread_id || !child.worktree_path) return false;
  if (parent && child.worktree_path === parent.worktree_path) return false;
  if (parent && child.branch && parent.branch && child.branch === parent.branch) return false;
  return true;
}

// ───────────────────────── search filter ─────────────────────────

/** Search snippet as plain prose: server highlight tags and Markdown syntax removed. */
export const plainSnippet = (s) => String(s || '')
  .replace(/<\/?(mark|b)>/gi, '')
  .replace(/!?\[([^\]\n]*)\]\([^)\n]*\)?/g, '$1')
  .replace(/```[\w-]*|`|\*\*|__|~~/g, '')
  .replace(/(^|\n)\s*(#{1,6}\s+|>\s*|[-*+]\s+)/g, '$1')
  .replace(/\|?\s*:?-{3,}:?\s*\|?|\|/g, ' ')
  .replace(/\s+/g, ' ').trim();

/** Splits a query into deduped terms, longest first; strips surrounding quotes. */
export const searchTerms = (q) => [...new Set(q.split(/\s+/).map((t) => t.replace(/^["'*]+|["'*]+$/g, '')).filter(Boolean))].sort((a, b) => b.length - a.length);

/** Starts a long snippet shortly before its first match so the highlight stays inside the two visible lines. */
export function aroundMatch(text, terms) {
  const low = text.toLowerCase();
  const at = Math.min(...terms.map((t) => low.indexOf(t.toLowerCase())).filter((i) => i >= 0));
  return Number.isFinite(at) && at > 50 ? `…${text.slice(at - 30)}` : text;
}

/** One group per thread with at most two distinct snippets; snippets that only repeat the title are dropped. */
export function groupHits(rows, terms) {
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

// ───────────────────────── swarm: members / badges ─────────────────────────

/** Strips a leading @ from a member name. */
export const bareName = (n) => String(n ?? '').replace(/^@/, '');

/** Swarm workers: every member except the coordinator. */
export const swarmWorkers = (b) => b.members.filter((m) => m.role !== 'coordinator');

/** Normalizes a raw board snapshot: object-only members/messages, messages by seq, head_seq fallback. */
export function normalizeBoard(b) {
  if (!b || typeof b !== 'object' || !b.board_id) return null;
  const objects = (x) => (Array.isArray(x) ? x.filter((v) => v && typeof v === 'object') : []);
  const messages = objects(b.messages).sort((a, c) => a.seq - c.seq);
  return { ...b, members: objects(b.members), messages, head_seq: Number(b.head_seq) || Number(messages.at(-1)?.seq) || 0 };
}

/** Worker counts for the Swarm pill and summary line. */
export function swarmCounts(b) {
  const workers = swarmWorkers(b);
  const count = (s) => workers.filter((m) => m.state === s).length;
  return {
    total: b.members.length,
    workers: workers.length,
    working: count('working'),
    waiting: count('waiting'),
    killed: count('killed'),
    alive: workers.filter((m) => m.state !== 'killed').length,
  };
}

/** The Swarm panel summary line: `稼働 X / 待機 Y` (+ ` / 終了 Z` when any). */
export function swarmSummaryText(counts) {
  return `稼働 ${counts.working} / 待機 ${counts.waiting}${counts.killed ? ` / 終了 ${counts.killed}` : ''}`;
}

/** The Swarm pill tooltip / aria-label: alive and working worker counts plus the Alt+S shortcut. */
export function swarmPillLabel(counts, isMac) {
  return `Swarm ボード: ワーカー ${counts.alive}（稼働 ${counts.working}） (${isMac ? '⌥S' : 'Alt+S'})`;
}

// ───────────────────────── runtime state aggregation ─────────────────────────

/** Normalizes a GET /api/monitor payload into the four sections the monitor dialog renders. */
export function normalizeMonitor(d) {
  return {
    pending: Array.isArray(d.pending_requests) ? d.pending_requests : [],
    runs: Array.isArray(d.active_runs) ? d.active_runs : [],
    queued: Array.isArray(d.queued_thread_ids) ? d.queued_thread_ids : [],
    shells: Number(d.active_shells) || 0,
  };
}

/** The sidebar tooltip line (tightest live window per provider) and whether anything is used up. */
export function limitsSummaryData(providers, now) {
  const parts = []; let exhausted = false;
  for (const p of providers || []) {
    const live = p.windows.filter((w) => !limitIsReset(w, now));
    if (!live.length) continue;
    const least = Math.min(...live.map((w) => w.remaining));
    exhausted ||= least <= 0;
    parts.push(`${p.label} ${fmtLeft(least)}`);
  }
  return { text: parts.length ? `上限の残り: ${parts.join(' · ')}` : '', exhausted };
}
