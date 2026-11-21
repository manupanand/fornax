<!-- Copyright 2026 Manu P Anand
     Licensed under the Apache License, Version 2.0
     See LICENSE for details. -->
<script module lang="ts">
  // Fornax's own line icons (16×16 grid, currentColor). No icon fonts or external assets.
  export type IconName =
    | 'chevron' | 'folder' | 'folder-open' | 'file' | 'module' | 'sidebar' | 'branch' | 'commit' | 'copy'
    | 'home' | 'repo' | 'settings' | 'menu' | 'sun' | 'moon' | 'monitor' | 'logout' | 'login' | 'palette' | 'plus' | 'check';
</script>

<script lang="ts">
  let { name, size = 16 }: { name: IconName; size?: number } = $props();
</script>

<svg
  width={size}
  height={size}
  viewBox="0 0 16 16"
  fill="none"
  stroke="currentColor"
  stroke-width="1.5"
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"
>
  {#if name === 'chevron'}
    <path d="M6 4l4 4-4 4" />
  {:else if name === 'folder'}
    <path d="M1.75 4.25a1.5 1.5 0 0 1 1.5-1.5h2.6l1.4 1.5h5.5a1.5 1.5 0 0 1 1.5 1.5v6a1.5 1.5 0 0 1-1.5 1.5h-9.5a1.5 1.5 0 0 1-1.5-1.5z" />
  {:else if name === 'folder-open'}
    <path d="M1.75 11.5v-7.25a1.5 1.5 0 0 1 1.5-1.5h2.6l1.4 1.5h4.5a1.5 1.5 0 0 1 1.5 1.5v.75" />
    <path d="M1.9 12.6l1.6-4.6a1 1 0 0 1 .95-.7h9.3a.75.75 0 0 1 .7 1l-1.5 4.3a1 1 0 0 1-.95.65h-9.4a.75.75 0 0 1-.7-.65z" />
  {:else if name === 'file'}
    <path d="M4 1.75h5l3 3v9.5H4z" /><path d="M9 1.75v3h3" />
  {:else if name === 'module'}
    <rect x="2" y="2" width="12" height="12" rx="3" /><path d="M5.5 8h5M8 5.5v5" />
  {:else if name === 'sidebar'}
    <rect x="1.75" y="2.75" width="12.5" height="10.5" rx="2.5" /><path d="M6 2.75v10.5" />
  {:else if name === 'branch'}
    <circle cx="4.5" cy="3.5" r="1.5" /><circle cx="4.5" cy="12.5" r="1.5" /><circle cx="11.5" cy="5" r="1.5" />
    <path d="M4.5 5v6M11.5 6.5c0 3-7 2-7 4.5" />
  {:else if name === 'commit'}
    <circle cx="8" cy="8" r="2.5" /><path d="M1.5 8h4M10.5 8h4" />
  {:else if name === 'copy'}
    <rect x="5" y="5" width="9" height="9" rx="2" /><path d="M11 5V3.5A1.5 1.5 0 0 0 9.5 2h-6A1.5 1.5 0 0 0 2 3.5v6A1.5 1.5 0 0 0 3.5 11H5" />
  {:else if name === 'home'}
    <path d="M2.25 7.25L8 2.5l5.75 4.75" /><path d="M3.75 6.25v7.25h3V10h2.5v3.5h3V6.25" />
  {:else if name === 'repo'}
    <path d="M3.25 12.5V3a1.25 1.25 0 0 1 1.25-1.25h8.25v9.5H4.5a1.25 1.25 0 0 0-1.25 1.25z" />
    <path d="M3.25 12.5a1.25 1.25 0 0 0 1.25 1.25h1.75M12.75 11.25v2.5H9.5" /><path d="M7 14.5v-2l.9.6.9-.6v2" />
  {:else if name === 'settings'}
    <circle cx="8" cy="8" r="2" />
    <path d="M8 1.75l1.2 1.6 2-.25.7 1.9 1.85.8-.4 2 .4 2-1.85.8-.7 1.9-2-.25L8 14.25l-1.2-1.6-2 .25-.7-1.9-1.85-.8.4-2-.4-2 1.85-.8.7-1.9 2 .25z" />
  {:else if name === 'menu'}
    <path d="M2.5 4.5h11M2.5 8h11M2.5 11.5h11" />
  {:else if name === 'sun'}
    <circle cx="8" cy="8" r="2.75" />
    <path d="M8 1.5v1.25M8 13.25v1.25M1.5 8h1.25M13.25 8h1.25M3.4 3.4l.9.9M11.7 11.7l.9.9M3.4 12.6l.9-.9M11.7 4.3l.9-.9" />
  {:else if name === 'moon'}
    <path d="M13.25 9.6A5.5 5.5 0 0 1 6.4 2.75a5.5 5.5 0 1 0 6.85 6.85z" />
  {:else if name === 'monitor'}
    <rect x="1.75" y="2.5" width="12.5" height="8.5" rx="1.75" /><path d="M5.5 13.75h5M8 11v2.75" />
  {:else if name === 'logout'}
    <path d="M6 2.25H3.75a1.5 1.5 0 0 0-1.5 1.5v8.5a1.5 1.5 0 0 0 1.5 1.5H6" /><path d="M10.5 11l3-3-3-3M13.5 8H6" />
  {:else if name === 'login'}
    <path d="M10 2.25h2.25a1.5 1.5 0 0 1 1.5 1.5v8.5a1.5 1.5 0 0 1-1.5 1.5H10" /><path d="M6.5 11l3-3-3-3M9.5 8H2.5" />
  {:else if name === 'palette'}
    <path d="M8 1.75a6.25 6.25 0 1 0 0 12.5c.9 0 1.4-.6 1.4-1.3 0-.9-.8-1.2-.8-2 0-.7.6-1.2 1.3-1.2h1.6a2.75 2.75 0 0 0 2.75-2.75C14.25 4.2 11.45 1.75 8 1.75z" />
    <circle cx="4.75" cy="7.25" r=".75" /><circle cx="6.5" cy="4.5" r=".75" /><circle cx="9.75" cy="4.5" r=".75" />
  {:else if name === 'plus'}
    <path d="M8 3v10M3 8h10" />
  {:else if name === 'check'}
    <path d="M3.5 8.5l3 3 6-7" />
  {/if}
</svg>
