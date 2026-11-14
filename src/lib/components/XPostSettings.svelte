<script lang="ts">
  import { onDestroy } from 'svelte';
  import { previewXPost } from '../ipc';
  import type { XPostConfig, XPostErrorInfo, XPostPreview } from '../ipc';

  // X 自動告知(Intent モード)の設定欄。値は親の config.xPost を直接編集し、保存は設定画面の保存ボタンで行う。
  // 文字数・警告は Rust の preview_x_post の結果をそのまま表示する(UI では数えない)。
  let { xPost }: { xPost: XPostConfig } = $props();

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
  配信開始を検知すると、タイトル・URL・日時を入れた X の投稿画面をブラウザで開きます。投稿ボタンは自分で押します(無料。ログインや開発者登録は不要)。
  チャンネル一覧の各チャンネルの「告知」を押して印を付けた配信だけが対象です。
</p>

<div class="field-row">
  <label for="xpost-enabled">配信開始を X に告知</label>
  <input id="xpost-enabled" type="checkbox" bind:checked={xPost.enabled} class="chk" />
  <span class="hint-inline">既定はオフ。設定を変えても、すでに始まっている配信にはさかのぼって告知しません</span>
</div>

<div class="field-row">
  <label for="xpost-open">投稿画面の開き方</label>
  <select id="xpost-open" bind:value={xPost.intentOpen} class="platform-select">
    <option value="button">バナーの「告知を投稿」を押した時</option>
    <option value="auto">猶予のあと自動で開く</option>
  </select>
</div>

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

<style>
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
