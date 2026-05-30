<script lang="ts">
  import { store } from '../stores.svelte';
  import type { DonationKind } from '../stores.svelte';
  import type { ChatMessage } from '../types';

  const KIND_LABEL: Record<DonationKind, string> = {
    superchat: 'superchat',
    bits: 'bits',
    membership: 'membership',
  };

  let donations = $derived(store.donationMessages);

  function formatAmount(message: ChatMessage): string {
    const amount = message.amount;
    if (!amount) return 'メンバーシップ';
    const raw = amount.rawText.trim();
    if (raw) return raw;

    const value = amount.value.toLocaleString('ja-JP');
    const currency = amount.currency.trim();
    return currency ? `${value} ${currency}` : value;
  }

  function bodyText(message: ChatMessage): string {
    return message.fragments.map((frag) => frag.type === 'text' ? frag.text : frag.name).join('');
  }
</script>

<div class="donation-panel" aria-label="投げ銭一覧">
  {#if donations.length === 0}
    <div class="empty">投げ銭はまだありません</div>
  {:else}
    <div class="donation-list" role="list">
      {#each donations as item (item.message.id)}
        <article class="donation-row {item.donationKind}" role="listitem">
          <div class="amount">{formatAmount(item.message)}</div>
          <div class="body">
            <div class="meta">
              <span class="author">{item.message.author.name}</span>
              <span class="kind-badge {item.donationKind}">{KIND_LABEL[item.donationKind]}</span>
            </div>
            <div class="text">{bodyText(item.message)}</div>
          </div>
        </article>
      {/each}
    </div>
  {/if}
</div>

<style>
  .donation-panel {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    background: #121212;
  }

  .donation-list {
    display: flex;
    flex-direction: column;
  }

  .donation-row {
    display: grid;
    grid-template-columns: minmax(86px, 128px) minmax(0, 1fr);
    gap: 10px;
    align-items: center;
    min-height: 46px;
    padding: 8px 10px;
    border-bottom: 1px solid rgba(255, 255, 255, 0.07);
    border-left: 3px solid transparent;
  }

  .donation-row.superchat { border-left-color: #ffd600; }
  .donation-row.bits { border-left-color: #9146ff; }
  .donation-row.membership { border-left-color: #00c853; }

  .amount {
    min-width: 0;
    color: #f5f5f5;
    font-size: 13px;
    font-weight: 700;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .body {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }

  .meta {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
  }

  .author {
    min-width: 0;
    color: #fff;
    font-size: 13px;
    font-weight: 700;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .kind-badge {
    flex-shrink: 0;
    border-radius: 4px;
    padding: 1px 6px;
    font-size: 10px;
    font-weight: 700;
    line-height: 1.5;
    text-transform: uppercase;
  }

  .kind-badge.superchat {
    background: #ffd600;
    color: #161616;
  }

  .kind-badge.bits {
    background: #9146ff;
    color: #fff;
  }

  .kind-badge.membership {
    background: #00a846;
    color: #fff;
  }

  .text {
    min-width: 0;
    color: #cfcfcf;
    font-size: 12px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .empty {
    display: grid;
    place-items: center;
    height: 100%;
    color: #777;
    font-size: 13px;
  }
</style>
