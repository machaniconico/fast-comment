<script lang="ts">
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import type { LocalApiStatus } from '../types';
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
<style>
  section { border-top: 1px solid #444; margin-top: 20px; padding-top: 12px; }
  h3 { margin: 0 0 10px; }
  p { font-size: 12px; }
  .row { display: flex; flex-wrap: wrap; align-items: center; gap: 6px; margin: 8px 0; }
  .token { overflow-wrap: anywhere; font-family: monospace; }
  pre { overflow-x: auto; font-size: 11px; }
</style>
