<script lang="ts" module>
  export interface ContextMenuItem {
    label: string;
    action: () => void | Promise<void>;
    danger?: boolean;
  }
</script>

<script lang="ts">
  import { onMount, tick } from 'svelte';

  interface Props {
    x: number;
    y: number;
    items: ContextMenuItem[];
    onClose: () => void;
  }

  let { x, y, items, onClose }: Props = $props();

  let menuEl: HTMLDivElement;
  let left = $state(0);
  let top = $state(0);
  let activeIndex = $state(0);

  const MARGIN = 8;

  async function clampPosition() {
    await tick();
    if (!menuEl || typeof window === 'undefined') return;

    const rect = menuEl.getBoundingClientRect();
    left = Math.min(
      Math.max(MARGIN, x),
      Math.max(MARGIN, window.innerWidth - rect.width - MARGIN)
    );
    top = Math.min(
      Math.max(MARGIN, y),
      Math.max(MARGIN, window.innerHeight - rect.height - MARGIN)
    );
  }

  $effect(() => {
    x;
    y;
    items;
    activeIndex = 0;
    void clampPosition();
  });

  function moveActive(delta: number) {
    if (items.length === 0) return;
    activeIndex = (activeIndex + delta + items.length) % items.length;
  }

  function choose(index: number) {
    const item = items[index];
    if (!item) return;
    onClose();
    void item.action();
  }

  function onKeydown(event: KeyboardEvent) {
    if (event.key === 'Escape') {
      event.preventDefault();
      onClose();
    } else if (event.key === 'ArrowDown') {
      event.preventDefault();
      moveActive(1);
    } else if (event.key === 'ArrowUp') {
      event.preventDefault();
      moveActive(-1);
    } else if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      choose(activeIndex);
    }
  }

  function onDocumentPointerDown(event: PointerEvent) {
    if (!menuEl || menuEl.contains(event.target as Node)) return;
    onClose();
  }

  onMount(() => {
    void clampPosition();
    menuEl?.focus();
    document.addEventListener('pointerdown', onDocumentPointerDown, true);
    window.addEventListener('resize', clampPosition);

    return () => {
      document.removeEventListener('pointerdown', onDocumentPointerDown, true);
      window.removeEventListener('resize', clampPosition);
    };
  });
</script>

<div
  bind:this={menuEl}
  class="context-menu"
  style:left="{left}px"
  style:top="{top}px"
  role="menu"
  tabindex="0"
  aria-label="コメント操作"
  onkeydown={onKeydown}
>
  {#each items as item, index (item.label)}
    <button
      type="button"
      class="menu-item"
      class:danger={item.danger}
      class:active={index === activeIndex}
      role="menuitem"
      tabindex="-1"
      onmouseenter={() => { activeIndex = index; }}
      onclick={() => choose(index)}
    >
      {item.label}
    </button>
  {/each}
</div>

<style>
  .context-menu {
    position: fixed;
    z-index: 1000;
    min-width: 190px;
    max-width: min(280px, calc(100vw - 16px));
    padding: 4px;
    border: 1px solid rgba(255, 255, 255, 0.16);
    border-radius: 6px;
    background: rgba(28, 28, 32, 0.98);
    box-shadow: 0 10px 28px rgba(0, 0, 0, 0.34);
    outline: none;
  }

  .menu-item {
    display: block;
    width: 100%;
    min-height: 28px;
    padding: 5px 9px;
    border: 0;
    border-radius: 4px;
    background: transparent;
    color: rgba(255, 255, 255, 0.92);
    font: inherit;
    font-size: 12px;
    line-height: 1.35;
    text-align: left;
    cursor: pointer;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .menu-item.active,
  .menu-item:hover {
    background: rgba(255, 255, 255, 0.1);
  }

  .menu-item.danger {
    color: #ffb4a9;
  }

  .menu-item.danger.active,
  .menu-item.danger:hover {
    background: rgba(244, 67, 54, 0.18);
  }
</style>
