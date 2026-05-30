<script lang="ts">
  import { onMount } from 'svelte';
  import type { ChannelConfig } from '../ipc';
  import { addChannel, removeChannel, getConfig } from '../ipc';

  // 追加成功時に親へ通知(任意)。
  let { onAdded }: { onAdded?: (ch: ChannelConfig) => void } = $props();

  // 接続中チャンネル一覧(このコンポーネントがチャンネル管理を一手に担う)。
  let channels: ChannelConfig[] = $state([]);

  onMount(async () => {
    const cfg = await getConfig();
    if (cfg) channels = cfg.channels;
  });

  function sameChannel(a: ChannelConfig, b: ChannelConfig): boolean {
    return a.platform === b.platform && a.identifier === b.identifier;
  }

  async function onRemove(ch: ChannelConfig) {
    addError = '';
    try {
      await removeChannel(`${ch.platform}:${ch.identifier}`);
      channels = channels.filter((c) => !sameChannel(c, ch));
    } catch (e) {
      addError = `削除に失敗しました: ${e instanceof Error ? e.message : String(e)}`;
    }
  }

  // 入力(配信URL or 生ID/名)から配信プラットフォームと識別子を判別する。
  // URL でない生入力は manual として扱い、手動選択へフォールバックする。
  type ChannelPlatform = ChannelConfig['platform'];
  type DetectedChannel = { kind: 'detected'; platform: ChannelPlatform; identifier: string };
  type ChannelDetection =
    | DetectedChannel
    | { kind: 'manual' }
    | { kind: 'known-url'; platform: ChannelPlatform; message: string }
    | { kind: 'unsupported-url'; host: string; message: string };
  type DetectHint = { tone: 'ok' | 'warn'; message: string; identifier?: string };

  const TWITCH_HOSTS = new Set(['twitch.tv', 'm.twitch.tv']);
  const YOUTUBE_HOSTS = new Set(['youtube.com', 'm.youtube.com', 'music.youtube.com', 'youtu.be']);
  const YOUTUBE_PATH_ID_PREFIXES = new Set(['live', 'embed', 'shorts', 'v']);
  const YOUTUBE_VIDEO_ID_RE = /^[A-Za-z0-9_-]{11}$/;
  const TWITCH_LOGIN_RE = /^[a-z0-9_]{2,25}$/;
  const TWITCH_RESERVED_PATHS = new Set([
    'about', 'admin', 'bits', 'broadcast', 'clip', 'clips', 'creatorcamp',
    'creator-dashboard', 'dashboard', 'directory', 'downloads', 'drops', 'embed',
    'event', 'events', 'following', 'friends', 'inventory', 'jobs', 'login',
    'logout', 'messages', 'moderator', 'p', 'payments', 'popout', 'prime',
    'search', 'settings', 'signup', 'store', 'subscriptions', 'team', 'teams',
    'turbo', 'user', 'videos', 'wallet', 'whispers'
  ]);
  const SUPPORTED_SCHEMELESS_URL_RE =
    /^(?:(?:www|m)\.twitch\.tv|twitch\.tv|(?:www|m|music)\.youtube\.com|youtube\.com|youtu\.be)(?::\d{1,5})?(?:[/?#].*)?$/i;

  // スキーム無しの "twitch.tv/foo" / "youtu.be/xxx" 等も URL として拾う。
  function parseUrlLoose(input: string): URL | null {
    const trimmed = input.trim();
    if (!trimmed) return null;
    try {
      const url = new URL(trimmed);
      return (url.protocol === 'http:' || url.protocol === 'https:') && url.hostname
        ? url
        : null;
    } catch {
      if (SUPPORTED_SCHEMELESS_URL_RE.test(trimmed)) {
        try { return new URL('https://' + trimmed); } catch { return null; }
      }
      return null;
    }
  }

  function normalizedHost(url: URL): string {
    return url.hostname.replace(/^www\./i, '').toLowerCase();
  }

  function extractTwitchLogin(url: URL): string | null {
    const segments = url.pathname.split('/').filter(Boolean);
    if (segments.length !== 1) return null;
    const login = segments[0].toLowerCase();
    if (TWITCH_RESERVED_PATHS.has(login)) return null;
    return TWITCH_LOGIN_RE.test(login) ? login : null;
  }

  function validYoutubeVideoId(id: string | null): string | null {
    if (!id) return null;
    const trimmed = id.trim();
    return YOUTUBE_VIDEO_ID_RE.test(trimmed) ? trimmed : null;
  }

  // YouTube URL から videoId を抽出する(watch?v= / youtu.be / live / embed / shorts)。
  function extractYoutubeId(url: URL): string | null {
    const host = normalizedHost(url);
    let candidate: string | null = null;
    if (host === 'youtu.be') {
      candidate = url.pathname.split('/').filter(Boolean)[0] ?? null;
    } else {
      candidate = url.searchParams.get('v');
      if (!candidate) {
        const segments = url.pathname.split('/').filter(Boolean);
        const prefix = segments[0]?.toLowerCase();
        if (prefix && segments[1] && YOUTUBE_PATH_ID_PREFIXES.has(prefix)) {
          candidate = segments[1];
        }
      }
    }
    return validYoutubeVideoId(candidate);
  }

  function detectChannel(input: string): ChannelDetection {
    const url = parseUrlLoose(input.trim());
    if (!url) return { kind: 'manual' };

    const host = normalizedHost(url);
    if (TWITCH_HOSTS.has(host)) {
      const login = extractTwitchLogin(url);
      if (login) {
        return { kind: 'detected', platform: 'twitch', identifier: login };
      }
      return {
        kind: 'known-url',
        platform: 'twitch',
        message: 'Twitchチャンネル名が見つかりません。チャンネル名を手動入力してください。'
      };
    }

    if (YOUTUBE_HOSTS.has(host)) {
      const id = extractYoutubeId(url);
      if (id) {
        return { kind: 'detected', platform: 'youtube', identifier: id };
      }
      return {
        kind: 'known-url',
        platform: 'youtube',
        message: 'YouTubeのライブ配信IDが見つかりません。動画IDを手動入力してください。'
      };
    }

    return {
      kind: 'unsupported-url',
      host,
      message: `未対応のURLです (${host})。Twitch/YouTubeの配信URLか識別子を入力してください。`
    };
  }

  let newPlatform: ChannelPlatform = $state('twitch');
  let newIdentifier: string = $state('');
  let addError: string = $state('');

  // URL を貼ったときの自動判別結果(プレビュー表示と追加処理に使う)。
  const detection = $derived(detectChannel(newIdentifier));
  const detected = $derived(detection.kind === 'detected' ? detection : null);
  const effectivePlatform = $derived(detected?.platform ?? newPlatform);
  const detectHint = $derived.by((): DetectHint | null => {
    if (detection.kind === 'detected') {
      const label = detection.platform === 'twitch' ? 'Twitch' : 'YouTube';
      return { tone: 'ok', message: `${label} として自動判別:`, identifier: detection.identifier };
    }
    if (detection.kind === 'known-url' || detection.kind === 'unsupported-url') {
      return { tone: 'warn', message: detection.message };
    }
    return null;
  });

  async function onAddChannel() {
    addError = '';
    const raw = newIdentifier.trim();
    if (!raw) { addError = 'URL か Twitchチャンネル名 / YouTube動画ID を入力してください'; return; }

    // URL から判別できればそれを優先。生入力は手動選択 + 生ID/名として追加する。
    const det = detectChannel(raw);
    if (det.kind === 'known-url' || det.kind === 'unsupported-url') {
      addError = det.message;
      return;
    }

    const platform = det.kind === 'detected' ? det.platform : newPlatform;
    const identifier = det.kind === 'detected' ? det.identifier : raw;

    const ch: ChannelConfig = { platform, identifier, enabled: true };
    try {
      await addChannel(ch);
      if (!channels.some((c) => sameChannel(c, ch))) {
        channels = [...channels, ch];
      }
      onAdded?.(ch);
      newIdentifier = '';
    } catch (e) {
      addError = `追加に失敗しました: ${e instanceof Error ? e.message : String(e)}`;
    }
  }
</script>

<div class="channel-add">
  <div class="add-channel-row">
    <select
      value={effectivePlatform}
      class="platform-select"
      aria-label="プラットフォーム"
      onchange={(e) => {
        const value = (e.currentTarget as HTMLSelectElement).value;
        if (value === 'twitch' || value === 'youtube') newPlatform = value;
      }}
    >
      <option value="twitch">Twitch</option>
      <option value="youtube">YouTube</option>
    </select>
    <input
      type="text"
      bind:value={newIdentifier}
      placeholder="配信URLを貼り付け（または Twitchチャンネル名 / YouTube動画ID）"
      class="id-input"
      aria-label="配信URL または チャンネル名 / 動画ID"
      onkeydown={(e) => e.key === 'Enter' && onAddChannel()}
    />
    <button class="add-btn" onclick={onAddChannel}>追加</button>
  </div>
  {#if detectHint}
    <p class="detect-hint" class:warn={detectHint.tone === 'warn'}>
      {detectHint.message}
      {#if detectHint.identifier}<code>{detectHint.identifier}</code>{/if}
    </p>
  {/if}
  {#if addError}<p class="error">{addError}</p>{/if}
  {#if channels.length > 0}
    <div class="channel-chips" role="list" aria-label="接続中チャンネル">
      {#each channels as ch (ch.platform + ':' + ch.identifier)}
        <span class="chip" class:twitch={ch.platform === 'twitch'} class:youtube={ch.platform === 'youtube'} role="listitem">
          <span class="chip-id">{ch.identifier}</span>
          <button class="chip-x" title="削除" aria-label="{ch.identifier} を削除" onclick={() => onRemove(ch)}>×</button>
        </span>
      {/each}
    </div>
  {/if}
</div>

<style>
  .channel-add {
    display: flex;
    flex-direction: column;
  }

  .add-channel-row {
    display: flex;
    gap: 6px;
    align-items: center;
  }

  .platform-select, .id-input {
    background: rgba(255,255,255,0.07);
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 4px;
    color: #e0e0e0;
    padding: 5px 8px;
    font-size: 13px;
  }

  .platform-select { flex-shrink: 0; }
  .id-input { flex: 1; min-width: 0; }
  .id-input::placeholder { color: #555; }
  .id-input:focus, .platform-select:focus { outline: none; border-color: rgba(255,255,255,0.25); }

  .add-btn {
    border: none;
    border-radius: 4px;
    cursor: pointer;
    font-size: 12px;
    padding: 5px 10px;
    font-weight: 600;
    background: #1976d2;
    color: #fff;
    flex-shrink: 0;
    transition: opacity 0.15s;
  }
  .add-btn:hover { opacity: 0.9; }

  .error { color: #f44336; font-size: 11px; margin: 4px 0 0; }
  .detect-hint { color: #4caf50; font-size: 11px; margin: 4px 0 0; }
  .detect-hint.warn { color: #ffb74d; }
  .detect-hint code {
    background: rgba(255, 255, 255, 0.08);
    padding: 0 4px;
    border-radius: 3px;
    font-family: ui-monospace, monospace;
  }

  .channel-chips {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    margin-top: 6px;
  }

  .chip {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    background: rgba(255,255,255,0.08);
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 12px;
    padding: 1px 4px 1px 8px;
    font-size: 11px;
    color: #ccc;
    max-width: 220px;
  }
  .chip.twitch { border-color: rgba(145,70,255,0.5); }
  .chip.youtube { border-color: rgba(255,0,0,0.4); }

  .chip-id {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .chip-x {
    border: none;
    background: none;
    color: #888;
    cursor: pointer;
    font-size: 13px;
    line-height: 1;
    padding: 0 2px;
    border-radius: 50%;
  }
  .chip-x:hover { color: #f44336; }
</style>
