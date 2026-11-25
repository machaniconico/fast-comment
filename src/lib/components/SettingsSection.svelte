<script lang="ts">
  import type { Snippet } from 'svelte';
  import { settingsCollapse } from '../settingsCollapse.svelte';
  import { matchesSettingsQuery } from '../settingsFilter';

  let { id, title, children }: { id: string; title: string; children: Snippet } = $props();

  let el: HTMLElement | undefined = $state();
  const collapsible = $derived(settingsCollapse.enabled);
  const filtering = $derived(collapsible && settingsCollapse.query.trim() !== '');
  let matched = $state(true);
  const open = $derived(!collapsible || settingsCollapse.isOpen(id) || (filtering && matched));
  const bodyId = $derived(`${id}-body`);

  $effect(() => {
    const q = settingsCollapse.query;
    matched = el ? matchesSettingsQuery(el.textContent ?? '', q) : true;
  });

  $effect(() => {
    const m = filtering ? matched : false;
    settingsCollapse.matches[id] = m;
    return () => {
      delete settingsCollapse.matches[id];
    };
  });
</script>

<section {id} bind:this={el} data-settings-section={id} class:filtered-out={filtering && !matched}>
  {#if collapsible}
    <h3 class="section-heading">
      <button
        type="button"
        class="section-toggle"
        aria-expanded={open}
        aria-controls={bodyId}
        onclick={() => settingsCollapse.toggle(id)}
      >
        <span class="chevron" aria-hidden="true">{open ? '▾' : '▸'}</span>{title}
      </button>
    </h3>
  {:else}
    <h3>{title}</h3>
  {/if}
  <div id={bodyId} class="section-body" hidden={!open}>
    {@render children()}
  </div>
</section>

<style>
  section {
    border-bottom: 1px solid rgba(255,255,255,0.07);
    padding-bottom: 12px;
    margin-bottom: 4px;
  }

  section.filtered-out { display: none; }

  h3 {
    font-size: 12px;
    font-weight: 600;
    color: #9e9e9e;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    margin: 14px 0 6px;
  }

  .section-heading { margin: 8px 0 2px; }

  .section-toggle {
    all: unset;
    box-sizing: border-box;
    display: block;
    width: 100%;
    padding: 6px 4px;
    border-radius: 4px;
    cursor: pointer;
    font: inherit;
    color: inherit;
    text-transform: inherit;
    letter-spacing: inherit;
  }

  .section-toggle:hover { background: rgba(255,255,255,0.06); }
  .section-toggle:focus-visible { outline: 2px solid #64b5f6; outline-offset: 1px; }

  .chevron { display: inline-block; width: 1.4em; }
</style>
