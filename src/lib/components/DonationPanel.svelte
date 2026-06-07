<script lang="ts">
  import { store } from '../stores.svelte';
  import type { DonationKind } from '../stores.svelte';
  import type { ChatMessage } from '../types';

  const KIND_LABEL: Record<DonationKind, string> = {
    superchat: 'スーパーチャット',
    bits: 'Bits',
    membership: 'メンバーシップ',
  };

  // フィルタ種別ラベル(ボタン表示用・短め)
  const FILTER_LABEL: Record<'all' | DonationKind, string> = {
    all: 'すべて',
    superchat: 'SC',
    bits: 'Bits',
    membership: 'メンバー',
  };

  type FilterKind = 'all' | DonationKind;

  const FILTER_OPTIONS: FilterKind[] = ['all', 'superchat', 'bits', 'membership'];

  let selectedKind = $state<FilterKind>('all');

  let donations = $derived(store.donationMessages);

  let filteredDonations = $derived(
    selectedKind === 'all'
      ? donations
      : donations.filter((item) => item.donationKind === selectedKind),
  );

  // 種別ごとの件数(ボタン併記用)
  let countByKind = $derived.by(() => {
    const counts: Record<FilterKind, number> = {
      all: donations.length,
      superchat: 0,
      bits: 0,
      membership: 0,
    };
    for (const item of donations) {
      counts[item.donationKind]++;
    }
    return counts;
  });

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

  // 空状態メッセージ
  function emptyMessage(): string {
    if (donations.length === 0) return '投げ銭はまだありません';
    return `${KIND_LABEL[selectedKind as DonationKind]}はまだありません`;
  }
</script>

<div class="donation-panel" aria-label="投げ銭一覧">
  <!-- フィルタUI -->
  <div class="filter-bar">
    <div class="filter-group" role="group" aria-label="投げ銭種別フィルタ">
      {#each FILTER_OPTIONS as kind (kind)}
        <button
          class="filter-btn {kind}"
          class:active={selectedKind === kind}
          aria-pressed={selectedKind === kind}
          onclick={() => { selectedKind = kind; }}
        >
          {FILTER_LABEL[kind]}
          <span class="filter-count">{countByKind[kind]}</span>
        </button>
      {/each}
    </div>
    <span class="total-count" aria-live="polite">{filteredDonations.length}件</span>
  </div>

  {#if filteredDonations.length === 0}
    <div class="empty">{emptyMessage()}</div>
  {:else}
    <div class="donation-list" role="list">
      {#each filteredDonations as item (item.message.id)}
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
    display: flex;
    flex-direction: column;
  }

  /* ---- フィルタバー ---- */
  .filter-bar {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 10px;
    border-bottom: 1px solid rgba(255, 255, 255, 0.1);
    flex-shrink: 0;
  }

  .filter-group {
    display: flex;
    gap: 4px;
    flex: 1;
    min-width: 0;
  }

  .filter-btn {
    flex: 1;
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 4px;
    padding: 3px 6px;
    border: 1px solid rgba(255, 255, 255, 0.15);
    border-radius: 5px;
    background: transparent;
    color: #aaa;
    font-size: 11px;
    font-weight: 600;
    cursor: pointer;
    white-space: nowrap;
    transition: background 0.12s, color 0.12s, border-color 0.12s;
  }

  .filter-btn:hover {
    background: rgba(255, 255, 255, 0.08);
    color: #fff;
  }

  /* active状態: all は白、各種別は固有色 */
  .filter-btn.active {
    color: #fff;
    border-color: transparent;
  }

  .filter-btn.all.active    { background: rgba(255, 255, 255, 0.18); }
  .filter-btn.superchat.active  { background: #b39700; border-color: #ffd600; }
  .filter-btn.bits.active       { background: #6226cc; border-color: #9146ff; }
  .filter-btn.membership.active { background: #007a30; border-color: #00c853; }

  .filter-count {
    font-size: 10px;
    opacity: 0.8;
    font-weight: 700;
  }

  .total-count {
    flex-shrink: 0;
    color: #888;
    font-size: 11px;
    white-space: nowrap;
  }

  /* ---- リスト ---- */
  .donation-list {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
    overflow-y: auto;
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
    flex: 1;
    min-height: 80px;
    color: #777;
    font-size: 13px;
  }
</style>
