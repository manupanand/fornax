// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

// Appearance (system / light / dark) and accent palette, remembered per browser.
// Applied as classes on <html>: `.dark` when the effective scheme is dark, and
// `data-accent="<id>"` for the palette (CSS tokens in app.css).

export type Appearance = 'system' | 'light' | 'dark';

export const ACCENTS = [
  { id: 'ember', label: 'Ember', swatch: '#e8590c' },
  { id: 'ocean', label: 'Ocean', swatch: '#2563eb' },
  { id: 'violet', label: 'Violet', swatch: '#7c3aed' },
  { id: 'emerald', label: 'Emerald', swatch: '#059669' },
  { id: 'ink', label: 'Ink', swatch: '#27272a' },
] as const;

export type AccentId = (typeof ACCENTS)[number]['id'];

const KEY = 'fornax.theme';
const media = typeof matchMedia === 'function' ? matchMedia('(prefers-color-scheme: dark)') : null;

function read(): { appearance: Appearance; accent: AccentId } {
  try {
    const raw = JSON.parse(localStorage.getItem(KEY) ?? '{}');
    const appearance: Appearance = ['system', 'light', 'dark'].includes(raw.appearance) ? raw.appearance : 'system';
    const accent: AccentId = ACCENTS.some((a) => a.id === raw.accent) ? raw.accent : 'ember';
    return { appearance, accent };
  } catch {
    return { appearance: 'system', accent: 'ember' };
  }
}

class Theme {
  appearance = $state<Appearance>('system');
  accent = $state<AccentId>('ember');
  dark = $state(false);

  constructor() {
    const saved = read();
    this.appearance = saved.appearance;
    this.accent = saved.accent;
    this.apply();
    media?.addEventListener('change', () => this.apply());
  }

  set(changes: Partial<{ appearance: Appearance; accent: AccentId }>) {
    if (changes.appearance) this.appearance = changes.appearance;
    if (changes.accent) this.accent = changes.accent;
    try {
      localStorage.setItem(KEY, JSON.stringify({ appearance: this.appearance, accent: this.accent }));
    } catch {
      /* storage unavailable: applies to this page only */
    }
    this.apply();
  }

  private apply() {
    this.dark = this.appearance === 'dark' || (this.appearance === 'system' && !!media?.matches);
    const root = document.documentElement;
    root.classList.toggle('dark', this.dark);
    root.style.colorScheme = this.dark ? 'dark' : 'light';
    if (this.accent === 'ember') root.removeAttribute('data-accent');
    else root.setAttribute('data-accent', this.accent);
  }
}

export const theme = new Theme();
