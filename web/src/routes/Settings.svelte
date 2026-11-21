<!-- Copyright 2026 Manu P Anand
     Licensed under the Apache License, Version 2.0
     See LICENSE for details. -->
<script lang="ts">
  import { api, timeAgo, type SshKey } from '../lib/api';
  import { router } from '../lib/router.svelte';
  import { session } from '../lib/session.svelte';

  let token = $state('');
  let error = $state('');
  let keys = $state<SshKey[]>([]);
  let keyName = $state('');
  let keyText = $state('');

  $effect(() => {
    if (session.user) api.keys().then((k) => (keys = k)).catch((e) => (error = e.message));
  });

  async function signIn(e: SubmitEvent) {
    e.preventDefault();
    error = '';
    try {
      await session.useToken(token);
      token = '';
      router.go('/');
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    }
  }

  async function addKey(e: SubmitEvent) {
    e.preventDefault();
    error = '';
    try {
      const k = await api.addKey(keyName, keyText);
      keys = [...keys, k];
      keyName = keyText = '';
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    }
  }

  async function removeKey(id: number) {
    await api.deleteKey(id);
    keys = keys.filter((k) => k.id !== id);
  }

  async function signOut() {
    await session.signOut();
    router.go('/');
  }
</script>

<div class="stack narrow">
  {#if error}<div class="error">{error}</div>{/if}

  {#if !session.user}
    <h1>Sign in</h1>
    {#if session.oidc}
      <p><a class="button primary" href="/api/v1/auth/oidc/login?return_to=/">Sign in with SSO</a></p>
      <p class="muted">Or use a personal access token:</p>
    {/if}
    <form class="box" onsubmit={signIn}>
      <div style="padding: 14px">
        <div class="field">
          <label for="token">Access token</label>
          <input id="token" type="password" bind:value={token} placeholder="fnx_…" autocomplete="off" required />
        </div>
        <p class="muted small">
          Create one with <code>fornax admin create-token &lt;user&gt;</code>. It is kept only for this browser tab.
        </p>
        <button class="primary" disabled={!token}>Sign in</button>
      </div>
    </form>
  {:else}
    <h1>Settings</h1>
    <section class="box">
      <div class="box-header"><strong>SSH keys</strong></div>
      {#each keys as k (k.id)}
        <div class="row">
          <div class="grow">
            <strong>{k.name}</strong>
            <div class="mono small muted ellipsis">{k.fingerprint}</div>
            <div class="small muted">Added {timeAgo(k.created_at)}{k.last_used_at ? ` · last used ${timeAgo(k.last_used_at)}` : ' · never used'}</div>
          </div>
          <button class="danger" onclick={() => removeKey(k.id)}>Delete</button>
        </div>
      {:else}
        <div class="row muted">No SSH keys.</div>
      {/each}
      <form class="row add" onsubmit={addKey}>
        <div class="field">
          <label for="kname">Title</label>
          <input id="kname" bind:value={keyName} required maxlength="100" placeholder="laptop" />
        </div>
        <div class="field">
          <label for="ktext">Public key</label>
          <textarea id="ktext" bind:value={keyText} required rows="3" placeholder="ssh-ed25519 AAAA… you@host"></textarea>
        </div>
        <button class="primary" disabled={!keyName || !keyText}>Add SSH key</button>
      </form>
    </section>
    <p><button onclick={signOut}>Sign out</button></p>
  {/if}
</div>

<style>
  .narrow { max-width: 680px; }
  .add { display: block; }
  .add button { margin-top: 12px; }
</style>
