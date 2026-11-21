<!-- Copyright 2026 Manu P Anand
     Licensed under the Apache License, Version 2.0
     See LICENSE for details. -->
<script lang="ts">
  import { timeAgo, type Repo } from './api';
  import { link } from './router.svelte';

  let { repos, empty = 'No repositories yet.' }: { repos: Repo[]; empty?: string } = $props();
</script>

<div class="box">
  {#each repos as r (r.id)}
    <div class="row">
      <div class="grow">
        <a href={'/' + encodeURIComponent(r.owner) + '/' + encodeURIComponent(r.name)} use:link><strong>{r.full_name}</strong></a>
        {#if r.private}<span class="badge">Private</span>{/if}
        {#if r.description}<div class="muted small ellipsis">{r.description}</div>{/if}
      </div>
      <span class="muted small">Updated {timeAgo(r.updated_at)}</span>
    </div>
  {:else}
    <div class="row muted">{empty}</div>
  {/each}
</div>
