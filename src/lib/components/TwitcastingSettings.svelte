<script lang="ts">
  import { onMount } from 'svelte';
  import { getTwitcastingStatus, reloadTwitcastingStatus, connectTwitcasting, cancelTwitcastingLogin, disconnectTwitcasting } from '../ipc';
  import type { TwitcastingStatus } from '../types';
  import { errorText } from '../xAutopost';

  let { clientId = '', onClientIdSaved }: { clientId?: string; onClientIdSaved?: (id: string) => void } = $props();
  // svelte-ignore state_referenced_locally
  let clientIdInput = $state(clientId);
  let status = $state<TwitcastingStatus | null>(null);
  let connecting = $state(false);
  let disconnecting = $state(false);
  let reloading = $state(false);
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
    markerUnavailable: '設定フォルダーを取得できません。利用できる状態でアプリを起動し直してください。',
    markerInaccessible: '設定フォルダーとアクセス権を確認し、ログアウトを再試行してください。',
    markerCorrupt: '設定フォルダー内の twitcasting-logout-pending を移動してから、ログアウトを再試行してください。',
    deleteFailed: '保存済みの認証情報を削除できません。ログアウトを再試行してください。',
    timedOut: 'ログアウトの処理が続いています。',
  };

  async function refresh() {
    try {
      const next = await getTwitcastingStatus();
      if (status?.logoutPending && next && !next.logoutPending) {
        message = 'この PC の認証情報を削除しました';
        failed = false;
      }
      status = next;
    }
    catch (e) { message = errorText(e); failed = true; }
    now = Date.now();
  }
  async function reload() {
    reloading = true;
    try {
      status = await reloadTwitcastingStatus();
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
    if (connecting || disconnecting || reloading) return;
    connecting = true;
    failed = false;
    message = 'ブラウザでツイキャスにログインし、アプリを許可してください…';
    const id = clientIdInput.trim();
    // コマンドはログインに先立って ClientID だけを保存する。親の未保存フォームにも反映する。
    onClientIdSaved?.(id);
    try {
      status = await connectTwitcasting(id);
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
    try { status = await disconnectTwitcasting(); message = 'この PC の認証情報を削除しました'; failed = false; }
    catch (e) { message = errorText(e); failed = true; await refresh(); }
    finally { disconnecting = false; }
  }
</script>

<fieldset>
  <legend>ツイキャス</legend>
  <div class="field-row">
    {#if status?.logoutPending}
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
      <button type="button" class="primary" onclick={connect} disabled={disconnecting || reloading || status?.logoutPending}>ログイン</button>
    {/if}
    {#if status?.logoutPending}
      <button type="button" onclick={disconnect} disabled={disconnecting}>ログアウトを再試行</button>
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
    <p>空欄ならアプリ既定値を使います。現在は既定値が未登録のため、自分の ClientID が必要です。ログイン時にこの項目だけ保存します。</p>
    <p>ツイキャスの開発者ページでアプリを登録し、次のコールバック URL をそのまま設定してください。</p>
    <code>{status?.callbackUrl ?? 'http://localhost:47822/twitcasting/callback'}</code>
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
