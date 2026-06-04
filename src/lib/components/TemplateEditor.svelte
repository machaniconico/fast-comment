<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { listTemplates, readTemplateFile, writeTemplateFile } from '../ipc';

  const FILES = ['style.css', 'index.html', 'app.js'] as const;
  type TemplateFile = (typeof FILES)[number];
  const PREVIEW_WIDTH_PRESETS = [
    { value: '1920', label: '1920' },
    { value: '1280', label: '1280' },
    { value: '720', label: '720' },
    { value: 'custom', label: 'Custom' }
  ] as const;
  type PreviewWidthPreset = (typeof PREVIEW_WIDTH_PRESETS)[number]['value'];

  const MIN_PREVIEW_WIDTH = 320;
  const MAX_PREVIEW_WIDTH = 3840;

  interface Props {
    obsPort: number;
    currentTemplate?: string;
  }

  let { obsPort, currentTemplate = 'default' }: Props = $props();

  let templates: string[] = $state([]);
  let selectedTemplate: string = $state('default');
  let selectedFile: TemplateFile = $state('style.css');
  let contents: string = $state('');
  let originalContents: string = $state('');
  let loading: boolean = $state(false);
  let saving: boolean = $state(false);
  let errorMsg: string = $state('');
  let saveMsg: string = $state('');
  let previewNonce: number = $state(Date.now());
  let previewWidth: number = $state(1280);
  let previewWidthPreset: PreviewWidthPreset = $state('1280');
  let checkerBg: boolean = $state(false);
  let frameWidth: number = $state(0);

  let loadSeq = 0;
  let saveMsgTimer: ReturnType<typeof setTimeout> | null = null;

  const dirty = $derived(contents !== originalContents);

  const templateOptions = $derived.by(() => {
    if (!selectedTemplate || templates.includes(selectedTemplate)) return templates;
    return [selectedTemplate, ...templates];
  });

  const previewUrl = $derived.by(() => {
    const port = Number.isFinite(obsPort) && obsPort > 0 ? obsPort : 11180;
    const template = selectedTemplate || 'default';
    const url = new URL(`http://127.0.0.1:${port}/`);
    url.searchParams.set('template', template);
    url.searchParams.set('ws', `ws://127.0.0.1:${port}/ws`);
    url.searchParams.set('t', String(previewNonce));
    return url.toString();
  });

  const obsTemplateUrl = $derived.by(() => {
    const port = Number.isFinite(obsPort) && obsPort > 0 ? obsPort : 11180;
    const template = selectedTemplate || 'default';
    const url = new URL(`http://127.0.0.1:${port}/`);
    url.searchParams.set('template', template);
    url.searchParams.set('ws', `ws://127.0.0.1:${port}/ws`);
    return url.toString();
  });

  const previewHeight = $derived(Math.round((previewWidth * 9) / 16));
  const previewScale = $derived.by(() => {
    if (frameWidth === 0) return 1;
    return Math.min(1, frameWidth / previewWidth);
  });
  const previewFrameHeight = $derived(previewHeight * previewScale);

  onMount(async () => {
    selectedTemplate = currentTemplate || 'default';
    await refreshTemplates();
    await loadSelectedFile();
  });

  onDestroy(() => {
    if (saveMsgTimer !== null) clearTimeout(saveMsgTimer);
  });

  async function refreshTemplates() {
    errorMsg = '';
    try {
      const names = await listTemplates();
      templates = names;
      if (templates.length > 0 && !templates.includes(selectedTemplate)) {
        selectedTemplate = templates.includes(currentTemplate) ? currentTemplate : templates[0];
      }
    } catch (e) {
      errorMsg = `テンプレート一覧の取得に失敗しました: ${errorText(e)}`;
    }
  }

  async function loadSelectedFile() {
    const seq = ++loadSeq;
    errorMsg = '';
    saveMsg = '';
    if (!selectedTemplate) {
      contents = '';
      originalContents = '';
      return;
    }
    loading = true;
    try {
      const text = await readTemplateFile(selectedTemplate, selectedFile);
      if (seq !== loadSeq) return;
      contents = text;
      originalContents = text;
    } catch (e) {
      if (seq !== loadSeq) return;
      contents = '';
      originalContents = '';
      errorMsg = `テンプレートファイルの読み込みに失敗しました: ${errorText(e)}`;
    } finally {
      if (seq === loadSeq) loading = false;
    }
  }

  async function onTemplateChange(event: Event) {
    selectedTemplate = (event.currentTarget as HTMLSelectElement).value;
    previewNonce = Date.now();
    await loadSelectedFile();
  }

  async function onFileChange(event: Event) {
    const value = (event.currentTarget as HTMLSelectElement).value;
    if (FILES.includes(value as TemplateFile)) {
      selectedFile = value as TemplateFile;
      await loadSelectedFile();
    }
  }

  async function onSave() {
    errorMsg = '';
    saveMsg = '';
    saving = true;
    try {
      await writeTemplateFile(selectedTemplate, selectedFile, contents);
      originalContents = contents;
      previewNonce = Date.now();
      saveMsg = '保存しました';
      scheduleSaveMessageClear();
    } catch (e) {
      errorMsg = `保存に失敗しました: ${errorText(e)}`;
    } finally {
      saving = false;
    }
  }

  async function copyObsTemplateUrl() {
    errorMsg = '';
    saveMsg = '';
    try {
      await navigator.clipboard.writeText(obsTemplateUrl);
      saveMsg = 'URLをコピーしました';
      scheduleSaveMessageClear();
    } catch (e) {
      errorMsg = `URLのコピーに失敗しました: ${errorText(e)}`;
    }
  }

  function reloadPreview() {
    previewNonce = Date.now();
  }

  function onPreviewWidthPresetChange(event: Event) {
    const value = (event.currentTarget as HTMLSelectElement).value as PreviewWidthPreset;
    previewWidthPreset = value;
    if (value !== 'custom') {
      previewWidth = Number(value);
    } else {
      previewWidth = clampPreviewWidth(previewWidth);
    }
  }

  function onCustomPreviewWidthInput(event: Event) {
    const value = Number((event.currentTarget as HTMLInputElement).value);
    previewWidth = clampPreviewWidth(value);
  }

  function clampPreviewWidth(value: number): number {
    if (!Number.isFinite(value)) return MIN_PREVIEW_WIDTH;
    return Math.min(MAX_PREVIEW_WIDTH, Math.max(MIN_PREVIEW_WIDTH, Math.round(value)));
  }

  function scheduleSaveMessageClear() {
    if (saveMsgTimer !== null) clearTimeout(saveMsgTimer);
    saveMsgTimer = setTimeout(() => {
      saveMsg = '';
      saveMsgTimer = null;
    }, 2500);
  }

  function errorText(e: unknown): string {
    return e instanceof Error ? e.message : String(e);
  }
</script>

<div class="template-editor">
  <div class="editor-pane">
    <div class="field-row">
      <label for="template-editor-template">編集テンプレート</label>
      <select
        id="template-editor-template"
        class="template-select"
        bind:value={selectedTemplate}
        onchange={onTemplateChange}
        disabled={loading || saving || templateOptions.length === 0}
      >
        {#each templateOptions as name}
          <option value={name}>{name}</option>
        {/each}
      </select>
    </div>

    <div class="field-row">
      <label for="template-editor-file">ファイル</label>
      <select
        id="template-editor-file"
        class="template-select"
        bind:value={selectedFile}
        onchange={onFileChange}
        disabled={loading || saving || !selectedTemplate}
      >
        {#each FILES as file}
          <option value={file}>{file}</option>
        {/each}
      </select>
    </div>

    <textarea
      class="template-textarea"
      bind:value={contents}
      spellcheck="false"
      disabled={loading || saving || !selectedTemplate}
      aria-label="OBSテンプレートファイル内容"
    ></textarea>

    <div class="editor-actions">
      <button
        type="button"
        class="save-template-btn"
        onclick={onSave}
        disabled={loading || saving || !selectedTemplate || !dirty}
      >
        {saving ? '保存中...' : '保存'}
      </button>
      {#if loading}<span class="status">読み込み中...</span>{/if}
      {#if saveMsg}<span class="status ok">{saveMsg}</span>{/if}
      {#if dirty}<span class="status dirty">未保存</span>{/if}
    </div>

    {#if errorMsg}<p class="error">{errorMsg}</p>{/if}
  </div>

  <div class="preview-pane">
    <div class="preview-controls">
      <button
        type="button"
        class="preview-action-btn"
        onclick={copyObsTemplateUrl}
        disabled={!selectedTemplate}
        aria-label="OBSテンプレートURLをコピー"
      >
        URLをコピー
      </button>
      <button
        type="button"
        class="preview-action-btn"
        onclick={reloadPreview}
        disabled={!selectedTemplate}
        aria-label="OBSテンプレートプレビューを再読み込み"
      >
        再読み込み
      </button>

      <div class="preview-width-control">
        <label for="preview-width-preset">幅</label>
        <select
          id="preview-width-preset"
          class="preview-select"
          bind:value={previewWidthPreset}
          onchange={onPreviewWidthPresetChange}
          aria-label="プレビュー幅プリセット"
        >
          {#each PREVIEW_WIDTH_PRESETS as preset}
            <option value={preset.value}>{preset.label}</option>
          {/each}
        </select>
      </div>

      {#if previewWidthPreset === 'custom'}
        <div class="preview-width-control custom-width">
          <label for="preview-custom-width">px</label>
          <input
            id="preview-custom-width"
            type="number"
            min={MIN_PREVIEW_WIDTH}
            max={MAX_PREVIEW_WIDTH}
            step="1"
            value={previewWidth}
            oninput={onCustomPreviewWidthInput}
            onchange={onCustomPreviewWidthInput}
            aria-label="カスタムプレビュー幅"
          />
        </div>
      {/if}

      <label class="checker-toggle">
        <input
          type="checkbox"
          bind:checked={checkerBg}
          aria-label="透明背景チェッカーを表示"
        />
        <span>透過背景</span>
      </label>
    </div>

    <div
      class="preview-frame"
      class:checker={checkerBg}
      bind:clientWidth={frameWidth}
      style:height={`${previewFrameHeight}px`}
    >
      <div
        class="preview-scale"
        style:width={`${previewWidth}px`}
        style:height={`${previewHeight}px`}
        style:transform={`scale(${previewScale})`}
      >
        <iframe
          title="OBSテンプレートプレビュー"
          src={previewUrl}
          style:width={`${previewWidth}px`}
          style:height={`${previewHeight}px`}
        ></iframe>
      </div>
    </div>
  </div>
</div>

<style>
  .template-editor {
    display: grid;
    grid-template-columns: minmax(280px, 1fr) minmax(260px, 0.8fr);
    gap: 12px;
    margin-top: 10px;
  }

  .editor-pane,
  .preview-pane {
    min-width: 0;
  }

  .field-row {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-top: 6px;
    flex-wrap: wrap;
  }

  .field-row label {
    font-size: 13px;
    color: #ccc;
    min-width: 110px;
  }

  .template-select {
    flex: 1;
    min-width: 160px;
    background: rgba(255,255,255,0.07);
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 4px;
    color: #e0e0e0;
    padding: 5px 8px;
    font-size: 13px;
  }

  .template-textarea {
    width: 100%;
    min-height: 330px;
    margin-top: 8px;
    box-sizing: border-box;
    background: rgba(255,255,255,0.05);
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 4px;
    color: #e0e0e0;
    font-size: 12px;
    line-height: 1.45;
    padding: 8px;
    resize: vertical;
    font-family: ui-monospace, SFMono-Regular, Consolas, 'Liberation Mono', monospace;
  }

  .editor-actions {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 8px;
    min-height: 28px;
    flex-wrap: wrap;
  }

  .save-template-btn {
    border: none;
    border-radius: 4px;
    cursor: pointer;
    font-size: 12px;
    padding: 6px 14px;
    font-weight: 600;
    background: #1976d2;
    color: #fff;
  }

  .save-template-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .preview-pane {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .preview-controls {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }

  .preview-action-btn {
    border: none;
    border-radius: 4px;
    cursor: pointer;
    font-size: 12px;
    padding: 6px 10px;
    font-weight: 600;
    background: rgba(255,255,255,0.12);
    color: #f0f0f0;
  }

  .preview-action-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .preview-width-control {
    display: flex;
    align-items: center;
    gap: 6px;
  }

  .preview-width-control label,
  .checker-toggle {
    font-size: 12px;
    color: #ccc;
  }

  .preview-select,
  .preview-width-control input {
    background: rgba(255,255,255,0.07);
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 4px;
    color: #e0e0e0;
    padding: 5px 8px;
    font-size: 12px;
  }

  .preview-width-control input {
    width: 86px;
    box-sizing: border-box;
  }

  .checker-toggle {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    cursor: pointer;
    user-select: none;
  }

  .status {
    font-size: 12px;
    color: #9e9e9e;
  }

  .status.ok {
    color: #66bb6a;
  }

  .status.dirty {
    color: #ffb74d;
  }

  .error {
    margin: 6px 0 0;
    color: #ef9a9a;
    font-size: 12px;
  }

  .preview-frame {
    position: relative;
    width: 100%;
    overflow: hidden;
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 4px;
    background: rgba(0,0,0,0.28);
    box-sizing: border-box;
  }

  .preview-frame.checker {
    background-color: #d8d8d8;
    background-image: conic-gradient(#d8d8d8 25%, #a8a8a8 0 50%, #d8d8d8 0 75%, #a8a8a8 0);
    background-size: 32px 32px;
  }

  .preview-scale {
    transform-origin: top left;
  }

  .preview-scale iframe {
    display: block;
    border: 0;
    background: transparent;
  }

  @media (max-width: 920px) {
    .template-editor {
      grid-template-columns: 1fr;
    }
  }
</style>
