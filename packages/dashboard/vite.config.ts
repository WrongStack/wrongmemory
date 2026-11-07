import path from 'node:path';
import { fileURLToPath } from 'node:url';
import tailwindcss from '@tailwindcss/vite';
import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';

const here = path.dirname(fileURLToPath(import.meta.url));

// The CLI embeds exactly three files (index.html, app.js, app.css) with
// `include_str!`, so the build emits fixed names, one script, no chunks and
// no inline script (the page's CSP allows only 'self').
export default defineConfig({
  plugins: [react(), tailwindcss()],
  build: {
    outDir: path.resolve(here, '../../crates/wrongmemory-cli/src/ui_assets'),
    emptyOutDir: true,
    target: 'es2022',
    modulePreload: false,
    assetsInlineLimit: 0,
    cssCodeSplit: false,
    reportCompressedSize: false,
    rollupOptions: {
      output: {
        entryFileNames: 'app.js',
        assetFileNames: 'app[extname]',
        codeSplitting: false,
      },
    },
  },
});
