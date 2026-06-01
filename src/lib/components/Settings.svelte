<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import type { AppConfig, GoalsConfig, TtsDictEntry, TtsOptions } from '../ipc';
  import {
    getConfig, setConfig, getObsUrl, getObsGoalsUrl
  } from '../ipc';
  import { ui, SETTINGS_ANCHOR_IDS } from '../ui.svelte';
  import { setNotify } from '../stores.svelte';

  interface Props {
    onConfigSaved?: (config: AppConfig) => void;
  }

  let { onConfigSaved }: Props = $props();

  let config: AppConfig | null = $state(null);
  let obsBaseUrl: string = $state('');
  let obsGoalsBaseUrl: string = $state('');
  let copiedObs: boolean = $state(false);
  let copiedGiftObs: boolean = $state(false);
  let copiedGoalsObs: boolean = $state(false);

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
  let copiedObsTimer: ReturnType<typeof setTimeout> | null = null;
  let copiedGiftObsTimer: ReturnType<typeof setTimeout> | null = null;
  let copiedGoalsObsTimer: ReturnType<typeof setTimeout> | null = null;

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
  let readName: boolean = $state(true);
  let bouyomiPath: string = $state('');
  let webSpeechRate: number = $state(1);
  let webSpeechPitch: number = $state(1);
  let webSpeechVolume: number = $state(1);
  let webSpeechVoice: string = $state('');
  let ttsDictionary: TtsDictEntry[] = $state([]);
  let speechVoices: SpeechSynthesisVoice[] = $state([]);
  let speechVoicesListenerAttached: boolean = false;
  let obsTtlSeconds: number = $state(12);

  onMount(async () => {
    config = await getConfig();
    if (config) {
      ngWordsText = config.moderation.ngWords.join('\n');
      ngUsersText = config.moderation.ngUsers.join('\n');
      highlightsText = config.moderation.highlights.join('\n');
      voicevoxSpeaker = ttsNum('voicevoxSpeaker', 1);
      maxLength = ttsNum('maxLength', MAX_LENGTH_DEFAULT);
      stripEmoji = ttsBool('stripEmoji', true);
      readName = ttsBool('readName', true);
      bouyomiPath = config.tts.options.bouyomiPath ?? '';
      webSpeechRate = ttsNum('webSpeechRate', 1);
      webSpeechPitch = ttsNum('webSpeechPitch', 1);
      webSpeechVolume = ttsNum('webSpeechVolume', 1);
      webSpeechVoice = config.tts.options.webSpeechVoice ?? '';
      ttsDictionary = normalizeTtsDictionary(config.tts.options.dictionary);
      config.tts.options.dictionary = ttsDictionary;
      normalizeObsConfig(true);
      normalizeGoalsConfig();
      normalizeParticipationConfig();
    }

    const url = await getObsUrl();
    obsBaseUrl = url ?? 'http://127.0.0.1:11180/?template=default';
    obsGoalsBaseUrl = await getObsGoalsUrl();

    refreshSpeechVoices();
    if (typeof window !== 'undefined' && 'speechSynthesis' in window) {
      window.speechSynthesis.addEventListener('voiceschanged', refreshSpeechVoices);
      speechVoicesListenerAttached = true;
    }
  });

  onDestroy(() => {
    if (saveMsgTimer !== null) clearTimeout(saveMsgTimer);
    if (copiedObsTimer !== null) clearTimeout(copiedObsTimer);
    if (copiedGiftObsTimer !== null) clearTimeout(copiedGiftObsTimer);
    if (copiedGoalsObsTimer !== null) clearTimeout(copiedGoalsObsTimer);
    if (speechVoicesListenerAttached && typeof window !== 'undefined' && 'speechSynthesis' in window) {
      window.speechSynthesis.removeEventListener('voiceschanged', refreshSpeechVoices);
      speechVoicesListenerAttached = false;
    }
  });

  // Displayed OBS URL: base URL with the config template and appearance reflected in.
  const obsUrl = $derived.by(() => {
    return withTemplate(obsBaseUrl, config?.obs ?? null);
  });

  const giftObsUrl = $derived.by(() => {
    return withOnlyGift(obsUrl);
  });

  const goalsObsUrl = $derived.by(() => {
    return withGoalsParams(obsGoalsBaseUrl, config?.obs ?? null);
  });

  function withTemplate(url: string, obs: AppConfig['obs'] | null): string {
    const name = (obs?.template || 'default').trim() || 'default';
    try {
      const u = new URL(url);
      u.searchParams.set('template', name);
      u.searchParams.set('max', String(clampInt(obs?.maxRows, 8, 1, 20)));
      u.searchParams.set('ttl', String(obs ? ttlMsFromSeconds(obsTtlSeconds) : 12000));
      u.searchParams.set('font', String(clampInt(obs?.fontScalePct, 100, 50, 200)));
      u.searchParams.set('bg', String(clampInt(obs?.bgOpacityPct, 0, 0, 100)));
      u.searchParams.set('pos', obs?.position === 'top' ? 'top' : 'bottom');
      u.searchParams.set('icon', obs?.showPlatform === false ? '0' : '1');
      return u.toString();
    } catch {
      return url;
    }
  }

  function withOnlyGift(url: string): string {
    try {
      const u = new URL(url);
      u.searchParams.set('only', 'gift');
      return u.toString();
    } catch {
      return url;
    }
  }

  function withGoalsParams(url: string, obs: AppConfig['obs'] | null): string {
    try {
      const u = new URL(url);
      u.searchParams.set('template', 'goals');
      u.searchParams.set('font', String(clampInt(obs?.fontScalePct, 100, 50, 200)));
      u.searchParams.set('bg', String(clampInt(obs?.bgOpacityPct, 0, 0, 100)));
      u.searchParams.set('pos', obs?.position === 'top' ? 'top' : 'bottom');
      return u.toString();
    } catch {
      return url;
    }
  }

  function refreshSpeechVoices() {
    if (typeof window === 'undefined' || !('speechSynthesis' in window)) {
      speechVoices = [];
      return;
    }
    speechVoices = window.speechSynthesis.getVoices();
  }

  function clampNumber(value: unknown, fallback: number, min: number, max: number): number {
    const n = typeof value === 'number' ? value : Number(value);
    if (!Number.isFinite(n)) return fallback;
    return Math.min(max, Math.max(min, n));
  }

  function clampInt(value: unknown, fallback: number, min: number, max: number): number {
    return Math.trunc(clampNumber(value, fallback, min, max));
  }

  function positiveInt(value: unknown, fallback: number): number {
    const n = typeof value === 'number' ? value : Number(value);
    if (!Number.isFinite(n) || n <= 0) return fallback;
    return Math.trunc(n);
  }

  function ttlMsFromSeconds(value: unknown): number {
    const n = typeof value === 'number' ? value : Number(value);
    if (!Number.isFinite(n) || n <= 0) return 12000;
    return Math.round(n * 1000);
  }

  function normalizeObsConfig(syncSeconds: boolean) {
    if (!config) return;
    config.obs.template = (config.obs.template || 'default').trim() || 'default';
    config.obs.fontScalePct = clampInt(config.obs.fontScalePct, 100, 50, 200);
    config.obs.maxRows = clampInt(config.obs.maxRows, 8, 1, 20);
    config.obs.ttlMs = positiveInt(config.obs.ttlMs, 12000);
    config.obs.bgOpacityPct = clampInt(config.obs.bgOpacityPct, 0, 0, 100);
    config.obs.position = config.obs.position === 'top' ? 'top' : 'bottom';
    config.obs.showPlatform = config.obs.showPlatform !== false;
    if (syncSeconds) obsTtlSeconds = config.obs.ttlMs / 1000;
  }

  function normalizeParticipationConfig() {
    if (!config) return;
    config.participation.max = clampInt(config.participation.max, 0, 0, 4294967295);
  }

  function defaultGoals(): GoalsConfig {
    return { enabled: false, showInApp: false, comments: 0, viewers: 0, likes: 0 };
  }

  function normalizeGoalsConfig() {
    if (!config) return;
    const editable = config as AppConfig & { goals?: Partial<GoalsConfig> };
    if (!editable.goals) editable.goals = defaultGoals();
    editable.goals.enabled = editable.goals.enabled === true;
    editable.goals.showInApp = editable.goals.showInApp === true;
    editable.goals.comments = clampInt(editable.goals.comments, 0, 0, 4294967295);
    editable.goals.viewers = clampInt(editable.goals.viewers, 0, 0, 4294967295);
    editable.goals.likes = clampInt(editable.goals.likes, 0, 0, 4294967295);
  }

  function normalizeWebSpeechSettings() {
    webSpeechRate = clampNumber(webSpeechRate, 1, 0.5, 2);
    webSpeechPitch = clampNumber(webSpeechPitch, 1, 0, 2);
    webSpeechVolume = clampNumber(webSpeechVolume, 1, 0, 1);
    webSpeechVoice = webSpeechVoice.trim();
  }

  function normalizeTtsDictionary(entries: TtsOptions['dictionary']): TtsDictEntry[] {
    return (entries ?? []).map((entry) => ({
      from: typeof entry.from === 'string' ? entry.from : '',
      to: typeof entry.to === 'string' ? entry.to : ''
    }));
  }

  function addTtsDictionaryEntry() {
    ttsDictionary = [...ttsDictionary, { from: '', to: '' }];
  }

  function removeTtsDictionaryEntry(index: number) {
    ttsDictionary = ttsDictionary.filter((_, i) => i !== index);
  }

  function syncTtsDictionaryToConfig() {
    if (!config) return;
    config.tts.options.dictionary = normalizeTtsDictionary(ttsDictionary);
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
    setTtsBool('readName', readName);
    config.tts.options.bouyomiPath = bouyomiPath.trim();
    normalizeWebSpeechSettings();
    setTtsNum('webSpeechRate', webSpeechRate);
    setTtsNum('webSpeechPitch', webSpeechPitch);
    setTtsNum('webSpeechVolume', webSpeechVolume);
    config.tts.options.webSpeechVoice = webSpeechVoice;
    syncTtsDictionaryToConfig();
    config.obs.ttlMs = ttlMsFromSeconds(obsTtlSeconds);
    normalizeObsConfig(false);
    normalizeGoalsConfig();
    normalizeParticipationConfig();
    try {
      await setConfig(config);
      setNotify(config.ui.notifySound, config.ui.notifyVolume);
      onConfigSaved?.(config);
      saveMsg = '保存しました';
    } catch (e) {
      saveMsg = `保存に失敗しました: ${e instanceof Error ? e.message : String(e)}`;
    } finally {
      saving = false;
    }
    if (saveMsgTimer !== null) clearTimeout(saveMsgTimer);
    saveMsgTimer = setTimeout(() => { saveMsg = ''; saveMsgTimer = null; }, 3000);
  }

  function copyText(text: string, markCopied: () => void) {
    navigator.clipboard.writeText(text)
      .then(() => {
        markCopied();
      })
      .catch(() => { /* clipboard denied — do not show success */ });
  }

  function onCopyObs() {
    copyText(obsUrl, () => {
      copiedObs = true;
      if (copiedObsTimer !== null) clearTimeout(copiedObsTimer);
      copiedObsTimer = setTimeout(() => { copiedObs = false; copiedObsTimer = null; }, 1500);
    });
  }

  function onCopyGiftObs() {
    copyText(giftObsUrl, () => {
      copiedGiftObs = true;
      if (copiedGiftObsTimer !== null) clearTimeout(copiedGiftObsTimer);
      copiedGiftObsTimer = setTimeout(() => { copiedGiftObs = false; copiedGiftObsTimer = null; }, 1500);
    });
  }

  function onCopyGoalsObs() {
    copyText(goalsObsUrl, () => {
      copiedGoalsObs = true;
      if (copiedGoalsObsTimer !== null) clearTimeout(copiedGoalsObsTimer);
      copiedGoalsObsTimer = setTimeout(() => { copiedGoalsObs = false; copiedGoalsObsTimer = null; }, 1500);
    });
  }
</script>

<div class="settings">
  <h2>設定</h2>

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

    {#if config.tts.backend === 'bouyomi'}
    <div class="field-row bouyomi-path-row">
      <label for="tts-bouyomi-path">棒読みちゃん.exe パス（自動起動）</label>
      <input
        id="tts-bouyomi-path"
        type="text"
        bind:value={bouyomiPath}
        class="id-input path-input"
      />
    </div>
    <p class="hint">指定すると起動時に未起動なら自動で立ち上げます（空欄なら手動起動）</p>
    {/if}

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

    {#if config.tts.backend === 'webSpeech'}
    <div class="field-row">
      <label for="tts-web-rate">速度</label>
      <input
        id="tts-web-rate"
        type="number"
        min="0.5"
        max="2"
        step="0.1"
        bind:value={webSpeechRate}
        class="num-input"
      />
    </div>
    <div class="field-row">
      <label for="tts-web-pitch">音程</label>
      <input
        id="tts-web-pitch"
        type="number"
        min="0"
        max="2"
        step="0.1"
        bind:value={webSpeechPitch}
        class="num-input"
      />
    </div>
    <div class="field-row">
      <label for="tts-web-volume">音量</label>
      <input
        id="tts-web-volume"
        type="number"
        min="0"
        max="1"
        step="0.1"
        bind:value={webSpeechVolume}
        class="num-input"
      />
    </div>
    <div class="field-row">
      <label for="tts-web-voice">声</label>
      <select id="tts-web-voice" bind:value={webSpeechVoice} class="platform-select voice-select">
        <option value="">ブラウザ既定</option>
        {#each speechVoices as voice (voice.voiceURI)}
          <option value={voice.name}>{voice.name}{voice.lang ? ` (${voice.lang})` : ''}</option>
        {/each}
      </select>
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
      <label for="read-name">ユーザー名も読み上げる</label>
      <input id="read-name" type="checkbox" bind:checked={readName} class="chk" />
      <span class="hint-inline">OFFでコメント本文のみ読み上げ</span>
    </div>
    <div class="field-row">
      <label for="strip-emoji">絵文字を除去</label>
      <input id="strip-emoji" type="checkbox" bind:checked={stripEmoji} class="chk" />
    </div>

    <div class="dict-editor">
      <div class="dict-header">
        <span>読み上げ辞書</span>
        <button type="button" class="copy-btn" onclick={addTtsDictionaryEntry}>行追加</button>
      </div>
      {#if ttsDictionary.length === 0}
        <p class="hint">置換前と置換後を入力すると、読み上げ本文だけに上から順に適用されます。</p>
      {/if}
      {#each ttsDictionary as entry, index (entry)}
        <div class="dict-row">
          <label for={`tts-dict-from-${index}`} class="dict-label">from</label>
          <input
            id={`tts-dict-from-${index}`}
            type="text"
            bind:value={entry.from}
            class="id-input dict-input"
            placeholder="置換前"
          />
          <label for={`tts-dict-to-${index}`} class="dict-label">to</label>
          <input
            id={`tts-dict-to-${index}`}
            type="text"
            bind:value={entry.to}
            class="id-input dict-input"
            placeholder="置換後"
          />
          <button type="button" class="copy-btn dict-delete" onclick={() => removeTtsDictionaryEntry(index)}>削除</button>
        </div>
      {/each}
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
        <option value="simple"></option>
      </datalist>
    </div>
    <div class="field-row">
      <label for="obs-font-scale">フォント倍率</label>
      <input
        id="obs-font-scale"
        type="number"
        min="50"
        max="200"
        step="1"
        bind:value={config.obs.fontScalePct}
        class="num-input"
      />
      <span class="hint-inline">%</span>
    </div>
    <div class="field-row">
      <label for="obs-max-rows">最大表示行</label>
      <input
        id="obs-max-rows"
        type="number"
        min="1"
        max="20"
        step="1"
        bind:value={config.obs.maxRows}
        class="num-input"
      />
    </div>
    <div class="field-row">
      <label for="obs-ttl-seconds">表示時間</label>
      <input
        id="obs-ttl-seconds"
        type="number"
        min="1"
        step="1"
        bind:value={obsTtlSeconds}
        class="num-input"
      />
      <span class="hint-inline">秒</span>
    </div>
    <div class="field-row">
      <label for="obs-bg-opacity">背景不透明度</label>
      <input
        id="obs-bg-opacity"
        type="number"
        min="0"
        max="100"
        step="1"
        bind:value={config.obs.bgOpacityPct}
        class="num-input"
      />
      <span class="hint-inline">%</span>
    </div>
    <div class="field-row">
      <label for="obs-position">表示位置</label>
      <select id="obs-position" bind:value={config.obs.position} class="platform-select">
        <option value="bottom">下</option>
        <option value="top">上</option>
      </select>
    </div>
    <div class="field-row">
      <label for="obs-show-platform">プラットフォーム表示</label>
      <input id="obs-show-platform" type="checkbox" bind:checked={config.obs.showPlatform} class="chk" />
    </div>
    <div class="obs-label">通常オーバーレイURL</div>
    <div class="obs-row">
      <input type="text" value={obsUrl} readonly class="obs-input" />
      <button class="copy-btn" class:copied={copiedObs} onclick={onCopyObs}>
        {copiedObs ? 'コピー済' : 'コピー'}
      </button>
    </div>
    <div class="obs-label">投げ銭専用オーバーレイURL</div>
    <div class="obs-row">
      <input type="text" value={giftObsUrl} readonly class="obs-input" />
      <button class="copy-btn" class:copied={copiedGiftObs} onclick={onCopyGiftObs}>
        {copiedGiftObs ? 'コピー済' : 'コピー'}
      </button>
    </div>
    <p class="hint">OBSのブラウザソースにこのURLを貼り付けてください。</p>
  </section>

  <!-- ── Goals ── -->
  <section id="settings-goals">
    <h3>目標（Goals）</h3>
    <div class="field-row">
      <label for="goals-enabled">目標ゲージを有効化</label>
      <input id="goals-enabled" type="checkbox" bind:checked={config.goals.enabled} class="chk" />
    </div>
    <div class="field-row">
      <label for="goals-show-in-app">アプリ内にも表示</label>
      <input id="goals-show-in-app" type="checkbox" bind:checked={config.goals.showInApp} class="chk" />
    </div>
    <div class="field-row">
      <label for="goals-comments">コメント</label>
      <input
        id="goals-comments"
        type="number"
        min="0"
        max="4294967295"
        step="1"
        bind:value={config.goals.comments}
        class="num-input"
      />
      <span class="hint-inline">（0で非表示）</span>
    </div>
    <div class="field-row">
      <label for="goals-viewers">視聴者</label>
      <input
        id="goals-viewers"
        type="number"
        min="0"
        max="4294967295"
        step="1"
        bind:value={config.goals.viewers}
        class="num-input"
      />
      <span class="hint-inline">（0で非表示）</span>
    </div>
    <div class="field-row">
      <label for="goals-likes">高評価</label>
      <input
        id="goals-likes"
        type="number"
        min="0"
        max="4294967295"
        step="1"
        bind:value={config.goals.likes}
        class="num-input"
      />
      <span class="hint-inline">（0で非表示）</span>
    </div>
    <div class="obs-label">GoalsオーバーレイURL</div>
    <div class="obs-row">
      <input type="text" value={goalsObsUrl} readonly class="obs-input" />
      <button class="copy-btn" class:copied={copiedGoalsObs} onclick={onCopyGoalsObs}>
        {copiedGoalsObs ? 'コピー済' : 'コピー'}
      </button>
    </div>
    <p class="hint">コメント・視聴者・高評価の目標ゲージをOBSに表示します。</p>
  </section>

  <!-- ── Participation ── -->
  <section id="settings-participation">
    <h3>参加型</h3>
    <div class="field-row">
      <label for="participation-enabled">参加管理を有効化</label>
      <input
        id="participation-enabled"
        type="checkbox"
        bind:checked={config.participation.enabled}
        class="chk"
      />
    </div>
    <div class="field-row">
      <label for="participation-keyword">キーワード</label>
      <input
        id="participation-keyword"
        type="text"
        bind:value={config.participation.keyword}
        class="id-input"
        placeholder="参加"
      />
    </div>
    <div class="field-row">
      <label for="participation-max">最大人数</label>
      <input
        id="participation-max"
        type="number"
        min="0"
        max="4294967295"
        step="1"
        bind:value={config.participation.max}
        class="num-input"
      />
      <span class="hint-inline">（0で無制限）</span>
    </div>
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
      <label for="show-donation-panel">投げ銭を別タブで表示</label>
      <input
        id="show-donation-panel"
        type="checkbox"
        bind:checked={config.ui.showDonationPanel}
        class="chk"
      />
    </div>
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

  .platform-select, .id-input, .obs-input, .num-input {
    background: rgba(255,255,255,0.07);
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 4px;
    color: #e0e0e0;
    padding: 5px 8px;
    font-size: 13px;
  }

	  .platform-select { flex-shrink: 0; }
	  .voice-select { flex: 1 1 160px; min-width: 0; }
	  .id-input { flex: 1; }
  .path-input { min-width: min(100%, 260px); }
  .obs-input { flex: 1; font-size: 12px; }
  .num-input { width: 90px; }
  .chk { width: 16px; height: 16px; accent-color: #1976d2; }
  .vol-slider { flex: 1; max-width: 160px; accent-color: #1976d2; }
  .vol-slider:disabled { opacity: 0.4; }

  .copy-btn, .save-btn {
    border: none;
    border-radius: 4px;
    cursor: pointer;
    font-size: 12px;
    padding: 5px 10px;
    font-weight: 600;
    transition: opacity 0.15s;
  }

  .copy-btn { background: #37474f; color: #fff; min-width: 60px; }
  .copy-btn.copied { background: #2e7d32; }
  .save-btn { background: #1976d2; color: #fff; padding: 7px 18px; }
  .save-btn:disabled { opacity: 0.5; cursor: not-allowed; }

  .dict-editor {
    margin-top: 10px;
  }

  .dict-header {
    display: flex;
    align-items: center;
    gap: 10px;
    justify-content: space-between;
    color: #ccc;
    font-size: 13px;
    font-weight: 600;
  }

  .dict-row {
    display: grid;
    grid-template-columns: auto minmax(120px, 1fr) auto minmax(120px, 1fr) auto;
    gap: 6px;
    align-items: center;
    margin-top: 6px;
  }

  .dict-label {
    color: #9e9e9e;
    font-size: 11px;
    font-family: monospace;
  }

  .dict-input {
    min-width: 0;
  }

  .dict-delete {
    background: #4e342e;
  }

  .obs-label {
    margin-top: 8px;
    color: #bdbdbd;
    font-size: 12px;
    font-weight: 600;
  }

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
	    flex-wrap: wrap;
	  }

  .field-row label {
    font-size: 13px;
    color: #ccc;
    min-width: 90px;
  }

  .bouyomi-path-row {
    align-items: flex-start;
    flex-wrap: wrap;
  }

  .bouyomi-path-row label {
    min-width: 210px;
  }

  .hint { font-size: 11px; color: #757575; margin: 4px 0 0; }
  .hint-inline { font-size: 11px; color: #757575; font-weight: 400; text-transform: none; letter-spacing: 0; }

  .save-row {
    display: flex;
    align-items: center;
    gap: 10px;
    padding-top: 12px;
  }

  .save-ok { color: #66bb6a; font-size: 12px; }

  @media (max-width: 720px) {
    .dict-row {
      grid-template-columns: auto minmax(0, 1fr);
    }

    .dict-delete {
      grid-column: 2;
      justify-self: flex-start;
    }
  }
</style>
