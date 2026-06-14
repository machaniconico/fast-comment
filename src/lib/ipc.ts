/**
 * IPC layer between Tauri backend and Svelte UI.
 *
 * - listen('chat', ...) receives batched ChatMessage arrays from Rust.
 * - rAF batching: incoming messages are queued and flushed on the next
 *   animation frame to avoid blocking the main thread on high-frequency input.
 * - Safe to import in a plain browser (Tauri absent): all calls are no-ops.
 */

import type { ChatMessage } from './types';
import { getCurrentWindow } from '@tauri-apps/api/window';

// ---- Tauri availability guard ----
// @tauri-apps/api throws when window.__TAURI_INTERNALS__ is absent (browser dev).
const isTauri = (): boolean =>
  typeof window !== 'undefined' && !!(window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;

// ---- rAF batch queue ----
type BatchHandler = (messages: ChatMessage[]) => void;

let _batchHandler: BatchHandler | null = null;
let _pending: ChatMessage[] = [];
let _rafId: number | null = null;

function scheduleFlusher() {
  if (_rafId !== null) return;
  _rafId = requestAnimationFrame(() => {
    _rafId = null;
    if (_pending.length === 0 || !_batchHandler) return;
    const batch = _pending;
    _pending = [];
    _batchHandler(batch);
  });
}

/**
 * Register the handler that receives flushed batches.
 * Called once from stores.ts initialisation.
 */
export function onChatBatch(handler: BatchHandler): void {
  _batchHandler = handler;
}

/**
 * Start listening to the 'chat' event from Tauri.
 * Returns an unlisten function; call it on component destroy.
 */
export async function startChatListener(): Promise<() => void> {
  if (!isTauri()) {
    console.info('[ipc] Tauri not detected — running in browser-only mode');
    return () => {};
  }
  const { listen } = await import('@tauri-apps/api/event');
  const unlisten = await listen<ChatMessage[]>('chat', (event) => {
    _pending.push(...event.payload);
    scheduleFlusher();
  });
  return unlisten;
}

/**
 * Start listening to the 'tts-speak' event from Tauri and speak the payload
 * via the browser SpeechSynthesis API (WebSpeech backend).
 * Returns an unlisten function; call it on component destroy.
 * No-op when Tauri or speechSynthesis is unavailable.
 */
export async function startTtsSpeakListener(): Promise<() => void> {
  if (!isTauri()) return () => {};
  if (typeof window === 'undefined' || !('speechSynthesis' in window)) return () => {};
  const { listen } = await import('@tauri-apps/api/event');
  const unlisten = await listen<TtsSpeakPayload | string>('tts-speak', (event) => {
    const payload = typeof event.payload === 'string'
      ? { text: event.payload, rate: 1, pitch: 1, volume: 1, voice: '' }
      : event.payload;
    const text = payload.text;
    if (!text) return;
    try {
      const utterance = new SpeechSynthesisUtterance(text);
      utterance.rate = boundedNumber(payload.rate, 1, 0.5, 2);
      utterance.pitch = boundedNumber(payload.pitch, 1, 0, 2);
      utterance.volume = boundedNumber(payload.volume, 1, 0, 1);
      if (payload.voice) {
        const voice = window.speechSynthesis.getVoices().find((v) => v.name === payload.voice);
        if (voice) utterance.voice = voice;
      }
      window.speechSynthesis.speak(utterance);
    } catch (e) {
      console.warn('[ipc] speechSynthesis.speak failed', e);
    }
  });
  return unlisten;
}

/**
 * Start listening to the 'tts-cancel' event from Tauri and stop WebSpeech.
 * Returns an unlisten function; call it on component destroy.
 */
export async function startTtsCancelListener(): Promise<() => void> {
  if (!isTauri()) return () => {};
  if (typeof window === 'undefined' || !('speechSynthesis' in window)) return () => {};
  const { listen } = await import('@tauri-apps/api/event');
  const unlisten = await listen('tts-cancel', () => {
    try {
      window.speechSynthesis.cancel();
    } catch (e) {
      console.warn('[ipc] speechSynthesis.cancel failed', e);
    }
  });
  return unlisten;
}

export interface TtsNotice {
  level: 'warn' | string;
  message: string;
}

export interface TtsQueueItem {
  id: string;
  preview: string;
}

export interface TtsQueueState {
  depth: number;
  paused: boolean;
  items: TtsQueueItem[];
}

export async function onTtsNotice(cb: (notice: TtsNotice) => void): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import('@tauri-apps/api/event');
  const unlisten = await listen<TtsNotice>('tts-notice', (event) => {
    cb(event.payload);
  });
  return unlisten;
}

export async function onTtsQueueState(cb: (state: TtsQueueState) => void): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import('@tauri-apps/api/event');
  const unlisten = await listen<TtsQueueState>('tts-queue-state', (event) => {
    cb(event.payload);
  });
  return unlisten;
}

/**
 * Listen to the 'stats' event from Tauri (StatsSnapshot, emitted ~1/s).
 * Returns an unlisten function; no-op in browser-only mode.
 */
export async function onStats(cb: (snapshot: StatsSnapshot) => void): Promise<() => void> {
  if (!isTauri()) return () => {};
  const { listen } = await import('@tauri-apps/api/event');
  const unlisten = await listen<StatsSnapshot>('stats', (event) => {
    cb(event.payload);
  });
  return unlisten;
}

export async function setAlwaysOnTop(value: boolean): Promise<void> {
  if (!isTauri()) return;
  try {
    await getCurrentWindow().setAlwaysOnTop(value);
  } catch (e) {
    console.warn('[ipc] setAlwaysOnTop failed', e);
  }
}

interface TtsSpeakPayload {
  text: string;
  rate: number;
  pitch: number;
  volume: number;
  voice: string;
}

function boundedNumber(value: unknown, fallback: number, min: number, max: number): number {
  if (typeof value !== 'number' || !Number.isFinite(value)) return fallback;
  return Math.min(max, Math.max(min, value));
}

// ---- Tauri invoke helpers ----

async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T | null> {
  if (!isTauri()) return null;
  const { invoke: tauriInvoke } = await import('@tauri-apps/api/core');
  return tauriInvoke<T>(cmd, args);
}

// Config

// Mirror of Rust `TtsOptions` (src-tauri/src/config.rs), serde camelCase.
// All fields optional so partially-written configs deserialize cleanly and the
// UI can read individual keys without asserting the whole object is present.
export interface TtsDictEntry {
  from: string;
  to: string;
}

export interface TtsOptions {
  bouyomiHost?: string;
  bouyomiPort?: number;
  bouyomiSpeed?: number;
  bouyomiVolume?: number;
  bouyomiTone?: number;
  bouyomiVoice?: number;
  bouyomiPath?: string;
  bouyomiLaunchElevated?: boolean;
  voicevoxUrl?: string;
  voicevoxSpeaker?: number;
  webSpeechRate?: number;
  webSpeechPitch?: number;
  webSpeechVolume?: number;
  webSpeechVoice?: string;
  readName?: boolean;
  omitUrl?: boolean;
  stripEmoji?: boolean;
  maxLength?: number;
  dictionary?: TtsDictEntry[];
}

export interface AppConfig {
  channels: ChannelConfig[];
  obs: {
    port: number;
    template: string;
    fontScalePct: number;
    maxRows: number;
    ttlMs: number;
    bgOpacityPct: number;
    position: string;
    showPlatform: boolean;
  };
  goals: GoalsConfig;
  timer: TimerConfig;
  effects: EffectsConfig;
  welcome: WelcomeConfig;
  tts: { backend: 'bouyomi' | 'voicevox' | 'webSpeech' | 'none'; options: TtsOptions };
  moderation: { ngWords: string[]; ngUsers: string[]; highlights: string[] };
  ui: { maxBuffer: number; showDonationPanel: boolean; notifySound: boolean; notifyVolume: number };
  participation: ParticipationConfig;
  youtubeOverrides?: { apiKey?: string; clientVersion?: string; paths?: Record<string, string> };
  // Credentials for self-posting to chat (Rust `CredentialsConfig`, serde camelCase).
  // Optional so older config.json without the field still deserializes cleanly.
  credentials?: { twitchOauth?: string; twitchUsername?: string };
}

export interface GoalsConfig {
  enabled: boolean;
  showInApp: boolean;
  comments: number;
  viewers: number;
  likes: number;
}

export interface TimerConfig {
  enabled: boolean;
  defaultDurationSec: number;
  mode: string;
}

export interface EffectRule {
  keyword: string;
  emoji: string;
  count: number;
}

export interface EffectsConfig {
  enabled: boolean;
  rules: EffectRule[];
}

export interface WelcomeConfig {
  enabled: boolean;
  greeting: string;
  tts: boolean;
  emoji: string;
  count: number;
}

export interface GoalsSnapshot {
  comments: number;
  viewers: number;
  likes: number;
}

export interface ChannelTitle {
  platform: string;
  identifier: string;
  title: string;
}

export interface ChannelStatus {
  platform: string;
  identifier: string;
  title?: string | null;
  viewers?: number | null;
  live?: boolean | null;
}

export interface StatsSnapshot {
  comments: number;
  viewers: number;
  viewersMax: number;
  likes: number;
  likesAvailable: boolean;
  goals: GoalsSnapshot;
  updatedAt: number;
  // Per-channel stream titles (currently YouTube only). Optional for backward
  // compatibility with older snapshots that don't carry it.
  channelTitles?: ChannelTitle[];
  channelStatus?: ChannelStatus[];
}

export interface TimerSnapshot {
  state: string;
  mode: string;
  durationSec: number;
  baseElapsedSec: number;
  runningSinceMs: number;
  updatedAt: number;
}

export interface ParticipationConfig {
  enabled: boolean;
  keyword: string;
  max: number;
}

export interface Participant {
  platform: string;
  userId: string;
  name: string;
  picked: boolean;
}

export interface ChannelConfig {
  platform: 'twitch' | 'youtube';
  identifier: string; // Twitch: channel name, YouTube: videoId
  enabled: boolean; // Rust ChannelConfig.enabled (serde default true)
}

export interface InjectTestCommentOptions {
  platform: 'twitch' | 'youtube';
  name: string;
  text: string;
  kind?: 'normal' | 'superChat' | 'membership' | 'bits';
  amount?: number;
  count?: number;
}

export interface UpdateStatus {
  updateAvailable: boolean;
  currentVersion: string;
  latestVersion: string;
  releaseUrl: string;
}

export async function getConfig(): Promise<AppConfig | null> {
  return invoke<AppConfig>('get_config');
}

export async function exportCommentsCsv(csv: string): Promise<string | null> {
  return invoke<string>('export_comments_csv', { csv });
}

export async function setConfig(config: AppConfig): Promise<void> {
  // Rust command is `update_config(new_config)`; Tauri maps the snake_case
  // param `new_config` to the JS key `newConfig`.
  await invoke<void>('update_config', { newConfig: config });
}

export async function setTtsPaused(paused: boolean): Promise<void> {
  await invoke<void>('set_tts_paused', { paused });
}

export async function getTtsQueueState(): Promise<TtsQueueState | null> {
  return invoke<TtsQueueState>('get_tts_queue_state');
}

export async function clearTtsQueue(): Promise<void> {
  await invoke<void>('clear_tts_queue');
}

export async function skipCurrentTts(): Promise<void> {
  await invoke<void>('skip_current_tts');
}

export async function ttsSpeakText(text: string): Promise<void> {
  await invoke<void>('tts_speak_text', { text });
}

export async function testTts(): Promise<string> {
  return (await invoke<string>('test_tts')) ?? 'Tauri環境でのみテストできます';
}

export async function addChannel(channel: ChannelConfig): Promise<void> {
  await invoke<void>('add_channel', { channel });
}

export async function removeChannel(key: string): Promise<void> {
  await invoke<void>('remove_channel', { key });
}

export async function hideMessage(id: string): Promise<void> {
  await invoke<void>('hide_message', { id });
}

export async function getObsUrl(): Promise<string | null> {
  return invoke<string>('get_obs_url');
}

export async function getObsGoalsUrl(): Promise<string> {
  return (await invoke<string>('get_obs_goals_url')) ?? 'http://127.0.0.1:11180/?template=goals&ws=ws://127.0.0.1:11180/stats';
}

export async function getObsTimerUrl(): Promise<string> {
  return (await invoke<string>('get_obs_timer_url')) ?? 'http://127.0.0.1:11180/?template=timer&ws=ws://127.0.0.1:11180/timer';
}

export async function controlTimer(action: string, durationSec?: number): Promise<void> {
  await invoke<void>('control_timer', { action, durationSec });
}

export async function listTemplates(): Promise<string[]> {
  return (await invoke<string[]>('list_templates')) ?? [];
}

export async function readTemplateFile(name: string, file: string): Promise<string> {
  return (await invoke<string>('read_template_file', { name, file })) ?? '';
}

export async function writeTemplateFile(name: string, file: string, contents: string): Promise<void> {
  await invoke<void>('write_template_file', { name, file, contents });
}

export async function getParticipants(): Promise<Participant[] | null> {
  return invoke<Participant[]>('get_participants');
}

export async function pickNextParticipant(): Promise<Participant | null> {
  return invoke<Participant>('pick_next_participant');
}

export async function pickRandomParticipant(): Promise<Participant | null> {
  return invoke<Participant>('pick_random_participant');
}

export async function removeParticipant(platform: string, userId: string): Promise<void> {
  await invoke<void>('remove_participant', { platform, userId });
}

export async function clearParticipants(): Promise<void> {
  await invoke<void>('clear_participants');
}

export async function injectTestComment(opts: InjectTestCommentOptions): Promise<void> {
  await invoke<void>('inject_test_comment', { ...opts });
}

/**
 * Post a message to the live chat as the configured account.
 * Twitch sends via an authenticated one-shot IRC connection (Rust side).
 * YouTube is not yet supported (returns an error from the backend).
 * No-op in browser-only mode (Tauri absent).
 */
export async function sendChatMessage(
  platform: 'twitch' | 'youtube',
  channel: string,
  text: string,
): Promise<void> {
  await invoke<void>('send_chat_message', { platform, channel, text });
}

export async function checkForUpdate(): Promise<UpdateStatus | null> {
  return invoke<UpdateStatus>('check_for_update');
}

export async function openReleaseUrl(url: string): Promise<void> {
  if (!url) return;
  await invoke<void>('open_url', { url });
}

// ── Danmaku overlay window (透明・枠なし・クリック透過・最前面) ────────────────
// ニコ生風にコメントを画面へ流す別ウィンドウ。?window=danmaku で開き、
// main.ts が DanmakuOverlay だけを mount する。Rust の app.emit("chat") は
// 全ウィンドウ配信なので、このウィンドウも追加配線なしでコメントを受信できる。
const DANMAKU_LABEL = 'danmaku';

export async function isDanmakuOverlayOpen(): Promise<boolean> {
  if (!isTauri()) return false;
  const { WebviewWindow } = await import('@tauri-apps/api/webviewWindow');
  return (await WebviewWindow.getByLabel(DANMAKU_LABEL)) !== null;
}

export async function openDanmakuOverlay(): Promise<void> {
  if (!isTauri()) return;
  const { WebviewWindow } = await import('@tauri-apps/api/webviewWindow');
  if (await WebviewWindow.getByLabel(DANMAKU_LABEL)) return; // 既に開いている

  // プライマリモニタ全面を覆う(取得失敗時は 1920x1080 / 原点で代替)。
  // WebviewWindow の width/height/x/y は論理ピクセルなので scaleFactor で割る。
  let width = 1920;
  let height = 1080;
  let x = 0;
  let y = 0;
  try {
    const { primaryMonitor } = await import('@tauri-apps/api/window');
    const mon = await primaryMonitor();
    if (mon) {
      const sf = mon.scaleFactor || 1;
      width = Math.round(mon.size.width / sf);
      height = Math.round(mon.size.height / sf);
      x = Math.round(mon.position.x / sf);
      y = Math.round(mon.position.y / sf);
    }
  } catch (e) {
    console.warn('[danmaku] primaryMonitor failed; using fallback size', e);
  }

  const overlay = new WebviewWindow(DANMAKU_LABEL, {
    url: 'index.html?window=danmaku',
    transparent: true,
    decorations: false,
    alwaysOnTop: true,
    skipTaskbar: true,
    resizable: false,
    focus: false,
    shadow: false,
    width,
    height,
    x,
    y,
    title: 'fast-comment 弾幕',
  });

  await new Promise<void>((resolve, reject) => {
    overlay.once('tauri://created', () => resolve());
    overlay.once('tauri://error', (e) =>
      reject(new Error(`弾幕ウィンドウの生成に失敗: ${String((e as { payload?: unknown })?.payload ?? e)}`)),
    );
  });
}

export async function closeDanmakuOverlay(): Promise<void> {
  if (!isTauri()) return;
  const { WebviewWindow } = await import('@tauri-apps/api/webviewWindow');
  const existing = await WebviewWindow.getByLabel(DANMAKU_LABEL);
  if (existing) await existing.close();
}

/** トグル。新しい開閉状態(open=true)を返す。 */
export async function toggleDanmakuOverlay(): Promise<boolean> {
  if (!isTauri()) return false;
  if (await isDanmakuOverlayOpen()) {
    await closeDanmakuOverlay();
    return false;
  }
  await openDanmakuOverlay();
  return true;
}

export async function openUrl(url: string): Promise<void> {
  if (!url) return;
  await invoke<void>('open_url', { url });
}
