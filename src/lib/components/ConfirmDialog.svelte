<script lang="ts">
  import { tick } from 'svelte';
  import { moderation } from '../moderation.svelte';
  import { focusReturnCandidates } from '../moderationActions';

  let request = $derived(moderation.confirm);
  let cancelEl: HTMLButtonElement | undefined = $state();
  let okEl: HTMLButtonElement | undefined = $state();
  // Focus returns to the requester-supplied element, else the element focused
  // when the dialog opened, else the comment list.
  let opener: HTMLElement | null = null;
  let wasOpen = false;

  $effect(() => {
    if (request) {
      wasOpen = true;
      const active = document.activeElement;
      opener =
        request.returnFocus ??
        (active instanceof HTMLElement && active !== document.body ? active : null);
      void tick().then(() => cancelEl?.focus());
    } else if (wasOpen) {
      wasOpen = false;
      const preferred = opener;
      opener = null;
      const list = document.querySelector<HTMLElement>('.comment-list-container');
      for (const el of focusReturnCandidates(preferred, list)) {
        el.focus();
        if (document.activeElement === el) break;
      }
    }
  });

  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      moderation.cancelConfirm();
      return;
    }
    if (e.key !== 'Tab') return;
    // Two-button focus trap.
    const active = document.activeElement;
    if (e.shiftKey && active === cancelEl) {
      e.preventDefault();
      okEl?.focus();
    } else if (!e.shiftKey && active === okEl) {
      e.preventDefault();
      cancelEl?.focus();
    }
  }
</script>

{#if request}
  <div class="cd-overlay">
    <button
      type="button"
      class="cd-backdrop"
      aria-label="キャンセル"
      tabindex="-1"
      onclick={() => moderation.cancelConfirm()}
    ></button>
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div
      class="cd-panel"
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="confirm-title"
      aria-describedby="confirm-desc"
      tabindex="-1"
      onkeydown={onKeydown}
    >
      <h2 id="confirm-title">永久 BAN の確認</h2>
      <dl id="confirm-desc" class="cd-info">
        <dt>対象</dt><dd>{request.name}</dd>
        <dt>プラットフォーム</dt><dd>{request.platform}</dd>
        <dt>チャンネル</dt><dd>{request.channel}</dd>
      </dl>
      <p class="cd-warn">この視聴者を配信側で永久に BAN します。</p>
      <div class="cd-actions">
        <button type="button" class="cd-btn" bind:this={cancelEl} onclick={() => moderation.cancelConfirm()}>キャンセル</button>
        <button type="button" class="cd-btn danger" bind:this={okEl} onclick={() => moderation.acceptConfirm()}>BAN する</button>
      </div>
    </div>
  </div>
{/if}

<style>
  .cd-overlay {
    position: fixed;
    inset: 0;
    z-index: 1004;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 16px;
  }
  .cd-backdrop {
    position: absolute;
    inset: 0;
    margin: 0;
    padding: 0;
    border: none;
    background: rgba(0, 0, 0, 0.55);
    cursor: pointer;
  }
  .cd-panel {
    position: relative;
    width: 360px;
    max-width: 100%;
    padding: 14px 16px;
    background: #1e1e1e;
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.6);
    color: #e0e0e0;
    font-size: 13px;
  }
  .cd-panel:focus {
    outline: none;
  }
  h2 {
    margin: 0 0 10px;
    font-size: 15px;
  }
  .cd-info {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 4px 12px;
    margin: 0 0 10px;
  }
  .cd-info dt {
    color: #9e9e9e;
  }
  .cd-info dd {
    margin: 0;
    word-break: break-all;
  }
  .cd-warn {
    margin: 0 0 12px;
    color: #ffb4a9;
  }
  .cd-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }
  .cd-btn {
    padding: 5px 14px;
    border: 1px solid rgba(255, 255, 255, 0.2);
    border-radius: 4px;
    background: rgba(255, 255, 255, 0.06);
    color: inherit;
    font: inherit;
    cursor: pointer;
  }
  .cd-btn:hover {
    background: rgba(255, 255, 255, 0.12);
  }
  .cd-btn.danger {
    border-color: rgba(244, 67, 54, 0.6);
    color: #ffb4a9;
  }
  .cd-btn.danger:hover {
    background: rgba(244, 67, 54, 0.22);
  }
  .cd-btn:focus-visible {
    outline: 2px solid #64b5f6;
    outline-offset: 1px;
  }
</style>
