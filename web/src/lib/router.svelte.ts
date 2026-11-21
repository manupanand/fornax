// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

// Minimal history-API router. URLs mirror GitHub:
//   /                                  home
//   /settings                          SSH keys, sign-out
//   /:owner                            repositories of a user or organisation
//   /:owner/:repo                      repository root at the default branch
//   /:owner/:repo/tree/:ref/*path      directory
//   /:owner/:repo/blob/:ref/*path      file
//   /:owner/:repo/commits/:ref         history
// Refs containing '/' are percent-encoded as one segment.

export type Route =
  | { name: 'home' }
  | { name: 'settings' }
  | { name: 'owner'; owner: string }
  | { name: 'repo'; owner: string; repo: string; view: 'tree' | 'blob' | 'commits'; ref?: string; path: string }
  | { name: 'notfound' };

export function parse(pathname: string): Route {
  const parts = pathname.split('/').filter(Boolean).map(decodeURIComponent);
  if (parts.length === 0) return { name: 'home' };
  if (parts.length === 1 && parts[0] === 'settings') return { name: 'settings' };
  if (parts.length === 1) return { name: 'owner', owner: parts[0] };
  const [owner, repo, view, ref, ...rest] = parts;
  if (parts.length === 2) return { name: 'repo', owner, repo, view: 'tree', path: '' };
  if ((view === 'tree' || view === 'blob' || view === 'commits') && ref !== undefined) {
    return { name: 'repo', owner, repo, view, ref, path: rest.join('/') };
  }
  return { name: 'notfound' };
}

export function href(route: Route): string {
  const e = encodeURIComponent;
  switch (route.name) {
    case 'home':
      return '/';
    case 'settings':
      return '/settings';
    case 'owner':
      return `/${e(route.owner)}`;
    case 'repo': {
      const base = `/${e(route.owner)}/${e(route.repo)}`;
      if (!route.ref) return base;
      const path = route.path ? '/' + route.path.split('/').map(e).join('/') : '';
      return `${base}/${route.view}/${e(route.ref)}${path}`;
    }
    default:
      return '/';
  }
}

class Router {
  route = $state<Route>(parse(location.pathname));

  constructor() {
    addEventListener('popstate', () => (this.route = parse(location.pathname)));
  }

  go(to: string | Route): void {
    const url = typeof to === 'string' ? to : href(to);
    if (url !== location.pathname) history.pushState(null, '', url);
    this.route = parse(location.pathname);
    scrollTo(0, 0);
  }
}

export const router = new Router();

/** Svelte action: intercept same-origin link clicks for client-side navigation. */
export function link(node: HTMLAnchorElement) {
  const onclick = (e: MouseEvent) => {
    if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
    const url = new URL(node.href);
    if (url.origin !== location.origin || url.pathname.startsWith('/api/')) return;
    e.preventDefault();
    router.go(url.pathname);
  };
  node.addEventListener('click', onclick);
  return { destroy: () => node.removeEventListener('click', onclick) };
}
