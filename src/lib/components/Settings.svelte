<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import type {
    AppConfig, EffectRule, EffectsConfig, GoalsConfig, InjectTestCommentOptions, TtsDictEntry, TtsOptions,
    TimerConfig, WelcomeConfig
  } from '../ipc';
  import {
    getConfig, setConfig, getObsUrl, getObsGoalsUrl, getObsTimerUrl, exportCommentsCsv, injectTestComment,
    setTtsPaused, clearTtsQueue, skipCurrentTts, testTts
  } from '../ipc';
  import { ui, SETTINGS_ANCHOR_IDS } from '../ui.svelte';
  import { buildCsv, setNotify, store } from '../stores.svelte';
  import TemplateEditor from './TemplateEditor.svelte';

  interface Props {
    onConfigSaved?: (config: AppConfig) => void;
  }

  let { onConfigSaved }: Props = $props();

  let config: AppConfig | null = $state(null);
  let obsBaseUrl: string = $state('');
  let obsGoalsBaseUrl: string = $state('');
  let obsTimerBaseUrl: string = $state('');
  let copiedObs: boolean = $state(false);
  let copiedGiftObs: boolean = $state(false);
  let copiedGoalsObs: boolean = $state(false);
  let copiedTimerObs: boolean = $state(false);
  let copiedCsvPath: boolean = $state(false);

  // NG / highlight lists
  let ngWords: string[] = $state([]);
  let ngUsers: string[] = $state([]);
  let highlights: string[] = $state([]);
  let ngWordDraft: string = $state('');
  let ngUserDraft: string = $state('');
  let highlightDraft: string = $state('');

  // Template selection is persisted in config (config.obs.template); SPEC §10
  // mandates config as the single persistence source (no localStorage).

  let saving: boolean = $state(false);
  let saveMsg: string = $state('');
  let exportingCsv: boolean = $state(false);
  let csvExportPath: string = $state('');
  let csvExportMsg: string = $state('');
  let injectingTestComment: boolean = $state(false);
  let testCommentMsg: string = $state('');

  type TestPlatform = InjectTestCommentOptions['platform'];
  type TestKind = NonNullable<InjectTestCommentOptions['kind']>;

  let testPlatform: TestPlatform = $state('youtube');
  let testName: string = $state('テスト太郎');
  let testText: string = $state('配信準備テストです');
  let testKind: TestKind = $state('normal');
  let testAmount: string = $state('500');
  let testCount: number = $state(1);

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
  let copiedTimerObsTimer: ReturnType<typeof setTimeout> | null = null;
  let copiedCsvPathTimer: ReturnType<typeof setTimeout> | null = null;
  let testCommentMsgTimer: ReturnType<typeof setTimeout> | null = null;
  let ttsControlMsgTimer: ReturnType<typeof setTimeout> | null = null;

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
  let bouyomiLaunchElevated: boolean = $state(false);
  let webSpeechRate: number = $state(1);
  let webSpeechPitch: number = $state(1);
  let webSpeechVolume: number = $state(1);
  let webSpeechVoice: string = $state('');
  let ttsDictionary: TtsDictEntry[] = $state([]);
  let ttsPaused: boolean = $state(false);
  let ttsControlBusy: boolean = $state(false);
  let ttsControlMsg: string = $state('');
  let testingTts: boolean = $state(false);
  let testTtsMsg: string = $state('');
  let testTtsOk: boolean | null = $state(null);
  let speechVoices: SpeechSynthesisVoice[] = $state([]);
  let speechVoicesListenerAttached: boolean = false;
  let obsTtlSeconds: number = $state(12);

  onMount(async () => {
    config = await getConfig();
    if (config) {
      ngWords = normalizeModerationEntries(config.moderation.ngWords);
      ngUsers = normalizeModerationEntries(config.moderation.ngUsers);
      highlights = normalizeModerationEntries(config.moderation.highlights);
      voicevoxSpeaker = ttsNum('voicevoxSpeaker', 1);
      maxLength = ttsNum('maxLength', MAX_LENGTH_DEFAULT);
      stripEmoji = ttsBool('stripEmoji', true);
      readName = ttsBool('readName', true);
      bouyomiPath = config.tts.options.bouyomiPath ?? '';
      bouyomiLaunchElevated = ttsBool('bouyomiLaunchElevated', false);
      webSpeechRate = ttsNum('webSpeechRate', 1);
      webSpeechPitch = ttsNum('webSpeechPitch', 1);
      webSpeechVolume = ttsNum('webSpeechVolume', 1);
      webSpeechVoice = config.tts.options.webSpeechVoice ?? '';
      ttsDictionary = normalizeTtsDictionary(config.tts.options.dictionary);
      config.tts.options.dictionary = ttsDictionary;
      normalizeObsConfig(true);
      normalizeGoalsConfig();
      normalizeTimerConfig();
      normalizeEffectsConfig();
      normalizeWelcomeConfig();
      normalizeParticipationConfig();
    }

    const url = await getObsUrl();
    obsBaseUrl = url ?? 'http://127.0.0.1:11180/?template=default';
    obsGoalsBaseUrl = await getObsGoalsUrl();
    obsTimerBaseUrl = await getObsTimerUrl();

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
    if (copiedTimerObsTimer !== null) clearTimeout(copiedTimerObsTimer);
    if (copiedCsvPathTimer !== null) clearTimeout(copiedCsvPathTimer);
    if (testCommentMsgTimer !== null) clearTimeout(testCommentMsgTimer);
    if (ttsControlMsgTimer !== null) clearTimeout(ttsControlMsgTimer);
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

  const timerObsUrl = $derived.by(() => {
    return withTimerParams(obsTimerBaseUrl, config?.obs ?? null);
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

  function withTimerParams(url: string, obs: AppConfig['obs'] | null): string {
    try {
      const u = new URL(url);
      u.searchParams.set('template', 'timer');
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

  function defaultTimer(): TimerConfig {
    return { enabled: false, defaultDurationSec: 300, mode: 'countdown' };
  }

  function normalizeTimerConfig() {
    if (!config) return;
    const editable = config as AppConfig & { timer?: Partial<TimerConfig> };
    if (!editable.timer) editable.timer = defaultTimer();
    editable.timer.enabled = editable.timer.enabled === true;
    editable.timer.defaultDurationSec = clampInt(editable.timer.defaultDurationSec, 300, 1, 4294967295);
    editable.timer.mode = editable.timer.mode === 'elapsed' ? 'elapsed' : 'countdown';
  }

  function timerDefaultMinutes(): number {
    return Math.floor((config?.timer.defaultDurationSec ?? 300) / 60);
  }

  function timerDefaultSeconds(): number {
    return (config?.timer.defaultDurationSec ?? 300) % 60;
  }

  function setTimerDefaultDuration(minutes: number, seconds: number) {
    if (!config) return;
    const mins = clampInt(minutes, 5, 0, Math.floor(4294967295 / 60));
    const secs = clampInt(seconds, 0, 0, 59);
    config.timer.defaultDurationSec = Math.max(1, mins * 60 + secs);
  }

  function onTimerMinutesInput(event: Event) {
    setTimerDefaultDuration(Number((event.currentTarget as HTMLInputElement).value), timerDefaultSeconds());
  }

  function onTimerSecondsInput(event: Event) {
    setTimerDefaultDuration(timerDefaultMinutes(), Number((event.currentTarget as HTMLInputElement).value));
  }

  function defaultEffects(): EffectsConfig {
    return { enabled: false, rules: [] };
  }

  function normalizeEffectsConfig() {
    if (!config) return;
    const editable = config as AppConfig & { effects?: Partial<EffectsConfig> };
    if (!editable.effects) editable.effects = defaultEffects();
    editable.effects.enabled = editable.effects.enabled === true;
    editable.effects.rules = normalizeEffectRules(editable.effects.rules);
  }

  function normalizeEffectRules(rules: Partial<EffectRule>[] | undefined): EffectRule[] {
    return (rules ?? []).map((rule) => ({
      keyword: typeof rule.keyword === 'string' ? rule.keyword : '',
      emoji: typeof rule.emoji === 'string' ? rule.emoji : '',
      count: clampInt(rule.count, 12, 0, 4294967295)
    }));
  }

  function defaultWelcome(): WelcomeConfig {
    return {
      enabled: false,
      greeting: '{name} さん、いらっしゃい！',
      tts: false,
      emoji: '👋',
      count: 8
    };
  }

  function normalizeWelcomeConfig() {
    if (!config) return;
    const editable = config as AppConfig & { welcome?: Partial<WelcomeConfig> };
    if (!editable.welcome) editable.welcome = defaultWelcome();
    editable.welcome.enabled = editable.welcome.enabled === true;
    editable.welcome.greeting =
      typeof editable.welcome.greeting === 'string' && editable.welcome.greeting.trim() !== ''
        ? editable.welcome.greeting
        : '{name} さん、いらっしゃい！';
    editable.welcome.tts = editable.welcome.tts === true;
    editable.welcome.emoji =
      typeof editable.welcome.emoji === 'string' && editable.welcome.emoji.trim() !== ''
        ? editable.welcome.emoji.trim()
        : '👋';
    editable.welcome.count = clampInt(editable.welcome.count, 8, 0, 4294967295);
  }

  function addEffectRule() {
    if (!config) return;
    normalizeEffectsConfig();
    config.effects.rules = [...config.effects.rules, { keyword: '', emoji: '🎉', count: 12 }];
  }

  function removeEffectRule(index: number) {
    if (!config) return;
    config.effects.rules = config.effects.rules.filter((_, i) => i !== index);
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

  function parseModerationText(value: string): string[] {
    return [...new Set(value.split('\n').map((s) => s.trim()).filter(Boolean))];
  }

  function normalizeModerationEntries(entries: string[]): string[] {
    return parseModerationText(entries.join('\n'));
  }

  function isValidRegex(entry: string): boolean {
    try {
      new RegExp(entry);
      return true;
    } catch {
      return false;
    }
  }

  function addNgWord() {
    const entry = ngWordDraft.trim();
    if (!entry || ngWords.includes(entry)) return;
    ngWords = [...ngWords, entry];
    ngWordDraft = '';
  }

  function addNgUser() {
    const entry = ngUserDraft.trim();
    if (!entry || ngUsers.includes(entry)) return;
    ngUsers = [...ngUsers, entry];
    ngUserDraft = '';
  }

  function addHighlight() {
    const entry = highlightDraft.trim();
    if (!entry || highlights.includes(entry)) return;
    highlights = [...highlights, entry];
    highlightDraft = '';
  }

  function removeNgWord(index: number) {
    ngWords = ngWords.filter((_, i) => i !== index);
  }

  function removeNgUser(index: number) {
    ngUsers = ngUsers.filter((_, i) => i !== index);
  }

  function removeHighlight(index: number) {
    highlights = highlights.filter((_, i) => i !== index);
  }

  function addNgWordOnEnter(event: KeyboardEvent) {
    if (event.key !== 'Enter') return;
    event.preventDefault();
    addNgWord();
  }

  function addNgUserOnEnter(event: KeyboardEvent) {
    if (event.key !== 'Enter') return;
    event.preventDefault();
    addNgUser();
  }

  function addHighlightOnEnter(event: KeyboardEvent) {
    if (event.key !== 'Enter') return;
    event.preventDefault();
    addHighlight();
  }

  function syncNgWordsFromTextarea(event: Event) {
    ngWords = parseModerationText((event.currentTarget as HTMLTextAreaElement).value);
  }

  function syncNgUsersFromTextarea(event: Event) {
    ngUsers = parseModerationText((event.currentTarget as HTMLTextAreaElement).value);
  }

  function syncHighlightsFromTextarea(event: Event) {
    highlights = parseModerationText((event.currentTarget as HTMLTextAreaElement).value);
  }

  function fillRandomTestComment() {
    const samples = [
      { name: '初見です', text: '初見です、よろしくお願いします！' },
      { name: 'テスト花子', text: '目標ゲージとエフェクト確認中です' },
      { name: '配信チェック', text: '音声読み上げのテストコメントです' },
      { name: 'ナイス応援', text: 'いい感じ！そのままお願いします' }
    ];
    const index = Math.floor(Math.random() * samples.length);
    testName = samples[index].name;
    testText = samples[index].text;
  }

  function clearTestCommentMsgSoon() {
    if (testCommentMsgTimer !== null) clearTimeout(testCommentMsgTimer);
    testCommentMsgTimer = setTimeout(() => {
      testCommentMsg = '';
      testCommentMsgTimer = null;
    }, 3000);
  }

  async function onInjectTestComment() {
    if (injectingTestComment) return;
    injectingTestComment = true;
    testCommentMsg = '';
    const amountText = testAmount.trim();
    const parsedAmount = amountText === '' ? undefined : Number(amountText);
    const includeAmount =
      (testKind === 'superChat' || testKind === 'bits') &&
      typeof parsedAmount === 'number' &&
      Number.isFinite(parsedAmount);

    try {
      await injectTestComment({
        platform: testPlatform,
        name: testName,
        text: testText,
        kind: testKind,
        amount: includeAmount ? parsedAmount : undefined,
        count: clampInt(testCount, 1, 1, 20)
      });
      testCommentMsg = 'テストコメントを送信しました';
    } catch (e) {
      testCommentMsg = `送信に失敗しました: ${e instanceof Error ? e.message : String(e)}`;
    } finally {
      injectingTestComment = false;
      clearTestCommentMsgSoon();
    }
  }

  function clearTtsControlMsgSoon() {
    if (ttsControlMsgTimer !== null) clearTimeout(ttsControlMsgTimer);
    ttsControlMsgTimer = setTimeout(() => {
      ttsControlMsg = '';
      ttsControlMsgTimer = null;
    }, 3000);
  }

  async function onToggleTtsPaused() {
    if (ttsControlBusy) return;
    const nextPaused = !ttsPaused;
    ttsControlBusy = true;
    ttsControlMsg = '';
    try {
      await setTtsPaused(nextPaused);
      ttsPaused = nextPaused;
      ttsControlMsg = nextPaused ? '読み上げを一時停止しました' : '読み上げを再開しました';
    } catch (e) {
      ttsControlMsg = `操作に失敗しました: ${e instanceof Error ? e.message : String(e)}`;
    } finally {
      ttsControlBusy = false;
      clearTtsControlMsgSoon();
    }
  }

  async function onClearTtsQueue() {
    if (ttsControlBusy) return;
    ttsControlBusy = true;
    ttsControlMsg = '';
    try {
      await clearTtsQueue();
      ttsControlMsg = '読み上げキューを全消ししました';
    } catch (e) {
      ttsControlMsg = `全消しに失敗しました: ${e instanceof Error ? e.message : String(e)}`;
    } finally {
      ttsControlBusy = false;
      clearTtsControlMsgSoon();
    }
  }

  async function onSkipCurrentTts() {
    if (ttsControlBusy) return;
    ttsControlBusy = true;
    ttsControlMsg = '';
    try {
      await skipCurrentTts();
      ttsControlMsg = '現在の読み上げをスキップしました';
    } catch (e) {
      ttsControlMsg = `スキップに失敗しました: ${e instanceof Error ? e.message : String(e)}`;
    } finally {
      ttsControlBusy = false;
      clearTtsControlMsgSoon();
    }
  }

  async function onTestTts() {
    if (testingTts) return;
    testingTts = true;
    testTtsMsg = '';
    testTtsOk = null;
    try {
      testTtsMsg = await testTts();
      testTtsOk = true;
    } catch (e) {
      testTtsMsg = e instanceof Error ? e.message : String(e);
      testTtsOk = false;
    } finally {
      testingTts = false;
    }
  }

  async function onSave() {
    if (!config) return;
    saving = true;
    saveMsg = '';
    config.moderation.ngWords = [...ngWords];
    config.moderation.ngUsers = [...ngUsers];
    config.moderation.highlights = [...highlights];
    setTtsNum('voicevoxSpeaker', Number.isFinite(voicevoxSpeaker) ? Math.trunc(voicevoxSpeaker) : 1);
    setTtsNum('maxLength', Number.isFinite(maxLength) && maxLength >= 0 ? Math.trunc(maxLength) : MAX_LENGTH_DEFAULT);
    setTtsBool('stripEmoji', stripEmoji);
    setTtsBool('readName', readName);
    config.tts.options.bouyomiPath = bouyomiPath.trim();
    setTtsBool('bouyomiLaunchElevated', bouyomiLaunchElevated);
    normalizeWebSpeechSettings();
    setTtsNum('webSpeechRate', webSpeechRate);
    setTtsNum('webSpeechPitch', webSpeechPitch);
    setTtsNum('webSpeechVolume', webSpeechVolume);
    config.tts.options.webSpeechVoice = webSpeechVoice;
    syncTtsDictionaryToConfig();
    config.obs.ttlMs = ttlMsFromSeconds(obsTtlSeconds);
    normalizeObsConfig(false);
    normalizeGoalsConfig();
    normalizeTimerConfig();
    normalizeEffectsConfig();
    normalizeWelcomeConfig();
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

  function onCopyTimerObs() {
    copyText(timerObsUrl, () => {
      copiedTimerObs = true;
      if (copiedTimerObsTimer !== null) clearTimeout(copiedTimerObsTimer);
      copiedTimerObsTimer = setTimeout(() => { copiedTimerObs = false; copiedTimerObsTimer = null; }, 1500);
    });
  }

  async function onExportCommentsCsv() {
    if (store.totalCount === 0 || exportingCsv) return;
    exportingCsv = true;
    csvExportMsg = '';
    csvExportPath = '';
    try {
      const path = await exportCommentsCsv(buildCsv());
      if (path) {
        csvExportPath = path;
        csvExportMsg = 'CSVを出力しました';
      } else {
        csvExportMsg = 'Tauri環境でのみCSV出力できます';
      }
    } catch (e) {
      csvExportMsg = `CSV出力に失敗しました: ${e instanceof Error ? e.message : String(e)}`;
    } finally {
      exportingCsv = false;
    }
  }

  function onCopyCsvPath() {
    if (!csvExportPath) return;
    copyText(csvExportPath, () => {
      copiedCsvPath = true;
      if (copiedCsvPathTimer !== null) clearTimeout(copiedCsvPathTimer);
      copiedCsvPathTimer = setTimeout(() => { copiedCsvPath = false; copiedCsvPathTimer = null; }, 1500);
    });
  }
</script>

<div class="settings">
  <h2>設定</h2>

  <!-- ── TTS ── -->
  {#if config}
  <section id="settings-test">
    <h3>テスト</h3>
    <div class="field-row">
      <label for="test-platform">プラットフォーム</label>
      <select id="test-platform" bind:value={testPlatform} class="platform-select">
        <option value="twitch">Twitch</option>
        <option value="youtube">YouTube</option>
      </select>
      <button type="button" class="copy-btn" onclick={fillRandomTestComment}>ランダム</button>
    </div>
    <div class="field-row">
      <label for="test-name">名前</label>
      <input
        id="test-name"
        type="text"
        bind:value={testName}
        class="id-input"
        placeholder="テスト太郎"
      />
    </div>
    <div class="field-row">
      <label for="test-text">本文</label>
      <input
        id="test-text"
        type="text"
        bind:value={testText}
        class="id-input"
        placeholder="テストコメントです"
      />
    </div>
    <div class="field-row">
      <label for="test-kind">種類</label>
      <select id="test-kind" bind:value={testKind} class="platform-select">
        <option value="normal">通常</option>
        <option value="superChat">Super Chat</option>
        <option value="membership">メンバーシップ</option>
        <option value="bits">Bits</option>
      </select>
      <label for="test-amount" class="compact-label">金額</label>
      <input
        id="test-amount"
        type="number"
        min="0"
        step="1"
        bind:value={testAmount}
        class="num-input"
        disabled={testKind !== 'superChat' && testKind !== 'bits'}
      />
      <label for="test-count" class="compact-label">連投数</label>
      <input
        id="test-count"
        type="number"
        min="1"
        max="20"
        step="1"
        bind:value={testCount}
        class="num-input"
      />
    </div>
    <div class="field-row">
      <button class="export-btn" onclick={onInjectTestComment} disabled={injectingTestComment}>
        {injectingTestComment ? '送信中...' : 'テストコメント送信'}
      </button>
      {#if testCommentMsg}<span class="hint-inline">{testCommentMsg}</span>{/if}
    </div>
  </section>

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

    <div class="tts-control-panel">
      <div class="field-row tts-control-row">
        <button
          type="button"
          class="export-btn"
          class:tts-paused={ttsPaused}
          onclick={onToggleTtsPaused}
          disabled={ttsControlBusy}
        >
          {ttsPaused ? '再開' : '一時停止'}
        </button>
        <button type="button" class="copy-btn" onclick={onClearTtsQueue} disabled={ttsControlBusy}>
          キュー全消し
        </button>
        <button type="button" class="copy-btn" onclick={onSkipCurrentTts} disabled={ttsControlBusy}>
          今のをスキップ
        </button>
        {#if ttsControlMsg}<span class="hint-inline">{ttsControlMsg}</span>{/if}
      </div>
      <p class="hint">
        一時停止中に届いたコメントは読み上げず破棄します。キュー全消し/スキップは未送出キューとWeb Speechの現在発話を止めます。棒読みちゃん/VOICEVOXは送信済みの1件をアプリ側からキャンセルできません。
      </p>
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
    <div class="field-row">
      <label for="tts-bouyomi-elevated">管理者として起動する</label>
      <input
        id="tts-bouyomi-elevated"
        type="checkbox"
        bind:checked={bouyomiLaunchElevated}
        class="chk"
      />
      <span class="hint-inline">UAC確認が出ます</span>
    </div>
    <p class="hint">環境によっては棒読みちゃんの自動起動に管理者権限が必要です。</p>
    <div class="field-row">
      <button type="button" class="export-btn" onclick={onTestTts} disabled={testingTts}>
        {testingTts ? 'テスト中...' : 'テスト読み上げ'}
      </button>
    </div>
    {#if testTtsMsg}
      <p
        class="tts-test-result"
        class:tts-test-result--ok={testTtsOk === true}
        class:tts-test-result--error={testTtsOk === false}
      >
        {testTtsMsg}
      </p>
    {/if}
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
    <div id="settings-obs-template-editor" class="template-editor-wrap">
      <h3>OBSテンプレート編集</h3>
      <TemplateEditor obsPort={config.obs.port} currentTemplate={config.obs.template} />
    </div>
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

  <!-- ── Timer ── -->
  <section id="settings-timer">
    <h3>タイマー</h3>
    <div class="field-row">
      <label for="timer-enabled">タイマーを有効化</label>
      <input id="timer-enabled" type="checkbox" bind:checked={config.timer.enabled} class="chk" />
    </div>
    <div class="field-row">
      <label for="timer-default-minutes">既定時間</label>
      <input
        id="timer-default-minutes"
        type="number"
        min="0"
        step="1"
        value={timerDefaultMinutes()}
        class="num-input"
        oninput={onTimerMinutesInput}
      />
      <span class="hint-inline">分</span>
      <input
        id="timer-default-seconds"
        type="number"
        min="0"
        max="59"
        step="1"
        value={timerDefaultSeconds()}
        class="num-input"
        oninput={onTimerSecondsInput}
      />
      <span class="hint-inline">秒</span>
    </div>
    <div class="field-row">
      <label for="timer-mode">表示モード</label>
      <select id="timer-mode" bind:value={config.timer.mode} class="platform-select">
        <option value="countdown">カウントダウン</option>
        <option value="elapsed">経過時間</option>
      </select>
    </div>
    <div class="obs-label">TimerオーバーレイURL</div>
    <div class="obs-row">
      <input type="text" value={timerObsUrl} readonly class="obs-input" />
      <button class="copy-btn" class:copied={copiedTimerObs} onclick={onCopyTimerObs}>
        {copiedTimerObs ? 'コピー済' : 'コピー'}
      </button>
    </div>
    <p class="hint">タイマー/カウントダウンをOBSに表示します。</p>
  </section>

  <!-- ── Effects ── -->
  <section id="settings-effects">
    <h3>コメントエフェクト</h3>
    <div class="field-row">
      <label for="effects-enabled">有効化</label>
      <input id="effects-enabled" type="checkbox" bind:checked={config.effects.enabled} class="chk" />
    </div>

    <div class="dict-editor">
      <div class="dict-header">
        <span>キーワードルール</span>
        <button type="button" class="copy-btn" onclick={addEffectRule}>行追加</button>
      </div>
      {#if config.effects.rules.length === 0}
        <p class="hint">コメント本文にキーワードが含まれると、指定した文字列をアプリ内に表示します。</p>
      {/if}
      {#each config.effects.rules as rule, index (rule)}
        <div class="effect-row">
          <label for={`effect-keyword-${index}`} class="dict-label">keyword</label>
          <input
            id={`effect-keyword-${index}`}
            type="text"
            bind:value={rule.keyword}
            class="id-input dict-input"
            placeholder="キーワード"
          />
          <label for={`effect-emoji-${index}`} class="dict-label">emoji</label>
          <input
            id={`effect-emoji-${index}`}
            type="text"
            bind:value={rule.emoji}
            class="id-input effect-emoji-input"
            placeholder="🎉"
          />
          <label for={`effect-count-${index}`} class="dict-label">count</label>
          <input
            id={`effect-count-${index}`}
            type="number"
            min="0"
            max="4294967295"
            step="1"
            bind:value={rule.count}
            class="num-input"
          />
          <button type="button" class="copy-btn dict-delete" onclick={() => removeEffectRule(index)}>削除</button>
        </div>
      {/each}
    </div>
  </section>

  <!-- ── Welcome ── -->
  <section id="settings-welcome">
    <h3>初見歓迎</h3>
    <div class="field-row">
      <label for="welcome-enabled">有効化</label>
      <input id="welcome-enabled" type="checkbox" bind:checked={config.welcome.enabled} class="chk" />
    </div>
    <div class="field-row">
      <label for="welcome-greeting">挨拶テンプレ</label>
      <input
        id="welcome-greeting"
        type="text"
        bind:value={config.welcome.greeting}
        class="id-input"
        placeholder="{name} さん、いらっしゃい！"
      />
      <span class="hint-inline">{'{name}'} を名前に置換</span>
    </div>
    <div class="field-row">
      <label for="welcome-tts">挨拶を読み上げ</label>
      <input id="welcome-tts" type="checkbox" bind:checked={config.welcome.tts} class="chk" />
    </div>
    <div class="field-row">
      <label for="welcome-emoji">emoji</label>
      <input
        id="welcome-emoji"
        type="text"
        bind:value={config.welcome.emoji}
        class="id-input welcome-emoji-input"
        placeholder="👋"
      />
      <label for="welcome-count" class="compact-label">count</label>
      <input
        id="welcome-count"
        type="number"
        min="0"
        max="4294967295"
        step="1"
        bind:value={config.welcome.count}
        class="num-input"
      />
    </div>
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
    <h3>モデレーション</h3>

    <div class="mod-list">
      <div class="mod-list-header">
        <span>NGワード（{ngWords.length}）</span>
        <span class="hint-inline">正規表現可</span>
      </div>
      {#if ngWords.length === 0}
        <p class="mod-empty">未登録</p>
      {/if}
      {#each ngWords as entry, index (entry)}
        <div class="mod-entry-row">
          <span class="mod-entry-text">{entry}</span>
          {#if !isValidRegex(entry)}
            <span class="mod-regex-warning" title="正規表現として無効" aria-label="正規表現として無効" role="img">⚠</span>
          {/if}
          <button type="button" class="copy-btn mod-delete" aria-label={`NGワード「${entry}」を削除`} onclick={() => removeNgWord(index)}>削除</button>
        </div>
      {/each}
      <div class="mod-add-row">
        <input
          type="text"
          bind:value={ngWordDraft}
          class="id-input mod-add-input"
          aria-label="NGワードを追加"
          placeholder="NG word"
          onkeydown={addNgWordOnEnter}
        />
        <button type="button" class="export-btn" onclick={addNgWord}>追加</button>
      </div>
    </div>

    <div class="mod-list">
      <div class="mod-list-header">
        <span>NGユーザー（{ngUsers.length}）</span>
        <span class="hint-inline">1行1ID</span>
      </div>
      {#if ngUsers.length === 0}
        <p class="mod-empty">未登録</p>
      {/if}
      {#each ngUsers as entry, index (entry)}
        <div class="mod-entry-row">
          <span class="mod-entry-text">{entry}</span>
          {#if !isValidRegex(entry)}
            <span class="mod-regex-warning" title="正規表現として無効" aria-label="正規表現として無効" role="img">⚠</span>
          {/if}
          <button type="button" class="copy-btn mod-delete" aria-label={`NGユーザー「${entry}」を削除`} onclick={() => removeNgUser(index)}>削除</button>
        </div>
      {/each}
      <div class="mod-add-row">
        <input
          type="text"
          bind:value={ngUserDraft}
          class="id-input mod-add-input"
          aria-label="NGユーザーを追加"
          placeholder="username123"
          onkeydown={addNgUserOnEnter}
        />
        <button type="button" class="export-btn" onclick={addNgUser}>追加</button>
      </div>
    </div>

    <div class="mod-list">
      <div class="mod-list-header">
        <span>ハイライト（{highlights.length}）</span>
        <span class="hint-inline">ユーザー名またはキーワード</span>
      </div>
      {#if highlights.length === 0}
        <p class="mod-empty">未登録</p>
      {/if}
      {#each highlights as entry, index (entry)}
        <div class="mod-entry-row">
          <span class="mod-entry-text">{entry}</span>
          {#if !isValidRegex(entry)}
            <span class="mod-regex-warning" title="正規表現として無効" aria-label="正規表現として無効" role="img">⚠</span>
          {/if}
          <button type="button" class="copy-btn mod-delete" aria-label={`ハイライト「${entry}」を削除`} onclick={() => removeHighlight(index)}>削除</button>
        </div>
      {/each}
      <div class="mod-add-row">
        <input
          type="text"
          bind:value={highlightDraft}
          class="id-input mod-add-input"
          aria-label="ハイライトを追加"
          placeholder="!command"
          onkeydown={addHighlightOnEnter}
        />
        <button type="button" class="export-btn" onclick={addHighlight}>追加</button>
      </div>
    </div>

    <details class="mod-bulk-editor">
      <summary>一括編集（上級者向け）</summary>

      <h3>NGワード <span class="hint-inline">（1行1語、正規表現可）</span></h3>
      <textarea
        value={ngWords.join('\n')}
        rows={4}
        class="mod-area"
        placeholder="NG word&#10;bad_word"
        aria-label="NGワードを一括編集"
        onblur={syncNgWordsFromTextarea}
      ></textarea>

      <h3>NGユーザー <span class="hint-inline">（1行1ID）</span></h3>
      <textarea
        value={ngUsers.join('\n')}
        rows={3}
        class="mod-area"
        placeholder="username123"
        aria-label="NGユーザーを一括編集"
        onblur={syncNgUsersFromTextarea}
      ></textarea>

      <h3>ハイライト <span class="hint-inline">（ユーザー名またはキーワード）</span></h3>
      <textarea
        value={highlights.join('\n')}
        rows={3}
        class="mod-area"
        placeholder="!command&#10;favorite_user"
        aria-label="ハイライトを一括編集"
        onblur={syncHighlightsFromTextarea}
      ></textarea>
    </details>
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

  <!-- ── Export ── -->
  <section id="settings-export">
    <h3>エクスポート</h3>
    <div class="field-row">
      <button class="export-btn" onclick={onExportCommentsCsv} disabled={exportingCsv || store.totalCount === 0}>
        {exportingCsv ? 'CSV出力中...' : 'コメントログをCSV出力'}
      </button>
      <span class="hint-inline">保持中 {store.totalCount} 件</span>
    </div>
    {#if csvExportMsg}<p class="hint">{csvExportMsg}</p>{/if}
    {#if csvExportPath}
      <div class="obs-row csv-path-row">
        <input type="text" value={csvExportPath} readonly class="obs-input" />
        <button class="copy-btn" class:copied={copiedCsvPath} onclick={onCopyCsvPath}>
          {copiedCsvPath ? 'コピー済' : 'コピー'}
        </button>
      </div>
    {/if}
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

  .copy-btn, .save-btn, .export-btn {
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
  .export-btn { background: #1976d2; color: #fff; padding: 7px 14px; }
  .save-btn:disabled, .export-btn:disabled, .copy-btn:disabled { opacity: 0.5; cursor: not-allowed; }
  .tts-paused { background: #2e7d32; }

  .tts-control-panel {
    margin-top: 8px;
  }

  .tts-control-row {
    align-items: flex-start;
  }

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

  .effect-row {
    display: grid;
    grid-template-columns: auto minmax(120px, 1fr) auto minmax(70px, 120px) auto 90px auto;
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

  .effect-emoji-input {
    min-width: 0;
  }

  .welcome-emoji-input {
    flex: 0 1 120px;
    min-width: 70px;
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

  .csv-path-row {
    margin-top: 6px;
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

  .mod-list {
    margin-top: 10px;
  }

  .mod-list-header {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 10px;
    color: #ccc;
    font-size: 13px;
    font-weight: 600;
  }

  .mod-empty {
    color: #757575;
    font-size: 12px;
    margin: 6px 0 0;
  }

  .mod-entry-row {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto auto;
    gap: 6px;
    align-items: center;
    margin-top: 6px;
  }

  .mod-entry-text {
    min-width: 0;
    overflow-wrap: anywhere;
    color: #e0e0e0;
    font-family: monospace;
    font-size: 12px;
  }

  .mod-regex-warning {
    color: #ffca28;
    font-size: 13px;
    line-height: 1;
  }

  .mod-delete {
    background: #4e342e;
  }

  .mod-add-row {
    display: flex;
    gap: 6px;
    align-items: center;
    margin-top: 6px;
  }

  .mod-add-input {
    min-width: 0;
  }

  .mod-bulk-editor {
    margin-top: 12px;
  }

  .mod-bulk-editor summary {
    color: #bdbdbd;
    cursor: pointer;
    font-size: 12px;
    font-weight: 600;
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

  .field-row label.compact-label {
    min-width: auto;
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
  .tts-test-result { font-size: 12px; margin: 4px 0 0; }
  .tts-test-result--ok { color: #81c784; }
  .tts-test-result--error { color: #ef9a9a; }

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

    .effect-row {
      grid-template-columns: auto minmax(0, 1fr);
    }

    .dict-delete {
      grid-column: 2;
      justify-self: flex-start;
    }
  }
</style>
