// Safe Markdown renderer (escapes before building HTML; zero raw HTML).
import { esc, h, svg, resolveThreadId } from './util.js';
import { highlightCode } from './highlight.js';

const SAFE_URL = /^(https?:\/\/|\/(?!\/)|#|mailto:)/i;
const LOCAL_PATH = /^(file:\/\/|~\/|\/(Users|home|private|tmp|var|Volumes|opt|etc)\/)/;
const MD_MAX_DEPTH = 16;
const safeDecode = (s) => { try { return decodeURIComponent(s); } catch { return s; } };

export function mdInline(src) {
  const stash = [];
  const put = (html) => { stash.push(html); return `\u0000${stash.length - 1}\u0001`; };
  let s = String(src).replace(/[\u0000\u0001]/g, '');
  s = s.replace(/(`+)([\s\S]*?[^`])\1(?!`)/g, (_, __, code) => put(`<code>${esc(code.trim())}</code>`));
  s = esc(s);
  const emph = (t) => t
    .replace(/\*\*([^*\n]+?)\*\*/g, '<strong>$1</strong>')
    .replace(/__([^_\n]+?)__/g, '<strong>$1</strong>')
    .replace(/(^|[^*\w])\*([^*\s][^*\n]*?)\*(?!\*)/g, '$1<em>$2</em>')
    .replace(/(^|[^\w_])_([^_\s][^_\n]*?)_(?![\w_])/g, '$1<em>$2</em>')
    .replace(/~~([^~\n]+?)~~/g, '<del>$1</del>');
  s = s.replace(/!?\[([^\]\n]+)\]\((&lt;[^\n\u0000\u0001]*?&gt;|[^)\s\u0000\u0001]+)(?:\s+&quot;[^)\u0000\u0001]*&quot;)?\)/g, (m, t, url) => {
    const u = url.startsWith('&lt;') && url.endsWith('&gt;') ? url.slice(4, -4) : url;
    if (u.startsWith('t3-thread:')) {
      const id = resolveThreadId(safeDecode(u.split('/').pop())) || resolveThreadId(u.split('/').pop());
      return id ? put(`<button type="button" class="thread-chip" data-thread="${id}">${emph(t)} →</button>`) : m;
    }
    if (LOCAL_PATH.test(u)) {
      const path = esc(safeDecode(u.replace(/^file:\/\//, '').replace(/&amp;/g, '&').replace(/&#39;/g, "'")));
      return put(`<button type="button" class="path-chip" data-path="${path}" title="${path}（クリックでコピー）">${emph(t)}</button>`);
    }
    return SAFE_URL.test(u) ? put(`<a href="${u}" target="_blank" rel="noopener noreferrer">${emph(t)}</a>`) : m;
  });
  s = s.replace(/(^|[\s(（])(https?:\/\/[^\s<>"'\u0000\u0001()（）]*[^\s<>"'\u0000\u0001()（）.,;:!?、。])/g, (m, pre, u) => `${pre}${put(`<a href="${u}" target="_blank" rel="noopener noreferrer">${u}</a>`)}`);
  s = emph(s);
  for (let i = 0; i < 4 && /\u0000\d+\u0001/.test(s); i++) s = s.replace(/\u0000(\d+)\u0001/g, (_, n) => stash[+n]);
  return s;
}

export function mdCodeBlock(code, lang) {
  lang = (lang || '').replace(/[^\w+#.-]/g, '').slice(0, 24);
  const isDiff = lang === 'diff' || lang === 'patch';
  const body = isDiff ? code.split('\n').map((l) => {
    const c = l.startsWith('+') && !l.startsWith('+++') ? 'dl-add' : l.startsWith('-') && !l.startsWith('---') ? 'dl-del' : l.startsWith('@@') ? 'dl-hunk' : '';
    return `<span class="${c}">${esc(l) || ' '}</span>`;
  }).join('') : highlightCode(code, lang);
  return `<div class="codeblock"><div class="cb-head"><span>${esc(lang || 'text')}</span><button type="button" data-copy>${svg('copy')}<span>コピー</span></button></div><pre><code${isDiff ? ' class="diffc"' : ''}>${body}</code></pre></div>`;
}

const RE_FENCE = /^\s*(`{3,}|~{3,})\s*([^\s`]*)/;
const RE_HR = /^\s*([-*_])(\s*\1){2,}\s*$/;
const RE_LI = /^(\s*)([-*+]|\d+[.)])\s+(.*)$/;
const RE_TSEP = /^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$/;
const indentOf = (l) => l.match(/^\s*/)[0].length;

function mdBlocks(lines, depth = 0) {
  const out = [];
  let i = 0;
  const startsBlock = (l, next) => RE_FENCE.test(l) || /^#{1,6}\s/.test(l) || RE_HR.test(l) || /^\s*>/.test(l) || RE_LI.test(l)
    || (l.includes('|') && next != null && RE_TSEP.test(next) && next.includes('-'));
  while (i < lines.length) {
    const line = lines[i];
    let m = RE_FENCE.exec(line);
    if (m) {
      const f = m[1];
      const close = new RegExp(`^\\s*${f[0] === '`' ? '`' : '~'}{${f.length},}\\s*$`);
      const buf = []; i++;
      while (i < lines.length && !close.test(lines[i])) buf.push(lines[i++]);
      i++;
      out.push(mdCodeBlock(buf.join('\n'), m[2]));
      continue;
    }
    if (!line.trim()) { i++; continue; }
    if ((m = /^(#{1,6})\s+(.*?)\s*#*\s*$/.exec(line))) { out.push(`<h${m[1].length}>${mdInline(m[2])}</h${m[1].length}>`); i++; continue; }
    if (RE_HR.test(line)) { out.push('<hr>'); i++; continue; }
    if (depth < MD_MAX_DEPTH && /^\s*>/.test(line)) {
      const buf = [];
      while (i < lines.length && /^\s*>/.test(lines[i])) buf.push(lines[i++].replace(/^\s*>\s?/, ''));
      out.push(`<blockquote>${mdBlocks(buf, depth + 1).join('')}</blockquote>`);
      continue;
    }
    if (line.includes('|') && i + 1 < lines.length && RE_TSEP.test(lines[i + 1]) && lines[i + 1].includes('-')) {
      const cells = (l) => l.trim().replace(/^\|/, '').replace(/\|$/, '').split('|').map((c) => c.trim());
      const head = cells(line);
      const al = cells(lines[i + 1]).map((c) => (/^:-+:$/.test(c) ? 'center' : /-+:$/.test(c) ? 'right' : ''));
      i += 2;
      const rows = [];
      while (i < lines.length && lines[i].trim() && lines[i].includes('|')) rows.push(cells(lines[i++]));
      const td = (tag, c, k) => `<${tag}${al[k] ? ` style="text-align:${al[k]}"` : ''}>${mdInline(c)}</${tag}>`;
      out.push(`<div class="table-wrap"><table><thead><tr>${head.map((c, k) => td('th', c, k)).join('')}</tr></thead><tbody>${
        rows.map((r) => `<tr>${head.map((_, k) => td('td', r[k] ?? '', k)).join('')}</tr>`).join('')}</tbody></table></div>`);
      continue;
    }
    if ((m = RE_LI.exec(line))) {
      const base = m[1].length;
      const ordered = /\d/.test(m[2]);
      const start = ordered ? parseInt(m[2], 10) : 1;
      const items = [];
      while (i < lines.length) {
        const lm = RE_LI.exec(lines[i]);
        if (!lm || lm[1].length !== base) break;
        const it = { text: lm[3], sub: [] };
        items.push(it); i++;
        while (i < lines.length) {
          const l = lines[i];
          if (!l.trim()) {
            const nx = lines.slice(i + 1).find((x) => x.trim());
            if (nx != null && indentOf(nx) > base) { it.sub.push(''); i++; continue; }
            break;
          }
          if (indentOf(l) > base) { it.sub.push(l); i++; continue; }
          break;
        }
      }
      const tag = ordered ? 'ol' : 'ul';
      out.push(`<${tag}${start > 1 ? ` start="${start}"` : ''}>${items.map((it) => {
        let t = it.text; let box = '';
        const tm = /^\[( |x|X)\]\s+/.exec(t);
        if (tm) { box = tm[1] === ' ' ? '☐ ' : '☑ '; t = t.slice(tm[0].length); }
        let subHtml = '';
        if (it.sub.length) {
          const min = Math.min(...it.sub.filter((x) => x.trim()).map(indentOf));
          const sub = it.sub.map((x) => x.slice(min));
          subHtml = depth < MD_MAX_DEPTH ? mdBlocks(sub, depth + 1).join('') : `<p>${esc(sub.join('\n'))}</p>`;
        }
        return `<li>${box}${mdInline(t)}${subHtml}</li>`;
      }).join('')}</${tag}>`);
      continue;
    }
    const buf = [line]; i++;
    while (i < lines.length && lines[i].trim() && !startsBlock(lines[i], lines[i + 1])) buf.push(lines[i++]);
    out.push(`<p>${buf.map(mdInline).join('<br>')}</p>`);
  }
  return out;
}

export const md = (src) => mdBlocks(String(src ?? '').replace(/\r\n?/g, '\n').split('\n')).join('');
export const mdEl = (src, cls = 'md') => { const el = h('div', { class: cls }); el.innerHTML = md(src); return el; };
