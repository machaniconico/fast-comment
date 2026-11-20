<script lang="ts">
  import { onMount } from 'svelte';
  import { getTwitcastingStatus, reloadTwitcastingStatus, connectTwitcasting, cancelTwitcastingLogin, disconnectTwitcasting, newOAuthAttempt } from '../ipc';
  import type { TwitcastingStatus } from '../types';
  import { errorText } from '../xAutopost';
  import { DEFAULT_TWITCASTING_CALLBACK_PORT, twitcastingDisplayCallbackUrl } from '../twitcasting';
  import { createCallbackPortInput } from '../callbackPortInput.svelte';

  let {
    clientId = '', callbackPort = DEFAULT_TWITCASTING_CALLBACK_PORT, configRevision,
    saving, allFieldsLocked, configReloading, onCallbackPortChange, onConnectingChange, onConnectionSettled,
  }: {
    clientId?: string;
    callbackPort?: number;
    configRevision: number;
    saving: boolean;
    allFieldsLocked: boolean;
    configReloading: boolean;
    onCallbackPortChange: (port: number | null) => void;
    onConnectingChange: (connecting: boolean) => void;
    onConnectionSettled: () => Promise<void>;
  } = $props();
  let clientIdInput = $state('');
  $effect.pre(() => {
    configRevision;
    clientIdInput = clientId;
  });
  let attempt = $state<string | null>(null);
  const portInput = createCallbackPortInput(
    () => callbackPort, () => configRevision, (port) => onCallbackPortChange(port),
  );
  // 待受中はそのポート、それ以外は入力中の有効値から登録用 URL を出す。
  let listeningPort = $state<number | null>(null);
  const callbackUrl = $derived(twitcastingDisplayCallbackUrl(listeningPort, portInput.port));
  let latestStatus = $state<TwitcastingStatus | null>(null);
  let statusRequest = 0;
  let connecting = $state(false);
  const status = $derived(!connecting && latestStatus?.clientId === clientId.trim() ? latestStatus : null);
  let disconnecting = $state(false);
  let reloading = $state(false);
  const authControlsLocked = $derived(allFieldsLocked || connecting || disconnecting || reloading);
  const fieldsLocked = $derived(allFieldsLocked || configReloading || connecting || disconnecting);
  let statusError = $state('');
  let message = $state('');
  let successClientId = $state<string | null>(null);
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
    const request = ++statusRequest;
    try {
      const next = await getTwitcastingStatus();
      if (request !== statusRequest) return;
      if (!next) throw new Error('アプリからログイン状態を受け取れませんでした。');
      // 削除を伴うログアウトが完了した時だけ知らせる(再検査や別ログインの保持では出さない)。
      if (latestStatus && (next.logoutsCompleted ?? 0) > (latestStatus.logoutsCompleted ?? 0)) {
        successClientId = null;
        message = 'この PC の認証情報を削除しました';
        failed = false;
      }
      latestStatus = next;
      statusError = '';
    }
    catch (e) {
      if (request === statusRequest) statusError = errorText(e);
    }
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
      latestStatus = await reloadTwitcastingStatus();
      statusError = '';
      message = '';
      failed = false;
      now = Date.now();
    } catch (e) { message = errorText(e); failed = true; }
    finally { reloading = false; }
  }
  $effect(() => {
    clientId;
    void refresh();
  });
  onMount(() => {
    const timer = setInterval(() => void refresh(), 30000);
    return () => {
      clearInterval(timer);
      if (attempt) void cancelTwitcastingLogin(attempt).catch(() => {});
    };
  });
  async function connect() {
    if (saving || configReloading || authControlsLocked || !status || status.state === 'loading') return;
    const listenPort = portInput.port;
    if (listenPort === null) {
      message = 'ポートは 1024〜65535 の整数にしてください';
      failed = true;
      return;
    }
    const current = attempt = newOAuthAttempt();
    connecting = true;
    onConnectingChange(true);
    listeningPort = listenPort;
    failed = false;
    successClientId = null;
    message = 'ブラウザでツイキャスにログインし、アプリを許可してください…';
    try {
      const next = await connectTwitcasting(clientIdInput.trim(), listenPort, current);
      if (!next) throw new Error('アプリからログイン結果を受け取れませんでした。');
      statusRequest += 1;
      latestStatus = next;
      successClientId = next.clientId;
      statusError = '';
      message = 'ツイキャスにログインしました';
    } catch (e) {
      statusRequest += 1;
      latestStatus = null;
      message = errorText(e);
      failed = true;
    } finally {
      attempt = null;
      listeningPort = null;
      try {
        await onConnectionSettled();
      } finally {
        connecting = false;
        onConnectingChange(false);
      }
      if (failed) void refresh();
    }
  }
  async function cancel() {
    if (!attempt) return;
    try { await cancelTwitcastingLogin(attempt); }
    catch (e) { message = errorText(e); failed = true; }
  }
  async function disconnect() {
    disconnecting = true;
    const completed = status?.logoutsCompleted ?? 0;
    successClientId = null;
    try {
      latestStatus = await disconnectTwitcasting();
      statusError = '';
      message = (latestStatus?.logoutsCompleted ?? 0) > completed ? 'この PC の認証情報を削除しました' : '';
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
    {:else if !connecting && (!status || status.state === 'loading')}
      <span>保存済みの認証情報を読み込んでいます…</span>
    {:else if status}
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
    {#if attempt}
      <button type="button" onclick={cancel}>取り消す</button>
    {:else}
      <button type="button" class="primary" onclick={connect} disabled={saving || configReloading || authControlsLocked || !status || status.state === 'loading' || status.logoutPending}>ログイン</button>
    {/if}
    {#if statusError}
      <button type="button" onclick={retryStatus} disabled={authControlsLocked}>{status ? 'ログイン状態を再取得' : '再読み込み'}</button>
    {/if}
    {#if status?.logoutPending}
      {#if status.logoutIssue === 'checkTimedOut'}
        <button type="button" onclick={reload} disabled={authControlsLocked}>再読み込み</button>
      {/if}
      <button type="button" onclick={disconnect} disabled={authControlsLocked}>ログアウトを再試行</button>
    {:else if status?.state === 'ready' || status?.state === 'reloginRequired' || status?.state === 'loadFailed'}
      <button type="button" onclick={disconnect} disabled={authControlsLocked}>ログアウト</button>
      {#if status.state === 'loadFailed'}
        <button type="button" onclick={reload} disabled={authControlsLocked}>認証情報を再読み込み</button>
      {/if}
    {/if}
  </div>
  <div aria-live="polite">{#if message && (successClientId === null || successClientId === clientId.trim())}<p class:warn={failed}>{message}</p>{/if}</div>
  <details>
    <summary>詳細: 自分の ClientID を使う</summary>
    <div class="field-row">
      <label for="twitcasting-client-id">ClientID</label>
      <input id="twitcasting-client-id" type="text" bind:value={clientIdInput} disabled={fieldsLocked} autocomplete="off" spellcheck="false" />
    </div>
    <div class="field-row">
      <label for="twitcasting-port">待受ポート</label>
      <input id="twitcasting-port" type="text" inputmode="numeric" value={portInput.text} oninput={portInput.onInput} disabled={fieldsLocked} autocomplete="off" />
    </div>
    {#if portInput.port === null}
      <p class="warn">ポートは 1024〜65535 の整数にしてください</p>
    {/if}
    <p>空欄ならアプリ既定値を使います。現在は既定値が未登録のため、自分の ClientID が必要です。ClientID はログイン時に保存します。待受ポート(既定 {DEFAULT_TWITCASTING_CALLBACK_PORT})はログイン時と「設定を保存」の両方で保存します。変えたら開発者ページのコールバック URL も登録し直してください。</p>
    {#if callbackUrl}
      <p>ツイキャスの開発者ページでアプリを登録し、次のコールバック URL をそのまま設定してください。</p>
      <code>{callbackUrl}</code>
    {/if}
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
