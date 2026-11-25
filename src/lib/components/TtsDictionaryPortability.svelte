<script lang="ts">
  import {
    exportTtsDictionary, importTtsDictionary, importTtsDictionaryUrl,
    type TtsDictionaryCommit, type TtsDictionaryImportMode,
  } from '../ipc';
  import { configSync } from '../configSync.svelte';

  interface Props {
    disabled: boolean;
    hasDictionaryEdits: () => boolean;
    onImported: (commit: TtsDictionaryCommit) => void;
  }

  let { disabled, hasDictionaryEdits, onImported }: Props = $props();

  let mode = $state<TtsDictionaryImportMode>('append');
  let importText = $state('');
  let importUrl = $state('');
  let exportedJson = $state('');
  let exporting = $state(false);
  let message = $state('');
  let error = $state('');
  let result = $state<TtsDictionaryCommit | null>(null);
  let exportTextarea = $state<HTMLTextAreaElement | null>(null);

  const importing = $derived(configSync.importing === 'ttsDictionary');
  const importDisabled = $derived(disabled || !!configSync.importing);

  function clearStatus() {
    message = '';
    error = '';
    result = null;
  }

  async function onExport() {
    if (exporting || importDisabled) return;
    exporting = true;
    clearStatus();
    try {
      const json = await exportTtsDictionary();
      if (json === null) throw new Error('Tauri環境で開いてから再試行してください。');
      exportedJson = json;

      const blob = new Blob([json], { type: 'application/json;charset=utf-8' });
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement('a');
      anchor.href = url;
      anchor.download = `fast-comment-tts-dictionary-${new Date().toISOString().slice(0, 10)}.json`;
      anchor.rel = 'noopener';
      document.body.append(anchor);
      anchor.click();
      anchor.remove();
      window.setTimeout(() => URL.revokeObjectURL(url), 0);
      message = '読み上げ辞書を書き出しました。';
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      exporting = false;
    }
  }

  async function copyExport() {
    clearStatus();
    try {
      await navigator.clipboard.writeText(exportedJson);
      message = 'クリップボードにコピーしました。';
    } catch {
      exportTextarea?.focus();
      exportTextarea?.select();
      error = 'クリップボードにコピーできませんでした。選択したJSONを手動でコピーしてください。';
    }
  }

  async function runImport(load: (mode: TtsDictionaryImportMode) => Promise<TtsDictionaryCommit | null>) {
    if (importDisabled || exporting) return;
    if (mode === 'replace') {
      if (!window.confirm('現在の読み上げ辞書を取り込む内容で置き換えますか？')) return;
    } else if (hasDictionaryEdits()) {
      if (!window.confirm('保存済みの辞書に追加します。読み上げ辞書の未保存の編集が上書きされる場合があります。続行しますか？')) return;
    }

    configSync.importing = 'ttsDictionary';
    clearStatus();
    try {
      const commit = await load(mode);
      if (commit === null) throw new Error('Tauri環境で開いてから再試行してください。');
      onImported(commit);
      result = commit;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      configSync.importing = null;
    }
  }

  function onFileSelected(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    input.value = '';
    if (!file) return;

    void runImport(async (mode) => {
      const raw = await file.text().catch(() => {
        throw new Error('ファイルを読み込めませんでした。');
      });
      return importTtsDictionary(raw, mode);
    });
  }
</script>

<div class="portability">
  <div class="actions">
    <button type="button" onclick={onExport} disabled={exporting || importDisabled}>保存済み辞書を書き出す</button>
    <button type="button" onclick={copyExport} disabled={!exportedJson || exporting || importDisabled}>クリップボードにコピー</button>
  </div>
  {#if exportedJson}
    <textarea aria-label="書き出した読み上げ辞書JSON" readonly rows="4" value={exportedJson} bind:this={exportTextarea}></textarea>
  {/if}

  <fieldset disabled={exporting || importDisabled}>
    <div class="actions">
      <label for="tts-dictionary-import-mode">取り込み方式</label>
      <select id="tts-dictionary-import-mode" bind:value={mode}>
        <option value="append">追加</option>
        <option value="replace">置き換え</option>
      </select>

      <label for="tts-dictionary-file">ファイルから取り込み</label>
      <input id="tts-dictionary-file" type="file" accept=".json,application/json" onchange={onFileSelected} />
    </div>
    <label for="tts-dictionary-import-text">辞書JSON</label>
    <textarea id="tts-dictionary-import-text" rows="4" bind:value={importText}></textarea>
    <div class="actions">
      <button type="button" onclick={() => runImport(mode => importTtsDictionary(importText, mode))}>貼り付けて取り込む</button>
    </div>
    <div class="actions">
      <label for="tts-dictionary-import-url">辞書URL</label>
      <input id="tts-dictionary-import-url" type="url" placeholder="https://" bind:value={importUrl} />
      <button type="button" onclick={() => runImport(mode => importTtsDictionaryUrl(importUrl, mode))}>URLから取り込む</button>
    </div>
  </fieldset>

  <div role="status">
    {#if importing}
      <p>読み上げ辞書を取り込み中…</p>
    {:else if error}
      <p class="error">{error}</p>
    {:else if result}
      <p>追加 {result.added} 件、置き換え {result.replaced} 件、重複スキップ {result.skippedDuplicate} 件、辞書全体 {result.total} 件</p>
      {#if result.rejected.length}
        <p>取り込めなかった項目 {result.rejectedCount} 件</p>
        <ul>
          {#each result.rejected as { index, reason } (index)}
            <li>{index} 件目 — {reason}</li>
          {/each}
        </ul>
        {#if result.rejectedCount > result.rejected.length}
          <p>ほか {result.rejectedCount - result.rejected.length} 件</p>
        {/if}
      {/if}
    {:else if message}
      <p>{message}</p>
    {/if}
  </div>
</div>

<style>
  .portability, fieldset {
    display: grid;
    gap: 8px;
  }

  .portability {
    margin-top: 12px;
    font-size: 12px;
  }

  fieldset {
    min-width: 0;
    border: 0;
    margin: 0;
    padding: 0;
  }

  .actions {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 8px;
  }

  button {
    background: #1976d2;
    color: #fff;
    border: 0;
    border-radius: 4px;
    padding: 6px 10px;
    cursor: pointer;
  }

  button:disabled, fieldset:disabled button {
    opacity: 0.5;
    cursor: default;
  }

  textarea, input[type='url'], select {
    border: 1px solid rgba(255, 255, 255, 0.15);
    border-radius: 4px;
    padding: 6px;
    color: inherit;
    background: var(--bg-secondary, #252525);
  }

  textarea {
    box-sizing: border-box;
    width: 100%;
    resize: vertical;
    font-family: monospace;
  }

  input[type='url'] {
    flex: 1 1 16em;
  }

  p, ul {
    margin: 4px 0;
  }

  .error {
    color: #ef9a9a;
    white-space: pre-line;
  }
</style>
