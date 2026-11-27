// OBS テンプレート一覧の表示・選択に関する判定(純関数)。UI は描画だけにする。
import type { TemplateInfo } from './ipc';

export const DEFAULT_TEMPLATE_ID = 'default';
const MAX_NAME_LEN = 64;

// 集計値を専用の WebSocket で受け取る内蔵テンプレ。それ以外は /ws(コメント)。
const BUILTIN_WS_PATHS: Readonly<Record<string, string>> = {
  goals: '/stats',
  'goals-neon': '/stats',
  'goals-retro': '/stats',
  timer: '/timer',
  poll: '/poll',
};

/** テンプレエディタのプレビュー / URL コピーで `ws` に載せるパス。カスタムは由来が分からないので /ws。 */
export function templateWsPath(id: string): string {
  return Object.hasOwn(BUILTIN_WS_PATHS, id) ? BUILTIN_WS_PATHS[id] : '/ws';
}

export function templateKindLabel(kind: TemplateInfo['kind']): string {
  return kind === 'builtin' ? '内蔵' : 'カスタム';
}

/** select の optgroup 用に内蔵 / カスタムへ分け、それぞれ名前順に並べる。 */
export function groupTemplates(list: readonly TemplateInfo[]): {
  builtin: TemplateInfo[];
  custom: TemplateInfo[];
} {
  const byName = (a: TemplateInfo, b: TemplateInfo) =>
    a.name < b.name ? -1 : a.name > b.name ? 1 : 0;
  return {
    builtin: list.filter((t) => t.kind === 'builtin').sort(byName),
    custom: list.filter((t) => t.kind === 'custom').sort(byName),
  };
}

/**
 * 複製先の名前候補。`<name>-copy`、衝突時は `<name>-copy-2`, `-3`, ...。
 * Rust 側の規則(英小文字・数字・`_`・`-`、先頭は英小文字か数字、64 字以内)に収める。
 * 衝突判定は大文字小文字を区別しない(Rust 側と同じ)。
 */
export function suggestCopyName(sourceName: string, existingCustomNames: readonly string[]): string {
  let base = sourceName
    .toLowerCase()
    .replace(/[^a-z0-9_-]/g, '-')
    .replace(/^[^a-z0-9]+/, '');
  if (base === '') base = 'template';
  const taken = new Set(existingCustomNames.map((n) => n.toLowerCase()));
  for (let i = 1; ; i++) {
    const suffix = i === 1 ? '-copy' : `-copy-${i}`;
    const candidate = base.slice(0, MAX_NAME_LEN - suffix.length) + suffix;
    if (!taken.has(candidate)) return candidate;
  }
}

/** 選択中の ID が一覧に無ければ 'default' に戻す。 */
export function resolveTemplateId(id: string, ids: readonly string[]): string {
  return ids.includes(id) ? id : DEFAULT_TEMPLATE_ID;
}

/** 複製の成功(または再取得後)に、作成した 1 件を一覧へ反映する。同じ ID があれば置き換える。 */
export function applyDuplicated(list: readonly TemplateInfo[], created: TemplateInfo): TemplateInfo[] {
  return [...list.filter((t) => t.id !== created.id), created];
}

/**
 * 削除の成功(または再取得後)に、一覧から ID を除き、選択値を直す。
 * current(選択中の ID や config.obs.template)が削除した ID なら 'default' に戻し、それ以外は変えない。
 */
export function applyDeleted(
  list: readonly TemplateInfo[],
  removedId: string,
  current: string
): { list: TemplateInfo[]; current: string } {
  const rest = list.filter((t) => t.id !== removedId);
  return {
    list: rest,
    current: current.trim() === removedId ? DEFAULT_TEMPLATE_ID : current,
  };
}
