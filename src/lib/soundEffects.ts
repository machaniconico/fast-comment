/**
 * 種別ごとの効果音の再生。判定・間引きは Rust が済ませ、`ChatMessage.sound` に入れて送ってくる。
 * ここは鳴らすだけ。AudioContext / Audio / bytes の読み込みは注入する(Node で試せる純モジュール)。
 * 型除去だけで動く構文(enum・parameter property なし)に限る。
 */

export interface SoundRuleConfig {
  enabled: boolean;
  /** `builtin:<name>` か `custom:<id>`。 */
  sound: string;
  /** 円。投げ銭系(superChat / bits / gift)だけが見る。 */
  minJpy: number;
}

/** Rust `SoundEffectsConfig`(config.json の `soundEffects`)の写し。 */
export interface SoundEffectsConfig {
  enabled: boolean;
  volume: number;
  minIntervalMs: number;
  superChat: SoundRuleConfig;
  bits: SoundRuleConfig;
  gift: SoundRuleConfig;
  membership: SoundRuleConfig;
  firstComment: SoundRuleConfig;
}

/** Rust `SoundEntry`(sounds.json の 1 件)。 */
export interface CustomSound {
  id: string;
  name: string;
  ext: string;
  size: number;
}

export type RuleKey = 'superChat' | 'bits' | 'gift' | 'membership' | 'firstComment';

export const RULES: ReadonlyArray<{ key: RuleKey; label: string; money: boolean }> = [
  { key: 'superChat', label: 'スーパーチャット', money: true },
  { key: 'bits', label: 'Bits', money: true },
  { key: 'gift', label: 'ギフト', money: true },
  { key: 'membership', label: 'メンバー加入', money: false },
  { key: 'firstComment', label: '初コメント', money: false },
];

export const BUILTIN_SOUNDS: ReadonlyArray<{ name: string; label: string }> = [
  { name: 'chime', label: 'チャイム' },
  { name: 'bell', label: 'ベル' },
  { name: 'pop', label: 'ポップ' },
  { name: 'coin', label: 'コイン' },
];

/** Rust `SoundEffectsConfig::default()` の各規則の音。削除済みの音源を差し替える先。 */
export const DEFAULT_RULE_SOUNDS: Record<RuleKey, string> = {
  superChat: 'builtin:coin',
  bits: 'builtin:pop',
  gift: 'builtin:bell',
  membership: 'builtin:chime',
  firstComment: 'builtin:pop',
};

export const FALLBACK_SOUND = 'builtin:chime';
export const MAX_CUSTOM_SOUND_BYTES = 2 * 1024 * 1024;
export const MAX_CUSTOM_SOUNDS = 32;
const MAX_CACHED_URLS = 8;

export type SoundRef = { kind: 'builtin'; name: string } | { kind: 'custom'; id: string };

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

export function parseSoundRef(value: unknown): SoundRef | null {
  if (typeof value !== 'string') return null;
  const sep = value.indexOf(':');
  if (sep < 0) return null;
  const scheme = value.slice(0, sep);
  const rest = value.slice(sep + 1);
  if (scheme === 'builtin' && BUILTIN_SOUNDS.some((s) => s.name === rest)) return { kind: 'builtin', name: rest };
  if (scheme === 'custom' && UUID.test(rest)) return { kind: 'custom', id: rest };
  return null;
}

export function clampVolume(volume: number): number {
  return Number.isFinite(volume) ? Math.min(1, Math.max(0, volume)) : 0;
}

interface Tone {
  freq: number;
  at: number;
  dur: number;
  type: 'sine' | 'triangle' | 'square';
  gain: number;
}

const TONES: Record<string, Tone[]> = {
  chime: [
    { freq: 880, at: 0, dur: 0.16, type: 'sine', gain: 1 },
    { freq: 1318, at: 0.12, dur: 0.24, type: 'sine', gain: 1 },
  ],
  bell: [
    { freq: 1046, at: 0, dur: 0.6, type: 'triangle', gain: 1 },
    { freq: 2093, at: 0, dur: 0.35, type: 'sine', gain: 0.3 },
  ],
  pop: [{ freq: 520, at: 0, dur: 0.08, type: 'square', gain: 0.4 }],
  coin: [
    { freq: 988, at: 0, dur: 0.07, type: 'square', gain: 0.35 },
    { freq: 1319, at: 0.07, dur: 0.28, type: 'square', gain: 0.35 },
  ],
};

export interface AudioParamLike {
  setValueAtTime(value: number, time: number): unknown;
  exponentialRampToValueAtTime(value: number, time: number): unknown;
}

export interface AudioContextLike {
  readonly currentTime: number;
  readonly state: string;
  readonly destination: unknown;
  resume(): Promise<void>;
  close(): Promise<void>;
  createOscillator(): {
    type: string;
    frequency: AudioParamLike;
    connect(node: unknown): unknown;
    start(time: number): void;
    stop(time: number): void;
  };
  createGain(): { gain: AudioParamLike; connect(node: unknown): unknown };
}

export interface PlayableAudio {
  src: string;
  volume: number;
  play(): Promise<void> | void;
}

export interface LoadedSound {
  mime: string;
  bytes: Uint8Array;
}

export interface SoundPlayerDeps {
  createContext: () => AudioContextLike;
  createAudio: () => PlayableAudio;
  /** カスタム音源の中身を Rust から取る。無ければ null。 */
  loadCustom: (id: string) => Promise<LoadedSound | null>;
  createObjectUrl: (blob: Blob) => string;
  revokeObjectUrl: (url: string) => void;
}

export interface SoundPlayer {
  /**
   * 鳴らせたら true。カスタムが読めない時は内蔵の既定音で代用する。
   * AudioContext が止まっている間(自動再生の制限)の音は溜めずに捨て、解除を試みる。
   * 利用者の操作中(試し鳴らし)は `unlock` を true にして、解除を待ってから鳴らす。
   */
  play(sound: string, volume: number, unlock?: boolean): Promise<boolean>;
  /** 削除したカスタム音源の Blob URL を捨てる。 */
  forget(id: string): void;
  dispose(): void;
}

export function createSoundPlayer(deps: SoundPlayerDeps): SoundPlayer {
  let ctx: AudioContextLike | null = null;
  const urls = new Map<string, Promise<string | null>>();

  async function synth(name: string, volume: number, unlock: boolean): Promise<boolean> {
    const tones = TONES[name];
    if (!tones) return false;
    try {
      ctx ??= deps.createContext();
      if (ctx.state === 'suspended') {
        if (!unlock) {
          // 古い通知が解除後にまとめて鳴らないよう、この音は捨てる。
          void Promise.resolve(ctx.resume()).catch(() => {});
          return false;
        }
        await ctx.resume();
      }
      const base = ctx.currentTime;
      for (const tone of tones) {
        const osc = ctx.createOscillator();
        const gain = ctx.createGain();
        const start = base + tone.at;
        osc.type = tone.type;
        osc.frequency.setValueAtTime(tone.freq, start);
        gain.gain.setValueAtTime(Math.max(0.0001, volume * tone.gain), start);
        gain.gain.exponentialRampToValueAtTime(0.0001, start + tone.dur);
        osc.connect(gain);
        gain.connect(ctx.destination);
        osc.start(start);
        osc.stop(start + tone.dur);
      }
      return true;
    } catch {
      return false;
    }
  }

  function customUrl(id: string): Promise<string | null> {
    const cached = urls.get(id);
    if (cached) {
      // 使った物を最新にする(LRU)。
      urls.delete(id);
      urls.set(id, cached);
      return cached;
    }
    const pending = (async () => {
      try {
        const loaded = await deps.loadCustom(id);
        if (!loaded) return null;
        return deps.createObjectUrl(new Blob([loaded.bytes as BlobPart], { type: loaded.mime }));
      } catch {
        return null;
      }
    })();
    urls.set(id, pending);
    void pending.then((url) => {
      if (url === null && urls.get(id) === pending) urls.delete(id);
    });
    while (urls.size > MAX_CACHED_URLS) {
      const oldest = urls.keys().next().value as string;
      revoke(oldest);
    }
    return pending;
  }

  function revoke(id: string): void {
    const entry = urls.get(id);
    urls.delete(id);
    void entry?.then((url) => {
      if (url !== null) deps.revokeObjectUrl(url);
    });
  }

  async function playCustom(id: string, volume: number): Promise<boolean> {
    const url = await customUrl(id);
    if (url === null) return false;
    try {
      const audio = deps.createAudio();
      audio.src = url;
      audio.volume = volume;
      await audio.play();
      return true;
    } catch {
      return false;
    }
  }

  /** 要求した時点で止まっているか。止まっていれば解除を試みる(この音は鳴らさない)。 */
  function suspendedNow(): boolean {
    try {
      ctx ??= deps.createContext();
      if (ctx.state !== 'suspended') return false;
      void Promise.resolve(ctx.resume()).catch(() => {});
      return true;
    } catch {
      return false;
    }
  }

  return {
    async play(sound, volume, unlock = false) {
      const v = clampVolume(volume);
      // 音量 0 は無音。発振器も Audio も作らない。
      if (v <= 0) return false;
      const ref = parseSoundRef(sound);
      if (ref?.kind === 'custom') {
        // 読み込み中に解除されても、要求時に止まっていた音は鳴らさない(試し鳴らしは除く)。
        if (!unlock && suspendedNow()) return false;
        if (await playCustom(ref.id, v)) return true;
      }
      if (ref?.kind === 'builtin') return synth(ref.name, v, unlock);
      return synth(FALLBACK_SOUND.slice('builtin:'.length), v, unlock);
    },
    forget: revoke,
    dispose() {
      for (const id of [...urls.keys()]) revoke(id);
      const closing = ctx;
      ctx = null;
      void closing?.close().catch(() => {});
    },
  };
}

/**
 * 1 件のコメントで鳴らす物。削除の印(`removed`)が付いたコメントはビープも効果音も鳴らさない。
 * 既存のハイライト通知音(ビープ)は、同じメッセージに効果音が付いた時は効果音を優先して鳴らさない。
 */
export function alertFor(
  highlighted: boolean,
  msg: { sound?: string; removed?: string },
): { beep: boolean; sound: string | undefined } {
  if (msg.removed !== undefined) return { beep: false, sound: undefined };
  return { beep: highlighted && !msg.sound, sound: msg.sound || undefined };
}

/** 削除したカスタム音源を使っているフォームの行だけを既定音にする。ほかの項目は触らない。 */
export function replaceRemovedSound(cfg: SoundEffectsConfig, id: string): SoundEffectsConfig {
  const removed = `custom:${id}`;
  const next = { ...cfg };
  for (const { key } of RULES) {
    if (cfg[key].sound === removed) next[key] = { ...cfg[key], sound: DEFAULT_RULE_SOUNDS[key] };
  }
  return next;
}

/** 「円」入力の欄を 0〜上限の整数にする。空・負・NaN は 0。 */
export function normalizeMinJpy(value: unknown, max = 10_000_000): number {
  const n = typeof value === 'number' ? value : Number(value);
  return Number.isFinite(n) && n > 0 ? Math.min(max, Math.floor(n)) : 0;
}

/** 取り込み前の検査(Rust が最終的に検証する。ここは早めに断るだけ)。理由が無ければ null。 */
export function validateSoundFile(name: string, size: number, existingCount: number): string | null {
  if (!/\.(wav|mp3|ogg)$/i.test(name)) return 'wav / mp3 / ogg のファイルを選んでください';
  if (size > MAX_CUSTOM_SOUND_BYTES) return `ファイルが大きすぎます(上限 ${MAX_CUSTOM_SOUND_BYTES / 1024 / 1024} MiB)`;
  if (size <= 0) return '空のファイルです';
  if (existingCount >= MAX_CUSTOM_SOUNDS) return `カスタム効果音は ${MAX_CUSTOM_SOUNDS} 件までです`;
  return null;
}

/** 選択肢の表示名。カスタムが一覧に無い(削除済み等)時はその旨を出す。 */
export function soundLabel(sound: string, custom: ReadonlyArray<CustomSound>): string {
  const ref = parseSoundRef(sound);
  if (ref?.kind === 'builtin') return BUILTIN_SOUNDS.find((s) => s.name === ref.name)?.label ?? sound;
  if (ref?.kind === 'custom') return custom.find((c) => c.id === ref.id)?.name ?? '(削除済みの音源)';
  return '(不明)';
}
