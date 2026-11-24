/**
 * TypeScript mirror of src-tauri/src/model.rs
 * JSON is camelCase (serde rename_all = "camelCase")
 * Platform is lowercase (serde rename_all = "lowercase")
 * Fragment uses tagged union with `type` discriminant
 */

export type Platform = 'twitch' | 'youtube' | 'x' | 'niconico' | 'twitcasting';

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
  donation: Record<string, number>;
  firstSeenMs: number;
  lastSeenMs: number;
  nameHistory: NameHistory[];
  ttsMuted: boolean;
  ttsVoice?: TtsVoiceOverride | null;
}

export interface ListenerDonation {
  totals: { currency: string; milli: number }[];
  jpyEstimateMilli: number | null;
  unconvertible: string[];
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
  /** 効果音の指定(`builtin:<name>` / `custom:<id>`)。Rust が判定済みで、UI は鳴らすだけ。 */
  sound?: string;
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
  achievement: boolean;
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


/** ツイキャスの取得経路(Rust `TwitcastingMode`)。 */
export type TwitcastingMode = 'auto' | 'unofficial' | 'official';

export interface TwitcastingStatus {
  clientId: string;
  configured: boolean;
  state: 'loggedOut' | 'ready' | 'reloginRequired' | 'loading' | 'loadFailed';
  logoutPending?: boolean;
  logoutIssue?: 'markerUnavailable' | 'markerInaccessible' | 'markerCorrupt' | 'deleteFailed' | 'markerAndDeleteFailed' | 'timedOut' | 'checkTimedOut' | 'otherLoginKept' | 'recordUnrecognized' | 'unreadableRecordDeleted' | 'lockTimedOut';
  /** 削除を伴って完了したログアウトの回数(0 のときは省略)。 */
  logoutsCompleted?: number;
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

// ---------------------------------------------------------------------------
// Achievements ('achievements' event; camelCase, mirrors achievements/engine.rs)
// ---------------------------------------------------------------------------

export type AchievementTier = 'bronze' | 'silver' | 'gold' | 'platinum';
export type AchievementSubjectKind = 'viewer' | 'streamer';

export interface AchievementUnlockSubject {
  kind: AchievementSubjectKind;
  platform?: Platform | null;
  authorId?: string | null;
  name?: string | null;
}

export interface AchievementUnlocked {
  id: string;
  achievementId: string;
  name: string;
  description: string;
  tier: AchievementTier;
  icon: string;
  subject: AchievementUnlockSubject;
  valueMilli: number;
  thresholdMilli: number;
  currency?: string | null;
  windowKey: string;
  unlockedAtMs: number;
}

// ---------------------------------------------------------------------------
// Achievement management UI (list/schema/commit IPC; camelCase, mirrors achievements/ipc.rs)
// ---------------------------------------------------------------------------

export type AchievementWindow = 'lifetime' | 'perStream';
export type AchievementMetric =
  | 'comments' | 'superChatCount' | 'donationAmount' | 'giftCount' | 'membershipEvents'
  | 'streamsAttended' | 'uniqueCommenters' | 'peakConcurrentViewers' | 'streams';
export type AchievementCurrencyKind = 'none' | 'jpyConverted' | 'only';
export type AchievementCurrency = { mode: 'none' | 'jpyConverted' } | { mode: 'only'; code: string };

export interface AchievementCondition {
  window: AchievementWindow;
  metric: AchievementMetric;
  /** null while the input is empty; Rust reports it as a threshold field error. */
  threshold: number | null;
  currency: AchievementCurrency;
  platforms: Platform[];
}

export interface AchievementDef {
  id: string;
  name: string;
  description: string;
  icon: string;
  tier: AchievementTier;
  subject: AchievementSubjectKind;
  enabled: boolean;
  builtinId: string | null;
  edited: boolean;
  condition: AchievementCondition | null;
  announce: { tts: boolean };
}

export interface AchievementRow extends AchievementDef {
  viewerUnlockCount: number;
  streamerUnlockCount: number;
  streamerLastUnlockedAtMs: number | null;
}

export interface AchievementList {
  revision: number;
  /** Token of the file contents this list was read from; pass it back when saving. */
  contentToken: string;
  enabled: boolean;
  rates: Record<string, number>;
  ratesUpdatedAtMs: number | null;
  symbolOverrides: Record<string, string>;
  achievements: AchievementRow[];
}

export interface AchievementMetricRule {
  metric: AchievementMetric;
  subjects: AchievementSubjectKind[];
  windows: AchievementWindow[];
  currencyModes: AchievementCurrencyKind[];
  platforms: Platform[];
}

export interface AchievementSchema {
  metrics: AchievementMetricRule[];
}

export type AchievementCommitReply =
  | { result: 'ok'; revision: number }
  | { result: 'stale' }
  | { result: 'invalid'; id: string; field: string }
  | { result: 'io' }
  | { result: 'saveRefused' };

export interface ListenerTrophy {
  achievementId: string;
  name: string;
  icon: string;
  tier: AchievementTier;
  windowKey: string;
  unlockedAtMs: number;
}
