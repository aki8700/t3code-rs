// Context window ring & section helpers for the T3 Code right sidebar.
import { h, svgNode } from './util.js';

export const CTX_SOURCE_LABEL = {
  live: 'ライブ',
  stored: '実測',
  session: '実測',
  run: '実測',
  estimated: '推定',
  empty: '未使用',
};

export const CTX_KIND_META = [
  ['user_message', 'ユーザー指示', 'var(--text)'],
  ['assistant_message', 'アシスタント応答', 'var(--accent)'],
  ['reasoning', '思考プロセス', '#a855f7'],
  ['tool_result', 'ツール結果', '#0ea5e9'],
  ['tool_call', 'ツール呼出', '#38bdf8'],
  ['command_execution', 'コマンド実行', 'var(--warning)'],
  ['file_change', 'ファイル変更', 'var(--success)'],
  ['subagent', 'サブエージェント', 'var(--p-antigravity)'],
  ['handoff', '引き継ぎ', 'var(--p-claude)'],
  ['system_notice', 'システム通知', 'var(--text-muted)'],
];

export function fallbackContextWindow(provider, model) {
  const m = String(model || '').toLowerCase();
  if (provider === 'claude') return m.includes('haiku') ? 200000 : 1000000;
  if (provider === 'codex') return /gpt-[56]|astra|sol|terra|luna/.test(m) ? 400000 : 200000;
  if (provider === 'antigravity') return 1000000;
  if (provider === 'open-code' || provider === 'opencode') return /[4789]b/.test(m) ? 32768 : 128000;
  return 200000;
}

export function contextRingSvg(pct, level, size = 16, stroke = 2.2) {
  const r = (size - stroke) / 2;
  const c = 2 * Math.PI * r;
  const clamped = Math.max(0, Math.min(100, pct));
  const dash = c * (1 - clamped / 100);
  const s = svgNode('svg', { class: `ctx-ring l-${level}`, width: String(size), height: String(size), viewBox: `0 0 ${size} ${size}`, 'aria-hidden': 'true' });
  s.append(
    svgNode('circle', { class: 'ctx-ring-track', cx: String(size / 2), cy: String(size / 2), r: String(r), 'stroke-width': String(stroke), fill: 'none' }),
    svgNode('circle', {
      class: 'ctx-ring-fill', cx: String(size / 2), cy: String(size / 2), r: String(r),
      'stroke-width': String(stroke), fill: 'none', 'stroke-linecap': 'round',
      'stroke-dasharray': c.toFixed(2), 'stroke-dashoffset': (clamped > 0 ? Math.min(c - 0.8, dash) : c).toFixed(2),
      transform: `rotate(-90 ${size / 2} ${size / 2})`,
    }),
  );
  return s;
}

export function setContextRing(container, pct, level, size = 16, stroke = 2.2) {
  if (!container) return;
  let s = container.querySelector(':scope > svg.ctx-ring');
  if (!s) {
    s = contextRingSvg(pct, level, size, stroke);
    container.prepend(s);
    return;
  }
  const r = (size - stroke) / 2;
  const c = 2 * Math.PI * r;
  const clamped = Math.max(0, Math.min(100, pct));
  const dash = c * (1 - clamped / 100);
  s.setAttribute('class', `ctx-ring l-${level}`);
  const fill = s.querySelector('.ctx-ring-fill');
  if (fill) {
    fill.setAttribute('stroke-dasharray', c.toFixed(2));
    fill.setAttribute('stroke-dashoffset', (clamped > 0 ? Math.min(c - 0.8, dash) : c).toFixed(2));
  }
}

export function ctxSection(title, action, body) {
  return h('section', { class: 'ctx-sec' },
    h('div', { class: 'ctx-sec-head' }, h('h3', { text: title }), action),
    body);
}
