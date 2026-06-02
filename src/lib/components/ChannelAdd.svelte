<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import type { ChannelConfig, ChannelStatus } from '../ipc';
  import { addChannel, removeChannel, getConfig, onStats } from '../ipc';

  // 追加成功時に親へ通知(任意)。
  let { onAdded }: { onAdded?: (ch: ChannelConfig) => void } = $props();

  // 接続中チャンネル一覧(このコンポーネントがチャンネル管理を一手に担う)。
  let channels: ChannelConfig[] = $state([]);

  // 配信タイトル(key=`${platform}:${identifier}`)。stats イベントで更新され、
  // チップ本文を識別子からタイトルへ差し替える。未取得のチャンネルは identifier 表示。
  let titles: Map<string, string> = $state(new Map());
  let channelStatus: Map<string, ChannelStatus> = $state(new Map());
  let unlistenStats: (() => void) | null = null;

  function chipKey(platform: string, identifier: string): string {
    return `${platform}:${identifier}`;
  }

  function sameChannelStatus(a: ChannelStatus | undefined, b: ChannelStatus): boolean {
    return !!a
      && (a.title ?? null) === (b.title ?? null)
      && (a.viewers ?? null) === (b.viewers ?? null)
      && (a.live ?? null) === (b.live ?? null);
  }

  function updateChannelStatus(statuses: ChannelStatus[] | undefined) {
    if (!statuses) return;
    const next = new Map(channelStatus);
    const seen = new Set<string>();
    let changed = false;

    for (const status of statuses) {
      const key = chipKey(status.platform, status.identifier);
      seen.add(key);
      if (!sameChannelStatus(next.get(key), status)) {
        next.set(key, status);
        changed = true;
      }
    }

    for (const key of next.keys()) {
      if (!seen.has(key)) {
        next.delete(key);
        changed = true;
      }
    }

    if (changed) channelStatus = next;
  }

  function statusTitle(status: ChannelStatus | undefined): string | undefined {
    const title = status?.title?.trim();
    return title ? title : undefined;
  }

  function chipTooltip(ch: ChannelConfig, status: ChannelStatus | undefined, display: string): string {
    if (status?.live === true) {
      return display === ch.identifier ? ch.identifier : `${display}\n${ch.identifier}`;
    }
    if (status?.live === false) {
      return `${ch.identifier}\n待機中（配信開始を検知すると自動接続）`;
    }
    return ch.identifier;
  }

  onMount(async () => {
    const cfg = await getConfig();
    if (cfg) channels = cfg.channels;
    unlistenStats = await onStats((s) => {
      if (s.channelTitles && s.channelTitles.length > 0) {
        const next = new Map(titles);
        let changed = false;
        for (const ct of s.channelTitles) {
          if (!ct || !ct.title) continue;
          const key = chipKey(ct.platform, ct.identifier);
          if (next.get(key) !== ct.title) {
            next.set(key, ct.title);
            changed = true;
          }
        }
        if (changed) titles = next;
      }
      updateChannelStatus(s.channelStatus);
    });
  });

  onDestroy(() => {
    unlistenStats?.();
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
  type DetectedChannel = {
    kind: 'detected';
    platform: ChannelPlatform;
    identifier: string;
    youtubeKind?: 'video' | 'channel';
  };
  type ChannelDetection =
    | DetectedChannel
    | { kind: 'manual' }
    | { kind: 'known-url'; platform: ChannelPlatform; message: string }
    | { kind: 'unsupported-url'; host: string; message: string };
  type DetectHint = { tone: 'ok' | 'warn'; message: string; identifier?: string };

  const TWITCH_HOSTS = new Set(['twitch.tv', 'm.twitch.tv']);
  const YOUTUBE_HOSTS = new Set(['youtube.com', 'm.youtube.com', 'music.youtube.com', 'youtu.be']);
  const NICONICO_HOSTS = new Set(['live.nicovideo.jp', 'live2.nicovideo.jp']);
  const YOUTUBE_PATH_ID_PREFIXES = new Set(['live', 'embed', 'shorts', 'v']);
  const YOUTUBE_VIDEO_ID_RE = /^[A-Za-z0-9_-]{11}$/;
  const YOUTUBE_CHANNEL_ID_RE = /^UC[A-Za-z0-9_-]{22}$/;
  // YouTube ハンドルは Unicode(日本語等)も許可される。ASCII 限定にすると @まちゃ05 を弾くため
  // Unicode 文字クラスで判定する。
  const YOUTUBE_HANDLE_RE = /^@[\p{L}\p{N}._-]+$/u;
  const NICONICO_LIVE_ID_RE = /^lv\d+$/i;
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
    /^(?:(?:www|m)\.twitch\.tv|twitch\.tv|(?:www|m|music)\.youtube\.com|youtube\.com|youtu\.be|live2?\.nicovideo\.jp)(?::\d{1,5})?(?:[/?#].*)?$/i;

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

  function validYoutubeChannelId(id: string | null): string | null {
    if (!id) return null;
    const trimmed = id.trim();
    return YOUTUBE_CHANNEL_ID_RE.test(trimmed) ? trimmed : null;
  }

  function validYoutubeHandle(handle: string | null): string | null {
    if (!handle) return null;
    const trimmed = handle.trim();
    return YOUTUBE_HANDLE_RE.test(trimmed) ? trimmed : null;
  }

  // URL の path セグメントは非ASCII(日本語ハンドル等)が percent-encoded のことがあるため
  // ハンドル判定前にデコードする(@%E3%81%BE…→@まちゃ)。不正な encoding は素通し。
  function decodeSegment(segment: string): string {
    try {
      return decodeURIComponent(segment);
    } catch {
      return segment;
    }
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

  // YouTube URL からチャンネル指定を抽出する(/@handle[/live] / /channel/UC...[/live])。
  function extractYoutubeChannelId(url: URL): string | null {
    const segments = url.pathname.split('/').filter(Boolean);
    if (segments.length < 1 || segments.length > 3) return null;
    const trailingLive = segments[segments.length - 1]?.toLowerCase() === 'live';
    const coreSegments = trailingLive ? segments.slice(0, -1) : segments;

    if (coreSegments.length === 1) {
      return validYoutubeHandle(decodeSegment(coreSegments[0]));
    }

    if (coreSegments.length === 2 && coreSegments[0]?.toLowerCase() === 'channel') {
      return validYoutubeChannelId(coreSegments[1]);
    }

    return null;
  }

  // ニコ生 視聴URL(/watch/lvXXXX)から番組IDを抽出する。
  function extractNiconicoLiveId(url: URL): string | null {
    const segments = url.pathname.split('/').filter(Boolean);
    if (segments[0]?.toLowerCase() !== 'watch') return null;
    const candidate = segments[1]?.toLowerCase() ?? null;
    return candidate && NICONICO_LIVE_ID_RE.test(candidate) ? candidate : null;
  }

  function detectChannel(input: string): ChannelDetection {
    const raw = input.trim();
    // 生入力(URLでない): lv番組ID を最優先で判別。
    if (NICONICO_LIVE_ID_RE.test(raw)) {
      return { kind: 'detected', platform: 'niconico', identifier: raw.toLowerCase() };
    }
    const url = parseUrlLoose(raw);
    if (!url) {
      const youtubeChannelIdentifier =
        validYoutubeHandle(raw) ?? validYoutubeChannelId(raw);
      if (youtubeChannelIdentifier) {
        return {
          kind: 'detected',
          platform: 'youtube',
          identifier: youtubeChannelIdentifier,
          youtubeKind: 'channel'
        };
      }
      return { kind: 'manual' };
    }

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
        return { kind: 'detected', platform: 'youtube', identifier: id, youtubeKind: 'video' };
      }
      const channelIdentifier = extractYoutubeChannelId(url);
      if (channelIdentifier) {
        return {
          kind: 'detected',
          platform: 'youtube',
          identifier: channelIdentifier,
          youtubeKind: 'channel'
        };
      }
      return {
        kind: 'known-url',
        platform: 'youtube',
        message: 'YouTubeの動画IDまたはチャンネル指定が見つかりません。動画ID / チャンネルID / @handle を手動入力してください。'
      };
    }

    if (NICONICO_HOSTS.has(host)) {
      const id = extractNiconicoLiveId(url);
      if (id) {
        return { kind: 'detected', platform: 'niconico', identifier: id };
      }
      return {
        kind: 'known-url',
        platform: 'niconico',
        message: 'ニコ生の番組IDが見つかりません。lvから始まる番組IDを入力してください。'
      };
    }

    return {
      kind: 'unsupported-url',
      host,
      message: `未対応のURLです (${host})。Twitch/YouTube/ニコ生の配信URLか識別子を入力してください。`
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
      const label = detection.platform === 'twitch'
        ? 'Twitch'
        : detection.platform === 'niconico'
          ? 'ニコ生'
          : detection.youtubeKind === 'channel'
            ? 'YouTubeチャンネル'
            : 'YouTube動画';
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
    if (!raw) { addError = 'URL か Twitchチャンネル名 / YouTube動画ID / YouTubeチャンネル指定 / lv番組ID を入力してください'; return; }

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
        if (value === 'twitch' || value === 'youtube' || value === 'niconico') newPlatform = value;
      }}
    >
      <option value="twitch">Twitch</option>
      <option value="youtube">YouTube</option>
      <option value="niconico">ニコ生</option>
    </select>
    <input
      type="text"
      bind:value={newIdentifier}
      placeholder="配信URLを貼り付け（または Twitchチャンネル名 / YouTube動画ID / @handle / lv番組ID）"
      class="id-input"
      aria-label="配信URL または チャンネル名 / 動画ID / YouTubeチャンネル / lv番組ID"
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
        {@const key = chipKey(ch.platform, ch.identifier)}
        {@const status = channelStatus.get(key)}
        {@const display = statusTitle(status) ?? titles.get(key) ?? ch.identifier}
        {@const isLive = status?.live === true}
        {@const isIdle = status?.live === false}
        {@const bodyText = isIdle ? ch.identifier : display}
        <span
          class="chip"
          class:twitch={ch.platform === 'twitch'}
          class:youtube={ch.platform === 'youtube'}
          class:niconico={ch.platform === 'niconico'}
          class:live={isLive}
          class:idle={isIdle}
          role="listitem"
          title={chipTooltip(ch, status, bodyText)}
        >
          <span class="chip-dot" aria-hidden="true"></span>
          {#if isLive}<span class="live-badge">LIVE</span>{/if}
          <span class="chip-id">{bodyText}</span>
          <button class="chip-x" title="削除" aria-label="{bodyText} を削除" onclick={() => onRemove(ch)}>×</button>
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
    gap: 5px;
    background: rgba(255,255,255,0.08);
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 12px;
    padding: 1px 4px 1px 8px;
    font-size: 11px;
    color: #ccc;
    max-width: 220px;
  }

  /* Per-platform brand coloring: border + faint background tint + accent dot. */
  .chip-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    flex-shrink: 0;
    background: #9e9e9e;
  }

  .chip.twitch {
    border-color: rgba(145,70,255,0.65);
    background: rgba(145,70,255,0.14);
  }
  .chip.twitch .chip-dot { background: #9146ff; }

  .chip.youtube {
    border-color: rgba(255,0,0,0.55);
    background: rgba(255,0,0,0.12);
  }
  .chip.youtube .chip-dot { background: #ff0000; }

  .chip.niconico {
    border-color: rgba(255,190,0,0.55);
    background: rgba(255,190,0,0.12);
  }
  .chip.niconico .chip-dot { background: #ffbe00; }

  .chip.live .chip-dot {
    animation: live-pulse 1.3s ease-in-out infinite;
    box-shadow: 0 0 0 0 rgba(255, 0, 0, 0.6);
  }

  .chip.idle {
    opacity: 0.6;
  }

  .chip.idle .chip-dot {
    background: #9e9e9e;
  }

  .live-badge {
    background: #e53935;
    color: #fff;
    border-radius: 3px;
    font-size: 9px;
    font-weight: 700;
    line-height: 1;
    padding: 2px 3px;
  }

  @keyframes live-pulse {
    0% { box-shadow: 0 0 0 0 rgba(255, 0, 0, 0.55); opacity: 1; }
    70% { box-shadow: 0 0 0 5px rgba(255, 0, 0, 0); opacity: 0.72; }
    100% { box-shadow: 0 0 0 0 rgba(255, 0, 0, 0); opacity: 1; }
  }

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
