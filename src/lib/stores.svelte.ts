/**
 * Svelte 5 runes-based comment store.
 *
 * Pattern: reactive class instance exported as a singleton.
 * This is the idiomatic Svelte 5 way to share reactive state across components
 * from a plain .svelte.ts module.
 *
 * - Ring buffer: keeps at most `maxBuffer` messages; oldest are evicted.
 * - Platform filter, text search, and individual hide state.
 * - Wired to ipc.ts via onChatBatch + startChatListener (+ startTtsSpeakListener).
 */

import type { ChatMessage, Platform } from './types';
import { onChatBatch, startChatListener, startTtsSpeakListener, getConfig } from './ipc';

const DEFAULT_MAX_BUFFER = 2000;

/** Buffer entry: the message plus a once-computed lowercase search haystack. */
interface BufEntry {
  msg: ChatMessage;
  /** Lowercased "author name + fragment text" used for substring search. */
  search: string;
}

/** Precompute the lowercase search haystack for a message (author + body). */
function buildSearch(m: ChatMessage): string {
  const body = m.fragments.map((f) => (f.type === 'text' ? f.text : f.name)).join('');
  return (m.author.name + ' ' + body).toLowerCase();
}

class CommentStore {
  // Private backing state — entries carry a precomputed search string.
  private _buf: BufEntry[] = $state([]);
  // msg-only projection of _buf, rebuilt only on push/eviction/clear so the
  // no-filter fast path can return it without re-mapping all entries.
  private _msgs: ChatMessage[] = $state([]);
  private _maxBuffer: number = $state(DEFAULT_MAX_BUFFER);

  // Public filter state
  filterPlatform: Platform | 'all' = $state('all');
  searchQuery: string = $state('');
  hiddenIds: Set<string> = $state(new Set());

  // Derived: filtered list
  readonly visibleMessages: ChatMessage[] = $derived.by(() => {
    const hidden = this.hiddenIds;
    const platform = this.filterPlatform;
    const q = this.searchQuery.trim().toLowerCase();

    // Fast path: no platform filter and no search — only strip hidden.
    if (platform === 'all' && q === '') {
      // No hidden either: return the prebuilt msg-only array as-is (no re-scan).
      if (hidden.size === 0) return this._msgs;
      return this._buf.filter((e) => !hidden.has(e.msg.id)).map((e) => e.msg);
    }

    const out: ChatMessage[] = [];
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

  /** Push a batch into the ring buffer, evicting oldest on overflow. */
  pushBatch(messages: ChatMessage[]): void {
    const incoming = messages.map((msg) => ({ msg, search: buildSearch(msg) }));
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

  setFilterPlatform(p: Platform | 'all'): void {
    this.filterPlatform = p;
  }

  setSearchQuery(q: string): void {
    this.searchQuery = q;
  }

  setMaxBuffer(n: number): void {
    this._maxBuffer = n;
    if (this._buf.length > this._maxBuffer) {
      this._buf = this._buf.slice(this._buf.length - this._maxBuffer);
      this.pruneHiddenIds();
      this._msgs = this._buf.map((e) => e.msg);
    }
  }

  clearMessages(): void {
    this._buf = [];
    this._msgs = [];
  }

  /** Call once from App.svelte onMount to start Tauri IPC listeners. */
  async init(): Promise<() => void> {
    const config = await getConfig();
    if (config && config.ui.maxBuffer > 0) {
      this.setMaxBuffer(config.ui.maxBuffer);
    }
    const [unlistenChat, unlistenTts] = await Promise.all([
      startChatListener(),
      startTtsSpeakListener(),
    ]);
    return () => {
      unlistenChat();
      unlistenTts();
    };
  }
}

// Singleton store instance — import and use directly in components.
export const store = new CommentStore();

// Wire the rAF batch handler once at module load time.
onChatBatch((batch) => store.pushBatch(batch));

// Convenience re-exports so call sites stay terse
export function hideMessage(id: string): void { store.hideMessage(id); }
export function setFilterPlatform(p: Platform | 'all'): void { store.setFilterPlatform(p); }
export function setSearchQuery(q: string): void { store.setSearchQuery(q); }
export function setMaxBuffer(n: number): void { store.setMaxBuffer(n); }
export function clearMessages(): void { store.clearMessages(); }
export async function initStore(): Promise<() => void> { return store.init(); }
