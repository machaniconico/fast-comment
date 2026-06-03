<script lang="ts">
  import { onDestroy } from 'svelte';
  import { store } from '../stores.svelte';

  const MILESTONES = [50, 100, 250, 500, 1000, 2500, 5000, 10000];
  const DISPLAY_MS = 4200;

  let currentMilestone: number | null = $state(null);
  let initialized = false;
  let baseline = 0;
  let previousCount = 0;
  let showing = false;
  let hideTimer: ReturnType<typeof setTimeout> | null = null;
  const queue: number[] = [];

  $effect(() => {
    const receivedCount = store.receivedCount;

    if (!initialized) {
      baseline = receivedCount;
      previousCount = receivedCount;
      initialized = true;
      return;
    }

    if (receivedCount <= previousCount) {
      previousCount = receivedCount;
      return;
    }

    const crossed = MILESTONES.filter((milestone) =>
      milestone > baseline && milestone > previousCount && milestone <= receivedCount
    );

    if (crossed.length > 0) {
      queue.push(...crossed);
      showNext();
    }

    previousCount = receivedCount;
  });

  onDestroy(() => {
    if (hideTimer !== null) {
      clearTimeout(hideTimer);
      hideTimer = null;
    }
    queue.length = 0;
  });

  function showNext() {
    if (showing) return;

    const next = queue.shift();
    if (next === undefined) return;

    showing = true;
    currentMilestone = next;
    hideTimer = setTimeout(() => {
      currentMilestone = null;
      showing = false;
      hideTimer = null;
      showNext();
    }, DISPLAY_MS);
  }
</script>

<div class="milestone-overlay" aria-live="polite">
  {#if currentMilestone !== null}
    {#key currentMilestone}
      <div class="milestone-banner" role="status">
        🎉 コメント{currentMilestone.toLocaleString('ja-JP')}件達成!
      </div>
    {/key}
  {/if}
</div>

<style>
  .milestone-overlay {
    position: fixed;
    top: 18px;
    left: 50%;
    z-index: 2147483645;
    width: min(540px, calc(100vw - 24px));
    pointer-events: none;
    transform: translateX(-50%);
  }

  .milestone-banner {
    max-width: 100%;
    padding: 12px 18px;
    border: 1px solid rgba(251, 191, 36, 0.56);
    border-radius: 8px;
    background: rgba(24, 24, 27, 0.94);
    box-shadow: 0 16px 36px rgba(0, 0, 0, 0.34);
    color: #fff7ed;
    font-size: 20px;
    font-weight: 800;
    line-height: 1.35;
    text-align: center;
    overflow-wrap: anywhere;
    animation: milestone-toast 4.05s ease forwards;
  }

  @keyframes milestone-toast {
    0% {
      opacity: 0;
      transform: translateY(-12px) scale(0.96);
    }
    8% {
      opacity: 1;
      transform: translateY(0) scale(1);
    }
    88% {
      opacity: 1;
      transform: translateY(0) scale(1);
    }
    100% {
      opacity: 0;
      transform: translateY(-8px) scale(0.98);
    }
  }
</style>
