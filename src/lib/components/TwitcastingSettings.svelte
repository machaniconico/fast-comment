<script lang="ts">
  import { onMount } from 'svelte';
  import { getTwitcastingStatus, reloadTwitcastingStatus, connectTwitcasting, cancelTwitcastingLogin, disconnectTwitcasting } from '../ipc';
  import type { TwitcastingStatus } from '../types';
  import { errorText, parseCallbackPort } from '../xAutopost';

  let { clientId = '', callbackPort = 39822, onClientIdSaved, onCallbackPortSaved }: {
    clientId?: string;
    callbackPort?: number;
    onClientIdSaved?: (id: string) => void;
    onCallbackPortSaved?: (port: number) => void;
  } = $props();
  // svelte-ignore state_referenced_locally
  let clientIdInput = $state(clientId);
  // svelte-ignore state_referenced_locally
  let portInput = $state(String(callbackPort));
  const port = $derived(parseCallbackPort(portInput));
  let status = $state<TwitcastingStatus | null>(null);
  let connecting = $state(false);
  let disconnecting = $state(false);
  let reloading = $state(false);
  let statusError = $state('');
  let message = $state('');
  let failed = $state(false);
  let now = $state(Date.now());
  // 範囲外の値は Invalid Date になるので、表示できる日時だけを期限として扱う。
  const expiry = $derived.by(() => {
    if (status?.expiresAtMs == null) return null;
    const date = new Date(status.expiresAtMs);
    return Number.isNaN(date.getTime()) ? 'invalid' : date;
  });
  const expiring = $derived(expiry instanceof Date && expiry.getTime() - now <= 14 * 86400000);

  const logoutIssueText: Record<NonNullable<TwitcastingStatus['logoutIssue']>, string> = {
    markerUnavailable: '設定フォルダーまたはローカルデータフォルダーを取得できないため、ツイキャスのログインは使えません。両方のフォルダーを利用できる状態でアプリを起動し直してください。',
    markerInaccessible: '設定フォルダーにアクセスできません。アクセス権を確認してアプリを再起動するか、ログアウトを再試行してください(再試行ではこの PC の認証情報を削除する場合があります)。',
    markerCorrupt: '設定フォルダー内の twitcasting-logout-pending を移動してから、ログアウトを再試行してください。',
    deleteFailed: '保存済みの認証情報を削除できません。ログアウトを再試行してください。',
    markerAndDeleteFailed: 'ログアウトの記録と、保存済みの認証情報の削除に両方失敗しました。設定フォルダーのアクセス権と資格情報の保存先を確認し、ログアウトを再試行してください。',
    timedOut: 'ログアウトの処理が続いています。',
    checkTimedOut: '起動時の確認が時間切れになりました。「再読み込み」を押すか、アプリを再起動してください。ログアウトした覚えがなければ、ログアウトの再試行はしないでください(認証情報を削除する場合があります)。',
    otherLoginKept: '別の場所でのログインは残しました',
    recordUnrecognized: '保存済みの認証情報を照合できなかったため、そのまま残しました。',
    unreadableRecordDeleted: '読み取れなかった認証情報を削除しました。',
    lockTimedOut: '別の場所でツイキャスの認証情報を使用中のため、処理を完了できませんでした。しばらく待ってから、もう一度操作してください。',
  };

  async function refresh() {
    try {
      const next = await getTwitcastingStatus();
      if (!next) throw new Error('アプリからログイン状態を受け取れませんでした。');
      // 削除を伴うログアウトが完了した時だけ知らせる(再検査や別ログインの保持では出さない)。
      if (status && (next.logoutsCompleted ?? 0) > (status.logoutsCompleted ?? 0)) {
        message = 'この PC の認証情報を削除しました';
        failed = false;
      }
      status = next;
      statusError = '';
    }
    catch (e) { statusError = errorText(e); }
    now = Date.now();
  }
  async function retryStatus() {
    reloading = true;
    try { await refresh(); }
    finally { reloading = false; }
  }
  async function reload() {
    reloading = true;
    try {
      status = await reloadTwitcastingStatus();
      statusError = '';
      message = '';
      failed = false;
      now = Date.now();
    } catch (e) { message = errorText(e); failed = true; }
    finally { reloading = false; }
  }
  onMount(() => {
    void refresh();
    const timer = setInterval(() => void refresh(), 30000);
    return () => {
      clearInterval(timer);
      if (connecting) void cancelTwitcastingLogin().catch(() => {});
    };
  });
  async function connect() {
    if (connecting || disconnecting || reloading || !status || status.state === 'loading') return;
    const callbackPort = port;
    if (callbackPort === null) {
      message = 'ポートは 1024〜65535 の整数にしてください';
      failed = true;
      return;
    }
    connecting = true;
    failed = false;
    message = 'ブラウザでツイキャスにログインし、アプリを許可してください…';
    const id = clientIdInput.trim();
    // コマンドはログインに先立って ClientID と待受ポートだけを保存する。親の未保存フォームにも反映する。
    onClientIdSaved?.(id);
    onCallbackPortSaved?.(callbackPort);
    try {
      status = await connectTwitcasting(id, callbackPort);
      statusError = '';
      message = 'ツイキャスにログインしました';
    } catch (e) { message = errorText(e); failed = true; await refresh(); }
    finally { connecting = false; }
  }
  async function cancel() {
    try { await cancelTwitcastingLogin(); }
    catch (e) { message = errorText(e); failed = true; }
  }
  async function disconnect() {
    disconnecting = true;
    const completed = status?.logoutsCompleted ?? 0;
    try {
      status = await disconnectTwitcasting();
      statusError = '';
      message = (status?.logoutsCompleted ?? 0) > completed ? 'この PC の認証情報を削除しました' : '';
      failed = false;
    }
    catch (e) { message = errorText(e); failed = true; await refresh(); }
    finally { disconnecting = false; }
  }
</script>

<fieldset>
  <legend>ツイキャス</legend>
  <div class="field-row">
    {#if !status && statusError}
      <span class="warn">ログイン状態を取得できません</span>
    {:else if status?.logoutIssue === 'markerUnavailable'}
      <span class="warn">ツイキャスのログインを利用できません</span>
    {:else if status?.logoutIssue === 'checkTimedOut'}
      <span class="warn">ログイン状態を確認できません</span>
    {:else if status?.logoutPending}
      <span class="warn">ログアウトが未完了です</span>
    {:else if status?.state === 'ready'}
      <span class="connected">@{status.screenId} ({status.name}) としてログイン中</span>
    {:else if status?.state === 'reloginRequired'}
      <span class="warn">再ログインしてください</span>
    {:else if status?.state === 'loadFailed'}
      <span class="warn">保存済みの認証情報を読み込めません</span>
    {:else if !status || status.state === 'loading'}
      <span>保存済みの認証情報を読み込んでいます…</span>
    {:else}
      <span>未ログイン</span>
    {/if}
  </div>
  {#if statusError}
    <p class="warn" role="status">{statusError}</p>
  {/if}
  {#if status?.logoutIssue}
    <p class="warn">{logoutIssueText[status.logoutIssue]}</p>
  {/if}
  {#if expiry === 'invalid'}
    <p class="warn">有効期限を読み取れません。再ログインしてください。</p>
  {:else if expiry}
    <p class:warn={expiring}>有効期限: {expiry.toLocaleString()}{expiring ? ' (期限が近いか、期限切れです。再ログインしてください)' : ''}</p>
  {:else if status?.state === 'ready'}
    <p>有効期限は不明です。認証が切れたら再ログインしてください。</p>
  {/if}
  <div class="field-row">
    {#if connecting}
      <button type="button" onclick={cancel}>取り消す</button>
    {:else}
      <button type="button" class="primary" onclick={connect} disabled={disconnecting || reloading || !status || status.state === 'loading' || status.logoutPending}>ログイン</button>
    {/if}
    {#if statusError}
      <button type="button" onclick={retryStatus} disabled={connecting || disconnecting || reloading}>{status ? 'ログイン状態を再取得' : '再読み込み'}</button>
    {/if}
    {#if status?.logoutPending}
      {#if status.logoutIssue === 'checkTimedOut'}
        <button type="button" onclick={reload} disabled={connecting || disconnecting || reloading}>再読み込み</button>
      {/if}
      <button type="button" onclick={disconnect} disabled={connecting || disconnecting || reloading}>ログアウトを再試行</button>
    {:else if status?.state === 'ready' || status?.state === 'reloginRequired' || status?.state === 'loadFailed'}
      <button type="button" onclick={disconnect} disabled={connecting || disconnecting || reloading}>ログアウト</button>
      {#if status.state === 'loadFailed'}
        <button type="button" onclick={reload} disabled={connecting || disconnecting || reloading}>認証情報を再読み込み</button>
      {/if}
    {/if}
  </div>
  <div aria-live="polite">{#if message}<p class:warn={failed}>{message}</p>{/if}</div>
  <details>
    <summary>詳細: 自分の ClientID を使う</summary>
    <div class="field-row">
      <label for="twitcasting-client-id">ClientID</label>
      <input id="twitcasting-client-id" type="text" bind:value={clientIdInput} disabled={connecting || disconnecting} autocomplete="off" spellcheck="false" />
    </div>
    <div class="field-row">
      <label for="twitcasting-port">待受ポート</label>
      <input id="twitcasting-port" type="text" inputmode="numeric" bind:value={portInput} disabled={connecting || disconnecting} autocomplete="off" />
    </div>
    {#if port === null}
      <p class="warn">ポートは 1024〜65535 の整数にしてください</p>
    {/if}
    <p>空欄ならアプリ既定値を使います。現在は既定値が未登録のため、自分の ClientID が必要です。ログイン時に ClientID と待受ポートだけ保存します。待受ポートの既定は 39822 です。変えたら開発者ページのコールバック URL も登録し直してください。</p>
    <p>ツイキャスの開発者ページでアプリを登録し、次のコールバック URL をそのまま設定してください。</p>
    <code>{status?.callbackUrl ?? 'http://localhost:39822/twitcasting/callback'}</code>
  </details>
  <p>認証の有効期間は通常約 180 日です。ログアウト後は、ツイキャスの連携アプリ設定から解除できます。</p>
</fieldset>

<style>
  fieldset { margin-top: 14px; border: 1px solid rgba(255,255,255,0.12); border-radius: 6px; padding: 8px 12px 12px; color: #ccc; }
  legend { font-size: 13px; padding: 0 4px; }
  .field-row { display: flex; align-items: center; gap: 8px; margin: 8px 0; flex-wrap: wrap; font-size: 13px; }
  p, details, code { font-size: 12px; }
  p { color: #bdbdbd; margin: 6px 0; }
  code { word-break: break-all; }
  input { flex: 1 1 260px; min-width: 0; background: rgba(255,255,255,0.07); border: 1px solid rgba(255,255,255,0.12); border-radius: 4px; color: #e0e0e0; padding: 5px 8px; font: 13px monospace; }
  button { border: 1px solid rgba(255,255,255,0.18); background: rgba(255,255,255,0.08); color: #e0e0e0; border-radius: 4px; cursor: pointer; font-size: 12px; padding: 4px 10px; }
  button:hover:not(:disabled) { background: rgba(255,255,255,0.14); }
  button:disabled { opacity: 0.5; cursor: not-allowed; }
  button.primary { background: #1976d2; border-color: #1976d2; color: #fff; }
  .connected { color: #81c784; }
  .warn { color: #ffb74d; }
</style>
