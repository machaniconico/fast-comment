// Pure helpers for the TTS settings UI (kept free of Svelte/Tauri so node --test can import them).
// Ranges mirror Rust `TtsOptions::normalize` (src-tauri/src/config.rs).

export type VoicevoxEngineKind = 'voicevox' | 'aivisSpeech';

export const ENGINE_DEFAULT_URLS: Record<VoicevoxEngineKind, string> = {
  voicevox: 'http://127.0.0.1:50021',
  aivisSpeech: 'http://127.0.0.1:10101'
};

function normUrl(url: string): string {
  return url.trim().replace(/\/+$/, '').toLowerCase();
}

/** URL after switching engine: blank or another preset's default is replaced; custom URLs stay. */
export function urlForEngineChange(currentUrl: string | null | undefined, next: VoicevoxEngineKind): string {
  const cur = normUrl(currentUrl ?? '');
  if (cur === '') return ENGINE_DEFAULT_URLS[next];
  const isPreset = (Object.values(ENGINE_DEFAULT_URLS) as string[]).some((u) => normUrl(u) === cur);
  return isPreset ? ENGINE_DEFAULT_URLS[next] : (currentUrl ?? '').trim();
}

export function clampNumber(v: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, v));
}

/** Blank / non-numeric input -> null (engine default); otherwise clamped to [min, max]. */
export function optionalClamped(v: unknown, min: number, max: number): number | null {
  if (v === null || v === undefined) return null;
  if (typeof v === 'string') {
    if (v.trim() === '') return null;
    v = Number(v);
  }
  if (typeof v !== 'number' || !Number.isFinite(v)) return null;
  return clampNumber(v, min, max);
}

/** Required number: non-finite -> fallback, else clamped. */
export function requiredClamped(v: unknown, min: number, max: number, fallback: number): number {
  if (typeof v === 'string' && v.trim() !== '') v = Number(v);
  if (typeof v !== 'number' || !Number.isFinite(v)) return fallback;
  return clampNumber(v, min, max);
}

export function intClamped(v: unknown, min: number, max: number, fallback: number): number {
  return Math.trunc(requiredClamped(v, min, max, fallback));
}

export const PARAM_RANGES = {
  speedScale: [0.5, 2],
  pitchScale: [-0.15, 0.15],
  intonationScale: [0, 2],
  volumeScale: [0, 2],
  prePhonemeLength: [0, 1.5],
  postPhonemeLength: [0, 1.5]
} as const;

export type VoicevoxParamKey = keyof typeof PARAM_RANGES;

export function buildVoicevoxParams(
  input: Partial<Record<VoicevoxParamKey, unknown>>
): Record<VoicevoxParamKey, number | null> {
  const out = {} as Record<VoicevoxParamKey, number | null>;
  for (const k of Object.keys(PARAM_RANGES) as VoicevoxParamKey[]) {
    const [lo, hi] = PARAM_RANGES[k];
    out[k] = optionalClamped(input[k], lo, hi);
  }
  return out;
}

export interface AutoSpeedForm {
  enabled: boolean;
  queueThreshold: unknown;
  perQueued: unknown;
  lengthThreshold: unknown;
  perTenChars: unknown;
  maxSpeed: unknown;
}

export function buildAutoSpeed(f: AutoSpeedForm) {
  return {
    enabled: !!f.enabled,
    queueThreshold: intClamped(f.queueThreshold, 0, 500, 2),
    perQueued: requiredClamped(f.perQueued, 0, 1, 0.1),
    lengthThreshold: intClamped(f.lengthThreshold, 0, 1000, 40),
    perTenChars: requiredClamped(f.perTenChars, 0, 1, 0.05),
    maxSpeed: requiredClamped(f.maxSpeed, 0.5, 2, 1.6)
  };
}

export function buildQueueLimits(maxPending: unknown, maxWaitSec: unknown) {
  return {
    maxPending: intClamped(maxPending, 1, 500, 64),
    maxWaitSec: intClamped(maxWaitSec, 0, 86400, 0)
  };
}

/** Per-kind template: checkbox OFF -> null (use the "all" template). ON keeps text as-is; "" means "don't read" (Rust distinguishes it from null). */
export function kindTemplate(enabled: boolean, text: string): string | null {
  if (!enabled) return null;
  return text.trim() === '' ? '' : text;
}

export const DEFAULT_TEMPLATE_ALL = '{name} {text}';

/** "all" template: an explicit ""/whitespace is kept as "" ("don't read"); the default is only filled in on load when the key is missing. */
export function allTemplate(text: string): string {
  return text.trim() === '' ? '' : text;
}

export interface SpeakerLike {
  id: number;
  name: string;
  style: string;
}

export function speakerLabel(s: SpeakerLike): string {
  return `${s.name}(${s.style}) [${s.id}]`;
}
