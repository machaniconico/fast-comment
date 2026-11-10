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

/**
 * Frontend-only projection of ChatMessage.
 * Keep ChatMessage itself a strict mirror of the Rust model.
 */
export type UiChatMessage = ChatMessage & {
  /** Session sequence number for this viewer, derived in the UI store. */
  viewerSeq?: number;
};
