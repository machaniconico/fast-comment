<script lang="ts">
  import { onMount } from 'svelte';
  import {
    getStartupLaunchSettings,
    isTauri,
    saveStartupLaunchSettings,
    type LaunchItem,
    type LaunchKind,
  } from '../ipc';

  let { onApprovalChanged }: { onApprovalChanged?: (needsApproval: boolean) => void } = $props();

  const desktop = isTauri();
  let items = $state.raw<LaunchItem[]>([]);
  let kind = $state<LaunchKind>('exe');
  let target = $state('');
  let argsText = $state('');
  let hideWindow = $state(false);
  let label = $state('');
  let loading = $state(desktop);
  let loaded = $state(false);
  let saving = $state(false);
  let needsApproval = $state(false);
  let readError = $state<string | null>(null);
  let error = $state('');
  let saved = $state(false);
  let dirty = $state(false);
  let reviewItems = $state.raw<LaunchItem[]>([]);
  let dialog: HTMLDialogElement;

  onMount(() => {
    if (desktop) void load();
  });

  async function load() {
    loading = true;
    error = '';
    try {
      const result = await getStartupLaunchSettings();
      if (!result) return;
      items = result.items;
      setNeedsApproval(result.needsApproval);
      readError = result.readError;
      loaded = true;
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  function setNeedsApproval(value: boolean) {
    needsApproval = value;
    onApprovalChanged?.(value);
  }

  function updateItems(next: LaunchItem[]) {
    items = next;
    dirty = true;
    error = '';
    saved = false;
  }

  function add(event: SubmitEvent) {
    event.preventDefault();
    const args = kind === 'exe' ? argsText.split('\n') : [];
    while (args.at(-1) === '') args.pop();
    updateItems([...items, {
      id: crypto.randomUUID(),
      kind,
      target: kind === 'exe' ? target.replace(/^"(.*)"$/s, '$1') : target,
      args,
      enabled: true,
      label: label === '' ? null : label,
      hideWindow: kind === 'exe' && hideWindow,
    }]);
    target = '';
    argsText = '';
    hideWindow = false;
    label = '';
  }

  function move(index: number, step: -1 | 1) {
    const next = [...items];
    const destination = index + step;
    [next[index], next[destination]] = [next[destination], next[index]];
    updateItems(next);
  }

  function review() {
    reviewItems = items;
    dialog.showModal();
  }

  async function approve() {
    dialog.close();
    saving = true;
    saved = false;
    error = '';
    try {
      await saveStartupLaunchSettings(reviewItems);
      dirty = false;
      setNeedsApproval(false);
      readError = null;
      saved = true;
    } catch (e) {
      error = String(e);
      // A credential-store error can leave the updated file waiting for approval.
      // Keep the draft while refreshing only the approval state.
      try {
        const result = await getStartupLaunchSettings();
        if (result) {
          setNeedsApproval(result.needsApproval);
          readError = result.readError;
        }
      } catch {
        // The save error remains visible and the draft can still be retried.
      }
    } finally {
      saving = false;
    }
  }
</script>

{#if !desktop}
  <p>デスクトップ版で設定できます。</p>
{:else if loading}
  <p role="status">読み込み中…</p>
{:else if !loaded}
  <p class="warning" role="alert">{error}</p>
  <button type="button" onclick={load}>再読み込み</button>
{:else}
  {#if needsApproval}
    <p class="warning" role="alert">
      起動時に開く設定が変更されています。設定画面で確認してください。
      {#if readError}<br />{readError}{/if}
    </p>
  {/if}

  <fieldset disabled={saving}>
    <form class="add" onsubmit={add}>
      <label>種類
        <select bind:value={kind}>
          <option value="exe">アプリ</option>
          <option value="url">URL</option>
        </select>
      </label>
      <label>{kind === 'exe' ? 'アプリの絶対パス(.exe / .bat / .cmd)' : 'URL'}
        <input type="text" bind:value={target} spellcheck="false" />
      </label>
      {#if kind === 'exe'}
        <label>引数（1 行に 1 つ）
          <textarea rows="2" bind:value={argsText} spellcheck="false"></textarea>
        </label>
        <label class="check"><input type="checkbox" bind:checked={hideWindow} />ウィンドウを隠す(コンソール窓を出さない)</label>
      {/if}
      <label>表示名（任意）
        <input type="text" bind:value={label} />
      </label>
      <button type="submit">一覧に追加</button>
    </form>

    {#if items.length === 0}
      <p>登録項目はありません。</p>
    {:else}
      <ol class="items">
        {#each items as item, index}
          <li>
            <div>
              <strong>{item.kind === 'exe' ? 'アプリ' : 'URL'}{item.label ? ` ・ ${item.label}` : ''}</strong>
              <div class="literal" dir="ltr">{item.target}</div>
              <label class="rename">表示名
                <input type="text" maxlength="100" placeholder="（なし）" value={item.label ?? ''}
                  onchange={(event) => {
                    const value = event.currentTarget.value;
                    updateItems(items.map((entry, i) =>
                      i === index ? { ...entry, label: value === '' ? null : value } : entry
                    ));
                  }} />
              </label>
              {#if item.kind === 'exe'}
                <label><input type="checkbox" checked={item.hideWindow}
                  onchange={(event) => updateItems(items.map((entry, i) =>
                    i === index ? { ...entry, hideWindow: event.currentTarget.checked } : entry
                  ))} />ウィンドウを隠す(コンソール窓を出さない)</label>
              {/if}
            </div>
            <div class="actions">
              <label><input type="checkbox" checked={item.enabled}
                onchange={(event) => updateItems(items.map((entry, i) =>
                  i === index ? { ...entry, enabled: event.currentTarget.checked } : entry
                ))} />有効</label>
              <button type="button" disabled={index === 0} onclick={() => move(index, -1)}>上へ</button>
              <button type="button" disabled={index === items.length - 1} onclick={() => move(index, 1)}>下へ</button>
              <button type="button" onclick={() => updateItems(items.filter((_, i) => i !== index))}>削除</button>
            </div>
          </li>
        {/each}
      </ol>
    {/if}

    <button type="button" onclick={review} disabled={!dirty && !needsApproval}>
      {needsApproval ? '内容を確認して承認' : '内容を確認して保存'}
    </button>
  </fieldset>
  {#if saving}<p role="status">保存中…</p>{/if}
  {#if saved}<p role="status">保存しました。次回の起動から適用されます。</p>{/if}
  {#if error}<p class="warning" role="alert">{error}</p>{/if}
{/if}

<dialog bind:this={dialog} aria-label="起動時に開く項目の確認" onkeydown={(event) => event.stopPropagation()}>
  <h4>起動時に開く項目の確認</h4>
  <p>次の一覧で有効にしたアプリと URL を、fast-comment を起動するたびに自動で開きます。信頼できるものだけを登録してください。</p>
  {#if readError}
    <p class="warning">読み込めなかった設定を、この一覧で上書きします。</p>
  {/if}
  <!-- svelte-ignore a11y_no_noninteractive_tabindex (長い一覧をキーボードでスクロールできるようにする) -->
  <div class="review-items" tabindex="0" role="region" aria-label="確認する項目">
    {#if reviewItems.length === 0}
      <p>起動時に開く項目はありません。</p>
    {:else}
      <ol>
        {#each reviewItems as item}
          <li>
            <dl>
              <dt>種類</dt><dd>{item.kind === 'exe' ? 'アプリ' : 'URL'}</dd>
              <dt>状態</dt><dd>{item.enabled ? '有効' : '無効'}</dd>
              {#if item.kind === 'exe'}
                <dt>ウィンドウ</dt><dd>{item.hideWindow ? '隠す' : '隠さない'}</dd>
              {/if}
              <dt>表示名</dt><dd class="literal">{item.label ?? '（なし）'}</dd>
              <dt>{item.kind === 'exe' ? 'パス' : 'URL'}</dt>
              <dd class="literal" dir="ltr">{item.target}</dd>
              <dt>引数</dt>
              <dd>
                {#if item.args.length}
                  <ol>
                    {#each item.args as arg}
                      <li class="literal" dir="ltr">{arg === '' ? '（空の引数）' : arg}</li>
                    {/each}
                  </ol>
                {:else}
                  （なし）
                {/if}
              </dd>
            </dl>
          </li>
        {/each}
      </ol>
    {/if}
  </div>
  <form method="dialog" class="actions dialog-actions">
    <!-- svelte-ignore a11y_autofocus -->
    <button type="submit" autofocus>キャンセル</button>
    <button type="button" onclick={approve}>承認して保存</button>
  </form>
</dialog>

<style>
  fieldset { border: 0; padding: 0; margin: 0; min-inline-size: 0; }
  p { margin: 6px 0; }
  .add { display: grid; gap: 8px; margin: 8px 0 12px; }
  .add label { display: grid; gap: 4px; }
  .add label.check { display: flex; align-items: center; gap: 6px; }
  .add button { justify-self: start; }
  input, select, textarea, button {
    border: 1px solid #666;
    border-radius: 4px;
    background: #292929;
    color: inherit;
    font: inherit;
    padding: 4px 8px;
  }
  button { cursor: pointer; }
  button:disabled { opacity: 0.5; cursor: default; }
  .items { margin: 8px 0 12px; }
  .items > li { padding: 6px 0; }
  .literal { white-space: pre-wrap; overflow-wrap: anywhere; unicode-bidi: isolate; }
  dialog .literal { unicode-bidi: plaintext; }
  .actions { display: flex; align-items: center; flex-wrap: wrap; gap: 6px; }
  .actions label { display: flex; align-items: center; }
  .rename { display: flex; align-items: center; gap: 6px; margin: 4px 0; }
  .rename input { flex: 1; min-width: 0; max-width: 320px; }
  .warning { padding: 8px; border-left: 3px solid #ef7777; color: #ffb4a9; background: #442224; }
  dialog {
    box-sizing: border-box;
    width: min(640px, calc(100% - 24px));
    max-height: calc(100% - 24px);
    margin: auto;
    padding: 14px 16px;
    border: 1px solid #666;
    border-radius: 8px;
    background: #1e1e1e;
    color: #e0e0e0;
    font-size: 13px;
  }
  dialog[open] { display: flex; flex-direction: column; gap: 8px; }
  dialog::backdrop { background: rgba(0, 0, 0, 0.6); }
  h4 { margin: 0; font-size: 15px; }
  .review-items { overflow: auto; min-height: 0; }
  .review-items > ol { margin: 0; padding-inline-start: 24px; }
  dl { display: grid; grid-template-columns: auto minmax(0, 1fr); gap: 4px 12px; margin: 8px 0; }
  dt { color: #aaa; }
  dd { margin: 0; }
  dd ol { padding-inline-start: 24px; }
  .dialog-actions { justify-content: flex-end; }
</style>
