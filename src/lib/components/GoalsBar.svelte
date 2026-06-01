<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { getObsGoalsUrl } from '../ipc';
  import type { GoalsSnapshot, StatsSnapshot } from '../ipc';

  type MetricKey = keyof GoalsSnapshot;

  interface Metric {
    key: MetricKey;
    label: string;
    className: string;
  }

  interface GoalCard {
    key: MetricKey;
    label: string;
    className: string;
    current: number;
    target: number;
    percent: number;
    width: number;
    reached: boolean;
  }

  const METRICS: Metric[] = [
    { key: 'likes', label: 'LIKES', className: 'likes' },
    { key: 'comments', label: 'COMMENTS', className: 'comments' },
    { key: 'viewers', label: 'VIEWERS', className: 'viewers' },
  ];

  const DEFAULT_WS_URL = 'ws://127.0.0.1:11180/stats';
  const nf = new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 });

  let socket: WebSocket | null = null;
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  let destroyed = false;
  let wsUrl = DEFAULT_WS_URL;
  let snapshot: StatsSnapshot | null = $state(null);

  const cards = $derived.by((): GoalCard[] => {
    if (!snapshot) return [];

    const goals = snapshot.goals ?? { comments: 0, viewers: 0, likes: 0 };
    const nextCards: GoalCard[] = [];

    for (const metric of METRICS) {
      if (metric.key === 'likes' && snapshot.likesAvailable === false) continue;

      const target = toCount(goals[metric.key]);
      if (target === 0) continue;

      const current = toCount(snapshot[metric.key]);
      const percent = Math.floor((current * 100) / target);
      const width = Math.min(100, percent);
      nextCards.push({
        key: metric.key,
        label: metric.label,
        className: metric.className,
        current,
        target,
        percent,
        width,
        reached: percent >= 100,
      });
    }

    return nextCards;
  });

  onMount(() => {
    void start();
  });

  onDestroy(() => {
    destroyed = true;
    if (reconnectTimer !== null) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    socket?.close();
    socket = null;
  });

  async function start() {
    wsUrl = extractWsUrl(await getObsGoalsUrl());
    connect();
  }

  function extractWsUrl(url: string): string {
    try {
      return new URL(url).searchParams.get('ws') || DEFAULT_WS_URL;
    } catch {
      return DEFAULT_WS_URL;
    }
  }

  function connect() {
    if (destroyed) return;

    try {
      socket = new WebSocket(wsUrl);
    } catch {
      scheduleReconnect();
      return;
    }

    socket.addEventListener('message', (event) => {
      try {
        const parsed = JSON.parse(String(event.data)) as unknown;
        if (parsed && typeof parsed === 'object') snapshot = parsed as StatsSnapshot;
      } catch {
        // Ignore malformed frames and wait for the next snapshot.
      }
    });

    socket.addEventListener('close', scheduleReconnect);
    socket.addEventListener('error', () => {
      try {
        socket?.close();
      } catch {
        // Already closed.
      }
    });
  }

  function scheduleReconnect() {
    if (destroyed || reconnectTimer !== null) return;
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      connect();
    }, 1500);
  }

  function toCount(value: unknown): number {
    const n = Number(value);
    if (!Number.isFinite(n) || n <= 0) return 0;
    return Math.floor(n);
  }

  function formatCount(value: number): string {
    return nf.format(value);
  }
</script>

{#if cards.length > 0}
  <div class="goals-bar" aria-label="配信目標">
    {#each cards as card (card.key)}
      <section class="goal-card {card.className}" class:reached={card.reached}>
        <div class="goal-head">
          <div class="goal-title">{card.label}</div>
          <div class="goal-percent">{card.percent}%</div>
        </div>
        <div class="goal-value">
          <span class="goal-current">{formatCount(card.current)}</span>
          <span class="goal-target">/ {formatCount(card.target)}</span>
        </div>
        <div class="goal-track">
          <div class="goal-fill" style={`width: ${card.width}%`}></div>
        </div>
      </section>
    {/each}
  </div>
{/if}

<style>
  .goals-bar {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
    gap: 6px;
    padding: 6px 8px;
    background: #171a1f;
    border-bottom: 1px solid rgba(255,255,255,0.08);
    flex-shrink: 0;
  }

  .goal-card {
    min-width: 0;
    padding: 6px 8px;
    border-radius: 6px;
    background: rgba(255,255,255,0.055);
    border: 1px solid rgba(255,255,255,0.08);
  }

  .goal-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    min-width: 0;
  }

  .goal-title {
    color: #aeb6c2;
    font-size: 10px;
    font-weight: 800;
    letter-spacing: 0;
  }

  .goal-percent {
    color: #f1f5f9;
    font-size: 11px;
    font-weight: 800;
    white-space: nowrap;
  }

  .goal-value {
    display: flex;
    align-items: baseline;
    gap: 4px;
    min-width: 0;
    margin-top: 2px;
  }

  .goal-current {
    color: #fff;
    font-size: 15px;
    font-weight: 800;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .goal-target {
    color: #8b949e;
    font-size: 11px;
    white-space: nowrap;
    flex-shrink: 0;
  }

  .goal-track {
    height: 5px;
    margin-top: 5px;
    overflow: hidden;
    border-radius: 999px;
    background: rgba(255,255,255,0.1);
  }

  .goal-fill {
    height: 100%;
    border-radius: inherit;
    transition: width 0.25s ease;
  }

  .likes .goal-fill { background: #f6c453; }
  .comments .goal-fill { background: #58a6ff; }
  .viewers .goal-fill { background: #56d364; }

  .goal-card.reached {
    border-color: rgba(255,255,255,0.2);
    background: rgba(255,255,255,0.09);
  }
</style>
