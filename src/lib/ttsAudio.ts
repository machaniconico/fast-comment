/**
 * FIFO player for base64 WAV chunks sent by the VOICEVOX backend ('tts-audio').
 * Pure module: the Audio element factory is injected so it can be tested in Node.
 * Only type-stripping-safe syntax (no enums / parameter properties).
 */

export interface AudioLike {
  src: string;
  onended: ((ev: Event) => unknown) | null;
  onerror: ((ev: Event | string) => unknown) | null;
  play(): Promise<void> | void;
  pause(): void;
}

export interface TtsAudioPlayer {
  /** Queue a base64 WAV. Empty / non-string input is ignored. */
  enqueue(base64Wav: string): void;
  /** Stop current playback and drop everything queued. */
  cancel(): void;
  /** Number of pending (not yet started) items. */
  readonly pending: number;
}

export const DEFAULT_MAX_QUEUE = 50;

export function createTtsAudioPlayer(
  createAudio: () => AudioLike,
  maxQueue: number = DEFAULT_MAX_QUEUE,
  onPlayError?: (error: unknown) => void,
): TtsAudioPlayer {
  // Guard the public arg: negative would spin the trim loop, NaN would disable it.
  const cap = Number.isFinite(maxQueue) && maxQueue >= 1 ? Math.floor(maxQueue) : DEFAULT_MAX_QUEUE;
  const queue: string[] = [];
  let current: AudioLike | null = null;

  function detach(audio: AudioLike): void {
    audio.onended = null;
    audio.onerror = null;
  }

  function reportError(error: unknown): void {
    if (!onPlayError) return;
    try {
      onPlayError(error);
    } catch (e) {
      console.warn('[ttsAudio] onPlayError threw', e);
    }
  }

  function playNext(): void {
    if (current) return;
    const next = queue.shift();
    if (next === undefined) return;
    let audio: AudioLike;
    try {
      audio = createAudio();
    } catch (e) {
      console.warn('[ttsAudio] createAudio failed', e);
      playNext();
      return;
    }
    current = audio;
    let done = false;
    const finish = (error?: unknown): void => {
      if (done || current !== audio) return;
      done = true;
      if (error !== undefined) reportError(error);
      detach(audio);
      current = null;
      playNext();
    };
    audio.onended = () => finish();
    audio.onerror = (ev) => finish(ev ?? new Error('audio error'));
    try {
      audio.src = `data:audio/wav;base64,${next}`;
      const p = audio.play();
      if (p && typeof p.then === 'function') {
        p.then(undefined, (e: unknown) => {
          console.warn('[ttsAudio] play failed', e);
          finish(e ?? new Error('play rejected'));
        });
      }
    } catch (e) {
      console.warn('[ttsAudio] play threw', e);
      finish(e ?? new Error('play threw'));
    }
  }

  return {
    enqueue(base64Wav: string): void {
      if (typeof base64Wav !== 'string' || base64Wav.length === 0) return;
      queue.push(base64Wav);
      while (queue.length > cap) queue.shift();
      playNext();
    },
    cancel(): void {
      queue.length = 0;
      const audio = current;
      current = null;
      if (audio) {
        detach(audio);
        try {
          audio.pause();
        } catch (e) {
          console.warn('[ttsAudio] pause failed', e);
        }
      }
    },
    get pending(): number {
      return queue.length;
    },
  };
}
