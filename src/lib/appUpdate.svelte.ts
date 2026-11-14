/**
 * In-app updater runner (tauri-plugin-updater + plugin-process).
 * - Startup check is silent: failures (offline, dev build, missing latest.json) only log.
 * - When the signed updater cannot be used, falls back to the GitHub Releases API check
 *   (`check_for_update`) so the banner can still link to the release page.
 * Pure progress/status helpers live in appUpdate.ts.
 */
import { getVersion } from '@tauri-apps/api/app';
import { relaunch } from '@tauri-apps/plugin-process';
import { check, type Update } from '@tauri-apps/plugin-updater';
import { checkForUpdate, openReleaseUrl } from './ipc';
import {
  EMPTY_PROGRESS,
  applyDownloadEvent,
  updateErrorText,
  type DownloadProgress,
  type UpdatePhase,
} from './appUpdate';

const CHECK_TIMEOUT_MS = 15_000;

class AppUpdateState {
  phase: UpdatePhase = $state('idle');
  currentVersion = $state('');
  latestVersion = $state('');
  notes = $state('');
  error = $state('');
  progress: DownloadProgress = $state({ ...EMPTY_PROGRESS });
  /** Release page for the manual-download fallback (empty = signed update or none). */
  fallbackUrl = $state('');
  /** User pressed "後で" on the banner (Settings still shows the state). */
  dismissed = $state(false);
  private pending: Update | null = $state.raw(null);

  /** Banner is shown when an update can be acted on and the user has not dismissed it. */
  get bannerVisible(): boolean {
    if (this.phase === 'downloading' || this.phase === 'installing') return true;
    return (this.phase === 'available' || this.phase === 'installError') && !this.dismissed;
  }

  /** True when the update can be installed in-app (false = manual download link only). */
  get canInstall(): boolean {
    return this.pending !== null;
  }

  get busy(): boolean {
    return this.phase === 'checking' || this.phase === 'downloading' || this.phase === 'installing';
  }

  async loadCurrentVersion(): Promise<void> {
    if (this.currentVersion) return;
    try {
      this.currentVersion = await getVersion();
    } catch (e) {
      console.warn('[update] getVersion failed', e);
    }
  }

  /** Startup check: never surfaces errors to the user. */
  async checkSilently(): Promise<void> {
    await this.run(false);
  }

  /** Settings "更新を確認": shows "最新版です" / errors, and re-opens a dismissed banner. */
  async checkManually(): Promise<void> {
    this.dismissed = false;
    await this.run(true);
  }

  dismiss(): void {
    this.dismissed = true;
  }

  async install(): Promise<void> {
    if (this.busy) return;
    if (!this.pending) {
      if (this.fallbackUrl) {
        try {
          await openReleaseUrl(this.fallbackUrl);
        } catch (e) {
          console.warn('[update] failed to open release URL', e);
        }
      }
      return;
    }
    const update = this.pending;
    this.phase = 'downloading';
    this.progress = { ...EMPTY_PROGRESS };
    this.error = '';
    try {
      await update.downloadAndInstall((ev) => {
        this.progress = applyDownloadEvent(this.progress, ev);
        if (ev.event === 'Finished') this.phase = 'installing';
      });
      // Windows: the installer exits the app before this line. Other OSes need an explicit restart.
      this.phase = 'installing';
      await relaunch();
    } catch (e) {
      console.warn('[update] download/install failed', e);
      this.error = updateErrorText(e);
      this.phase = 'installError';
      this.dismissed = false;
    }
  }

  private async run(manual: boolean): Promise<void> {
    if (this.busy) return;
    void this.loadCurrentVersion();
    this.phase = 'checking';
    this.error = '';
    try {
      const update = await check({ timeout: CHECK_TIMEOUT_MS });
      await this.replacePending(update);
      if (update) {
        this.latestVersion = update.version;
        this.notes = update.body ?? '';
        this.fallbackUrl = '';
        this.phase = 'available';
        if (!this.currentVersion && update.currentVersion) this.currentVersion = update.currentVersion;
      } else {
        this.phase = manual ? 'upToDate' : 'idle';
      }
      return;
    } catch (e) {
      console.warn('[update] signed updater check failed', e);
      await this.replacePending(null);
      const fallback = await this.tryFallback();
      if (fallback === 'available') return;
      if (fallback === 'upToDate') {
        this.phase = manual ? 'upToDate' : 'idle';
      } else if (manual) {
        this.error = updateErrorText(e);
        this.phase = 'error';
      } else {
        this.phase = 'idle';
      }
    }
  }

  /** GitHub Releases API check (no signature, link-only). */
  private async tryFallback(): Promise<'available' | 'upToDate' | 'failed'> {
    try {
      const status = await checkForUpdate();
      if (!status) return 'failed';
      if (status.currentVersion && !this.currentVersion) this.currentVersion = status.currentVersion;
      if (status.updateAvailable && status.releaseUrl) {
        this.latestVersion = status.latestVersion;
        this.notes = '';
        this.fallbackUrl = status.releaseUrl;
        this.phase = 'available';
        return 'available';
      }
      // check_for_update swallows network errors and reports "no update" with an empty latestVersion.
      return status.latestVersion ? 'upToDate' : 'failed';
    } catch (e) {
      console.warn('[update] fallback release check failed', e);
      return 'failed';
    }
  }

  private async replacePending(next: Update | null): Promise<void> {
    const prev = this.pending;
    this.pending = next;
    if (prev && prev !== next) {
      try {
        await prev.close();
      } catch {
        // Resource already gone; nothing to release.
      }
    }
  }
}

export const appUpdate = new AppUpdateState();
