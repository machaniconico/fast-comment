<script lang="ts">
  import { ui } from '../ui.svelte';

  let panelEl: HTMLDivElement | null = $state(null);

  const shortcuts = [
    { keys: ['Ctrl/⌘', 'K'], label: 'コマンドパレット' },
    { keys: ['?'], label: 'このヘルプ' },
    { keys: ['Esc'], label: 'パレット/メニュー/ヘルプを閉じる' },
    { keys: ['↑ ↓', 'Enter'], label: 'パレットの項目移動・実行' },
  ];

  const actions = [
    { control: 'ツール', label: 'タイマー/抽選/振り返りを開く' },
    { control: '×', label: 'コメント一覧をクリア' },
    { control: 'text / .*', label: 'コメント検索のテキスト/正規表現モード切替' },
  ];

  $effect(() => {
    if (ui.showShortcuts) {
      requestAnimationFrame(() => {
        panelEl?.focus();
      });
    }
  });

  function onPanelKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.preventDefault();
      ui.closeShortcuts();
    }
  }
</script>

{#if ui.showShortcuts}
  <div class="shortcut-overlay">
    <!-- Backdrop is a real <button> (native keyboard/click semantics, kept out
         of the tab order) so the dialog below stays in the accessibility tree —
         an aria-hidden ancestor would hide the dialog from assistive tech. -->
    <button
      type="button"
      class="shortcut-backdrop"
      aria-label="ショートカットヘルプを閉じる"
      tabindex="-1"
      onclick={() => ui.closeShortcuts()}
    ></button>
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div
      bind:this={panelEl}
      class="shortcut-panel"
      role="dialog"
      aria-modal="true"
      aria-labelledby="shortcut-help-title"
      tabindex="-1"
      onkeydown={onPanelKeydown}
    >
      <header class="shortcut-header">
        <h2 id="shortcut-help-title">ショートカット</h2>
        <button
          type="button"
          class="shortcut-close"
          onclick={() => ui.closeShortcuts()}
          title="閉じる"
          aria-label="ショートカットヘルプを閉じる"
        >×</button>
      </header>

      <section aria-labelledby="shortcut-keys-title">
        <h3 id="shortcut-keys-title">キー操作</h3>
        <dl class="shortcut-list">
          {#each shortcuts as item}
            <div class="shortcut-row">
              <dt>
                {#each item.keys as key}
                  <kbd>{key}</kbd>
                {/each}
              </dt>
              <dd>{item.label}</dd>
            </div>
          {/each}
        </dl>
      </section>

      <section aria-labelledby="shortcut-actions-title">
        <h3 id="shortcut-actions-title">画面操作</h3>
        <dl class="shortcut-list">
          {#each actions as item}
            <div class="shortcut-row">
              <dt><kbd>{item.control}</kbd></dt>
              <dd>{item.label}</dd>
            </div>
          {/each}
        </dl>
      </section>
    </div>
  </div>
{/if}

<style>
  .shortcut-overlay {
    position: fixed;
    inset: 0;
    z-index: 1001;
    display: flex;
    align-items: flex-start;
    justify-content: center;
    padding: 72px 16px 24px;
  }

  .shortcut-backdrop {
    position: absolute;
    inset: 0;
    z-index: 0;
    margin: 0;
    padding: 0;
    border: none;
    background: rgba(0, 0, 0, 0.55);
    cursor: pointer;
  }

  .shortcut-panel {
    position: relative;
    z-index: 1;
    width: 520px;
    max-width: 100%;
    max-height: calc(100vh - 96px);
    overflow-y: auto;
    background: #1e1e1e;
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.6);
    color: #e0e0e0;
  }

  .shortcut-panel:focus {
    outline: 2px solid rgba(76, 175, 80, 0.65);
    outline-offset: 2px;
  }

  .shortcut-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 14px 16px;
    border-bottom: 1px solid rgba(255, 255, 255, 0.08);
  }

  h2,
  h3 {
    margin: 0;
    font-weight: 600;
    letter-spacing: 0;
  }

  h2 {
    font-size: 15px;
  }

  h3 {
    padding: 14px 16px 6px;
    color: #9e9e9e;
    font-size: 12px;
  }

  .shortcut-close {
    width: 28px;
    height: 28px;
    border: none;
    border-radius: 6px;
    background: transparent;
    color: #9e9e9e;
    font: inherit;
    font-size: 18px;
    line-height: 1;
    cursor: pointer;
  }

  .shortcut-close:hover,
  .shortcut-close:focus-visible {
    background: rgba(255, 255, 255, 0.08);
    color: #fff;
    outline: none;
  }

  .shortcut-list {
    margin: 0;
    padding: 0 16px 10px;
  }

  .shortcut-row {
    display: grid;
    grid-template-columns: minmax(132px, auto) 1fr;
    gap: 14px;
    align-items: center;
    min-height: 38px;
    border-top: 1px solid rgba(255, 255, 255, 0.06);
  }

  .shortcut-row:first-child {
    border-top: none;
  }

  dt {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin: 0;
  }

  dd {
    margin: 0;
    color: #d0d0d0;
    font-size: 13px;
  }

  kbd {
    min-width: 28px;
    padding: 3px 7px;
    border: 1px solid rgba(255, 255, 255, 0.16);
    border-radius: 5px;
    background: rgba(255, 255, 255, 0.07);
    color: #f5f5f5;
    font-family: inherit;
    font-size: 12px;
    line-height: 1.2;
    text-align: center;
    white-space: nowrap;
  }

  @media (max-width: 520px) {
    .shortcut-overlay {
      padding-top: 48px;
    }

    .shortcut-row {
      grid-template-columns: 1fr;
      gap: 4px;
      padding: 9px 0;
    }
  }
</style>
