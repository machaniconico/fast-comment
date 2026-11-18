/**
 * TypeScript mirror of src-tauri/src/model.rs
 * JSON is camelCase (serde rename_all = "camelCase")
 * Platform is lowercase (serde rename_all = "lowercase")
 * Fragment uses tagged union with `type` discriminant
 */

export type Platform = 'twitch' | 'youtube' | 'x' | 'niconico';

export type MessageKind = 'normal' | 'superChat' | 'membership' | 'bits' | 'gift' | 'system';

export interface Roles {
  broadcaster: boolean;
  moderator: boolean;
  member: boolean;
  subscriber: boolean;
  vip: boolean;
}

export interface Badge {
  kind: string;
  label: string;
  imageUrl?: string;
}

export interface Author {
  nickname?: string;
  avatarUrl?: string;
  id: string;
  name: string;
  displayColor?: string;
  badges: Badge[];
  roles: Roles;
}

export type Fragment =
  | { type: 'text'; text: string }
  | { type: 'emote'; id: string; name: string; url: string };

export interface Amount {
  value: number;
  currency: string;
  rawText: string;
}

/** Mirror of Rust `ListenerInfo` (model.rs), serde camelCase. */
export interface ListenerInfo {
  commentCount: number;
  giftCount: number;
  firstSeenMs: number;
  prevSeenMs: number | null;
  isFirstEver: boolean;
  label: string | null;
  ttsMuted: boolean;
}

/** Mirror of Rust `NameHistory` (listeners.rs). */
export interface NameHistory {
  name: string;
  changedAtMs: number;
}

/** Mirror of Rust `TtsVoiceOverride` (listeners.rs). rate 0.5..2.0, pitch 0.0..2.0. */
export interface TtsVoiceOverride {
  voicevoxSpeaker?: number | null;
  bouyomiVoice?: number | null;
  webSpeechVoice?: string | null;
  rate?: number | null;
  pitch?: number | null;
}

/** Mirror of Rust `ListenerRecord` (listeners.rs). */
export interface ListenerRecord {
  platform: string;
  authorId: string;
  name: string;
  nickname: string | null;
  label: string | null;
  memo: string | null;
  commentCount: number;
  giftCount: number;
  firstSeenMs: number;
  lastSeenMs: number;
  nameHistory: NameHistory[];
  ttsMuted: boolean;
  ttsVoice?: TtsVoiceOverride | null;
}

/** Payload of the 'listener-updated' event: the record, or a deletion marker. */
export interface ListenerDeleted {
  platform: string;
  authorId: string;
  deleted: true;
}
export type ListenerUpdate = ListenerRecord | ListenerDeleted;

/** Only http(s) avatar URLs are rendered (rejects javascript:, data:, etc.). */
export function safeAvatarUrl(url: string | undefined | null): string | undefined {
  return url && /^https?:\/\//i.test(url) ? url : undefined;
}

export function isListenerDeleted(u: ListenerUpdate): u is ListenerDeleted {
  return 'deleted' in u && u.deleted === true;
}

export interface ChatMessage {
  listener?: ListenerInfo;
  id: string;
  /** true when `id` is the platform's own message id (usable for moderation APIs). */
  nativeId?: boolean;
  platform: Platform;
  channel: string;
  author: Author;
  fragments: Fragment[];
  kind: MessageKind;
  amount?: Amount;
  timestampMs: number;
  raw?: unknown;
}

/** Anonymous YouTube reaction counts received in one backend polling update. */
export interface YoutubeReaction {
  channel: string;
  emoji: string;
  count: number;
}

/** Platform-side moderation (mirror of Rust `ModerationEvent`, emitted as `chat-moderation`). */
export type ModerationEvent =
  | { type: 'messageDeleted'; platform: Platform; channel: string; messageId: string }
  | {
      type: 'userCleared'; platform: Platform; channel: string; authorId: string; durationSecs: number | null;
      /** false: timeout vs ban unknown (InnerTube) — never overwrites an existing ban/timeout mark. */
      certain: boolean;
      /** Server-time boundary (ms): only comments with timestampMs <= beforeMs. null = no boundary. */
      beforeMs: number | null;
    }
  | { type: 'chatCleared'; platform: Platform; channel: string; beforeMs: number | null };

/**
 * Frontend-only projection of ChatMessage.
 * Keep ChatMessage itself a strict mirror of the Rust model.
 */
/** Local mark after a successful stream-side moderation action (UI only). */
export type RemovedState = 'deleted' | 'timeout' | 'banned';

export type UiChatMessage = ChatMessage & {
  removed?: RemovedState;
  /** Session sequence number for this viewer, derived in the UI store. */
  viewerSeq?: number;
};

export interface LocalApiStatus {
  enabled: boolean;
  writesBlocked: boolean;
  hasToken: boolean;
  tokenError: string | null;
  notice: string | null;
  baseUrl: string;
}

// Webhook(Rust `integrations::webhooks` / `webhook::runtime` の DTO。すべて camelCase)。
export type WebhookFormat = 'discord' | 'generic';

export interface WebhookTriggers {
  donation: boolean;
  membership: boolean;
  keyword: boolean;
}

export interface WebhookPaused {
  reason: string;
  atMs: number;
}

export interface WebhookLastResult {
  atMs: number;
  httpStatus: number | null;
  kind: string;
}

/** `webhook-status` イベントと `list_webhooks` の `status` に載る送り先ごとの状態。 */
export interface WebhookStatus {
  id: string;
  state: string;
  pausedReason: string | null;
  delivered: number;
  failed: number;
  dropped: number;
  uncertain: number;
  queued: number;
  last: WebhookLastResult | null;
}

/** `save_webhook` の応答(定義 + URL の有無・伏せ字プレビュー)。URL 本体は返らない。 */
export interface WebhookView {
  id: string;
  name: string;
  format: WebhookFormat;
  enabled: boolean;
  triggers: WebhookTriggers;
  keywords: string[];
  paused: WebhookPaused | null;
  hasUrl: boolean;
  urlPreview: string | null;
}

/** `list_webhooks` の 1 件。`WebhookView` に実行時の状態が付く。 */
export interface WebhookListItem extends WebhookView {
  status: WebhookStatus | null;
}

/** `save_webhook` の入力。`id` 無しは新規、`url` 無しは保存済みの URL を変えない。 */
export interface WebhookInput {
  id?: string;
  name: string;
  format: WebhookFormat;
  enabled: boolean;
  triggers: WebhookTriggers;
  keywords: string[];
  url?: string;
}

export interface WebhookTestResult {
  ok: boolean;
  status: number | null;
  errorKind: string | null;
}


export interface TwitcastingStatus {
  configured: boolean;
  state: 'loggedOut' | 'ready' | 'reloginRequired' | 'loading' | 'loadFailed';
  logoutPending?: boolean;
  logoutIssue?: 'markerUnavailable' | 'markerInaccessible' | 'markerCorrupt' | 'deleteFailed' | 'markerAndDeleteFailed' | 'timedOut' | 'checkTimedOut';
  screenId: string | null;
  name: string | null;
  expiresAtMs: number | null;
  callbackUrl: string;
  customClientId: boolean;
}
export interface TwitcastingAuthError {
  code: 'notConfigured' | 'portInUse' | 'loginTimeout' | 'cancelled' | 'denied' | 'clientMismatch' | 'tokenStore' | 'network' | 'other';
  message: string;
}
