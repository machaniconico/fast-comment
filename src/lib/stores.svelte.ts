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

import type { ChatMessage, Platform, UiChatMessage } from './types';
import { onChatBatch, startChatListener, startTtsSpeakListener, startTtsCancelListener, getConfig } from './ipc';

const DEFAULT_MAX_BUFFER = 2000;
/** Max pinned comments kept in the pinned strip (oldest dropped on overflow). */
const MAX_PINNED = 5;

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

class CommentStore {
  // Private backing state — entries carry a precomputed search string.
  private _buf: BufEntry[] = $state([]);
  // msg-only projection of _buf, rebuilt only on push/eviction/clear so the
  // no-filter fast path can return it without re-mapping all entries.
  private _msgs: UiChatMessage[] = $state([]);
  // Viewer counts are internal mutable bookkeeping; _uniqueViewers is the
  // reactive projection exposed to Svelte consumers.
  private _viewerCounts: Map<string, number> = new Map();
  private _uniqueViewers: number = $state(0);
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
  hiddenIds: Set<string> = $state(new Set());
  // Pinned comments: full ChatMessage objects (NOT ids) so a pinned comment
  // survives ring-buffer eviction and stays visible after it scrolls out.
  // Capped FIFO (oldest dropped) so the pinned strip never grows unbounded.
  private _pinned: UiChatMessage[] = $state([]);

  // Derived: filtered list
  readonly visibleMessages: UiChatMessage[] = $derived.by(() => {
    const hidden = this.hiddenIds;
    const platform = this.filterPlatform;
    const q = this.searchQuery.trim().toLowerCase();

    // Fast path: no platform filter and no search — only strip hidden.
    if (platform === 'all' && q === '') {
      // No hidden either: return the prebuilt msg-only array as-is (no re-scan).
      if (hidden.size === 0) return this._msgs;
      return this._buf.filter((e) => !hidden.has(e.msg.id)).map((e) => e.msg);
    }

    const out: UiChatMessage[] = [];
    for (const e of this._buf) {
      if (hidden.has(e.msg.id)) continue;
      if (platform !== 'all' && e.msg.platform !== platform) continue;
      if (q !== '' && !e.search.includes(q)) continue;
      out.push(e.msg);
    }
    return out;
  });

  // Derived: total buffered count
  readonly totalCount: number = $derived(this._buf.length);

  // Derived: unique viewers seen since the last explicit clear.
  readonly uniqueViewers: number = $derived(this._uniqueViewers);

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

  // Derived: pinned comments (oldest first)
  readonly pinnedMessages: UiChatMessage[] = $derived(this._pinned);

  // Derived: monotonically increasing highlight-message count (notification trigger)
  readonly highlightCount: number = $derived(this._highlightSeq);

  /** Push a batch into the ring buffer, evicting oldest on overflow. */
  pushBatch(messages: ChatMessage[]): void {
    // Accumulate the session donation tally (clone-on-write so the reactive
    // object is only reallocated when this batch actually carries donations).
    const don = this._donations;
    let byCurrency = don.byCurrency;
    let memberships = don.memberships;
    let donChanged = false;
    let highlightDelta = 0;
    let newViewerCount = 0;
    const incoming: BufEntry[] = [];

    for (const msg of messages) {
      const key = viewerKey(msg);
      let uiMsg: UiChatMessage = msg;
      if (key) {
        const seq = (this._viewerCounts.get(key) ?? 0) + 1;
        if (seq === 1) newViewerCount += 1;
        this._viewerCounts.set(key, seq);
        uiMsg = { ...msg, viewerSeq: seq };
      }
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
    if (newViewerCount) this._uniqueViewers += newViewerCount;
    if (donChanged) this._donations = { byCurrency, memberships };
    if (highlightDelta) this._highlightSeq += highlightDelta;

    const combined = this._buf.concat(incoming);
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

  setFilterPlatform(p: Platform | 'all'): void {
    this.filterPlatform = p;
  }

  setSearchQuery(q: string): void {
    this.searchQuery = q;
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
    this._uniqueViewers = 0;
    // Explicit clear is a session reset, so the donation summary resets too.
    this._donations = emptyDonationSummary();
    this._pinned = [];
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
    const [unlistenChat, unlistenTts, unlistenTtsCancel] = await Promise.all([
      startChatListener(),
      startTtsSpeakListener(),
      startTtsCancelListener(),
    ]);
    return () => {
      unlistenChat();
      unlistenTts();
      unlistenTtsCancel();
    };
  }
}

// Singleton store instance — import and use directly in components.
export const store = new CommentStore();

// Wire the rAF batch handler once at module load time.
onChatBatch((batch) => store.pushBatch(batch));

// Convenience re-exports so call sites stay terse
export function hideMessage(id: string): void { store.hideMessage(id); }
export function pinMessage(msg: UiChatMessage): void { store.pinMessage(msg); }
export function unpinMessage(id: string): void { store.unpinMessage(id); }
export function togglePin(msg: UiChatMessage): void { store.togglePin(msg); }
export function setNotify(sound: boolean, volume: number): void { store.setNotify(sound, volume); }
export function setFilterPlatform(p: Platform | 'all'): void { store.setFilterPlatform(p); }
export function setSearchQuery(q: string): void { store.setSearchQuery(q); }
export function setMaxBuffer(n: number): void { store.setMaxBuffer(n); }
export function clearMessages(): void { store.clearMessages(); }
export function buildCsv(): string { return store.buildCsv(); }
export async function initStore(): Promise<() => void> { return store.init(); }
