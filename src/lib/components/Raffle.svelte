<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import {
    clearParticipants,
    getParticipants,
    pickRandomParticipant,
    ttsSpeakText,
  } from '../ipc';
  import type { Participant } from '../ipc';

  type RaffleMode = 'exclude' | 'all';

  interface HistoryEntry {
    name: string;
    platform: string;
    time: Date;
  }

  const MAX_HISTORY = 50;

  let participants: Participant[] = $state([]);
  let mode: RaffleMode = $state('exclude');
  let spinning: boolean = $state(false);
  let displayName: string = $state('');
  let winner: Participant | null = $state(null);
  let actionError: string = $state('');
  let actionMessage: string = $state('');
  let history: HistoryEntry[] = $state([]);
  let unlisten: (() => void) | null = null;
  let animationTimer: ReturnType<typeof setTimeout> | null = null;
  let messageTimer: ReturnType<typeof setTimeout> | null = null;

  const unpicked = $derived(participants.filter((p) => !p.picked));
  const drawDisabled = $derived(
    spinning || participants.length === 0 || (mode === 'exclude' && unpicked.length === 0)
  );

  onMount(async () => {
    try {
      const current = await getParticipants();
      if (!current) return;
      participants = current;

      const { listen } = await import('@tauri-apps/api/event');
      unlisten = await listen<Participant[]>('participants-updated', (event) => {
        participants = event.payload;
      });
    } catch (e) {
      actionError = `参加者一覧の取得に失敗しました: ${formatError(e)}`;
    }
  });

  onDestroy(() => {
    unlisten?.();
    clearAnimationTimer();
    clearMessageTimer();
  });

  async function onDraw() {
    if (drawDisabled) return;

    actionError = '';
    actionMessage = '';
    winner = null;

    const poolSnapshot = mode === 'all' ? [...participants] : [...unpicked];
    if (poolSnapshot.length === 0) {
      actionError = mode === 'exclude'
        ? '未抽選の参加者がいません。参加者をリセットすると再抽選できます。'
        : '参加者がいません。';
      return;
    }

    spinning = true;

    try {
      const selected = mode === 'all'
        ? pickFrontendWinner(poolSnapshot)
        : await pickBackendWinner(poolSnapshot);

      if (!selected) {
        throw new Error('抽選できる参加者が見つかりませんでした');
      }

      await animateToWinner(poolSnapshot, selected);
      winner = selected;
      displayName = selected.name;
      appendHistory(selected);
    } catch (e) {
      actionError = `抽選に失敗しました: ${formatError(e)}`;
      displayName = '';
    } finally {
      spinning = false;
    }
  }

  async function pickBackendWinner(beforeUnpicked: Participant[]): Promise<Participant | null> {
    const beforeKeys = new Set(beforeUnpicked.map(participantKey));
    const picked = await pickRandomParticipant();

    if (isParticipant(picked)) {
      void refreshParticipants();
      return picked;
    }

    const after = await getParticipants();
    if (after) {
      participants = after;
      const newlyPicked = after.find((p) => p.picked && beforeKeys.has(participantKey(p)));
      if (newlyPicked) return newlyPicked;
    }

    return null;
  }

  function pickFrontendWinner(pool: Participant[]): Participant | null {
    if (pool.length === 0) return null;
    return pool[Math.floor(Math.random() * pool.length)] ?? null;
  }

  async function refreshParticipants() {
    try {
      const current = await getParticipants();
      if (current) participants = current;
    } catch {
      // The participants-updated event is the primary sync path.
    }
  }

  function animateToWinner(pool: Participant[], selected: Participant): Promise<void> {
    clearAnimationTimer();

    const reel = ensureWinnerInPool(pool, selected);
    const totalSteps = Math.max(26, Math.min(56, reel.length * 6 + 20));
    let step = 0;

    return new Promise((resolve) => {
      const tick = () => {
        if (step >= totalSteps) {
          animationTimer = null;
          displayName = selected.name;
          resolve();
          return;
        }

        if (step === totalSteps - 1) {
          displayName = selected.name;
        } else {
          displayName = reel[step % reel.length]?.name ?? selected.name;
        }

        const progress = totalSteps <= 1 ? 1 : step / (totalSteps - 1);
        const delay = Math.round(50 + Math.pow(progress, 2.25) * 560);
        step += 1;
        animationTimer = setTimeout(tick, delay);
      };

      tick();
    });
  }

  function ensureWinnerInPool(pool: Participant[], selected: Participant): Participant[] {
    const reel = [...pool];
    if (!reel.some((p) => isSameParticipant(p, selected))) {
      reel.push(selected);
    }
    return reel.length > 0 ? reel : [selected];
  }

  function appendHistory(selected: Participant) {
    history = [
      ...history,
      { name: selected.name, platform: selected.platform, time: new Date() },
    ].slice(-MAX_HISTORY);
  }

  async function copyWinnerName() {
    if (!winner || typeof navigator === 'undefined' || !navigator.clipboard?.writeText) return;

    actionError = '';
    try {
      await navigator.clipboard.writeText(winner.name);
      showActionMessage('名前をコピーしました');
    } catch (e) {
      actionError = `コピーに失敗しました: ${formatError(e)}`;
    }
  }

  function speakWinnerNow() {
    if (!winner) return;

    const text = winner.name.trim();
    if (!text) return;

    if (typeof window !== 'undefined' && (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__) {
      void ttsSpeakText(text).catch(() => speakWithWebSpeech(text));
      return;
    }

    speakWithWebSpeech(text);
  }

  function speakWithWebSpeech(text: string) {
    if (typeof window === 'undefined' || !('speechSynthesis' in window)) return;

    try {
      window.speechSynthesis.speak(new SpeechSynthesisUtterance(text));
    } catch {
      // Winner TTS is best-effort, matching the comment context-menu action.
    }
  }

  async function onClearParticipants() {
    if (spinning || participants.length === 0) return;
    if (typeof window !== 'undefined' && !window.confirm('参加者をリセットしますか？')) return;

    actionError = '';
    actionMessage = '';

    try {
      await clearParticipants();
      participants = [];
      winner = null;
      displayName = '';
      showActionMessage('参加者をリセットしました');
    } catch (e) {
      actionError = `参加者のリセットに失敗しました: ${formatError(e)}`;
    }
  }

  function showActionMessage(message: string) {
    actionMessage = message;
    clearMessageTimer();
    messageTimer = setTimeout(() => {
      actionMessage = '';
      messageTimer = null;
    }, 2200);
  }

  function clearAnimationTimer() {
    if (animationTimer) {
      clearTimeout(animationTimer);
      animationTimer = null;
    }
  }

  function clearMessageTimer() {
    if (messageTimer) {
      clearTimeout(messageTimer);
      messageTimer = null;
    }
  }

  function participantKey(p: Participant): string {
    return `${p.platform}:${p.userId}`;
  }

  function isSameParticipant(a: Participant, b: Participant): boolean {
    return a.platform === b.platform && a.userId === b.userId;
  }

  function isParticipant(value: Participant | null): value is Participant {
    return !!value
      && typeof value.platform === 'string'
      && typeof value.userId === 'string'
      && typeof value.name === 'string'
      && typeof value.picked === 'boolean';
  }

  function formatTime(time: Date): string {
    const h = time.getHours().toString().padStart(2, '0');
    const m = time.getMinutes().toString().padStart(2, '0');
    const s = time.getSeconds().toString().padStart(2, '0');
    return `${h}:${m}:${s}`;
  }

  function formatError(e: unknown): string {
    return e instanceof Error ? e.message : String(e);
  }
</script>

<section class="raffle" aria-label="視聴者抽選ルーレット">
  <header class="raffle-header">
    <div>
      <h2>抽選ルーレット</h2>
      <p class="summary">
        参加者 <strong>{participants.length}</strong>
        <span aria-hidden="true">/</span>
        未抽選 <strong>{unpicked.length}</strong>
      </p>
    </div>
  </header>

  <fieldset class="mode-toggle" disabled={spinning}>
    <legend>抽選モード</legend>
    <label class:active={mode === 'exclude'}>
      <input
        type="radio"
        name="raffle-mode"
        value="exclude"
        checked={mode === 'exclude'}
        onchange={() => { mode = 'exclude'; }}
      />
      <span>母集団から除外(picked, 既定)</span>
    </label>
    <label class:active={mode === 'all'}>
      <input
        type="radio"
        name="raffle-mode"
        value="all"
        checked={mode === 'all'}
        onchange={() => { mode = 'all'; }}
      />
      <span>毎回全員から(除外しない)</span>
    </label>
  </fieldset>

  <div class="roulette-stage" aria-live="polite" aria-atomic="true">
    <div class="slot-window" class:spinning class:winner={winner !== null && !spinning}>
      {#if displayName}
        <span>{displayName}</span>
      {:else}
        <span class="placeholder">待機中</span>
      {/if}
    </div>
  </div>

  <div class="action-row">
    <button
      type="button"
      class="primary-btn"
      onclick={onDraw}
      disabled={drawDisabled}
      aria-label="抽選を開始"
    >
      {spinning ? '抽選中...' : '抽選する'}
    </button>
    <button
      type="button"
      class="danger-btn"
      onclick={onClearParticipants}
      disabled={spinning || participants.length === 0}
      aria-label="参加者をリセット"
    >
      参加者をリセット
    </button>
  </div>

  {#if participants.length === 0}
    <p class="empty-message">参加者がいません(参加型を有効化してキーワードで募集してください)</p>
  {:else if mode === 'exclude' && unpicked.length === 0}
    <p class="empty-message">未抽選の参加者がいません(参加者をリセットすると再抽選できます)</p>
  {/if}

  {#if actionError}
    <p class="error" role="alert">{actionError}</p>
  {/if}
  {#if actionMessage}
    <p class="notice" role="status">{actionMessage}</p>
  {/if}

  {#if winner}
    <section class="winner-panel" aria-label="当選者">
      <p class="winner-label">当選</p>
      <p class="winner-name">{winner.name}</p>
      <p class="winner-meta">{winner.platform}</p>
      <div class="winner-actions">
        <button type="button" class="secondary-btn" onclick={copyWinnerName} aria-label="当選者の名前をコピー">
          名前をコピー
        </button>
        <button type="button" class="secondary-btn" onclick={speakWinnerNow} aria-label="当選者の名前を今すぐ読み上げ">
          今すぐ読み上げ
        </button>
      </div>
    </section>
  {/if}

  <section class="participant-section">
    <div class="section-title">
      <h3>参加者</h3>
      <span>{participants.length}</span>
    </div>
    <div class="participant-list" role="list" aria-label="参加者一覧">
      {#if participants.length === 0}
        <p class="empty">なし</p>
      {:else}
        {#each participants as p (participantKey(p))}
          <div class="participant-row" class:picked={p.picked} role="listitem">
            <span class="platform" class:twitch={p.platform === 'twitch'} class:youtube={p.platform === 'youtube'}>
              {p.platform}
            </span>
            <span class="name">{p.name}</span>
            {#if p.picked}
              <span class="picked-badge">選出済み</span>
            {/if}
          </div>
        {/each}
      {/if}
    </div>
  </section>

  <section class="history-section">
    <div class="section-title">
      <h3>履歴</h3>
      <span>{history.length}</span>
    </div>
    <div class="history-list" role="list" aria-label="抽選履歴">
      {#if history.length === 0}
        <p class="empty">まだ抽選していません</p>
      {:else}
        {#each history as entry, index (`${entry.platform}:${entry.name}:${entry.time.getTime()}:${index}`)}
          <div class="history-row" role="listitem">
            <time datetime={entry.time.toISOString()}>{formatTime(entry.time)}</time>
            <span class="history-name">{entry.name}</span>
            <span class="history-platform">{entry.platform}</span>
          </div>
        {/each}
      {/if}
    </div>
  </section>
</section>

<style>
  .raffle {
    height: 100%;
    overflow-y: auto;
    padding: 14px 16px 18px;
    background:
      linear-gradient(180deg, rgba(30, 36, 42, 0.95), rgba(18, 18, 18, 1) 280px),
      #121212;
  }

  .raffle-header {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 12px;
  }

  h2, h3, p {
    margin: 0;
  }

  h2 {
    color: #fff;
    font-size: 18px;
    line-height: 1.25;
  }

  .summary {
    margin-top: 4px;
    color: #9e9e9e;
    font-size: 12px;
  }

  .summary strong {
    color: #e0e0e0;
    font-weight: 700;
  }

  .mode-toggle {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
    padding: 0;
    margin: 0 0 14px;
    border: none;
  }

  .mode-toggle legend {
    width: 100%;
    margin-bottom: 6px;
    color: #9e9e9e;
    font-size: 12px;
    font-weight: 700;
  }

  .mode-toggle label {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    min-height: 30px;
    padding: 5px 9px;
    border: 1px solid rgba(255, 255, 255, 0.1);
    border-radius: 4px;
    background: rgba(255, 255, 255, 0.05);
    color: #cfcfcf;
    cursor: pointer;
    font-size: 12px;
  }

  .mode-toggle label.active {
    border-color: rgba(64, 196, 255, 0.5);
    background: rgba(64, 196, 255, 0.14);
    color: #e8f7ff;
  }

  .mode-toggle input {
    margin: 0;
  }

  .mode-toggle:disabled label {
    opacity: 0.58;
    cursor: not-allowed;
  }

  .roulette-stage {
    display: flex;
    align-items: center;
    justify-content: center;
    min-height: 132px;
    margin-bottom: 12px;
    border: 1px solid rgba(255, 255, 255, 0.09);
    border-radius: 8px;
    background: #17191c;
    box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.03);
  }

  .slot-window {
    display: flex;
    align-items: center;
    justify-content: center;
    width: min(560px, 100%);
    min-height: 76px;
    padding: 12px 18px;
    border-top: 1px solid rgba(255, 255, 255, 0.12);
    border-bottom: 1px solid rgba(255, 255, 255, 0.12);
    color: #f5f5f5;
    font-size: clamp(24px, 5vw, 44px);
    font-weight: 800;
    line-height: 1.1;
    text-align: center;
    overflow-wrap: anywhere;
  }

  .slot-window.spinning {
    color: #dff3ff;
    text-shadow: 0 0 16px rgba(64, 196, 255, 0.45);
  }

  .slot-window.winner {
    animation: winner-pop 780ms ease-out;
    color: #fff;
    text-shadow:
      0 0 18px rgba(255, 214, 102, 0.75),
      0 0 34px rgba(64, 196, 255, 0.38);
  }

  .placeholder {
    color: #626b72;
    font-size: 18px;
    font-weight: 700;
  }

  .action-row,
  .winner-actions {
    display: flex;
    gap: 8px;
    align-items: center;
    flex-wrap: wrap;
  }

  .action-row {
    margin-bottom: 10px;
  }

  .primary-btn,
  .secondary-btn,
  .danger-btn {
    min-height: 32px;
    border: none;
    border-radius: 4px;
    color: #fff;
    cursor: pointer;
    font-size: 12px;
    font-weight: 700;
    padding: 7px 12px;
    white-space: nowrap;
  }

  .primary-btn {
    background: #1976d2;
  }

  .secondary-btn {
    background: #37474f;
  }

  .danger-btn {
    background: #7f1d1d;
  }

  .primary-btn:disabled,
  .secondary-btn:disabled,
  .danger-btn:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }

  .empty-message,
  .error,
  .notice {
    margin: 8px 0 0;
    font-size: 12px;
  }

  .empty-message {
    color: #9e9e9e;
  }

  .error {
    color: #ef9a9a;
  }

  .notice {
    color: #81c784;
  }

  .winner-panel {
    margin: 14px 0;
    padding: 12px;
    border: 1px solid rgba(255, 214, 102, 0.28);
    border-radius: 8px;
    background: rgba(255, 214, 102, 0.08);
  }

  .winner-label {
    color: #ffd666;
    font-size: 12px;
    font-weight: 800;
    letter-spacing: 0.08em;
  }

  .winner-name {
    margin-top: 4px;
    color: #fff;
    font-size: 24px;
    font-weight: 800;
    line-height: 1.2;
    overflow-wrap: anywhere;
  }

  .winner-meta {
    margin: 2px 0 10px;
    color: #b6bec6;
    font-size: 12px;
  }

  .participant-section,
  .history-section {
    margin-top: 16px;
    padding-top: 12px;
    border-top: 1px solid rgba(255, 255, 255, 0.07);
  }

  .section-title {
    display: flex;
    align-items: baseline;
    gap: 6px;
    margin-bottom: 8px;
  }

  .section-title h3 {
    color: #9e9e9e;
    font-size: 12px;
    font-weight: 700;
    letter-spacing: 0.05em;
    text-transform: uppercase;
  }

  .section-title span {
    color: #e0e0e0;
    font-size: 12px;
    font-weight: 700;
  }

  .participant-list,
  .history-list {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .participant-row,
  .history-row {
    display: flex;
    align-items: center;
    gap: 8px;
    min-height: 30px;
    padding: 4px 8px;
    border: 1px solid rgba(255, 255, 255, 0.08);
    border-radius: 4px;
    background: rgba(255, 255, 255, 0.05);
  }

  .participant-row.picked {
    opacity: 0.68;
  }

  .platform,
  .history-platform {
    width: 58px;
    flex-shrink: 0;
    color: #bbb;
    font-size: 11px;
    font-weight: 700;
    text-transform: uppercase;
  }

  .platform.twitch {
    color: #d4aaff;
  }

  .platform.youtube {
    color: #ff9999;
  }

  .name,
  .history-name {
    min-width: 0;
    overflow: hidden;
    color: #e0e0e0;
    font-weight: 700;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .picked-badge {
    margin-left: auto;
    flex-shrink: 0;
    padding: 2px 6px;
    border-radius: 4px;
    background: rgba(255, 255, 255, 0.08);
    color: #9e9e9e;
    font-size: 11px;
    font-weight: 700;
  }

  .history-row time {
    width: 72px;
    flex-shrink: 0;
    color: #8f989f;
    font-size: 12px;
    font-variant-numeric: tabular-nums;
  }

  .history-name {
    flex: 1;
  }

  .history-platform {
    width: auto;
    margin-left: auto;
  }

  .empty {
    color: #666;
    font-size: 12px;
    margin: 2px 0 0;
  }

  @keyframes winner-pop {
    0% {
      transform: scale(0.92);
      opacity: 0.78;
    }
    55% {
      transform: scale(1.08);
      opacity: 1;
    }
    100% {
      transform: scale(1);
      opacity: 1;
    }
  }

  @media (max-width: 620px) {
    .raffle {
      padding: 12px;
    }

    .roulette-stage {
      min-height: 112px;
    }

    .slot-window {
      min-height: 66px;
      padding: 10px 12px;
    }
  }
</style>
