// Slash commands & direct shell (!) command registry, fuzzy/typo matcher, and argument completion.

export const GROUP_LABEL = {
  session: 'スレッド',
  shell: 'シェル',
  config: '設定',
  panel: 'パネル',
  agent: 'エージェント',
  ui: '表示・検索',
  provider: 'プロバイダ',
};

export const LOCAL_COMMANDS = [
  { name: 'compact', aliases: ['compress'], args: '[方針]', desc: '会話を圧縮してコンテキストを節約', icon: 'handoff', group: 'session', requiresThread: true },
  { name: 'sh', aliases: ['shell', 'bash', 'run'], args: '<コマンド>', desc: '作業ディレクトリでシェルコマンドを実行（! と同等）', icon: 'terminal', group: 'shell', needsArg: true },
  { name: 'new', aliases: ['clear', 'reset'], args: '', desc: '新規スレッドを開始', icon: 'compose', group: 'session' },
  { name: 'model', aliases: ['m'], args: '[モデル名]', desc: 'モデルを切り替え／ピッカーを開く', icon: 'bot', group: 'config' },
  { name: 'effort', aliases: [], args: '[レベル]', desc: '推論強度 (Effort) を変更', icon: 'gauge', group: 'config' },
  { name: 'plan', aliases: [], args: '[on|off]', desc: 'Plan モードを切り替え', icon: 'shield', group: 'config' },
  { name: 'mode', aliases: ['perm'], args: '[権限]', desc: '実行権限モードを変更', icon: 'shield', group: 'config' },
  { name: 'diff', aliases: [], args: '[all|latest]', desc: '右パネルの Git 差分を表示／範囲切替', icon: 'diff', group: 'panel', requiresThread: true },
  { name: 'context', aliases: ['ctx'], args: '', desc: '右パネルのコンテキスト詳細を表示', icon: 'info', group: 'panel', requiresThread: true },
  { name: 'swarm', aliases: [], args: '<指示>', desc: 'Swarm マルチエージェント協調ボードを起動', icon: 'swarm', group: 'agent', needsArg: true, sendToProvider: true },
  { name: 'stop', aliases: ['interrupt', 'cancel'], args: '', desc: '実行中のターンを停止', icon: 'stop', group: 'session', requiresThread: true },
  { name: 'fork', aliases: ['handoff'], args: '[タイトル]', desc: '文脈を引き継いで新しいスレッドへ分岐', icon: 'fork', group: 'session', requiresThread: true },
  { name: 'delegate', aliases: ['subagent'], args: '[タスク]', desc: '子スレッドに並列委譲', icon: 'split', group: 'agent', requiresThread: true },
  { name: 'title', aliases: ['rename'], args: '<タイトル>', desc: '現在のスレッドタイトルを変更', icon: 'compose', group: 'session', requiresThread: true, needsArg: true },
  { name: 'pin', aliases: [], args: '', desc: '現在のスレッドのピン留めを切り替え', icon: 'pin', group: 'session', requiresThread: true },
  { name: 'archive', aliases: [], args: '', desc: '現在のスレッドをアーカイブ／解除', icon: 'archive', group: 'session', requiresThread: true },
  { name: 'search', aliases: ['find'], args: '[語句]', desc: '全スレッドを全文検索', icon: 'search', group: 'ui' },
  { name: 'usage', aliases: ['cost'], args: '', desc: '使用量・コストを表示', icon: 'chart', group: 'ui' },
  { name: 'limits', aliases: ['usage-limits'], args: '', desc: 'プロバイダの使用上限を表示', icon: 'gauge', group: 'ui' },
  { name: 'theme', aliases: [], args: '[テーマ名]', desc: '配色テーマを切り替え', icon: 'palette', group: 'ui' },
  { name: 'help', aliases: ['?', 'commands'], args: '', desc: 'コマンド一覧とショートカットを表示', icon: 'info', group: 'ui' },
];

export const PROVIDER_COMMANDS = {
  antigravity: [
    { name: 'goal', args: '<目標>', desc: '目標達成まで自律的に継続実行', icon: 'trendUp', needsArg: true },
    { name: 'grill-me', args: '[テーマ]', desc: '対話形式で設計・要件を詰める', icon: 'brain' },
    { name: 'teamwork', args: '<タスク>', desc: 'エージェントチームで並列実行', icon: 'swarm', needsArg: true },
    { name: 'boost', args: '<タスク>', desc: '深い思考と検証モードで実行', icon: 'gauge', needsArg: true },
    { name: 'learn', args: '[内容]', desc: '学びや設定を永続化', icon: 'filePen' },
    { name: 'schedule', args: '<指示>', desc: '定期実行・タイマーを設定', icon: 'refresh', needsArg: true },
    { name: 'browser', args: '<指示>', desc: 'ブラウザ操作・Web 調査を実行', icon: 'globe', needsArg: true },
  ],
  claude: [
    { name: 'review', args: '[対象]', desc: 'コードレビューを実行', icon: 'diff' },
    { name: 'init', args: '', desc: 'CLAUDE.md を初期化・更新', icon: 'filePen' },
    { name: 'doctor', args: '', desc: '環境診断を実行', icon: 'info' },
  ],
  codex: [
    { name: 'review', args: '[対象]', desc: 'コードレビューを実行', icon: 'diff' },
    { name: 'init', args: '', desc: 'AGENTS.md を初期化', icon: 'filePen' },
  ],
  'open-code': [
    { name: 'review', args: '[対象]', desc: 'コードレビューを実行', icon: 'diff' },
    { name: 'init', args: '', desc: 'プロジェクト設定を初期化', icon: 'filePen' },
  ],
};

/** Returns local + active-provider commands (deduplicated by name). */
export function allCommands(provider) {
  const local = LOCAL_COMMANDS.map((c) => ({ ...c, scope: c.sendToProvider ? 'agent' : 'local' }));
  const seen = new Set(local.map((c) => c.name));
  const extra = (PROVIDER_COMMANDS[provider] || [])
    .filter((c) => !seen.has(c.name))
    .map((c) => ({ ...c, aliases: c.aliases || [], group: 'provider', scope: 'provider', provider, sendToProvider: true }));
  return [...local, ...extra];
}

/** Parses `!command` direct shell input. Returns null if input does not start with `!`. */
export function parseShellInput(text) {
  const m = /^\s*!([\s\S]*)$/.exec(String(text ?? ''));
  if (!m) return null;
  return { command: m[1].trim(), raw: m[1] };
}

/**
 * Parses `/command [arg]` input.
 * Paths containing a second slash in the first token (e.g. `/Users/apple/...`) are NOT treated as commands.
 */
export function parseSlashInput(text) {
  const raw = String(text ?? '');
  const m = /^\s*\/([^\s/]+)([ \t]+([\s\S]*))?$/.exec(raw);
  if (!m) return null;
  const rawName = m[1];
  const hasSpace = m[2] != null;
  const arg = (m[3] || '').trim();
  return { name: rawName.toLowerCase(), rawName, arg, hasSpace };
}

/** Exact command lookup by name or alias. */
export function findCommand(name, provider) {
  const q = String(name || '').toLowerCase();
  if (!q) return null;
  return allCommands(provider).find((c) => c.name === q || (c.aliases || []).includes(q)) || null;
}

/** Levenshtein edit distance with an early cutoff for short command names. */
function editDistance(a, b, max = 2) {
  if (Math.abs(a.length - b.length) > max) return max + 1;
  let prev = Array.from({ length: b.length + 1 }, (_, i) => i);
  for (let i = 1; i <= a.length; i++) {
    const cur = [i];
    let rowMin = i;
    for (let j = 1; j <= b.length; j++) {
      const cost = a[i - 1] === b[j - 1] ? 0 : 1;
      const v = Math.min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + cost);
      cur.push(v);
      if (v < rowMin) rowMin = v;
    }
    if (rowMin > max) return max + 1;
    prev = cur;
  }
  return prev[b.length];
}

function scoreCommand(cmd, q) {
  if (!q) return 100;
  const names = [cmd.name, ...(cmd.aliases || [])];
  for (let i = 0; i < names.length; i++) {
    const n = names[i];
    if (n === q) return i === 0 ? 0 : 1;
    if (n.startsWith(q)) return i === 0 ? 10 + (n.length - q.length) * 0.01 : 15 + (n.length - q.length) * 0.01;
  }
  for (const n of names) {
    if (n.includes(q)) return 30;
  }
  if (cmd.desc.toLowerCase().includes(q)) return 40;
  if (q.length >= 3) {
    let best = 99;
    for (const n of names) {
      const d = editDistance(q, n, q.length >= 5 ? 2 : 1);
      if (d <= (q.length >= 5 ? 2 : 1)) best = Math.min(best, 50 + d);
    }
    if (best < 99) return best;
  }
  return -1;
}

const MODE_OPTIONS = [
  { value: 'approval-required', label: 'approval-required', desc: '確認あり（実行前に承認を求める）' },
  { value: 'auto-accept-edits', label: 'auto-accept-edits', desc: '編集自動（ファイル編集のみ自動承認）' },
  { value: 'full-access', label: 'full-access', desc: 'フルアクセス（すべて自動実行）' },
];

const PLAN_OPTIONS = [
  { value: 'on', label: 'on', desc: 'Plan モードを有効化（変更せず調査・計画のみ）' },
  { value: 'off', label: 'off', desc: 'Plan モードを解除（通常実行に戻す）' },
];

const DIFF_OPTIONS = [
  { value: 'all', label: 'all', desc: '作業ツリーのすべての変更を表示' },
  { value: 'latest', label: 'latest', desc: '最新ターンで変更されたファイルのみ表示' },
];

/**
 * Computes inline palette suggestions for the current textarea value.
 * Returns null when the palette should stay closed.
 */
export function getPaletteSuggestions(text, ctx = {}) {
  const raw = String(text ?? '');
  if (!raw || raw.includes('\n')) return null;

  // Direct shell history suggestions when user starts typing `!`
  const shell = parseShellInput(raw);
  if (shell) {
    const hist = Array.isArray(ctx.shellHistory) ? ctx.shellHistory : [];
    const q = shell.command.toLowerCase();
    const items = hist
      .filter((cmd) => !q || cmd.toLowerCase().includes(q))
      .slice(0, 6)
      .map((cmd) => ({
        kind: 'shell',
        insert: `!${cmd}`,
        execute: true,
        label: `!${cmd}`,
        desc: '最近のシェルコマンド',
        icon: 'terminal',
      }));
    return items.length ? { mode: 'shell', items } : null;
  }

  const trimmedStart = raw.replace(/^[ \t]+/, '');
  if (!trimmedStart.startsWith('/')) return null;
  // Bare `/` opens full command list
  if (trimmedStart === '/') {
    const cmds = allCommands(ctx.provider).filter((c) => !c.requiresThread || ctx.hasThread);
    return {
      mode: 'command',
      items: cmds.map((c) => commandItem(c, false)),
    };
  }

  const parsed = parseSlashInput(raw);
  if (!parsed) return null;

  // Argument autocomplete after `/cmd `
  if (parsed.hasSpace) {
    const cmd = findCommand(parsed.name, ctx.provider);
    if (!cmd) return null;
    const argQ = parsed.arg.toLowerCase();
    let opts = null;
    if (cmd.name === 'theme') {
      opts = (ctx.themes || []).map(([id, label]) => ({ value: id, label: id, desc: label }));
    } else if (cmd.name === 'effort') {
      opts = (ctx.effortLevels || []).map((lvl) => ({ value: lvl, label: lvl, desc: `推論強度を ${lvl} に設定` }));
    } else if (cmd.name === 'mode') {
      opts = MODE_OPTIONS;
    } else if (cmd.name === 'plan') {
      opts = PLAN_OPTIONS;
    } else if (cmd.name === 'diff') {
      opts = DIFF_OPTIONS;
    } else if (cmd.name === 'model') {
      opts = (ctx.models || []).map((m) => ({
        value: `${m.provider}/${m.id}`,
        label: m.label || m.id,
        desc: `${m.providerLabel} · ${m.id}`,
        provider: m.provider,
        search: `${m.label} ${m.id} ${m.provider} ${m.providerLabel}`.toLowerCase(),
      }));
    }
    if (!opts) return null;
    const matched = opts
      .filter((o) => !argQ || (o.search || `${o.value} ${o.label} ${o.desc}`.toLowerCase()).includes(argQ))
      .slice(0, 10);
    if (!matched.length) return null;
    return {
      mode: 'arg',
      command: cmd,
      items: matched.map((o) => ({
        kind: 'arg',
        insert: `/${cmd.name} ${o.value}`,
        execute: true,
        label: o.label,
        desc: o.desc,
        icon: cmd.icon,
        provider: o.provider,
        cmd,
      })),
    };
  }

  // Command name autocomplete while typing `/foo`
  const q = parsed.name;
  const cmds = allCommands(ctx.provider)
    .filter((c) => !c.requiresThread || ctx.hasThread)
    .map((c) => ({ c, score: scoreCommand(c, q) }))
    .filter((x) => x.score >= 0)
    .sort((a, b) => a.score - b.score || a.c.name.localeCompare(b.c.name))
    .slice(0, 10);

  if (!cmds.length) return null;
  return {
    mode: 'command',
    items: cmds.map(({ c, score }) => commandItem(c, score >= 50)),
  };
}

function commandItem(c, fuzzy) {
  return {
    kind: 'command',
    cmd: c,
    insert: c.needsArg || c.args ? `/${c.name} ` : `/${c.name}`,
    execute: !c.needsArg,
    label: `/${c.name}`,
    args: c.args || '',
    desc: c.desc,
    icon: c.icon,
    group: GROUP_LABEL[c.group] || c.group,
    provider: c.provider || null,
    fuzzy,
  };
}

const SHELL_HIST_KEY = 't3.shhist';
const SHELL_HIST_MAX = 20;

export function loadShellHistory(ls) {
  const arr = ls.json(SHELL_HIST_KEY, []);
  return Array.isArray(arr) ? arr.filter((x) => typeof x === 'string' && x.trim()).slice(0, SHELL_HIST_MAX) : [];
}

export function pushShellHistory(ls, command) {
  const cmd = String(command || '').trim();
  if (!cmd) return loadShellHistory(ls);
  const next = [cmd, ...loadShellHistory(ls).filter((x) => x !== cmd)].slice(0, SHELL_HIST_MAX);
  ls.set(SHELL_HIST_KEY, JSON.stringify(next));
  return next;
}
