import type { ChannelConfig, ChannelOverrides, ChannelTtsVoice, ChannelsCommit, TtsEngineSpeaker } from './ipc';

export type TtsBackend = 'bouyomi' | 'voicevox' | 'webSpeech' | 'none';

/**
 * ダイアログの状態。`voice` は選択肢の value(棒読みちゃんは数値入力なので number か、消すと null)。
 * 空文字・null・undefined は「全体に従う」。
 */
export interface OverridesForm {
  ttsDisabled: boolean;
  /** 効果音を鳴らさない(false だけを保存する。チェックなし = 全体に従う)。 */
  soundDisabled: boolean;
  voice: string | number | null | undefined;
}

export interface VoiceOption {
  value: string;
  label: string;
}

export interface VoiceSources {
  speakers?: TtsEngineSpeaker[];
  webVoices?: string[];
  /** 保存済みの声の value。一覧に無くても選択肢に残す。 */
  saved?: string;
}

export const FOLLOW_GLOBAL_LABEL = '全体に従う';

function voiceValue(voice: ChannelTtsVoice | undefined, backend: TtsBackend): string {
  if (!voice) return '';
  if (backend === 'voicevox') return voice.voicevoxSpeaker === undefined ? '' : String(voice.voicevoxSpeaker);
  if (backend === 'bouyomi') return voice.bouyomiVoice === undefined ? '' : String(voice.bouyomiVoice);
  if (backend === 'webSpeech') return voice.webSpeechVoice ?? '';
  return '';
}

export function toFormState(overrides: ChannelOverrides | undefined, backend: TtsBackend): OverridesForm {
  return {
    ttsDisabled: overrides?.ttsEnabled === false,
    soundDisabled: overrides?.soundEnabled === false,
    voice: voiceValue(overrides?.ttsVoice, backend),
  };
}

/** 空の声と「読む」(true)は送らない。他 backend の声は `prev` のまま残す。 */
export function fromFormState(
  form: OverridesForm,
  backend: TtsBackend,
  prev?: ChannelOverrides,
): ChannelOverrides {
  const out: ChannelOverrides = {};
  if (form.ttsDisabled) out.ttsEnabled = false;
  if (form.soundDisabled) out.soundEnabled = false;
  const voice: ChannelTtsVoice = { ...prev?.ttsVoice };
  if (backend === 'voicevox') delete voice.voicevoxSpeaker;
  else if (backend === 'bouyomi') delete voice.bouyomiVoice;
  else if (backend === 'webSpeech') delete voice.webSpeechVoice;
  const raw = form.voice === null || form.voice === undefined ? '' : String(form.voice);
  if (raw.trim() !== '') {
    if (backend === 'webSpeech') voice.webSpeechVoice = raw;
    else {
      const n = Number(raw);
      if (Number.isInteger(n)) {
        if (backend === 'voicevox') voice.voicevoxSpeaker = n;
        else if (backend === 'bouyomi') voice.bouyomiVoice = n;
      }
    }
  }
  if (Object.keys(voice).length > 0) out.ttsVoice = voice;
  return out;
}

/**
 * Stale で読み直した後の入力。利用者の未保存の選択を新しい基準の上に載せ直す(もう一度保存すれば
 * それが新しい revision に対して保存される)。backend が変わった時は声の意味が変わるので声だけ基準に戻す。
 */
export function reapplyPendingForm(
  previousBase: OverridesForm,
  pending: OverridesForm,
  nextBase: OverridesForm,
  backendChanged: boolean,
): OverridesForm {
  const voiceChanged = String(pending.voice ?? '') !== String(previousBase.voice ?? '');
  return {
    ttsDisabled: pending.ttsDisabled !== previousBase.ttsDisabled ? pending.ttsDisabled : nextBase.ttsDisabled,
    soundDisabled:
      pending.soundDisabled !== previousBase.soundDisabled ? pending.soundDisabled : nextBase.soundDisabled,
    voice: voiceChanged && !backendChanged ? pending.voice : nextBase.voice,
  };
}

/** 棒読みちゃんは番号入力(空 = 全体に従う、0 = 棒読みちゃん既定)なので選択肢は先頭だけ。範囲は Rust が検証する。 */
export function voiceOptions(backend: TtsBackend, sources: VoiceSources = {}): VoiceOption[] {
  const options: VoiceOption[] = [{ value: '', label: FOLLOW_GLOBAL_LABEL }];
  const seen = new Set<string>(['']);
  const add = (value: string, label: string) => {
    if (seen.has(value)) return;
    seen.add(value);
    options.push({ value, label });
  };
  if (backend === 'voicevox') {
    for (const s of sources.speakers ?? []) add(String(s.id), `${s.name}(${s.style})`);
    if (sources.saved) add(sources.saved, `話者 ${sources.saved}`);
  } else if (backend === 'webSpeech') {
    for (const name of sources.webVoices ?? []) add(name, name);
    if (sources.saved) add(sources.saved, sources.saved);
  }
  return options;
}

/** チップの印。読まない枠・効果音を切った枠だけ出す。 */
export function chipBadge(ch: ChannelConfig): string | null {
  const marks: string[] = [];
  if (ch.overrides?.ttsEnabled === false) marks.push('読まない');
  if (ch.overrides?.soundEnabled === false) marks.push('効果音なし');
  return marks.length > 0 ? marks.join('・') : null;
}

/** 行を書く IPC の応答で、画面の行を丸ごと置き換える(Rust が正規化した値が正)。 */
export function applyChannelsSaved(_local: ChannelConfig[], saved: Pick<ChannelsCommit, 'channels'>): ChannelConfig[] {
  return saved.channels.map((c) => ({ ...c }));
}

/** X/ニコ生は配信ごとに URL を貼る運用なので、同じプラットフォームの前の接続をすべて置き換える(1 回の保存)。 */
export function replacedChannelKeys(channels: ChannelConfig[], added: ChannelConfig): string[] {
  if (added.platform !== 'x' && added.platform !== 'niconico') return [];
  return channels
    .filter((c) => c.platform === added.platform && c.identifier !== added.identifier)
    .map((c) => `${c.platform}:${c.identifier}`);
}
