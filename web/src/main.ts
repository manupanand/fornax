// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

import { mount } from 'svelte';
import App from './App.svelte';
import './lib/theme.svelte'; // applies saved appearance/accent before first render
import './app.css';

const target = document.getElementById('app');
if (!target) throw new Error('missing #app');
export default mount(App, { target });
