// Lightweight zero-dependency syntax highlighter (safe: escapes every token before emitting HTML).
import { esc } from './util.js';

const ALIASES = {
  rs: 'rust', rust: 'rust',
  js: 'js', mjs: 'js', cjs: 'js', jsx: 'js', ts: 'js', tsx: 'js', mts: 'js', javascript: 'js', typescript: 'js',
  py: 'python', python: 'python',
  go: 'go', golang: 'go',
  sh: 'sh', bash: 'sh', zsh: 'sh', shell: 'sh',
  json: 'json', jsonc: 'json',
  toml: 'toml', ini: 'toml',
  yml: 'yaml', yaml: 'yaml',
  sql: 'sql',
  css: 'css', scss: 'css',
  html: 'html', htm: 'html', xml: 'html', svg: 'html',
  c: 'clike', h: 'clike', cpp: 'clike', cc: 'clike', hpp: 'clike', java: 'clike', swift: 'clike', kt: 'clike',
};

const KW = {
  rust: new Set(['as', 'async', 'await', 'break', 'const', 'continue', 'crate', 'dyn', 'else', 'enum', 'extern', 'false', 'fn', 'for', 'if', 'impl', 'in', 'let', 'loop', 'match', 'mod', 'move', 'mut', 'pub', 'ref', 'return', 'self', 'Self', 'static', 'struct', 'super', 'trait', 'true', 'type', 'unsafe', 'use', 'where', 'while']),
  js: new Set(['async', 'await', 'break', 'case', 'catch', 'class', 'const', 'continue', 'default', 'delete', 'do', 'else', 'enum', 'export', 'extends', 'false', 'finally', 'for', 'from', 'function', 'if', 'implements', 'import', 'in', 'instanceof', 'interface', 'let', 'new', 'null', 'of', 'private', 'protected', 'public', 'readonly', 'return', 'static', 'super', 'switch', 'this', 'throw', 'true', 'try', 'type', 'typeof', 'undefined', 'var', 'void', 'while', 'yield']),
  python: new Set(['and', 'as', 'assert', 'async', 'await', 'break', 'class', 'continue', 'def', 'del', 'elif', 'else', 'except', 'False', 'finally', 'for', 'from', 'global', 'if', 'import', 'in', 'is', 'lambda', 'None', 'nonlocal', 'not', 'or', 'pass', 'raise', 'return', 'True', 'try', 'while', 'with', 'yield']),
  go: new Set(['break', 'case', 'chan', 'const', 'continue', 'default', 'defer', 'else', 'fallthrough', 'false', 'for', 'func', 'go', 'goto', 'if', 'import', 'interface', 'map', 'nil', 'package', 'range', 'return', 'select', 'struct', 'switch', 'true', 'type', 'var']),
  sh: new Set(['case', 'do', 'done', 'elif', 'else', 'esac', 'export', 'fi', 'for', 'function', 'if', 'in', 'local', 'readonly', 'return', 'set', 'then', 'unset', 'until', 'while']),
  sql: new Set(['SELECT', 'FROM', 'WHERE', 'INSERT', 'INTO', 'VALUES', 'UPDATE', 'SET', 'DELETE', 'CREATE', 'TABLE', 'INDEX', 'IF', 'NOT', 'EXISTS', 'PRIMARY', 'KEY', 'FOREIGN', 'REFERENCES', 'ON', 'CASCADE', 'JOIN', 'LEFT', 'INNER', 'ORDER', 'BY', 'GROUP', 'LIMIT', 'OFFSET', 'AS', 'AND', 'OR', 'IS', 'NULL', 'UNIQUE', 'CONFLICT', 'DO', 'RETURNING', 'DESC', 'ASC', 'VIRTUAL', 'USING', 'PRAGMA', 'CHECK', 'DEFAULT', 'COALESCE']),
  clike: new Set(['auto', 'bool', 'break', 'case', 'catch', 'char', 'class', 'const', 'continue', 'default', 'do', 'double', 'else', 'enum', 'extern', 'false', 'final', 'float', 'for', 'func', 'guard', 'if', 'import', 'int', 'let', 'long', 'namespace', 'new', 'nil', 'null', 'override', 'private', 'protected', 'public', 'return', 'short', 'signed', 'sizeof', 'static', 'struct', 'switch', 'template', 'this', 'throw', 'true', 'try', 'typedef', 'typename', 'unsigned', 'using', 'var', 'virtual', 'void', 'while']),
};

const HASH_COMMENT = new Set(['python', 'sh', 'toml', 'yaml']);
const SLASH_COMMENT = new Set(['rust', 'js', 'go', 'css', 'clike', 'json']);
const MAX_LINE_LEN = 1200;

export function normLang(lang) {
  return ALIASES[String(lang || '').trim().toLowerCase()] || '';
}

export function langFromPath(path) {
  const name = String(path || '').split('/').pop() || '';
  if (/^(Makefile|Dockerfile|Containerfile)$/i.test(name)) return 'sh';
  const dot = name.lastIndexOf('.');
  return dot > 0 ? normLang(name.slice(dot + 1)) : '';
}

const span = (cls, text) => `<span class="hl-${cls}">${esc(text)}</span>`;

function highlightHtmlLine(line) {
  let out = '';
  let i = 0;
  while (i < line.length) {
    if (line.startsWith('<!--', i)) {
      const end = line.indexOf('-->', i + 4);
      const to = end < 0 ? line.length : end + 3;
      out += span('cmt', line.slice(i, to));
      i = to;
      continue;
    }
    if (line[i] === '<') {
      const m = /^<\/?[A-Za-z][\w:-]*/.exec(line.slice(i));
      if (m) {
        out += span('tag', m[0]);
        i += m[0].length;
        continue;
      }
    }
    if (line[i] === '"' || line[i] === "'") {
      const q = line[i];
      let j = i + 1;
      while (j < line.length && line[j] !== q) { if (line[j] === '\\') j += 1; j += 1; }
      out += span('str', line.slice(i, Math.min(line.length, j + 1)));
      i = j + 1;
      continue;
    }
    const attr = /^([A-Za-z_:][\w:.-]*)(?==)/.exec(line.slice(i));
    if (attr) {
      out += span('attr', attr[1]);
      i += attr[1].length;
      continue;
    }
    out += esc(line[i++]);
  }
  return out;
}

function highlightCssLine(line) {
  let out = '';
  let i = 0;
  while (i < line.length) {
    if (line.startsWith('/*', i)) {
      const end = line.indexOf('*/', i + 2);
      const to = end < 0 ? line.length : end + 2;
      out += span('cmt', line.slice(i, to));
      i = to;
      continue;
    }
    if (line[i] === '"' || line[i] === "'") {
      const q = line[i]; let j = i + 1;
      while (j < line.length && line[j] !== q) { if (line[j] === '\\') j += 1; j += 1; }
      out += span('str', line.slice(i, Math.min(line.length, j + 1)));
      i = j + 1; continue;
    }
    const prop = /^(--[\w-]+|[a-z-]+)(?=\s*:)/.exec(line.slice(i));
    if (prop && (i === 0 || /[\s{;]/.test(line[i - 1]))) {
      out += span('attr', prop[1]);
      i += prop[1].length; continue;
    }
    const at = /^@[a-z-]+/.exec(line.slice(i));
    if (at) { out += span('kw', at[0]); i += at[0].length; continue; }
    const num = /^(?:#[0-9a-fA-F]{3,8}\b|\b\d+(?:\.\d+)?(?:px|rem|em|vh|vw|dvh|%|s|ms|fr|ch)?\b)/.exec(line.slice(i));
    if (num) { out += span('num', num[0]); i += num[0].length; continue; }
    out += esc(line[i++]);
  }
  return out;
}

export function highlightLine(line, lang) {
  const id = normLang(lang);
  if (!id || !line || line.length > MAX_LINE_LEN) return esc(line);
  if (id === 'html') return highlightHtmlLine(line);
  if (id === 'css') return highlightCssLine(line);
  const kws = KW[id];
  let out = '';
  let i = 0;
  while (i < line.length) {
    const ch = line[i];
    // Comments
    if (SLASH_COMMENT.has(id) && line.startsWith('//', i)) {
      out += span('cmt', line.slice(i)); break;
    }
    if (SLASH_COMMENT.has(id) && line.startsWith('/*', i)) {
      const end = line.indexOf('*/', i + 2);
      const to = end < 0 ? line.length : end + 2;
      out += span('cmt', line.slice(i, to)); i = to; continue;
    }
    if (id === 'sql' && line.startsWith('--', i)) {
      out += span('cmt', line.slice(i)); break;
    }
    if (HASH_COMMENT.has(id) && ch === '#' && (i === 0 || /\s/.test(line[i - 1]))) {
      out += span('cmt', line.slice(i)); break;
    }
    // Strings
    if (ch === '"' || ch === '`' || (ch === "'" && id !== 'rust')) {
      let j = i + 1;
      while (j < line.length && line[j] !== ch) { if (line[j] === '\\') j += 1; j += 1; }
      const raw = line.slice(i, Math.min(line.length, j + 1));
      const isJsonKey = id === 'json' && ch === '"' && /^\s*:/.test(line.slice(j + 1));
      out += span(isJsonKey ? 'attr' : 'str', raw);
      i = j + 1; continue;
    }
    // Rust attributes / macros / lifetimes
    if (id === 'rust' && ch === '#' && line[i + 1] === '[') {
      out += span('attr', '#['); i += 2; continue;
    }
    // TOML / YAML keys & section headers
    if ((id === 'toml' || id === 'yaml') && i === 0) {
      const sec = /^(\s*\[+[^\]]+\]+)/.exec(line);
      if (sec) { out += span('type', sec[1]); i += sec[1].length; continue; }
      const key = /^(\s*[A-Za-z0-9_.-]+)(\s*[:=])/.exec(line);
      if (key) { out += span('attr', key[1]) + esc(key[2]); i += key[0].length; continue; }
    }
    // Numbers
    if (/[0-9]/.test(ch) && (i === 0 || !/[A-Za-z0-9_$]/.test(line[i - 1]))) {
      const num = /^(?:0x[0-9a-fA-F_]+|0b[01_]+|\d[\d_]*(?:\.[\d_]+)?(?:[eE][+-]?\d+)?)/.exec(line.slice(i));
      if (num) { out += span('num', num[0]); i += num[0].length; continue; }
    }
    // Identifiers / keywords / types / functions
    if (/[A-Za-z_$]/.test(ch)) {
      const idm = /^[A-Za-z_$][A-Za-z0-9_$]*/.exec(line.slice(i));
      const word = idm[0];
      const next = line[i + word.length];
      if (id === 'json' && (word === 'true' || word === 'false' || word === 'null')) {
        out += span('kw', word);
      } else if (id === 'sql' && kws?.has(word.toUpperCase())) {
        out += span('kw', word);
      } else if (kws?.has(word)) {
        out += span('kw', word);
      } else if (id === 'rust' && next === '!') {
        out += span('fn', `${word}!`); i += word.length + 1; continue;
      } else if (/^[A-Z][A-Za-z0-9_]*$/.test(word) && word.length > 1 && /[a-z]/.test(word)) {
        out += span('type', word);
      } else if (next === '(') {
        out += span('fn', word);
      } else {
        out += esc(word);
      }
      i += word.length; continue;
    }
    out += esc(ch);
    i += 1;
  }
  return out;
}

export function highlightCode(code, lang) {
  const id = normLang(lang);
  if (!id) return esc(code);
  return String(code ?? '').split('\n').map((l) => highlightLine(l, id)).join('\n');
}
