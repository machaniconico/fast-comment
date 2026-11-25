<script lang="ts">
  import { exportConfig, importConfig, type ExportedConfig } from '../ipc';
  import { configReloadMessage, configSync } from '../configSync.svelte';
  import { importMessage } from '../configPortability';
  import { theme, type AppearanceSnapshot } from '../theme.svelte';

  // v0.8.0 までの版は schema が違うファイルを拒否する。資格情報の欄が無い書き出しを
  // 旧版が読み込んで今の資格情報を空にしないよう、書き出しの schema を変えた。
  const SCHEMA = 'fast-comment/settings-v2';
  const LEGACY_SCHEMA = 'fast-comment/settings';
  // import_config の型エラーは Rust が固定フィールド名だけで組み立てる。
  const configTypeError = /^設定の形式が不正です \(項目 (?:\.|[a-zA-Z][a-zA-Z0-9]*(?:\.[a-zA-Z][a-zA-Z0-9]*)*)\)$/;

  interface SettingsBundle {
    schema: typeof SCHEMA;
    version: 2;
    exportedAt: string;
    config: ExportedConfig;
    appearance: AppearanceSnapshot;
  }

  interface Props {
    onImported?: () => Promise<boolean>;
    onReload?: () => Promise<boolean> | undefined;
    disabled?: boolean;
    reloading?: boolean;
  }

  let { onImported, onReload, disabled = false, reloading = false }: Props = $props();

  let exportedJson: string = $state('');
  let importText: string = $state('');
  let status: { kind: 'success' | 'error'; message: string } | null = $state(null);
  let exporting: boolean = $state(false);
  const importDisabled = $derived(disabled || !!configSync.importing || configSync.reloadRequired);
  let exportTextarea: HTMLTextAreaElement | null = null;
  let importFileInput: HTMLInputElement | null = null;

  function isRecord(value: unknown): value is Record<string, unknown> {
    return typeof value === 'object' && value !== null;
  }

  function isAppearanceSnapshot(value: unknown): value is AppearanceSnapshot {
    return (
      isRecord(value) &&
      (value.theme === 'dark' || value.theme === 'light' || value.theme === 'auto') &&
      (value.fontSize === 's' || value.fontSize === 'm' || value.fontSize === 'l') &&
      (value.density === 'comfortable' || value.density === 'compact') &&
      (value.viewerDisplay === undefined || value.viewerDisplay === 'rotate' || value.viewerDisplay === 'totalRotate' || value.viewerDisplay === 'all') && // totalRotate は廃止済み。旧ファイルを弾かないため受理のみ
      (value.showViewerBadges === undefined || typeof value.showViewerBadges === 'boolean') &&
      (value.showCommentMilestones === undefined || typeof value.showCommentMilestones === 'boolean')
    );
  }

  function makeFileName(): string {
    const stamp = new Date().toISOString().slice(0, 10);
    return `fast-comment-settings-${stamp}.json`;
  }

  function showSuccess(message: string): void {
    status = { kind: 'success', message };
  }

  function showError(message: string): void {
    status = { kind: 'error', message };
  }

  function buildBundle(config: ExportedConfig): SettingsBundle {
    return {
      schema: SCHEMA,
      version: 2,
      exportedAt: new Date().toISOString(),
      config,
      appearance: theme.getSnapshot(),
    };
  }

  function downloadJson(json: string): void {
    if (typeof document === 'undefined' || typeof URL === 'undefined') return;

    const blob = new Blob([json], { type: 'application/json;charset=utf-8' });
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement('a');
    anchor.href = url;
    anchor.download = makeFileName();
    anchor.rel = 'noopener';
    document.body.append(anchor);
    anchor.click();
    anchor.remove();
    window.setTimeout(() => URL.revokeObjectURL(url), 0);
  }

  async function onExport(): Promise<void> {
    if (exporting) return;
    exporting = true;
    status = null;

    try {
      const config = await exportConfig();
      if (!config) {
        showError('設定を取得できませんでした。Tauri環境で開いてから再試行してください。');
        return;
      }

      const json = JSON.stringify(buildBundle(config), null, 2);
      exportedJson = json;
      downloadJson(json);
      showSuccess('設定JSONを作成しました。ダウンロードまたはコピーで保存できます。');
    } catch (e) {
      showError(`エクスポートに失敗しました: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      exporting = false;
    }
  }

  async function copyExportedJson(): Promise<void> {
    if (!exportedJson) return;

    try {
      if (typeof navigator === 'undefined' || !navigator.clipboard) {
        throw new Error('clipboard unavailable');
      }
      await navigator.clipboard.writeText(exportedJson);
      showSuccess('クリップボードにコピーしました。');
    } catch {
      exportTextarea?.focus();
      exportTextarea?.select();
      showError('クリップボードにコピーできませんでした。選択済みのJSONを手動でコピーしてください。');
    }
  }

  function parseBundle(raw: string): { config: ExportedConfig; appearance?: unknown } {
    let parsed: unknown;
    try {
      parsed = JSON.parse(raw);
    } catch {
      throw new Error('JSONの形式が壊れています。');
    }

    if (!isRecord(parsed)) {
      throw new Error('設定JSONのルートがオブジェクトではありません。');
    }

    if ('schema' in parsed && parsed.schema !== SCHEMA && parsed.schema !== LEGACY_SCHEMA) {
      throw new Error('fast-comment の設定バックアップではないため読み込めません。');
    }

    if (!isRecord(parsed.config)) {
      throw new Error('config オブジェクトが見つかりません。');
    }

    return {
      config: parsed.config,
      appearance: parsed.appearance,
    };
  }

  async function applyRawImport(raw: string): Promise<void> {
    if (importDisabled) return;

    const trimmed = raw.trim();
    if (!trimmed) {
      showError('読み込むJSONを選択または貼り付けてください。');
      return;
    }

    let parsed: { config: ExportedConfig; appearance?: unknown };
    try {
      parsed = parseBundle(trimmed);
    } catch (e) {
      showError(e instanceof Error ? e.message : String(e));
      return;
    }

    if (parsed.appearance !== undefined && !isAppearanceSnapshot(parsed.appearance)) {
      showError('外観設定の値が不正です。インポートは実行していません。');
      return;
    }

    const ok = typeof window === 'undefined'
      ? true
      : window.confirm('現在の設定をバックアップJSONの内容で上書きします。続行しますか？');
    if (!ok) return;

    configSync.importing = 'all';
    status = null;
    try {
      const result = await importConfig(parsed.config);
      if (result && result.revision > configSync.revision) configSync.revision = result.revision;
      if (parsed.appearance !== undefined) theme.applySnapshot(parsed.appearance);
      if (!result) {
        showSuccess(parsed.appearance === undefined
          ? '設定JSONを読み込みました。'
          : '外観設定を反映しました。');
        return;
      }

      const reloaded = !!(await onImported?.());
      const message = importMessage(result, parsed.appearance !== undefined, reloaded);
      if (message) showSuccess(message);
    } catch (e) {
      const typeError = typeof e === 'string' ? configTypeError.exec(e) : null;
      showError(typeError?.[0] ?? '設定のインポートに失敗しました。');
    } finally {
      configSync.importing = null;
    }
  }

  function onImportFromText(): void {
    void applyRawImport(importText);
  }

  function onFileSelected(event: Event): void {
    const input = event.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    if (!file) return;

    const reader = new FileReader();
    reader.onload = () => {
      const raw = typeof reader.result === 'string' ? reader.result : '';
      importText = raw;
      void applyRawImport(raw);
      if (importFileInput) importFileInput.value = '';
    };
    reader.onerror = () => {
      showError('ファイルを読み込めませんでした。');
      if (importFileInput) importFileInput.value = '';
    };
    reader.readAsText(file);
  }
</script>

<div class="portability">
  {#if configSync.reloadRequired}
    <p class="portability-banner banner-error" role="status">{configReloadMessage}</p>
    <div class="field-row">
      <button type="button" class="export-btn" onclick={onReload} disabled={disabled}>
        {reloading ? '読込中...' : '設定を再読み込み'}
      </button>
    </div>
  {/if}
  {#if status}
    <p
      class:banner-success={status.kind === 'success'}
      class:banner-error={status.kind === 'error'}
      class="portability-banner"
      role="status"
    >
      {status.message}
    </p>
  {/if}

  <div class="portability-block">
    <h4>エクスポート</h4>
    <p class="portability-note">ログイン情報(トークン・Cookie・API キー・Client ID)と棒読みちゃんの起動パスは書き出しません。URL のユーザー名とパスワードも消します。v0.8.0 までの版では読み込めません。</p>
    <div class="field-row portability-actions">
      <button type="button" class="export-btn" onclick={onExport} disabled={exporting}>
        {exporting ? 'エクスポート中...' : '設定JSONをエクスポート'}
      </button>
      <button type="button" class="copy-btn" onclick={copyExportedJson} disabled={!exportedJson}>
        クリップボードにコピー
      </button>
    </div>
    <label class="portability-label" for="settings-export-json">エクスポートJSON</label>
    <textarea
      id="settings-export-json"
      bind:this={exportTextarea}
      class="portability-textarea"
      readonly
      rows="8"
      value={exportedJson}
      aria-label="エクスポートされた設定JSON"
    ></textarea>
  </div>

  <div class="portability-block">
    <h4>インポート</h4>
    <p class="portability-note">ログイン情報、X の接続先、棒読みちゃんの起動パスは、ファイルの値を使わず今の設定のままにします。読み上げの接続先は、この PC を指す時だけ読み込みます。X の自動告知は無効にします。</p>
    <div class="field-row portability-actions">
      <label class="file-button" for="settings-import-file">JSONファイルを選択</label>
      <input
        id="settings-import-file"
        bind:this={importFileInput}
        class="file-input"
        type="file"
        disabled={importDisabled}
        accept=".json,application/json"
        onchange={onFileSelected}
        aria-label="インポートする設定JSONファイル"
      />
      <button type="button" class="export-btn" onclick={onImportFromText} disabled={importDisabled}>
        {configSync.importing === 'all' ? '読込中...' : 'テキストから読込'}
      </button>
    </div>
    <label class="portability-label" for="settings-import-json">貼り付け用JSON</label>
    <textarea
      id="settings-import-json"
      class="portability-textarea"
      rows="8"
      bind:value={importText}
      placeholder="エクスポートした設定JSONを貼り付け"
      aria-label="インポートする設定JSON"
    ></textarea>
  </div>
</div>

<style>
  .portability {
    display: grid;
    gap: 12px;
    margin-top: 8px;
  }

  .portability-block {
    display: grid;
    gap: 6px;
  }

  h4 {
    color: #ccc;
    font-size: 13px;
    margin: 0;
  }

  .field-row {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
  }

  .portability-actions {
    align-items: center;
  }

  .copy-btn,
  .export-btn,
  .file-button {
    border: none;
    border-radius: 4px;
    cursor: pointer;
    font-size: 12px;
    padding: 7px 14px;
    font-weight: 600;
    transition: opacity 0.15s;
  }

  .copy-btn {
    background: #37474f;
    color: #fff;
  }

  .export-btn,
  .file-button {
    background: #1976d2;
    color: #fff;
  }

  .copy-btn:disabled,
  .export-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .file-input {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
    white-space: nowrap;
    border: 0;
  }

  .portability-label {
    color: #ccc;
    font-size: 12px;
  }

  .portability-note {
    color: #757575;
    font-size: 11px;
    margin: 0;
  }

  .portability-textarea {
    width: 100%;
    min-height: 120px;
    box-sizing: border-box;
    background: rgba(255,255,255,0.05);
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 4px;
    color: #e0e0e0;
    font-family: monospace;
    font-size: 12px;
    line-height: 1.5;
    padding: 8px;
    resize: vertical;
  }

  .portability-banner {
    border-radius: 4px;
    font-size: 12px;
    margin: 0;
    padding: 8px 10px;
  }

  .banner-success {
    background: rgba(46, 125, 50, 0.22);
    border: 1px solid rgba(129, 199, 132, 0.45);
    color: #a5d6a7;
  }

  .banner-error {
    background: rgba(183, 28, 28, 0.22);
    border: 1px solid rgba(239, 154, 154, 0.45);
    color: #ef9a9a;
  }
</style>
