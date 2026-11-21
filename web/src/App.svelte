<!-- Copyright 2026 Manu P Anand
     Licensed under the Apache License, Version 2.0
     See LICENSE for details. -->
<script lang="ts">
  import { glow } from './lib/glow';
  import Icon, { type IconName } from './lib/Icon.svelte';
  import { link, router } from './lib/router.svelte';
  import { session } from './lib/session.svelte';
  import ThemeMenu from './lib/ThemeMenu.svelte';
  import Home from './routes/Home.svelte';
  import Owner from './routes/Owner.svelte';
  import Repo from './routes/Repo.svelte';
  import Settings from './routes/Settings.svelte';

  session.load();
  const route = $derived(router.route);

  // ---- navigation sidebar: full width ↔ icon rail on desktop, drawer on phones -------
  const NAV_KEY = 'fornax.nav';
  const phoneQuery = typeof matchMedia === 'function' ? matchMedia('(max-width: 760px)') : null;
  let phone = $state(phoneQuery?.matches ?? false);
  let drawerOpen = $state(false);
  function storedRail(): boolean {
    try {
      return localStorage.getItem(NAV_KEY) === 'rail';
    } catch {
      return false;
    }
  }
  let rail = $state(storedRail());
  // Row-by-row entrance only after the user toggles (not on first paint).
  let toggled = $state(false);

  function toggleRail() {
    rail = !rail;
    toggled = true;
    try {
      localStorage.setItem(NAV_KEY, rail ? 'rail' : 'full');
    } catch {
      /* storage unavailable */
    }
  }

  $effect(() => {
    if (!phoneQuery) return;
    const onChange = (e: MediaQueryListEvent) => {
      phone = e.matches;
      drawerOpen = false;
    };
    phoneQuery.addEventListener('change', onChange);
    return () => phoneQuery.removeEventListener('change', onChange);
  });

  // Close the drawer after navigating.
  $effect(() => {
    void router.route;
    drawerOpen = false;
  });

  const collapsed = $derived(!phone && rail);

  type NavItem = { href: string; label: string; icon: IconName; active: boolean };
  const items = $derived.by((): NavItem[] => {
    const me = session.user?.username;
    const list: NavItem[] = [{ href: '/', label: 'Home', icon: 'home', active: route.name === 'home' }];
    if (me) {
      list.push({
        href: '/' + encodeURIComponent(me),
        label: 'Repositories',
        icon: 'repo',
        active: (route.name === 'owner' && route.owner === me) || (route.name === 'repo' && route.owner === me),
      });
    }
    list.push({ href: '/settings', label: me ? 'Settings' : 'Sign in', icon: me ? 'settings' : 'login', active: route.name === 'settings' });
    return list;
  });

  function onKey(e: KeyboardEvent) {
    if (e.key === 'Escape' && drawerOpen) drawerOpen = false;
  }
</script>

<svelte:window onkeydown={onKey} />

<div class="shell" class:collapsed class:phone>
  {#if phone}
    <header class="topbar glass" use:glow>
      <button class="ghost icon" aria-label="Open navigation" aria-expanded={drawerOpen} aria-controls="app-nav" onclick={() => (drawerOpen = true)}>
        <Icon name="menu" size={18} />
      </button>
      <a href="/" use:link class="brand"><img src="/favicon.svg" alt="" width="24" height="24" /><span>Fornax</span></a>
      <span class="grow"></span>
      {#if session.user}
        <a class="avatar" href={'/' + encodeURIComponent(session.user.username)} use:link aria-label="Your repositories">
          {session.user.username.slice(0, 1).toUpperCase()}
        </a>
      {/if}
    </header>
    {#if drawerOpen}
      <button class="scrim" aria-label="Close navigation" onclick={() => (drawerOpen = false)}></button>
    {/if}
  {/if}

  <nav
    id="app-nav"
    class="appnav glass"
    class:open={drawerOpen}
    class:waved={toggled}
    aria-label="Main"
    inert={phone && !drawerOpen}
    use:glow
  >
    <div class="nav-head">
      <a href="/" use:link class="brand" title="Fornax">
        <img src="/favicon.svg" alt="" width="28" height="28" />
        <span class="label">Fornax</span>
      </a>
      {#if !phone}
        <button class="ghost icon collapse" onclick={toggleRail} aria-label={rail ? 'Expand navigation' : 'Collapse navigation'} aria-expanded={!rail} title={rail ? 'Expand' : 'Collapse'}>
          <Icon name="sidebar" size={17} />
        </button>
      {/if}
    </div>

    <ul class="nav-items">
      {#each items as item, i (item.href + item.label)}
        <li style:--i={i}>
          <a href={item.href} use:link class="nav-btn" class:active={item.active} aria-current={item.active ? 'page' : undefined} title={collapsed ? item.label : undefined}>
            <Icon name={item.icon} size={18} />
            <span class="label">{item.label}</span>
          </a>
        </li>
      {/each}
    </ul>

    <span class="grow"></span>

    <div class="nav-foot">
      <ThemeMenu compact={collapsed} />
      {#if session.user}
        <a class="nav-btn me" href={'/' + encodeURIComponent(session.user.username)} use:link title={collapsed ? session.user.username : undefined}>
          <span class="avatar small" aria-hidden="true">{session.user.username.slice(0, 1).toUpperCase()}</span>
          <span class="label ellipsis">{session.user.username}</span>
        </a>
      {:else if session.ready && session.oidc}
        <a class="nav-btn" href={'/api/v1/auth/oidc/login?return_to=' + encodeURIComponent(location.pathname)} title={collapsed ? 'Sign in with SSO' : undefined}>
          <Icon name="login" size={18} /><span class="label">Sign in with SSO</span>
        </a>
      {/if}
    </div>
  </nav>

  <main>
    {#if !session.ready}
      <p class="muted">Loading…</p>
    {:else if route.name === 'home'}
      <Home />
    {:else if route.name === 'settings'}
      <Settings />
    {:else if route.name === 'owner'}
      {#key route.owner}
        <Owner owner={route.owner} />
      {/key}
    {:else if route.name === 'repo'}
      {#key route.owner + '/' + route.repo}
        <Repo {route} />
      {/key}
    {:else}
      <h1>Not found</h1>
      <p><a href="/" use:link>Go home</a></p>
    {/if}
  </main>
</div>

<style>
  .shell {
    --nav-w: 232px; --rail-w: 64px; --gap: 12px;
    display: grid; grid-template-columns: var(--nav-w) minmax(0, 1fr); min-height: 100vh;
    transition: grid-template-columns 0.5s var(--ease-out);
  }
  .shell.collapsed { grid-template-columns: var(--rail-w) minmax(0, 1fr); }
  .shell.phone { grid-template-columns: minmax(0, 1fr); grid-template-rows: auto 1fr; align-items: start; }

  /* Floating sidebar panel. */
  .appnav {
    position: sticky; top: var(--gap); align-self: start; margin: var(--gap) 0 var(--gap) var(--gap);
    height: calc(100vh - 2 * var(--gap)); display: flex; flex-direction: column; gap: 6px;
    padding: 10px 8px; border-radius: 20px; z-index: 15;
  }
  .nav-head { display: flex; align-items: center; gap: 4px; padding: 2px 2px 8px 6px; min-height: 44px; }
  .brand { display: flex; align-items: center; gap: 9px; color: var(--fg); font-weight: 750; font-size: 17px; letter-spacing: -0.01em; min-width: 0; }
  .brand:hover { text-decoration: none; }
  .brand img { flex: none; filter: drop-shadow(0 2px 6px color-mix(in srgb, var(--accent) 40%, transparent)); }
  .collapse { margin-left: auto; color: var(--fg-muted); }

  ul { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 2px; }
  .nav-btn, :global(.appnav .nav-btn) {
    display: flex; align-items: center; justify-content: flex-start; gap: 12px; width: 100%;
    height: 40px; padding: 0 12px; border-radius: var(--r-md); color: var(--fg-muted);
    font-weight: 600; font-size: 14px; white-space: nowrap; text-decoration: none; overflow: hidden;
    transition: background 0.2s var(--ease), color 0.2s var(--ease), padding 0.4s var(--ease-out);
  }
  .nav-btn:hover, :global(.appnav .nav-btn:hover) { background: var(--hover); color: var(--fg); text-decoration: none; }
  .nav-btn.active { color: var(--fg); background: var(--glass-strong); box-shadow: var(--rim), var(--depth); }
  .nav-btn.active :global(svg) { color: var(--accent); }
  .nav-btn :global(svg), :global(.appnav .nav-btn svg), .avatar { flex: none; }
  .label { min-width: 0; overflow: hidden; text-overflow: ellipsis; }
  .nav-foot { display: flex; flex-direction: column; gap: 2px; border-top: 1px solid var(--glass-edge); padding-top: 8px; }
  .label { transition: opacity 0.25s var(--ease); }

  /* Icon rail. */
  .collapsed .label { opacity: 0; pointer-events: none; }
  .collapsed .nav-head { flex-direction: column; padding: 2px 0 8px; gap: 8px; }
  .collapsed .brand .label { display: none; }
  .collapsed .collapse { margin: 0; }
  .collapsed .nav-btn, .collapsed :global(.appnav .nav-btn) { padding: 0 9px; gap: 0; }
  .collapsed .nav-btn:hover :global(svg) { animation: nudge 0.5s var(--spring); }
  @keyframes nudge { 40% { transform: translateY(-2px) scale(1.15); } 100% { transform: none; } }

  /* Rows glide in one after another after a toggle. */
  .waved .nav-items li { animation: wave 0.5s var(--ease-out) backwards; animation-delay: calc(var(--i) * 40ms + 60ms); }
  @keyframes wave { from { opacity: 0; transform: translateX(-10px); } to { opacity: 1; transform: none; } }

  .avatar {
    width: 30px; height: 30px; border-radius: 50%; display: inline-grid; place-items: center; flex: none;
    font-size: 13px; font-weight: 750; color: var(--accent-fg); text-decoration: none;
    background: linear-gradient(135deg, var(--accent), var(--accent-2));
    box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.35);
  }
  .avatar:hover { text-decoration: none; }
  .avatar.small { width: 22px; height: 22px; font-size: 11px; margin: 0 -2px; }

  main { max-width: 1240px; width: 100%; margin: 0 auto; padding: 24px 24px 72px; min-width: 0; }

  /* Phones: top capsule bar + drawer. */
  .topbar {
    position: sticky; top: 10px; z-index: 20; margin: 10px 12px 0; display: flex; align-items: center; gap: 8px;
    padding: 6px 8px; border-radius: var(--r-pill); box-shadow: var(--rim), var(--depth-lift);
  }
  .phone .appnav {
    position: fixed; top: 12px; bottom: 12px; left: 12px; margin: 0; height: auto; align-self: stretch; width: min(280px, calc(100vw - 48px));
    z-index: 45; background-color: var(--glass-strong); box-shadow: var(--rim), var(--depth-lift);
    transform: translateX(calc(-100% - 24px)); transition: transform 0.4s var(--ease-out);
  }
  .phone .appnav.open { transform: none; }
  .scrim {
    position: fixed; inset: 0; z-index: 40; border: 0; border-radius: 0; padding: 0;
    background: rgba(8, 12, 20, 0.3); box-shadow: none;
    -webkit-backdrop-filter: blur(4px); backdrop-filter: blur(4px);
  }
  .scrim:active { scale: 1; }
  .phone main { padding: 18px 14px 64px; }
</style>
