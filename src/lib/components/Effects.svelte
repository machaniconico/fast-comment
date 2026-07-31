<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { onYoutubeReactions, type EffectsConfig } from '../ipc';
  import type { UiChatMessage, YoutubeReaction } from '../types';
  import { store } from '../stores.svelte';

  interface Props {
    config: EffectsConfig;
  }

  interface Particle {
    id: number;
    emoji: string;
    x: number;
    drift: number;
    duration: number;
    delay: number;
    size: number;
    rotate: number;
    rise: number;
    reaction: boolean;
  }

  const MAX_PARTICLES = 200;
  const MAX_REACTION_TEXT_LENGTH = 32;
  const MAX_SEEN_IDS = 2000;
  const FALLBACK_REMOVE_MS = 4500;

  let { config }: Props = $props();
  let particles: Particle[] = $state([]);
  let nextParticleId = 1;
  let initialized = false;
  let lastReceivedCount = 0;
  const processedIds = new Set<string>();
  const removeTimers = new Map<number, ReturnType<typeof setTimeout>>();
  let reducedMotion = false;

  onMount(() => {
    let disposed = false;
    let unlistenReactions = () => {};
    const motionQuery = window.matchMedia('(prefers-reduced-motion: reduce)');
    const syncMotionPreference = () => {
      reducedMotion = motionQuery.matches;
      if (reducedMotion) clearParticles();
    };
    syncMotionPreference();
    motionQuery.addEventListener('change', syncMotionPreference);

    void onYoutubeReactions((reactions) => {
      if (!disposed && !reducedMotion) spawnReactionBatch(reactions);
    })
      .then((unlisten) => {
        if (disposed) unlisten();
        else unlistenReactions = unlisten;
      })
      .catch((error) => {
        if (!disposed) console.warn('YouTubeリアクション購読に失敗しました', error);
      });

    return () => {
      disposed = true;
      unlistenReactions();
      motionQuery.removeEventListener('change', syncMotionPreference);
    };
  });

  $effect(() => {
    const receivedCount = store.receivedCount;
    const messages = store.allMessages;
    const enabled = config.enabled;
    const rules = normalizeRules(config.rules);

    if (!initialized) {
      markMessagesProcessed(messages);
      lastReceivedCount = receivedCount;
      initialized = true;
      return;
    }

    if (!enabled || rules.length === 0 || receivedCount === lastReceivedCount) {
      lastReceivedCount = receivedCount;
      return;
    }

    for (const message of messages) {
      if (processedIds.has(message.id)) continue;
      processedIds.add(message.id);
      const text = messageText(message).toLowerCase();
      for (const rule of rules) {
        if (text.includes(rule.keyword)) {
          spawnParticles(rule.emoji, rule.count);
        }
      }
    }
    pruneProcessedIds(messages);
    lastReceivedCount = receivedCount;
  });

  onDestroy(() => {
    clearParticles();
  });

  function normalizeRules(rules: EffectsConfig['rules']) {
    return (rules ?? [])
      .map((rule) => ({
        keyword: rule.keyword.trim().toLowerCase(),
        emoji: rule.emoji,
        count: boundedCount(rule.count)
      }))
      .filter((rule) => rule.keyword !== '' && rule.emoji !== '' && rule.count > 0);
  }

  function boundedCount(value: unknown): number {
    const n = typeof value === 'number' ? value : Number(value);
    if (!Number.isFinite(n) || n <= 0) return 0;
    return Math.min(MAX_PARTICLES, Math.trunc(n));
  }

  function messageText(message: UiChatMessage): string {
    return message.fragments.map((fragment) => fragment.type === 'text' ? fragment.text : '').join('');
  }

  function markMessagesProcessed(messages: UiChatMessage[]) {
    for (const message of messages) processedIds.add(message.id);
    pruneProcessedIds(messages);
  }

  function pruneProcessedIds(messages: UiChatMessage[]) {
    if (processedIds.size <= MAX_SEEN_IDS) return;
    const keep = new Set(messages.slice(-MAX_SEEN_IDS).map((message) => message.id));
    for (const id of processedIds) {
      if (!keep.has(id)) processedIds.delete(id);
      if (processedIds.size <= MAX_SEEN_IDS) break;
    }
  }

  function spawnReactionBatch(reactions: YoutubeReaction[]) {
    const merged = new Map<string, number>();
    for (const reaction of reactions) {
      const emoji = typeof reaction.emoji === 'string' ? reaction.emoji.trim() : '';
      const count = boundedCount(reaction.count);
      if (emoji === '' || emoji.length > MAX_REACTION_TEXT_LENGTH || count <= 0) continue;
      merged.set(emoji, Math.min(MAX_PARTICLES, (merged.get(emoji) ?? 0) + count));
    }

    const entries = [...merged.entries()].map(([emoji, count]) => ({ emoji, remaining: count, quota: 0 }));
    const limits = reactionParticleLimits();
    const activeReactions = particles.reduce((count, particle) => count + Number(particle.reaction), 0);
    let budget = Math.min(
      limits.perFrame,
      Math.max(0, limits.active - activeReactions),
      Math.max(0, MAX_PARTICLES - particles.length)
    );
    while (budget > 0 && entries.some((entry) => entry.remaining > 0)) {
      for (const entry of entries) {
        if (budget <= 0) break;
        if (entry.remaining <= 0) continue;
        entry.remaining -= 1;
        entry.quota += 1;
        budget -= 1;
      }
    }
    for (const entry of entries) {
      if (entry.quota > 0) spawnParticles(entry.emoji, entry.quota, true);
    }
  }

  function reactionParticleLimits() {
    if (window.innerWidth <= 480) return { perFrame: 10, active: 24 };
    if (window.innerWidth <= 768) return { perFrame: 14, active: 36 };
    return { perFrame: 20, active: 56 };
  }

  function spawnParticles(emoji: string, requestedCount: number, reaction = false) {
    if (reducedMotion) return;
    const available = Math.max(0, MAX_PARTICLES - particles.length);
    const count = Math.min(requestedCount, available);
    if (count <= 0) return;

    const next: Particle[] = [];
    for (let i = 0; i < count; i += 1) {
      const id = nextParticleId++;
      next.push({
        id,
        emoji,
        x: reaction ? 70 + Math.random() * 24 : 4 + Math.random() * 92,
        drift: reaction ? (Math.random() - 0.58) * 100 : (Math.random() - 0.5) * 220,
        duration: reaction ? 1800 + Math.random() * 700 : 2200 + Math.random() * 1200,
        delay: Math.random() * (reaction ? 240 : 180),
        size: reaction ? 22 + Math.random() * 9 : 22 + Math.random() * 18,
        rotate: (Math.random() - 0.5) * (reaction ? 48 : 120),
        rise: reaction ? 58 + Math.random() * 16 : 110,
        reaction
      });
      const timer = setTimeout(() => removeParticle(id), FALLBACK_REMOVE_MS);
      removeTimers.set(id, timer);
    }
    particles = [...particles, ...next];
  }

  function clearParticles() {
    for (const timer of removeTimers.values()) clearTimeout(timer);
    removeTimers.clear();
    particles = [];
  }

  function removeParticle(id: number) {
    const timer = removeTimers.get(id);
    if (timer) {
      clearTimeout(timer);
      removeTimers.delete(id);
    }
    if (!particles.some((particle) => particle.id === id)) return;
    particles = particles.filter((particle) => particle.id !== id);
  }
</script>

<div class="effects-overlay" aria-hidden="true">
  {#each particles as particle (particle.id)}
    <span
      class="particle"
      style={`left:${particle.x}%; --drift:${particle.drift}px; --duration:${particle.duration}ms; --delay:${particle.delay}ms; --size:${particle.size}px; --rotate:${particle.rotate}deg; --rise:-${particle.rise}vh;`}
      onanimationend={() => removeParticle(particle.id)}
    >
      {particle.emoji}
    </span>
  {/each}
</div>

<style>
  .effects-overlay {
    position: fixed;
    inset: 0;
    z-index: 2147483647;
    pointer-events: none;
    overflow: hidden;
  }

  .particle {
    position: absolute;
    bottom: -48px;
    display: inline-block;
    font-size: var(--size);
    line-height: 1;
    transform: translate3d(-50%, 0, 0);
    animation: effect-float var(--duration) cubic-bezier(0.23, 1, 0.32, 1) var(--delay) forwards;
    will-change: transform, opacity;
    user-select: none;
  }

  @keyframes effect-float {
    0% {
      opacity: 0;
      transform: translate3d(-50%, 0, 0) scale(0.92) rotate(0deg);
    }
    12% {
      opacity: 1;
    }
    100% {
      opacity: 0;
      transform: translate3d(calc(-50% + var(--drift)), var(--rise), 0) scale(1.08) rotate(var(--rotate));
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .particle {
      display: none;
      animation: none;
    }
  }
</style>
