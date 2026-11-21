<script lang="ts">
  import { onMount } from 'svelte';
  import { onAchievements } from '../ipc';
  import { ACHIEVEMENT_TOAST_MS, AchievementQueue, achievementViewerLabel } from '../achievementQueue';
  import type { AchievementTier, AchievementUnlocked } from '../types';

  const TIER_LABEL: Record<AchievementTier, string> = {
    bronze: 'ブロンズ',
    silver: 'シルバー',
    gold: 'ゴールド',
    platinum: 'プラチナ',
  };

  const queue = new AchievementQueue();
  let current = $state<AchievementUnlocked | null>(null);
  let hidden = $state(0);
  let timer: ReturnType<typeof setTimeout> | null = null;

  function sync() {
    current = queue.current;
    hidden = queue.hidden;
  }

  function arm() {
    if (timer !== null) clearTimeout(timer);
    timer = setTimeout(() => {
      timer = null;
      queue.advance();
      sync();
      if (queue.current !== null) arm();
    }, ACHIEVEMENT_TOAST_MS);
  }

  function receive(unlocks: AchievementUnlocked[]) {
    if (!Array.isArray(unlocks) || unlocks.length === 0) return;
    const wasIdle = queue.current === null;
    queue.push(unlocks);
    sync();
    if (wasIdle && queue.current !== null) arm();
  }

  onMount(() => {
    let destroyed = false;
    let unlisten: (() => void) | null = null;
    void onAchievements(receive).then((fn) => {
      if (destroyed) fn();
      else unlisten = fn;
    });
    return () => {
      destroyed = true;
      unlisten?.();
      if (timer !== null) clearTimeout(timer);
    };
  });
</script>

{#if current}
  <div class="ach-toast {current.tier}" role="status" aria-live="polite">
    <span class="ach-icon" aria-hidden="true">{current.icon || '🏆'}</span>
    <div class="ach-body">
      <div class="ach-head">{TIER_LABEL[current.tier] ?? ''}</div>
      <div class="ach-name">{current.name}</div>
      <div class="ach-who">{achievementViewerLabel(current)}</div>
    </div>
    {#if hidden > 0}
      <span class="ach-more">+{hidden}</span>
    {/if}
  </div>
{/if}

<style>
  .ach-toast {
    --ach-color: #cd7f32;
    position: fixed;
    right: 12px;
    bottom: 12px;
    z-index: 1004;
    display: flex;
    align-items: center;
    gap: 10px;
    max-width: min(360px, calc(100vw - 24px));
    padding: 8px 12px;
    border-radius: 6px;
    border-left: 4px solid var(--ach-color);
    background: rgba(0, 0, 0, 0.88);
    color: #fff;
    font-size: 13px;
    pointer-events: none;
  }
  .ach-toast.silver { --ach-color: #b0bec5; }
  .ach-toast.gold { --ach-color: #ffca28; }
  .ach-toast.platinum { --ach-color: #4dd0e1; }
  .ach-icon { font-size: 24px; line-height: 1; }
  .ach-body { min-width: 0; }
  .ach-head { font-size: 11px; color: var(--ach-color); }
  .ach-name { font-weight: 600; overflow-wrap: anywhere; }
  .ach-who { font-size: 12px; opacity: 0.8; overflow-wrap: anywhere; }
  .ach-more {
    flex-shrink: 0;
    padding: 1px 6px;
    border-radius: 8px;
    background: rgba(255, 255, 255, 0.18);
    font-size: 11px;
  }
</style>
