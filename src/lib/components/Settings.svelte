<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import type { AppConfig, ChannelConfig, TtsOptions } from '../ipc';
  import {
    getConfig, setConfig, addChannel, removeChannel, getObsUrl
  } from '../ipc';
  import { ui, SETTINGS_ANCHOR_IDS } from '../ui.svelte';
  import { setNotify } from '../stores.svelte';

  let config: AppConfig | null = $state(null);
  let obsBaseUrl: string = $state('');
  let copied: boolean = $state(false);

  // New channel form
  let newPlatform: 'twitch' | 'youtube' = $state('twitch');
  let newIdentifier: string = $state('');
  let addError: string = $state('');

  // NG / highlight edit buffers
  let ngWordsText: string = $state('');
  let ngUsersText: string = $state('');
  let highlightsText: string = $state('');

  // Template selection is persisted in config (config.obs.template); SPEC §10
  // mandates config as the single persistence source (no localStorage).

  let saving: boolean = $state(false);
  let saveMsg: string = $state('');

  // Scroll to settings section when the command palette sets a settingsAnchor.
  // Gate on `config`: the tts/obs/moderation sections live inside {#if config},
  // so a cold navigation from the comments tab mounts Settings while config is
  // still loading (null) and the target section is not yet in the DOM. We read
  // `config` (so it is tracked as a dependency) and bail without clearing the
  // anchor until config resolves; the effect then re-fires and the scroll lands.
  $effect(() => {
    const a = ui.settingsAnchor;
    if (!a) return;
    if (!config) return;
    const id = SETTINGS_ANCHOR_IDS[a];
    requestAnimationFrame(() => {
      document.getElementById(id)?.scrollIntoView({ behavior: 'smooth', block: 'start' });
    });
    ui.clearSettingsAnchor();
  });

  // setTimeout handles (cleared on destroy)
  let saveMsgTimer: ReturnType<typeof setTimeout> | null = null;
  let copiedTimer: ReturnType<typeof setTimeout> | null = null;

  // ── TTS options accessors (config.tts.options mirrors Rust TtsOptions) ──
  // Keys are partitioned by value type so reads/writes stay type-checked
  // against the TtsOptions interface.
  type TtsNumKey = {
    [K in keyof TtsOptions]-?: NonNullable<TtsOptions[K]> extends number ? K : never;
  }[keyof TtsOptions];
  type TtsBoolKey = {
    [K in keyof TtsOptions]-?: NonNullable<TtsOptions[K]> extends boolean ? K : never;
  }[keyof TtsOptions];

  function ttsNum(key: TtsNumKey, fallback: number): number {
    const v = config?.tts.options?.[key];
    return typeof v === 'number' && Number.isFinite(v) ? v : fallback;
  }
  function ttsBool(key: TtsBoolKey, fallback: boolean): boolean {
    const v = config?.tts.options?.[key];
    return typeof v === 'boolean' ? v : fallback;
  }
  function setTtsNum(key: TtsNumKey, value: number) {
    if (!config) return;
    config.tts.options[key] = value;
  }
  function setTtsBool(key: TtsBoolKey, value: boolean) {
    if (!config) return;
    config.tts.options[key] = value;
  }

  // Bound proxies for TTS option fields
  // Rust default for tts.options.maxLength is 140 (config.rs default_max_read_len).
  // Initial/fallback must match so an unedited save does not silently change it.
  const MAX_LENGTH_DEFAULT = 140;
  let voicevoxSpeaker: number = $state(1);
  let maxLength: number = $state(MAX_LENGTH_DEFAULT);
  let stripEmoji: boolean = $state(true);

  onMount(async () => {
    config = await getConfig();
    if (config) {
      ngWordsText = config.moderation.ngWords.join('\n');
      ngUsersText = config.moderation.ngUsers.join('\n');
      highlightsText = config.moderation.highlights.join('\n');
      voicevoxSpeaker = ttsNum('voicevoxSpeaker', 1);
      maxLength = ttsNum('maxLength', MAX_LENGTH_DEFAULT);
      stripEmoji = ttsBool('stripEmoji', true);
      // Backward-compat: older config.json may lack obs.template.
      if (!config.obs.template || !config.obs.template.trim()) {
        config.obs.template = 'default';
      }
    }

    const url = await getObsUrl();
    obsBaseUrl = url ?? 'http://127.0.0.1:11180/?template=default';
  });

  onDestroy(() => {
    if (saveMsgTimer !== null) clearTimeout(saveMsgTimer);
    if (copiedTimer !== null) clearTimeout(copiedTimer);
  });

  // Displayed OBS URL: base URL with the config template reflected in.
  const obsUrl = $derived.by(() => {
    const tmpl = config?.obs.template ?? 'default';
    return withTemplate(obsBaseUrl, tmpl);
  });

  function withTemplate(url: string, tmpl: string): string {
    const name = (tmpl || 'default').trim() || 'default';
    try {
      const u = new URL(url);
      u.searchParams.set('template', name);
      return u.toString();
    } catch {
      return url;
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
    'about',
    'admin',
    'bits',
    'broadcast',
    'clip',
    'clips',
    'creatorcamp',
    'creator-dashboard',
    'dashboard',
    'directory',
    'downloads',
    'drops',
    'embed',
    'event',
    'events',
    'following',
    'friends',
    'inventory',
    'jobs',
    'login',
    'logout',
    'messages',
    'moderator',
    'p',
    'payments',
    'popout',
    'prime',
    'search',
    'settings',
    'signup',
    'store',
    'subscriptions',
    'team',
    'teams',
    'turbo',
    'user',
    'videos',
    'wallet',
    'whispers'
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
    if (!config) return;
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
      // Update only the channels list to avoid discarding unsaved NG/TTS edits.
      if (!config.channels.some(c => c.platform === ch.platform && c.identifier === ch.identifier)) {
        config.channels = [...config.channels, ch];
      }
      newIdentifier = '';
    } catch (e) {
      addError = `追加に失敗しました: ${e instanceof Error ? e.message : String(e)}`;
    }
  }

  async function onRemoveChannel(ch: ChannelConfig) {
    if (!config) return;
    try {
      await removeChannel(`${ch.platform}:${ch.identifier}`);
      config.channels = config.channels.filter(
        c => !(c.platform === ch.platform && c.identifier === ch.identifier)
      );
    } catch (e) {
      addError = `削除に失敗しました: ${e instanceof Error ? e.message : String(e)}`;
    }
  }

  async function onSave() {
    if (!config) return;
    saving = true;
    saveMsg = '';
    config.moderation.ngWords = ngWordsText.split('\n').map(s => s.trim()).filter(Boolean);
    config.moderation.ngUsers = ngUsersText.split('\n').map(s => s.trim()).filter(Boolean);
    config.moderation.highlights = highlightsText.split('\n').map(s => s.trim()).filter(Boolean);
    setTtsNum('voicevoxSpeaker', Number.isFinite(voicevoxSpeaker) ? Math.trunc(voicevoxSpeaker) : 1);
    setTtsNum('maxLength', Number.isFinite(maxLength) && maxLength >= 0 ? Math.trunc(maxLength) : MAX_LENGTH_DEFAULT);
    setTtsBool('stripEmoji', stripEmoji);
    // Normalize template before persisting (config is the single source).
    config.obs.template = (config.obs.template || 'default').trim() || 'default';
    try {
      await setConfig(config);
      setNotify(config.ui.notifySound, config.ui.notifyVolume);
      saveMsg = '保存しました';
    } catch (e) {
      saveMsg = `保存に失敗しました: ${e instanceof Error ? e.message : String(e)}`;
    } finally {
      saving = false;
    }
    if (saveMsgTimer !== null) clearTimeout(saveMsgTimer);
    saveMsgTimer = setTimeout(() => { saveMsg = ''; saveMsgTimer = null; }, 3000);
  }

  function onCopyObs() {
    navigator.clipboard.writeText(obsUrl)
      .then(() => {
        copied = true;
        if (copiedTimer !== null) clearTimeout(copiedTimer);
        copiedTimer = setTimeout(() => { copied = false; copiedTimer = null; }, 1500);
      })
      .catch(() => { /* clipboard denied — do not show success */ });
  }
</script>

<div class="settings">
  <h2>設定</h2>

  <!-- ── Channels ── -->
  <section id="settings-channels">
    <h3>チャンネル</h3>
    {#if config && config.channels.length > 0}
      <ul class="channel-list">
        {#each config.channels as ch}
          <li>
            <span class="platform-badge" class:twitch={ch.platform === 'twitch'} class:youtube={ch.platform === 'youtube'}>
              {ch.platform}
            </span>
            <span class="ch-id">{ch.identifier}</span>
            <button class="remove-btn" onclick={() => onRemoveChannel(ch)}>削除</button>
          </li>
        {/each}
      </ul>
    {:else}
      <p class="empty">チャンネルなし</p>
    {/if}

    <div class="add-channel-row">
      <select
        value={effectivePlatform}
        class="platform-select"
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
  </section>

  <!-- ── TTS ── -->
  {#if config}
  <section id="settings-tts">
    <h3>TTS（読み上げ）</h3>
    <div class="field-row">
      <label for="tts-backend">バックエンド</label>
      <select id="tts-backend" bind:value={config.tts.backend} class="platform-select">
        <option value="none">読み上げOFF</option>
        <option value="webSpeech">Web Speech（内蔵）</option>
        <option value="bouyomi">棒読みちゃん</option>
        <option value="voicevox">VOICEVOX</option>
      </select>
    </div>

    {#if config.tts.backend === 'voicevox'}
    <div class="field-row">
      <label for="tts-voicevox-speaker">VOICEVOX 話者ID</label>
      <input
        id="tts-voicevox-speaker"
        type="number"
        min="0"
        step="1"
        bind:value={voicevoxSpeaker}
        class="num-input"
      />
    </div>
    {/if}

    <div class="field-row">
      <label for="tts-max-length">最大読み上げ文字数</label>
      <input
        id="tts-max-length"
        type="number"
        min="0"
        step="1"
        bind:value={maxLength}
        class="num-input"
      />
      <span class="hint-inline">（0で無制限）</span>
    </div>

    <div class="field-row">
      <label for="strip-emoji">絵文字を除去</label>
      <input id="strip-emoji" type="checkbox" bind:checked={stripEmoji} class="chk" />
    </div>
  </section>

  <!-- ── OBS URL ── -->
  <section id="settings-obs">
    <h3>OBSオーバーレイ URL</h3>
    <div class="field-row">
      <label for="obs-template">テンプレート</label>
      <input
        id="obs-template"
        type="text"
        list="obs-template-list"
        bind:value={config.obs.template}
        class="id-input"
        placeholder="default"
      />
      <datalist id="obs-template-list">
        <option value="default"></option>
      </datalist>
    </div>
    <div class="obs-row">
      <input type="text" value={obsUrl} readonly class="obs-input" />
      <button class="copy-btn" class:copied onclick={onCopyObs}>
        {copied ? 'コピー済' : 'コピー'}
      </button>
    </div>
    <p class="hint">OBSのブラウザソースにこのURLを貼り付けてください。</p>
  </section>

  <!-- ── Moderation ── -->
  <section id="settings-moderation">
    <h3>NGワード <span class="hint-inline">（1行1語、正規表現可）</span></h3>
    <textarea bind:value={ngWordsText} rows={4} class="mod-area" placeholder="NG word&#10;bad_word"></textarea>

    <h3>NGユーザー <span class="hint-inline">（1行1ID）</span></h3>
    <textarea bind:value={ngUsersText} rows={3} class="mod-area" placeholder="username123"></textarea>

    <h3>ハイライト <span class="hint-inline">（ユーザー名またはキーワード）</span></h3>
    <textarea bind:value={highlightsText} rows={3} class="mod-area" placeholder="!command&#10;favorite_user"></textarea>
  </section>

  <!-- ── Notification ── -->
  <section id="settings-notify">
    <h3>通知</h3>
    <div class="field-row">
      <label for="notify-sound">ハイライトを効果音で通知</label>
      <input
        id="notify-sound"
        type="checkbox"
        bind:checked={config.ui.notifySound}
        class="chk"
        onchange={() => setNotify(config!.ui.notifySound, config!.ui.notifyVolume)}
      />
    </div>
    <div class="field-row">
      <label for="notify-volume">音量</label>
      <input
        id="notify-volume"
        type="range"
        min="0"
        max="1"
        step="0.05"
        bind:value={config.ui.notifyVolume}
        class="vol-slider"
        disabled={!config.ui.notifySound}
        oninput={() => setNotify(config!.ui.notifySound, config!.ui.notifyVolume)}
      />
      <span class="hint-inline">{Math.round(config.ui.notifyVolume * 100)}%</span>
    </div>
  </section>

  <!-- ── Save ── -->
  <div class="save-row">
    <button class="save-btn" onclick={onSave} disabled={saving}>
      {saving ? '保存中...' : '設定を保存'}
    </button>
    {#if saveMsg}<span class="save-ok">{saveMsg}</span>{/if}
  </div>
  {/if}
</div>

<style>
  .settings {
    padding: 12px 16px;
    overflow-y: auto;
    height: 100%;
    box-sizing: border-box;
  }

  h2 {
    font-size: 15px;
    font-weight: 700;
    margin: 0 0 12px;
    color: #e0e0e0;
  }

  h3 {
    font-size: 12px;
    font-weight: 600;
    color: #9e9e9e;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    margin: 14px 0 6px;
  }

  section {
    border-bottom: 1px solid rgba(255,255,255,0.07);
    padding-bottom: 12px;
    margin-bottom: 4px;
  }

  .channel-list {
    list-style: none;
    margin: 0 0 8px;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .channel-list li {
    display: flex;
    align-items: center;
    gap: 6px;
    background: rgba(255,255,255,0.05);
    border-radius: 4px;
    padding: 4px 8px;
  }

  .platform-badge {
    font-size: 10px;
    font-weight: 700;
    padding: 1px 6px;
    border-radius: 10px;
    text-transform: uppercase;
  }

  .platform-badge.twitch { background: #9146ff; color: #fff; }
  .platform-badge.youtube { background: #ff0000; color: #fff; }

  .ch-id {
    flex: 1;
    font-size: 12px;
    color: #ccc;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .add-channel-row {
    display: flex;
    gap: 6px;
    align-items: center;
  }

  .platform-select, .id-input, .obs-input, .num-input {
    background: rgba(255,255,255,0.07);
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 4px;
    color: #e0e0e0;
    padding: 5px 8px;
    font-size: 13px;
  }

  .platform-select { flex-shrink: 0; }
  .id-input { flex: 1; }
  .obs-input { flex: 1; font-size: 12px; }
  .num-input { width: 90px; }
  .chk { width: 16px; height: 16px; accent-color: #1976d2; }
  .vol-slider { flex: 1; max-width: 160px; accent-color: #1976d2; }
  .vol-slider:disabled { opacity: 0.4; }

  .add-btn, .remove-btn, .copy-btn, .save-btn {
    border: none;
    border-radius: 4px;
    cursor: pointer;
    font-size: 12px;
    padding: 5px 10px;
    font-weight: 600;
    transition: opacity 0.15s;
  }

  .add-btn { background: #1976d2; color: #fff; }
  .remove-btn { background: rgba(244,67,54,0.15); color: #f44336; }
  .copy-btn { background: #37474f; color: #fff; min-width: 60px; }
  .copy-btn.copied { background: #2e7d32; }
  .save-btn { background: #1976d2; color: #fff; padding: 7px 18px; }
  .save-btn:disabled { opacity: 0.5; cursor: not-allowed; }

  .obs-row {
    display: flex;
    gap: 6px;
    align-items: center;
  }

  .mod-area {
    width: 100%;
    background: rgba(255,255,255,0.05);
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 4px;
    color: #e0e0e0;
    font-size: 12px;
    padding: 6px 8px;
    resize: vertical;
    box-sizing: border-box;
    font-family: monospace;
  }

  .field-row {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-top: 6px;
  }

  .field-row label {
    font-size: 13px;
    color: #ccc;
    min-width: 90px;
  }

  .hint { font-size: 11px; color: #757575; margin: 4px 0 0; }
  .hint-inline { font-size: 11px; color: #757575; font-weight: 400; text-transform: none; letter-spacing: 0; }
  .error { color: #f44336; font-size: 11px; margin: 4px 0 0; }
  .empty { color: #757575; font-size: 12px; }
  .detect-hint { color: #4caf50; font-size: 11px; margin: 4px 0 0; }
  .detect-hint.warn { color: #ffb74d; }
  .detect-hint code {
    background: rgba(255, 255, 255, 0.08);
    padding: 0 4px;
    border-radius: 3px;
    font-family: ui-monospace, monospace;
  }

  .save-row {
    display: flex;
    align-items: center;
    gap: 10px;
    padding-top: 12px;
  }

  .save-ok { color: #66bb6a; font-size: 12px; }
</style>
