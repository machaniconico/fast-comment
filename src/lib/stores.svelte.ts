/**
 * Svelte 5 runes-based comment store.
 *
 * Pattern: reactive class instance exported as a singleton.
 * This is the idiomatic Svelte 5 way to share reactive state across components
 * from a plain .svelte.ts module.
 *
 * - Ring buffer: keeps at most `maxBuffer` messages; oldest are evicted.
 * - Platform filter, text search, and individual hide state.
 * - Wired to ipc.ts via onChatBatch + startChatListener.
 */

import type { ChatMessage, Platform } from './types';
import { onChatBatch, startChatListener, getConfig } from './ipc';

const DEFAULT_MAX_BUFFER = 2000;

class CommentStore {
  // Private backing state
  private _buf: ChatMessage[] = $state([]);
  private _maxBuffer: number = $state(DEFAULT_MAX_BUFFER);

  // Public filter state
  filterPlatform: Platform | 'all' = $state('all');
  searchQuery: string = $state('');
  hiddenIds: Set<string> = $state(new Set());

  // Derived: filtered list
  readonly visibleMessages: ChatMessage[] = $derived(
    this._buf.filter((m) => {
      if (this.hiddenIds.has(m.id)) return false;
      if (this.filterPlatform !== 'all' && m.platform !== this.filterPlatform) return false;
      if (this.searchQuery.trim()) {
        const q = this.searchQuery.trim().toLowerCase();
        const text = m.fragments
          .map((f) => (f.type === 'text' ? f.text : f.name))
          .join('')
          .toLowerCase();
        if (!text.includes(q) && !m.author.name.toLowerCase().includes(q)) return false;
      }
      return true;
    })
  );

  // Derived: total buffered count
  readonly totalCount: number = $derived(this._buf.length);

  /** Push a batch into the ring buffer, evicting oldest on overflow. */
  pushBatch(messages: ChatMessage[]): void {
    const combined = this._buf.concat(messages);
    this._buf =
      combined.length > this._maxBuffer
        ? combined.slice(combined.length - this._maxBuffer)
        : combined;
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
    }
  }

  clearMessages(): void {
    this._buf = [];
  }

  /** Call once from App.svelte onMount to start Tauri IPC listener. */
  async init(): Promise<() => void> {
    const config = await getConfig();
    if (config && config.ui.maxBuffer > 0) {
      this.setMaxBuffer(config.ui.maxBuffer);
    }
    return startChatListener();
  }
}

// Singleton store instance — import and use directly in components.
export const store = new CommentStore();

// Wire the rAF batch handler once at module load time.
onChatBatch((batch) => store.pushBatch(batch));

// Convenience re-exports so call sites stay terse
export const visibleMessages = $derived(store.visibleMessages);
export const totalCount = $derived(store.totalCount);

export function hideMessage(id: string): void { store.hideMessage(id); }
export function setFilterPlatform(p: Platform | 'all'): void { store.setFilterPlatform(p); }
export function setSearchQuery(q: string): void { store.setSearchQuery(q); }
export function setMaxBuffer(n: number): void { store.setMaxBuffer(n); }
export function clearMessages(): void { store.clearMessages(); }
export async function initStore(): Promise<() => void> { return store.init(); }
