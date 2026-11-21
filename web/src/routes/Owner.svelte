<!-- Copyright 2026 Manu P Anand
     Licensed under the Apache License, Version 2.0
     See LICENSE for details. -->
<script lang="ts">
  import { api, type Repo } from '../lib/api';
  import RepoList from '../lib/RepoList.svelte';

  let { owner }: { owner: string } = $props();
  let repos = $state<Repo[] | null>(null);
  let error = $state('');

  $effect(() => {
    api.ownerRepos(owner).then((r) => (repos = r)).catch((e) => (error = e.status === 404 ? `${owner} does not exist.` : e.message));
  });
</script>

<div class="stack">
  <h1>{owner}</h1>
  {#if error}
    <div class="error">{error}</div>
  {:else if repos === null}
    <p class="muted">Loading…</p>
  {:else}
    <RepoList {repos} empty={`${owner} has no repositories you can see.`} />
  {/if}
</div>
