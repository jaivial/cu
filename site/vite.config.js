import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// Served from https://jaivial.github.io/cu/, so every asset URL is absolute
// under /cu/ rather than the root of the domain.
export default defineConfig({
  base: '/cu/',
  plugins: [svelte()],
});
