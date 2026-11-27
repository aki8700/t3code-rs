// ui-guards.test.mjs — unit tests for web/js/ui-guards.js.
// Runs with: node --test web/js/ui-guards.test.mjs  (node:test + node:assert/strict only)
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { createGenerationFence, createDeltaCoalescer, createNotificationGate } from './ui-guards.js';

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
