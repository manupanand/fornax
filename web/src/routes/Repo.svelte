<!-- Copyright 2026 Manu P Anand
     Licensed under the Apache License, Version 2.0
     See LICENSE for details. -->
<script lang="ts">
  import { api, formatSize, timeAgo, type Branch, type Commit, type Repo, type TreeEntry } from '../lib/api';
  import FileTree from '../lib/FileTree.svelte';
  import { glow } from '../lib/glow';
  import Icon from '../lib/Icon.svelte';
  import { href, link, router, type Route } from '../lib/router.svelte';

  type RepoRoute = Extract<Route, { name: 'repo' }>;
  let { route }: { route: RepoRoute } = $props();

  let repo = $state<Repo | null>(null);
  let branches = $state<Branch[]>([]);
  let error = $state('');
  let entries = $state<TreeEntry[] | null>(null);
  let commits = $state<Commit[] | null>(null);
  let file = $state<{ text: string | null; size: number } | null>(null);
  let readme = $state<string | null>(null);
  let cloneKind = $state<'https' | 'ssh'>('https');

  const ref = $derived(route.ref ?? repo?.default_branch ?? 'main');
  const empty = $derived(repo !== null && branches.length === 0);
  const crumbs = $derived(route.path ? route.path.split('/') : []);

  const decoder = new TextDecoder('utf-8', { fatal: false });
  const toText = (bytes: Uint8Array) => (bytes.includes(0) ? null : decoder.decode(bytes));

  function to(view: RepoRoute['view'], path = '', r = ref): string {
    return href({ name: 'repo', owner: route.owner, repo: route.repo, view, ref: r, path });
  }

  // ---- sidebar: open on wide screens (remembered), a drawer on narrow ones -------------
  const SIDEBAR_KEY = 'fornax.sidebar';
  const narrowQuery = typeof matchMedia === 'function' ? matchMedia('(max-width: 900px)') : null;
  let narrow = $state(narrowQuery?.matches ?? false);
  function storedOpen(): boolean {
    try {
      return localStorage.getItem(SIDEBAR_KEY) !== 'closed';
    } catch {
      return true;
    }
  }
  let sidebarOpen = $state(narrowQuery?.matches ? false : storedOpen());

  function setSidebar(open: boolean) {
    sidebarOpen = open;
    if (narrow) return; // the drawer state on phones isn't a preference
    try {
      localStorage.setItem(SIDEBAR_KEY, open ? 'open' : 'closed');
    } catch {
      /* storage unavailable */
    }
  }

  $effect(() => {
    if (!narrowQuery) return;
    const onChange = (e: MediaQueryListEvent) => {
      narrow = e.matches;
      sidebarOpen = e.matches ? false : storedOpen();
    };
    narrowQuery.addEventListener('change', onChange);
    return () => narrowQuery.removeEventListener('change', onChange);
  });

  // Close the drawer after navigating on phones.
  $effect(() => {
    void route.path;
    void route.view;
    if (narrow) sidebarOpen = false;
  });

  const showSidebar = $derived(!empty && route.view !== 'commits');

  function onKey(e: KeyboardEvent) {
    if (e.key === 'Escape' && narrow && sidebarOpen) sidebarOpen = false;
  }

  // ---- data ----------------------------------------------------------------------------
  $effect(() => {
    Promise.all([api.repo(route.owner, route.repo), api.branches(route.owner, route.repo)])
      .then(([r, b]) => {
        repo = r;
        branches = b;
      })
      .catch((e) => (error = e.status === 404 || e.status === 401 ? 'Repository not found.' : e.message));
  });

  // Load whatever the current view needs once the repository is known.
  $effect(() => {
    if (!repo || empty) return;
    const { owner, repo: name, view, path } = route;
    const r = ref;
    error = '';
    entries = commits = file = readme = null;
    if (view === 'commits') {
      api.commits(owner, name, r).then((c) => (commits = c)).catch((e) => (error = e.message));
    } else if (view === 'blob') {
      api.raw(owner, name, r, path).then((b) => (file = { text: toText(b), size: b.length })).catch((e) => (error = e.message));
    } else {
      api
        .tree(owner, name, r, path)
        .then((t) => {
          entries = t;
          const md = t.find((e) => e.kind === 'blob' && /^readme(\.md|\.txt)?$/i.test(e.name));
          if (md) api.raw(owner, name, r, path ? `${path}/${md.name}` : md.name).then((b) => (readme = toText(b)));
        })
        .catch((e) => (error = e.message));
    }
  });

  function switchRef(e: Event) {
    const value = (e.target as HTMLSelectElement).value;
    router.go(to(route.view === 'commits' ? 'commits' : 'tree', route.view === 'commits' ? '' : route.path, value));
  }

  const cloneUrl = $derived(cloneKind === 'ssh' && repo?.ssh_url ? repo.ssh_url : (repo?.clone_url ?? ''));
  let copied = $state(false);
  async function copyClone() {
    try {
      await navigator.clipboard.writeText(cloneUrl);
      copied = true;
      setTimeout(() => (copied = false), 1500);
    } catch {
      /* clipboard unavailable: the field is selectable */
    }
  }
</script>

<svelte:window onkeydown={onKey} />

{#if error && !repo}
  <div class="error">{error}</div>
{:else if repo}
  <div class="stack">
    <div class="title">
      <h1>
        <a href={'/' + encodeURIComponent(repo.owner)} use:link>{repo.owner}</a>
        <span class="faint">/</span>
        <a href={'/' + encodeURIComponent(repo.owner) + '/' + encodeURIComponent(repo.name)} use:link>{repo.name}</a>
        <span class="badge">{repo.private ? 'Private' : 'Public'}</span>
      </h1>
      {#if repo.description}<p class="muted">{repo.description}</p>{/if}
    </div>

    <div class="toolbar glass" use:glow>
      {#if showSidebar}
        <button
          class="icon"
          onclick={() => setSidebar(!sidebarOpen)}
          aria-expanded={sidebarOpen}
          aria-controls="repo-sidebar"
          aria-label={sidebarOpen ? 'Hide file tree' : 'Show file tree'}
          title={sidebarOpen ? 'Hide file tree' : 'Show file tree'}
        >
          <Icon name="sidebar" size={17} />
        </button>
      {/if}
      <nav class="segmented" aria-label="Repository sections">
        <a class:active={route.view !== 'commits'} aria-current={route.view !== 'commits' ? 'page' : undefined} href={to('tree')} use:link>Code</a>
        <a class:active={route.view === 'commits'} aria-current={route.view === 'commits' ? 'page' : undefined} href={to('commits')} use:link>Commits</a>
      </nav>
      {#if !empty}
        <label class="branch">
          <span class="faint"><Icon name="branch" size={15} /></span>
          <select value={ref} onchange={switchRef} aria-label="Branch">
            {#each branches as b (b.name)}<option value={b.name}>{b.name}</option>{/each}
            {#if !branches.some((b) => b.name === ref)}<option value={ref}>{ref.length > 12 ? ref.slice(0, 8) : ref}</option>{/if}
          </select>
        </label>
      {/if}
      <span class="grow"></span>
      <div class="clone">
        {#if repo.ssh_url}
          <select bind:value={cloneKind} aria-label="Clone protocol">
            <option value="https">HTTP</option>
            <option value="ssh">SSH</option>
          </select>
        {/if}
        <input readonly value={cloneUrl} aria-label="Clone URL" class="mono" onfocus={(e) => (e.target as HTMLInputElement).select()} />
        <button class="icon" onclick={copyClone} aria-label="Copy clone URL" title={copied ? 'Copied' : 'Copy'}>
          {#if copied}✓{:else}<Icon name="copy" size={15} />{/if}
        </button>
      </div>
    </div>

    {#if empty}
      <div class="box">
        <div class="box-header"><strong>This repository is empty</strong></div>
        <pre class="code">git remote add origin {repo.clone_url}
git push -u origin {repo.default_branch}</pre>
      </div>
    {:else}
      <div class="workspace" class:with-sidebar={showSidebar} class:collapsed={!sidebarOpen} class:narrow>
        {#if showSidebar}
          {#if narrow && sidebarOpen}
            <button class="scrim" aria-label="Close file tree" onclick={() => (sidebarOpen = false)}></button>
          {/if}
          <aside id="repo-sidebar" class="sidebar glass" inert={!sidebarOpen} aria-label="File tree" use:glow>
            <div class="sidebar-head">
              <strong class="ellipsis">{repo.name}</strong>
              <span class="badge mono ellipsis" title={ref}>{ref.length > 24 ? ref.slice(0, 10) : ref}</span>
              <span class="grow"></span>
              <button class="icon small-icon" onclick={() => setSidebar(false)} aria-label="Hide file tree">
                <Icon name="sidebar" size={15} />
              </button>
            </div>
            <div class="sidebar-body">
              <FileTree owner={route.owner} repo={route.repo} {ref} path={route.path} expandSelf={route.view === 'tree'} />
            </div>
          </aside>
        {/if}

        <section class="content stack">
          {#if route.view !== 'commits'}
            <nav class="crumbs mono" aria-label="Path">
              <a href={to('tree')} use:link>{repo.name}</a>
              {#each crumbs as part, i (i)}
                <span class="faint">/</span>
                {#if i === crumbs.length - 1}<strong>{part}</strong>{:else}<a href={to('tree', crumbs.slice(0, i + 1).join('/'))} use:link>{part}</a>{/if}
              {/each}
            </nav>
          {/if}

          {#if error}<div class="error">{error}</div>{/if}

          {#if route.view === 'commits'}
            <div class="box">
              {#if commits === null}<div class="row muted">Loading…</div>{/if}
              {#each commits ?? [] as c (c.id)}
                <div class="row">
                  <span class="faint"><Icon name="commit" size={16} /></span>
                  <div class="grow">
                    <div class="ellipsis"><strong>{c.summary}</strong></div>
                    <div class="muted small">{c.author_name} committed {timeAgo(c.authored_at)}</div>
                  </div>
                  <a class="sha mono small" href={to('tree', '', c.id)} use:link title={c.id}>{c.id.slice(0, 8)}</a>
                </div>
              {/each}
            </div>
          {:else if route.view === 'blob'}
            <div class="box">
              <div class="box-header">
                <span class="faint"><Icon name="file" size={15} /></span>
                <strong class="mono">{crumbs.at(-1)}</strong>
                {#if file}<span class="muted small">{formatSize(file.size)}</span>{/if}
              </div>
              {#if file === null}
                <div class="row muted">Loading…</div>
              {:else if file.text === null}
                <div class="row muted">Binary file not shown.</div>
              {:else}
                <pre class="code">{file.text}</pre>
              {/if}
            </div>
          {:else}
            <div class="box">
              {#if entries === null}<div class="row muted">Loading…</div>{/if}
              {#if route.path}
                <div class="row"><span class="glyph"></span><a href={to('tree', crumbs.slice(0, -1).join('/'))} use:link>..</a></div>
              {/if}
              {#each entries ?? [] as e (e.name)}
                <div class="row">
                  <span class="glyph" class:folder={e.kind === 'tree'}>
                    <Icon name={e.kind === 'tree' ? 'folder' : e.kind === 'commit' ? 'module' : 'file'} size={16} />
                  </span>
                  <span class="grow ellipsis">
                    {#if e.kind === 'commit'}
                      <span class="muted">{e.name} @ {e.id.slice(0, 8)}</span>
                    {:else}
                      <a href={to(e.kind === 'tree' ? 'tree' : 'blob', route.path ? `${route.path}/${e.name}` : e.name)} use:link>{e.name}</a>
                    {/if}
                  </span>
                  <span class="muted small">{formatSize(e.size)}</span>
                </div>
              {/each}
            </div>
            {#if readme}
              <div class="box">
                <div class="box-header"><span class="faint"><Icon name="file" size={15} /></span><strong>README</strong></div>
                <pre class="code readme">{readme}</pre>
              </div>
            {/if}
          {/if}
        </section>
      </div>
    {/if}
  </div>
{:else}
  <p class="muted">Loading…</p>
{/if}

<style>
  .title h1 { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
  .title h1 a { color: var(--fg); }
  .title p { margin: 0; }

  .toolbar {
    display: flex; gap: 10px; align-items: center; flex-wrap: wrap;
    padding: 8px; border-radius: var(--r-xl);
    position: sticky; top: 12px; z-index: 5;
  }
  .branch { display: inline-flex; align-items: center; gap: 6px; margin: 0; font-weight: 500; }
  .branch select { width: auto; max-width: 220px; }
  .clone { display: flex; gap: 6px; align-items: center; min-width: 280px; flex: 0 1 440px; }
  .clone select { width: auto; }
  .clone input { border-radius: var(--r-pill); }

  /* Two columns; the sidebar column collapses to zero. */
  .workspace { display: grid; grid-template-columns: minmax(0, 1fr); gap: 16px; align-items: start; }
  .workspace.with-sidebar { grid-template-columns: 280px minmax(0, 1fr); transition: grid-template-columns 0.35s var(--ease); }
  .workspace.with-sidebar.collapsed { grid-template-columns: 0px minmax(0, 1fr); gap: 0; }
  .sidebar {
    position: sticky; top: 80px; max-height: calc(100vh - 100px); display: flex; flex-direction: column;
    border-radius: var(--r-xl); overflow: hidden; min-width: 0;
    transition: opacity 0.25s var(--ease), transform 0.35s var(--ease);
  }
  .collapsed .sidebar { opacity: 0; transform: translateX(-12px); pointer-events: none; }
  .sidebar-head {
    display: flex; align-items: center; gap: 8px; padding: 10px 10px 10px 16px;
    border-bottom: 1px solid var(--glass-edge); background: var(--glass-subtle);
  }
  .sidebar-head .badge { max-width: 120px; }
  .sidebar-body { overflow: auto; overscroll-behavior: contain; }
  .small-icon { width: 28px; height: 28px; }

  /* Phones and small windows: the sidebar is a drawer over the content. */
  .narrow.with-sidebar, .narrow.with-sidebar.collapsed { grid-template-columns: minmax(0, 1fr); gap: 16px; }
  .narrow .sidebar {
    position: fixed; top: 12px; bottom: 12px; left: 12px; width: min(320px, calc(100vw - 48px));
    max-height: none; z-index: 40; background: var(--glass-strong); box-shadow: var(--specular), var(--shadow-lift);
    transform: translateX(0); opacity: 1;
  }
  .narrow.collapsed .sidebar { transform: translateX(calc(-100% - 24px)); opacity: 0; }
  .scrim {
    position: fixed; inset: 0; z-index: 30; border: 0; border-radius: 0; padding: 0;
    background: rgba(10, 12, 18, 0.28); box-shadow: none;
    -webkit-backdrop-filter: blur(4px); backdrop-filter: blur(4px);
    animation: fade 0.25s var(--ease) both;
  }
  .scrim:active { transform: none; }
  @keyframes fade { from { opacity: 0; } to { opacity: 1; } }

  .content { min-width: 0; }
  .crumbs { font-size: 14px; display: flex; gap: 6px; flex-wrap: wrap; align-items: center; padding: 0 4px; }
  .glyph { width: 18px; display: inline-flex; justify-content: center; color: var(--fg-faint); flex: none; }
  .glyph.folder { color: var(--accent); }
  .sha {
    padding: 2px 10px; border-radius: var(--r-pill); background: var(--glass-subtle);
    border: 1px solid var(--glass-border);
  }
  .readme { white-space: pre-wrap; font-family: inherit; font-size: 14px; }

  @media (max-width: 900px) {
    .toolbar { top: 72px; }
    .clone { min-width: 0; flex: 1 1 100%; }
  }
</style>
