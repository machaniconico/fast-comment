<script lang="ts">
  import { onDestroy, onMount, tick } from 'svelte';
  import {
    cancelXPostLogin,
    connectXPost,
    disconnectXPost,
    getXPostStatus,
    openUrl,
    previewXPost,
    sendXTestPost,
  } from '../ipc';
  import type { XPostConfig, XPostErrorInfo, XPostPreview, XPostStatus } from '../ipc';
  import { canSelectApiMode, errorText, parseCallbackPort, testPostPreview } from '../xAutopost';

  // X 自動告知の設定欄。値は親の config.xPost を直接編集し、保存は設定画面の保存ボタンで行う。
  // 文字数・警告は Rust の preview_x_post の結果をそのまま表示する(UI では数えない)。
  // API モードの接続(Client ID・ポート)だけは connect_x_post がその場で保存する(YouTube OAuth と同じ流儀)。
  let {
    xPost,
    clientId = '',
    onClientIdSaved,
  }: {
    xPost: XPostConfig;
    clientId?: string;
    onClientIdSaved?: (clientId: string) => void;
  } = $props();

  // ── API モードの接続 ──
  // svelte-ignore state_referenced_locally
  let clientIdInput = $state(clientId);
  // svelte-ignore state_referenced_locally
  let portInput = $state(String(xPost.callbackPort));
  let status = $state<XPostStatus | null>(null);
  let connecting = $state(false);
  let disconnecting = $state(false);
  let authMsg = $state('');
  let authOk = $state<boolean | null>(null);
  let copied = $state(false);
  let testConfirm = $state<string | null>(null);
  let testBusy = $state(false);
  let testMsg = $state('');
  let testOk = $state<boolean | null>(null);
  let testTweetUrl = $state<string | null>(null);
  let confirmCancelEl: HTMLButtonElement | null = $state(null);
  let testButtonEl: HTMLButtonElement | null = $state(null);

  const port = $derived(parseCallbackPort(portInput));
  const displayCallbackUrl = $derived(port !== null ? `http://127.0.0.1:${port}/callback` : '');
  const connected = $derived(canSelectApiMode(status));

  async function refreshStatus() {
    try {
      status = await getXPostStatus();
    } catch (e) {
      status = null;
      authMsg = errorText(e);
      authOk = false;
    }
  }

  onMount(() => {
    void refreshStatus();
  });

  async function connect() {
    if (connecting) return;
    const id = clientIdInput.trim();
    if (!id) {
      authMsg = 'Client ID を入力してください';
      authOk = false;
      return;
    }
    if (port === null) {
      authMsg = 'ポートは 1024〜65535 の整数にしてください';
      authOk = false;
      return;
    }
    connecting = true;
    authMsg = 'ブラウザで X にログインし、アプリを許可してください…';
    authOk = null;
    try {
      // Client ID とポートは Rust 側で保存済み。親の未保存フォームにも反映して、設定保存で戻らないようにする。
      onClientIdSaved?.(id);
      xPost.callbackPort = port;
      status = await connectXPost(id, port);
      authMsg = status?.username ? `@${status.username} として接続しました` : 'X に接続しました';
      authOk = true;
    } catch (e) {
      authMsg = errorText(e);
      authOk = false;
      await refreshStatus();
    } finally {
      connecting = false;
    }
  }

  async function cancelLogin() {
    try {
      await cancelXPostLogin();
    } catch (e) {
      authMsg = errorText(e);
      authOk = false;
    }
  }

  async function disconnect() {
    if (disconnecting) return;
    disconnecting = true;
    try {
      status = await disconnectXPost();
      authMsg = 'X との接続を切りました(この PC の認証を消しました。X 側の許可は X の設定から外せます)';
      authOk = true;
    } catch (e) {
      authMsg = errorText(e);
      authOk = false;
      await refreshStatus();
    } finally {
      // Rust 側は切断の成否によらず API モードを Intent モードへ戻している。手元の未保存フォームも合わせる。
      if (xPost.mode === 'api') xPost.mode = 'intent';
      disconnecting = false;
    }
  }

  async function copyCallbackUrl() {
    if (!displayCallbackUrl) return;
    try {
      await navigator.clipboard.writeText(displayCallbackUrl);
      copied = true;
      setTimeout(() => (copied = false), 1500);
    } catch {
      copied = false;
    }
  }

  async function askTestPost() {
    testConfirm = testPostPreview(new Date());
    await tick();
    confirmCancelEl?.focus();
  }

  function closeConfirm() {
    testConfirm = null;
    void tick().then(() => testButtonEl?.focus());
  }

  async function confirmTestPost() {
    testConfirm = null;
    if (testBusy) return;
    testBusy = true;
    testMsg = '投稿しています…';
    testOk = null;
    testTweetUrl = null;
    try {
      const result = await sendXTestPost();
      testMsg = result ? `投稿しました: ${result.text}` : '投稿しました';
      testOk = true;
      testTweetUrl = result?.tweetId ? `https://x.com/i/status/${encodeURIComponent(result.tweetId)}` : null;
    } catch (e) {
      testMsg = errorText(e);
      testOk = false;
    } finally {
      // 成功時も読み直す(投稿前の refresh で保存に失敗した警告 unsavedToken を出すため)。
      await refreshStatus();
      testBusy = false;
      void tick().then(() => testButtonEl?.focus());
    }
  }

  function onConfirmKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      closeConfirm();
    }
  }


  const VARIABLES = ['{title}', '{url}', '{platform}', '{date}', '{time}'];
  const X_LIMIT = 280;

  let templateEl: HTMLTextAreaElement | null = $state(null);
  let preview = $state<XPostPreview | null>(null);
  let previewError = $state('');
  let previewSeq = 0;
  let timer: ReturnType<typeof setTimeout> | null = null;

  async function refreshPreview(template: string) {
    const seq = ++previewSeq;
    try {
      const result = await previewXPost(template);
      if (seq !== previewSeq) return;
      preview = result;
      previewError = '';
    } catch (e) {
      if (seq !== previewSeq) return;
      preview = null;
      const info = e as Partial<XPostErrorInfo> | null;
      previewError = info && typeof info === 'object' && info.message
        ? info.message
        : e instanceof Error ? e.message : String(e);
    }
  }

  // テンプレートが変わったら 250ms 後にプレビューを取り直す。
  $effect(() => {
    const template = xPost.template;
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => {
      timer = null;
      void refreshPreview(template);
    }, 250);
  });

  onDestroy(() => {
    if (timer) clearTimeout(timer);
    previewSeq++;
  });

  function insertVariable(v: string) {
    const el = templateEl;
    if (!el) {
      xPost.template += v;
      return;
    }
    const start = el.selectionStart ?? xPost.template.length;
    const end = el.selectionEnd ?? start;
    xPost.template = xPost.template.slice(0, start) + v + xPost.template.slice(end);
    const pos = start + v.length;
    queueMicrotask(() => {
      el.focus();
      el.setSelectionRange(pos, pos);
    });
  }

  const over = $derived(preview !== null && preview.weightedLen > X_LIMIT);
</script>

<p class="hint">
  {#if xPost.mode === 'api'}
    配信開始を検知すると、猶予のあと X API で自動的に投稿します(従量課金。下の「X API で自動投稿」で接続が必要)。
  {:else}
    配信開始を検知すると、タイトル・URL・日時を入れた X の投稿画面をブラウザで開きます。投稿ボタンは自分で押します(無料。ログインや開発者登録は不要)。
  {/if}
  チャンネル一覧の各チャンネルの「告知」を押して印を付けた配信だけが対象です。
</p>

<div class="field-row">
  <label for="xpost-mode">投稿の方法</label>
  <select id="xpost-mode" bind:value={xPost.mode} class="platform-select">
    <option value="intent">投稿画面を開く(無料・自分で投稿)</option>
    <option value="api" disabled={!connected && xPost.mode !== 'api'}>X API で自動投稿(有料・要接続)</option>
  </select>
  {#if !connected}
    <span class="hint-inline">API で自動投稿するには、先に下で X に接続してください</span>
  {/if}
</div>
{#if xPost.mode === 'api' && status && !connected}
  <p class="xpost-warn">⚠ X に接続されていないため、API モードでは告知されません。接続し直すか「投稿画面を開く」に戻してください。</p>
{/if}

<div class="field-row">
  <label for="xpost-enabled">配信開始を X に告知</label>
  <input id="xpost-enabled" type="checkbox" bind:checked={xPost.enabled} class="chk" />
  <span class="hint-inline">既定はオフ。設定を変えても、すでに始まっている配信にはさかのぼって告知しません</span>
</div>

{#if xPost.mode !== 'api'}
<div class="field-row">
  <label for="xpost-open">投稿画面の開き方</label>
  <select id="xpost-open" bind:value={xPost.intentOpen} class="platform-select">
    <option value="button">バナーの「告知を投稿」を押した時</option>
    <option value="auto">猶予のあと自動で開く</option>
  </select>
</div>
{/if}

<div class="field-row">
  <label for="xpost-combine">同時配信を 1 件にまとめる</label>
  <input id="xpost-combine" type="checkbox" bind:checked={xPost.combine} class="chk" />
  <span class="hint-inline">オフなら配信ごとに 1 件</span>
</div>

<div class="field-row xpost-template-row">
  <label for="xpost-template">文面テンプレート</label>
  <textarea
    id="xpost-template"
    bind:this={templateEl}
    bind:value={xPost.template}
    rows={4}
    class="xpost-area"
    spellcheck="false"
  ></textarea>
</div>
<div class="field-row">
  <span class="hint-inline">変数を挿入:</span>
  {#each VARIABLES as v (v)}
    <button type="button" class="xpost-var" onclick={() => insertVariable(v)}>{v}</button>
  {/each}
</div>

<div class="xpost-preview" aria-live="polite">
  <div class="xpost-preview-head">
    <span>プレビュー(見本データ)</span>
    {#if preview}
      <span class="xpost-count" class:over>{preview.weightedLen}/{X_LIMIT}</span>
    {/if}
  </div>
  {#if previewError}
    <p class="xpost-error">{previewError}</p>
  {:else if preview}
    <pre class="xpost-preview-text">{preview.text}</pre>
    {#each preview.warnings as w (w)}
      <p class="xpost-warn">⚠ {w}</p>
    {/each}
  {:else}
    <p class="hint">読み込み中…</p>
  {/if}
</div>
<p class="hint">
  毎回同じ文面は X の規約に触れることがあります。タイトルか日時を文面に入れてください。{'{url}'} は必須です。
  280 字を超える時は {'{title}'} だけが「…」で縮みます。
</p>

<div class="field-row">
  <label for="xpost-delay">猶予</label>
  <input id="xpost-delay" type="number" min="0" max="600" step="1" bind:value={xPost.delaySecs} class="num-input" />
  <span class="hint-inline">秒(0〜600。同時配信をまとめる時間と、取消できる時間)</span>
</div>
<div class="field-row">
  <label for="xpost-stale">取りこぼし許容</label>
  <input id="xpost-stale" type="number" min="0" max="180" step="1" bind:value={xPost.staleMinutes} class="num-input" />
  <span class="hint-inline">分(0〜180。配信の途中でアプリを起動した時、開始からこの時間以内なら告知。0 で開始を観測した時だけ)</span>
</div>
<div class="field-row">
  <label for="xpost-cooldown">クールダウン</label>
  <input id="xpost-cooldown" type="number" min="10" max="1440" step="1" bind:value={xPost.cooldownMinutes} class="num-input" />
  <span class="hint-inline">分(10〜1440。直前の告知からこの間は告知しない)</span>
</div>
<div class="field-row">
  <label for="xpost-daily">1 日の上限</label>
  <input id="xpost-daily" type="number" min="1" max="20" step="1" bind:value={xPost.dailyLimit} class="num-input" />
  <span class="hint-inline">件(1〜20)</span>
</div>

<fieldset class="xpost-api">
  <legend>X API で自動投稿(任意)</legend>
  <details class="xpost-steps">
    <summary>準備手順</summary>
    <ol>
      <li>X Developer Console(console.x.com)でアプリを作る。アプリの種類は Native App(公開クライアント)</li>
      <li>User authentication settings の Callback URI に、下のコールバック URL をそのまま登録する(ポートも含めて完全一致)</li>
      <li>権限は Read and write。クレジットを購入する(無料枠はありません)</li>
      <li>表示された OAuth 2.0 Client ID を下に貼り、「X に接続」を押す</li>
    </ol>
  </details>

  <div class="field-row">
    <label for="xpost-client-id">Client ID</label>
    <input id="xpost-client-id" type="text" bind:value={clientIdInput} class="text-input" autocomplete="off" spellcheck="false" />
  </div>
  <div class="field-row">
    <label for="xpost-port">コールバックポート</label>
    <input id="xpost-port" type="text" inputmode="numeric" bind:value={portInput} class="num-input" />
    <span class="hint-inline">既定 47821。変えたら X の登録も直す</span>
  </div>
  <div class="field-row">
    <span class="label-like">コールバック URL</span>
    {#if displayCallbackUrl}
      <code class="xpost-url">{displayCallbackUrl}</code>
      <button type="button" class="btn-small" onclick={copyCallbackUrl}>{copied ? 'コピーしました' : 'コピー'}</button>
    {:else}
      <span class="xpost-error">ポートは 1024〜65535 の整数にしてください</span>
    {/if}
  </div>

  <div class="field-row">
    <span class="label-like">接続</span>
    {#if connected}
      <span class="xpost-connected">@{status?.username ?? '?'} として接続中</span>
    {:else}
      <span class="hint-inline">未接続</span>
    {/if}
  </div>
  {#if status?.unsavedToken}
    <p class="xpost-warn">⚠ 認証の保存に失敗しました。アプリを終了するまでに保存できないと、再ログインが必要になります。</p>
  {/if}
  <div class="field-row">
    {#if connecting}
      <button type="button" class="btn-small" onclick={cancelLogin}>ログインを取り消す</button>
    {:else}
      <button type="button" class="btn-small primary" onclick={connect} disabled={disconnecting}>
        {connected ? '別のアカウントで接続し直す' : 'X に接続'}
      </button>
    {/if}
    {#if connected}
      <button type="button" class="btn-small" onclick={disconnect} disabled={connecting || disconnecting}>切断</button>
    {/if}
  </div>
  {#if authMsg}
    <p class="xpost-msg" class:ok={authOk === true} class:ng={authOk === false} aria-live="polite">{authMsg}</p>
  {/if}

  <div class="field-row">
    <button
      type="button"
      class="btn-small"
      bind:this={testButtonEl}
      onclick={askTestPost}
      disabled={!connected || testBusy || connecting}
    >テスト投稿</button>
    <span class="hint-inline">「fast-comment 連携テスト 日付 時刻」を実際に投稿します(URL なし)。告知の記録・上限には数えません</span>
  </div>
  {#if testMsg}
    <p class="xpost-msg" class:ok={testOk === true} class:ng={testOk === false} aria-live="polite">
      {testMsg}
      {#if testTweetUrl}<button type="button" class="xpost-link" onclick={() => void openUrl(testTweetUrl ?? '')}>ポストを開く</button>{/if}
    </p>
  {/if}

  <ul class="xpost-notes">
    <li>費用の目安(2026-11 時点): 投稿 1 件 $0.015、URL 入りは 1 件 $0.20。告知は URL 入りなので 1 件 $0.20 です。正しい金額は Developer Console で確認してください</li>
    <li>毎回同じ文面は X の規約に触れ、同一文面は X に拒否されます。タイトルか日時を文面に入れてください</li>
    <li>X の許可はアプリ×ユーザーで 1 つです。同じ Client ID で別の PC やツールからログインすると、先の接続は切れます</li>
    <li>しばらく(約 6 か月)使わないと認証が切れます。その時は接続し直してください</li>
  </ul>
</fieldset>

{#if testConfirm !== null}
  <div class="xpost-backdrop" role="presentation">
    <div
      class="xpost-dialog"
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="xpost-test-title"
      aria-describedby="xpost-test-desc"
      tabindex="-1"
      onkeydown={onConfirmKeydown}
    >
      <h4 id="xpost-test-title">テスト投稿しますか?</h4>
      <div id="xpost-test-desc">
        <p>@{status?.username ?? '?'} で次の文面を投稿します。</p>
        <pre class="xpost-preview-text">{testConfirm}</pre>
        <p class="xpost-warn">実際に X で公開され、X API の利用料(1 件 $0.015 の目安)が課金されます。</p>
      </div>
      <div class="xpost-dialog-actions">
        <button type="button" class="btn-small" bind:this={confirmCancelEl} onclick={closeConfirm}>やめる</button>
        <button type="button" class="btn-small primary" onclick={confirmTestPost}>投稿する</button>
      </div>
    </div>
  </div>
{/if}

<style>
  .xpost-api {
    margin-top: 14px;
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 6px;
    padding: 8px 12px 12px;
  }
  .xpost-api legend { font-size: 13px; color: #ccc; padding: 0 4px; }
  .xpost-steps { font-size: 12px; color: #bdbdbd; margin-top: 4px; }
  .xpost-steps ol { margin: 6px 0 0 18px; padding: 0; }
  .xpost-steps li { margin: 2px 0; }
  .label-like { font-size: 13px; color: #ccc; min-width: 90px; }
  .text-input {
    flex: 1 1 260px;
    min-width: 0;
    background: rgba(255,255,255,0.07);
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 4px;
    color: #e0e0e0;
    padding: 5px 8px;
    font-size: 13px;
    font-family: monospace;
  }
  .xpost-url { font-size: 12px; color: #e0e0e0; word-break: break-all; }
  .xpost-connected { font-size: 13px; color: #81c784; font-weight: 600; }
  .btn-small {
    border: 1px solid rgba(255,255,255,0.18);
    background: rgba(255,255,255,0.08);
    color: #e0e0e0;
    border-radius: 4px;
    cursor: pointer;
    font-size: 12px;
    padding: 4px 10px;
  }
  .btn-small:hover:not(:disabled) { background: rgba(255,255,255,0.14); }
  .btn-small:disabled { opacity: 0.5; cursor: not-allowed; }
  .btn-small.primary { background: #1976d2; border-color: #1976d2; color: #fff; }
  .btn-small.primary:hover:not(:disabled) { background: #1e88e5; }
  .xpost-msg { font-size: 12px; margin: 4px 0 0; color: #bdbdbd; }
  .xpost-msg.ok { color: #81c784; }
  .xpost-msg.ng { color: #f44336; }
  .xpost-link {
    background: none;
    border: none;
    color: #64b5f6;
    cursor: pointer;
    font-size: 12px;
    text-decoration: underline;
    padding: 0 0 0 6px;
  }
  .xpost-notes { font-size: 11px; color: #9e9e9e; margin: 8px 0 0 16px; padding: 0; }
  .xpost-notes li { margin: 2px 0; }
  .xpost-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0,0,0,0.55);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
    padding: 16px;
  }
  .xpost-dialog {
    background: #1e1e1e;
    border: 1px solid rgba(255,255,255,0.15);
    border-radius: 8px;
    padding: 16px;
    max-width: 440px;
    width: 100%;
    color: #e0e0e0;
    font-size: 13px;
  }
  .xpost-dialog h4 { margin: 0 0 8px; font-size: 15px; }
  .xpost-dialog p { margin: 6px 0; }
  .xpost-dialog-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 12px; }
  .field-row {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-top: 6px;
    flex-wrap: wrap;
  }
  .field-row label { font-size: 13px; color: #ccc; min-width: 90px; }
  .xpost-template-row { align-items: flex-start; }
  .platform-select, .num-input, .xpost-area {
    background: rgba(255,255,255,0.07);
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 4px;
    color: #e0e0e0;
    padding: 5px 8px;
    font-size: 13px;
  }
  .num-input { width: 90px; }
  .xpost-area { flex: 1 1 260px; min-width: 0; font-family: inherit; resize: vertical; }
  .chk { width: 16px; height: 16px; accent-color: #1976d2; }
  .hint { font-size: 11px; color: #757575; margin: 4px 0 0; }
  .hint-inline { font-size: 11px; color: #757575; font-weight: 400; }
  .xpost-var {
    border: 1px solid rgba(255,255,255,0.15);
    background: rgba(255,255,255,0.06);
    color: #ccc;
    border-radius: 4px;
    cursor: pointer;
    font-size: 11px;
    padding: 2px 8px;
    font-family: monospace;
  }
  .xpost-var:hover { background: rgba(255,255,255,0.12); }
  .xpost-preview {
    margin-top: 8px;
    padding: 8px;
    border: 1px solid rgba(255,255,255,0.1);
    border-radius: 4px;
    background: rgba(255,255,255,0.03);
  }
  .xpost-preview-head {
    display: flex;
    justify-content: space-between;
    font-size: 11px;
    color: #9e9e9e;
  }
  .xpost-count { font-variant-numeric: tabular-nums; }
  .xpost-count.over { color: #f44336; font-weight: 700; }
  .xpost-preview-text {
    margin: 4px 0 0;
    white-space: pre-wrap;
    word-break: break-all;
    font-family: inherit;
    font-size: 13px;
    color: #e0e0e0;
  }
  .xpost-error { color: #f44336; font-size: 12px; margin: 4px 0 0; }
  .xpost-warn { color: #ffb74d; font-size: 11px; margin: 4px 0 0; }
</style>
