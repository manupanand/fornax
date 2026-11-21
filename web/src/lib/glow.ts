// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

// Svelte action: a glass surface catches a soft highlight under the pointer.
// Sets --mx / --my (pointer position) and --ho (0/1 hover) on the element; the
// `.glass` styles in app.css paint the light. Off for coarse pointers and reduced motion.

const enabled =
  typeof matchMedia === 'function' &&
  matchMedia('(hover: hover) and (pointer: fine)').matches &&
  !matchMedia('(prefers-reduced-motion: reduce)').matches;

export function glow(node: HTMLElement) {
  if (!enabled) return {};
  let frame = 0;
  let last: PointerEvent | null = null;

  const paint = () => {
    frame = 0;
    if (!last) return;
    const r = node.getBoundingClientRect();
    node.style.setProperty('--mx', `${last.clientX - r.left}px`);
    node.style.setProperty('--my', `${last.clientY - r.top}px`);
  };
  const move = (e: PointerEvent) => {
    last = e;
    if (!frame) frame = requestAnimationFrame(paint);
  };
  const enter = () => node.style.setProperty('--ho', '1');
  const leave = () => node.style.setProperty('--ho', '0');

  node.addEventListener('pointermove', move);
  node.addEventListener('pointerenter', enter);
  node.addEventListener('pointerleave', leave);
  return {
    destroy() {
      if (frame) cancelAnimationFrame(frame);
      node.removeEventListener('pointermove', move);
      node.removeEventListener('pointerenter', enter);
      node.removeEventListener('pointerleave', leave);
    },
  };
}
