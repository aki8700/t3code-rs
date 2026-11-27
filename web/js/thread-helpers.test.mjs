// thread-helpers.test.mjs — unit tests for web/js/thread-helpers.js.
// Runs with: node --test web/js/thread-helpers.test.mjs  (node:test + node:assert/strict only)
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import {
  threadCmp, buildTree, treeHas, orderProjects, threadsForProject, visibleProjectTree,
  isWorktreeChild, searchTerms, plainSnippet, aroundMatch, groupHits,
  swarmWorkers, bareName, normalizeBoard, swarmCounts, swarmSummaryText, swarmPillLabel,
  normalizeMonitor, limitsSummaryData,
} from './thread-helpers.js';

const thread = (id, extra = {}) => ({ id, title: id, project_id: 'p1', updated_at: '2026-11-01T00:00:00Z', pinned: 0, archived: false, ...extra });

describe('threadCmp', () => {
  it('sorts pinned threads before unpinned ones', () => {
    const pinned = thread('a', { pinned: 1, updated_at: '2026-01-01T00:00:00Z' });
    const fresh = thread('b', { pinned: 0, updated_at: '2026-12-01T00:00:00Z' });
    assert.equal([fresh, pinned].sort(threadCmp)[0].id, 'a');
  });

  it('sorts by updated_at descending when the pin state matches', () => {
    const old = thread('a', { updated_at: '2026-11-01T00:00:00Z' });
    const recent = thread('b', { updated_at: '2026-11-03T00:00:00Z' });
    assert.deepEqual([old, recent].sort(threadCmp).map((t) => t.id), ['b', 'a']);
  });

  it('returns 0 for identical pin state and timestamp', () => {
    const a = thread('a');
    const b = thread('b');
    assert.equal(threadCmp(a, b), 0);
  });
});

describe('buildTree', () => {
  it('nests children under parents and sorts roots newest first', () => {
    const list = [
      thread('b', { updated_at: '2026-11-01T00:00:00Z' }),
      thread('a', { updated_at: '2026-11-03T00:00:00Z' }),
      thread('c', { parent_thread_id: 'a', updated_at: '2026-11-02T00:00:00Z' }),
      thread('d', { parent_thread_id: 'c', updated_at: '2026-11-02T00:00:00Z' }),
    ];
    const tree = buildTree(list);
    assert.deepEqual(tree.map((n) => n.t.id), ['a', 'b']);
    assert.deepEqual(tree[0].kids.map((n) => n.t.id), ['c']);
    assert.deepEqual(tree[0].kids[0].kids.map((n) => n.t.id), ['d']);
  });

  it('treats a thread whose parent is missing as a root', () => {
    const list = [thread('a'), thread('orphan', { parent_thread_id: 'missing' })];
    assert.deepEqual(buildTree(list).map((n) => n.t.id), ['a', 'orphan']);
  });

  it('does not loop on parent cycles or a self-parent', () => {
    const cycle = [thread('x', { parent_thread_id: 'y' }), thread('y', { parent_thread_id: 'x' })];
    assert.deepEqual(buildTree(cycle), []);
    assert.deepEqual(buildTree([thread('z', { parent_thread_id: 'z' })]), []);
  });
});

describe('treeHas', () => {
  const node = { t: { id: 'a' }, kids: [{ t: { id: 'b' }, kids: [{ t: { id: 'c' }, kids: [] }] }] };
  it('finds the node itself and descendants', () => {
    assert.equal(treeHas(node, 'a'), true);
    assert.equal(treeHas(node, 'c'), true);
  });
  it('returns false for an id outside the subtree', () => {
    assert.equal(treeHas(node, 'z'), false);
  });
});

describe('threadsForProject', () => {
  const threads = [
    thread('a', { project_id: 'p1' }),
    thread('b', { project_id: 'p1', archived: true }),
    thread('c', { project_id: 'p2' }),
  ];
  it('returns only the project threads, hiding archived ones by default', () => {
    assert.deepEqual(threadsForProject(threads, 'p1', false).map((t) => t.id), ['a']);
  });
  it('includes archived threads when showArchived is true', () => {
    assert.deepEqual(threadsForProject(threads, 'p1', true).map((t) => t.id), ['a', 'b']);
    assert.deepEqual(threadsForProject(threads, 'p2', false).map((t) => t.id), ['c']);
  });
});

describe('orderProjects', () => {
  const projects = [{ id: 'p1' }, { id: 'p2' }, { id: 'p3' }];
  const threads = [
    thread('a', { project_id: 'p1', updated_at: '2026-11-01T00:00:00Z' }),
    thread('b', { project_id: 'p2', updated_at: '2026-11-03T00:00:00Z' }),
    thread('c', { project_id: 'p1', updated_at: '2026-11-05T00:00:00Z', archived: true }),
  ];
  it('orders active projects by latest thread activity and lists the rest as empty', () => {
    const { active, empty } = orderProjects(projects, threads, false);
    assert.deepEqual(active.map((p) => p.id), ['p2', 'p1']);
    assert.deepEqual(empty.map((p) => p.id), ['p3']);
  });
  it('counts archived threads when showArchived is true', () => {
    const { active } = orderProjects(projects, threads, true);
    assert.deepEqual(active.map((p) => p.id), ['p1', 'p2']);
  });
});

describe('visibleProjectTree', () => {
  const tree = Array.from({ length: 12 }, (_, i) => ({ t: { id: `n${i}` }, kids: [] }));
  it('shows the first 10 nodes plus the node hiding the active thread', () => {
    assert.deepEqual(visibleProjectTree(tree, 'n11', false, 10).map((n) => n.t.id),
      ['n0', 'n1', 'n2', 'n3', 'n4', 'n5', 'n6', 'n7', 'n8', 'n9', 'n11']);
  });
  it('shows only the first 10 nodes when the active thread is already among them', () => {
    assert.equal(visibleProjectTree(tree, 'n3', false, 10).length, 10);
  });
  it('shows everything when expanded', () => {
    assert.equal(visibleProjectTree(tree, 'n11', true, 10).length, 12);
  });
});

describe('isWorktreeChild', () => {
  it('is false without a parent or without a worktree', () => {
    assert.equal(isWorktreeChild({ worktree_path: '/w/a' }, null), false);
    assert.equal(isWorktreeChild({ parent_thread_id: 'p' }, null), false);
    assert.equal(isWorktreeChild(null, null), false);
  });
  it('is true for a different worktree, false for the same worktree or branch', () => {
    const child = { parent_thread_id: 'p', worktree_path: '/w/child', branch: 'feat-x' };
    assert.equal(isWorktreeChild(child, null), true);
    assert.equal(isWorktreeChild(child, { worktree_path: '/w/child' }), false);
    assert.equal(isWorktreeChild(child, { worktree_path: '/w/other', branch: 'feat-x' }), false);
    assert.equal(isWorktreeChild(child, { worktree_path: '/w/other', branch: 'feat-y' }), true);
  });
});

describe('plainSnippet', () => {
  it('strips highlight tags, Markdown links, code ticks and headings', () => {
    assert.equal(plainSnippet('<mark>hello</mark> world'), 'hello world');
    assert.equal(plainSnippet('[link](https://example.com) text'), 'link text');
    assert.equal(plainSnippet('## 見出し `code`'), '見出し code');
  });
  it('returns an empty string for empty input', () => {
    assert.equal(plainSnippet(''), '');
    assert.equal(plainSnippet(null), '');
  });
});

describe('searchTerms', () => {
  it('dedupes terms and sorts them longest first', () => {
    assert.deepEqual(searchTerms('a bb ccc bb'), ['ccc', 'bb', 'a']);
  });
  it('strips surrounding quotes', () => {
    assert.deepEqual(searchTerms('  "quoted" \'x\' '), ['quoted', 'x']);
  });
  it('returns an empty array for a blank query', () => {
    assert.deepEqual(searchTerms('   '), []);
  });
});

describe('aroundMatch', () => {
  it('leaves a short snippet untouched', () => {
    assert.equal(aroundMatch('hello needle world', ['needle']), 'hello needle world');
  });
  it('starts a long snippet shortly before the first match', () => {
    const text = `${'a'.repeat(60)}needle${'b'.repeat(10)}`;
    const out = aroundMatch(text, ['needle']);
    assert.equal(out.startsWith('…'), true);
    assert.equal(out.includes('needle'), true);
    assert.equal(out.length, text.length - 30 + 1);
  });
  it('leaves text without a match untouched', () => {
    assert.equal(aroundMatch('nothing to see here', ['needle']), 'nothing to see here');
  });
});

describe('groupHits', () => {
  const rows = [
    { thread_id: 't1', thread_title: '実装メモ', updated_at: '2026-11-01T00:00:00Z', snippet: 'サーバーの seq を追跡する', item_id: 'i1' },
    { thread_id: 't1', thread_title: '実装メモ', updated_at: '2026-11-01T00:00:00Z', snippet: '再接続時に reset する', item_id: 'i2' },
    { thread_id: 't1', thread_title: '実装メモ', updated_at: '2026-11-01T00:00:00Z', snippet: '三つ目の異なる内容', item_id: 'i3' },
    { thread_id: 't2', thread_title: '実装メモ', updated_at: '2026-11-02T00:00:00Z', snippet: '実装メモ', item_id: 'i4' },
  ];
  it('groups rows by thread with at most two snippets', () => {
    const groups = groupHits(rows, ['seq']);
    assert.equal(groups.length, 2);
    assert.equal(groups[0].thread_id, 't1');
    assert.deepEqual(groups[0].snippets.map((s) => s.item_id), ['i1', 'i2']);
  });
  it('drops a snippet that only repeats the thread title', () => {
    const groups = groupHits(rows, ['seq']);
    assert.equal(groups[1].snippets.length, 0);
  });
  it('drops a snippet that only repeats an earlier snippet', () => {
    const dup = [
      { thread_id: 't1', thread_title: '題名', updated_at: '2026-11-01T00:00:00Z', snippet: '同じ内容です', item_id: 'i1' },
      { thread_id: 't1', thread_title: '題名', updated_at: '2026-11-01T00:00:00Z', snippet: '同じ内容です!', item_id: 'i2' },
    ];
    assert.deepEqual(groupHits(dup, ['内容']).at(0).snippets.map((s) => s.item_id), ['i1']);
  });
});

describe('bareName', () => {
  it('strips one leading @ and tolerates missing names', () => {
    assert.equal(bareName('@worker'), 'worker');
    assert.equal(bareName('worker'), 'worker');
    assert.equal(bareName(null), '');
    assert.equal(bareName(undefined), '');
  });
});

describe('swarmWorkers', () => {
  it('keeps every member except the coordinator', () => {
    const b = { members: [
      { name: 'coordinator', role: 'coordinator' },
      { name: 'w1', role: 'worker' },
      { name: 'w2' },
    ] };
    assert.deepEqual(swarmWorkers(b).map((m) => m.name), ['w1', 'w2']);
  });
});

describe('normalizeBoard', () => {
  it('returns null for a missing or board-less payload', () => {
    assert.equal(normalizeBoard(null), null);
    assert.equal(normalizeBoard({}), null);
    assert.equal(normalizeBoard({ board_id: '' }), null);
  });
  it('sorts messages by seq and falls back to the last message seq for head_seq', () => {
    const b = normalizeBoard({
      board_id: 'b1',
      members: [{ name: 'a' }, null, 'x'],
      messages: [{ seq: 3, text: 'c' }, { seq: 1, text: 'a' }, { seq: 2, text: 'b' }],
    });
    assert.deepEqual(b.messages.map((m) => m.seq), [1, 2, 3]);
    assert.equal(b.head_seq, 3);
    assert.deepEqual(b.members, [{ name: 'a' }]);
  });
  it('keeps an explicit head_seq and does not mutate the input message order', () => {
    const messages = [{ seq: 2 }, { seq: 1 }];
    const b = normalizeBoard({ board_id: 'b1', members: [], messages, head_seq: '7' });
    assert.equal(b.head_seq, 7);
    assert.deepEqual(messages.map((m) => m.seq), [2, 1]);
  });
});

describe('swarmCounts', () => {
  const board = { members: [
    { name: 'coordinator', role: 'coordinator', state: 'working' },
    { name: 'w1', role: 'worker', state: 'working' },
    { name: 'w2', role: 'worker', state: 'waiting' },
    { name: 'w3', role: 'worker', state: 'killed' },
    { name: 'w4', role: 'worker', state: 'starting' },
  ] };
  it('counts workers by state, excluding the coordinator', () => {
    assert.deepEqual(swarmCounts(board), { total: 5, workers: 4, working: 1, waiting: 1, killed: 1, alive: 3 });
  });
  it('counts an empty board as all zeros', () => {
    assert.deepEqual(swarmCounts({ members: [] }), { total: 0, workers: 0, working: 0, waiting: 0, killed: 0, alive: 0 });
  });
});

describe('swarmSummaryText', () => {
  it('shows working and waiting counts', () => {
    assert.equal(swarmSummaryText({ working: 2, waiting: 1, killed: 0 }), '稼働 2 / 待機 1');
  });
  it('appends the killed count only when non-zero', () => {
    assert.equal(swarmSummaryText({ working: 2, waiting: 1, killed: 1 }), '稼働 2 / 待機 1 / 終了 1');
  });
});

describe('swarmPillLabel', () => {
  it('uses ⌥S on Mac and Alt+S elsewhere', () => {
    assert.equal(swarmPillLabel({ alive: 3, working: 2 }, true), 'Swarm ボード: ワーカー 3（稼働 2） (⌥S)');
    assert.equal(swarmPillLabel({ alive: 3, working: 2 }, false), 'Swarm ボード: ワーカー 3（稼働 2） (Alt+S)');
  });
});

describe('normalizeMonitor', () => {
  it('passes a full payload through', () => {
    assert.deepEqual(
      normalizeMonitor({ pending_requests: [{ id: 1 }], active_runs: [{ id: 2 }], queued_thread_ids: ['t1'], active_shells: 2 }),
      { pending: [{ id: 1 }], runs: [{ id: 2 }], queued: ['t1'], shells: 2 },
    );
  });
  it('coerces missing or non-array sections to empty values', () => {
    assert.deepEqual(
      normalizeMonitor({ pending_requests: 'x', active_runs: null, queued_thread_ids: {}, active_shells: '3' }),
      { pending: [], runs: [], queued: [], shells: 3 },
    );
    assert.deepEqual(normalizeMonitor({}), { pending: [], runs: [], queued: [], shells: 0 });
  });
});

describe('limitsSummaryData', () => {
  it('returns empty text when there are no providers', () => {
    assert.deepEqual(limitsSummaryData([], 1000), { text: '', exhausted: false });
    assert.deepEqual(limitsSummaryData(undefined, 1000), { text: '', exhausted: false });
  });
  it('uses the tightest live window per provider', () => {
    const providers = [{ label: 'Claude', windows: [
      { remaining: 80, resetsAt: 0 },
      { remaining: 30, resetsAt: 5000 },
    ] }];
    assert.deepEqual(limitsSummaryData(providers, 1000), { text: '上限の残り: Claude 30%', exhausted: false });
  });
  it('skips windows that already reset', () => {
    const providers = [{ label: 'Claude', windows: [{ remaining: 5, resetsAt: 500 }] }];
    assert.deepEqual(limitsSummaryData(providers, 1000), { text: '', exhausted: false });
  });
  it('marks exhausted when the tightest window is used up and joins providers', () => {
    const providers = [
      { label: 'Claude', windows: [{ remaining: 0, resetsAt: 0 }] },
      { label: 'Codex', windows: [{ remaining: 0.5, resetsAt: 0 }] },
    ];
    assert.deepEqual(limitsSummaryData(providers, 1000), { text: '上限の残り: Claude 0% · Codex <1%', exhausted: true });
  });
});
