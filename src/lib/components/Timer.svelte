<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { controlTimer, getConfig, getObsTimerUrl } from '../ipc';
  import type { TimerSnapshot } from '../ipc';

  const DEFAULT_DURATION_SEC = 300;
  const DEFAULT_WS_URL = 'ws://127.0.0.1:11180/timer';

  let minutes: number = $state(5);
  let seconds: number = $state(0);
  let timerUrl: string = $state('');
  let copied: boolean = $state(false);
  let busy: boolean = $state(false);
  let actionMsg: string = $state('');
  let actionError: string = $state('');
  let snapshot: TimerSnapshot = $state(defaultSnapshot());
  let nowMs: number = $state(Date.now());

  let socket: WebSocket | null = null;
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  let tickTimer: ReturnType<typeof setInterval> | null = null;
  let copiedTimer: ReturnType<typeof setTimeout> | null = null;
  let messageTimer: ReturnType<typeof setTimeout> | null = null;
  let destroyed = false;
  let wsUrl = DEFAULT_WS_URL;

  const inputDurationSec = $derived(Math.max(1, Math.trunc(minutes) * 60 + Math.trunc(seconds)));
  const rawDisplaySeconds = $derived.by(() => displayValue(snapshot, nowMs));
  const displaySeconds = $derived(
    snapshot.mode === 'countdown'
      ? Math.ceil(rawDisplaySeconds)
      : Math.floor(rawDisplaySeconds)
  );
  const finished = $derived(
    snapshot.state === 'finished' ||
      (snapshot.mode === 'countdown' && snapshot.state !== 'idle' && rawDisplaySeconds <= 0)
  );
  const stateLabel = $derived.by(() => {
    if (finished) return '終了';
    if (snapshot.state === 'running') return '実行中';
    if (snapshot.state === 'paused') return '一時停止';
    return '待機中';
  });

  onMount(() => {
    void initialize();
    tickTimer = setInterval(() => {
      nowMs = Date.now();
    }, 200);
  });

  onDestroy(() => {
    destroyed = true;
    if (reconnectTimer !== null) clearTimeout(reconnectTimer);
    if (tickTimer !== null) clearInterval(tickTimer);
    if (copiedTimer !== null) clearTimeout(copiedTimer);
    if (messageTimer !== null) clearTimeout(messageTimer);
    socket?.close();
    socket = null;
  });

  async function initialize() {
    try {
      const config = await getConfig();
      if (config?.timer) {
        setInputDuration(config.timer.defaultDurationSec || DEFAULT_DURATION_SEC);
        snapshot = {
          ...snapshot,
          mode: config.timer.mode === 'elapsed' ? 'elapsed' : 'countdown',
          durationSec: config.timer.defaultDurationSec || DEFAULT_DURATION_SEC,
        };
      }
    } catch {
      // Keep local defaults in browser-only mode.
    }

    try {
      timerUrl = await getObsTimerUrl();
      wsUrl = extractWsUrl(timerUrl);
    } catch {
      timerUrl = 'http://127.0.0.1:11180/?template=timer&ws=ws://127.0.0.1:11180/timer';
      wsUrl = DEFAULT_WS_URL;
    }
    connect();
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
        if (!isRecord(parsed)) return;
        const next = normalizeSnapshot(parsed);
        snapshot = next;
        if (next.durationSec > 0 && next.state === 'idle') setInputDuration(next.durationSec);
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

  async function onStart() {
    await runTimerAction('start', inputDurationSec);
  }

  async function onPause() {
    await runTimerAction('pause');
  }

  async function onResume() {
    await runTimerAction('resume');
  }

  async function onReset() {
    await runTimerAction('reset');
  }

  async function runTimerAction(action: string, durationSec?: number) {
    if (busy) return;
    busy = true;
    actionError = '';
    actionMsg = '';
    try {
      await controlTimer(action, durationSec);
      snapshot = optimisticSnapshot(action, durationSec);
      actionMsg = actionMessage(action);
      clearActionMsgSoon();
    } catch (e) {
      actionError = `操作に失敗しました: ${e instanceof Error ? e.message : String(e)}`;
    } finally {
      busy = false;
    }
  }

  function optimisticSnapshot(action: string, durationSec?: number): TimerSnapshot {
    const now = Date.now();
    const current = normalizeSnapshot(snapshot);

    if (action === 'start') {
      return {
        state: 'running',
        mode: current.mode === 'elapsed' ? 'elapsed' : 'countdown',
        durationSec: durationSec ?? inputDurationSec,
        baseElapsedSec: 0,
        runningSinceMs: now,
        updatedAt: now,
      };
    }

    if (action === 'pause') {
      return {
        ...current,
        state: 'paused',
        baseElapsedSec: Math.floor(elapsedSeconds(current, now)),
        runningSinceMs: 0,
        updatedAt: now,
      };
    }

    if (action === 'resume') {
      return {
        ...current,
        state: 'running',
        runningSinceMs: now,
        updatedAt: now,
      };
    }

    return {
      ...current,
      state: 'idle',
      baseElapsedSec: 0,
      runningSinceMs: 0,
      updatedAt: now,
    };
  }

  function actionMessage(action: string): string {
    if (action === 'start') return '開始しました';
    if (action === 'pause') return '一時停止しました';
    if (action === 'resume') return '再開しました';
    return 'リセットしました';
  }

  function clearActionMsgSoon() {
    if (messageTimer !== null) clearTimeout(messageTimer);
    messageTimer = setTimeout(() => {
      actionMsg = '';
      messageTimer = null;
    }, 2200);
  }

  async function onCopyUrl() {
    if (!timerUrl || typeof navigator === 'undefined' || !navigator.clipboard?.writeText) return;
    try {
      await navigator.clipboard.writeText(timerUrl);
      copied = true;
      if (copiedTimer !== null) clearTimeout(copiedTimer);
      copiedTimer = setTimeout(() => {
        copied = false;
        copiedTimer = null;
      }, 1500);
    } catch {
      copied = false;
    }
  }

  function onMinutesInput(event: Event) {
    minutes = clampInt(Number((event.currentTarget as HTMLInputElement).value), 0, 0, 71582788);
  }

  function onSecondsInput(event: Event) {
    seconds = clampInt(Number((event.currentTarget as HTMLInputElement).value), 0, 0, 59);
  }

  function setInputDuration(durationSec: number) {
    const total = clampInt(durationSec, DEFAULT_DURATION_SEC, 1, 4294967295);
    minutes = Math.floor(total / 60);
    seconds = total % 60;
  }

  function extractWsUrl(url: string): string {
    try {
      return new URL(url).searchParams.get('ws') || DEFAULT_WS_URL;
    } catch {
      return DEFAULT_WS_URL;
    }
  }

  function normalizeSnapshot(value: unknown): TimerSnapshot {
    if (!isRecord(value)) return defaultSnapshot();
    return {
      state: typeof value.state === 'string' && value.state ? value.state : 'idle',
      mode: value.mode === 'elapsed' ? 'elapsed' : 'countdown',
      durationSec: toNonNegativeInt(value.durationSec),
      baseElapsedSec: toNonNegativeInt(value.baseElapsedSec),
      runningSinceMs: toNonNegativeInt(value.runningSinceMs),
      updatedAt: toNonNegativeInt(value.updatedAt),
    };
  }

  function defaultSnapshot(): TimerSnapshot {
    return {
      state: 'idle',
      mode: 'countdown',
      durationSec: DEFAULT_DURATION_SEC,
      baseElapsedSec: 0,
      runningSinceMs: 0,
      updatedAt: 0,
    };
  }

  function displayValue(current: TimerSnapshot, now: number): number {
    const elapsed = elapsedSeconds(current, now);
    if (current.mode === 'elapsed') return elapsed;
    return Math.max(0, current.durationSec - elapsed);
  }

  function elapsedSeconds(current: TimerSnapshot, now: number): number {
    const base = Math.max(0, Number(current.baseElapsedSec) || 0);
    if (current.state !== 'running') return base;
    return base + Math.max(0, now - current.runningSinceMs) / 1000;
  }

  function formatDuration(value: number): string {
    const totalSeconds = Math.max(0, Math.floor(Number(value) || 0));
    const hours = Math.floor(totalSeconds / 3600);
    const mins = Math.floor((totalSeconds % 3600) / 60);
    const secs = totalSeconds % 60;
    if (hours > 0) return `${hours}:${pad2(mins)}:${pad2(secs)}`;
    return `${pad2(mins)}:${pad2(secs)}`;
  }

  function pad2(value: number): string {
    return String(value).padStart(2, '0');
  }

  function toNonNegativeInt(value: unknown): number {
    const n = typeof value === 'number' ? value : Number(value);
    if (!Number.isFinite(n) || n <= 0) return 0;
    return Math.floor(n);
  }

  function clampInt(value: unknown, fallback: number, min: number, max: number): number {
    const n = typeof value === 'number' ? value : Number(value);
    if (!Number.isFinite(n)) return fallback;
    return Math.trunc(Math.min(max, Math.max(min, n)));
  }

  function isRecord(value: unknown): value is Record<string, unknown> {
    return value !== null && typeof value === 'object';
  }
</script>

<div class="timer-panel">
  <section class="timer-display" class:finished>
    <div class="timer-meta">
      <span>{snapshot.mode === 'elapsed' ? '経過時間' : 'カウントダウン'}</span>
      <span>{stateLabel}</span>
    </div>
    <div class="timer-value">{formatDuration(displaySeconds)}</div>
  </section>

  <section class="timer-controls">
    <div class="duration-row">
      <label for="timer-minutes">時間</label>
      <input
        id="timer-minutes"
        type="number"
        min="0"
        step="1"
        value={minutes}
        oninput={onMinutesInput}
        class="num-input"
      />
      <span class="unit">分</span>
      <input
        id="timer-seconds"
        type="number"
        min="0"
        max="59"
        step="1"
        value={seconds}
        oninput={onSecondsInput}
        class="num-input"
      />
      <span class="unit">秒</span>
    </div>

    <div class="button-row">
      <button class="primary-btn" onclick={onStart} disabled={busy}>開始</button>
      {#if snapshot.state === 'running'}
        <button class="secondary-btn" onclick={onPause} disabled={busy}>一時停止</button>
      {:else}
        <button class="secondary-btn" onclick={onResume} disabled={busy || snapshot.state !== 'paused'}>再開</button>
      {/if}
      <button class="danger-btn" onclick={onReset} disabled={busy}>リセット</button>
      {#if actionMsg}<span class="status-ok">{actionMsg}</span>{/if}
      {#if actionError}<span class="status-error">{actionError}</span>{/if}
    </div>
  </section>

  <section class="obs-url-section">
    <div class="obs-label">OBSタイマーURL</div>
    <div class="obs-row">
      <input type="text" value={timerUrl} readonly class="obs-input" />
      <button class="copy-btn" class:copied onclick={onCopyUrl}>
        {copied ? 'コピー済' : 'コピー'}
      </button>
    </div>
  </section>
</div>

<style>
  .timer-panel {
    height: 100%;
    overflow-y: auto;
    padding: 14px 16px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .timer-display {
    padding: 16px;
    border-radius: 8px;
    border: 1px solid rgba(255,255,255,0.1);
    background: #171a1f;
  }

  .timer-display.finished {
    border-color: rgba(248,113,113,0.48);
    background: #24191b;
  }

  .timer-meta {
    display: flex;
    justify-content: space-between;
    gap: 10px;
    color: #9e9e9e;
    font-size: 12px;
    font-weight: 700;
  }

  .timer-value {
    margin-top: 8px;
    color: #fff;
    font-size: 64px;
    line-height: 1;
    font-weight: 900;
    font-variant-numeric: tabular-nums;
  }

  .timer-display.finished .timer-value {
    color: #ffb4b4;
  }

  .timer-controls,
  .obs-url-section {
    border-bottom: 1px solid rgba(255,255,255,0.07);
    padding-bottom: 12px;
  }

  .duration-row,
  .button-row,
  .obs-row {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }

  .duration-row label {
    min-width: 42px;
    color: #ccc;
    font-size: 13px;
  }

  .num-input,
  .obs-input {
    background: rgba(255,255,255,0.07);
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 4px;
    color: #e0e0e0;
    padding: 6px 8px;
    font-size: 13px;
  }

  .num-input {
    width: 90px;
  }

  .obs-input {
    flex: 1;
    min-width: min(100%, 260px);
    font-size: 12px;
  }

  .unit,
  .obs-label {
    color: #bdbdbd;
    font-size: 12px;
    font-weight: 600;
  }

  .button-row {
    margin-top: 10px;
  }

  .primary-btn,
  .secondary-btn,
  .danger-btn,
  .copy-btn {
    border: none;
    border-radius: 4px;
    cursor: pointer;
    color: #fff;
    font-size: 12px;
    font-weight: 700;
    padding: 7px 12px;
  }

  .primary-btn { background: #1976d2; }
  .secondary-btn { background: #37474f; }
  .danger-btn { background: #7f1d1d; }
  .copy-btn { background: #37474f; min-width: 72px; }
  .copy-btn.copied { background: #2e7d32; }

  .primary-btn:disabled,
  .secondary-btn:disabled,
  .danger-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .status-ok,
  .status-error {
    font-size: 12px;
  }

  .status-ok { color: #81c784; }
  .status-error { color: #ef9a9a; }

  @media (max-width: 620px) {
    .timer-value {
      font-size: 46px;
    }
  }
</style>
