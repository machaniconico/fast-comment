<script lang="ts">
  import { onMount } from 'svelte';
  import {
    createBackupNow, getLastAutoBackupFailure, isTauri, listBackups,
    onAutoBackupStatus, openBackupDir, restoreBackup,
    type BackupEntry, type BackupSettings,
  } from '../ipc';

  let { settings = $bindable() }: { settings: BackupSettings } = $props();
  let entries = $state<BackupEntry[] | null>(null);
  let busy = $state(false);
  let status = $state('');
  let listError = $state('');
  let lastFailure = $state<string | null>(null);
  const available = isTauri();
  const date = new Intl.DateTimeFormat('ja-JP', { dateStyle: 'medium', timeStyle: 'medium' });

  function formatDate(timestamp: number): string {
    const value = new Date(timestamp);
    return Number.isNaN(value.getTime()) ? '日時不明' : date.format(value);
  }
  const size = new Intl.NumberFormat('ja-JP', { maximumFractionDigits: 1 });

  async function refresh() {
    try {
      entries = await listBackups();
      listError = '';
    } catch (error) {
      entries = null;
      listError = typeof error === 'string' ? error : 'バックアップの一覧を取得できませんでした。';
    }
  }

  onMount(() => {
    void refresh();
    let active = true;
    let unlisten: (() => void) | undefined;
    let received = false;
    void (async () => {
      try {
        unlisten = await onAutoBackupStatus((reason) => {
          received = true;
          if (active) lastFailure = reason;
        });
        if (!active) {
          unlisten();
          return;
        }
        const reason = await getLastAutoBackupFailure();
        if (active && !received) lastFailure = reason;
      } catch {
        // 一覧と作成の操作は引き続き利用できる。
      }
    })();
    return () => {
      active = false;
      unlisten?.();
    };
  });

  async function create() {
    busy = true;
    status = '';
    try {
      const created = await createBackupNow();
      await refresh();
      status = created ? 'バックアップを作成しました。' : '前回のバックアップから変更はありません。';
    } catch (error) {
      status = `バックアップを作成できませんでした。${typeof error === 'string' ? error : ''}`;
    } finally {
      busy = false;
    }
  }

  async function restore(entry: BackupEntry) {
    if (!window.confirm(
      `${formatDate(entry.createdAtMs)} のバックアップを復元しますか？\n\n` +
      '現在の状態を先にバックアップしてからアプリを再起動します。\n' +
      '事前バックアップの後、再起動までの変更は失われる可能性があります。\n' +
      '資格情報、ローカル API、Webhook の設定は復元の対象外です。\n' +
      'X 自動告知の台帳は戻らないため、現在の状態を引き継ぎます。\n' +
      '実行ファイルの指定や接続先の上書きは今の設定のままです。',
    )) return;
    busy = true;
    status = '復元を準備しています…';
    try {
      await restoreBackup(entry.fileName);
      status = 'アプリを再起動しています…';
    } catch {
      status = '復元の準備に失敗しました。';
      busy = false;
    }
  }

  async function openFolder() {
    try {
      await openBackupDir();
    } catch {
      status = 'バックアップフォルダを開けませんでした。';
    }
  }
</script>

<div class="backup-panel">
  <div class="row">
    <label><input type="checkbox" bind:checked={settings.enabled} /> 自動バックアップを有効にする</label>
    <label>保存する世代数
      <input type="number" min="1" max="50" step="1" value={settings.keep}
        onchange={(event) => { settings.keep = Math.min(50, Math.max(1, Math.trunc(event.currentTarget.valueAsNumber || 1)));
          event.currentTarget.value = String(settings.keep); }} />
    </label>
  </div>
  <p>有効化と世代数の変更は「設定を保存」で反映されます。</p>
  <div class="row">
    <button type="button" onclick={create} disabled={!available || busy}>今すぐ作成</button>
    <button type="button" onclick={openFolder} disabled={!available || busy}>フォルダを開く</button>
  </div>
  {#if status}<p role="status">{status}</p>{/if}
  {#if lastFailure}<p role="status">自動バックアップに失敗しました。{lastFailure}</p>{/if}
  {#if listError}
    <p role="status">{listError}</p>
  {:else if entries?.length}
    <ul aria-label="バックアップ一覧">
      {#each entries as entry (entry.fileName)}
        <li class="row">
          <span>{formatDate(entry.createdAtMs)}</span>
          <span>{size.format(entry.size / 1024)} KB</span>
          <button type="button" onclick={() => restore(entry)} disabled={busy}
            aria-label={`${formatDate(entry.createdAtMs)} のバックアップを復元`}>復元…</button>
        </li>
      {/each}
    </ul>
  {:else if entries}
    <p>保存済みのバックアップはありません。</p>
  {/if}
</div>

<style>
  .backup-panel {
    display: grid;
    gap: 8px;
  }

  .row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 10px;
  }

  label, p, li {
    font-size: 12px;
  }

  p {
    margin: 0;
  }

  input[type='number'] {
    width: 5em;
    margin-left: 4px;
  }

  button {
    border: 1px solid var(--line, #616161);
    border-radius: 4px;
    padding: 4px 10px;
    background: var(--surface-raised, #37474f);
    color: var(--text, #fff);
    cursor: pointer;
  }

  button:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  ul {
    display: grid;
    gap: 4px;
    max-height: 16rem;
    overflow: auto;
    margin: 0;
    padding: 0;
    list-style: none;
  }
</style>
