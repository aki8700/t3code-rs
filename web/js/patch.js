// Diff patch renderer (with syntax highlighting) and turn-scoped file path extraction.
import { h, fmtNum, kindLetter, pstr } from './util.js';
import { highlightLine, langFromPath } from './highlight.js';

export function fileEntries(p) {
  const raw = p?.files ?? p?.changes ?? p?.paths ?? (p?.path || p?.file_path ? [p] : []);
  const arr = Array.isArray(raw) ? raw : typeof raw === 'object' && raw ? Object.entries(raw).map(([path, v]) => ({ path, ...(typeof v === 'object' ? v : { kind: v }) })) : [];
  return arr.map((f) => (typeof f === 'string' ? { path: f } : f)).filter(Boolean)
    .map((f) => ({ ...f, path: f.path || f.file_path || f.file || f.name })).filter((f) => f.path);
}

export function splitDiff(diff) {
  const lines = String(diff || '').split('\n');
  const parts = [];
  let cur = null;
  lines.forEach((l, i) => {
    const git = l.startsWith('diff --git ');
    const pair = l.startsWith('--- ') && lines[i + 1]?.startsWith('+++ ') && lines[i + 2]?.startsWith('@@');
    if (!cur || git || (pair && cur.hunk)) { cur = { lines: [], hunk: false, path: '', old: '' }; parts.push(cur); }
    if (!cur.hunk && l.startsWith('--- ')) cur.old = l.slice(4).split('\t')[0].replace(/^a\//, '');
    if (!cur.hunk && l.startsWith('+++ ')) cur.path = l.slice(4).split('\t')[0].replace(/^b\//, '');
    if (l.startsWith('@@')) cur.hunk = true;
    cur.lines.push(l);
  });
  return parts.map((x) => ({ patch: x.lines.join('\n'), path: x.path && x.path !== '/dev/null' ? x.path : x.old, added: x.old === '/dev/null', deleted: x.path === '/dev/null' }))
    .filter((x) => x.patch.trim());
}

export function countPatch(patch) {
  let add = 0; let del = 0;
  const lines = String(patch || '').split('\n');
  let inHunk = !lines.some((l) => l.startsWith('@@'));
  for (const l of lines) {
    if (l.startsWith('@@')) inHunk = true;
    else if (l.startsWith('diff ')) inHunk = false;
    else if (inHunk && l.startsWith('+') && !l.startsWith('+++ ')) add += 1;
    else if (inHunk && l.startsWith('-') && !l.startsWith('--- ')) del += 1;
  }
  return { add, del };
}

export function changedFiles(p) {
  const files = fileEntries(p).map((f) => ({ ...f, patch: typeof f.diff === 'string' ? f.diff : typeof f.patch === 'string' ? f.patch : '' }));
  if (typeof p?.diff === 'string' && p.diff && !files.some((f) => f.patch)) {
    const parts = splitDiff(p.diff);
    if (parts.length === 1 && files.length <= 1) {
      if (files.length) Object.assign(files[0], { patch: parts[0].patch, added: parts[0].added, deleted: parts[0].deleted });
      else files.push({ ...parts[0], path: parts[0].path || p.path || '(不明なファイル)' });
    } else {
      for (const part of parts) {
        const hit = files.find((f) => !f.patch && part.path && (f.path === part.path || f.path.endsWith(`/${part.path}`)));
        if (hit) Object.assign(hit, { patch: part.patch, added: part.added, deleted: part.deleted });
        else files.push({ ...part, path: part.path || p.path || '(不明なファイル)' });
      }
    }
  }
  return files.map((f) => {
    const n = f.patch ? countPatch(f.patch) : null;
    const created = f.added === true || /^--- \/dev\/null/m.test(f.patch);
    const removed = f.deleted === true || /^\+\+\+ \/dev\/null/m.test(f.patch);
    return {
      ...f,
      add: n ? n.add : Number(f.additions ?? f.added_lines) || 0,
      del: n ? n.del : Number(f.deletions ?? f.removed_lines) || 0,
      letter: kindLetter(f.kind ?? f.status ?? f.change_type ?? (created ? 'A' : removed ? 'D' : f.operation)),
    };
  });
}

export function patchLines(patch, max = 3000, filePath = '') {
  const lines = String(patch || '').split('\n');
  if (lines.at(-1) === '') lines.pop();
  const lang = langFromPath(filePath);
  const frag = document.createDocumentFragment();
  let a = 0; let b = 0; let numbered = false;
  let inHunk = !lines.some((l) => l.startsWith('@@'));
  const row = (cls, o, n, sign, text) => {
    const tx = h('span', { class: 'tx' });
    if (lang) tx.innerHTML = highlightLine(text, lang);
    else tx.textContent = text;
    frag.append(h('div', { class: `dl ${cls}` },
      h('span', { class: 'ln', text: o }), h('span', { class: 'ln', text: n }), h('span', { class: 'sg', text: sign }), tx));
  };
  for (const l of lines.slice(0, max)) {
    if (l.startsWith('@@')) {
      const hunk = /^@@+ -(\d+)(?:,\d+)? (?:-\d+(?:,\d+)? )*\+(\d+)/.exec(l);
      numbered = !!hunk;
      if (hunk) { a = +hunk[1]; b = +hunk[2]; }
      inHunk = true; frag.append(h('div', { class: 'dl hunk', text: l })); continue;
    }
    if (l.startsWith('diff ')) inHunk = false;
    if (!inHunk) { if (/^(rename|copy|new file|deleted file|old mode|new mode|Binary)/.test(l)) frag.append(h('div', { class: 'dl meta', text: l })); continue; }
    if (l.startsWith('+')) row('add', '', numbered ? b++ : '', '+', l.slice(1));
    else if (l.startsWith('-')) row('del', numbered ? a++ : '', '', '−', l.slice(1));
    else if (l.startsWith('\\')) frag.append(h('div', { class: 'dl meta', text: l }));
    else row('', numbered ? a++ : '', numbered ? b++ : '', '', l.slice(1));
  }
  if (!frag.childNodes.length) frag.append(h('div', { class: 'dl meta', text: '（パッチ本文なし：バイナリ or 空白のみの変更）' }));
  const inner = h('div', { class: 'patch-in' }, frag);
  const box = h('div', { class: 'patch' }, inner);
  if (lines.length > max) {
    box.append(h('button', { type: 'button', class: 'btn xs out-more', text: `残り ${fmtNum(lines.length - max)} 行を表示`, onclick: () => box.replaceWith(patchLines(patch, Infinity, filePath)) }));
  }
  return box;
}

const EDIT_TOOL_RE = /^(edit|write|multiedit|multi_edit|notebookedit|write_file|edit_file|replace|patch|apply_patch|str_replace_based_edit_tool)$/i;
const normPath = (p) => String(p || '').replace(/\\/g, '/').replace(/^\.\//, '');

/** Extracts file paths touched during the latest turn (run) of a thread. */
export function latestTurnChangedPaths(detail, workspaceRoot = '') {
  const runs = detail?.runs || [];
  const items = detail?.items || [];
  const lastRun = runs.at(-1);
  if (!lastRun) return [];
  const root = normPath(workspaceRoot).replace(/\/$/, '');
  const paths = new Set();
  const add = (raw) => {
    let p = normPath(raw);
    if (!p) return;
    if (root && p.startsWith(`${root}/`)) p = p.slice(root.length + 1);
    paths.add(p);
  };
  for (const it of items) {
    if (it.run_id !== lastRun.id) continue;
    const p = it.payload;
    if (it.kind === 'file_change' || (it.kind === 'system_notice' && p?.type === 'checkpoint')) {
      for (const f of fileEntries(p)) add(f.path);
    } else if (it.kind === 'tool_call') {
      const name = pstr(p, 'name', 'tool', 'tool_name', 'title');
      if (!EDIT_TOOL_RE.test(name)) continue;
      const args = p?.input ?? p?.arguments ?? p?.args ?? p?.params ?? p?.state?.input ?? p;
      const fp = args?.Path ?? args?.path ?? args?.file_path ?? args?.filePath ?? args?.target_file ?? args?.filename;
      if (typeof fp === 'string') add(fp);
    }
  }
  return [...paths];
}

export function filterDiffFilesByPaths(files, turnPaths) {
  if (!turnPaths.length) return [];
  return files.filter((f) => {
    const fp = normPath(f.path);
    return turnPaths.some((tp) => fp === tp || tp.endsWith(`/${fp}`) || fp.endsWith(`/${tp}`));
  });
}
