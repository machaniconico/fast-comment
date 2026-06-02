<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { listTemplates, readTemplateFile, writeTemplateFile } from '../ipc';

  const FILES = ['style.css', 'index.html', 'app.js'] as const;
  type TemplateFile = (typeof FILES)[number];

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
      if (saveMsgTimer !== null) clearTimeout(saveMsgTimer);
      saveMsgTimer = setTimeout(() => {
        saveMsg = '';
        saveMsgTimer = null;
      }, 2500);
    } catch (e) {
      errorMsg = `保存に失敗しました: ${errorText(e)}`;
    } finally {
      saving = false;
    }
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
    <iframe title="OBSテンプレートプレビュー" src={previewUrl}></iframe>
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

  .preview-pane iframe {
    width: 100%;
    min-height: 420px;
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 4px;
    background: rgba(0,0,0,0.28);
  }

  @media (max-width: 920px) {
    .template-editor {
      grid-template-columns: 1fr;
    }
  }
</style>
