<script lang="ts">
  import type { UiChatMessage } from '../types';
  import { store, togglePin } from '../stores.svelte';
  import { getConfig, hideMessage as ipcHideMessage, setConfig, ttsSpeakText } from '../ipc';
  import type { ContextMenuItem } from './ContextMenu.svelte';

  interface CommentContextMenuRequest {
    x: number;
    y: number;
    items: ContextMenuItem[];
  }

  interface Props {
    message: UiChatMessage;
    onOpenContextMenu: (request: CommentContextMenuRequest) => void;
  }

  let { message, onOpenContextMenu }: Props = $props();

  const PLATFORM_COLORS: Record<string, string> = {
    twitch: '#9146ff',
    youtube: '#ff0000',
  };

  const KIND_BG: Record<string, string> = {
    superChat: '#ffd600',
    membership: '#00c853',
    bits: '#9146ff',
    system: '#37474f',
  };

  const KIND_TEXT: Record<string, string> = {
    superChat: '#000',
    membership: '#fff',
    bits: '#fff',
    system: '#fff',
  };

  let platformColor = $derived(PLATFORM_COLORS[message.platform] ?? '#888');
  let hasHighlightBadge = $derived(
    message.author.badges.some(b => b.kind === 'highlight')
  );
  // kind 由来のベタ塗りを持つ種別。従来 isHighlighted に含めていた集合と一致させ、
  // system は従来どおり塗らない(KIND_BG['system'] は定義済みだが旧実装でも未適用だった)。
  const FILLED_KINDS = new Set(['superChat', 'membership', 'bits']);
  // kind 由来の塗りがある行 = 名前がベタ塗り背景に乗る行。
  let hasKindStyle = $derived(FILLED_KINDS.has(message.kind));
  let kindBg = $derived(hasKindStyle ? KIND_BG[message.kind] : undefined);
  let kindFg = $derived(hasKindStyle ? KIND_TEXT[message.kind] : undefined);
  // 行強調(太字 + 左枠): kind 塗り or highlight バッジのどちらかで点灯。
  let isHighlighted = $derived(hasKindStyle || hasHighlightBadge);
  let viewerSeq = $derived(message.viewerSeq);
  let viewerBadgeText = $derived(formatViewerBadgeText(viewerSeq));
  let viewerBadgeLabel = $derived(formatViewerBadgeLabel(viewerSeq));

  let pinned = $derived(store.isPinned(message.id));

  function onHide() {
    store.hideMessage(message.id);
    ipcHideMessage(message.id);
  }

  function getMessageText(): string {
    return message.fragments.map((frag) => frag.type === 'text' ? frag.text : frag.name).join('');
  }

  function getNgWordCandidate(): string {
    const selected = typeof window === 'undefined' ? '' : window.getSelection()?.toString().trim() ?? '';
    if (selected) return selected;
    return getMessageText().trim().match(/\S+/)?.[0] ?? '';
  }

  function copyText(text: string) {
    if (!text || typeof navigator === 'undefined' || !navigator.clipboard?.writeText) return;
    void navigator.clipboard.writeText(text).catch(() => {});
  }

  async function appendModerationValue(kind: 'ngUsers' | 'ngWords', value: string) {
    const normalized = value.trim();
    if (!normalized) return;

    const config = await getConfig();
    if (!config) return;

    const current = config.moderation[kind] ?? [];
    if (current.includes(normalized)) return;
    config.moderation[kind] = [...current, normalized];
    await setConfig(config);
  }

  function speakNow() {
    const text = getMessageText().trim();
    if (!text) return;
    if (typeof window !== 'undefined' && (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__) {
      void ttsSpeakText(text).catch(() => speakWithWebSpeech(text));
      return;
    }
    speakWithWebSpeech(text);
  }

  function speakWithWebSpeech(text: string) {
    if (typeof window === 'undefined' || !('speechSynthesis' in window)) return;
    try {
      window.speechSynthesis.speak(new SpeechSynthesisUtterance(text));
    } catch {
      // TTS is best-effort from the context menu.
    }
  }

  function buildMenuItems(): ContextMenuItem[] {
    const pinnedLabel = pinned ? 'ピン留め解除' : 'ピン留め';
    const ngWord = getNgWordCandidate();
    return [
      { label: pinnedLabel, action: () => togglePin(message) },
      { label: '本文をコピー', action: () => copyText(getMessageText()) },
      { label: '投稿者名をコピー', action: () => copyText(message.author.name) },
      {
        label: 'この人をNGユーザーに追加',
        danger: true,
        action: () => {
          store.hideMessage(message.id);
          void ipcHideMessage(message.id);
          void appendModerationValue('ngUsers', message.author.name);
        },
      },
      {
        label: 'この語をNGワードに追加',
        danger: true,
        action: () => {
          if (!ngWord) return;
          store.hideMessage(message.id);
          void ipcHideMessage(message.id);
          void appendModerationValue('ngWords', ngWord);
        },
      },
      { label: '今すぐ読み上げ', action: speakNow },
    ];
  }

  function openContextMenu(x: number, y: number) {
    onOpenContextMenu({ x, y, items: buildMenuItems() });
  }

  function onContextMenu(event: MouseEvent) {
    event.preventDefault();
    openContextMenu(event.clientX, event.clientY);
  }

  function onMoreClick(event: MouseEvent) {
    const target = event.currentTarget as HTMLElement;
    const rect = target.getBoundingClientRect();
    openContextMenu(rect.right, rect.bottom);
  }

  function formatTime(ms: number): string {
    const d = new Date(ms);
    const h = d.getHours().toString().padStart(2, '0');
    const m = d.getMinutes().toString().padStart(2, '0');
    const s = d.getSeconds().toString().padStart(2, '0');
    return `${h}:${m}:${s}`;
  }

  function formatViewerBadgeText(seq: number | undefined): string {
    if (!seq) return '';
    if (seq === 1) return '初';
    if (seq < 10) return String(seq);
    return `常連 ${seq}`;
  }

  function formatViewerBadgeLabel(seq: number | undefined): string {
    if (!seq) return '';
    if (seq === 1) return 'この視聴者の初回コメント';
    if (seq < 10) return `この視聴者の${seq}回目のコメント`;
    return `常連視聴者の${seq}回目のコメント`;
  }
</script>

<div
  class="comment-item"
  class:highlighted={isHighlighted}
  class:highlight-badge={hasHighlightBadge}
  style:background={kindBg}
  style:color={kindFg}
  role="listitem"
  oncontextmenu={onContextMenu}
>
  <!-- Platform indicator -->
  <span class="platform-dot" style:background={platformColor} title={message.platform}></span>

  <!-- Badges -->
  {#each message.author.badges as badge}
    {#if badge.imageUrl}
      <img class="badge-img" src={badge.imageUrl} alt={badge.label} title={badge.label} />
    {:else}
      <span class="badge-text" title={badge.label}>{badge.kind[0]?.toUpperCase()}</span>
    {/if}
  {/each}

  <!-- Viewer sequence badge: after platform/role badges so Host/Mod stay first. -->
  {#if viewerSeq}
    <span
      class="viewer-badge"
      class:badge-first={viewerSeq === 1}
      class:badge-count={viewerSeq > 1 && viewerSeq < 10}
      class:badge-regular={viewerSeq >= 10}
      title={viewerBadgeLabel}
      aria-label={viewerBadgeLabel}
    >
      {viewerBadgeText}
    </span>
  {/if}

  <!-- Author name -->
  <span
    class="author-name"
    style:color={!hasKindStyle && message.author.displayColor ? message.author.displayColor : undefined}
  >
    {message.author.name}
  </span>

  <!-- Separator -->
  <span class="sep">:</span>

  <!-- Amount badge for SuperChat/Bits -->
  {#if message.amount}
    <span class="amount-badge">{message.amount.rawText}</span>
  {/if}

  <!-- Fragments -->
  <span class="fragments">
    {#each message.fragments as frag}
      {#if frag.type === 'text'}
        <span>{frag.text}</span>
      {:else if frag.type === 'emote'}
        <img class="emote" src={frag.url} alt={frag.name} title={frag.name} />
      {/if}
    {/each}
  </span>

  <!-- Time + hide button -->
  <span class="meta">
    <span class="time">{formatTime(message.timestampMs)}</span>
    <button
      class="pin-btn"
      class:pinned
      onclick={() => togglePin(message)}
      title={pinned ? 'ピン解除' : 'ピン留め'}
      aria-label={pinned ? 'このコメントのピンを解除' : 'このコメントをピン留め'}
    >📌</button>
    <button
      class="menu-btn"
      onclick={onMoreClick}
      title="コメント操作"
      aria-label="コメント操作メニューを開く"
    >...</button>
    <button class="hide-btn" onclick={onHide} title="非表示" aria-label="このコメントを非表示">✕</button>
  </span>
</div>

<style>
  /* Every row is exactly ROW_HEIGHT (28px) tall so the virtual-scroll
     translateY math never drifts. Highlight styles must NOT change the box
     height: no extra padding/margin, only background + left border. */
  .comment-item {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 0 6px;
    font-size: 13px;
    line-height: 1.4;
    border-bottom: 1px solid rgba(255, 255, 255, 0.05);
    /* Reserve space for the highlight left border on every row so the
       content does not shift horizontally when a row becomes highlighted. */
    border-left: 3px solid transparent;
    height: 28px;
    box-sizing: border-box;
    position: relative;
  }

  .comment-item:hover .hide-btn,
  .comment-item:hover .menu-btn {
    opacity: 1;
  }

  /* Highlight stays within the 28px row: only color/border, no size change. */
  .comment-item.highlighted {
    border-left-color: rgba(0, 0, 0, 0.35);
    font-weight: 500;
  }

  /* highlight badge (moderation.highlights 由来): 左枠オレンジ + 薄い背景tint。
     normal kind の行には kind 由来の inline background が出ない(kindBg=undefined)ため
     この tint が実描画される。kind 塗りのある行では inline background が優先され tint は隠れる。
     後段ルールなので .highlighted の dark 枠色より #ff9800 が勝つ。
     border-left は既に3px確保済みなのでレイアウトシフト無し。 */
  .comment-item.highlight-badge {
    border-left-color: #ff9800;
    background: rgba(255, 152, 0, 0.08);
  }

  .platform-dot {
    display: inline-block;
    width: 6px;
    height: 6px;
    border-radius: 50%;
    flex-shrink: 0;
  }

  .badge-img {
    width: 18px;
    height: 18px;
    vertical-align: middle;
    flex-shrink: 0;
  }

  .badge-text {
    display: inline-block;
    width: 16px;
    height: 16px;
    font-size: 10px;
    line-height: 16px;
    text-align: center;
    background: rgba(255, 255, 255, 0.2);
    border-radius: 2px;
    flex-shrink: 0;
  }

  .viewer-badge {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    box-sizing: border-box;
    height: 16px;
    min-width: 16px;
    padding: 0 4px;
    border-radius: 2px;
    font-size: 10px;
    line-height: 16px;
    font-weight: 700;
    white-space: nowrap;
    flex-shrink: 0;
  }

  .badge-first {
    background: #fdd835;
    color: #1b1b1b;
  }

  .badge-count {
    border: 1px solid rgba(255, 255, 255, 0.28);
    background: rgba(255, 255, 255, 0.14);
    color: inherit;
  }

  .badge-regular {
    background: #26a69a;
    color: #061b18;
  }

  .author-name {
    font-weight: 600;
    white-space: nowrap;
    flex-shrink: 0;
    max-width: 140px;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .sep {
    flex-shrink: 0;
    opacity: 0.5;
  }

  .amount-badge {
    display: inline-block;
    padding: 1px 6px;
    background: rgba(0, 0, 0, 0.3);
    border-radius: 10px;
    font-size: 11px;
    font-weight: 700;
    flex-shrink: 0;
  }

  .fragments {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    display: flex;
    align-items: center;
    gap: 2px;
    flex-wrap: nowrap;
  }

  .emote {
    height: 20px;
    width: auto;
    vertical-align: middle;
    flex-shrink: 0;
  }

  .meta {
    display: flex;
    align-items: center;
    gap: 4px;
    margin-left: auto;
    flex-shrink: 0;
  }

  .time {
    font-size: 10px;
    opacity: 0.4;
    white-space: nowrap;
  }

  .hide-btn,
  .menu-btn {
    background: none;
    border: none;
    color: inherit;
    cursor: pointer;
    padding: 0 2px;
    font-size: 10px;
    line-height: 1;
    opacity: 0;
    transition: opacity 0.15s;
  }

  .hide-btn:hover {
    opacity: 1 !important;
    color: #f44336;
  }

  .menu-btn:hover {
    opacity: 1 !important;
    color: #90caf9;
  }

  .pin-btn {
    background: none;
    border: none;
    color: inherit;
    cursor: pointer;
    padding: 0 2px;
    font-size: 10px;
    line-height: 1;
    opacity: 0;
    transition: opacity 0.15s;
    filter: grayscale(1);
  }

  .comment-item:hover .pin-btn {
    opacity: 1;
  }

  .pin-btn.pinned {
    opacity: 1;
    filter: none;
    color: #ffb300;
  }

  .pin-btn:hover {
    opacity: 1 !important;
    filter: none;
  }
</style>
