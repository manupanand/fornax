<!-- Copyright 2026 Manu P Anand
     Licensed under the Apache License, Version 2.0
     See LICENSE for details. -->
<script lang="ts">
  // Appearance (System / Light / Dark) and accent palette, in a glass popover.
  import Icon from './Icon.svelte';
  import { ACCENTS, theme, type Appearance } from './theme.svelte';

  let { compact = false }: { compact?: boolean } = $props();
  let open = $state(false);
  let root: HTMLDivElement | undefined = $state();

  const modes: { id: Appearance; label: string; icon: 'monitor' | 'sun' | 'moon' }[] = [
    { id: 'system', label: 'System', icon: 'monitor' },
    { id: 'light', label: 'Light', icon: 'sun' },
    { id: 'dark', label: 'Dark', icon: 'moon' },
  ];

  function onDocClick(e: MouseEvent) {
    if (open && root && !root.contains(e.target as Node)) open = false;
  }
  function onKey(e: KeyboardEvent) {
    if (e.key === 'Escape') open = false;
  }
</script>

<svelte:document onclick={onDocClick} onkeydown={onKey} />

<div class="theme-menu" bind:this={root}>
  <button
    class="ghost nav-btn"
    class:compact
    aria-haspopup="dialog"
    aria-expanded={open}
    title="Appearance"
    onclick={() => (open = !open)}
  >
    <Icon name="palette" size={18} />
    {#if !compact}<span class="label">Appearance</span>{/if}
  </button>
  {#if open}
    <div class="popover glass" role="dialog" aria-label="Appearance">
      <div class="section-label">Mode</div>
      <div class="segmented modes" role="radiogroup" aria-label="Colour mode">
        {#each modes as m (m.id)}
          <button
            role="radio"
            aria-checked={theme.appearance === m.id}
            class:active={theme.appearance === m.id}
            onclick={() => theme.set({ appearance: m.id })}
          >
            <Icon name={m.icon} size={14} />{m.label}
          </button>
        {/each}
      </div>
      <div class="section-label">Accent</div>
      <div class="swatches" role="radiogroup" aria-label="Accent colour">
        {#each ACCENTS as a (a.id)}
          <button
            class="swatch"
            role="radio"
            aria-checked={theme.accent === a.id}
            aria-label={a.label}
            title={a.label}
            style:--swatch={a.swatch}
            onclick={() => theme.set({ accent: a.id })}
          >
            {#if theme.accent === a.id}<Icon name="check" size={14} />{/if}
          </button>
        {/each}
      </div>
    </div>
  {/if}
</div>

<style>
  .theme-menu { position: relative; }
  .popover {
    position: absolute; left: calc(100% + 10px); bottom: 0; z-index: 60; width: 260px;
    padding: 14px; border-radius: var(--r-lg); background-color: var(--glass-strong);
    box-shadow: var(--rim), var(--depth-lift); animation: pop 0.22s var(--ease-out) both;
  }
  @keyframes pop { from { opacity: 0; transform: translateX(-6px) scale(0.97); } to { opacity: 1; transform: none; } }
  .section-label { font-size: var(--fs-xs); font-weight: 700; color: var(--fg-faint); text-transform: uppercase; letter-spacing: 0.06em; margin: 2px 2px 8px; }
  .modes { display: flex; width: 100%; margin-bottom: 14px; }
  .modes button { flex: 1; gap: 5px; padding: 6px 8px; font-size: 13px; }
  .swatches { display: flex; gap: 10px; padding: 2px; }
  .swatch {
    width: 30px; height: 30px; padding: 0; border-radius: 50%; color: #fff;
    background: var(--swatch); border: 2px solid var(--glass-border);
    box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.35), 0 2px 8px rgba(15, 23, 42, 0.18);
    -webkit-backdrop-filter: none; backdrop-filter: none;
  }
  .swatch:hover { background: var(--swatch); filter: brightness(1.08); }
  .swatch[aria-checked='true'] { outline: 2px solid var(--swatch); outline-offset: 2px; }
  @media (max-width: 760px) {
    .popover { left: 0; bottom: calc(100% + 10px); }
  }
</style>
