/**
 * TypeScript mirror of src-tauri/src/model.rs
 * JSON is camelCase (serde rename_all = "camelCase")
 * Platform is lowercase (serde rename_all = "lowercase")
 * Fragment uses tagged union with `type` discriminant
 */

export type Platform = 'twitch' | 'youtube';

export type MessageKind = 'normal' | 'superChat' | 'membership' | 'bits' | 'system';

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

export interface ChatMessage {
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

/**
 * Frontend-only projection of ChatMessage.
 * Keep ChatMessage itself a strict mirror of the Rust model.
 */
export type UiChatMessage = ChatMessage & {
  /** Session sequence number for this viewer, derived in the UI store. */
  viewerSeq?: number;
};
