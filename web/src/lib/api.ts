// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

// Typed client for the Fornax REST API (`/api/v1`).
//
// Browser sessions (OIDC login) use the HttpOnly cookie. Users without OIDC can paste a
// personal access token; it is kept in sessionStorage only (cleared when the tab closes).

export interface User {
  id: number;
  username: string;
  is_admin: boolean;
  is_org: boolean;
  email: string | null;
  created_at: number;
}

export type Permission = 'none' | 'read' | 'write' | 'admin';

export interface Repo {
  id: number;
  owner: string;
  owner_kind: 'user' | 'org';
  name: string;
  full_name: string;
  description: string;
  private: boolean;
  default_branch: string;
  clone_url: string;
  ssh_url: string | null;
  permission: Permission;
  created_at: number;
  updated_at: number;
}

export interface Branch {
  name: string;
  commit: string;
}

export interface Commit {
  id: string;
  author_name: string;
  author_email: string;
  authored_at: number;
  summary: string;
}

export interface TreeEntry {
  name: string;
  kind: 'blob' | 'tree' | 'commit';
  mode: string;
  id: string;
  size: number | null;
}

export interface SshKey {
  id: number;
  name: string;
  fingerprint: string;
  created_at: number;
  last_used_at: number | null;
}

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

const TOKEN_KEY = 'fornax.token';

export function getToken(): string | null {
  try {
    return sessionStorage.getItem(TOKEN_KEY);
  } catch {
    return null;
  }
}

export function setToken(token: string | null): void {
  try {
    if (token) sessionStorage.setItem(TOKEN_KEY, token);
    else sessionStorage.removeItem(TOKEN_KEY);
  } catch {
    /* storage unavailable: token lasts for this page only */
  }
}

async function request(method: string, path: string, body?: unknown): Promise<Response> {
  const headers: Record<string, string> = {};
  const token = getToken();
  if (token) headers['Authorization'] = `Bearer ${token}`;
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  const res = await fetch(`/api/v1${path}`, {
    method,
    headers,
    credentials: 'same-origin',
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) {
    let message = res.statusText;
    try {
      const data = await res.json();
      message = data?.error?.message ?? message;
    } catch {
      /* not JSON */
    }
    throw new ApiError(res.status, message);
  }
  return res;
}

async function json<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await request(method, path, body);
  return res.status === 204 ? (undefined as T) : ((await res.json()) as T);
}

const q = (params: Record<string, string | undefined>) => {
  const s = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v !== undefined && v !== '') s.set(k, v);
  const str = s.toString();
  return str ? `?${str}` : '';
};

const seg = encodeURIComponent;

export const api = {
  authConfig: () => json<{ oidc: boolean }>('GET', '/auth/config'),
  logout: () => json<void>('POST', '/auth/logout'),
  me: () => json<User>('GET', '/user'),
  ownerRepos: (owner: string) => json<Repo[]>('GET', `/users/${seg(owner)}/repos`),
  createRepo: (body: { name: string; description?: string; private?: boolean }) =>
    json<Repo>('POST', '/user/repos', body),
  repo: (owner: string, name: string) => json<Repo>('GET', `/repos/${seg(owner)}/${seg(name)}`),
  branches: (owner: string, name: string) =>
    json<Branch[]>('GET', `/repos/${seg(owner)}/${seg(name)}/branches`),
  commits: (owner: string, name: string, ref?: string) =>
    json<Commit[]>('GET', `/repos/${seg(owner)}/${seg(name)}/commits${q({ ref, limit: '50' })}`),
  tree: (owner: string, name: string, ref: string, path: string) =>
    json<TreeEntry[]>('GET', `/repos/${seg(owner)}/${seg(name)}/tree${q({ ref, path })}`),
  raw: async (owner: string, name: string, ref: string, path: string): Promise<Uint8Array> => {
    const res = await request('GET', `/repos/${seg(owner)}/${seg(name)}/raw${q({ ref, path })}`);
    return new Uint8Array(await res.arrayBuffer());
  },
  keys: () => json<SshKey[]>('GET', '/user/keys'),
  addKey: (name: string, key: string) => json<SshKey>('POST', '/user/keys', { name, key }),
  deleteKey: (id: number) => json<void>('DELETE', `/user/keys/${id}`),
};

export function timeAgo(unix: number): string {
  const s = Math.max(0, Math.floor(Date.now() / 1000) - unix);
  if (s < 60) return 'just now';
  const units: [number, string][] = [
    [31536000, 'year'],
    [2592000, 'month'],
    [86400, 'day'],
    [3600, 'hour'],
    [60, 'minute'],
  ];
  for (const [n, name] of units) {
    if (s >= n) {
      const v = Math.floor(s / n);
      return `${v} ${name}${v === 1 ? '' : 's'} ago`;
    }
  }
  return 'just now';
}

export function formatSize(n: number | null): string {
  if (n === null) return '';
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}
