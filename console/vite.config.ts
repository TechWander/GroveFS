import { fileURLToPath, URL } from 'node:url'
import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

/*
  The build is a drop-in replacement for the old hand-written trio, because the
  gateway has exactly four static routes (`web.rs`): `/` -> index.html,
  `/app.css`, `/app.js`, `/favicon.svg`. Nothing else is served, so:

    · the output goes straight into `src/webui/`, where `include_str!` reads it
      at compile time — the binary is still self-contained and air-gapped, and
      the byte-for-byte "the page equals the source" checks in
      `.build-bin/fdb4_console_read.sh` keep passing;
    · the file names are FIXED (no content hash), because a hashed name would be
      a URL the gateway does not answer;
    · everything ends in ONE chunk, because `/app.js` is the only script route —
      hence `codeSplitting: false` and no lazy route;
    · no inline `<script>`/`<style>` and no `data:` URLs are emitted, because the
      gateway's CSP is `script-src 'self'; style-src 'self'`.

  `npm run dev` serves this directory itself and proxies `/api/*` to a gateway on
  :8787, so the console can be worked on without touching the installed binary.
*/
export default defineConfig({
  plugins: [vue()],
  resolve: {
    alias: { '@': fileURLToPath(new URL('./src', import.meta.url)) },
  },
  build: {
    outDir: '../src/webui',
    emptyOutDir: true,
    // Same floor the hand-written ES2020 file had: no optional-chaining-to-
    // syntax-level transpiles needed, and nothing the old page could not run.
    target: 'es2020',
    cssCodeSplit: false,
    // One stylesheet, one script, no side-car asset files.
    assetsInlineLimit: Number.MAX_SAFE_INTEGER,
    modulePreload: { polyfill: false },
    rollupOptions: {
      output: {
        entryFileNames: 'app.js',
        chunkFileNames: 'app.js',
        // The stylesheet Vite emits is named after the entry ("style.css"); the
        // gateway only answers `/app.css`, so the one CSS asset is renamed and
        // every other asset (there are none — see assetsInlineLimit) keeps its
        // own name.
        assetFileNames: (asset) =>
          asset.name?.endsWith('.css') ? 'app.css' : (asset.name ?? 'assets/[name][extname]'),
        // The current spelling of "never split" (`inlineDynamicImports` is now
        // the deprecated way to say it). A lazy route would emit a second file
        // under a name the gateway has no route for, so the ban stays enforced.
        codeSplitting: false,
      },
    },
  },
  server: {
    port: 5173,
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:8787',
        changeOrigin: false,
      },
    },
  },
})
