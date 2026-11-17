<script lang="ts">
  import { onMount } from 'svelte';
  import { onDestroy } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import type { LocalApiStatus, WebhookInput, WebhookStatus, WebhookListItem, WebhookTestResult, WebhookView } from '../types';
  import {
    WEBHOOK_KEYWORDS_MAX,
    WEBHOOK_MAX,
    WEBHOOK_TEXT_MAX,
    pauseReasonLabel,
    resultKindLabel,
    splitKeywords,
    trimWhite,
    stateLabel,
    testResultLabel,
    textLength,
  } from '../integrations';
  let status = $state<LocalApiStatus | null>(null);
  let token = $state('');
  let busy = $state(false);
  let error = $state('');
  let confirming = $state(false);
  async function run(action: () => Promise<void>) {
    if (busy) return;
    busy = true;
    error = '';
    try { await action(); } catch (cause) {
      token = '';
      // Render only fixed messages: an IPC error must never expose a token value.
      switch (cause) {
        case 'SecretRead': error = 'トークンを読めません。再生成してください。'; break;
        case 'SecretWrite': error = '資格情報を保存できませんでした。OS の資格情報ストアを確認してください。'; break;
        case 'FileWrite': error = '連携設定を保存できませんでした。設定ファイルと保存先の権限を確認してください。読み込みに失敗した場合はアプリの再起動が必要です。'; break;
        case 'ForbiddenWindow': error = 'メインウィンドウから操作してください。'; break;
        default: error = '操作に失敗しました。もう一度お試しください。';
      }
      try { status = await invoke<LocalApiStatus>('get_local_api_status'); }
      catch { status = null; }
    }
    finally { busy = false; }
  }
  export async function refreshStatus() {
    await run(async () => { status = await invoke<LocalApiStatus>('get_local_api_status'); });
  }
  onMount(() => { void refreshStatus(); });
  async function toggle(event: Event) {
    // The browser changes checked before onchange. Restore the confirmed value
    // while the operation runs, including when a failed enable stays disabled.
    (event.currentTarget as HTMLInputElement).checked = status?.enabled ?? false;
    await run(async () => {
      status = await invoke<LocalApiStatus>('set_local_api_enabled', { enabled: !status?.enabled });
      token = '';
    });
  }
  async function reveal(copy: boolean) {
    await run(async () => {
      const value = await invoke<string>('get_local_api_token');
      if (copy) await navigator.clipboard.writeText(value);
      else token = value;
    });
  }
  async function regenerate() {
    await run(async () => {
      status = await invoke<LocalApiStatus>('regenerate_local_api_token');
      token = '';
      confirming = false;
    });
  }

  // ── Webhook ──
  // 検証・判定・停止はすべて Rust 側。URL・署名シークレットは console にも
  // エラー表示にも出さない(一覧に載るのは Rust が返す伏せ字プレビューだけ)。
  let hooks = $state<WebhookListItem[]>([]);
  let hooksBusy = $state(false);
  let hooksError = $state('');
  let editing = $state<WebhookInput | null>(null);
  let editingPreview = $state<string | null>(null);
  let formUrl = $state('');
  let formKeywords = $state('');
  let deleteId = $state<string | null>(null);
  let regenId = $state<string | null>(null);
  let secrets = $state<Record<string, string>>({});
  let testResults = $state<Record<string, string>>({});
  let unlisten: UnlistenFn | undefined;
  let destroyed = false;

  const atLimit = $derived(hooks.length >= WEBHOOK_MAX);

  function hookErrorMessage(cause: unknown): string {
    switch (cause) {
      case 'InvalidWebhook': return '設定が正しくありません。名前(64 文字以内)・URL・キーワード(20 件まで、各 64 文字以内)・件数の上限(10 件)を確認してください。形式を変えた時は URL も入れ直してください。';
      case 'WebhookSecretRead': return 'Webhook の資格情報を読めません。URL を設定し直してください。';
      case 'SecretWrite': return '資格情報を保存できませんでした。OS の資格情報ストアを確認してください。';
      case 'FileWrite': return '連携設定を保存できませんでした。設定ファイルと保存先の権限を確認してください。';
      case 'ForbiddenWindow': return 'メインウィンドウから操作してください。';
      default: return '操作に失敗しました。もう一度お試しください。';
    }
  }

  async function loadHooks() {
    hooks = await invoke<WebhookListItem[]>('list_webhooks');
  }

  async function runHook(action: () => Promise<void>) {
    if (hooksBusy) return;
    hooksBusy = true;
    hooksError = '';
    try { await action(); } catch (cause) {
      secrets = {};
      hooksError = hookErrorMessage(cause);
      try { await loadHooks(); } catch { /* 一覧は直前の状態のまま */ }
    }
    finally { hooksBusy = false; }
  }

  function applyStatuses(list: WebhookStatus[]) {
    hooks = hooks.map((h) => ({ ...h, status: list.find((s) => s.id === h.id) ?? h.status }));
  }

  onMount(() => {
    void runHook(loadHooks);
    void listen<WebhookStatus[]>('webhook-status', (e) => applyStatuses(e.payload)).then((fn) => {
      if (destroyed) fn(); else unlisten = fn;
    });
  });
  onDestroy(() => { destroyed = true; unlisten?.(); });

  function startAdd() {
    editing = { name: '', format: 'discord', enabled: true, triggers: { donation: true, membership: true, keyword: false }, keywords: [] };
    editingPreview = null; formUrl = ''; formKeywords = '';
  }
  function startEdit(h: WebhookListItem) {
    editing = { id: h.id, name: h.name, format: h.format, enabled: h.enabled, triggers: { ...h.triggers }, keywords: [...h.keywords] };
    editingPreview = h.urlPreview; formUrl = ''; formKeywords = h.keywords.join('\n');
  }
  function cancelEdit() { editing = null; formUrl = ''; formKeywords = ''; }

  async function saveHook() {
    const form = editing;
    if (!form) return;
    const input: WebhookInput = { ...form, name: trimWhite(form.name), keywords: splitKeywords(formKeywords) };
    const url = formUrl.trim();
    if (url) input.url = url; else delete input.url;
    await runHook(async () => {
      await invoke<WebhookView>('save_webhook', { input });
      // 形式変更などで Rust がシークレットを作り直すことがあるため、表示キャッシュを捨てる。
      if (input.id) delete secrets[input.id];
      editing = null; formUrl = ''; formKeywords = '';
      await loadHooks();
    });
  }

  async function deleteHook(id: string) {
    await runHook(async () => {
      await invoke('delete_webhook', { id });
      deleteId = null;
      delete secrets[id]; delete testResults[id];
      if (editing?.id === id) cancelEdit();
      await loadHooks();
    });
  }
  async function resumeHook(id: string) {
    await runHook(async () => { await invoke('resume_webhook', { id }); await loadHooks(); });
  }
  async function testHook(id: string) {
    await runHook(async () => {
      testResults[id] = '送信中…';
      const result = await invoke<WebhookTestResult>('test_webhook', { id });
      testResults[id] = testResultLabel(result);
    });
  }
  async function revealSecret(id: string, copy: boolean) {
    await runHook(async () => {
      const value = await invoke<string>('reveal_webhook_signing_secret', { id });
      if (copy) await navigator.clipboard.writeText(value);
      else secrets[id] = value;
    });
  }
  async function regenerateSecret(id: string) {
    await runHook(async () => {
      await invoke<string>('regenerate_webhook_signing_secret', { id });
      delete secrets[id];
      regenId = null;
    });
  }

  function triggerSummary(h: WebhookListItem): string {
    const names: string[] = [];
    if (h.triggers.donation) names.push('投げ銭');
    if (h.triggers.membership) names.push('メンバー加入');
    if (h.triggers.keyword) names.push('キーワード一致');
    return names.length ? names.join(' / ') : 'きっかけなし';
  }
  const keywordCount = $derived(splitKeywords(formKeywords).length);
</script>

<section id="settings-local-api">
  <h3>ローカル API</h3>
  <p>この節の操作はその場で保存されます。</p>
  <label><input type="checkbox" checked={status?.enabled ?? false} disabled={busy || !status || status.writesBlocked} onchange={toggle} /> 有効にする</label>
  <p role="status">{status?.writesBlocked ? (status.notice ?? '連携設定を読めませんでした。設定ファイルを確認してアプリを再起動してください') : (status?.tokenError ?? status?.notice ?? (status?.enabled ? '有効' : '無効'))}</p>
  {#if status}
    <div class="row"><code>{status.baseUrl}</code><button disabled={busy} onclick={() => run(async () => { await navigator.clipboard.writeText(status!.baseUrl); })}>URL をコピー</button></div>
    <div class="row"><span class="token">{token || '••••••••••••••••'}</span>
      <button disabled={busy || !status.hasToken} onclick={() => token ? token = '' : reveal(false)}>{token ? '隠す' : '表示'}</button>
      <button disabled={busy || !status.hasToken} onclick={() => reveal(true)}>トークンをコピー</button>
      <button disabled={busy || status.writesBlocked} onclick={() => { confirming = true; }}>再生成</button>
    </div>
    {#if confirming}
      <div role="alert">
        <p>今のトークンを使っているツールは動かなくなります。再生成しますか？</p>
        <button disabled={busy || status.writesBlocked} onclick={regenerate}>再生成する</button>
        <button disabled={busy} onclick={() => { confirming = false; }}>キャンセル</button>
      </div>
    {/if}
    <p>トークンを OBS のソース URL やテンプレートに書かないでください。ブラウザからの利用には対応していません。</p>
    <pre>curl.exe -H "Authorization: Bearer &lt;token&gt;" {status.baseUrl}/status</pre>
  {/if}
  {#if error}<p role="alert">{error}</p>{/if}
</section>

<section id="settings-webhooks">
  <h3>Webhook</h3>
  <p>この節の操作はその場で保存されます。URL と署名シークレットは OS の資格情報ストアに保存され、設定のエクスポートには含まれません。</p>
  {#if hooks.length === 0}<p>送り先はまだありません。</p>{/if}
  {#each hooks as h (h.id)}
    {@const paused = h.status?.state === 'paused' || h.paused != null}
    <div class="hook">
      <div class="row">
        <strong>{h.name}</strong>
        <span>{h.format === 'generic' ? '汎用 JSON' : 'Discord'}</span>
        <span>{h.enabled ? '有効' : '無効'}</span>
        {#if h.enabled}
          <!-- 無効の時は直前の「無効」と同じ語になるので出さない -->
          <span role="status">{stateLabel(h.status?.state ?? null)}</span>
        {/if}
      </div>
      <div class="row"><code>{h.hasUrl ? (h.urlPreview ?? 'URL 設定済み') : 'URL 未設定'}</code><span>{triggerSummary(h)}{h.triggers.keyword && h.keywords.length ? `(${h.keywords.length} 語)` : ''}</span></div>
      {#if h.status}
        <p>送信 {h.status.delivered} / 失敗 {h.status.failed} / 破棄 {h.status.dropped} / 不明 {h.status.uncertain} / 待ち {h.status.queued}{h.status.last ? `、直近: ${resultKindLabel(h.status.last.kind)}${h.status.last.httpStatus != null ? `(HTTP ${h.status.last.httpStatus})` : ''}` : ''}</p>
      {/if}
      {#if paused}
        <p role="alert">停止中: {pauseReasonLabel(h.status?.pausedReason ?? h.paused?.reason)}</p>
        <div class="row"><button disabled={hooksBusy} onclick={() => resumeHook(h.id)}>再開</button></div>
      {/if}
      <div class="row">
        <button disabled={hooksBusy} onclick={() => testHook(h.id)}>テスト送信</button>
        <button disabled={hooksBusy} onclick={() => startEdit(h)}>編集</button>
        <button disabled={hooksBusy} onclick={() => { deleteId = h.id; }}>削除</button>
      </div>
      {#if testResults[h.id]}<p role="status">{testResults[h.id]}</p>{/if}
      {#if deleteId === h.id}
        <div role="alert">
          <p>「{h.name}」を削除しますか？ 保存済みの URL と署名シークレットも消えます。</p>
          <button disabled={hooksBusy} onclick={() => deleteHook(h.id)}>削除する</button>
          <button disabled={hooksBusy} onclick={() => { deleteId = null; }}>キャンセル</button>
        </div>
      {/if}
      {#if h.format === 'generic'}
        <div class="row"><span class="token">{secrets[h.id] || '署名シークレット: ••••••••••••••••'}</span>
          <button disabled={hooksBusy} onclick={() => secrets[h.id] ? delete secrets[h.id] : revealSecret(h.id, false)}>{secrets[h.id] ? '隠す' : '表示'}</button>
          <button disabled={hooksBusy} onclick={() => revealSecret(h.id, true)}>コピー</button>
          <button disabled={hooksBusy} onclick={() => { regenId = h.id; }}>再生成</button>
        </div>
        {#if regenId === h.id}
          <div role="alert">
            <p>今の署名シークレットで検証している受け側は、検証に失敗するようになります。再生成しますか？</p>
            <button disabled={hooksBusy} onclick={() => regenerateSecret(h.id)}>再生成する</button>
            <button disabled={hooksBusy} onclick={() => { regenId = null; }}>キャンセル</button>
          </div>
        {/if}
      {/if}
    </div>
  {/each}

  {#if editing}
    <form class="hook" onsubmit={(e) => { e.preventDefault(); void saveHook(); }}>
      <h4>{editing.id ? '送り先を編集' : '送り先を追加'}</h4>
      <label>名前 <input type="text" bind:value={editing.name} maxlength="200" disabled={hooksBusy} /></label>
      {#if textLength(editing.name.trim()) > WEBHOOK_TEXT_MAX}<p role="alert">名前は {WEBHOOK_TEXT_MAX} 文字以内にしてください。</p>{/if}
      <label>形式
        <select bind:value={editing.format} disabled={hooksBusy}>
          <option value="discord">Discord</option>
          <option value="generic">汎用 JSON</option>
        </select>
      </label>
      <label>URL <input type="password" autocomplete="off" spellcheck="false" bind:value={formUrl} disabled={hooksBusy}
        placeholder={editing.id ? '空欄なら今の URL を変更しません' : 'https://…'} /></label>
      {#if editing.id}<p>今の URL: {editingPreview ?? 'URL 未設定'}。形式を変える時は URL も入れ直してください。</p>{/if}
      <div class="row">
        <label><input type="checkbox" bind:checked={editing.triggers.donation} disabled={hooksBusy} /> 投げ銭</label>
        <label><input type="checkbox" bind:checked={editing.triggers.membership} disabled={hooksBusy} /> メンバー加入</label>
        <label><input type="checkbox" bind:checked={editing.triggers.keyword} disabled={hooksBusy} /> キーワード一致</label>
      </div>
      <label>キーワード(1 行 1 語)
        <textarea rows="4" bind:value={formKeywords} disabled={hooksBusy}></textarea>
      </label>
      <p>大文字小文字は区別しません。全角 / 半角・ひらがな / カタカナは区別します。{keywordCount} / {WEBHOOK_KEYWORDS_MAX} 語、各 {WEBHOOK_TEXT_MAX} 文字以内。</p>
      {#if keywordCount > WEBHOOK_KEYWORDS_MAX}<p role="alert">キーワードは {WEBHOOK_KEYWORDS_MAX} 語までです。</p>{/if}
      <label><input type="checkbox" bind:checked={editing.enabled} disabled={hooksBusy} /> 有効にする</label>
      <div class="row">
        <button type="submit" disabled={hooksBusy || !editing.name.trim()}>保存</button>
        <button type="button" disabled={hooksBusy} onclick={cancelEdit}>キャンセル</button>
      </div>
    </form>
  {:else}
    <div class="row">
      <button disabled={hooksBusy || atLimit} onclick={startAdd}>送り先を追加</button>
      {#if atLimit}<span>送り先は {WEBHOOK_MAX} 件までです。追加するには既存の送り先を削除してください。</span>{/if}
    </div>
  {/if}
  <p>Discord へ送る本文は @everyone などで通知が飛ばないよう、メンション無効で送信されます。ブラウザからは呼べません(アプリの画面からだけ設定できます)。</p>
  {#if hooksError}<p role="alert">{hooksError}</p>{/if}
</section>
<style>
  section { border-top: 1px solid #444; margin-top: 20px; padding-top: 12px; }
  h3 { margin: 0 0 10px; }
  p { font-size: 12px; }
  .row { display: flex; flex-wrap: wrap; align-items: center; gap: 6px; margin: 8px 0; }
  .token { overflow-wrap: anywhere; font-family: monospace; }
  pre { overflow-x: auto; font-size: 11px; }
  h4 { margin: 0 0 8px; }
  .hook { border: 1px solid #444; border-radius: 4px; padding: 8px; margin: 8px 0; display: flex; flex-direction: column; gap: 6px; }
  textarea { font-family: inherit; }
</style>
