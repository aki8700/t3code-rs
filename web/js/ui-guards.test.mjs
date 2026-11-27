// ui-guards.test.mjs — unit tests for web/js/ui-guards.js.
// Runs with: node --test web/js/ui-guards.test.mjs  (node:test + node:assert/strict only)
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { createGenerationFence, createDeltaCoalescer, createNotificationGate, createSeqTracker } from './ui-guards.js';

describe('GenerationFence', () => {
  it('rejects an older token once a newer one was issued for the same scope', () => {
    const fence = createGenerationFence();
    const first = fence.next('detail');
    const second = fence.next('detail');
    assert.equal(fence.isCurrent('detail', first), false);
    assert.equal(fence.isCurrent('detail', second), true);
  });

  it('keeps scopes independent', () => {
    const fence = createGenerationFence();
    const detail = fence.next('detail');
    const diff = fence.next('diff');
    assert.equal(fence.isCurrent('detail', detail), true);
    assert.equal(fence.isCurrent('diff', diff), true);
    fence.next('detail');
    assert.equal(fence.isCurrent('detail', detail), false);
    assert.equal(fence.isCurrent('diff', diff), true);
    assert.equal(fence.isCurrent('detail', diff), false);
  });

  it('rejects every token issued before invalidate()', () => {
    const fence = createGenerationFence();
    const before = fence.next('swarm');
    fence.invalidate('swarm');
    assert.equal(fence.isCurrent('swarm', before), false);
    const after = fence.next('swarm');
    assert.equal(fence.isCurrent('swarm', after), true);
  });

  it('rejects a token for a scope that never issued one', () => {
    const fence = createGenerationFence();
    assert.equal(fence.isCurrent('detail', 1), false);
    assert.equal(fence.isCurrent('detail', 0), false);
  });
});

describe('DeltaCoalescer', () => {
  it('coalesces multiple chunks pushed in one frame into a single flush', () => {
    let scheduled = null;
    const flushed = [];
    const coalescer = createDeltaCoalescer((cb) => { scheduled = cb; }, (chunks) => flushed.push(chunks));
    coalescer.push({ text: 'a' });
    coalescer.push({ text: 'b' });
    coalescer.push({ text: 'c' });
    assert.equal(flushed.length, 0);
    assert.equal(typeof scheduled, 'function');
    scheduled();
    assert.equal(flushed.length, 1);
    assert.deepEqual(flushed[0].map((c) => c.text), ['a', 'b', 'c']);
  });

  it('schedules only one frame per burst and flushes the next burst separately', () => {
    let frames = 0;
    let scheduled = null;
    const flushed = [];
    const coalescer = createDeltaCoalescer((cb) => { frames++; scheduled = cb; }, (chunks) => flushed.push(chunks));
    coalescer.push({ text: 'a' });
    coalescer.push({ text: 'b' });
    assert.equal(frames, 1);
    scheduled();
    coalescer.push({ text: 'c' });
    assert.equal(frames, 2);
    scheduled();
    assert.deepEqual(flushed.map((f) => f.map((c) => c.text)), [['a', 'b'], ['c']]);
  });

  it('drops buffered chunks on clear() and does not flush an empty frame', () => {
    let scheduled = null;
    const flushed = [];
    const coalescer = createDeltaCoalescer((cb) => { scheduled = cb; }, (chunks) => flushed.push(chunks));
    coalescer.push({ text: 'a' });
    coalescer.clear();
    scheduled();
    assert.equal(flushed.length, 0);
    coalescer.push({ text: 'b' });
    scheduled();
    assert.deepEqual(flushed[0].map((c) => c.text), ['b']);
  });

  it('flushNow() emits buffered chunks immediately without waiting for the frame', () => {
    let scheduled = null;
    const flushed = [];
    const coalescer = createDeltaCoalescer((cb) => { scheduled = cb; }, (chunks) => flushed.push(chunks));
    coalescer.push({ text: 'a' });
    coalescer.push({ text: 'b' });
    coalescer.flushNow();
    assert.deepEqual(flushed[0].map((c) => c.text), ['a', 'b']);
    // The already-queued frame must not flush the same chunks again.
    scheduled();
    assert.equal(flushed.length, 1);
  });

  it('flushNow() on an empty buffer is a no-op', () => {
    let frames = 0;
    const flushed = [];
    const coalescer = createDeltaCoalescer(() => { frames++; }, (chunks) => flushed.push(chunks));
    coalescer.flushNow();
    assert.equal(flushed.length, 0);
    assert.equal(frames, 0);
  });
});

describe('NotificationGate', () => {
  const base = { visible: true, focused: true, activeThreadId: 't1', threadId: 't1', kind: 'run:完了', now: 1000 };

  it('suppresses a notification for the active thread while focused and visible', () => {
    const gate = createNotificationGate();
    assert.equal(gate.shouldNotify(base), false);
  });

  it('allows a notification for a different thread while focused and visible', () => {
    const gate = createNotificationGate();
    assert.equal(gate.shouldNotify({ ...base, threadId: 't2' }), true);
  });

  it('allows a notification for the active thread when the tab is hidden', () => {
    const gate = createNotificationGate();
    assert.equal(gate.shouldNotify({ ...base, visible: false }), true);
  });

  it('allows a notification for the active thread when the window is unfocused', () => {
    const gate = createNotificationGate();
    assert.equal(gate.shouldNotify({ ...base, focused: false }), true);
  });

  it('suppresses a repeat of the same (threadId, kind) inside the dedupe window', () => {
    const gate = createNotificationGate({ dedupeWindowMs: 5000 });
    const hidden = { ...base, visible: false, now: 1000 };
    assert.equal(gate.shouldNotify(hidden), true);
    assert.equal(gate.shouldNotify({ ...hidden, now: 1000 }), false);
    assert.equal(gate.shouldNotify({ ...hidden, now: 5999 }), false);
  });

  it('allows the same (threadId, kind) again once the dedupe window has passed', () => {
    const gate = createNotificationGate({ dedupeWindowMs: 5000 });
    const hidden = { ...base, visible: false, now: 1000 };
    assert.equal(gate.shouldNotify(hidden), true);
    assert.equal(gate.shouldNotify({ ...hidden, now: 6000 }), true);
  });

  it('dedupes per (threadId, kind): a different kind or thread is not suppressed', () => {
    const gate = createNotificationGate({ dedupeWindowMs: 5000 });
    const hidden = { ...base, visible: false, now: 1000 };
    assert.equal(gate.shouldNotify(hidden), true);
    assert.equal(gate.shouldNotify({ ...hidden, kind: 'run:失敗' }), true);
    assert.equal(gate.shouldNotify({ ...hidden, threadId: 't2' }), true);
  });

  it('uses a 5000 ms dedupe window by default', () => {
    const gate = createNotificationGate();
    const hidden = { ...base, visible: false, now: 0 };
    assert.equal(gate.shouldNotify(hidden), true);
    assert.equal(gate.shouldNotify({ ...hidden, now: 4999 }), false);
    assert.equal(gate.shouldNotify({ ...hidden, now: 5000 }), true);
  });
});

describe('SeqTracker', () => {
  it('records the first seq as the baseline with no gap or duplicate', () => {
    const tracker = createSeqTracker();
    assert.deepEqual(tracker.observe(5), { gap: false, duplicate: false });
  });

  it('reports no gap for consecutive seqs', () => {
    const tracker = createSeqTracker();
    tracker.observe(5);
    assert.deepEqual(tracker.observe(6), { gap: false, duplicate: false });
    assert.deepEqual(tracker.observe(7), { gap: false, duplicate: false });
  });

  it('reports a gap with the missed count when seq jumps ahead', () => {
    const tracker = createSeqTracker();
    tracker.observe(5);
    assert.deepEqual(tracker.observe(8), { gap: true, duplicate: false, missed: 2 });
    assert.deepEqual(tracker.observe(10), { gap: true, duplicate: false, missed: 1 });
  });

  it('reports a gap of exactly one missed message', () => {
    const tracker = createSeqTracker();
    tracker.observe(41);
    assert.deepEqual(tracker.observe(43), { gap: true, duplicate: false, missed: 1 });
  });

  it('flags the same seq as a duplicate and does not advance the baseline', () => {
    const tracker = createSeqTracker();
    tracker.observe(5);
    assert.deepEqual(tracker.observe(5), { gap: false, duplicate: true });
    // Still anchored at 5: the next expected seq is 6, not 7.
    assert.deepEqual(tracker.observe(6), { gap: false, duplicate: false });
  });

  it('flags an older seq as a duplicate', () => {
    const tracker = createSeqTracker();
    tracker.observe(10);
    assert.deepEqual(tracker.observe(7), { gap: false, duplicate: true });
    assert.deepEqual(tracker.observe(11), { gap: false, duplicate: false });
  });

  it('ignores a non-positive or non-integer seq without moving the baseline', () => {
    const tracker = createSeqTracker();
    for (const bad of [0, -1, 1.5, NaN, undefined, null, '5', Infinity]) {
      assert.deepEqual(tracker.observe(bad), { gap: false, duplicate: false });
    }
    // No baseline was set by the bad values: 3 becomes the baseline.
    assert.deepEqual(tracker.observe(3), { gap: false, duplicate: false });
    for (const bad of [0, -2, 2.5, '4', undefined]) {
      assert.deepEqual(tracker.observe(bad), { gap: false, duplicate: false });
    }
    assert.deepEqual(tracker.observe(4), { gap: false, duplicate: false });
  });

  it('reset() clears the baseline so the next seq starts fresh', () => {
    const tracker = createSeqTracker();
    tracker.observe(5);
    tracker.observe(6);
    tracker.reset();
    assert.deepEqual(tracker.observe(100), { gap: false, duplicate: false });
    assert.deepEqual(tracker.observe(101), { gap: false, duplicate: false });
  });

  it('keeps independent state per tracker instance', () => {
    const a = createSeqTracker();
    const b = createSeqTracker();
    a.observe(5);
    assert.deepEqual(b.observe(5), { gap: false, duplicate: false });
    assert.deepEqual(a.observe(8), { gap: true, duplicate: false, missed: 2 });
    assert.deepEqual(b.observe(6), { gap: false, duplicate: false });
  });

  it('advances the baseline past a gap so the next seq is consecutive', () => {
    const tracker = createSeqTracker();
    tracker.observe(5);
    tracker.observe(9);
    assert.deepEqual(tracker.observe(10), { gap: false, duplicate: false });
    assert.deepEqual(tracker.observe(9), { gap: false, duplicate: true });
  });
});
