<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import type { Participant } from '../ipc';
  import {
    clearParticipants,
    getParticipants,
    pickNextParticipant,
    pickRandomParticipant,
    removeParticipant,
  } from '../ipc';

  let participants: Participant[] = $state([]);
  let actionError: string = $state('');
  let unlisten: (() => void) | null = null;
  let destroyed = false;

  const waiting = $derived(participants.filter((p) => !p.picked));
  const picked = $derived(participants.filter((p) => p.picked));

  onMount(async () => {
    try {
      const current = await getParticipants();
      if (!current) return;
      participants = current;
      const { listen } = await import('@tauri-apps/api/event');
      const fn = await listen<Participant[]>('participants-updated', (event) => {
        participants = event.payload;
      });
      if (destroyed) fn();
      else unlisten = fn;
    } catch (e) {
      actionError = `参加者一覧の取得に失敗しました: ${e instanceof Error ? e.message : String(e)}`;
    }
  });

  onDestroy(() => {
    destroyed = true;
    unlisten?.();
  });

  async function onPickNext() {
    actionError = '';
    try {
      await pickNextParticipant();
    } catch (e) {
      actionError = `選出に失敗しました: ${e instanceof Error ? e.message : String(e)}`;
    }
  }

  async function onPickRandom() {
    actionError = '';
    try {
      await pickRandomParticipant();
    } catch (e) {
      actionError = `ランダム選出に失敗しました: ${e instanceof Error ? e.message : String(e)}`;
    }
  }

  async function onRemove(p: Participant) {
    actionError = '';
    try {
      await removeParticipant(p.platform, p.userId);
    } catch (e) {
      actionError = `削除に失敗しました: ${e instanceof Error ? e.message : String(e)}`;
    }
  }

  async function onClear() {
    actionError = '';
    try {
      await clearParticipants();
    } catch (e) {
      actionError = `全消去に失敗しました: ${e instanceof Error ? e.message : String(e)}`;
    }
  }
</script>

<div class="participation">
  <div class="action-row">
    <button class="primary-btn" onclick={onPickNext} disabled={waiting.length === 0}>次の人</button>
    <button class="secondary-btn" onclick={onPickRandom} disabled={waiting.length === 0}>ランダム</button>
    <button class="danger-btn" onclick={onClear} disabled={participants.length === 0}>全消去</button>
  </div>

  {#if actionError}
    <p class="error">{actionError}</p>
  {/if}

  <section>
    <h2>待機中 <span>{waiting.length}</span></h2>
    <div class="participant-list" role="list" aria-label="待機中の参加者">
      {#if waiting.length === 0}
        <p class="empty">なし</p>
      {:else}
        {#each waiting as p (p.platform + ':' + p.userId)}
          <div class="participant-row" role="listitem">
            <span class="platform" class:twitch={p.platform === 'twitch'} class:youtube={p.platform === 'youtube'}>
              {p.platform}
            </span>
            <span class="name">{p.name}</span>
            <span class="user-id">{p.userId}</span>
            <button class="delete-btn" title="削除" aria-label="{p.name} を削除" onclick={() => onRemove(p)}>×</button>
          </div>
        {/each}
      {/if}
    </div>
  </section>

  <section>
    <h2>選出済み <span>{picked.length}</span></h2>
    <div class="participant-list" role="list" aria-label="選出済みの参加者">
      {#if picked.length === 0}
        <p class="empty">なし</p>
      {:else}
        {#each picked as p (p.platform + ':' + p.userId)}
          <div class="participant-row picked" role="listitem">
            <span class="platform" class:twitch={p.platform === 'twitch'} class:youtube={p.platform === 'youtube'}>
              {p.platform}
            </span>
            <span class="name">{p.name}</span>
            <span class="user-id">{p.userId}</span>
            <button class="delete-btn" title="削除" aria-label="{p.name} を削除" onclick={() => onRemove(p)}>×</button>
          </div>
        {/each}
      {/if}
    </div>
  </section>
</div>

<style>
  .participation {
    height: 100%;
    overflow-y: auto;
    padding: 12px 16px;
  }

  .action-row {
    display: flex;
    gap: 6px;
    align-items: center;
    margin-bottom: 10px;
    flex-wrap: wrap;
  }

  .primary-btn, .secondary-btn, .danger-btn {
    border: none;
    border-radius: 4px;
    cursor: pointer;
    font-size: 12px;
    padding: 6px 12px;
    font-weight: 600;
    color: #fff;
  }

  .primary-btn { background: #1976d2; }
  .secondary-btn { background: #37474f; }
  .danger-btn { background: #7f1d1d; }

  .primary-btn:disabled, .secondary-btn:disabled, .danger-btn:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }

  .error {
    color: #f44336;
    font-size: 12px;
    margin: 0 0 8px;
  }

  section {
    border-bottom: 1px solid rgba(255,255,255,0.07);
    padding-bottom: 12px;
    margin-bottom: 4px;
  }

  h2 {
    font-size: 12px;
    font-weight: 600;
    color: #9e9e9e;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    margin: 14px 0 6px;
  }

  h2 span {
    color: #e0e0e0;
    font-weight: 700;
    margin-left: 4px;
  }

  .participant-list {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .participant-row {
    display: flex;
    align-items: center;
    gap: 8px;
    min-height: 30px;
    padding: 4px 8px;
    background: rgba(255,255,255,0.05);
    border: 1px solid rgba(255,255,255,0.08);
    border-radius: 4px;
  }

  .participant-row.picked {
    opacity: 0.72;
  }

  .platform {
    width: 58px;
    flex-shrink: 0;
    color: #bbb;
    font-size: 11px;
    font-weight: 700;
    text-transform: uppercase;
  }

  .platform.twitch { color: #d4aaff; }
  .platform.youtube { color: #ff9999; }

  .name {
    min-width: 0;
    max-width: 220px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: #e0e0e0;
    font-weight: 600;
  }

  .user-id {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: #757575;
    font-size: 11px;
  }

  .delete-btn {
    margin-left: auto;
    flex-shrink: 0;
    background: none;
    border: none;
    color: #777;
    cursor: pointer;
    padding: 2px 6px;
    border-radius: 3px;
  }

  .delete-btn:hover {
    color: #f44336;
    background: rgba(244,67,54,0.08);
  }

  .empty {
    color: #666;
    font-size: 12px;
    margin: 4px 0 0;
  }
</style>
