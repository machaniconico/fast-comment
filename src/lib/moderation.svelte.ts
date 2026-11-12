/**
 * Stream-side moderation runner: login state, in-flight guard, confirm request,
 * toasts and local "removed" marking. Pure decision logic lives in moderationActions.ts.
 */
import { getTwitchOauthStatus, getYoutubeOauthStatus, moderateBanUser, moderateDeleteMessage, moderateUnbanUser } from './ipc';
import { formatTimeoutLabel, toModerationError, type ModerationAuth } from './moderationActions';
import { store } from './stores.svelte';
import { ui } from './ui.svelte';

export interface ModerationTarget {
  platform: string;
  channel: string;
  authorId: string;
  name: string;
}

export interface BanRequest extends ModerationTarget {
  /** Element to refocus when the dialog closes (e.g. the comment whose menu requested it). */
  returnFocus: HTMLElement | null;
}

function isTwitch(t: ModerationTarget): boolean {
  return t.platform.trim().toLowerCase() === 'twitch';
}

class ModerationState {
  auth: ModerationAuth = $state({ youtubeConnected: false });
  /** Permanent-BAN confirmation request (null = dialog closed). */
  confirm: BanRequest | null = $state(null);
  private busy: Record<string, true> = $state({});

  /** Re-read the YouTube login state. Called on app start and after connect/disconnect (no polling). */
  async refreshAuth(): Promise<void> {
    try {
      const s = await getYoutubeOauthStatus();
      this.setYoutubeConnected(s?.connected === true);
    } catch {
      this.setYoutubeConnected(false);
    }
    try {
      const t = await getTwitchOauthStatus();
      this.setTwitchConnected(t?.connected === true);
    } catch {
      this.setTwitchConnected(false);
    }
  }

  setYoutubeConnected(connected: boolean): void {
    this.auth = { ...this.auth, youtubeConnected: connected };
  }

  setTwitchConnected(connected: boolean): void {
    this.auth = { ...this.auth, twitchConnected: connected };
  }

  userKey(t: ModerationTarget): string {
    return `user|${t.platform}|${t.channel}|${t.authorId}`;
  }

  deleteKey(t: ModerationTarget, messageId: string): string {
    return `del|${t.platform}|${t.channel}|${messageId}`;
  }

  isBusy(key: string): boolean {
    return this.busy[key] === true;
  }

  private async run(key: string, job: () => Promise<void>, success: string, onOk: () => void, twitch = false): Promise<void> {
    if (this.busy[key]) return; // double-click guard (each call burns API quota)
    this.busy = { ...this.busy, [key]: true };
    try {
      await job();
      onOk();
      ui.showToast(success, 'success');
    } catch (e) {
      const err = toModerationError(e);
      const action =
        err.code === 'notLoggedIn' || err.code === 'scopeMissing'
          ? { label: '設定を開く', run: () => ui.gotoSetting(twitch ? 'twitchOauth' : 'youtubeOauth') }
          : undefined;
      ui.showToast(err.message, 'error', action);
      if (err.code === 'notLoggedIn') {
        if (twitch) this.setTwitchConnected(false);
        else this.setYoutubeConnected(false);
      }
    } finally {
      const { [key]: _done, ...rest } = this.busy;
      this.busy = rest;
    }
  }

  deleteMessage(t: ModerationTarget, messageId: string): Promise<void> {
    return this.run(
      this.deleteKey(t, messageId),
      () => moderateDeleteMessage(t.platform, t.channel, messageId, t.authorId),
      'コメントを削除しました',
      () => store.markRemoved(t.platform, t.authorId, 'deleted', messageId),
      isTwitch(t),
    );
  }

  timeout(t: ModerationTarget, secs: number): Promise<void> {
    return this.run(
      this.userKey(t),
      () => moderateBanUser(t.platform, t.channel, t.authorId, secs, null),
      `タイムアウトしました(${formatTimeoutLabel(secs)})`,
      () => store.markRemoved(t.platform, t.authorId, 'timeout'),
      isTwitch(t),
    );
  }

  ban(t: ModerationTarget): Promise<void> {
    return this.run(
      this.userKey(t),
      () => moderateBanUser(t.platform, t.channel, t.authorId, null, null),
      'BAN しました',
      () => store.markRemoved(t.platform, t.authorId, 'banned'),
      isTwitch(t),
    );
  }

  unban(t: ModerationTarget): Promise<void> {
    return this.run(
      this.userKey(t),
      () => moderateUnbanUser(t.platform, t.channel, t.authorId),
      'BAN を解除しました',
      () => {},
      isTwitch(t),
    );
  }

  /**
   * Open the permanent-BAN confirmation. Pass `returnFocus` when the caller's own
   * UI (e.g. a context menu) closes before the dialog opens, so focus can go back
   * to the originating element instead of <body>.
   */
  requestBan(t: ModerationTarget, returnFocus: HTMLElement | null = null): void {
    const { platform, channel, authorId, name } = t;
    this.confirm = { platform, channel, authorId, name, returnFocus };
  }

  cancelConfirm(): void {
    this.confirm = null;
  }

  acceptConfirm(): void {
    const t = this.confirm;
    this.confirm = null;
    if (t) {
      const { platform, channel, authorId, name } = t;
      void this.ban({ platform, channel, authorId, name });
    }
  }
}

export const moderation = new ModerationState();
