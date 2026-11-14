// X 自動告知の UI 側の純粋な判定。告知するかどうかは Rust 側が決め、ここは
// 表示(印ボタンの可否)と保存前の値の整形だけを持つ。

/** Rust `XPostConfig` の数値欄(u32)。 */
export interface XPostNumbers {
  delaySecs: number;
  staleMinutes: number;
  cooldownMinutes: number;
  dailyLimit: number;
}

const XPOST_NUMBER_DEFAULTS: XPostNumbers = {
  delaySecs: 60,
  staleMinutes: 15,
  cooldownMinutes: 60,
  dailyLimit: 5,
};

const U32_MAX = 0xffffffff;

/**
 * 数値欄を u32 に収める(小数は切り捨て、負数は 0、非数は既定値)。範囲の丸めは
 * Rust の `normalize()` が行う。ここで整えないと 2.5 や -1 で設定全体の保存が失敗する。
 */
export function sanitizeXPostNumbers<T extends XPostNumbers>(xPost: T): T {
  const next = { ...xPost };
  for (const key of Object.keys(XPOST_NUMBER_DEFAULTS) as (keyof XPostNumbers)[]) {
    const n = Number(xPost[key]);
    next[key] = (Number.isFinite(n)
      ? Math.min(U32_MAX, Math.max(0, Math.trunc(n)))
      : XPOST_NUMBER_DEFAULTS[key]) as T[keyof XPostNumbers];
  }
  return next;
}

// Rust `coordinator::youtube_channel` と同じ規則(@ハンドル・UC チャンネル ID・
// youtube.com/@x・youtube.com/channel/UC…、前後空白は無視)。
function isChannelId(id: string): boolean {
  return /^UC[A-Za-z0-9_-]{22}$/.test(id);
}

function isHandle(value: string): boolean {
  return value.split(/[/?#]/)[0].trim().replace(/^@+/, '') !== '';
}

export function isYoutubeChannelIdentifier(identifier: string): boolean {
  const id = identifier.trim();
  if (id.startsWith('@')) return isHandle(id.slice(1));
  if (isChannelId(id)) return true;
  let url: URL;
  try {
    url = new URL(id.includes('://') ? id : `https://${id}`);
  } catch {
    return false;
  }
  if (!['youtube.com', 'www.youtube.com', 'm.youtube.com'].includes(url.hostname)) return false;
  const parts = url.pathname.split('/').filter((s) => s !== '');
  const first = parts[0];
  if (first === undefined) return false;
  if (first.startsWith('@')) return isHandle(first.slice(1));
  if (first.toLowerCase() === 'channel') return parts[1] !== undefined && isChannelId(parts[1]);
  return false;
}

/** 「自分の配信」印が付けられない行なら理由を返す。 */
export function ownStreamBlockedReason(ch: { platform: string; identifier: string }): string | null {
  if (ch.platform === 'x') return 'X の行は告知の対象外です';
  if (ch.platform === 'youtube' && !isYoutubeChannelIdentifier(ch.identifier)) {
    return 'videoId 直接指定の行は告知の対象外です(@ハンドルかチャンネル ID で追加してください)';
  }
  return null;
}

/** コールバックポートの入力を検査する。1024〜65535 の整数だけを通す(それ以外は null)。 */
export function parseCallbackPort(input: unknown): number | null {
  const text = typeof input === 'number' ? String(input) : typeof input === 'string' ? input.trim() : '';
  if (!/^\d+$/.test(text)) return null;
  const port = Number(text);
  return port >= 1024 && port <= 65535 ? port : null;
}

/** API モードを選べるか(接続済みの時だけ)。Rust の `update_config` も同じ条件で拒否する。 */
export function canSelectApiMode(status: { connected?: boolean } | null | undefined): boolean {
  return status?.connected === true;
}

/** invoke の reject 値(Rust `XPostError` か文字列か Error)を表示用の文に直す。 */
export function errorText(e: unknown): string {
  if (e && typeof e === 'object' && 'message' in e) {
    const message = (e as { message?: unknown }).message;
    if (typeof message === 'string' && message) return message;
  }
  return typeof e === 'string' ? e : String(e);
}

/** テスト投稿の確認ダイアログに出す文面(実際の文面は Rust が送信時刻で作る)。 */
export function testPostPreview(now: Date): string {
  const pad = (n: number) => String(n).padStart(2, '0');
  return `fast-comment 連携テスト ${now.getFullYear()}/${pad(now.getMonth() + 1)}/${pad(now.getDate())} ${pad(now.getHours())}:${pad(now.getMinutes())}`;
}
