/**
 * IPC layer between Tauri backend and Svelte UI.
 *
 * - listen('chat', ...) receives batched ChatMessage arrays from Rust.
 * - rAF batching: incoming messages are queued and flushed on the next
 *   animation frame to avoid blocking the main thread on high-frequency input.
 * - Safe to import in a plain browser (Tauri absent): all calls are no-ops.
 */

import type { ChatMessage } from './types';

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
  tts: { backend: 'bouyomi' | 'voicevox' | 'webSpeech' | 'none'; options: TtsOptions };
  moderation: { ngWords: string[]; ngUsers: string[]; highlights: string[] };
  ui: { maxBuffer: number; showDonationPanel: boolean; notifySound: boolean; notifyVolume: number };
  participation: ParticipationConfig;
  youtubeOverrides?: { apiKey?: string; clientVersion?: string; paths?: Record<string, string> };
}

export interface GoalsConfig {
  enabled: boolean;
  showInApp: boolean;
  comments: number;
  viewers: number;
  likes: number;
}

export interface GoalsSnapshot {
  comments: number;
  viewers: number;
  likes: number;
}

export interface StatsSnapshot {
  comments: number;
  viewers: number;
  viewersMax: number;
  likes: number;
  likesAvailable: boolean;
  goals: GoalsSnapshot;
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

export async function checkForUpdate(): Promise<UpdateStatus | null> {
  return invoke<UpdateStatus>('check_for_update');
}

export async function openReleaseUrl(url: string): Promise<void> {
  if (!url) return;
  await invoke<void>('open_url', { url });
}
