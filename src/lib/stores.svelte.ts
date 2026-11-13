/**
 * Svelte 5 runes-based comment store.
 *
 * Pattern: reactive class instance exported as a singleton.
 * This is the idiomatic Svelte 5 way to share reactive state across components
 * from a plain .svelte.ts module.
 *
 * - Ring buffer: keeps at most `maxBuffer` messages; oldest are evicted.
 * - Platform filter, text search, and individual hide state.
 * - Wired to ipc.ts via onChatBatch + startChatListener (+ startTtsSpeakListener/startTtsCancelListener).
 */

import { isListenerDeleted, type ChatMessage, type ListenerUpdate, type ModerationEvent, type Platform, type RemovedState, type UiChatMessage } from './types';
import { activeFilterCount, prependCapped, defaultDisplayFilter, isDefaultFilter, matchesDisplayFilter, normalizeDisplayFilter, type DisplayFilter } from './displayFilter';
import { carryModeration, moderationMatcher, RecentModerationLog } from './moderationEvents';
import { onChatBatch, onModerationBatch, startModerationListener, startChatListener, startListenerUpdatedListener, startTtsSpeakListener, startTtsCancelListener, startTtsAudioListener, getConfig } from './ipc';

const DEFAULT_MAX_BUFFER = 2000;
/** Max pinned comments kept in the pinned strip (oldest dropped on overflow). */
const MAX_PINNED = 5;
export type SearchMode = 'text' | 'regex';

/** Buffer entry: the message plus a once-computed lowercase search haystack. */
interface BufEntry {
  msg: UiChatMessage;
  /** Lowercased "author name + fragment text" used for substring search. */
  search: string;
}

/** Precompute the lowercase search haystack for a message (author + body). */
function buildSearch(m: ChatMessage): string {
  const body = m.fragments.map((f) => (f.type === 'text' ? f.text : f.name)).join('');
  return (m.author.name + ' ' + body).toLowerCase();
}

/** RFC 4180-style CSV cell escaping. Always quote cells for Excel-safe output. */
export function escapeCsvCell(value: unknown): string {
  let text = value == null ? '' : String(value);
  // CSV formula injection 対策: =, +, -, @, タブ, CR で始まるセルは Excel/
  // Google スプレッドシート等が数式として評価し得る。先頭にシングルクオートを
  // 付けて無害化する(表示は元の文字列のまま読める)。
  if (/^[=+\-@\t\r]/.test(text)) {
    text = `'${text}`;
  }
  return `"${text.replace(/"/g, '""')}"`;
}

function messageText(m: ChatMessage): string {
  return m.fragments.map((f) => (f.type === 'text' ? f.text : f.name)).join('');
}

function amountText(m: ChatMessage): string {
  if (!m.amount) return '';
  if (m.amount.rawText) return m.amount.rawText;
  const currency = m.amount.currency ? ` ${m.amount.currency}` : '';
  return `${m.amount.value}${currency}`;
}

/** Stable viewer key. Falls back to name only when the backend has no author id. */
function viewerKey(m: ChatMessage): string | null {
  const scope = `${m.platform}\0${m.channel}`;
  const authorId = m.author.id.trim();
  if (authorId !== '') return `${scope}\0id\0${authorId}`;

  const authorName = m.author.name.trim().toLowerCase();
  if (authorName !== '') return `${scope}\0name\0${authorName}`;
  return null;
}

/** Per-currency monetary tally (SuperChat + Bits). */
export interface CurrencyTally {
  total: number;
  count: number;
}

/**
 * Session donation summary. Accumulated incrementally per batch and reset on
 * clear — a SESSION total, intentionally NOT limited to the ring buffer, so
 * donations evicted from the buffer still count toward the running totals.
 */
export interface DonationSummary {
  /** SuperChat + Bits totals keyed by currency code (e.g. "JPY", "USD", "bits"). */
  byCurrency: Record<string, CurrencyTally>;
  /** Number of membership / new-member events seen this session. */
  memberships: number;
}

export type DonationKind = 'superchat' | 'bits' | 'membership';

export interface DonationMessage {
  message: UiChatMessage;
  donationKind: DonationKind;
}

function emptyDonationSummary(): DonationSummary {
  return { byCurrency: {}, memberships: 0 };
}

export function getDonationKind(msg: ChatMessage): DonationKind | null {
  if (msg.kind === 'membership') return 'membership';
  if (
    (msg.kind === 'superChat' || msg.kind === 'bits') &&
    msg.amount &&
    Number.isFinite(msg.amount.value) &&
    msg.amount.value > 0
  ) {
    return msg.kind === 'bits' ? 'bits' : 'superchat';
  }
  return null;
}

const DISPLAY_FILTER_KEY = 'fc.displayFilter';
/** Max entries shown in the first-comment (初見) panel. */
const FIRST_COMMENT_PANEL_MAX = 50;

/** Load the persisted display filter; any read/parse error or invalid value → default. */
function loadDisplayFilter(): DisplayFilter {
  if (typeof window === 'undefined') return defaultDisplayFilter();
  try {
    const raw = localStorage.getItem(DISPLAY_FILTER_KEY);
    if (raw === null) return defaultDisplayFilter();
    return normalizeDisplayFilter(JSON.parse(raw));
  } catch {
    return defaultDisplayFilter();
  }
}

class CommentStore {
  // Private backing state — entries carry a precomputed search string.
  private _buf: BufEntry[] = $state([]);
  // msg-only projection of _buf, rebuilt only on push/eviction/clear so the
  // no-filter fast path can return it without re-mapping all entries.
  private _msgs: UiChatMessage[] = $state([]);
  // Viewer counts are internal mutable bookkeeping for viewerSeq.
  private _viewerCounts: Map<string, number> = new Map();
  private _maxBuffer: number = $state(DEFAULT_MAX_BUFFER);
  // Monotonically increasing counter — incremented by every pushBatch call,
  // never reset on clear. Used by CommentList to detect genuinely new messages
  // regardless of buffer saturation or filter state.
  private _received: number = $state(0);
  // Session donation tally — accumulated per batch, reset only on clear.
  // Intentionally a session total (not buffer-bounded): evicted donations stay
  // counted, same rationale as _received.
  private _donations: DonationSummary = $state(emptyDonationSummary());
  // Monotonically increasing count of highlight-badge messages received — drives
  // the keyword notification (sound/flash). Never reset, like _received.
  private _highlightSeq: number = $state(0);

  // Notification prefs, loaded from config.ui at init and updated live by Settings.
  notifySound: boolean = $state(false);
  notifyVolume: number = $state(0.5);

  // Public filter state
  filterPlatform: Platform | 'all' = $state('all');
  searchQuery: string = $state('');
  searchMode: SearchMode = $state('text');
  // Condition filter (visit / kind / role). Persisted; the search query is not.
  displayFilter: DisplayFilter = $state(loadDisplayFilter());
  hiddenIds: Set<string> = $state(new Set());
  // Pinned comments: full ChatMessage objects (NOT ids) so a pinned comment
  // survives ring-buffer eviction and stays visible after it scrolls out.
  // Capped FIFO (oldest dropped) so the pinned strip never grows unbounded.
  private _pinned: UiChatMessage[] = $state([]);
  // Session-scoped first-ever comments (newest first, capped); reset on clear.
  private _firstComments: UiChatMessage[] = $state([]);
  // Recent platform moderation events, re-applied to comments that arrive in a later flush.
  private _recentModeration = new RecentModerationLog();

  // Derived: full unfiltered list
  readonly allMessages: UiChatMessage[] = $derived(this._msgs);

  // Derived: compiled search regex. Empty text queries and invalid patterns
  // intentionally resolve to null so partial regex input never throws.
  readonly searchRegex: RegExp | null = $derived.by(() => {
    const q = this.searchQuery.trim();
    if (this.searchMode !== 'regex' || q === '') return null;
    try {
      return new RegExp(q, 'i');
    } catch {
      return null;
    }
  });

  // Derived: UI-facing invalid flag for regex mode.
  readonly searchRegexInvalid: boolean = $derived.by(() => {
    const q = this.searchQuery.trim();
    if (this.searchMode !== 'regex' || q === '') return false;
    return this.searchRegex === null;
  });

  // Derived: filtered list
  readonly visibleMessages: UiChatMessage[] = $derived.by(() => {
    const hidden = this.hiddenIds;
    const platform = this.filterPlatform;
    const q = this.searchQuery.trim();
    const df = this.displayFilter;
    const dfDefault = isDefaultFilter(df);

    // Fast path: no platform/condition filter and no search — only strip hidden.
    if (platform === 'all' && q === '' && dfDefault) {
      // No hidden either: return the prebuilt msg-only array as-is (no re-scan).
      if (hidden.size === 0) return this._msgs;
      return this._buf.filter((e) => !hidden.has(e.msg.id)).map((e) => e.msg);
    }

    const out: UiChatMessage[] = [];
    for (const e of this._buf) {
      if (hidden.has(e.msg.id)) continue;
      if (platform !== 'all' && e.msg.platform !== platform) continue;
      if (!dfDefault && !matchesDisplayFilter(e.msg, df)) continue;
      if (q !== '' && !this.matchesSearch(e.search)) continue;
      out.push(e.msg);
    }
    return out;
  });

  // Derived: total buffered count
  readonly totalCount: number = $derived(this._buf.length);

  // Derived: monotonically increasing received count (never decreases on clear)
  readonly receivedCount: number = $derived(this._received);

  // Derived: session donation summary (SuperChat/Bits totals + membership count)
  readonly donationSummary: DonationSummary = $derived(this._donations);

  // Derived: buffered donation messages, newest first. Not affected by the
  // main comment tab's platform/text filters.
  readonly donationMessages: DonationMessage[] = $derived.by(() => {
    const out: DonationMessage[] = [];
    for (let i = this._buf.length - 1; i >= 0; i -= 1) {
      const entry = this._buf[i];
      if (!entry) continue;
      const msg = entry.msg;
      const donationKind = getDonationKind(msg);
      if (donationKind) out.push({ message: msg, donationKind });
    }
    return out;
  });

  // Derived: number of active display-filter condition groups (toolbar badge).
  readonly displayFilterCount: number = $derived(activeFilterCount(this.displayFilter));

  // Derived: session-scoped first-ever comments, newest first (max 50). Kept
  // outside the ring buffer so it survives eviction; not affected by the main
  // comment tab's filters.
  readonly firstCommentMessages: UiChatMessage[] = $derived(this._firstComments);

  // Derived: pinned comments (oldest first)
  readonly pinnedMessages: UiChatMessage[] = $derived(this._pinned);

  // Derived: monotonically increasing highlight-message count (notification trigger)
  readonly highlightCount: number = $derived(this._highlightSeq);

  // Latest 'listener-updated' payload with a monotonically increasing receive number,
  // so the user card can tell already-seen events from new ones.
  lastListenerUpdate: { seq: number; update: ListenerUpdate } | null = $state(null);
  private _listenerUpdateSeq = 0;

  /** Session comments of one author, newest first (for the user card). */
  sessionCommentsOf(platform: string, authorId: string, limit = 100): UiChatMessage[] {
    const out: UiChatMessage[] = [];
    for (let i = this._buf.length - 1; i >= 0 && out.length < limit; i -= 1) {
      const msg = this._buf[i]?.msg;
      if (msg && msg.platform === platform && msg.author.id === authorId) out.push(msg);
    }
    return out;
  }

  // Latest listener edit/deletion per author. Comments still waiting for the
  // next IPC frame were built from the pre-edit record, so pushBatch re-applies
  // this to every arriving message (otherwise they would show stale nickname/label).
  private _listenerOverrides: Map<string, ListenerUpdate> = new Map();

  /** Overlay the latest listener edit/deletion (if any) onto one message. */
  private withListenerOverride(msg: UiChatMessage): UiChatMessage {
    if (this._listenerOverrides.size === 0) return msg;
    const update = this._listenerOverrides.get(`${msg.platform}\u0000${msg.author.id}`);
    if (!update) return msg;
    const deleted = isListenerDeleted(update);
    const nickname = deleted ? undefined : update.nickname ?? undefined;
    const label = deleted ? null : update.label;
    const ttsMuted = deleted ? false : update.ttsMuted;
    return {
      ...msg,
      author: { ...msg.author, nickname },
      listener: msg.listener ? { ...msg.listener, label, ttsMuted } : msg.listener,
    };
  }

  /** Reflect a listener edit/deletion onto buffered and not-yet-arrived messages of that author. */
  applyListenerUpdate(update: ListenerUpdate): void {
    this._listenerUpdateSeq += 1;
    this.lastListenerUpdate = { seq: this._listenerUpdateSeq, update };
    this._listenerOverrides.set(`${update.platform}\u0000${update.authorId}`, update);
    const patch = (msg: UiChatMessage): UiChatMessage =>
      msg.platform === update.platform && msg.author.id === update.authorId
        ? this.withListenerOverride(msg)
        : msg;
    let changed = false;
    const next = this._buf.map((entry) => {
      const msg = patch(entry.msg);
      if (msg === entry.msg) return entry;
      changed = true;
      return { msg, search: entry.search };
    });
    if (changed) {
      this._buf = next;
      this._msgs = next.map((e) => e.msg);
    }
    if (this._pinned.some((m) => patch(m) !== m)) this._pinned = this._pinned.map(patch);
  }

  /**
   * Mark buffered comments as removed after a successful stream-side moderation action
   * (UI only; touched only on user action). With `messageId` only that message is marked,
   * otherwise every message of `platform + authorId`. Matching pinned comments are unpinned.
   */
  markRemoved(platform: string, authorId: string, removed: RemovedState, messageId?: string): void {
    const hit = (m: UiChatMessage) =>
      m.platform === platform && (messageId !== undefined ? m.id === messageId : m.author.id === authorId);
    const mark = (m: UiChatMessage): UiChatMessage => (hit(m) && m.removed !== removed ? { ...m, removed } : m);
    let changed = false;
    const next = this._buf.map((entry) => {
      const msg = mark(entry.msg);
      if (msg === entry.msg) return entry;
      changed = true;
      return { msg, search: entry.search };
    });
    if (changed) {
      this._buf = next;
      this._msgs = next.map((e) => e.msg);
    }
    if (this._firstComments.some((m) => mark(m) !== m)) this._firstComments = this._firstComments.map(mark);
    if (this._pinned.some(hit)) this._pinned = this._pinned.filter((m) => !hit(m));
  }

  /**
   * Reflect platform-side deletes / timeouts / bans (`chat-moderation`). Same marks and pin
   * handling as `markRemoved`; one pass over each list per flush. Idempotent.
   */
  applyModerationEvents(events: ModerationEvent[]): void {
    const matcher = moderationMatcher(events);
    if (!matcher) return;
    this._recentModeration.add(events, performance.now());
    let changed = false;
    const next = this._buf.map((entry) => {
      const msg = matcher.mark(entry.msg);
      if (msg === entry.msg) return entry;
      changed = true;
      return { msg, search: entry.search };
    });
    if (changed) {
      this._buf = next;
      this._msgs = next.map((e) => e.msg);
    }
    if (this._firstComments.some((m) => matcher.mark(m) !== m)) this._firstComments = this._firstComments.map((m) => matcher.mark(m));
    if (this._pinned.some(matcher.hits)) this._pinned = this._pinned.filter((m) => !matcher.hits(m));
  }

  /** Push a batch into the ring buffer, evicting oldest on overflow. */
  pushBatch(messages: ChatMessage[]): void {
    // Accumulate the session donation tally (clone-on-write so the reactive
    // object is only reallocated when this batch actually carries donations).
    const don = this._donations;
    let byCurrency = don.byCurrency;
    let memberships = don.memberships;
    let donChanged = false;
    let highlightDelta = 0;
    const incoming: BufEntry[] = [];
    let baseBuffer = this._buf;
    let copiedForGiftUpdate = false;
    // Moderation that reached the UI before its target comment (separate IPC batches).
    const lateModeration = this._recentModeration.matcher(performance.now());

    for (const msg of messages) {
      if (msg.kind === 'gift') {
        const existingIndex = baseBuffer.findIndex((entry) => entry.msg.id === msg.id);
        if (existingIndex >= 0) {
          if (!copiedForGiftUpdate) {
            baseBuffer = baseBuffer.slice();
            copiedForGiftUpdate = true;
          }
          const previous = baseBuffer[existingIndex]?.msg;
          const uiMsg: UiChatMessage = carryModeration(this.withListenerOverride(previous?.viewerSeq === undefined
            ? msg
            : { ...msg, viewerSeq: previous.viewerSeq }), previous, lateModeration);
          baseBuffer[existingIndex] = { msg: uiMsg, search: buildSearch(msg) };
          continue;
        }

        const pendingIndex = incoming.findIndex((entry) => entry.msg.id === msg.id);
        if (pendingIndex >= 0) {
          const previous = incoming[pendingIndex]?.msg;
          const uiMsg: UiChatMessage = carryModeration(this.withListenerOverride(previous?.viewerSeq === undefined
            ? msg
            : { ...msg, viewerSeq: previous.viewerSeq }), previous, lateModeration);
          incoming[pendingIndex] = { msg: uiMsg, search: buildSearch(msg) };
          continue;
        }
      }

      const key = viewerKey(msg);
      let uiMsg: UiChatMessage = this.withListenerOverride(msg);
      if (key) {
        const seq = (this._viewerCounts.get(key) ?? 0) + 1;
        this._viewerCounts.set(key, seq);
        uiMsg = { ...uiMsg, viewerSeq: seq };
      }
      if (lateModeration) uiMsg = lateModeration.mark(uiMsg);
      incoming.push({ msg: uiMsg, search: buildSearch(msg) });

      // Highlight detection is independent of kind (a SuperChat can also be a
      // highlight), so check it separately from the donation tally below.
      if (msg.author.badges.some((b) => b.kind === 'highlight')) highlightDelta += 1;
      const donationKind = getDonationKind(msg);
      if (donationKind === 'membership') {
        memberships += 1;
        donChanged = true;
      } else if (
        (donationKind === 'superchat' || donationKind === 'bits') &&
        msg.amount
      ) {
        // Bits は Rust 側が currency="BITS" を送るが、表示キーは casing に依存しない
        // 正準値 'bits' に寄せる(SuperChat は通貨コード/記号をそのまま使う)。
        const cur = donationKind === 'bits' ? 'bits' : (msg.amount.currency || '?');
        if (byCurrency === don.byCurrency) byCurrency = { ...don.byCurrency };
        const prev = byCurrency[cur] ?? { total: 0, count: 0 };
        byCurrency[cur] = { total: prev.total + msg.amount.value, count: prev.count + 1 };
        donChanged = true;
      }
    }
    this._received += incoming.length;
    if (donChanged) this._donations = { byCurrency, memberships };
    if (highlightDelta) this._highlightSeq += highlightDelta;

    this._firstComments = prependCapped(
      this._firstComments,
      incoming,
      (e) => (e.msg.listener?.isFirstEver === true ? e.msg : null),
      FIRST_COMMENT_PANEL_MAX,
    );

    const combined = baseBuffer.concat(incoming);
    if (combined.length > this._maxBuffer) {
      this._buf = combined.slice(combined.length - this._maxBuffer);
      this.pruneHiddenIds();
    } else {
      this._buf = combined;
    }
    this._msgs = this._buf.map((e) => e.msg);
  }

  /** Drop hiddenIds that no longer reference any buffered message. */
  private pruneHiddenIds(): void {
    if (this.hiddenIds.size === 0) return;
    const present = new Set(this._buf.map((e) => e.msg.id));
    let changed = false;
    for (const id of this.hiddenIds) {
      if (!present.has(id)) {
        changed = true;
        break;
      }
    }
    if (!changed) return;
    const next = new Set<string>();
    for (const id of this.hiddenIds) {
      if (present.has(id)) next.add(id);
    }
    this.hiddenIds = next;
  }

  hideMessage(id: string): void {
    this.hiddenIds = new Set([...this.hiddenIds, id]);
  }

  /** True if a comment with this id is currently pinned. */
  isPinned(id: string): boolean {
    return this._pinned.some((m) => m.id === id);
  }

  /** Pin a comment (no-op if already pinned). FIFO-capped at MAX_PINNED. */
  pinMessage(msg: UiChatMessage): void {
    if (this._pinned.some((m) => m.id === msg.id)) return;
    const next = [...this._pinned, msg];
    this._pinned = next.length > MAX_PINNED ? next.slice(next.length - MAX_PINNED) : next;
  }

  unpinMessage(id: string): void {
    if (!this._pinned.some((m) => m.id === id)) return;
    this._pinned = this._pinned.filter((m) => m.id !== id);
  }

  /** Pin if not pinned, otherwise unpin. */
  togglePin(msg: UiChatMessage): void {
    if (this.isPinned(msg.id)) this.unpinMessage(msg.id);
    else this.pinMessage(msg);
  }

  setDisplayFilter(f: DisplayFilter): void {
    this.displayFilter = normalizeDisplayFilter(f);
    try {
      localStorage.setItem(DISPLAY_FILTER_KEY, JSON.stringify(this.displayFilter));
    } catch {
      // Storage quota or private-mode errors are non-fatal.
    }
  }

  setFilterPlatform(p: Platform | 'all'): void {
    this.filterPlatform = p;
  }

  setSearchQuery(q: string): void {
    this.searchQuery = q;
  }

  setSearchMode(m: SearchMode): void {
    this.searchMode = m;
  }

  /**
   * Whether an already-lowercased search haystack matches the current query and
   * mode. Single source of truth for both `visibleMessages` and the per-platform
   * CommentList path, so text/regex behavior is identical everywhere (the
   * multi-column view used to silently fall back to substring matching).
   * Empty query → always true. Invalid regex → filter not applied (true) so a
   * partially typed pattern never makes the list vanish.
   */
  matchesSearch(haystackLower: string): boolean {
    const q = this.searchQuery.trim();
    if (q === '') return true;
    if (this.searchMode === 'regex') {
      const regex = this.searchRegex;
      return regex ? regex.test(haystackLower) : true;
    }
    return haystackLower.includes(q.toLowerCase());
  }

  setMaxBuffer(n: number): void {
    if (!Number.isFinite(n) || n < 1) return;
    this._maxBuffer = Math.trunc(n);
    if (this._buf.length > this._maxBuffer) {
      this._buf = this._buf.slice(this._buf.length - this._maxBuffer);
      this.pruneHiddenIds();
      this._msgs = this._buf.map((e) => e.msg);
    }
  }

  /** Update notification prefs live (called by Settings after save). */
  setNotify(sound: boolean, volume: number): void {
    this.notifySound = sound;
    if (Number.isFinite(volume)) this.notifyVolume = Math.min(1, Math.max(0, volume));
  }

  clearMessages(): void {
    this._buf = [];
    this._msgs = [];
    this._viewerCounts.clear();
    // Explicit clear is a session reset, so the donation summary resets too.
    this._donations = emptyDonationSummary();
    this._pinned = [];
    this._firstComments = [];
  }

  buildCsv(): string {
    const rows = [
      ['timestamp', 'platform', 'channel', 'author', 'kind', 'text', 'amount'],
      ...this._buf.map(({ msg }) => [
        Number.isFinite(msg.timestampMs) ? new Date(msg.timestampMs).toISOString() : '',
        msg.platform,
        msg.channel,
        msg.author.name,
        msg.kind,
        messageText(msg),
        amountText(msg),
      ]),
    ];
    return `${rows.map((row) => row.map(escapeCsvCell).join(',')).join('\r\n')}\r\n`;
  }

  /** Call once from App.svelte onMount to start Tauri IPC listeners. */
  async init(): Promise<() => void> {
    const config = await getConfig();
    if (config && config.ui.maxBuffer > 0) {
      this.setMaxBuffer(config.ui.maxBuffer);
    }
    if (config) {
      this.setNotify(config.ui.notifySound ?? false, config.ui.notifyVolume ?? 0.5);
    }
    // allSettled: if one listener fails to register, release the ones that did.
    const results = await Promise.allSettled([
      startChatListener(),
      startModerationListener(),
      startTtsSpeakListener(),
      startTtsCancelListener(),
      startTtsAudioListener(),
      startListenerUpdatedListener((u) => this.applyListenerUpdate(u)),
    ]);
    const unlisteners: Array<() => void> = [];
    let failure: unknown = null;
    for (const r of results) {
      if (r.status === 'fulfilled') unlisteners.push(r.value);
      else if (failure === null) failure = r.reason ?? new Error('listener registration failed');
    }
    if (failure !== null) {
      for (const un of unlisteners) un();
      throw failure;
    }
    return () => {
      for (const un of unlisteners) un();
    };
  }
}

// Singleton store instance — import and use directly in components.
export const store = new CommentStore();

// Wire the rAF batch handler once at module load time.
onChatBatch((batch) => store.pushBatch(batch));
onModerationBatch((events) => store.applyModerationEvents(events));

// Convenience re-exports so call sites stay terse
export function hideMessage(id: string): void { store.hideMessage(id); }
export function pinMessage(msg: UiChatMessage): void { store.pinMessage(msg); }
export function unpinMessage(id: string): void { store.unpinMessage(id); }
export function togglePin(msg: UiChatMessage): void { store.togglePin(msg); }
export function setNotify(sound: boolean, volume: number): void { store.setNotify(sound, volume); }
export function setFilterPlatform(p: Platform | 'all'): void { store.setFilterPlatform(p); }
export function setSearchQuery(q: string): void { store.setSearchQuery(q); }
export function setSearchMode(m: SearchMode): void { store.setSearchMode(m); }
export function setMaxBuffer(n: number): void { store.setMaxBuffer(n); }
export function clearMessages(): void { store.clearMessages(); }
export function buildCsv(): string { return store.buildCsv(); }
export async function initStore(): Promise<() => void> { return store.init(); }
