<!-- Copyright 2026 Manu P Anand
     Licensed under the Apache License, Version 2.0
     See LICENSE for details. -->
<script lang="ts">
  // Lazy file tree for the repository sidebar. Folders load on first expand; the folders
  // leading to the current path open automatically; the current entry is highlighted.
  import { api, type TreeEntry } from './api';
  import Icon from './Icon.svelte';
  import { href, link } from './router.svelte';

  let {
    owner,
    repo,
    ref,
    path,
    expandSelf = false,
  }: { owner: string; repo: string; ref: string; path: string; expandSelf?: boolean } = $props();

  type Node = TreeEntry[] | 'loading' | 'error';
  let nodes = $state<Record<string, Node>>({});
  let open = $state<Record<string, boolean>>({ '': true });
  let loadedFor = '';

  const join = (dir: string, name: string) => (dir ? `${dir}/${name}` : name);

  function load(dir: string) {
    if (nodes[dir] && nodes[dir] !== 'error') return;
    nodes[dir] = 'loading';
    api
      .tree(owner, repo, ref, dir)
      .then((entries) => (nodes[dir] = entries))
      .catch(() => (nodes[dir] = 'error'));
  }

  function toggle(dir: string) {
    open[dir] = !open[dir];
    if (open[dir]) load(dir);
  }

  // New ref → start over. New path → open its ancestors.
  $effect(() => {
    const key = `${owner}/${repo}@${ref}`;
    if (key !== loadedFor) {
      loadedFor = key;
      nodes = {};
      open = { '': true };
    }
    load('');
    const parts = path.split('/').filter(Boolean);
    // Ancestors always; the current folder too when `path` is a directory.
    const last = expandSelf ? parts.length : parts.length - 1;
    for (let i = 1; i <= last; i++) {
      const dir = parts.slice(0, i).join('/');
      open[dir] = true;
      load(dir);
    }
  });

  const isActive = (p: string) => p === path;
  const isAncestor = (p: string) => path.startsWith(p + '/');
</script>

{#snippet branch(dir: string, depth: number)}
  {@const node = nodes[dir]}
  {#if node === 'loading' || node === undefined}
    <li class="placeholder" style:--depth={depth}><span class="shimmer"></span></li>
  {:else if node === 'error'}
    <li class="placeholder faint small" style:--depth={depth}>Couldn't load</li>
  {:else}
    {#each node as entry (entry.name)}
      {@const p = join(dir, entry.name)}
      <li role="treeitem" aria-selected={isActive(p)} aria-expanded={entry.kind === 'tree' ? !!open[p] : undefined}>
        <div class="item" class:active={isActive(p)} class:ancestor={isAncestor(p)} style:--depth={depth}>
          {#if entry.kind === 'tree'}
            <button
              class="twisty"
              class:open={open[p]}
              aria-label={open[p] ? `Collapse ${entry.name}` : `Expand ${entry.name}`}
              onclick={() => toggle(p)}
            >
              <Icon name="chevron" size={12} />
            </button>
            <a
              href={href({ name: 'repo', owner, repo, view: 'tree', ref, path: p })}
              use:link
              onclick={() => {
                if (!open[p]) toggle(p);
              }}
            >
              <span class="glyph folder"><Icon name={open[p] ? 'folder-open' : 'folder'} size={15} /></span>
              <span class="ellipsis">{entry.name}</span>
            </a>
          {:else if entry.kind === 'commit'}
            <span class="spacer"></span>
            <span class="static faint" title="Submodule at {entry.id.slice(0, 8)}">
              <span class="glyph"><Icon name="module" size={15} /></span>
              <span class="ellipsis">{entry.name}</span>
            </span>
          {:else}
            <span class="spacer"></span>
            <a href={href({ name: 'repo', owner, repo, view: 'blob', ref, path: p })} use:link>
              <span class="glyph"><Icon name="file" size={15} /></span>
              <span class="ellipsis">{entry.name}</span>
            </a>
          {/if}
        </div>
        {#if entry.kind === 'tree' && open[p]}
          <ul role="group">{@render branch(p, depth + 1)}</ul>
        {/if}
      </li>
    {:else}
      <li class="placeholder faint small" style:--depth={depth}>Empty</li>
    {/each}
  {/if}
{/snippet}

<ul class="tree" role="tree" aria-label="Files">
  {@render branch('', 0)}
</ul>

<style>
  ul { list-style: none; margin: 0; padding: 0; }
  .tree { padding: 6px; font-size: 13.5px; }
  .item {
    display: flex; align-items: center; gap: 2px; height: 30px;
    padding-left: calc(var(--depth) * 14px + 2px); padding-right: 6px;
    border-radius: 10px; transition: background 0.15s var(--ease);
  }
  .item:hover { background: var(--hover); }
  .item.active { background: var(--selected); }
  .item.active a { color: var(--fg); font-weight: 600; }
  .item.ancestor a { color: var(--fg); }
  .item a, .static {
    display: flex; align-items: center; gap: 7px; min-width: 0; flex: 1; height: 100%;
    color: var(--fg-muted); text-decoration: none;
  }
  .item a:hover { color: var(--fg); }
  .glyph { display: inline-flex; flex: none; color: var(--fg-faint); }
  .glyph.folder { color: var(--accent); }
  .twisty, .spacer { width: 20px; height: 20px; flex: none; }
  .twisty {
    padding: 0; border: 0; background: transparent; box-shadow: none; border-radius: 6px;
    color: var(--fg-faint); -webkit-backdrop-filter: none; backdrop-filter: none;
    transition: transform 0.25s var(--spring), color 0.15s var(--ease);
  }
  .twisty:hover { background: var(--hover); color: var(--fg); box-shadow: none; }
  .twisty.open { transform: rotate(90deg); }
  .twisty.open:active { transform: rotate(90deg) scale(0.9); }
  .placeholder { height: 28px; display: flex; align-items: center; padding-left: calc(var(--depth) * 14px + 30px); }
  .shimmer {
    display: block; height: 10px; width: 55%; border-radius: 6px;
    background: linear-gradient(90deg, var(--hover), var(--glass-strong), var(--hover));
    background-size: 200% 100%; animation: shimmer 1.2s linear infinite;
  }
  @keyframes shimmer { from { background-position: 200% 0; } to { background-position: -200% 0; } }
</style>
