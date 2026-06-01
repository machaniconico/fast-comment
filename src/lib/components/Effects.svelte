<script lang="ts">
  import { onDestroy } from 'svelte';
  import type { EffectsConfig } from '../ipc';
  import type { UiChatMessage } from '../types';
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
  }

  const MAX_PARTICLES = 200;
  const MAX_SEEN_IDS = 2000;
  const FALLBACK_REMOVE_MS = 4500;

  let { config }: Props = $props();
  let particles: Particle[] = $state([]);
  let nextParticleId = 1;
  let initialized = false;
  let lastReceivedCount = 0;
  const processedIds = new Set<string>();
  const removeTimers = new Map<number, ReturnType<typeof setTimeout>>();

  $effect(() => {
    const receivedCount = store.receivedCount;
    const messages = store.visibleMessages;
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
    for (const timer of removeTimers.values()) clearTimeout(timer);
    removeTimers.clear();
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

  function spawnParticles(emoji: string, requestedCount: number) {
    const available = Math.max(0, MAX_PARTICLES - particles.length);
    const count = Math.min(requestedCount, available);
    if (count <= 0) return;

    const next: Particle[] = [];
    for (let i = 0; i < count; i += 1) {
      const id = nextParticleId++;
      next.push({
        id,
        emoji,
        x: Math.random() * 100,
        drift: (Math.random() - 0.5) * 220,
        duration: 2200 + Math.random() * 1200,
        delay: Math.random() * 180,
        size: 22 + Math.random() * 18,
        rotate: (Math.random() - 0.5) * 120
      });
      const timer = setTimeout(() => removeParticle(id), FALLBACK_REMOVE_MS);
      removeTimers.set(id, timer);
    }
    particles = [...particles, ...next];
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
      style={`left:${particle.x}%; --drift:${particle.drift}px; --duration:${particle.duration}ms; --delay:${particle.delay}ms; --size:${particle.size}px; --rotate:${particle.rotate}deg;`}
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
    animation: effect-float var(--duration) ease-out var(--delay) forwards;
    will-change: transform, opacity;
    user-select: none;
  }

  @keyframes effect-float {
    0% {
      opacity: 0;
      transform: translate3d(-50%, 0, 0) scale(0.75) rotate(0deg);
    }
    12% {
      opacity: 1;
    }
    100% {
      opacity: 0;
      transform: translate3d(calc(-50% + var(--drift)), -110vh, 0) scale(1.15) rotate(var(--rotate));
    }
  }
</style>
