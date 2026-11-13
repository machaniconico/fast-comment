/**
 * Apply platform-side moderation events (`chat-moderation`) to buffered comments.
 * Pure: no Svelte state. The store runs the returned matcher once per list per flush.
 *
 * - messageDeleted → 'deleted' on that message id
 * - userCleared    → 'timeout' (durationSecs set) / 'banned' (null) on every message of the author
 * - chatCleared    → 'deleted' on every message of that channel
 *
 * userCleared / chatCleared only reach comments with `timestampMs <= beforeMs` (server time) when
 * `beforeMs` is set, so a comment posted after a clear in the same frame stays unmarked.
 * An uncertain userCleared (`certain: false`, InnerTube) never overwrites an existing
 * 'banned' / 'timeout' mark; without a mark it shows as 'banned'.
 *
 * Idempotent: re-applying the same events (app action + platform echo) changes nothing.
 * A user-level mark wins over 'deleted'; 'deleted' never overwrites an existing mark.
 * Ids not in the buffer are ignored here; `RecentModerationLog` re-applies recent events to
 * comments that arrive in a later flush.
 */
import type { ModerationEvent, RemovedState, UiChatMessage } from './types';

export interface ModerationMatcher {
  /** True when any event targets `m`. */
  hits(m: UiChatMessage): boolean;
  /** `m` with the resulting `removed` mark; the same object when nothing changes. */
  mark<T extends UiChatMessage>(m: T): T;
}

interface UserRule { state: RemovedState; certain: boolean; beforeMs: number | null }

const SEP = '\u0000';
const within = (beforeMs: number | null | undefined, m: UiChatMessage) => beforeMs == null || m.timestampMs <= beforeMs;

export function moderationMatcher(events: readonly ModerationEvent[]): ModerationMatcher | null {
  if (events.length === 0) return null;
  const deletedIds = new Set<string>();
  const channelClears = new Map<string, Array<number | null>>();
  const users = new Map<string, UserRule[]>();
  for (const e of events) {
    if (e.type === 'messageDeleted') {
      if (e.messageId) deletedIds.add(e.platform + SEP + e.channel + SEP + e.messageId);
    } else if (e.type === 'userCleared') {
      if (!e.authorId) continue;
      const key = e.platform + SEP + e.channel + SEP + e.authorId;
      const rule: UserRule = { state: e.durationSecs == null ? 'banned' : 'timeout', certain: e.certain !== false, beforeMs: e.beforeMs ?? null };
      const list = users.get(key);
      if (list) list.push(rule); else users.set(key, [rule]);
    } else if (e.type === 'chatCleared') {
      const key = e.platform + SEP + e.channel;
      const list = channelClears.get(key);
      if (list) list.push(e.beforeMs ?? null); else channelClears.set(key, [e.beforeMs ?? null]);
    }
  }
  const target = (m: UiChatMessage): RemovedState | undefined => {
    const ch = m.platform + SEP + m.channel;
    let next = m.removed;
    let hit = false;
    // Rules apply in arrival order: a certain event sets the state, an uncertain one only
    // fills in when there is no ban/timeout mark yet.
    for (const rule of users.get(ch + SEP + m.author.id) ?? []) {
      if (!within(rule.beforeMs, m)) continue;
      hit = true;
      if (rule.certain || (next !== 'banned' && next !== 'timeout')) next = rule.state;
    }
    if (deletedIds.has(ch + SEP + m.id) || (channelClears.get(ch) ?? []).some((b) => within(b, m))) {
      hit = true;
      next = next ?? 'deleted';
    }
    return hit ? next : undefined;
  };
  return {
    hits: (m) => target(m) !== undefined,
    mark<T extends UiChatMessage>(m: T): T {
      const next = target(m);
      return next === undefined || next === m.removed ? m : { ...m, removed: next };
    },
  };
}

/** One pass over `entries`; returns the same array when nothing changed. */
export function applyModerationEvents<T extends UiChatMessage>(entries: T[], events: readonly ModerationEvent[]): T[] {
  const matcher = moderationMatcher(events);
  if (!matcher) return entries;
  let changed = false;
  const next = entries.map((m) => {
    const marked = matcher.mark(m);
    if (marked !== m) changed = true;
    return marked;
  });
  return changed ? next : entries;
}

/**
 * For an in-place update of an already-shown comment (gift combo updates reuse the id): keep the
 * previous `removed` mark and re-apply recent moderation, so the update never clears the mark.
 */
export function carryModeration<T extends UiChatMessage>(next: T, previous: UiChatMessage | undefined, late: ModerationMatcher | null): T {
  const carried = previous?.removed && next.removed === undefined ? { ...next, removed: previous.removed } : next;
  return late ? late.mark(carried) : carried;
}

/** Max events remembered for comments that arrive after their moderation event. */
export const RECENT_MODERATION_MAX = 512;
/** How long (ms, monotonic clock — pass `performance.now()`) a remembered event keeps applying to late-arriving comments. */
export const RECENT_MODERATION_TTL_MS = 10_000;

/**
 * Bounded memory of recent moderation events. The chat and moderation IPC streams are batched
 * separately in Rust, so an event can reach the UI a frame before its target comment; this
 * re-applies it to comments arriving shortly after. Entries drop oldest-first past `max` and
 * expire `ttlMs` after they were recorded (so an unbounded InnerTube clear cannot mark the
 * author's future comments indefinitely).
 */
export class RecentModerationLog {
  private entries: Array<{ event: ModerationEvent; at: number }> = [];
  private cached: ModerationMatcher | null = null;
  private dirty = false;
  private readonly max: number;
  private readonly ttlMs: number;

  constructor(max = RECENT_MODERATION_MAX, ttlMs = RECENT_MODERATION_TTL_MS) {
    this.max = max;
    this.ttlMs = ttlMs;
  }

  get size(): number { return this.entries.length; }

  add(events: readonly ModerationEvent[], nowMs: number): void {
    if (events.length === 0) return;
    for (const event of events) this.entries.push({ event, at: nowMs });
    if (this.entries.length > this.max) this.entries.splice(0, this.entries.length - this.max);
    this.dirty = true;
  }

  /** Matcher over still-live entries, or null when nothing is remembered. */
  matcher(nowMs: number): ModerationMatcher | null {
    if (this.entries.length === 0) return null;
    // Expiry is judged per entry. A negative age (clock went backwards) also counts as expired,
    // so a non-monotonic clock can never keep entries alive forever.
    const live = this.entries.filter((e) => {
      const age = nowMs - e.at;
      return age >= 0 && age <= this.ttlMs;
    });
    if (live.length !== this.entries.length) {
      this.entries = live;
      this.dirty = true;
    }
    if (this.dirty) {
      this.cached = moderationMatcher(this.entries.map((e) => e.event));
      this.dirty = false;
    }
    return this.cached;
  }
}
