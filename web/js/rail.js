// Right rail drag-to-resize handle with localStorage persistence and keyboard support.
import { $, ls } from './util.js';

const MIN_WIDTH = 260;
const MAX_WIDTH = 760;
const STORAGE_KEY = 't3.railWidth';

function clampWidth(w) {
  const cap = Math.min(MAX_WIDTH, Math.max(MIN_WIDTH, Math.floor(window.innerWidth * 0.65)));
  return Math.max(MIN_WIDTH, Math.min(cap, Math.round(w)));
}

export function applySavedRailWidth() {
  const saved = Number(ls.get(STORAGE_KEY, '0'));
  if (Number.isFinite(saved) && saved >= MIN_WIDTH) {
    document.documentElement.style.setProperty('--rail-custom-width', `${clampWidth(saved)}px`);
  }
}

export function resetRailWidth() {
  ls.remove(STORAGE_KEY);
  document.documentElement.style.removeProperty('--rail-custom-width');
}

export function wireRailResizer() {
  const handle = $('#rail-resizer');
  if (!handle) return;
  applySavedRailWidth();

  const currentWidth = () => {
    const panel = $('.rail-panel:not([hidden])');
    return panel ? panel.getBoundingClientRect().width : 360;
  };
  const setWidth = (w, persist = false) => {
    const clamped = clampWidth(w);
    document.documentElement.style.setProperty('--rail-custom-width', `${clamped}px`);
    handle.setAttribute('aria-valuenow', String(clamped));
    if (persist) ls.set(STORAGE_KEY, String(clamped));
  };

  let dragging = false;
  let startX = 0;
  let startW = 0;

  handle.addEventListener('pointerdown', (e) => {
    if (e.button !== 0 || matchMedia('(max-width: 1180px)').matches) return;
    e.preventDefault();
    dragging = true;
    startX = e.clientX;
    startW = currentWidth();
    handle.setPointerCapture(e.pointerId);
    handle.classList.add('dragging');
    document.body.classList.add('resizing-rail');
  });
  handle.addEventListener('pointermove', (e) => {
    if (!dragging) return;
    setWidth(startW + (startX - e.clientX), false);
  });
  const finish = (e) => {
    if (!dragging) return;
    dragging = false;
    try { handle.releasePointerCapture(e.pointerId); } catch { /* noop */ }
    handle.classList.remove('dragging');
    document.body.classList.remove('resizing-rail');
    setWidth(currentWidth(), true);
  };
  handle.addEventListener('pointerup', finish);
  handle.addEventListener('pointercancel', finish);
  handle.addEventListener('dblclick', () => resetRailWidth());
  handle.addEventListener('keydown', (e) => {
    if (e.key === 'ArrowLeft') { e.preventDefault(); setWidth(currentWidth() + 24, true); }
    else if (e.key === 'ArrowRight') { e.preventDefault(); setWidth(currentWidth() - 24, true); }
    else if (e.key === 'Home') { e.preventDefault(); resetRailWidth(); }
  });
}
