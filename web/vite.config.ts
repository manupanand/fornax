// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// `npm run dev` proxies API and Git calls to a local `fornax serve` on :3000.
export default defineConfig({
  plugins: [svelte()],
  build: { outDir: 'dist', emptyOutDir: true, sourcemap: false },
  server: {
    proxy: {
      '/api': 'http://127.0.0.1:3000',
    },
  },
});
