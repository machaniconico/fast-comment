<script lang="ts">
  import { onDestroy, onMount, untrack } from 'svelte';
  import { store } from '../stores.svelte';
  import { readCustomSound } from '../ipc';
  import { createSoundPlayer } from '../soundEffects';

  let prev = $state(0);
  let initialized = $state(false);
  let flashing = $state(false);

  let lastBeep = 0;
  const sounds = createSoundPlayer({
    createContext: () => new AudioContext() as never,
    createAudio: () => new Audio(),
    loadCustom: readCustomSound,
    createObjectUrl: (blob) => URL.createObjectURL(blob),
    revokeObjectUrl: (url) => URL.revokeObjectURL(url),
  });

  onMount(() => {
    store.soundHandler = (sound, volume) => void sounds.play(sound, volume);
    return () => {
      store.soundHandler = null;
    };
  });

  let audioCtx: AudioContext | null = null;
  let flashTimeout: ReturnType<typeof setTimeout> | null = null;

  function clampVolume(volume: number): number {
    return Math.max(0, Math.min(1, volume));
  }

  function triggerFlash() {
    if (flashTimeout !== null) {
      clearTimeout(flashTimeout);
      flashTimeout = null;
    }

    flashing = true;
    flashTimeout = setTimeout(() => {
      flashing = false;
      flashTimeout = null;
    }, 200);
  }

  async function playBeep() {
    try {
      audioCtx ??= new AudioContext();

      if (audioCtx.state === 'suspended') {
        await audioCtx.resume();
      }

      const now = audioCtx.currentTime;
      const oscillator = audioCtx.createOscillator();
      const gain = audioCtx.createGain();
      const volume = clampVolume(store.notifyVolume);
      const duration = 0.14;

      oscillator.type = 'sine';
      oscillator.frequency.setValueAtTime(880, now);

      gain.gain.cancelScheduledValues(now);
      gain.gain.setValueAtTime(volume, now);
      gain.gain.exponentialRampToValueAtTime(0.0001, now + duration);

      oscillator.connect(gain);
      gain.connect(audioCtx.destination);
      oscillator.start(now);
      oscillator.stop(now + duration);
    } catch {
      // Notification audio must never break the app if Web Audio is unavailable.
    }
  }

  $effect(() => {
    const current = store.highlightCount;
    const last = untrack(() => prev);
    const wasInitialized = untrack(() => initialized);

    if (!wasInitialized) {
      untrack(() => {
        lastBeep = store.beepCount;
        prev = current;
        initialized = true;
      });
    } else if (current > last) {
      untrack(() => {
        prev = current;
      });

      if (store.notifySound) {
        // 同じメッセージに効果音が付いた時は効果音を優先し、ビープは鳴らさない(フラッシュは出す)。
        const beeps = untrack(() => store.beepCount);
        if (beeps > lastBeep) void playBeep();
        triggerFlash();
      }
      lastBeep = untrack(() => store.beepCount);
    } else if (current !== last) {
      // highlightCount is monotonic, but keep local state synced if stores are replaced in tests.
      untrack(() => {
        prev = current;
      });
    }
  });

  onDestroy(() => {
    sounds.dispose();
    if (flashTimeout !== null) {
      clearTimeout(flashTimeout);
      flashTimeout = null;
    }

    if (audioCtx !== null) {
      void audioCtx.close();
      audioCtx = null;
    }
  });
</script>

<div class:flashing class="notification-flash" aria-hidden="true"></div>

<style>
  .notification-flash {
    position: fixed;
    inset: 0;
    pointer-events: none;
    z-index: 2147483647;
    opacity: 0;
    box-shadow: inset 0 0 42px rgba(255, 152, 0, 0);
    transition:
      opacity 0.2s ease-out,
      box-shadow 0.2s ease-out;
  }

  .notification-flash.flashing {
    opacity: 1;
    box-shadow: inset 0 0 42px rgba(255, 152, 0, 0.15);
  }
</style>
