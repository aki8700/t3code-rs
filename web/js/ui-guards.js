// ui-guards.js — pure, DOM-free guards for the t3code-rs web UI.
//
// Browser + Node compatible ES module: no DOM, no fetch, no timers of its
// own. app.js only wires these in; all decisions live here so they can be
// unit-tested with `node --test web/js/ui-guards.test.mjs`.

/**
 * Monotonic generation fence: guards async responses against a newer
 * request for the same scope (thread switch, reload, refresh).
 * `next(scope)` issues a strictly increasing token, `isCurrent(scope, token)`
 * is true only for the latest token, `invalidate(scope)` retires every token
 * issued so far for that scope.
 */
export function createGenerationFence() {
  const current = new Map(); // scope -> latest token (0 = none issued)
  return {
    next(scope) {
      const token = (current.get(scope) || 0) + 1;
      current.set(scope, token);
      return token;
    },
    isCurrent(scope, token) {
      // Token 0 means "none issued": it is never current.
      return Number.isInteger(token) && token > 0 && (current.get(scope) || 0) === token;
    },
    invalidate(scope) {
      current.set(scope, (current.get(scope) || 0) + 1);
    },
  };
}

/**
 * Coalesces many small chunks into one flush per animation frame.
 * `scheduleFrame(cb)` schedules `cb` (e.g. requestAnimationFrame in the
 * browser); `flush(chunks)` receives the buffered chunks in push order.
 * `clear()` drops buffered chunks, `flushNow()` flushes synchronously.
 */
export function createDeltaCoalescer(scheduleFrame, flush) {
  let buffer = [];
  let scheduled = false;
  const drain = () => {
    scheduled = false;
    if (!buffer.length) return;
    const chunks = buffer;
    buffer = [];
    flush(chunks);
  };
  return {
    push(chunk) {
      buffer.push(chunk);
      if (scheduled) return;
      scheduled = true;
      scheduleFrame(drain);
    },
    clear() {
      buffer = [];
    },
    flushNow() {
      if (!buffer.length) return;
      // A frame is already queued; drain() will find an empty buffer then.
      const chunks = buffer;
      buffer = [];
      flush(chunks);
    },
  };
}

/**
 * Decides whether an out-of-band notification may fire.
 * Suppresses a notification when the user is already looking at that thread
 * (visible + focused + activeThreadId matches), and suppresses a repeat of
 * the same (threadId, kind) within `dedupeWindowMs` (default 5000 ms).
 * Pure: all inputs come from the argument; `now` defaults to Date.now().
 */
export function createNotificationGate({ dedupeWindowMs = 5000 } = {}) {
  const lastShown = new Map(); // `${threadId}\0${kind}` -> ms timestamp
  return {
    shouldNotify({ visible, focused, activeThreadId, threadId, kind, now = Date.now() }) {
      if (visible && focused && activeThreadId != null && threadId != null && activeThreadId === threadId) return false;
      const key = `${threadId ?? ''}\0${kind ?? ''}`;
      const prev = lastShown.get(key);
      if (prev !== undefined && now - prev < dedupeWindowMs) return false;
      lastShown.set(key, now);
      return true;
    },
  };
}
