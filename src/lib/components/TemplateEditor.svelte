<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import {
    deleteTemplate,
    duplicateTemplate,
    importTemplateZip,
    listTemplates,
    readTemplateFile,
    writeTemplateFile,
    type TemplateInfo
  } from '../ipc';
  import {
    applyDeleted,
    applyDuplicated,
    groupTemplates,
    suggestCopyName,
    templateKindLabel,
    templateWsPath
  } from '../templateCatalog';

  const FILES = ['style.css', 'index.html', 'app.js'] as const;
  type TemplateFile = (typeof FILES)[number];
  const PREVIEW_WIDTH_PRESETS = [
    { value: '1920', label: '1920' },
    { value: '1280', label: '1280' },
    { value: '720', label: '720' },
    { value: 'custom', label: 'Custom' }
  ] as const;
  type PreviewWidthPreset = (typeof PREVIEW_WIDTH_PRESETS)[number]['value'];

  const MAX_ZIP_BYTES = 100 * 1024 * 1024;
  const MIN_PREVIEW_WIDTH = 320;
  const MAX_PREVIEW_WIDTH = 3840;

  interface Props {
    obsPort: number;
    currentTemplate?: string;
    /** 一覧の取得・複製・削除の後に呼ぶ。removedId は削除した ID。 */
    onTemplatesChange?: (list: TemplateInfo[], removedId?: string) => void;
  }

  let { obsPort, currentTemplate = 'default', onTemplatesChange }: Props = $props();

  let templateList: TemplateInfo[] = $state([]);
  let copyName: string = $state('');
  let copyOpen: boolean = $state(false);
  let busy: boolean = $state(false);
  let importOpen: boolean = $state(false);
  let importFile: File | null = $state(null);
  let importName: string = $state('');
  let importMsg: string = $state('');
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

  const templates = $derived(templateList.map((t) => t.id));
  const grouped = $derived(groupTemplates(templateList));
  const selectedInfo = $derived(templateList.find((t) => t.id === selectedTemplate));
  // 一覧に無い ID(読み込み前など)は編集不可として扱う。
  const editable = $derived(selectedInfo?.editable ?? false);
  const unknownSelected = $derived(!!selectedTemplate && !templates.includes(selectedTemplate));

  const previewUrl = $derived.by(() => {
    const port = Number.isFinite(obsPort) && obsPort > 0 ? obsPort : 11180;
    const template = selectedTemplate || 'default';
    const url = new URL(`http://127.0.0.1:${port}/`);
    url.searchParams.set('template', template);
    url.searchParams.set('ws', `ws://127.0.0.1:${port}${templateWsPath(template)}`);
    url.searchParams.set('t', String(previewNonce));
    return url.toString();
  });

  const obsTemplateUrl = $derived.by(() => {
    const port = Number.isFinite(obsPort) && obsPort > 0 ? obsPort : 11180;
    const template = selectedTemplate || 'default';
    const url = new URL(`http://127.0.0.1:${port}/`);
    url.searchParams.set('template', template);
    url.searchParams.set('ws', `ws://127.0.0.1:${port}${templateWsPath(template)}`);
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
      templateList = await listTemplates();
      onTemplatesChange?.(templateList);
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
    copyOpen = false;
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

  function openCopy() {
    errorMsg = '';
    copyName = suggestCopyName(
      selectedInfo?.name ?? selectedTemplate,
      grouped.custom.map((t) => t.name)
    );
    copyOpen = true;
  }

  async function onDuplicate() {
    if (dirty && !window.confirm('未保存の変更があります。破棄して複製しますか?')) return;
    errorMsg = '';
    saveMsg = '';
    busy = true;
    try {
      let created: TemplateInfo;
      try {
        created = await duplicateTemplate(selectedTemplate, copyName.trim());
      } catch (e) {
        errorMsg = `複製に失敗しました: ${errorText(e)}`;
        return;
      }
      // 再取得の成否に関わらず、作成した 1 件を先に反映して選択・編集可能にする。
      templateList = applyDuplicated(templateList, created);
      onTemplatesChange?.(templateList);
      copyOpen = false;
      selectedTemplate = created.id;
      previewNonce = Date.now();
      await loadSelectedFile();
      const refetchError = await refetchList((list) => applyDuplicated(list, created));
      if (refetchError) {
        errorMsg = errorMsg ? `${errorMsg} / ${refetchError}` : refetchError;
      } else if (!errorMsg) {
        saveMsg = `${created.id} を作成しました`;
        scheduleSaveMessageClear();
      }
    } finally {
      busy = false;
    }
  }

  async function onDelete() {
    if (!editable) return;
    const id = selectedTemplate;
    const ok = window.confirm(
      `カスタムテンプレート ${id} を削除しますか?\n` +
        'この URL を使っている OBS ソースは表示されなくなります。この操作は元に戻せません。'
    );
    if (!ok) return;
    errorMsg = '';
    saveMsg = '';
    busy = true;
    try {
      try {
        await deleteTemplate(id);
      } catch (e) {
        errorMsg = `削除に失敗しました: ${errorText(e)}`;
        return;
      }
      // 削除が成功した時点で一覧・選択・config.obs.template を直してから再取得する。
      const applied = applyDeleted(templateList, id, selectedTemplate);
      templateList = applied.list;
      onTemplatesChange?.(templateList, id);
      selectedTemplate = applied.current;
      previewNonce = Date.now();
      await loadSelectedFile();
      const refetchError = await refetchList((list) => applyDeleted(list, id, selectedTemplate).list);
      if (refetchError) {
        errorMsg = errorMsg ? `${errorMsg} / ${refetchError}` : refetchError;
      } else if (!errorMsg) {
        saveMsg = `${id} を削除しました`;
        scheduleSaveMessageClear();
      }
    } finally {
      busy = false;
    }
  }

  function onZipPick(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    const file = input.files?.[0] ?? null;
    input.value = '';
    errorMsg = '';
    importMsg = '';
    if (!file) return;
    if (file.size > MAX_ZIP_BYTES) {
      importFile = null;
      importOpen = false;
      errorMsg = `zip が大きすぎます(${(file.size / 1024 / 1024).toFixed(1)} MiB)。上限は 100 MiB です`;
      return;
    }
    importFile = file;
    importName = file.name
      .replace(/\.zip$/i, '')
      .toLowerCase()
      .replace(/[^a-z0-9_-]/g, '-');
    importOpen = true;
  }

  async function onImport() {
    const file = importFile;
    if (!file) return;
    const name = importName.trim();
    if (!name || name.length > 64 || !/^[A-Za-z0-9_-]+$/.test(name)) {
      errorMsg = '名前は半角英数字・- _ の 64 文字以内で入力してください';
      return;
    }
    const ok = window.confirm(
      `${file.name} を custom:${name} として取り込みますか?\n` +
        'テンプレートの JavaScript は配信画面で実行されます。信頼できる配布元の zip だけ取り込んでください。'
    );
    if (!ok) return;
    if (dirty && !window.confirm('未保存の変更があります。破棄して取り込みますか?')) return;
    errorMsg = '';
    saveMsg = '';
    importMsg = '';
    busy = true;
    try {
      let res: Awaited<ReturnType<typeof importTemplateZip>>;
      try {
        res = await importTemplateZip(name, new Uint8Array(await file.arrayBuffer()));
      } catch (e) {
        errorMsg = `取り込みに失敗しました: ${errorText(e)}`;
        return;
      }
      templateList = applyDuplicated(templateList, res.template);
      onTemplatesChange?.(templateList);
      importOpen = false;
      importFile = null;
      selectedTemplate = res.template.id;
      previewNonce = Date.now();
      await loadSelectedFile();
      const refetchError = await refetchList((list) => applyDuplicated(list, res.template));
      if (refetchError) errorMsg = errorMsg ? `${errorMsg} / ${refetchError}` : refetchError;
      importMsg =
        `${res.template.id} を取り込みました(${res.files} ファイル)` +
        (res.skipped.length > 0
          ? `。取り込まなかったファイル ${res.skipped.length} 件: ${res.skipped.join(', ')}`
          : '');
    } finally {
      busy = false;
    }
  }

  /** 一覧を取り直し、fix で直前の操作を重ねて反映する。失敗時はエラー文を返す(状態は変えない)。 */
  async function refetchList(fix: (list: TemplateInfo[]) => TemplateInfo[]): Promise<string> {
    try {
      templateList = fix(await listTemplates());
      onTemplatesChange?.(templateList);
      return '';
    } catch (e) {
      return `テンプレート一覧の再取得に失敗しました: ${errorText(e)}`;
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
        disabled={loading || saving || busy || (templates.length === 0 && !unknownSelected)}
      >
        {#if unknownSelected}
          <option value={selectedTemplate}>{selectedTemplate}</option>
        {/if}
        {#if grouped.builtin.length > 0}
          <optgroup label={templateKindLabel('builtin')}>
            {#each grouped.builtin as t (t.id)}
              <option value={t.id}>{t.name}</option>
            {/each}
          </optgroup>
        {/if}
        {#if grouped.custom.length > 0}
          <optgroup label={templateKindLabel('custom')}>
            {#each grouped.custom as t (t.id)}
              <option value={t.id}>{t.name}</option>
            {/each}
          </optgroup>
        {/if}
      </select>
    </div>

    <div class="template-actions">
      {#if selectedInfo}
        <span class="kind-badge" class:custom={selectedInfo.kind === 'custom'}>
          {templateKindLabel(selectedInfo.kind)}
        </span>
      {/if}
      <button
        type="button"
        class="preview-action-btn"
        onclick={openCopy}
        disabled={loading || saving || busy || !selectedInfo}
      >
        {editable ? '複製' : '複製して編集'}
      </button>
      <button
        type="button"
        class="preview-action-btn danger"
        onclick={onDelete}
        disabled={loading || saving || busy || !editable}
        aria-label="カスタムテンプレートを削除"
      >
        削除
      </button>
    </div>

    {#if copyOpen}
      <div class="field-row">
        <label for="template-editor-copy-name">複製先の名前</label>
        <input
          id="template-editor-copy-name"
          type="text"
          class="template-select"
          bind:value={copyName}
          spellcheck="false"
          disabled={busy}
          placeholder="英小文字・数字・_・-"
        />
        <button
          type="button"
          class="save-template-btn"
          onclick={onDuplicate}
          disabled={busy || copyName.trim() === ''}
        >
          {busy ? '複製中...' : '作成'}
        </button>
        <button type="button" class="preview-action-btn" onclick={() => (copyOpen = false)} disabled={busy}>
          キャンセル
        </button>
      </div>
    {/if}

    <div class="field-row">
      <label for="template-editor-zip">zip から取り込み</label>
      <input
        id="template-editor-zip"
        type="file"
        accept=".zip"
        onchange={onZipPick}
        disabled={loading || saving || busy}
      />
    </div>

    {#if importOpen}
      <div class="field-row">
        <label for="template-editor-import-name">取り込み先の名前</label>
        <input
          id="template-editor-import-name"
          type="text"
          class="template-select"
          bind:value={importName}
          spellcheck="false"
          disabled={busy}
          placeholder="英小文字・数字・_・-"
        />
        <button
          type="button"
          class="save-template-btn"
          onclick={onImport}
          disabled={busy || importName.trim() === ''}
        >
          {busy ? '取り込み中...' : '取り込む'}
        </button>
        <button type="button" class="preview-action-btn" onclick={() => (importOpen = false)} disabled={busy}>
          キャンセル
        </button>
      </div>
    {/if}
    {#if importMsg}<p class="status ok">{importMsg}</p>{/if}

    <div class="field-row">
      <label for="template-editor-file">ファイル</label>
      <select
        id="template-editor-file"
        class="template-select"
        bind:value={selectedFile}
        onchange={onFileChange}
        disabled={loading || saving || busy || !selectedTemplate}
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
      readonly={!editable}
      disabled={loading || saving || busy || !selectedTemplate}
      aria-label="OBSテンプレートファイル内容"
    ></textarea>

    <div class="editor-actions">
      <button
        type="button"
        class="save-template-btn"
        onclick={onSave}
        disabled={loading || saving || busy || !selectedTemplate || !editable || !dirty}
      >
        {saving ? '保存中...' : '保存'}
      </button>
      {#if selectedInfo && !editable}
        <span class="status">内蔵テンプレートは読み取り専用です。「複製して編集」で編集できます</span>
      {/if}
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
          sandbox="allow-scripts allow-same-origin"
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

  .template-actions {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 6px;
    flex-wrap: wrap;
  }

  .kind-badge {
    font-size: 11px;
    padding: 2px 8px;
    border-radius: 10px;
    background: rgba(255,255,255,0.1);
    color: #ccc;
  }

  .kind-badge.custom {
    background: rgba(25,118,210,0.25);
    color: #90caf9;
  }

  .preview-action-btn.danger {
    background: rgba(239,83,80,0.22);
    color: #ef9a9a;
  }

  .template-textarea[readonly] {
    color: #a8a8a8;
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
