<script lang="ts">
  import { onDestroy } from 'svelte';
  import type { UiChatMessage } from '../types';
  import type { WelcomeConfig } from '../ipc';
  import { ttsSpeakText } from '../ipc';
  import { store } from '../stores.svelte';

  interface Props {
    config: WelcomeConfig;
  }

  interface Banner {
    id: number;
    text: string;
  }

  interface Particle {
    id: number;
    emoji: string;
    x: number;
    y: number;
    drift: number;
    duration: number;
    delay: number;
    size: number;
    rotate: number;
  }

  const MAX_PARTICLES = 120;
  const MAX_WELCOME_COUNT = 48;
  const MAX_SEEN_IDS = 2000;
  const MAX_BANNERS = 4;
  const BANNER_TTL_MS = 3600;
  const FALLBACK_REMOVE_MS = 4500;

  let { config }: Props = $props();
  let banners: Banner[] = $state([]);
  let particles: Particle[] = $state([]);
  let nextBannerId = 1;
  let nextParticleId = 1;
  let initialized = false;
  let lastReceivedCount = 0;
  const processedIds = new Set<string>();
  const bannerTimers = new Map<number, ReturnType<typeof setTimeout>>();
  const particleTimers = new Map<number, ReturnType<typeof setTimeout>>();

  $effect(() => {
    const receivedCount = store.receivedCount;
    const messages = store.allMessages;
    const welcome = normalizeWelcome(config);

    if (!initialized) {
      markMessagesProcessed(messages);
      lastReceivedCount = receivedCount;
      initialized = true;
      return;
    }

    if (!welcome.enabled || receivedCount === lastReceivedCount) {
      lastReceivedCount = receivedCount;
      return;
    }

    for (const message of messages) {
      if (processedIds.has(message.id)) continue;
      processedIds.add(message.id);
      if (message.viewerSeq !== 1) continue;
      const greeting = buildGreeting(welcome.greeting, message.author.name);
      showBanner(greeting);
      spawnParticles(welcome.emoji, welcome.count);
      if (welcome.tts) {
        void ttsSpeakText(greeting).catch((e) => {
          console.warn('[welcome] ttsSpeakText failed', e);
        });
      }
    }
    pruneProcessedIds(messages);
    lastReceivedCount = receivedCount;
  });

  onDestroy(() => {
    for (const timer of bannerTimers.values()) clearTimeout(timer);
    bannerTimers.clear();
    for (const timer of particleTimers.values()) clearTimeout(timer);
    particleTimers.clear();
  });

  function normalizeWelcome(raw: WelcomeConfig) {
    const greeting = typeof raw.greeting === 'string' && raw.greeting.trim() !== ''
      ? raw.greeting
      : '{name} さん、いらっしゃい！';
    const emoji = typeof raw.emoji === 'string' && raw.emoji.trim() !== ''
      ? raw.emoji.trim()
      : '👋';
    return {
      enabled: raw.enabled === true,
      greeting,
      tts: raw.tts === true,
      emoji,
      count: boundedCount(raw.count)
    };
  }

  function boundedCount(value: unknown): number {
    const n = typeof value === 'number' ? value : Number(value);
    if (!Number.isFinite(n) || n <= 0) return 0;
    return Math.min(MAX_WELCOME_COUNT, Math.trunc(n));
  }

  function buildGreeting(template: string, name: string): string {
    return template.split('{name}').join(name);
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

  function showBanner(text: string) {
    const id = nextBannerId++;
    banners = [...banners.slice(-(MAX_BANNERS - 1)), { id, text }];
    const timer = setTimeout(() => removeBanner(id), BANNER_TTL_MS);
    bannerTimers.set(id, timer);
  }

  function removeBanner(id: number) {
    const timer = bannerTimers.get(id);
    if (timer) {
      clearTimeout(timer);
      bannerTimers.delete(id);
    }
    if (!banners.some((banner) => banner.id === id)) return;
    banners = banners.filter((banner) => banner.id !== id);
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
        x: 18 + Math.random() * 64,
        y: 38 + Math.random() * 34,
        drift: (Math.random() - 0.5) * 180,
        duration: 1900 + Math.random() * 1100,
        delay: Math.random() * 160,
        size: 24 + Math.random() * 18,
        rotate: (Math.random() - 0.5) * 120
      });
      const timer = setTimeout(() => removeParticle(id), FALLBACK_REMOVE_MS);
      particleTimers.set(id, timer);
    }
    particles = [...particles, ...next];
  }

  function removeParticle(id: number) {
    const timer = particleTimers.get(id);
    if (timer) {
      clearTimeout(timer);
      particleTimers.delete(id);
    }
    if (!particles.some((particle) => particle.id === id)) return;
    particles = particles.filter((particle) => particle.id !== id);
  }
</script>

<div class="welcome-overlay" aria-live="polite">
  <div class="banner-stack">
    {#each banners as banner (banner.id)}
      <div class="welcome-banner" role="status">
        {banner.text}
      </div>
    {/each}
  </div>

  <div class="particle-layer" aria-hidden="true">
    {#each particles as particle (particle.id)}
      <span
        class="particle"
        style={`left:${particle.x}%; top:${particle.y}%; --drift:${particle.drift}px; --duration:${particle.duration}ms; --delay:${particle.delay}ms; --size:${particle.size}px; --rotate:${particle.rotate}deg;`}
        onanimationend={() => removeParticle(particle.id)}
      >
        {particle.emoji}
      </span>
    {/each}
  </div>
</div>

<style>
  .welcome-overlay {
    position: fixed;
    inset: 0;
    z-index: 2147483646;
    pointer-events: none;
    overflow: hidden;
  }

  .banner-stack {
    position: absolute;
    top: 46px;
    left: 50%;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
    width: min(520px, calc(100vw - 24px));
    transform: translateX(-50%);
  }

  .welcome-banner {
    max-width: 100%;
    padding: 10px 16px;
    border: 1px solid rgba(125, 211, 252, 0.45);
    border-radius: 8px;
    background: rgba(12, 20, 28, 0.92);
    box-shadow: 0 12px 30px rgba(0, 0, 0, 0.28);
    color: #f8fafc;
    font-size: 18px;
    font-weight: 700;
    line-height: 1.35;
    text-align: center;
    overflow-wrap: anywhere;
    animation: welcome-banner-in 0.22s ease-out, welcome-banner-out 0.28s ease-in 3.25s forwards;
  }

  .particle-layer {
    position: absolute;
    inset: 0;
  }

  .particle {
    position: absolute;
    display: inline-block;
    font-size: var(--size);
    line-height: 1;
    transform: translate3d(-50%, -50%, 0);
    animation: welcome-pop var(--duration) ease-out var(--delay) forwards;
    user-select: none;
    will-change: transform, opacity;
  }

  @keyframes welcome-banner-in {
    from {
      opacity: 0;
      transform: translateY(-10px) scale(0.96);
    }
    to {
      opacity: 1;
      transform: translateY(0) scale(1);
    }
  }

  @keyframes welcome-banner-out {
    to {
      opacity: 0;
      transform: translateY(-8px) scale(0.98);
    }
  }

  @keyframes welcome-pop {
    0% {
      opacity: 0;
      transform: translate3d(-50%, -50%, 0) scale(0.65) rotate(0deg);
    }
    16% {
      opacity: 1;
    }
    100% {
      opacity: 0;
      transform: translate3d(calc(-50% + var(--drift)), -170px, 0) scale(1.18) rotate(var(--rotate));
    }
  }
</style>
