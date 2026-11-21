// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

// Who is signed in, shared by all pages.

import { ApiError, api, setToken, type User } from './api';

class Session {
  user = $state<User | null>(null);
  oidc = $state(false);
  ready = $state(false);

  async load(): Promise<void> {
    try {
      this.oidc = (await api.authConfig()).oidc;
    } catch {
      this.oidc = false;
    }
    try {
      this.user = await api.me();
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) setToken(null);
      this.user = null;
    }
    this.ready = true;
  }

  async useToken(token: string): Promise<void> {
    setToken(token.trim());
    await this.load();
    if (!this.user) throw new Error('That token was not accepted.');
  }

  async signOut(): Promise<void> {
    setToken(null);
    try {
      await api.logout();
    } catch {
      /* no browser session */
    }
    this.user = null;
  }
}

export const session = new Session();
