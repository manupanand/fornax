<!-- Copyright 2026 Manu P Anand
     Licensed under the Apache License, Version 2.0
     See LICENSE for details. -->
<script lang="ts">
  import { api, type Repo } from '../lib/api';
  import RepoList from '../lib/RepoList.svelte';
  import { router } from '../lib/router.svelte';
  import { session } from '../lib/session.svelte';

  let repos = $state<Repo[]>([]);
  let error = $state('');
  let name = $state('');
  let description = $state('');
  let isPrivate = $state(false);
  let creating = $state(false);

  $effect(() => {
    const user = session.user;
    if (!user) return;
    api.ownerRepos(user.username).then((r) => (repos = r)).catch((e) => (error = String(e.message ?? e)));
  });

  async function create(e: SubmitEvent) {
    e.preventDefault();
    creating = true;
    error = '';
    try {
      const repo = await api.createRepo({ name, description, private: isPrivate });
      router.go(`/${encodeURIComponent(repo.owner)}/${encodeURIComponent(repo.name)}`);
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      creating = false;
    }
  }
</script>

{#if session.user}
  <div class="layout">
    <section class="stack">
      <h1>Your repositories</h1>
      {#if error}<div class="error">{error}</div>{/if}
      <RepoList {repos} empty="You have no repositories yet. Create one →" />
    </section>
    <aside>
      <form class="box rise" onsubmit={create}>
        <div class="box-header"><strong>New repository</strong></div>
        <div style="padding: 14px">
          <div class="field">
            <label for="name">Name</label>
            <input id="name" bind:value={name} required maxlength="100" pattern="[A-Za-z0-9][A-Za-z0-9._\-]*" />
          </div>
          <div class="field">
            <label for="desc">Description</label>
            <input id="desc" bind:value={description} maxlength="1000" />
          </div>
          <div class="field">
            <label class="check"><input type="checkbox" bind:checked={isPrivate} /> Private</label>
          </div>
          <div class="field">
            <button class="primary" disabled={creating || !name}>Create repository</button>
          </div>
        </div>
      </form>
    </aside>
  </div>
{:else}
  <section class="hero glass rise">
    <h1>Fornax</h1>
    <p class="muted">Git hosting, CI/CD and GitOps in a single Rust binary.</p>
    <p>
      {#if session.oidc}
        <a class="button primary" href="/api/v1/auth/oidc/login?return_to=/">Sign in</a>
      {:else}
        <a class="button primary" href="/settings">Sign in with an access token</a>
      {/if}
    </p>
    <p class="muted small">Public repositories can be browsed without signing in: go to <code>/&lt;owner&gt;</code>.</p>
  </section>
{/if}

<style>
  .layout { display: grid; grid-template-columns: 1fr 320px; gap: 24px; align-items: start; }
  @media (max-width: 800px) { .layout { grid-template-columns: 1fr; } }
  .check { display: flex; gap: 8px; align-items: center; font-weight: 500; }
  .check input { width: auto; }
  .hero { text-align: center; padding: 56px 24px; max-width: 640px; margin: 48px auto; border-radius: 28px; }
  .hero h1 {
    font-size: 44px; letter-spacing: -0.03em;
    background: linear-gradient(135deg, var(--fg), var(--accent)); -webkit-background-clip: text;
    background-clip: text; color: transparent;
  }
</style>
