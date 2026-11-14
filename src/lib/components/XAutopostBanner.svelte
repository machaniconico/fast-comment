<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import {
    getXAutopostStatus,
    onXAutopostStatus,
    cancelXAutopost,
    approveXAutopost,
    retryXAutopost,
    openUrl,
  } from '../ipc';
  import type { XAutopostStatus } from '../ipc';

  // X 自動告知の状態バナー。状態は Rust 側が決め、ここは表示と操作の送信だけ行う。
  // mode は待機中の文言(投稿画面を開く / 投稿する)を分けるためだけに使う。
  let { mode = 'intent' }: { mode?: string } = $props();
  let status = $state<XAutopostStatus | null>(null);
  // 一時表示の状態を自動で隠すための印(status の参照ごとに 1 回)。
  let hidden = $state(false);
  let now = $state(Date.now());
  let actionError = $state('');
  // 操作の送信中は同じバナーのボタンを押せなくする(二度押し防止)。
  let busy = $state(false);
  // 購読後に届いたイベントがあれば、起動時取得の古い結果で上書きしない。
  let gotEvent = false;
  let hideTimer: ReturnType<typeof setTimeout> | null = null;
  let tickTimer: ReturnType<typeof setInterval> | null = null;
  let unlisten: (() => void) | null = null;
  let destroyed = false;

  const TRANSIENT_MS: Record<string, number> = {
    opened: 15000,
    posted: 15000,
    skipped: 6000,
    cancelled: 3000,
  };
  // 理由付きの取消(期限切れなど)はユーザー操作ではないので長めに出す。
  const CANCELLED_WITH_REASON_MS = 8000;

  const REASON_LABELS: Record<string, string> = {
    stale: '配信開始から時間が経っているため告知しませんでした',
    unknownStart: '開始時刻が分からない配信のため告知しませんでした',
    cooldown: '直前に告知したため告知しませんでした(クールダウン中)',
    dailyLimit: '1 日の告知上限に達したため告知しませんでした',
    notConnected: 'X に接続されていないため告知できません',
    expired: '承認待ちの期限が切れたため告知しませんでした',
    ledgerFailed: '告知の記録ファイルを読み書きできないため、自動告知を停止しました',
    openFailed: 'ブラウザで X の投稿画面を開けませんでした',
    Duplicate: '直近と同じ文面のため告知できません。テンプレートに日時やタイトルを入れてください',
    TooLong: '文面が 280 字を超えるため告知できません',
    MissingUrl: 'テンプレートに {url} がありません',
    Uncertain: '投稿されたか不明です。X を確認してください',
    Network: '通信に失敗しました',
    ReloginRequired: 'X への再ログインが必要です',
    NotLoggedIn: 'X に接続されていません',
    Forbidden: 'X に拒否されました',
    InsufficientCredits: 'X のクレジットが不足しています。Developer Console でクレジットを購入してください',
    RateLimited: 'X のレート制限に達しました。しばらく待ってください',
    Cancelled: '取り消されました',
    PortInUse: 'ログイン待受のポートが使用中です',
    LoginTimeout: 'X へのログインが時間切れになりました',
    TokenStore: 'X の認証を保存・読み込みできませんでした',
    Other: '告知に失敗しました',
  };

  function reasonText(reason: string | null | undefined): string {
    if (!reason) return '';
    return REASON_LABELS[reason] ?? reason;
  }

  function apply(next: XAutopostStatus) {
    status = next;
    hidden = false;
    actionError = '';
    clearBusy();
    if (hideTimer) {
      clearTimeout(hideTimer);
      hideTimer = null;
    }
    const ms = next.state === 'cancelled' && next.reason ? CANCELLED_WITH_REASON_MS : TRANSIENT_MS[next.state];
    if (ms) {
      hideTimer = setTimeout(() => {
        hidden = true;
        hideTimer = null;
      }, ms);
    }
  }

  onMount(async () => {
    tickTimer = setInterval(() => {
      now = Date.now();
    }, 1000);
    // 先に購読してから現在の状態を取る(間に emit された遷移を落とさない)。
    const fn = await onXAutopostStatus((s) => {
      if (destroyed) return;
      gotEvent = true;
      apply(s);
    });
    if (destroyed) {
      fn();
      return;
    }
    unlisten = fn;
    try {
      const current = await getXAutopostStatus();
      if (current && !destroyed && !gotEvent) apply(current);
    } catch (e) {
      console.warn('[x-autopost] status load failed', e);
    }
  });

  onDestroy(() => {
    destroyed = true;
    unlisten?.();
    if (hideTimer) clearTimeout(hideTimer);
    if (tickTimer) clearInterval(tickTimer);
    if (busyTimer) clearTimeout(busyTimer);
  });

  const visible = $derived(status !== null && status.state !== 'idle' && !hidden);

  function remainingSecs(at: number | null | undefined): number {
    if (at == null) return 0;
    return Math.max(0, Math.ceil((at - now) / 1000));
  }

  const fireSecs = $derived(remainingSecs(status?.fireAtMs));
  const expireSecs = $derived(remainingSecs(status?.expiresAtMs));

  // 送信が成功しても、次の状態が届くまで(最長 BUSY_MAX_MS)ボタンを戻さない。
  // 戻すのが早いと、二度目のクリックが入れ替わった次のバナーに当たる。
  const BUSY_MAX_MS = 2000;
  let busyTimer: ReturnType<typeof setTimeout> | null = null;

  function clearBusy() {
    busy = false;
    if (busyTimer) {
      clearTimeout(busyTimer);
      busyTimer = null;
    }
  }

  async function run(action: () => Promise<void>) {
    if (busy) return;
    busy = true;
    actionError = '';
    try {
      await action();
      if (busy && !destroyed) busyTimer = setTimeout(clearBusy, BUSY_MAX_MS);
    } catch (e) {
      actionError = e instanceof Error ? e.message : String(e);
      clearBusy();
    }
  }

  // バナーに出ている告知の ID で送る(状態が先に進んでいたら Rust 側が無視する)。
  function approveShown(id: number | null | undefined) {
    if (id == null) return;
    void run(() => approveXAutopost(id));
  }

  function cancelShown(id: number | null | undefined) {
    if (id == null) return;
    void run(() => cancelXAutopost(id));
  }

  function dismiss() {
    hidden = true;
  }

  const tweetUrl = $derived(status?.tweetId ? `https://x.com/i/status/${encodeURIComponent(status.tweetId)}` : null);
</script>

{#if visible && status}
  {@const s = status}
  <div
    class="xpost-banner"
    class:ok={s.state === 'opened' || s.state === 'posted'}
    class:warn={s.state === 'failed' || s.state === 'uncertain'}
    role={s.state === 'failed' || s.state === 'uncertain' ? 'alert' : 'status'}
    aria-label="X 告知"
  >
    <span class="xpost-label">X 告知</span>
    <div class="xpost-body">
      {#if s.state === 'pending'}
        <span class="xpost-msg">{mode === 'api' ? `${fireSecs} 秒後に X へ告知します(取消できます)` : `${fireSecs} 秒後に X の投稿画面を開きます`}</span>
      {:else if s.state === 'awaitingApproval'}
        <span class="xpost-msg">配信開始を検知しました。投稿画面を開く準備ができています{expireSecs > 0 ? `(あと ${expireSecs} 秒で失効)` : ''}</span>
      {:else if s.state === 'posting'}
        <span class="xpost-msg">投稿しています…</span>
      {:else if s.state === 'opened'}
        <span class="xpost-msg">X の投稿画面を開きました。投稿ボタンを押してください(投稿されたかはアプリでは確認できません)</span>
      {:else if s.state === 'posted'}
        <span class="xpost-msg">
          投稿しました
          {#if tweetUrl}<button type="button" class="xpost-link" onclick={() => void openUrl(tweetUrl)}>ポストを開く</button>{/if}
        </span>
      {:else if s.state === 'failed'}
        <span class="xpost-msg">告知に失敗しました。{reasonText(s.reason)}</span>
      {:else if s.state === 'uncertain'}
        <span class="xpost-msg">{reasonText(s.reason) || '投稿されたか不明です。X を確認し、必要なら再試行してください'}</span>
      {:else if s.state === 'cancelled'}
        <span class="xpost-msg">{reasonText(s.reason) || '告知を取り消しました'}</span>
      {:else if s.state === 'skipped'}
        <span class="xpost-msg">{reasonText(s.reason) || '告知しませんでした'}</span>
      {/if}
      {#if s.text && (s.state === 'pending' || s.state === 'awaitingApproval' || s.state === 'opened' || s.state === 'failed' || s.state === 'uncertain')}
        <span class="xpost-text" title={s.text}>{s.text}</span>
      {/if}
      {#if actionError}<span class="xpost-err">{actionError}</span>{/if}
    </div>
    <div class="xpost-actions">
      {#if s.state === 'awaitingApproval'}
        <button type="button" class="xpost-btn primary" disabled={busy} onclick={() => approveShown(s.id)}>告知を投稿</button>
      {/if}
      {#if s.state === 'pending' || s.state === 'awaitingApproval'}
        <button type="button" class="xpost-btn" disabled={busy} onclick={() => cancelShown(s.id)}>取消</button>
      {/if}
      {#if (s.state === 'failed' || s.state === 'uncertain') && s.canRetry}
        <button type="button" class="xpost-btn primary" disabled={busy} onclick={() => run(retryXAutopost)}>再試行</button>
      {/if}
      {#if s.state !== 'pending' && s.state !== 'awaitingApproval' && s.state !== 'posting'}
        <button type="button" class="xpost-close" title="閉じる" aria-label="X 告知バナーを閉じる" onclick={dismiss}>×</button>
      {/if}
    </div>
  </div>
{/if}

<style>
  .xpost-banner {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 8px;
    background: #1d2733;
    border-bottom: 1px solid rgba(255, 255, 255, 0.06);
    border-left: 3px solid #1da1f2;
    font-size: 12px;
    color: #ddd;
    flex-shrink: 0;
  }
  :global(.app[data-theme='light']) .xpost-banner { background: #e8f1fa; color: #1f2937; }
  .xpost-banner.ok { border-left-color: #66bb6a; }
  .xpost-banner.warn { border-left-color: #ffb74d; }
  .xpost-label {
    font-weight: 700;
    color: #1da1f2;
    flex-shrink: 0;
  }
  .xpost-body {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
    flex: 1;
  }
  .xpost-text {
    color: #9aa4ad;
    font-size: 11px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .xpost-err { color: #f44336; font-size: 11px; }
  .xpost-actions { display: flex; align-items: center; gap: 6px; flex-shrink: 0; }
  .xpost-btn {
    border: none;
    border-radius: 4px;
    cursor: pointer;
    font-size: 12px;
    font-weight: 600;
    padding: 4px 10px;
    background: #37474f;
    color: #fff;
  }
  .xpost-btn.primary { background: #1976d2; }
  .xpost-btn:hover { opacity: 0.9; }
  .xpost-btn:disabled { opacity: 0.5; cursor: default; }
  .xpost-close {
    border: none;
    background: none;
    color: #888;
    cursor: pointer;
    font-size: 14px;
    line-height: 1;
    padding: 0 2px;
  }
  .xpost-close:hover { color: #f44336; }
  .xpost-link {
    border: none;
    background: none;
    color: #64b5f6;
    cursor: pointer;
    font-size: 12px;
    text-decoration: underline;
    padding: 0 4px;
  }
</style>
