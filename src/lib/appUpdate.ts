/**
 * Pure helpers for the in-app updater (tauri-plugin-updater). No Tauri imports here
 * so the logic can be unit-tested with `node --test`. Runtime calls live in appUpdate.svelte.ts.
 */

export type UpdatePhase =
  | 'idle'
  | 'checking'
  | 'upToDate'
  | 'available'
  | 'downloading'
  | 'installing'
  /** Check failed (manual check only). */
  | 'error'
  /** Check succeeded but download/install failed; the pending Update can be retried. */
  | 'installError';

/** Mirrors the plugin's DownloadEvent union (kept structural so tests need no plugin import). */
export type DownloadEvent =
  | { event: 'Started'; data: { contentLength?: number } }
  | { event: 'Progress'; data: { chunkLength: number } }
  | { event: 'Finished' };

export interface DownloadProgress {
  downloaded: number;
  /** null when the server did not send Content-Length. */
  total: number | null;
}

export const EMPTY_PROGRESS: DownloadProgress = { downloaded: 0, total: null };

/** Fold one plugin download event into the running progress. */
export function applyDownloadEvent(prev: DownloadProgress, ev: DownloadEvent): DownloadProgress {
  switch (ev.event) {
    case 'Started': {
      const len = ev.data.contentLength;
      return { downloaded: 0, total: typeof len === 'number' && Number.isFinite(len) && len > 0 ? len : null };
    }
    case 'Progress': {
      const chunk = Number.isFinite(ev.data.chunkLength) && ev.data.chunkLength > 0 ? ev.data.chunkLength : 0;
      return { downloaded: prev.downloaded + chunk, total: prev.total };
    }
    case 'Finished':
      return prev.total !== null ? { downloaded: prev.total, total: prev.total } : prev;
  }
}

/** 0..100 integer, or null when the total size is unknown. */
export function progressPercent(p: DownloadProgress): number | null {
  if (p.total === null || p.total <= 0) return null;
  return Math.max(0, Math.min(100, Math.floor((p.downloaded / p.total) * 100)));
}

/** Human-readable byte size (KB/MB, 1 decimal for MB). */
export function formatBytes(n: number): string {
  if (!Number.isFinite(n) || n < 0) return '0 B';
  if (n < 1024) return `${Math.floor(n)} B`;
  if (n < 1024 * 1024) return `${Math.floor(n / 1024)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

/** One-line progress label, e.g. "42% (3.1 MB / 7.4 MB)" or "3.1 MB" when the size is unknown. */
export function progressLabel(p: DownloadProgress): string {
  const pct = progressPercent(p);
  if (pct === null || p.total === null) return formatBytes(p.downloaded);
  return `${pct}% (${formatBytes(p.downloaded)} / ${formatBytes(p.total)})`;
}

/** Normalize an unknown thrown value into a short message. */
export function updateErrorText(e: unknown): string {
  if (e instanceof Error && e.message) return e.message;
  if (typeof e === 'string' && e.trim()) return e.trim();
  return '不明なエラー';
}

/** Status line shown under the "更新を確認" button in Settings. */
export function statusText(phase: UpdatePhase, latestVersion: string, error: string): string {
  switch (phase) {
    case 'idle':
      return '';
    case 'checking':
      return '更新を確認しています...';
    case 'upToDate':
      return '最新版です。';
    case 'available':
      return `新しいバージョン v${latestVersion} があります。`;
    case 'downloading':
      return `v${latestVersion} をダウンロードしています...`;
    case 'installing':
      return 'インストールしています。完了するとアプリが再起動します。';
    case 'error':
      return `更新を確認できませんでした: ${error}`;
    case 'installError':
      return `更新をインストールできませんでした: ${error}`;
  }
}
