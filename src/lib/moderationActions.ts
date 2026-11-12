/**
 * Pure helpers for stream-side moderation (delete / timeout / ban).
 * No Svelte or Tauri imports so `node --test` can load this file directly.
 */

export type ModerationErrorCode =
  | 'notLoggedIn'
  | 'scopeMissing'
  | 'notModerator'
  | 'protectedTarget'
  | 'notFound'
  | 'unavailable'
  | 'rateLimited'
  | 'conflict'
  | 'network'
  | 'other';

export interface ModerationError {
  code: ModerationErrorCode;
  message: string;
}

export interface ModerationAuth {
  youtubeConnected: boolean;
}

export interface ModerationTargetMessage {
  platform: string;
  kind: string;
  id: string;
  nativeId?: boolean;
  author: { id: string; roles: { broadcaster: boolean; moderator: boolean } };
}

export interface ActionAvailability {
  enabled: boolean;
  /** Tooltip explaining why the action is disabled (undefined when enabled). */
  reason?: string;
}

export interface ModerationAvailability {
  /** false = do not render any moderation item (X / niconico / system). */
  visible: boolean;
  delete: ActionAvailability;
  timeout: ActionAvailability;
  ban: ActionAvailability;
}

/** Timeout presets in seconds, shared by the user card. */
export const TIMEOUT_PRESETS: readonly number[] = [60, 600, 3600, 86400];
/** Context-menu quick timeout (seconds). */
export const QUICK_TIMEOUT_SECS = 600;

export const TWITCH_PENDING_REASON = 'Twitch のモデレーションは次の更新で対応';
export const DELETE_UNSUPPORTED_REASON =
  'この受信方式のコメントは削除できません(YouTube API キー設定時の受信分のみ)';

const HIDDEN: ModerationAvailability = {
  visible: false,
  delete: { enabled: false },
  timeout: { enabled: false },
  ban: { enabled: false },
};

function allDisabled(reason: string): ModerationAvailability {
  const a = { enabled: false, reason };
  return { visible: true, delete: a, timeout: a, ban: a };
}

export function moderationAvailability(
  msg: ModerationTargetMessage,
  auth: ModerationAuth,
): ModerationAvailability {
  const platform = msg.platform.trim().toLowerCase();
  if (msg.kind === 'system') return HIDDEN;
  if (platform !== 'youtube' && platform !== 'twitch') return HIDDEN;
  if (platform === 'twitch') return allDisabled(TWITCH_PENDING_REASON);
  if (!auth.youtubeConnected) return allDisabled('設定で YouTube(Google アカウント)に接続してください');
  if (!msg.author.id) return allDisabled('投稿者を特定できません');
  const roles = msg.author.roles;
  if (roles.broadcaster || roles.moderator) return allDisabled('配信者・モデレーターは対象外');

  const ok = { enabled: true };
  const canDelete = msg.nativeId === true && msg.id.startsWith('LCC.');
  return {
    visible: true,
    delete: canDelete ? ok : { enabled: false, reason: DELETE_UNSUPPORTED_REASON },
    timeout: ok,
    ban: ok,
  };
}

/** "60" -> "1分", "600" -> "10分", "3600" -> "1時間", "86400" -> "24時間". */
export function formatTimeoutLabel(secs: number): string {
  if (secs % 3600 === 0) return `${secs / 3600}時間`;
  if (secs % 60 === 0) return `${secs / 60}分`;
  return `${secs}秒`;
}

/** Focus-return target shape (any HTMLElement satisfies it). */
export interface FocusCandidate {
  isConnected: boolean;
  disabled?: boolean;
}

/**
 * Focus-return candidates in priority order, dropping missing, detached
 * (`!isConnected`, e.g. a recycled virtual-list row) and disabled elements.
 */
export function focusReturnCandidates<T extends FocusCandidate>(...candidates: (T | null | undefined)[]): T[] {
  return candidates.filter((c): c is T => !!c && c.isConnected && c.disabled !== true);
}

const KNOWN_CODES: ReadonlySet<string> = new Set([
  'notLoggedIn', 'scopeMissing', 'notModerator', 'protectedTarget', 'notFound',
  'unavailable', 'rateLimited', 'conflict', 'network', 'other',
]);

/**
 * Normalise an invoke rejection. Tauri v2 rejects with the serialized error value:
 * an object `{code, message}` for ModerationError, a bare string for `Err(String)`.
 */
export function toModerationError(e: unknown): ModerationError {
  if (typeof e === 'string') return { code: 'other', message: e || '操作に失敗しました' };
  if (e && typeof e === 'object') {
    const o = e as { code?: unknown; message?: unknown };
    const message = typeof o.message === 'string' && o.message ? o.message : '操作に失敗しました';
    const code = typeof o.code === 'string' && KNOWN_CODES.has(o.code) ? (o.code as ModerationErrorCode) : 'other';
    return { code, message };
  }
  return { code: 'other', message: '操作に失敗しました' };
}
